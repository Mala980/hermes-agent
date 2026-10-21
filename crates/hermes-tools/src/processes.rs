//! Registri proses background + tool `process_manage`.
//!
//! `exec` dengan `background: true` mendaftarkan proses di sini; `process_manage`
//! bisa melihat daftar (`list`), membaca output (`log`), menunggu (`wait`),
//! atau menghentikan (`kill`).
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex as AsyncMutex;

pub struct BgProcess {
    pub id: String,
    pub command: String,
    pub started_at: String,
    pub output: Arc<AsyncMutex<String>>,
    pub child: AsyncMutex<Option<tokio::process::Child>>,
    /// Handle stdin untuk aksi write/submit/close (None bila tidak di-pipe
    /// atau sudah ditutup).
    pub stdin: AsyncMutex<Option<tokio::process::ChildStdin>>,
    pub exit_code: AsyncMutex<Option<i32>>,
}

static REGISTRY: StdMutex<Option<HashMap<String, Arc<BgProcess>>>> = StdMutex::new(None);

fn registry() -> std::sync::MutexGuard<'static, Option<HashMap<String, Arc<BgProcess>>>> {
    REGISTRY.lock().unwrap()
}

/// Counter monotonik agar id unik walau dua proses didaftarkan dalam
/// milidetik yang sama (tes paralel memakai registry global yang sama).
static PROC_ID_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Daftarkan child yang baru di-spawn; kembalikan id proses.
pub async fn register_process(command: String, mut child: tokio::process::Child) -> String {
    let id = format!(
        "proc-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        PROC_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let output = Arc::new(AsyncMutex::new(String::new()));

    // Pompa stdout/stderr ke buffer.
    if let Some(stdout) = child.stdout.take() {
        let output = output.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut reader = tokio::io::BufReader::new(stdout);
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = output.lock().await;
                        out.push_str(&crate::shell::redact_secrets(&String::from_utf8_lossy(
                            &buf[..n],
                        )));
                        if out.len() > 200_000 {
                            let drain = out.len() - 200_000;
                            out.drain(..drain);
                        }
                    }
                }
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        let output = output.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut out = output.lock().await;
                        out.push_str(&format!(
                            "[stderr] {}",
                            crate::shell::redact_secrets(&String::from_utf8_lossy(&buf[..n]))
                        ));
                        if out.len() > 200_000 {
                            let drain = out.len() - 200_000;
                            out.drain(..drain);
                        }
                    }
                }
            }
        });
    }

    // Ambil stdin sebelum child dipindah; None bila spawn tanpa pipe.
    let stdin = child.stdin.take();
    let proc = Arc::new(BgProcess {
        id: id.clone(),
        command,
        started_at: chrono_utc_now(),
        output,
        child: AsyncMutex::new(Some(child)),
        stdin: AsyncMutex::new(stdin),
        exit_code: AsyncMutex::new(None),
    });
    // Reaper: tandai selesai saat child exit. Polling try_wait (bukan
    // wait() sambil memegang lock) agar kill/close bisa mengunci child dan
    // menghentikan proses yang masih berjalan — wait() sambil memegang lock
    // membuat kill tak pernah bisa masuk sampai proses keluar sendiri.
    {
        let proc = proc.clone();
        tokio::spawn(async move {
            let code = loop {
                let status = {
                    let mut guard = proc.child.lock().await;
                    match guard.as_mut() {
                        Some(c) => c.try_wait().ok().flatten(),
                        None => break None,
                    }
                };
                if let Some(s) = status {
                    break s.code();
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            };
            // Jangan timpa exit_code yang sudah diset kill/close (mis. -9):
            // proses yang mati karena sinyal punya code() = None.
            {
                let mut ec = proc.exit_code.lock().await;
                if ec.is_none() {
                    *ec = code;
                }
            }
            // Kosongkan child agar kill berikutnya jadi no-op.
            *proc.child.lock().await = None;
        });
    }
    registry()
        .get_or_insert_with(HashMap::new)
        .insert(id.clone(), proc);
    id
}

fn chrono_utc_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_default()
}

/// Ringkasan baca-saja satu proses background untuk UI (panel TUI).
/// `running` true selama `exit_code` belum terisi; `exit_code` = None
/// berarti masih berjalan (atau baru didaftarkan).
#[derive(Clone, Debug)]
pub struct BgProcessSummary {
    pub id: String,
    pub command: String,
    pub started_at: String,
    pub running: bool,
    pub exit_code: Option<i32>,
}

/// Snapshot semua proses background di registri global untuk panel TUI.
/// Lock std tidak dibawa melewati await (klon Arc dulu), jadi aman
/// dipanggil dari loop UI tiap tick. Urutan: berjalan dulu, lalu yang
/// terbaru.
pub async fn bg_processes_snapshot() -> Vec<BgProcessSummary> {
    let procs: Vec<Arc<BgProcess>> = registry()
        .as_ref()
        .map(|m| m.values().cloned().collect())
        .unwrap_or_default();
    let mut out = Vec::with_capacity(procs.len());
    for p in &procs {
        let exit_code = *p.exit_code.lock().await;
        out.push(BgProcessSummary {
            id: p.id.clone(),
            command: p.command.clone(),
            started_at: p.started_at.clone(),
            running: exit_code.is_none(),
            exit_code,
        });
    }
    out.sort_by(|a, b| {
        b.running
            .cmp(&a.running)
            .then(b.started_at.cmp(&a.started_at))
    });
    out
}

pub struct ProcessManageTool;

/// Resolusi timeout `wait`: `timeout` adalah nama argumen asli (`timeout_secs`
/// alias lama; `timeout` menang). Default 180 = TERMINAL_TIMEOUT aslinya;
/// timeout <= 0 adalah error; nilai > 180 dijepit ke 180 dengan catatan.
fn resolve_wait_timeout(args: &serde_json::Value) -> Result<(u64, Option<String>)> {
    const MAX: u64 = 180;
    let raw = opt_i64(args, "timeout").or_else(|| opt_i64(args, "timeout_secs"));
    match raw {
        Some(v) if v <= 0 => Err(HermesError::tool(
            "process_manage",
            format!("timeout must be positive (got {v})"),
        )),
        Some(v) if v as u64 > MAX => Ok((
            MAX,
            Some(format!(
                "Requested wait of {v}s was clamped to configured limit of {MAX}s"
            )),
        )),
        Some(v) => Ok((v as u64, None)),
        None => Ok((MAX, None)),
    }
}

/// Resolusi jendela `log`: `limit` baris maks (default 200), `offset` opsional.
/// Tanpa offset → 200 baris TERAKHIR (seperti aslinya); offset eksplisit
/// (0 = dari baris pertama) → baris[offset..offset+limit].
fn resolve_log_window(args: &serde_json::Value) -> (Option<usize>, usize) {
    let limit = opt_u64(args, "limit")
        .or_else(|| opt_u64(args, "tail_lines"))
        .unwrap_or(200)
        .max(1) as usize;
    let offset = opt_u64(args, "offset").map(|o| o as usize);
    (offset, limit)
}

#[async_trait]
impl Tool for ProcessManageTool {
    fn name(&self) -> &str {
        "process_manage"
    }

    fn description(&self) -> &str {
        "Kelola proses background (dari exec background=true). Aksi: list, log (baca output), \
         wait (tunggu selesai, opsional timeout), write/submit (kirim teks ke stdin; submit \
         menambah newline seperti menekan Enter — pakai untuk menjawab prompt), close (tutup \
         stdin/kirim EOF; kill bila proses masih hidup setelah masa tenggang), kill. ID boleh \
         berupa prefiks unik dari ID proses."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "action",
                    string_prop("list | log | wait | write | submit | close | kill."),
                ),
                (
                    "id",
                    string_prop(
                        "ID proses (untuk log/wait/write/submit/close/kill); prefiks unik juga diterima.",
                    ),
                ),
                ("data", string_prop("Teks stdin untuk write/submit.")),
                (
                    "timeout",
                    integer_prop(
                        "Detik maks untuk wait (default 180; dijepit ke 180 bila lebih besar). \
                         `timeout_secs` adalah alias (bila keduanya ada, `timeout` menang).",
                    ),
                ),
                (
                    "timeout_secs",
                    integer_prop("Alias lama untuk `timeout`."),
                ),
                (
                    "limit",
                    integer_prop(
                        "Baris log maks untuk log (default 200). `tail_lines` adalah alias.",
                    ),
                ),
                (
                    "offset",
                    integer_prop(
                        "Offset baris log (default: 200 baris TERAKHIR; 0 = dari baris pertama).",
                    ),
                ),
                (
                    "tail_lines",
                    integer_prop("Alias lama untuk `limit` (default 200)."),
                ),
            ],
            vec!["action"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let action = get_str(&args, "action")?;
        let content = match action {
            "list" => {
                // Ambil snapshot Arc dulu agar lock tidak dibawa melewati await.
                let procs: Vec<Arc<BgProcess>> = registry()
                    .as_ref()
                    .map(|m| m.values().cloned().collect())
                    .unwrap_or_default();
                if procs.is_empty() {
                    "Tidak ada proses background.".to_string()
                } else {
                    let mut lines = Vec::new();
                    for p in &procs {
                        let status = match *p.exit_code.lock().await {
                            Some(c) => format!("selesai (exit {c})"),
                            None => "berjalan".to_string(),
                        };
                        lines.push(format!("{} — `{}` — {status}", p.id, p.command));
                    }
                    lines.join("\n")
                }
            }
            "log" => {
                let id = get_str(&args, "id")?;
                let proc = find_proc(id)?;
                // Batch 8: default 200 baris terakhir seperti aslinya
                // (read_log(offset=None, limit=200)); offset eksplisit
                // memaginasi dari baris tersebut.
                let (offset, limit) = resolve_log_window(&args);
                let out = proc.output.lock().await;
                let lines: Vec<&str> = out.lines().collect();
                let (start, selected) = match offset {
                    None => {
                        let start = lines.len().saturating_sub(limit);
                        (start, &lines[start..])
                    }
                    Some(o) => {
                        let start = o.min(lines.len());
                        let end = (start + limit).min(lines.len());
                        (start, &lines[start..end])
                    }
                };
                let status = match *proc.exit_code.lock().await {
                    Some(c) => format!("selesai (exit {c})"),
                    None => "berjalan".to_string(),
                };
                let range = if selected.is_empty() {
                    format!("0 baris (dari {})", lines.len())
                } else {
                    format!(
                        "baris {}-{} dari {}",
                        start + 1,
                        start + selected.len(),
                        lines.len()
                    )
                };
                format!(
                    "Proses {} [{status}], {range}:\n{}",
                    proc.id,
                    crate::shell::redact_secrets(&selected.join("\n"))
                )
            }
            "wait" => {
                let id = get_str(&args, "id")?;
                let proc = find_proc(id)?;
                // Batch 8: default 180 (= TERMINAL_TIMEOUT aslinya);
                // <= 0 error; > 180 dijepit dengan catatan.
                let (timeout, note) = resolve_wait_timeout(&args)?;
                if wait_for_exit(&proc, timeout).await {
                    let code = proc.exit_code.lock().await.unwrap_or(-1);
                    let mut msg = format!("Proses {} selesai (exit {code}).", proc.id);
                    if let Some(n) = note {
                        msg.push_str(&format!(" Catatan: {n}"));
                    }
                    msg
                } else {
                    let mut msg = format!("Timeout {timeout}s: proses {} masih berjalan.", proc.id);
                    if let Some(n) = note {
                        msg.push_str(&format!(" Catatan: {n}"));
                    }
                    msg
                }
            }
            "write" | "submit" => {
                let id = get_str(&args, "id")?;
                let proc = find_proc(id)?;
                // submit = write + newline (seperti menekan Enter).
                let mut data = opt_str(&args, "data").unwrap_or("").to_string();
                if action == "submit" {
                    data.push('\n');
                }
                let n = data.len();
                let mut guard = proc.stdin.lock().await;
                match guard.as_mut() {
                    Some(stdin) => {
                        use tokio::io::AsyncWriteExt;
                        stdin.write_all(data.as_bytes()).await.map_err(|e| {
                            HermesError::tool("process_manage", format!("tulis stdin gagal: {e}"))
                        })?;
                        stdin.flush().await.map_err(|e| {
                            HermesError::tool("process_manage", format!("flush stdin gagal: {e}"))
                        })?;
                        let label = if action == "submit" {
                            "dikirim + Enter"
                        } else {
                            "dikirim"
                        };
                        format!("{n} byte {label} ke stdin proses {}.", proc.id)
                    }
                    None => {
                        return Err(HermesError::tool(
                            "process_manage",
                            format!(
                                "stdin proses `{}` tidak tersedia (proses selesai / stdin sudah ditutup)",
                                proc.id
                            ),
                        ))
                    }
                }
            }
            "close" => {
                let id = get_str(&args, "id")?;
                let proc = find_proc(id)?;
                let full_id = proc.id.clone();
                // Definisi close: tutup stdin dulu (anak menerima EOF), beri masa
                // tenggang agar proses keluar sendiri; bila masih hidup, kill.
                let had_stdin = proc.stdin.lock().await.take().is_some();
                if !had_stdin {
                    match *proc.exit_code.lock().await {
                        Some(c) => format!(
                            "Proses {full_id} sudah selesai (exit {c}); stdin tidak tersedia."
                        ),
                        None => format!(
                            "Stdin proses {full_id} sudah ditutup sebelumnya; proses masih berjalan."
                        ),
                    }
                } else if wait_for_exit(&proc, CLOSE_GRACE_SECS).await {
                    let code = proc.exit_code.lock().await.unwrap_or(-1);
                    format!(
                        "Stdin proses {full_id} ditutup (EOF); proses selesai sendiri (exit {code})."
                    )
                } else {
                    let mut guard = proc.child.lock().await;
                    match guard.as_mut() {
                        Some(child) => {
                            child.kill().await.map_err(|e| {
                                HermesError::tool("process_manage", format!("kill gagal: {e}"))
                            })?;
                            *proc.exit_code.lock().await = Some(-9);
                            format!(
                                "Stdin proses {full_id} ditutup (EOF); proses masih hidup setelah \
                                 {CLOSE_GRACE_SECS} dtk → dihentikan (kill)."
                            )
                        }
                        None => {
                            let code = proc.exit_code.lock().await.unwrap_or(-1);
                            format!(
                                "Stdin proses {full_id} ditutup (EOF); proses selesai (exit {code})."
                            )
                        }
                    }
                }
            }
            "kill" => {
                let id = get_str(&args, "id")?;
                let proc = find_proc(id)?;
                let mut guard = proc.child.lock().await;
                match guard.as_mut() {
                    Some(child) => {
                        child.kill().await.map_err(|e| {
                            HermesError::tool("process_manage", format!("kill gagal: {e}"))
                        })?;
                        *proc.exit_code.lock().await = Some(-9);
                        format!("Proses {} dihentikan.", proc.id)
                    }
                    None => format!("Proses {} sudah selesai.", proc.id),
                }
            }
            other => {
                return Err(HermesError::tool(
                    "process_manage",
                    format!("aksi tidak dikenal: {other}"),
                ))
            }
        };
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: self.name().to_string(),
            content,
            is_error: false,
        })
    }
}

/// Masa tenggang `close`: tutup stdin (EOF) dulu, kill hanya bila proses
/// masih hidup setelah ini.
const CLOSE_GRACE_SECS: u64 = 5;

/// Tunggu proses selesai sampai `timeout_secs`; true bila selesai tepat waktu.
async fn wait_for_exit(proc: &BgProcess, timeout_secs: u64) -> bool {
    tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), async {
        loop {
            if proc.exit_code.lock().await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await
    .is_ok()
}

/// Cari proses berdasarkan id persis atau prefiks unik (B34):
/// cocok persis dipakai dulu; satu prefiks cocok → dipakai; >1 cocok → error
/// ambigu yang mencantumkan id-idnya; tidak ada → error tidak ditemukan.
fn find_proc(id: &str) -> Result<Arc<BgProcess>> {
    if id.is_empty() {
        return Err(HermesError::tool("process_manage", "id proses kosong"));
    }
    let map = registry();
    let m = map.as_ref();
    if let Some(p) = m.and_then(|m| m.get(id).cloned()) {
        return Ok(p);
    }
    let matches: Vec<(String, Arc<BgProcess>)> = m
        .map(|m| {
            m.iter()
                .filter(|(k, _)| k.starts_with(id))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    match matches.len() {
        0 => Err(HermesError::tool(
            "process_manage",
            format!("proses `{id}` tidak ditemukan"),
        )),
        1 => Ok(matches.into_iter().next().expect("len == 1").1),
        _ => {
            let mut ids: Vec<String> = matches.into_iter().map(|(k, _)| k).collect();
            ids.sort();
            Err(HermesError::tool(
                "process_manage",
                format!(
                    "prefiks `{id}` ambigu — cocok dengan {} proses: {}. Pakai id lengkap.",
                    ids.len(),
                    ids.join(", ")
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bg_process_lifecycle() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "echo hello-bg"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().unwrap();
        let id = register_process("echo hello-bg".to_string(), child).await;
        // Tunggu selesai.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let done = registry()
                    .as_ref()
                    .and_then(|m| m.get(&id).cloned())
                    .map(|p| p.exit_code.try_lock().map(|g| g.is_some()).unwrap_or(false))
                    .unwrap_or(false);
                if done {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let proc = find_proc(&id).unwrap();
        let out = proc.output.lock().await;
        assert!(out.contains("hello-bg"), "output: {out}");
    }

    #[tokio::test]
    async fn bg_process_output_is_redacted() {
        // Rakit nilai palsu secara programatik agar tidak ada literal
        // berbentuk-secret di source.
        let val: String = ["FA", "KE", "STORE", "123"].concat();
        let ghtok = format!("ghp_{}", "c".repeat(20));
        let script = format!("echo api_key={val}; echo {ghtok} >&2");
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", &script])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().unwrap();
        let id = register_process("echo secrets".to_string(), child).await;
        // Tunggu selesai.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let done = registry()
                    .as_ref()
                    .and_then(|m| m.get(&id).cloned())
                    .map(|p| p.exit_code.try_lock().map(|g| g.is_some()).unwrap_or(false))
                    .unwrap_or(false);
                if done {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        // Beri waktu pompa menyelesaikan sisa buffer.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let proc = find_proc(&id).unwrap();
        let out = proc.output.lock().await;
        assert!(!out.contains(&val), "stdout bocor: {out}");
        assert!(!out.contains(&ghtok), "stderr bocor: {out}");
        assert!(out.contains("api_key=[REDACTED]"), "output: {out}");
        assert!(out.contains("[REDACTED]"), "output: {out}");
    }

    /// Roundtrip stdin: write mentah (tanpa newline) + submit (+newline),
    /// lalu close (EOF) membuat `cat` keluar sendiri; output terbaca via log.
    #[tokio::test]
    async fn write_submit_close_roundtrip() {
        use crate::test_support::test_ctx;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "cat"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().unwrap();
        let id = register_process("cat".to_string(), child).await;

        let tool = ProcessManageTool;
        let ctx = test_ctx(std::path::PathBuf::from("/tmp"));

        let r = tool
            .execute(
                serde_json::json!({"action": "write", "id": id, "data": "ping"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r.is_error, "write: {}", r.content);
        assert!(r.content.contains("dikirim"), "write: {}", r.content);

        let r = tool
            .execute(
                serde_json::json!({"action": "submit", "id": id, "data": "pong"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r.is_error, "submit: {}", r.content);
        assert!(r.content.contains("Enter"), "submit: {}", r.content);

        // close → EOF → cat keluar sendiri dalam masa tenggang.
        let r = tool
            .execute(serde_json::json!({"action": "close", "id": id}), &ctx)
            .await
            .unwrap();
        assert!(!r.is_error, "close: {}", r.content);
        assert!(r.content.contains("EOF"), "close: {}", r.content);

        // Tunggu pompa menyedot output, lalu verifikasi roundtrip.
        let log = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let out = find_proc(&id).unwrap().output.lock().await.clone();
                if out.contains("pingpong") {
                    break out;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .expect("timeout menunggu output cat");
        assert!(log.contains("pingpong"), "log: {log}");

        // write setelah stdin ditutup → error jelas.
        let r = tool
            .execute(
                serde_json::json!({"action": "write", "id": id, "data": "x"}),
                &ctx,
            )
            .await;
        assert!(r.is_err(), "write pasca-close harus error");
    }

    /// close pada proses yang mengabaikan EOF → kill setelah masa tenggang.
    #[tokio::test]
    async fn close_kills_process_ignoring_eof() {
        use crate::test_support::test_ctx;
        // `sleep` tidak membaca stdin: EOF tidak membuatnya keluar.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "sleep 300"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().unwrap();
        let id = register_process("sleep 300".to_string(), child).await;

        let tool = ProcessManageTool;
        let ctx = test_ctx(std::path::PathBuf::from("/tmp"));
        let r = tool
            .execute(serde_json::json!({"action": "close", "id": id}), &ctx)
            .await
            .unwrap();
        assert!(
            r.content.contains("dihentikan (kill)"),
            "close: {}",
            r.content
        );
        let proc = find_proc(&id).unwrap();
        let code = proc.exit_code.lock().await;
        assert_eq!(*code, Some(-9), "exit_code: {code:?}");
    }

    /// Snapshot memuat proses yang didaftarkan, dengan status berjalan
    /// lalu selesai setelah exit.
    #[tokio::test]
    async fn bg_processes_snapshot_reflects_registry() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.args(["-c", "echo snapshot-probe"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn().unwrap();
        let id = register_process("echo snapshot-probe".to_string(), child).await;

        let snap = bg_processes_snapshot().await;
        let entry = snap
            .iter()
            .find(|s| s.id == id)
            .expect("snapshot memuat id terdaftar");
        assert_eq!(entry.command, "echo snapshot-probe");
        assert!(!entry.started_at.is_empty());

        // Tunggu proses keluar, lalu snapshot harus menandai selesai.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let done = bg_processes_snapshot()
                    .await
                    .into_iter()
                    .find(|s| s.id == id)
                    .map(|s| !s.running)
                    .unwrap_or(false);
                if done {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();
        let entry = bg_processes_snapshot()
            .await
            .into_iter()
            .find(|s| s.id == id)
            .expect("entri tetap ada setelah selesai");
        assert!(!entry.running);
        assert_eq!(entry.exit_code, Some(0));
    }

    /// Prefiks unik diterima; prefiks ambigu → error berisi daftar id;
    /// id tak dikenal → error tidak ditemukan.
    #[tokio::test]
    async fn prefix_match_unique_ambiguous_missing() {
        fn fake(id: &str) -> Arc<BgProcess> {
            Arc::new(BgProcess {
                id: id.to_string(),
                command: "fake".to_string(),
                started_at: "0".to_string(),
                output: Arc::new(AsyncMutex::new(String::new())),
                child: AsyncMutex::new(None),
                stdin: AsyncMutex::new(None),
                exit_code: AsyncMutex::new(None),
            })
        }
        // Prefiks unik per-proses uji agar tak bertabrakan dengan tes paralel.
        let pfx = format!("zzprefixtest-{}", std::process::id());
        let id1 = format!("{pfx}-alpha");
        let id2 = format!("{pfx}-beta");
        {
            let mut reg = registry();
            let m = reg.get_or_insert_with(HashMap::new);
            m.insert(id1.clone(), fake(&id1));
            m.insert(id2.clone(), fake(&id2));
        }

        // Prefiks unik → cocok satu.
        let p = find_proc(&format!("{pfx}-alpha")).unwrap();
        assert_eq!(p.id, id1);
        let p = find_proc(&id2).unwrap(); // id persis tetap bisa
        assert_eq!(p.id, id2);

        // Ambigu → error mencantumkan kedua id.
        // (match, bukan unwrap_err: BgProcess tidak derive Debug)
        let err = match find_proc(&pfx) {
            Ok(_) => panic!("seharusnya ambigu"),
            Err(e) => e,
        }
        .to_string();
        assert!(err.contains("ambigu"), "err: {err}");
        assert!(err.contains(&id1) && err.contains(&id2), "err: {err}");

        // Tak ada yang cocok → tidak ditemukan.
        let err = match find_proc("zz-tidak-ada-proses-000") {
            Ok(_) => panic!("seharusnya tidak ditemukan"),
            Err(e) => e,
        }
        .to_string();
        assert!(err.contains("tidak ditemukan"), "err: {err}");

        // Bersihkan entri sintetis.
        let mut reg = registry();
        if let Some(m) = reg.as_mut() {
            m.remove(&id1);
            m.remove(&id2);
        }
    }

    // --- Batch 8: paritas minor default wait/log ---

    #[test]
    fn wait_timeout_default_180_dan_jepit() {
        // Default 180 = TERMINAL_TIMEOUT aslinya.
        let (t, note) = resolve_wait_timeout(&serde_json::json!({})).unwrap();
        assert_eq!(t, 180);
        assert!(note.is_none());
        // `timeout` menang atas alias `timeout_secs`.
        let (t, _) =
            resolve_wait_timeout(&serde_json::json!({"timeout": 30, "timeout_secs": 10})).unwrap();
        assert_eq!(t, 30);
        let (t, _) = resolve_wait_timeout(&serde_json::json!({"timeout_secs": 30})).unwrap();
        assert_eq!(t, 30);
        // > 180 dijepit ke 180 dengan catatan.
        let (t, note) = resolve_wait_timeout(&serde_json::json!({"timeout": 900})).unwrap();
        assert_eq!(t, 180);
        assert!(note.unwrap().contains("clamped"), "catatan jepit hilang");
    }

    #[test]
    fn wait_timeout_nol_ditolak() {
        for v in [0, -3] {
            let err = resolve_wait_timeout(&serde_json::json!({"timeout": v})).unwrap_err();
            assert!(
                err.to_string().contains("timeout must be positive"),
                "pesan salah: {err}"
            );
        }
    }

    #[test]
    fn log_window_default_200_baris_terakhir() {
        let (offset, limit) = resolve_log_window(&serde_json::json!({}));
        assert_eq!(offset, None);
        assert_eq!(limit, 200);
        let (offset, limit) = resolve_log_window(&serde_json::json!({"tail_lines": 50}));
        assert_eq!(offset, None);
        assert_eq!(limit, 50);
        let (offset, limit) = resolve_log_window(&serde_json::json!({"offset": 10, "limit": 25}));
        assert_eq!(offset, Some(10));
        assert_eq!(limit, 25);
    }
}
