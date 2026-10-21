//! Scheduler: loop tick yang menjalankan job yang jatuh tempo.
//!
//! Setiap job yang due dijalankan di task async-nya sendiri (konkuren),
//! sehingga satu job lambat tidak memblokir job lain.
//!
//! Kebijakan overlap per job (default `skip-if-running`): bila run sebelumnya
//! dari job yang SAMA belum selesai saat tick berikutnya tiba, tick ini
//! dilewati dengan warn log. Set `overlap = allow` untuk mengizinkan run
//! tumpang tindih.
//!
//! Ketahanan cron:
//! - Lock tick lintas-proses ([`TickLock`]): sebelum satu tick diproses,
//!   scheduler mengakuisisi lock eksklusif di direktori data cron. Bila
//!   scheduler lain (mis. hasil relaunch watchdog setelah crash) sedang
//!   memegangnya, seluruh tick dilewati dengan warn log agar job tidak
//!   double-fire.
//! - Tangga retry run-level: kegagalan run karena model/provider unreachable
//!   (network error, 5xx, timeout) dicoba ulang hingga 3x dengan jittered
//!   backoff via `hermes_core::with_retries`; error deterministik (400/401/403)
//!   langsung gagal tanpa retry.
use crate::{CronJob, CronStore, OverlapPolicy, TickLock};
use hermes_core::Result;
use hermes_gateway::PlatformRegistry;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Fungsi yang menjalankan prompt sebuah job dan mengembalikan hasilnya.
/// Diimplementasikan di CLI dengan Agent sungguhan.
pub trait JobRunner: Send + Sync {
    fn run_job<'a>(
        &'a self,
        job: &'a CronJob,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

/// ID job yang sedang berjalan (untuk kebijakan skip-if-running).
pub type RunningJobs = Arc<Mutex<HashSet<String>>>;

/// Loop scheduler: cek tiap `tick_secs`, jalankan job yang due, kirim hasilnya.
pub async fn run_scheduler<R: JobRunner + 'static>(
    store: Arc<CronStore>,
    platforms: Arc<PlatformRegistry>,
    runner: Arc<R>,
    tick_secs: u64,
    cancel: hermes_core::CancelFlag,
) {
    tracing::info!("cron scheduler berjalan (tick {tick_secs} dtk)");
    let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));
    loop {
        if cancel.is_cancelled() {
            break;
        }
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        tokio::time::sleep(std::time::Duration::from_secs(tick_secs)).await;
    }
}

/// Satu putaran tick: jalankan semua job yang due, masing-masing di task
/// async sendiri (konkuren). Job dengan overlap `skip` (default) yang run
/// sebelumnya masih in-flight akan dilewati dengan warn log.
pub async fn process_due_jobs<R: JobRunner + 'static>(
    store: &Arc<CronStore>,
    platforms: &Arc<PlatformRegistry>,
    runner: Arc<R>,
    running: &RunningJobs,
) {
    // B15: lock tick lintas-proses. Bila scheduler lain sedang memegang lock
    // (mis. proses lama yang belum mati + relaunch watchdog), lewati seluruh
    // tick ini dengan warn log — jangan error, jangan double-fire.
    let _tick_lock = match TickLock::acquire(store.data_dir()) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            tracing::warn!("tick dilewati: scheduler lain memegang lock tick cron");
            return;
        }
        Err(e) => {
            tracing::warn!("tick dilewati: gagal mengakuisisi lock tick cron: {e}");
            return;
        }
    };
    let now = chrono::Utc::now();
    let due = match store.due_jobs(now) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("gagal membaca job: {e}");
            vec![]
        }
    };
    for mut job in due {
        // Kebijakan skip-if-running: cek-dan-tandai atomik di bawah satu lock.
        if job.overlap == OverlapPolicy::Skip {
            let already_running = {
                let mut set = running.lock().unwrap();
                !set.insert(job.id.clone()) // insert=false bila id sudah ada
            };
            if already_running {
                tracing::warn!(
                    job = %job.name,
                    "tick dilewati: run sebelumnya masih berjalan (skip-if-running)"
                );
                if let Err(e) = store.skip_tick(&mut job, chrono::Utc::now()) {
                    tracing::warn!("gagal memajukan jadwal job: {e}");
                }
                continue;
            }
        }
        let tracked = job.overlap == OverlapPolicy::Skip;
        let store = store.clone();
        let platforms = platforms.clone();
        let runner = runner.clone();
        let running = running.clone();
        tokio::spawn(async move {
            // Guard melepas id dari set saat task selesai — termasuk bila
            // task panic — agar job tidak macet selamanya.
            let _guard = RunningGuard::new(running, tracked.then(|| job.id.clone()));
            tracing::info!(job = %job.name, "menjalankan cron job");
            // B14: tangga retry run-level — kegagalan unreachable (network,
            // 5xx, timeout) dicoba ulang hingga 3x dengan jittered backoff;
            // error deterministik (400/401/403) langsung gagal tanpa retry.
            // Ini melapisi retry per-LLM-call di dalam agent (batch 2): retry
            // dalam menstabilkan tiap panggilan, tangga luar menstabilkan
            // seluruh run job (termasuk runner tanpa retry internal).
            let result = hermes_core::with_retries(|| runner.run_job(&job)).await;
            match result {
                Ok(reply) => {
                    if let Err(e) = platforms.send(&job.platform, &job.target, &reply).await {
                        tracing::warn!(job = %job.name, "gagal mengirim hasil cron: {e}");
                    }
                }
                Err(e) => {
                    tracing::warn!(job = %job.name, "cron job error: {e}");
                    let _ = platforms
                        .send(
                            &job.platform,
                            &job.target,
                            &format!("Cron job `{}` gagal: {e}", job.name),
                        )
                        .await;
                }
            }
            if let Err(e) = store.mark_ran(&mut job, chrono::Utc::now()) {
                tracing::warn!("gagal menandai job: {e}");
            }
            // One-shot: hapus dari store setelah berjalan tepat satu kali
            // (auto-remove; tidak perlu disable-then-cleanup karena tidak ada
            // sisa jadwal). Dihapus apa pun hasil run-nya — ia sudah "fired".
            if job.is_oneshot() {
                if let Err(e) = store.remove(&job.id) {
                    tracing::warn!(job = %job.name, "gagal menghapus one-shot job: {e}");
                } else {
                    tracing::info!(job = %job.name, "one-shot job selesai; dihapus dari store");
                }
            }
        });
    }
}

/// Melepas job id dari set "sedang berjalan" saat task selesai.
struct RunningGuard {
    running: RunningJobs,
    id: Option<String>,
}

impl RunningGuard {
    fn new(running: RunningJobs, id: Option<String>) -> Self {
        Self { running, id }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.running.lock().unwrap().remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Runner uji: memblokir di barrier sampai tes melepasnya.
    struct BlockingRunner {
        started: AtomicUsize,
        barrier: tokio::sync::Barrier,
    }

    impl JobRunner for BlockingRunner {
        fn run_job<'a>(
            &'a self,
            job: &'a CronJob,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                let _ = tokio::time::timeout(Duration::from_secs(10), self.barrier.wait()).await;
                Ok(format!("selesai {}", job.name))
            })
        }
    }

    fn test_store(name: &str) -> Arc<CronStore> {
        // Direktori unik per tes: tiap tes punya lockfile tick sendiri, karena
        // tes tokio berjalan paralel dalam satu proses.
        let dir = std::env::temp_dir()
            .join("hermes-cron-sched-test")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(CronStore::open(dir.join("cron.db")).unwrap())
    }

    /// Buat job yang due sekarang; kembalikan job-nya.
    fn due_job(store: &CronStore, name: &str, overlap: OverlapPolicy) -> CronJob {
        let mut job = CronJob::new(name, "*/1 * * * * *", "p", "telegram", "t", "default").unwrap();
        job.overlap = overlap;
        job.next_run = Some(chrono::Utc::now() - chrono::Duration::seconds(1));
        store.add(&job).unwrap();
        job
    }

    /// Tunggu sampai `started` mencapai `n` (timeout 5 dtk agar tes tak hang).
    async fn wait_started(runner: &BlockingRunner, n: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while runner.started.load(Ordering::SeqCst) < n {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("job harus mulai");
    }

    fn get_job(store: &CronStore, id: &str) -> CronJob {
        store
            .list()
            .unwrap()
            .into_iter()
            .find(|j| j.id == id)
            .unwrap()
    }

    /// Runner uji yang langsung selesai.
    struct OkRunner;

    impl JobRunner for OkRunner {
        fn run_job<'a>(
            &'a self,
            job: &'a CronJob,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            let name = job.name.clone();
            Box::pin(async move { Ok(format!("selesai {name}")) })
        }
    }

    /// Runner uji yang menghitung pemanggilan dan selalu sukses.
    struct CountingRunner {
        calls: AtomicUsize,
    }

    impl JobRunner for CountingRunner {
        fn run_job<'a>(
            &'a self,
            _job: &'a CronJob,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok("ok".to_string()) })
        }
    }

    /// Runner uji yang gagal `fail_times` kali pertama dengan status HTTP
    /// tertentu, lalu berhasil.
    struct FlakyRunner {
        calls: AtomicUsize,
        fail_times: usize,
        status: u16,
    }

    impl JobRunner for FlakyRunner {
        fn run_job<'a>(
            &'a self,
            job: &'a CronJob,
        ) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n < self.fail_times {
                    Err(hermes_core::HermesError::http(self.status, None, "boom"))
                } else {
                    Ok(format!("selesai {}", job.name))
                }
            })
        }
    }

    /// Platform uji yang merekam semua pesan terkirim.
    struct RecordingPlatform {
        sent: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl hermes_core::Platform for RecordingPlatform {
        fn id(&self) -> &str {
            "telegram"
        }
        async fn send_message(&self, _target: &str, text: &str) -> Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    /// Registry berisi RecordingPlatform "telegram" + handle pesannya.
    fn recording_platforms() -> (Arc<PlatformRegistry>, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut reg = PlatformRegistry::new();
        reg.register(Arc::new(RecordingPlatform { sent: sent.clone() }));
        (Arc::new(reg), sent)
    }

    /// Tunggu sampai `sent` berisi minimal satu pesan (timeout agar tak hang).
    async fn wait_sent(sent: &Arc<Mutex<Vec<String>>>) {
        tokio::time::timeout(Duration::from_secs(20), async {
            while sent.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pesan hasil run harus terkirim");
    }

    /// B14: model unreachable (503 2x) → tangga retry run-level membuat job
    /// akhirnya sukses, bukan langsung gagal.
    #[tokio::test]
    async fn cron_run_retries_unreachable_model_then_succeeds() {
        let store = test_store("retry-ladder-ok");
        let (platforms, sent) = recording_platforms();
        let runner = Arc::new(FlakyRunner {
            calls: AtomicUsize::new(0),
            fail_times: 2,
            status: 503,
        });
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));
        due_job(&store, "flaky", OverlapPolicy::Skip);

        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        // 2 retry × backoff ±(1s + 2s) + jitter — beri ruang 20 dtk.
        wait_sent(&sent).await;

        assert_eq!(
            runner.calls.load(Ordering::SeqCst),
            3,
            "503 dua kali lalu sukses = tepat 3 percobaan"
        );
        let msgs = sent.lock().unwrap();
        assert_eq!(msgs.len(), 1, "tepat satu pesan hasil terkirim");
        assert!(
            msgs[0].contains("selesai flaky"),
            "pesan sukses harus terkirim, dapat: {}",
            msgs[0]
        );
        assert!(!msgs[0].contains("gagal"), "bukan pesan kegagalan");
    }

    /// B14: error deterministik (400) → gagal langsung tanpa retry.
    #[tokio::test]
    async fn cron_run_deterministic_error_fails_without_retry() {
        let store = test_store("retry-ladder-400");
        let (platforms, sent) = recording_platforms();
        let runner = Arc::new(FlakyRunner {
            calls: AtomicUsize::new(0),
            fail_times: 100,
            status: 400,
        });
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));
        due_job(&store, "bad-req", OverlapPolicy::Skip);

        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        wait_sent(&sent).await;

        assert_eq!(
            runner.calls.load(Ordering::SeqCst),
            1,
            "400 tidak boleh di-retry"
        );
        let msgs = sent.lock().unwrap();
        assert_eq!(msgs.len(), 1);
        assert!(
            msgs[0].contains("gagal"),
            "pesan kegagalan harus terkirim, dapat: {}",
            msgs[0]
        );
    }

    /// B15: bila lock tick dipegang "scheduler lain", tick dilewati total —
    /// job tidak dijalankan dan jadwal tidak dimajukan.
    #[tokio::test]
    async fn tick_skipped_when_lock_held_by_other_scheduler() {
        let store = test_store("lock-skip");
        let platforms = Arc::new(PlatformRegistry::new());
        let runner = Arc::new(CountingRunner {
            calls: AtomicUsize::new(0),
        });
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));
        let job = due_job(&store, "terkunci", OverlapPolicy::Skip);
        let next_before = get_job(&store, &job.id).next_run;

        // Simulasikan scheduler lain yang sedang memegang lock tick.
        let _other = TickLock::acquire(store.data_dir())
            .unwrap()
            .expect("lock harus diperoleh");
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(
            runner.calls.load(Ordering::SeqCst),
            0,
            "job tidak boleh dijalankan selagi lock dipegang scheduler lain"
        );
        assert_eq!(
            get_job(&store, &job.id).next_run,
            next_before,
            "tick yang dilewati tidak boleh memajukan jadwal"
        );

        // Setelah lock dilepas, tick berikutnya berjalan normal.
        drop(_other);
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while runner.calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("job harus berjalan setelah lock dilepas");
    }

    #[tokio::test]
    async fn oneshot_job_removed_after_single_fire() {
        let store = test_store("oneshot-remove");
        let platforms = Arc::new(PlatformRegistry::new());
        let runner = Arc::new(OkRunner);
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));

        // One-shot satu jam ke depan, lalu paksa due sekarang.
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        let job = CronJob::new(
            "sekali",
            format!("at {}", future.to_rfc3339()),
            "p",
            "telegram",
            "t",
            "default",
        )
        .unwrap();
        assert!(job.is_oneshot());
        store.add(&job).unwrap();
        store
            .set_next_run(&job.id, chrono::Utc::now() - chrono::Duration::seconds(1))
            .unwrap();

        process_due_jobs(&store, &platforms, runner, &running).await;

        // Tunggu task spawned selesai: job harus hilang dari store (tepat sekali jalan).
        tokio::time::timeout(Duration::from_secs(5), async {
            while store.list().unwrap().iter().any(|j| j.id == job.id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one-shot job harus dihapus dari store setelah berjalan");
        assert!(store.list().unwrap().is_empty(), "tidak boleh ada sisa job");
    }

    #[tokio::test]
    async fn overlap_skip_skips_tick_while_running() {
        let store = test_store("overlap-skip");
        let platforms = Arc::new(PlatformRegistry::new());
        let runner = Arc::new(BlockingRunner {
            started: AtomicUsize::new(0),
            barrier: tokio::sync::Barrier::new(2), // run pertama + tes
        });
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));

        let job = due_job(&store, "skip-me", OverlapPolicy::Skip);

        // Tick 1: job mulai, runner memblokir di barrier.
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        wait_started(&runner, 1).await;

        // Buat job due lagi selagi run pertama masih in-flight.
        let past = chrono::Utc::now() - chrono::Duration::seconds(1);
        store.set_next_run(&job.id, past).unwrap();

        // Tick 2: harus dilewati (skip-if-running).
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;

        // Lepaskan run pertama; beri waktu task selesai.
        tokio::time::timeout(Duration::from_secs(5), runner.barrier.wait())
            .await
            .expect("barrier harus lepas");
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(
            runner.started.load(Ordering::SeqCst),
            1,
            "tick kedua harus dilewati selagi run pertama in-flight"
        );
        let got = get_job(&store, &job.id);
        assert!(
            got.next_run.unwrap() > past,
            "skip_tick harus memajukan next_run"
        );
        assert!(
            running.lock().unwrap().is_empty(),
            "set running harus kosong setelah run selesai"
        );
    }

    #[tokio::test]
    async fn overlap_allow_runs_despite_inflight() {
        let store = test_store("overlap-allow");
        let platforms = Arc::new(PlatformRegistry::new());
        let runner = Arc::new(BlockingRunner {
            started: AtomicUsize::new(0),
            barrier: tokio::sync::Barrier::new(3), // run1 + run2 + tes
        });
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));

        let job = due_job(&store, "allow-me", OverlapPolicy::Allow);

        // Tick 1: run pertama mulai dan memblokir.
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        wait_started(&runner, 1).await;

        // Tick 2 selagi run pertama in-flight: dengan overlap=allow harus jalan.
        store
            .set_next_run(&job.id, chrono::Utc::now() - chrono::Duration::seconds(1))
            .unwrap();
        process_due_jobs(&store, &platforms, runner.clone(), &running).await;

        // Kedua run memblokir di barrier yang sama → keduanya harus tiba.
        tokio::time::timeout(Duration::from_secs(5), runner.barrier.wait())
            .await
            .expect("kedua run harus berjalan tumpang tindih");
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(
            runner.started.load(Ordering::SeqCst),
            2,
            "overlap=allow harus menjalankan tick kedua"
        );
    }

    #[tokio::test]
    async fn due_jobs_run_concurrently_not_sequentially() {
        let store = test_store("concurrent");
        let platforms = Arc::new(PlatformRegistry::new());
        // 2 job + tes = 3 pihak di barrier: tes hanya lepas bila KEDUA job
        // sudah mulai (bila sekuensial, job kedua takkan mulai → timeout).
        let runner = Arc::new(BlockingRunner {
            started: AtomicUsize::new(0),
            barrier: tokio::sync::Barrier::new(3),
        });
        let running: RunningJobs = Arc::new(Mutex::new(HashSet::new()));

        due_job(&store, "job-a", OverlapPolicy::Skip);
        due_job(&store, "job-b", OverlapPolicy::Skip);

        process_due_jobs(&store, &platforms, runner.clone(), &running).await;
        tokio::time::timeout(Duration::from_secs(5), runner.barrier.wait())
            .await
            .expect("kedua job harus berjalan konkuren, bukan sekuensial");
        assert_eq!(runner.started.load(Ordering::SeqCst), 2);
    }
}
