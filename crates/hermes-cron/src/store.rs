use crate::schedule::{parse_schedule, system_local_tz, ParsedSchedule};
use chrono::{DateTime, Utc};
use hermes_core::{HermesError, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Mutex;
use uuid::Uuid;

/// Kebijakan bila tick berikutnya tiba selagi run sebelumnya masih berjalan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlapPolicy {
    /// Lewati tick ini bila run sebelumnya belum selesai (default).
    #[default]
    Skip,
    /// Biarkan run tumpang tindih (konkuren).
    Allow,
}

impl std::fmt::Display for OverlapPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverlapPolicy::Skip => write!(f, "skip"),
            OverlapPolicy::Allow => write!(f, "allow"),
        }
    }
}

impl FromStr for OverlapPolicy {
    type Err = HermesError;
    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skip" | "skip-if-running" => Ok(OverlapPolicy::Skip),
            "allow" => Ok(OverlapPolicy::Allow),
            other => Err(HermesError::Cron(format!(
                "kebijakan overlap tidak dikenal: `{other}` (pakai `skip` atau `allow`)"
            ))),
        }
    }
}

/// Satu job cron.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    /// Spesifikasi jadwal kanonis (lihat [`crate::schedule`]): cron 6/7 kolom
    /// verbatim (mis. `"0 30 9 * * *"`), cron 5 kolom yang dinormalisasi
    /// (`"30 9 * * *"` → `"0 30 9 * * *"`), interval (`"every 900s"`),
    /// sekali-jalan (`"at 2026-10-22T01:00:00+00:00"`), atau padanan cron dari
    /// kata kunci (`daily` → `"0 0 0 * * *"`).
    pub schedule: String,
    /// Prompt yang dijalankan agen saat job tiba.
    pub prompt: String,
    pub platform: String,
    pub target: String,
    pub profile: String,
    pub enabled: bool,
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    /// Nama zona waktu IANA, mis. "Asia/Makassar". `None` = zona waktu lokal
    /// sistem (lihat [`CronJob::job_timezone`]).
    pub timezone: Option<String>,
    /// Kebijakan tumpang tindih; default [`OverlapPolicy::Skip`].
    #[serde(default)]
    pub overlap: OverlapPolicy,
}

impl CronJob {
    pub fn new(
        name: impl Into<String>,
        schedule: impl Into<String>,
        prompt: impl Into<String>,
        platform: impl Into<String>,
        target: impl Into<String>,
        profile: impl Into<String>,
    ) -> Result<Self> {
        // Validasi + normalisasi via parser jadwal terpadu. Cron 6 kolom
        // yang valid disimpan byte-identik seperti yang diketik.
        let parsed = parse_schedule(&schedule.into())?;
        let mut job = Self {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            schedule: parsed.canonical(),
            prompt: prompt.into(),
            platform: platform.into(),
            target: target.into(),
            profile: profile.into(),
            enabled: true,
            last_run: None,
            next_run: None,
            timezone: None,
            overlap: OverlapPolicy::default(),
        };
        job.recompute_next();
        Ok(job)
    }

    /// True bila jadwalnya sekali-jalan (`at <datetime>`).
    pub fn is_oneshot(&self) -> bool {
        matches!(
            parse_schedule(&self.schedule),
            Ok(ParsedSchedule::Once { .. })
        )
    }

    /// Zona waktu efektif job untuk evaluasi wall-clock jadwal.
    ///
    /// - `timezone` diset & valid (IANA) → dipakai.
    /// - tidak diset → zona waktu lokal sistem (via `iana-time-zone`).
    /// - nama invalid atau zona lokal tak terdeteksi → warn + fallback
    ///   zona lokal, lalu UTC sebagai pilihan terakhir.
    pub fn job_timezone(&self) -> chrono_tz::Tz {
        match self.timezone.as_deref() {
            Some(name) => match name.parse::<chrono_tz::Tz>() {
                Ok(tz) => tz,
                Err(_) => {
                    tracing::warn!(
                        job = %self.name,
                        "timezone `{name}` invalid; fallback ke zona lokal sistem"
                    );
                    system_local_tz()
                }
            },
            None => system_local_tz(),
        }
    }

    pub fn recompute_next(&mut self) {
        self.recompute_next_at(Utc::now());
    }

    /// Varian [`CronJob::recompute_next`] dengan jam referensi yang
    /// diinjeksikan — deterministik, untuk pengujian.
    pub fn recompute_next_at(&mut self, now: DateTime<Utc>) {
        let tz = self.job_timezone();
        let last_run = self.last_run;
        self.next_run = parse_schedule(&self.schedule)
            .ok()
            .and_then(|s| s.next_run_after(tz, now, last_run));
    }
}

/// Penyimpanan job cron di SQLite.
pub struct CronStore {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl CronStore {
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&path)
            .map_err(|e| HermesError::Cron(format!("gagal membuka cron db: {e}")))?;
        let store = Self {
            conn: Mutex::new(conn),
            path,
        };
        store.migrate()?;
        Ok(store)
    }

    pub fn open_default(hermes_home: &std::path::Path) -> Result<Self> {
        Self::open(hermes_home.join("cron.db"))
    }

    /// Path file database cron.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Direktori data cron (induk dari file db) — tempat lockfile tick
    /// lintas-proses berada (lihat [`crate::TickLock`]).
    pub fn data_dir(&self) -> &std::path::Path {
        self.path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
    }

    fn migrate(&self) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS jobs (
                    id TEXT PRIMARY KEY,
                    name TEXT NOT NULL,
                    schedule TEXT NOT NULL,
                    prompt TEXT NOT NULL,
                    platform TEXT NOT NULL,
                    target TEXT NOT NULL,
                    profile TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    last_run TEXT,
                    next_run TEXT
                );",
            )
            .map_err(|e| HermesError::Cron(format!("migrasi gagal: {e}")))?;
        // Kolom tambahan batch 4 (abaikan bila sudah ada dari migrasi lama).
        for ddl in [
            "ALTER TABLE jobs ADD COLUMN timezone TEXT",
            "ALTER TABLE jobs ADD COLUMN overlap TEXT NOT NULL DEFAULT 'skip'",
        ] {
            match self.conn.lock().unwrap().execute_batch(ddl) {
                Ok(()) => {}
                Err(e) if e.to_string().contains("duplicate column") => {}
                Err(e) => return Err(HermesError::Cron(format!("migrasi kolom gagal: {e}"))),
            }
        }
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap()
    }

    pub fn add(&self, job: &CronJob) -> Result<()> {
        self.lock()
            .execute(
                "INSERT INTO jobs (id,name,schedule,prompt,platform,target,profile,enabled,last_run,next_run,timezone,overlap)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    job.id,
                    job.name,
                    job.schedule,
                    job.prompt,
                    job.platform,
                    job.target,
                    job.profile,
                    job.enabled as i32,
                    job.last_run.map(|d| d.to_rfc3339()),
                    job.next_run.map(|d| d.to_rfc3339()),
                    job.timezone,
                    job.overlap.to_string(),
                ],
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(())
    }

    pub fn remove(&self, id_or_name: &str) -> Result<bool> {
        let n = self
            .lock()
            .execute(
                "DELETE FROM jobs WHERE id = ?1 OR name = ?1",
                params![id_or_name],
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(n > 0)
    }

    pub fn set_enabled(&self, id_or_name: &str, enabled: bool) -> Result<bool> {
        let n = self
            .lock()
            .execute(
                "UPDATE jobs SET enabled = ?1 WHERE id = ?2 OR name = ?2",
                params![enabled as i32, id_or_name],
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(n > 0)
    }

    pub fn list(&self) -> Result<Vec<CronJob>> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare(
                "SELECT id,name,schedule,prompt,platform,target,profile,enabled,last_run,next_run,timezone,overlap
                 FROM jobs ORDER BY name",
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        let jobs = stmt
            .query_map([], row_to_job)
            .map_err(|e| HermesError::Cron(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(jobs)
    }

    /// Job yang sudah waktunya berjalan.
    pub fn due_jobs(&self, now: DateTime<Utc>) -> Result<Vec<CronJob>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|j| j.enabled && j.next_run.map(|n| n <= now).unwrap_or(false))
            .collect())
    }

    pub fn mark_ran(&self, job: &mut CronJob, now: DateTime<Utc>) -> Result<()> {
        job.last_run = Some(now);
        job.recompute_next_at(now);
        // Bila next_run masih <= now (jadwal sangat rapat), majukan paksa +60 dtk.
        // next_run == None berarti jadwal sekali-jalan yang sudah lewat (atau
        // cron tanpa kemunculan mendatang): biarkan None agar tidak bangkit
        // lagi — scheduler menghapus one-shot setelah run ini.
        if let Some(n) = job.next_run {
            if n <= now {
                job.next_run = Some(now + chrono::Duration::seconds(60));
            }
        }
        self.persist_times(job)
    }

    /// Majukan `next_run` tanpa menandai run (dipakai saat tick dilewati
    /// oleh kebijakan skip-if-running): `last_run` tidak diubah.
    pub fn skip_tick(&self, job: &mut CronJob, now: DateTime<Utc>) -> Result<()> {
        job.recompute_next_at(now);
        if let Some(n) = job.next_run {
            if n <= now {
                job.next_run = Some(now + chrono::Duration::seconds(60));
            }
        }
        self.lock()
            .execute(
                "UPDATE jobs SET next_run = ?1 WHERE id = ?2",
                params![job.next_run.map(|d| d.to_rfc3339()), job.id],
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(())
    }

    /// Setel `next_run` secara eksplisit (berguna untuk pengujian).
    pub fn set_next_run(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        self.lock()
            .execute(
                "UPDATE jobs SET next_run = ?1 WHERE id = ?2",
                params![when.to_rfc3339(), id],
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(())
    }

    fn persist_times(&self, job: &CronJob) -> Result<()> {
        self.lock()
            .execute(
                "UPDATE jobs SET last_run = ?1, next_run = ?2 WHERE id = ?3",
                params![
                    job.last_run.map(|d| d.to_rfc3339()),
                    job.next_run.map(|d| d.to_rfc3339()),
                    job.id
                ],
            )
            .map_err(|e| HermesError::Cron(e.to_string()))?;
        Ok(())
    }
}

fn parse_opt_dt(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|x| x.parse::<DateTime<Utc>>().ok())
}

fn parse_overlap(s: Option<String>) -> OverlapPolicy {
    s.as_deref()
        .and_then(|x| x.parse::<OverlapPolicy>().ok())
        .unwrap_or_default()
}

fn row_to_job(r: &rusqlite::Row) -> rusqlite::Result<CronJob> {
    Ok(CronJob {
        id: r.get(0)?,
        name: r.get(1)?,
        schedule: r.get(2)?,
        prompt: r.get(3)?,
        platform: r.get(4)?,
        target: r.get(5)?,
        profile: r.get(6)?,
        enabled: r.get::<_, i32>(7)? != 0,
        last_run: parse_opt_dt(r.get(8)?),
        next_run: parse_opt_dt(r.get(9)?),
        timezone: r.get(10)?,
        overlap: parse_overlap(r.get(11)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn test_store(name: &str) -> CronStore {
        let path = std::env::temp_dir().join(format!("hermes-cron-test-{name}.db"));
        let _ = std::fs::remove_file(&path);
        CronStore::open(path).unwrap()
    }

    #[test]
    fn job_validates_cron() {
        assert!(CronJob::new("x", "not a cron", "p", "telegram", "1", "default").is_err());
        let j = CronJob::new("x", "0 30 9 * * *", "p", "telegram", "1", "default").unwrap();
        assert!(j.next_run.is_some());
        assert_eq!(j.overlap, OverlapPolicy::Skip, "default overlap = skip");
        assert_eq!(j.timezone, None);
    }

    #[test]
    fn overlap_policy_parse() {
        assert_eq!(
            "skip".parse::<OverlapPolicy>().unwrap(),
            OverlapPolicy::Skip
        );
        assert_eq!(
            "skip-if-running".parse::<OverlapPolicy>().unwrap(),
            OverlapPolicy::Skip
        );
        assert_eq!(
            "ALLOW".parse::<OverlapPolicy>().unwrap(),
            OverlapPolicy::Allow
        );
        assert!("bogus".parse::<OverlapPolicy>().is_err());
    }

    #[test]
    fn timezone_next_run_uses_job_timezone() {
        // "0 0 9 * * *" = tiap hari jam 09:00 wall-clock di zona job.
        let mut j = CronJob::new("x", "0 0 9 * * *", "p", "telegram", "1", "default").unwrap();
        j.timezone = Some("Asia/Makassar".to_string());
        j.recompute_next();
        let next = j.next_run.expect("next_run harus ada");
        // Asia/Makassar = UTC+8 tanpa DST: 09:00 lokal == 01:00 UTC.
        assert_eq!(next.hour(), 1, "09:00 Makassar harus jadi 01:00 UTC");
        let lokal = next.with_timezone(&chrono_tz::Asia::Makassar);
        assert_eq!(lokal.hour(), 9);
    }

    #[test]
    fn timezone_unset_defaults_to_system_local() {
        let local_name =
            iana_time_zone::get_timezone().expect("zona lokal sistem harus terdeteksi di tes ini");
        let mut a = CronJob::new("a", "0 0 9 * * *", "p", "telegram", "1", "default").unwrap();
        let mut b = CronJob::new("b", "0 0 9 * * *", "p", "telegram", "1", "default").unwrap();
        b.timezone = Some(local_name);
        a.recompute_next();
        b.recompute_next();
        assert_eq!(
            a.next_run, b.next_run,
            "timezone unset harus sama dengan zona lokal sistem"
        );
    }

    #[test]
    fn timezone_invalid_falls_back_without_panic() {
        let mut j = CronJob::new("x", "0 0 9 * * *", "p", "telegram", "1", "default").unwrap();
        j.timezone = Some("Bukan/Zona".to_string());
        j.recompute_next(); // tidak boleh panic; fallback ke lokal
        assert!(j.next_run.is_some());
    }

    #[test]
    fn timezone_and_overlap_persisted_roundtrip() {
        let store = test_store("tz-overlap");
        let mut job = CronJob::new("j", "0 0 9 * * *", "p", "telegram", "1", "default").unwrap();
        job.timezone = Some("Asia/Makassar".to_string());
        job.overlap = OverlapPolicy::Allow;
        job.recompute_next();
        store.add(&job).unwrap();
        let got = store
            .list()
            .unwrap()
            .into_iter()
            .find(|j| j.id == job.id)
            .unwrap();
        assert_eq!(got.timezone.as_deref(), Some("Asia/Makassar"));
        assert_eq!(got.overlap, OverlapPolicy::Allow);
    }

    #[test]
    fn new_accepts_all_schedule_formats() {
        for s in [
            "0 30 9 * * *", // cron 6 kolom
            "30 9 * * *",   // cron 5 kolom
            "every 15m",
            "every 1h",
            "every 30s",
            "hourly",
            "daily",
            "weekly",
            "midnight",
            "noon",
        ] {
            let j = CronJob::new("j", s, "p", "telegram", "1", "default");
            assert!(j.is_ok(), "{s} harus diterima");
            assert!(j.unwrap().next_run.is_some(), "{s} harus punya next_run");
        }
        // One-shot di masa depan diterima…
        let future = (Utc::now() + chrono::Duration::days(30)).to_rfc3339();
        let j = CronJob::new("j", format!("at {future}"), "p", "telegram", "1", "default").unwrap();
        assert!(j.is_oneshot());
        assert!(j.next_run.is_some());
        // …tapi waktu yang sudah lewat ditolak saat add.
        assert!(CronJob::new(
            "j",
            "at 2000-01-01T00:00:00Z",
            "p",
            "telegram",
            "1",
            "default"
        )
        .is_err());
        // Sampah tetap ditolak dengan pesan yang menyebut format.
        let err = CronJob::new("j", "kapan saja", "p", "telegram", "1", "default")
            .unwrap_err()
            .to_string();
        assert!(err.contains("cron 5 kolom"), "pesan: {err}");
    }

    #[test]
    fn new_canonicalizes_schedule_storage() {
        let j = CronJob::new("j", "30 9 * * *", "p", "telegram", "1", "default").unwrap();
        assert_eq!(j.schedule, "0 30 9 * * *", "5 kolom → detik 0");
        let j = CronJob::new("j", "0 30 9 * * *", "p", "telegram", "1", "default").unwrap();
        assert_eq!(j.schedule, "0 30 9 * * *", "6 kolom byte-identik");
        let j = CronJob::new("j", "every 15m", "p", "telegram", "1", "default").unwrap();
        assert_eq!(j.schedule, "every 900s");
        assert!(!j.is_oneshot());
        let j = CronJob::new("j", "daily", "p", "telegram", "1", "default").unwrap();
        assert_eq!(j.schedule, "0 0 0 * * *");
    }

    #[test]
    fn recompute_next_at_is_deterministic() {
        // Interval: next = from + durasi, tanpa membaca jam dinding.
        let mut j = CronJob::new("j", "every 15m", "p", "telegram", "1", "default").unwrap();
        let from = Utc
            .with_ymd_and_hms(2026, 10, 21, 12, 0, 0)
            .single()
            .unwrap();
        j.recompute_next_at(from);
        assert_eq!(j.next_run, Some(from + chrono::Duration::minutes(15)));
        // Cron 5 kolom dari instant tetap.
        let mut j = CronJob::new("j", "30 9 * * *", "p", "telegram", "1", "default").unwrap();
        j.timezone = Some("UTC".to_string());
        j.recompute_next_at(from);
        assert_eq!(
            j.next_run,
            Utc.with_ymd_and_hms(2026, 10, 22, 9, 30, 0).single()
        );
    }

    #[test]
    fn once_never_reschedules_after_firing() {
        // Pure function level: last_run terisi → None apa pun `from`-nya.
        let p = parse_schedule("at 2030-06-15T01:00:00Z").unwrap();
        assert!(p.is_oneshot());
        let from = Utc.with_ymd_and_hms(2030, 6, 14, 0, 0, 0).single().unwrap();
        assert!(p.next_run_after(chrono_tz::UTC, from, None).is_some());
        assert!(p.next_run_after(chrono_tz::UTC, from, Some(from)).is_none());
    }

    #[test]
    fn mark_ran_oneshot_leaves_next_run_none() {
        let store = test_store("oneshot-mark");
        let future = Utc::now() + chrono::Duration::hours(1);
        let mut job = CronJob::new(
            "j",
            format!("at {}", future.to_rfc3339()),
            "p",
            "telegram",
            "1",
            "default",
        )
        .unwrap();
        store.add(&job).unwrap();
        let now = Utc::now();
        store.mark_ran(&mut job, now).unwrap();
        assert_eq!(
            job.next_run, None,
            "one-shot tidak boleh dijadwalkan ulang setelah run"
        );
        // Tidak pernah due lagi.
        let later = now + chrono::Duration::hours(2);
        assert!(store
            .due_jobs(later)
            .unwrap()
            .iter()
            .all(|d| d.id != job.id));
    }

    #[test]
    fn skip_tick_advances_next_without_touching_last_run() {
        let store = test_store("skip-tick");
        let mut job = CronJob::new("j", "0 0 9 * * *", "p", "telegram", "1", "default").unwrap();
        store.add(&job).unwrap();
        let now = Utc::now();
        store.skip_tick(&mut job, now).unwrap();
        assert_eq!(job.last_run, None, "skip tidak boleh menandai last_run");
        assert!(job.next_run.unwrap() > now);
        // Terpersist ke DB.
        let got = store
            .list()
            .unwrap()
            .into_iter()
            .find(|j| j.id == job.id)
            .unwrap();
        assert_eq!(got.next_run, job.next_run);
        assert_eq!(got.last_run, None);
    }
}
