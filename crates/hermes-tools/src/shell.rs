//! Tool `exec`: menjalankan perintah shell.
//!
//! Mendukung backend eksekusi ganda seperti Hermes asli
//! (local, docker, ssh); backend cloud (daytona/modal/...) memberi pesan jelas.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

pub struct ExecTool;

/// Pola perintah yang dianggap berbahaya → risiko High.
fn is_dangerous(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    const PATTERNS: &[&str] = &[
        "rm -rf /",
        "rm -rf ~",
        "rm -rf .",
        "mkfs",
        ":(){",
        "dd if=",
        "> /dev/sd",
        "chmod -r 777 /",
        "shutdown",
        "reboot",
        "poweroff",
        "curl | sh",
        "wget | sh",
    ];
    PATTERNS.iter().any(|p| c.contains(p))
}

/// Samarkan rahasia pada output terminal, meniru `redact_terminal_output`
/// pada Hermes asli: nilai setelah kunci seperti `api_key=`/`token:` diganti
/// `[REDACTED]` (nama kunci dipertahankan), begitu juga pola token terkenal
/// (AWS key, JWT, Slack, GitHub). Diterapkan ke semua output exec/process
/// sebelum dikembalikan ke pemanggil atau disimpan.
pub fn redact_secrets(text: &str) -> String {
    static RES: OnceLock<Vec<(regex::Regex, &'static str)>> = OnceLock::new();
    let res = RES.get_or_init(|| {
        vec![
            // key=value atau key: value (case-insensitive); nama kunci dipertahankan.
            (
                regex::Regex::new(
                    r#"(?i)([\w.-]*?(?:api[_-]?key|token|secret|password|passwd|bearer))(\s*[=:]\s*)("[^"]*"|'[^']*'|\S+)"#,
                )
                .unwrap(),
                "${1}${2}[REDACTED]",
            ),
            // "Bearer <token>" ala header Authorization (tanpa =/:).
            // Dibatasi ke kata `bearer`: bentuk spasi untuk kata kunci lain
            // (`token`, `password`, ...) terlalu sering muncul di prosa biasa.
            (
                regex::Regex::new(r"(?i)\b(bearer)(\s+)(\S+)").unwrap(),
                "${1}${2}[REDACTED]",
            ),
            // AWS access key ID: AKIA + 16 karakter.
            (
                regex::Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
                "[REDACTED]",
            ),
            // JWT: eyJ.<base64url>.<base64url>.
            (
                regex::Regex::new(r"\beyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\b")
                    .unwrap(),
                "[REDACTED]",
            ),
            // Token Slack: xox[bap]-...
            (
                regex::Regex::new(r"\bxox[bap]-[A-Za-z0-9-]+\b").unwrap(),
                "[REDACTED]",
            ),
            // Token GitHub: ghp_/gho_/ghu_/ghs_/ghr_...
            (
                regex::Regex::new(r"\bgh[pousr]_[A-Za-z0-9_]+\b").unwrap(),
                "[REDACTED]",
            ),
        ]
    });
    let mut out = text.to_string();
    for (re, rep) in res {
        out = re.replace_all(&out, *rep).into_owned();
    }
    out
}

/// Nama env yang kredensialnya tidak pernah boleh diteruskan ke child process
/// (case-insensitive). Daftar ini non-reallowable: tidak bisa dibuka kembali.
fn is_credential_env_var(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const EXACT: &[&str] = &[
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GITHUB_TOKEN",
        "TELEGRAM_BOT_TOKEN",
    ];
    if EXACT.iter().any(|e| upper == *e) {
        return true;
    }
    const PREFIXES: &[&str] = &["AWS_", "AZURE_", "GOOGLE_", "ANTHROPIC_", "OPENAI_", "XAI_"];
    if PREFIXES.iter().any(|p| upper.starts_with(p)) {
        return true;
    }
    const SUFFIXES: &[&str] = &[
        "_API_KEY",
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PASSWD",
        "_PRIVATE_KEY",
        "_CLIENT_SECRET",
    ];
    SUFFIXES.iter().any(|s| upper.ends_with(s))
}

/// Terapkan lingkungan child yang sudah dibersihkan dari variabel kredensial.
///
/// Berlaku untuk env yang diwarisi dari parent MAUPUN `ctx.env` yang diteruskan.
/// Semua variabel lain (PATH, HOME, dsb.) diteruskan tanpa perubahan.
fn apply_sanitized_env(
    cmd: &mut tokio::process::Command,
    ctx_env: &std::collections::HashMap<String, String>,
) {
    // 1. Hapus kredensial yang diwarisi dari env parent.
    for (k, _v) in std::env::vars_os() {
        if let Some(name) = k.to_str() {
            if is_credential_env_var(name) {
                cmd.env_remove(&k);
            }
        }
    }
    // 2. Terapkan ctx.env dengan kredensial disaring.
    for (k, v) in ctx_env {
        if !is_credential_env_var(k) {
            cmd.env(k, v);
        }
    }
}

/// Hasil probe `bash --version`, dihitung sekali per proses.
static BASH_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Apakah `bash` terinstal? Probe sekali per proses (`bash --version`);
/// dipakai untuk memilih shell default backend local (audit W5-B5).
fn bash_available() -> bool {
    *BASH_AVAILABLE.get_or_init(|| {
        std::process::Command::new("bash")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Argv shell untuk backend spawn-per-call: `bash -c` seperti Hermes asli
/// (`bash_argv` di tools/environments/remote_common.py aslinya). Bila bash
/// tidak terinstal, fallback ke `sh -c` + warn log.
fn shell_argv(command: &str) -> (String, Vec<String>) {
    if bash_available() {
        (
            "bash".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    } else {
        tracing::warn!(
            "bash tidak ditemukan di PATH; exec fallback ke `sh -c` \
             (bash-ism seperti [[ ]], array, (( )) tidak didukung)"
        );
        (
            "sh".to_string(),
            vec!["-c".to_string(), command.to_string()],
        )
    }
}

/// Konfigurasi backend eksekusi SSH (audit W5-B8).
///
/// Meniru kunci config aslinya (`ssh_host`, `ssh_user`, `ssh_port`,
/// `ssh_key` di tools/terminal_tool_backends.py). Port ini sebelumnya tidak
/// punya struct config exec, jadi config dibaca dari environment dengan
/// presedensi `HERMES_SSH_*` lalu `TERMINAL_SSH_*` (nama env aslinya untuk
/// host/user). Argumen tool `host` tetap didukung sebagai override
/// per-panggilan.
#[derive(Debug, Clone)]
pub struct ExecConfig {
    /// Host SSH remote (wajib untuk backend ssh).
    pub ssh_host: String,
    /// User login SSH (opsional; bila kosong ssh memakai user lokal).
    pub ssh_user: String,
    /// Port SSH (default 22).
    pub ssh_port: u16,
    /// Path private key (`-i`); kosong = pakai ssh-agent/default ssh.
    pub ssh_key_path: String,
    /// NAMA env var yang menyimpan password SSH — bukan password-nya.
    /// Bila diset, auth memakai `sshpass -e` (butuh binary `sshpass`);
    /// bila kosong, dipakai `-o BatchMode=yes` (tanpa prompt interaktif).
    pub ssh_password_env: String,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            ssh_host: String::new(),
            ssh_user: String::new(),
            ssh_port: 22,
            ssh_key_path: String::new(),
            ssh_password_env: String::new(),
        }
    }
}

impl ExecConfig {
    /// Baca config dari environment. `HERMES_SSH_*` menang atas
    /// `TERMINAL_SSH_*` (fallback nama env aslinya). Port tak-valid → 22.
    pub fn from_env() -> Self {
        fn pick(primary: &str, fallback: Option<&str>) -> String {
            std::env::var(primary)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| {
                    fallback
                        .and_then(|f| std::env::var(f).ok())
                        .filter(|v| !v.trim().is_empty())
                })
                .unwrap_or_default()
        }
        Self {
            ssh_host: pick("HERMES_SSH_HOST", Some("TERMINAL_SSH_HOST")),
            ssh_user: pick("HERMES_SSH_USER", Some("TERMINAL_SSH_USER")),
            ssh_port: std::env::var("HERMES_SSH_PORT")
                .ok()
                .and_then(|p| p.trim().parse::<u16>().ok())
                .unwrap_or(22),
            ssh_key_path: pick("HERMES_SSH_KEY_PATH", Some("TERMINAL_SSH_KEY")),
            ssh_password_env: pick("HERMES_SSH_PASSWORD_ENV", None),
        }
    }
}

/// Susun perintah SSH dari config (pure — bisa di-unit-test tanpa koneksi).
///
/// Bentuk meniru `_build_ssh_command` di tools/environments/ssh.py aslinya
/// (tanpa ControlMaster karena backend ini one-shot):
/// `ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new
///  -o ConnectTimeout=10 [-p <port>] [-i <key>] [<user>@]<host> -- bash -c <command>`
///
/// Keputusan yang didokumentasikan:
/// - `BatchMode=yes` hanya bila TANPA password (mode password memakai
///   `sshpass -e`; BatchMode akan mematikan auth password).
/// - `StrictHostKeyChecking=accept-new` (sama seperti aslinya): kunci host
///   yang belum dikenal diterima & disimpan, tetapi host yang BERUBAH
///   kuncinya tetap ditolak. Sengaja TIDAK memakai `=no` agar verifikasi
///   host-key tidak dinonaktifkan diam-diam.
/// - Perintah remote dibungkus `bash -c` seperti aslinya (paritas B5:
///   bash-ism juga berlaku di remote); `--` memisahkan target dari
///   perintah dan argv terpisah menghindari bug quoting.
/// - `-p` hanya bila port != 22 dan `-i` hanya bila key diset (seperti aslinya).
pub fn build_ssh_command(cfg: &ExecConfig, command: &str) -> Result<(String, Vec<String>)> {
    let host = cfg.ssh_host.trim();
    if host.is_empty() {
        return Err(HermesError::tool(
            "exec",
            "backend ssh butuh ssh_host: set env HERMES_SSH_HOST (atau TERMINAL_SSH_HOST) \
             atau berikan argumen tool `host`",
        ));
    }
    let use_password = !cfg.ssh_password_env.trim().is_empty();
    let (prog, mut argv) = if use_password {
        (
            "sshpass".to_string(),
            vec!["-e".to_string(), "ssh".to_string()],
        )
    } else {
        ("ssh".to_string(), vec!["ssh".to_string()])
    };
    if !use_password {
        argv.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
    }
    argv.extend([
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
    ]);
    if cfg.ssh_port != 22 {
        argv.extend(["-p".to_string(), cfg.ssh_port.to_string()]);
    }
    let key = cfg.ssh_key_path.trim();
    if !key.is_empty() {
        argv.extend(["-i".to_string(), key.to_string()]);
    }
    let user = cfg.ssh_user.trim();
    argv.push(if user.is_empty() {
        host.to_string()
    } else {
        format!("{user}@{host}")
    });
    // Bungkus perintah remote dengan `bash -c` (paritas aslinya).
    argv.extend([
        "--".to_string(),
        "bash".to_string(),
        "-c".to_string(),
        command.to_string(),
    ]);
    Ok((prog, argv))
}

/// Baca password SSH dari env var yang namanya ada di `ssh_password_env`.
///
/// Nilainya tidak pernah masuk config/log — hanya diteruskan sebagai env
/// `SSHPASS` ke child `sshpass -e`. Gagal dengan pesan jelas bila env tidak
/// diset atau kosong.
fn read_ssh_password(cfg: &ExecConfig) -> Result<String> {
    let name = cfg.ssh_password_env.trim();
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            HermesError::tool(
                "exec",
                format!(
                    "backend ssh: env `{name}` (dari ssh_password_env) tidak diset/kosong; \
                 set env tersebut atau kosongkan ssh_password_env untuk auth kunci"
                ),
            )
        })
}

/// Pastikan binary `sshpass` ada di PATH (hanya untuk mode password).
fn ensure_sshpass_available() -> Result<()> {
    let found = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join("sshpass").is_file()))
        .unwrap_or(false);
    if found {
        Ok(())
    } else {
        Err(HermesError::tool(
            "exec",
            "backend ssh dengan ssh_password_env butuh binary `sshpass` di PATH \
             (atau kosongkan ssh_password_env dan pakai auth kunci SSH)",
        ))
    }
}

/// Batas pemotongan output exec (B6), meniru Hermes asli: head+tail.
const EXEC_TRUNC_HEAD: usize = 25_000;
const EXEC_TRUNC_TAIL: usize = 25_000;

/// Penomoran unik nama file spill dalam satu proses.
static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

/// Potong `s` menjadi head+tail bila melebihi batas.
///
/// Mengembalikan `(teks_tampil, Option<path_spill>)`: bila dipotong, teks
/// penuh juga ditulis ke spill file sementara (best-effort) dan path-nya
/// dikembalikan agar bisa dicantumkan pada hasil.
fn truncate_head_tail(s: &str) -> (String, Option<PathBuf>) {
    let total = s.chars().count();
    if total <= EXEC_TRUNC_HEAD + EXEC_TRUNC_TAIL {
        return (s.to_string(), None);
    }
    let head: String = s.chars().take(EXEC_TRUNC_HEAD).collect();
    let tail: String = s.chars().skip(total - EXEC_TRUNC_TAIL).collect();
    let cut = total - EXEC_TRUNC_HEAD - EXEC_TRUNC_TAIL;
    let shown = format!("{head}\n... [truncated {cut} chars] ...\n{tail}");
    (shown, write_spill_file(s))
}

/// Tulis teks penuh ke file spill di temp dir; best-effort (gagal → None,
/// tool tetap berhasil tanpa info spill).
fn write_spill_file(full: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("hermes-exec-spills");
    std::fs::create_dir_all(&dir).ok()?;
    let name = format!(
        "exec-{}-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        SPILL_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    std::fs::write(&path, full).ok()?;
    Some(path)
}

/// Bentuk akhir output exec: redaksi secret diterapkan pada teks penuh
/// (redaksi juga berlaku untuk isi spill file), lalu pemotongan head+tail
/// + cantuman path spill bila dipotong.
fn finalize_exec_output(full: &str) -> String {
    let redacted = redact_secrets(full);
    let (shown, spill) = truncate_head_tail(&redacted);
    match spill {
        Some(path) => format!(
            "{shown}\n[output lengkap ({} karakter) tersimpan di: {}]",
            redacted.chars().count(),
            path.display()
        ),
        None => shown,
    }
}

/// Karakter yang menandakan `path` tak-dikutip bukan path polos melainkan
/// ekspresi shell (mis. `cd /tmp || x`) → serahkan ke shell, jangan parse.
fn has_shell_metachars(p: &str) -> bool {
    p.chars().any(|c| {
        matches!(
            c,
            '|' | '&' | '<' | '>' | '(' | ')' | '$' | '`' | '!' | '*' | '?' | '#' | '\\' | '\n'
        )
    })
}

/// Parse `cd <path>` di awal perintah (B7).
///
/// Mengembalikan `(path, sisa)` bila perintah diawali `cd <path>` yang diikuti
/// akhir perintah, `&&`, atau `;`. Path boleh dikutip (`"..."`/`'...'`) agar
/// bisa mengandung spasi/`;`. Bukan pola `cd` murni (diawali kata lain, `cd`
/// tanpa path, atau path tak-dikutip mengandung operator shell) → `None` dan
/// perintah dijalankan seperti biasa oleh shell.
fn parse_leading_cd(command: &str) -> Option<(String, String)> {
    let s = command.trim_start();
    let rest = s.strip_prefix("cd")?;
    // `cd` harus diikuti whitespace — bukan `cdx`, `cd..`, dsb.
    if !rest.starts_with(|c: char| c.is_whitespace()) {
        return None;
    }
    let after = rest.trim_start();
    if after.is_empty() {
        return None; // `cd` tanpa path → biarkan shell yang menangani.
    }
    let (path, rest, quoted) = match after.chars().next() {
        Some(q @ ('"' | '\'')) => {
            let inner = &after[q.len_utf8()..];
            let end = inner.find(q)?;
            (
                inner[..end].to_string(),
                inner[end + q.len_utf8()..].to_string(),
                true,
            )
        }
        _ => {
            // Path tak-dikutip berakhir di `&&`, `;`, atau akhir perintah.
            let cut = after.find("&&").into_iter().chain(after.find(';')).min();
            let (p, r) = match cut {
                Some(i) => (after[..i].trim_end().to_string(), after[i..].to_string()),
                None => (after.trim_end().to_string(), String::new()),
            };
            (p, r, false)
        }
    };
    if path.is_empty() {
        return None;
    }
    if !quoted && has_shell_metachars(&path) {
        return None;
    }
    let rest_t = rest.trim_start();
    if rest_t.is_empty() || rest_t.starts_with("&&") || rest_t.starts_with(';') {
        Some((path, rest_t.to_string()))
    } else {
        None
    }
}

/// Ekspansi `~` di awal path (ala `shellexpand_tilde` di hermes-core).
fn expand_tilde(p: &str) -> String {
    if p == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return home;
        }
    } else if let Some(rest) = p.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    p.to_string()
}

/// Terapkan `cd <path>` ke cwd sesi (B7).
///
/// Path absolut dipakai langsung; path relatif di-resolve terhadap cwd sesi
/// saat ini. Target di-canonicalize dulu (gagal → direktori tidak ada) agar
/// symlink/`..` tidak bisa lolos gating; tanpa `allow_outside_workdir`, target
/// harus tetap di bawah `working_dir` awal sesi seperti `resolve_path`.
/// Gagal → error yang jelas dan cwd sesi TIDAK berubah.
fn apply_cd(ctx: &ToolContext, path: &str) -> Result<()> {
    let raw = PathBuf::from(expand_tilde(path));
    let target = if raw.is_absolute() {
        raw
    } else {
        ctx.current_dir().join(raw)
    };
    let resolved = target
        .canonicalize()
        .map_err(|_| HermesError::tool("exec", format!("cd: direktori tidak ada: {path}")))?;
    if !resolved.is_dir() {
        return Err(HermesError::tool(
            "exec",
            format!("cd: bukan direktori: {path}"),
        ));
    }
    if !ctx.allow_outside_workdir {
        let anchor = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());
        if !resolved.starts_with(&anchor) {
            return Err(HermesError::tool(
                "exec",
                format!("cd: di luar working dir tidak diizinkan: {path}"),
            ));
        }
    }
    ctx.set_current_dir(resolved);
    Ok(())
}

/// Resolusi timeout exec: `timeout` (nama asli; `timeout_secs` alias lama,
/// `timeout` menang bila keduanya ada); default 180 seperti TERMINAL_TIMEOUT
/// Hermes asli. Nilai <= 0 adalah error (bukan clamp diam-diam) — pesan sama
/// seperti aslinya: "timeout must be a positive number of seconds (got N)."
fn resolve_exec_timeout(args: &serde_json::Value) -> Result<u64> {
    let raw = opt_i64(args, "timeout").or_else(|| opt_i64(args, "timeout_secs"));
    match raw {
        Some(v) if v <= 0 => Err(HermesError::tool(
            "exec",
            format!("timeout must be a positive number of seconds (got {v})."),
        )),
        Some(v) => Ok(v as u64),
        None => Ok(180),
    }
}

/// Catatan sinyal keyed by signum — port dari `_SIGNAL_EXIT_NOTES`
/// (terminal_tool_result.py). 130/SIGINT sengaja dikecualikan: jalur
/// interrupt-marker yang menguasainya.
fn signal_exit_note(signum: i64) -> Option<&'static str> {
    match signum {
        3 => Some("SIGQUIT (quit from keyboard)"),
        4 => Some("SIGILL (illegal instruction — corrupt binary or wrong architecture)"),
        6 => Some("SIGABRT (abort — assertion failure, fatal runtime error, or glibc abort)"),
        7 => Some("SIGBUS (bus error — misaligned or unmapped memory access)"),
        8 => Some("SIGFPE (fatal arithmetic error, e.g. integer division by zero)"),
        9 => Some(
            "SIGKILL — often the kernel OOM killer on memory exhaustion, or an explicit kill -9",
        ),
        11 => Some("SIGSEGV (segmentation fault — the program crashed)"),
        13 => Some("SIGPIPE (wrote to a closed pipe — e.g. output piped to a reader that exited)"),
        15 => Some("SIGTERM (terminated — kill/timeout or shutdown requested it to stop)"),
        24 => Some("SIGXCPU (CPU time limit exceeded)"),
        25 => Some("SIGXFSZ (file size limit exceeded)"),
        _ => None,
    }
}

/// Catatan exit-code non-informatif vs error asli — port dari
/// `_interpret_exit_code` (terminal_tool_result.py): terminasi sinyal
/// (kode negatif subprocess, atau konvensi shell 128+signum) dan exit-code
/// non-nol informatif per perintah dasar (grep=1 "no matches", dsb.), agar
/// model tidak membakar turn menyelidiki hasil yang sebenarnya normal.
pub fn interpret_exit_code(command: &str, exit_code: i64) -> Option<String> {
    if exit_code == 0 {
        return None;
    }
    // Terminasi sinyal: kode negatif = subprocess semantics (pasti); 128+signum
    // = konvensi shell (heuristik, bisa saja program exit 139 sendiri — makanya
    // berbunyi "usually"). SIGINT dikecualikan (milik jalur interrupt-marker).
    if exit_code < 0 {
        let signum = -exit_code;
        if signum == 2 {
            return None;
        }
        return Some(match signal_exit_note(signum) {
            Some(note) => format!("Command terminated by signal {signum}: {note}"),
            None => format!("Command terminated by signal {signum}"),
        });
    }
    if exit_code > 128 {
        if let Some(note) = signal_exit_note(exit_code - 128) {
            let signum = exit_code - 128;
            return Some(format!(
                "Exit code {exit_code} usually means the command was terminated by signal {signum}: {note}"
            ));
        }
    }
    // Perintah dasar = kata pertama segmen terakhir yang bukan penugasan
    // VAR=nilai; segmen terakhir pipeline/chain menentukan exit code.
    let last_segment = command
        .rsplit(['|', '&', ';'])
        .next()
        .unwrap_or(command)
        .trim();
    let base_cmd = last_segment
        .split_whitespace()
        .find(|w| !w.contains('=') || w.starts_with('-'))
        .map(|w| w.rsplit('/').next().unwrap_or(w))
        .unwrap_or("");
    let semantics: Option<&str> = match (base_cmd, exit_code) {
        ("grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack", 1) => {
            Some("No matches found (not an error)")
        }
        ("diff" | "colordiff", 1) => Some("Files differ (expected, not an error)"),
        ("test" | "[", 1) => Some("Condition evaluated to false (expected, not an error)"),
        ("find", 1) => {
            Some("Some directories were inaccessible (partial results may still be valid)")
        }
        ("curl", 6) => Some("Could not resolve host"),
        ("curl", 7) => Some("Failed to connect to host"),
        ("curl", 22) => Some("HTTP response code indicated error (e.g. 404, 500)"),
        ("curl", 28) => Some("Operation timed out"),
        ("git", 1) => {
            Some("Non-zero exit (often normal — e.g. 'git diff' returns 1 when files differ)")
        }
        _ => None,
    };
    semantics.map(|s| s.to_string())
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "Menjalankan perintah shell dan mengembalikan stdout+stderr. \
         Backend: local (default; shell `bash -c`, fallback `sh -c` bila bash \
         tak terinstal), docker (butuh container), ssh, daytona, modal, \
         singularity, apptainer, vercel. Backend ssh membaca config dari env \
         HERMES_SSH_HOST/HERMES_SSH_USER/HERMES_SSH_PORT (default 22)/ \
         HERMES_SSH_KEY_PATH/HERMES_SSH_PASSWORD_ENV (nama env var berisi \
         password, bukan password-nya); arg `host` (user@host) meng-override \
         per panggilan. Arg `timeout` (nama asli; alias lama `timeout_secs` — \
         bila keduanya ada, `timeout` menang; default 180): nilai > 600 detik \
         mempromosikan eksekusi menjadi proses background terdaftar (kelola \
         via process_manage). Output panjang dipotong head+tail 25k karakter \
         dengan spill file berisi output penuh. `cd <dir>` di awal perintah \
         memperbarui direktori kerja sesi untuk exec berikutnya. \
         Perintah berbahaya akan meminta persetujuan eksplisit."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("command", string_prop("Perintah shell yang dijalankan.")),
                (
                    "workdir",
                    string_prop("Direktori kerja (default: working dir sesi)."),
                ),
                (
                    "timeout",
                    integer_prop(
                        "Batas waktu detik (default 180). Nama argumen asli; `timeout_secs` \
                         adalah alias (bila keduanya ada, `timeout` menang). Nilai > 600 \
                         mempromosikan eksekusi menjadi proses background terdaftar.",
                    ),
                ),
                (
                    "timeout_secs",
                    integer_prop("Alias lama untuk `timeout`."),
                ),
                (
                    "backend",
                    string_prop("Backend eksekusi: local, docker, ssh, daytona, modal, singularity, apptainer, vercel (default local)."),
                ),
                ("container", string_prop("Nama container untuk backend docker.")),
                (
                    "host",
                    string_prop(
                        "Target SSH (user@host atau host saja) untuk backend ssh; \
                         meng-override config. Bila kosong, host/user/port/key \
                         dibaca dari env HERMES_SSH_HOST, HERMES_SSH_USER, \
                         HERMES_SSH_PORT (default 22), HERMES_SSH_KEY_PATH, dan \
                         HERMES_SSH_PASSWORD_ENV (nama env var berisi password).",
                    ),
                ),
                ("image", string_prop("Snapshot/image untuk backend daytona.")),
                (
                    "background",
                    boolean_prop("Jalankan sebagai proses background; kembalikan ID proses (kelola via process_manage)."),
                ),
            ],
            vec!["command"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn dynamic_risk(&self, args: &serde_json::Value) -> RiskLevel {
        let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        if is_dangerous(cmd) {
            RiskLevel::High
        } else {
            RiskLevel::Medium
        }
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let command = get_str(&args, "command")?;
        // B3: `timeout` adalah nama argumen asli; `timeout_secs` alias lama.
        // Bila keduanya diberikan, `timeout` menang. Default 180 (seperti
        // TERMINAL_TIMEOUT aslinya). timeout <= 0 adalah error, bukan clamp.
        let timeout = resolve_exec_timeout(&args)?;
        let backend = opt_str(&args, "backend").unwrap_or("local");
        // B7: direktori kerja default = cwd sesi (bisa berubah via `cd`).
        let workdir = match opt_str(&args, "workdir") {
            Some(w) => ctx.resolve_path(w)?,
            None => ctx.current_dir(),
        };
        // B7: `cd <path>` di awal perintah memperbarui cwd sesi; bila gagal,
        // kembalikan error yang jelas dan cwd tidak berubah.
        if let Some((cd_path, _rest)) = parse_leading_cd(command) {
            apply_cd(ctx, &cd_path)?;
        }

        // Backend cloud: perilaku timeout dipertahankan (clamp 1..=600);
        // promosi background (B4) hanya berlaku untuk jalur foreground.
        let cloud_timeout = timeout.min(600);
        // Backend cloud Daytona: buat sandbox, eksekusi via toolbox API, hapus.
        if backend == "daytona" {
            return exec_daytona(command, cloud_timeout, &args).await;
        }
        // Modal: via CLI + aplikasi Python ephemeral.
        if backend == "modal" {
            return exec_modal(command, cloud_timeout, &args).await;
        }
        // Vercel Sandbox: via node + @vercel/sandbox.
        if backend == "vercel" {
            return exec_vercel(command, cloud_timeout, &args).await;
        }

        // Env ekstra khusus backend (mis. SSHPASS untuk mode password SSH).
        let mut extra_env: Option<(String, String)> = None;
        let (prog, argv): (String, Vec<String>) = match backend {
            "docker" => {
                let container = opt_str(&args, "container").ok_or_else(|| {
                    HermesError::tool("exec", "backend docker butuh argumen `container`")
                })?;
                (
                    "docker".to_string(),
                    vec![
                        "exec".to_string(),
                        "-i".to_string(),
                        container.to_string(),
                        "bash".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ],
                )
            }
            "ssh" => {
                let mut cfg = ExecConfig::from_env();
                // Override per-panggilan: arg `host` boleh `user@host`.
                if let Some(h) = opt_str(&args, "host") {
                    if let Some((u, hh)) = h.split_once('@') {
                        cfg.ssh_user = u.to_string();
                        cfg.ssh_host = hh.to_string();
                    } else {
                        cfg.ssh_host = h.to_string();
                    }
                }
                let (p, a) = build_ssh_command(&cfg, command)?;
                if !cfg.ssh_password_env.trim().is_empty() {
                    ensure_sshpass_available()?;
                    extra_env = Some(("SSHPASS".to_string(), read_ssh_password(&cfg)?));
                }
                (p, a)
            }
            "singularity" | "apptainer" => {
                let bin = if backend == "apptainer" {
                    "apptainer"
                } else {
                    "singularity"
                };
                let image = opt_str(&args, "image").ok_or_else(|| {
                    HermesError::tool("exec", "backend singularity butuh argumen `image`")
                })?;
                (
                    bin.to_string(),
                    vec![
                        "exec".to_string(),
                        image.to_string(),
                        "bash".to_string(),
                        "-c".to_string(),
                        command.to_string(),
                    ],
                )
            }
            // B5: default `bash -c` seperti Hermes asli (fallback `sh` + warn
            // bila bash tak terinstal).
            "local" => shell_argv(command),
            other => {
                return Err(HermesError::tool(
                    "exec",
                    format!("backend tidak dikenal: {other}"),
                ))
            }
        };

        let mut cmd = tokio::process::Command::new(&prog);
        cmd.args(&argv).current_dir(&workdir);
        apply_sanitized_env(&mut cmd, &ctx.env);
        // Diterapkan setelah sanitasi agar tidak ikut tersaring.
        if let Some((k, v)) = extra_env {
            cmd.env(k, v);
        }

        // B4: timeout > 600 detik → promosikan menjadi proses background
        // terdaftar (kembali segera dengan handle), bukan clamp diam-diam.
        let explicit_bg = opt_bool(&args, "background").unwrap_or(false);
        let promoted = !explicit_bg && timeout > 600;
        // Mode background: spawn terpisah, daftarkan ke registri proses.
        if explicit_bg || promoted {
            // stdin di-pipe agar process_manage write/submit/close bisa
            // berinteraksi dengan proses (B33); default null mematikan itu.
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .stdin(std::process::Stdio::piped());
            let child = cmd
                .spawn()
                .map_err(|e| HermesError::tool("exec", format!("spawn gagal: {e}")))?;
            let id = crate::processes::register_process(command.to_string(), child).await;
            let content = if promoted {
                format!(
                    "Timeout {timeout}s melebihi batas foreground 600s; dipromosikan ke proses background: {id}\nGunakan process_manage untuk log/wait/write/kill."
                )
            } else {
                format!(
                    "Proses background dimulai: {id}\nGunakan process_manage untuk log/wait/write/kill."
                )
            };
            return Ok(ToolResult {
                tool_call_id: String::new(),
                name: self.name().to_string(),
                content,
                is_error: false,
            });
        }

        let out = crate::run_command(&mut cmd, timeout, "exec").await?;
        let mut combined = String::new();
        combined.push_str(&out.stdout);
        if !out.stderr.trim().is_empty() {
            combined.push_str("\n[stderr]\n");
            combined.push_str(&out.stderr);
        }
        let header = format!("[exit: {}]", out.code);
        let full = if combined.trim().is_empty() {
            header
        } else {
            format!("{header}\n{combined}")
        };
        // B6: redaksi diterapkan pada output penuh (termasuk spill file),
        // lalu dipotong head+tail.
        let mut content = finalize_exec_output(&full);
        // Catatan exit-code (sinyal/semantik per perintah) bila non-nol,
        // seperti aslinya — agar model tak membakar turn menyelidiki hasil
        // yang sebenarnya normal (mis. grep=1 "no matches").
        if !out.success {
            if let Some(note) = interpret_exit_code(command, out.code as i64) {
                content.push_str(&format!("\nCatatan: {note}"));
            }
        }
        // id diisi registry; pakai placeholder.
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: self.name().to_string(),
            content,
            is_error: !out.success,
        })
    }
}

/// Eksekusi perintah di sandbox Daytona via REST API.
///
/// Env: `DAYTONA_API_KEY` (wajib), `DAYTONA_API_URL` (opsional, default
/// `https://app.daytona.io/api`), `DAYTONA_SNAPSHOT` (opsional). Argumen tool
/// `image` bisa meng-override snapshot.
async fn exec_daytona(command: &str, timeout: u64, args: &serde_json::Value) -> Result<ToolResult> {
    let api_key = std::env::var("DAYTONA_API_KEY")
        .map_err(|_| HermesError::tool("exec", "backend daytona butuh env DAYTONA_API_KEY"))?;
    let base = std::env::var("DAYTONA_API_URL")
        .unwrap_or_else(|_| "https://app.daytona.io/api".to_string());
    let base = base.trim_end_matches('/').to_string();
    let client = reqwest::Client::new();

    // 1. Buat sandbox.
    let mut create_body = serde_json::json!({});
    if let Some(img) = opt_str(args, "image") {
        create_body["snapshot"] = img.into();
    } else if let Ok(snap) = std::env::var("DAYTONA_SNAPSHOT") {
        create_body["snapshot"] = snap.into();
    }
    let resp = client
        .post(format!("{base}/sandbox"))
        .bearer_auth(&api_key)
        .json(&create_body)
        .send()
        .await
        .map_err(|e| HermesError::tool("exec", format!("daytona: buat sandbox gagal: {e}")))?;
    if !resp.status().is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(HermesError::tool(
            "exec",
            format!("daytona: buat sandbox ditolak: {b}"),
        ));
    }
    let created: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| HermesError::tool("exec", format!("daytona: parse sandbox gagal: {e}")))?;
    let sandbox_id = created
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| HermesError::tool("exec", "daytona: respons sandbox tanpa id"))?
        .to_string();

    // 2. Eksekusi perintah via toolbox API.
    let exec_result = async {
        let resp = client
            .post(format!("{base}/toolbox/{sandbox_id}/process/execute"))
            .bearer_auth(&api_key)
            .json(&serde_json::json!({ "command": command, "timeout": timeout }))
            .send()
            .await
            .map_err(|e| HermesError::tool("exec", format!("daytona: eksekusi gagal: {e}")))?;
        if !resp.status().is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(HermesError::tool(
                "exec",
                format!("daytona: eksekusi ditolak: {b}"),
            ));
        }
        resp.json::<serde_json::Value>()
            .await
            .map_err(|e| HermesError::tool("exec", format!("daytona: parse hasil gagal: {e}")))
    }
    .await;

    // 3. Hapus sandbox (best effort).
    let _ = client
        .delete(format!("{base}/sandbox/{sandbox_id}"))
        .bearer_auth(&api_key)
        .send()
        .await;

    let v = exec_result?;
    let exit_code = v.get("exitCode").and_then(|c| c.as_i64()).unwrap_or(-1);
    let stdout = v
        .get("result")
        .or_else(|| v.get("artifacts").and_then(|a| a.get("stdout")))
        .and_then(|s| s.as_str())
        .unwrap_or("");
    let content = finalize_exec_output(&format!("[exit: {exit_code}]\n{stdout}"));
    Ok(ToolResult {
        tool_call_id: String::new(),
        name: "exec".to_string(),
        content,
        is_error: exit_code != 0,
    })
}

/// Eksekusi via Modal: bangkitkan aplikasi Python ephemeral lalu `modal run`.
///
/// Butuh CLI `modal` terinstal & terkonfigurasi (`modal token new`).
async fn exec_modal(command: &str, timeout: u64, args: &serde_json::Value) -> Result<ToolResult> {
    let image = opt_str(args, "image").unwrap_or("debian_slim");
    let py = format!(
        r#"import modal, subprocess, json
app = modal.App("hermes-exec")
image = modal.Image.debian_slim() if "{image}" == "debian_slim" else modal.Image.from_registry("{image}")

@app.function(image=image, timeout=1800)
def run(cmd: str):
    r = subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=1700)
    return {{"code": r.returncode, "stdout": r.stdout[-20000:], "stderr": r.stderr[-8000:]}}

@app.local_entrypoint()
def main(cmd: str):
    print(json.dumps(run.remote(cmd)))
"#
    );
    let dir = std::env::temp_dir().join("hermes-modal");
    std::fs::create_dir_all(&dir)
        .map_err(|e| HermesError::tool("exec", format!("tmpdir gagal: {e}")))?;
    let script = dir.join("hermes_exec.py");
    std::fs::write(&script, py)
        .map_err(|e| HermesError::tool("exec", format!("tulis script gagal: {e}")))?;

    let mut cmd = tokio::process::Command::new("modal");
    cmd.args(["run", &script.display().to_string(), "--cmd", command]);
    let out = crate::run_command(&mut cmd, timeout, "exec")
        .await
        .map_err(|e| {
            if e.to_string().contains("spawn") || e.to_string().contains("No such") {
                HermesError::tool(
                    "exec",
                    "backend modal butuh CLI `modal` terinstal & login (`modal token new`)",
                )
            } else {
                e
            }
        })?;
    // Output berupa JSON satu baris dari print(json.dumps(...)).
    let line = out
        .stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'));
    let (code, stdout, stderr) =
        match line.and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
            Some(v) => (
                v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
                v.get("stdout")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("stderr")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            None => (-1, out.stdout.clone(), out.stderr.clone()),
        };
    let mut combined = stdout;
    if !stderr.trim().is_empty() {
        combined.push_str("\n[stderr]\n");
        combined.push_str(&stderr);
    }
    Ok(ToolResult {
        tool_call_id: String::new(),
        name: "exec".to_string(),
        content: finalize_exec_output(&format!("[exit: {code}]\n{combined}")),
        is_error: code != 0,
    })
}

/// Eksekusi via Vercel Sandbox: node + @vercel/sandbox.
///
/// Butuh `node` dan env `VERCEL_TOKEN`.
async fn exec_vercel(command: &str, timeout: u64, _args: &serde_json::Value) -> Result<ToolResult> {
    if std::env::var("VERCEL_TOKEN").is_err() {
        return Err(HermesError::tool(
            "exec",
            "backend vercel butuh env VERCEL_TOKEN",
        ));
    }
    let js = r#"import { Sandbox } from '@vercel/sandbox';
const cmd = process.argv.slice(2).join(' ');
const sandbox = await Sandbox.create();
try {
  const result = await sandbox.runCommand({ cmd: ['sh', '-c', cmd] });
  const stdout = await result.stdout();
  const stderr = await result.stderr();
  console.log(JSON.stringify({ code: result.exitCode, stdout: stdout.slice(-20000), stderr: stderr.slice(-8000) }));
} finally {
  await sandbox.stop();
}
"#;
    let dir = std::env::temp_dir().join("hermes-vercel");
    std::fs::create_dir_all(&dir)
        .map_err(|e| HermesError::tool("exec", format!("tmpdir gagal: {e}")))?;
    let script = dir.join("hermes_exec.mjs");
    std::fs::write(&script, js)
        .map_err(|e| HermesError::tool("exec", format!("tulis script gagal: {e}")))?;

    // Pastikan @vercel/sandbox tersedia (install sekali bila perlu).
    let pkg = dir.join("package.json");
    if !dir.join("node_modules/@vercel/sandbox").exists() {
        if !pkg.exists() {
            std::fs::write(&pkg, r#"{"type":"module"}"#)
                .map_err(|e| HermesError::tool("exec", format!("tulis package.json gagal: {e}")))?;
        }
        let mut npm = tokio::process::Command::new("npm");
        npm.args(["install", "@vercel/sandbox"]).current_dir(&dir);
        let _ = crate::run_command(&mut npm, 300, "exec").await;
    }

    let mut cmd = tokio::process::Command::new("node");
    cmd.args([script.display().to_string(), command.to_string()])
        .current_dir(&dir)
        .env(
            "VERCEL_TOKEN",
            std::env::var("VERCEL_TOKEN").unwrap_or_default(),
        );
    let out = crate::run_command(&mut cmd, timeout, "exec")
        .await
        .map_err(|_| HermesError::tool("exec", "backend vercel butuh `node` terinstal"))?;
    let line = out
        .stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'));
    let (code, stdout, stderr) =
        match line.and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok()) {
            Some(v) => (
                v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
                v.get("stdout")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                v.get("stderr")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
            ),
            None => (-1, out.stdout.clone(), out.stderr.clone()),
        };
    let mut combined = stdout;
    if !stderr.trim().is_empty() {
        combined.push_str("\n[stderr]\n");
        combined.push_str(&stderr);
    }
    Ok(ToolResult {
        tool_call_id: String::new(),
        name: "exec".to_string(),
        content: finalize_exec_output(&format!("[exit: {code}]\n{combined}")),
        is_error: code != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::ToolContext;
    use std::collections::HashMap;

    struct StubMemory;
    #[async_trait::async_trait]
    impl hermes_core::MemoryStore for StubMemory {
        async fn remember(&self, _e: hermes_core::MemoryEntry) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn recall(
            &self,
            _q: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _q: &str) -> hermes_core::Result<usize> {
            Ok(0)
        }
        async fn list_memories(
            &self,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::MemoryEntry>> {
            Ok(vec![])
        }
        async fn save_session(&self, _s: &hermes_core::Session) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn load_session(
            &self,
            _id: &str,
        ) -> hermes_core::Result<Option<hermes_core::Session>> {
            Ok(None)
        }
        async fn list_sessions(
            &self,
            _p: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::Session>> {
            Ok(vec![])
        }
        async fn delete_session(&self, _id: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn search_sessions(
            &self,
            _q: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::Session>> {
            Ok(vec![])
        }
    }

    struct StubSkills;
    #[async_trait::async_trait]
    impl hermes_core::SkillStore for StubSkills {
        async fn list(&self) -> hermes_core::Result<Vec<hermes_core::Skill>> {
            Ok(vec![])
        }
        async fn get(&self, _n: &str) -> hermes_core::Result<Option<hermes_core::Skill>> {
            Ok(None)
        }
        async fn create(
            &self,
            n: &str,
            d: &str,
            b: &str,
        ) -> hermes_core::Result<hermes_core::Skill> {
            Ok(hermes_core::Skill {
                name: n.into(),
                description: d.into(),
                body: b.into(),
                path: Default::default(),
                triggers: vec![],
            })
        }
        async fn update(&self, _n: &str, _b: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _n: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn find_relevant(
            &self,
            _t: &str,
            _l: usize,
        ) -> hermes_core::Result<Vec<hermes_core::Skill>> {
            Ok(vec![])
        }
    }

    fn test_ctx(env: HashMap<String, String>) -> ToolContext {
        let working_dir = std::env::temp_dir();
        ToolContext {
            working_dir: working_dir.clone(),
            session_cwd: std::sync::Arc::new(std::sync::Mutex::new(working_dir)),
            approval: std::sync::Arc::new(hermes_core::AllowAllApproval),
            memory: std::sync::Arc::new(StubMemory),
            skills: std::sync::Arc::new(StubSkills),
            cancel: hermes_core::CancelFlag::new(),
            env,
            allow_outside_workdir: true,
            provider: None,
            model: "test".into(),
        }
    }

    /// Jalankan `env` lewat jalur spawn yang sama dipakai ExecTool, kembalikan
    /// outputnya (tanpa header `[exit: ...]`).
    async fn run_env_child(env: HashMap<String, String>) -> String {
        let ctx = test_ctx(env);
        let tool = ExecTool;
        let args = serde_json::json!({ "command": "env", "timeout_secs": 30 });
        let res = tool.execute(args, &ctx).await.expect("exec env gagal");
        assert!(!res.is_error, "child env gagal: {}", res.content);
        res.content
            .lines()
            .skip_while(|l| l.starts_with("[exit:"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn credential_var_matching_rules() {
        // Sufiks (case-insensitive).
        for n in [
            "OPENAI_API_KEY",
            "my_api_key",
            "DB_PASSWORD",
            "db_passwd",
            "S3_SECRET",
            "id_rsa_PRIVATE_KEY",
            "oauth_CLIENT_SECRET",
            "AUTH_TOKEN",
            "MY_TOKEN",
        ] {
            assert!(is_credential_env_var(n), "{n} harus dianggap kredensial");
        }
        // Nama eksak (case-insensitive).
        for n in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "github_token",
            "TELEGRAM_BOT_TOKEN",
        ] {
            assert!(is_credential_env_var(n), "{n} harus dianggap kredensial");
        }
        // Prefiks (case-insensitive).
        for n in [
            "AWS_REGION",
            "AZURE_TENANT_ID",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "ANTHROPIC_BASE_URL",
            "OPENAI_BASE_URL",
            "XAI_FOO",
        ] {
            assert!(is_credential_env_var(n), "{n} harus dianggap kredensial");
        }
        // Negatif: tidak boleh cocok.
        for n in [
            "PATH",
            "HOME",
            "MY_NORMAL_VAR",
            "API_KEYS_LIST", // "_API_KEY" di tengah, bukan sufiks
            "SECRETARY",
            "PASSWORDLESS",
            "TOKENIZER",
        ] {
            assert!(
                !is_credential_env_var(n),
                "{n} tidak boleh dianggap kredensial"
            );
        }
    }

    #[tokio::test]
    async fn exec_strips_credential_vars_from_ctx_env() {
        let mut env = HashMap::new();
        env.insert("FAKE_API_KEY".into(), "FAKESECRET_A1".into());
        env.insert("MY_TOKEN".into(), "FAKESECRET_T2".into());
        env.insert("AWS_ACCESS_KEY_ID".into(), "FAKESECRET_AWS3".into());
        env.insert("MY_NORMAL_VAR".into(), "keepme_ctx".into());
        let out = run_env_child(env).await;
        assert!(
            !out.lines().any(|l| l.starts_with("FAKE_API_KEY=")),
            "FAKE_API_KEY bocor ke child"
        );
        assert!(!out.contains("FAKESECRET_A1"), "nilai FAKE_API_KEY bocor");
        assert!(
            !out.lines().any(|l| l.starts_with("MY_TOKEN=")),
            "MY_TOKEN bocor ke child"
        );
        assert!(!out.contains("FAKESECRET_T2"), "nilai MY_TOKEN bocor");
        assert!(
            !out.lines().any(|l| l.starts_with("AWS_ACCESS_KEY_ID=")),
            "AWS_ACCESS_KEY_ID bocor ke child"
        );
        assert!(
            !out.contains("FAKESECRET_AWS3"),
            "nilai AWS_ACCESS_KEY_ID bocor"
        );
        assert!(
            out.lines().any(|l| l == "MY_NORMAL_VAR=keepme_ctx"),
            "var normal ctx harus diteruskan"
        );
        assert!(
            out.lines().any(|l| l.starts_with("PATH=")),
            "PATH harus diteruskan"
        );
    }

    #[tokio::test]
    async fn exec_strips_credential_vars_inherited_from_parent() {
        std::env::set_var("HERMES_TEST_PARENT_API_KEY", "FAKESECRET_P1");
        std::env::set_var("HERMES_TEST_PARENT_TOKEN", "FAKESECRET_P2");
        std::env::set_var("HERMES_TEST_PARENT_NORMAL", "parent_keep");
        let out = run_env_child(HashMap::new()).await;
        std::env::remove_var("HERMES_TEST_PARENT_API_KEY");
        std::env::remove_var("HERMES_TEST_PARENT_TOKEN");
        std::env::remove_var("HERMES_TEST_PARENT_NORMAL");
        assert!(
            !out.lines()
                .any(|l| l.starts_with("HERMES_TEST_PARENT_API_KEY=")),
            "kredensial dari parent bocor ke child"
        );
        assert!(!out.contains("FAKESECRET_P1"), "nilai dari parent bocor");
        assert!(
            !out.lines()
                .any(|l| l.starts_with("HERMES_TEST_PARENT_TOKEN=")),
            "kredensial dari parent bocor ke child"
        );
        assert!(!out.contains("FAKESECRET_P2"), "nilai dari parent bocor");
        assert!(
            out.lines()
                .any(|l| l == "HERMES_TEST_PARENT_NORMAL=parent_keep"),
            "var normal parent harus diteruskan"
        );
    }

    #[tokio::test]
    async fn exec_credential_stripping_is_case_insensitive() {
        let mut env = HashMap::new();
        env.insert("my_api_key".into(), "FAKESECRET_C1".into());
        env.insert("Weird_Token".into(), "FAKESECRET_C2".into());
        env.insert("azure_tenant_id".into(), "FAKESECRET_C3".into());
        let out = run_env_child(env).await;
        assert!(!out.contains("FAKESECRET_C1"), "my_api_key bocor");
        assert!(!out.contains("FAKESECRET_C2"), "Weird_Token bocor");
        assert!(!out.contains("FAKESECRET_C3"), "azure_tenant_id bocor");
        assert!(
            !out.lines().any(|l| l.starts_with("my_api_key=")),
            "my_api_key bocor"
        );
    }

    // ---------- B3: alias `timeout` ----------

    #[tokio::test]
    async fn timeout_alias_is_accepted() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let res = tool
            .execute(
                serde_json::json!({"command": "echo ok", "timeout": 30}),
                &ctx,
            )
            .await
            .expect("exec dengan arg `timeout` gagal");
        assert!(!res.is_error, "content: {}", res.content);
        assert!(res.content.contains("ok"), "content: {}", res.content);
    }

    #[tokio::test]
    async fn timeout_alias_wins_over_timeout_secs() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        // `timeout: 1` menang atas `timeout_secs: 600` → sleep 2 kena timeout.
        let err = tool
            .execute(
                serde_json::json!({"command": "sleep 2", "timeout": 1, "timeout_secs": 600}),
                &ctx,
            )
            .await
            .expect_err("seharusnya timeout setelah 1 detik");
        assert!(
            err.to_string().contains("timeout setelah 1 detik"),
            "pesan tak terduga: {err}"
        );
        // Sebaliknya: `timeout: 600` menang atas `timeout_secs: 1`.
        let res = tool
            .execute(
                serde_json::json!({"command": "sleep 2", "timeout": 600, "timeout_secs": 1}),
                &ctx,
            )
            .await
            .expect("exec gagal");
        assert!(!res.is_error, "content: {}", res.content);
    }

    // ---------- B4: promosi background ----------

    #[tokio::test]
    async fn timeout_over_600_promotes_to_background() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let start = std::time::Instant::now();
        let res = tool
            .execute(
                serde_json::json!({"command": "sleep 20", "timeout_secs": 601}),
                &ctx,
            )
            .await
            .expect("exec promosi gagal");
        let elapsed = start.elapsed();
        assert!(!res.is_error, "content: {}", res.content);
        assert!(
            res.content.contains("dipromosikan ke proses background"),
            "content: {}",
            res.content
        );
        // Kembali segera — tidak menunggu `sleep 20` selesai.
        assert!(
            elapsed < std::time::Duration::from_secs(15),
            "tidak kembali segera: {elapsed:?}"
        );
        // Handle proses ada dan terdaftar di registry.
        let id = res
            .content
            .split_whitespace()
            .find(|w| w.starts_with("proc-"))
            .expect("id proses tidak ada di konten")
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .to_string();
        let pm = crate::processes::ProcessManageTool;
        let list = pm
            .execute(serde_json::json!({"action": "list"}), &ctx)
            .await
            .expect("process_manage list gagal");
        assert!(
            list.content.contains(&id),
            "id {id} tidak ada di list: {}",
            list.content
        );
        // Bersihkan prosesnya.
        let _ = pm
            .execute(serde_json::json!({"action": "kill", "id": id}), &ctx)
            .await;
    }

    #[tokio::test]
    async fn timeout_600_stays_foreground() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let res = tool
            .execute(
                serde_json::json!({"command": "echo fg", "timeout": 600}),
                &ctx,
            )
            .await
            .expect("exec gagal");
        assert!(!res.is_error, "content: {}", res.content);
        assert!(
            !res.content.contains("dipromosikan"),
            "batas 600 tidak boleh dipromosikan: {}",
            res.content
        );
        assert!(res.content.contains("fg"), "content: {}", res.content);
    }

    // ---------- B6: truncasi head+tail + spill ----------

    #[tokio::test]
    async fn truncation_keeps_head_and_tail_with_spill() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        // ~118k karakter: jauh di atas batas 50k.
        let res = tool
            .execute(serde_json::json!({"command": "seq 1 20000"}), &ctx)
            .await
            .expect("exec gagal");
        assert!(!res.is_error, "content: {}", res.content);
        let c = &res.content;
        // Head dipertahankan: baris pertama setelah header adalah "1".
        let mut lines = c.lines();
        assert_eq!(lines.next().unwrap(), "[exit: 0]");
        assert_eq!(lines.next().unwrap(), "1");
        // Tail dipertahankan — inilah yang hilang pada perilaku lama.
        assert!(
            c.lines().any(|l| l.trim() == "20000"),
            "tail (baris akhir) hilang"
        );
        // Marker + info spill ada.
        assert!(c.contains("... [truncated"), "marker truncasi hilang");
        let spill_marker = "tersimpan di: ";
        let i = c.find(spill_marker).expect("info spill file hilang");
        let path_str = c[i + spill_marker.len()..]
            .trim_end()
            .trim_end_matches(']')
            .trim()
            .to_string();
        let path = std::path::PathBuf::from(&path_str);
        assert!(path.exists(), "spill file tidak ada: {path_str}");
        let spilled = std::fs::read_to_string(&path).expect("baca spill gagal");
        // Spill berisi output PENUH: head dan tail.
        let total = spilled.chars().count();
        assert!(total > 50_000, "spill terlalu pendek: {total}");
        assert!(
            spilled.lines().any(|l| l.trim() == "1"),
            "head hilang di spill"
        );
        assert!(
            spilled.lines().any(|l| l.trim() == "20000"),
            "tail hilang di spill"
        );
        // Jumlah pada marker konsisten dengan yang dipotong.
        let cut = total - 50_000;
        assert!(
            c.contains(&format!("... [truncated {cut} chars] ...")),
            "marker tidak konsisten dengan isi spill"
        );
        // Teks tampil jauh lebih pendek dari teks penuh.
        assert!(c.chars().count() < total);
        std::fs::remove_file(&path).ok();
    }

    // ---------- B7: `cd` persists ----------

    fn canonical_tmp() -> std::path::PathBuf {
        std::fs::canonicalize("/tmp").expect("/tmp tidak ada")
    }

    /// cwd sesi menurut exec berikutnya (via `pwd`, di-canonicalize agar
    /// kebal symlink maupun perbedaan pwd logis vs fisik).
    async fn exec_pwd(tool: &ExecTool, ctx: &ToolContext) -> std::path::PathBuf {
        let res = tool
            .execute(serde_json::json!({"command": "pwd"}), ctx)
            .await
            .expect("pwd gagal");
        assert!(!res.is_error, "content: {}", res.content);
        let printed = res.content.lines().nth(1).expect("tanpa output").trim();
        std::fs::canonicalize(printed).expect("canonicalize pwd gagal")
    }

    #[tokio::test]
    async fn cd_and_operator_updates_session_cwd() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let target = canonical_tmp();
        let r1 = tool
            .execute(serde_json::json!({"command": "cd /tmp && pwd"}), &ctx)
            .await
            .expect("cd gagal");
        assert!(!r1.is_error, "content: {}", r1.content);
        assert_eq!(exec_pwd(&tool, &ctx).await, target);
        assert_eq!(ctx.current_dir(), target);
    }

    #[tokio::test]
    async fn cd_semicolon_and_bare_cd_update_session_cwd() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let target = canonical_tmp();
        tool.execute(serde_json::json!({"command": "cd /tmp; pwd"}), &ctx)
            .await
            .expect("cd gagal");
        assert_eq!(exec_pwd(&tool, &ctx).await, target);

        let ctx2 = test_ctx(HashMap::new());
        let r = tool
            .execute(serde_json::json!({"command": "cd /tmp"}), &ctx2)
            .await
            .expect("cd saja gagal");
        assert!(!r.is_error, "content: {}", r.content);
        assert_eq!(exec_pwd(&tool, &ctx2).await, target);
    }

    #[tokio::test]
    async fn cd_quoted_path_with_spaces() {
        let dir = std::env::temp_dir().join("hermes cd test");
        std::fs::create_dir_all(&dir).unwrap();
        let target = std::fs::canonicalize(&dir).unwrap();
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let cmd = format!("cd \"{}\" && pwd", dir.display());
        let r = tool
            .execute(serde_json::json!({"command": cmd}), &ctx)
            .await
            .expect("cd quoted gagal");
        assert!(!r.is_error, "content: {}", r.content);
        assert_eq!(exec_pwd(&tool, &ctx).await, target);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cd_failure_returns_clear_error_and_keeps_cwd() {
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let before = ctx.current_dir();
        let marker = std::env::temp_dir().join("hermes-cd-marker-xyz123");
        std::fs::remove_file(&marker).ok();
        let cmd = format!(
            "cd /direktori/yang/pasti/tidak/ada-xyz123 && touch {}",
            marker.display()
        );
        let err = tool
            .execute(serde_json::json!({"command": cmd}), &ctx)
            .await
            .expect_err("cd ke direktori fiktif harus gagal");
        let msg = err.to_string();
        assert!(msg.contains("cd"), "pesan tidak jelas: {msg}");
        assert_eq!(ctx.current_dir(), before, "cwd berubah padahal cd gagal");
        assert!(!marker.exists(), "perintah setelah && ikut dijalankan");
    }

    #[tokio::test]
    async fn cd_relative_resolves_against_session_cwd() {
        let sub = std::env::temp_dir().join("hermes-cd-rel");
        std::fs::create_dir_all(&sub).unwrap();
        let target = std::fs::canonicalize(&sub).unwrap();
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        tool.execute(serde_json::json!({"command": "cd /tmp"}), &ctx)
            .await
            .expect("cd /tmp gagal");
        tool.execute(serde_json::json!({"command": "cd hermes-cd-rel"}), &ctx)
            .await
            .expect("cd relatif gagal");
        assert_eq!(ctx.current_dir(), target);
        std::fs::remove_dir_all(&sub).ok();
    }

    #[test]
    fn parse_leading_cd_cases() {
        assert_eq!(
            parse_leading_cd("cd /tmp"),
            Some(("/tmp".to_string(), "".to_string()))
        );
        assert_eq!(
            parse_leading_cd("cd /tmp && ls"),
            Some(("/tmp".to_string(), "&& ls".to_string()))
        );
        assert_eq!(
            parse_leading_cd("cd /tmp;ls"),
            Some(("/tmp".to_string(), ";ls".to_string()))
        );
        assert_eq!(
            parse_leading_cd("  cd \"/a b\" && ls"),
            Some(("/a b".to_string(), "&& ls".to_string()))
        );
        assert_eq!(
            parse_leading_cd("cd '/a;b'"),
            Some(("/a;b".to_string(), "".to_string()))
        );
        // Bukan pola cd murni → None (dijalankan normal oleh shell).
        assert_eq!(parse_leading_cd("cd /tmp || echo x"), None);
        assert_eq!(parse_leading_cd("cd $HOME"), None);
        assert_eq!(parse_leading_cd("echo cd /tmp"), None);
        assert_eq!(parse_leading_cd("cd"), None);
        assert_eq!(parse_leading_cd("cdx /tmp"), None);
    }

    // ---------- B5: default shell `bash -c` ----------

    #[test]
    fn shell_argv_uses_bash_when_available() {
        let (prog, argv) = shell_argv("echo hi");
        if bash_available() {
            assert_eq!(prog, "bash");
            assert_eq!(argv, vec!["-c".to_string(), "echo hi".to_string()]);
        } else {
            // Fallback terdokumentasi bila bash tak terinstal.
            assert_eq!(prog, "sh");
            assert_eq!(argv, vec!["-c".to_string(), "echo hi".to_string()]);
        }
    }

    #[tokio::test]
    async fn bash_isms_work_through_exec() {
        if !bash_available() {
            // Tanpa bash hanya POSIX dasar yang dijamin; lewati bash-ism.
            return;
        }
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        // `[[ ]]` — gagal di sh/dash.
        let r = tool
            .execute(
                serde_json::json!({"command": "[[ 1 -eq 1 && 'ab' == a* ]] && echo bracket-ok"}),
                &ctx,
            )
            .await
            .expect("exec [[ ]] gagal");
        assert!(!r.is_error, "content: {}", r.content);
        assert!(r.content.contains("bracket-ok"), "content: {}", r.content);
        // Array.
        let r = tool
            .execute(
                serde_json::json!({"command": "a=(x y z); echo \"${a[1]}:${#a[@]}\""}),
                &ctx,
            )
            .await
            .expect("exec array gagal");
        assert!(!r.is_error, "content: {}", r.content);
        assert!(r.content.contains("y:3"), "content: {}", r.content);
        // `(( ))` aritmetika.
        let r = tool
            .execute(
                serde_json::json!({"command": "i=0; ((i++)); ((i+=41)); echo $i"}),
                &ctx,
            )
            .await
            .expect("exec (( )) gagal");
        assert!(!r.is_error, "content: {}", r.content);
        assert!(
            r.content.lines().any(|l| l.trim() == "42"),
            "content: {}",
            r.content
        );
    }

    // ---------- B8: config backend SSH ----------

    fn full_ssh_config() -> ExecConfig {
        ExecConfig {
            ssh_host: "example.com".to_string(),
            ssh_user: "deploy".to_string(),
            ssh_port: 2222,
            ssh_key_path: "/home/u/.ssh/id_ed".to_string(),
            ssh_password_env: String::new(),
        }
    }

    #[test]
    fn ssh_argv_full_config() {
        let (prog, argv) = build_ssh_command(&full_ssh_config(), "echo hi").unwrap();
        assert_eq!(prog, "ssh");
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
                "-p",
                "2222",
                "-i",
                "/home/u/.ssh/id_ed",
                "deploy@example.com",
                "--",
                "bash",
                "-c",
                "echo hi",
            ]
            .into_iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn ssh_argv_defaults_omit_port_key_user() {
        let cfg = ExecConfig {
            ssh_host: "h".to_string(),
            ..Default::default()
        };
        let (prog, argv) = build_ssh_command(&cfg, "id").unwrap();
        assert_eq!(prog, "ssh");
        let argv_str: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            argv_str,
            vec![
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ConnectTimeout=10",
                "h",
                "--",
                "bash",
                "-c",
                "id",
            ]
        );
    }

    #[test]
    fn ssh_argv_password_mode_uses_sshpass_without_batchmode() {
        let cfg = ExecConfig {
            ssh_host: "h".to_string(),
            ssh_password_env: "HERMES_TEST_SSH_PW_XYZ".to_string(),
            ..Default::default()
        };
        let (prog, argv) = build_ssh_command(&cfg, "id").unwrap();
        assert_eq!(prog, "sshpass");
        let argv_str: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
        // argv tidak memuat nama program (prog terpisah): ["-e", "ssh", ...]
        assert_eq!(&argv_str[..2], ["-e", "ssh"]);
        assert!(
            !argv_str.contains(&"BatchMode=yes"),
            "BatchMode=yes mematikan auth password: {argv_str:?}"
        );
    }

    #[test]
    fn ssh_argv_missing_host_is_clear_error() {
        let err = build_ssh_command(&ExecConfig::default(), "id").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ssh_host"), "pesan tidak jelas: {msg}");
        assert!(
            msg.contains("HERMES_SSH_HOST"),
            "pesan tidak memberi tahu cara set: {msg}"
        );
    }

    #[test]
    fn ssh_password_missing_env_is_clear_error_without_value() {
        std::env::remove_var("HERMES_TEST_SSH_PW_XYZ");
        let cfg = ExecConfig {
            ssh_password_env: "HERMES_TEST_SSH_PW_XYZ".to_string(),
            ..Default::default()
        };
        let err = read_ssh_password(&cfg).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("HERMES_TEST_SSH_PW_XYZ"),
            "pesan tidak menyebut nama env: {msg}"
        );
        // Pesan tidak boleh membocorkan nilai apa pun (env memang kosong di sini).
        std::env::set_var("HERMES_TEST_SSH_PW_XYZ", "s3cr3t-dummy");
        let v = read_ssh_password(&cfg).unwrap();
        assert_eq!(v, "s3cr3t-dummy");
        std::env::remove_var("HERMES_TEST_SSH_PW_XYZ");
    }

    #[test]
    fn exec_config_from_env_reads_vars() {
        std::env::set_var("HERMES_TEST2_SSH_HOST", "envhost");
        std::env::set_var("HERMES_SSH_PORT", "2022");
        // from_env membaca HERMES_SSH_*; pakai nama unik via fallback tidak
        // didukung untuk host, jadi uji port + default host di sini.
        let cfg = ExecConfig::from_env();
        assert_eq!(cfg.ssh_port, 2022);
        std::env::remove_var("HERMES_SSH_PORT");
        std::env::remove_var("HERMES_TEST2_SSH_HOST");
        let cfg2 = ExecConfig::from_env();
        assert_eq!(cfg2.ssh_port, 22);
        assert_eq!(cfg2.ssh_host, "");
    }

    #[test]
    fn exec_config_from_env_falls_back_to_terminal_ssh_names() {
        std::env::remove_var("HERMES_SSH_HOST");
        std::env::remove_var("HERMES_SSH_USER");
        std::env::set_var("TERMINAL_SSH_HOST", "orig-host");
        std::env::set_var("TERMINAL_SSH_USER", "orig-user");
        let cfg = ExecConfig::from_env();
        assert_eq!(cfg.ssh_host, "orig-host");
        assert_eq!(cfg.ssh_user, "orig-user");
        // HERMES_SSH_* menang atas TERMINAL_SSH_*.
        std::env::set_var("HERMES_SSH_HOST", "new-host");
        let cfg2 = ExecConfig::from_env();
        assert_eq!(cfg2.ssh_host, "new-host");
        assert_eq!(cfg2.ssh_user, "orig-user");
        std::env::remove_var("TERMINAL_SSH_HOST");
        std::env::remove_var("TERMINAL_SSH_USER");
        std::env::remove_var("HERMES_SSH_HOST");
    }

    #[tokio::test]
    async fn ssh_backend_without_host_fails_clearly() {
        std::env::remove_var("HERMES_SSH_HOST");
        std::env::remove_var("TERMINAL_SSH_HOST");
        let ctx = test_ctx(HashMap::new());
        let tool = ExecTool;
        let err = tool
            .execute(
                serde_json::json!({"command": "id", "backend": "ssh", "timeout": 5}),
                &ctx,
            )
            .await
            .expect_err("ssh tanpa host harus gagal");
        let msg = err.to_string();
        assert!(msg.contains("ssh_host"), "pesan tidak jelas: {msg}");
        assert!(msg.contains("HERMES_SSH_HOST"), "pesan: {msg}");
    }
}

#[cfg(test)]
mod redact_tests {
    use super::redact_secrets;
    use super::{interpret_exit_code, resolve_exec_timeout, ExecTool};
    use hermes_core::Tool;

    /// Rakit nilai rahasia palsu secara programatik agar tidak ada literal
    /// berbentuk-secret di source (pemindai secret otomatis menyuntingnya).
    fn fake(prefix_parts: &[&str]) -> String {
        prefix_parts.concat()
    }

    #[test]
    fn redact_key_value_pairs() {
        // api_key dengan = (case-insensitive, nama kunci dipertahankan).
        let v1 = fake(&["sk-live-", "FA", "KE123"]);
        assert_eq!(
            redact_secrets(&format!("api_key={v1}")),
            "api_key=[REDACTED]"
        );
        // Bentuk hyphen + colon + spasi.
        let v2 = fake(&["hun", "ter2"]);
        assert_eq!(
            redact_secrets(&format!("API-KEY: {v2}")),
            "API-KEY: [REDACTED]"
        );
        // Quoted value.
        assert_eq!(
            redact_secrets(r#"password = "hunter2""#),
            "password = [REDACTED]"
        );
        // Compound name (client_secret).
        let v3 = fake(&["FA", "KEabc"]);
        assert_eq!(
            redact_secrets(&format!("client_secret={v3}")),
            "client_secret=[REDACTED]"
        );
        // passwd variant.
        assert_eq!(
            redact_secrets("db passwd: s3cr3t!"),
            "db passwd: [REDACTED]"
        );
    }

    #[test]
    fn redact_bearer_authorization_header() {
        let tok = fake(&["FA", "KEtok123"]);
        let out = redact_secrets(&format!("Authorization: Bearer {tok}"));
        assert_eq!(out, "Authorization: Bearer [REDACTED]");
        assert!(!out.contains(&tok));
    }

    #[test]
    fn redact_aws_access_key_id() {
        let key = format!("AKIA{}", "0".repeat(16));
        let out = redact_secrets(&format!("using key {key} for s3"));
        assert_eq!(out, "using key [REDACTED] for s3");
    }

    #[test]
    fn redact_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
                   eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
                   SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let out = redact_secrets(&format!("got token {jwt} ok"));
        assert_eq!(out, "got token [REDACTED] ok");
        assert!(!out.contains("eyJ"));
    }

    #[test]
    fn redact_slack_tokens() {
        let t1 = fake(&["xoxb-", "12345", "6789012"]);
        assert_eq!(redact_secrets(&t1), "[REDACTED]");
        let t2 = fake(&["xoxp-", "999-", "abc"]);
        assert_eq!(redact_secrets(&t2), "[REDACTED]");
    }

    #[test]
    fn redact_github_tokens() {
        let t1 = format!("ghp_{}", "a".repeat(20));
        assert_eq!(redact_secrets(&t1), "[REDACTED]");
        let t2 = format!("gho_{}", "b".repeat(20));
        assert_eq!(redact_secrets(&t2), "[REDACTED]");
    }

    #[test]
    fn redact_multiple_and_idempotent() {
        let v1 = fake(&["FA", "KEAAA111"]);
        let v2 = fake(&["FA", "KEBBB222"]);
        let aws = format!("AKIA{}", "1".repeat(16));
        let slack = fake(&["xoxb-", "1", "-2"]);
        let raw = format!("api_key={v1}\ntoken: {v2}\n{aws}\n{slack}");
        let once = redact_secrets(&raw);
        for leaked in [&v1, &v2, &aws, &slack] {
            assert!(!once.contains(leaked.as_str()), "nilai bocor: {once}");
        }
        assert_eq!(once.matches("[REDACTED]").count(), 4);
        // Redaksi ulang tidak mengubah hasil (idempoten).
        assert_eq!(redact_secrets(&once), once);
    }

    #[test]
    fn passthrough_normal_output() {
        // Path, kode, dan prosa tanpa pola rahasia harus lolos utuh.
        let cases = [
            "/home/chewy/workspace/hermes-rs/Cargo.toml",
            "fn main() { println!(\"hello\"); }",
            "[exit: 0]\nDeploy finished in 42s; see /var/log/deploy.log.",
            "Rotate the token regularly and never share it.",
            "status: ok\nlines: 120",
        ];
        for c in cases {
            assert_eq!(redact_secrets(c), c, "terubah padahal aman: {c}");
        }
    }

    // --- Batch 8: paritas minor timeout + catatan exit-code (vs terminal_tool) ---

    #[test]
    fn exec_timeout_default_180() {
        // Default 180 seperti TERMINAL_TIMEOUT aslinya.
        let t = resolve_exec_timeout(&serde_json::json!({})).unwrap();
        assert_eq!(t, 180);
    }

    #[test]
    fn exec_timeout_alias_timeout_secs() {
        let t = resolve_exec_timeout(&serde_json::json!({"timeout_secs": 30})).unwrap();
        assert_eq!(t, 30);
        // Bila keduanya ada, `timeout` menang.
        let t =
            resolve_exec_timeout(&serde_json::json!({"timeout": 45, "timeout_secs": 30})).unwrap();
        assert_eq!(t, 45);
    }

    #[test]
    fn exec_timeout_nol_ditolak_bukan_clamp() {
        // timeout <= 0 adalah error (bukan clamp ke 1), seperti aslinya.
        for v in [0, -5] {
            let err = resolve_exec_timeout(&serde_json::json!({"timeout": v})).unwrap_err();
            assert!(
                err.to_string()
                    .contains("timeout must be a positive number of seconds"),
                "pesan salah: {err}"
            );
        }
    }

    #[test]
    fn exec_schema_menyebut_default_180() {
        let s = ExecTool.parameters_schema().to_string();
        assert!(
            s.contains("default 180"),
            "skema harus menyebut default 180"
        );
    }

    #[test]
    fn exit_note_nol_tanpa_catatan() {
        assert_eq!(interpret_exit_code("ls", 0), None);
    }

    #[test]
    fn exit_note_grep_tanpa_cocokan_bukan_error() {
        let note = interpret_exit_code("grep foo bar.txt", 1).unwrap();
        assert!(note.contains("No matches found"), "catatan salah: {note}");
        // env assignment di awal tidak mengacaukan deteksi perintah dasar.
        let note = interpret_exit_code("FOO=1 rg pat .", 1).unwrap();
        assert!(note.contains("No matches found"), "catatan salah: {note}");
    }

    #[test]
    fn exit_note_semantik_perintah_dasar() {
        assert!(interpret_exit_code("diff a b", 1)
            .unwrap()
            .contains("Files differ"));
        assert!(interpret_exit_code("test -f x", 1)
            .unwrap()
            .contains("false"));
        assert!(interpret_exit_code("curl https://x", 28)
            .unwrap()
            .contains("timed out"));
        assert!(interpret_exit_code("git diff --stat", 1)
            .unwrap()
            .contains("git diff"));
        // Segmen terakhir pipeline yang menentukan exit code.
        assert!(interpret_exit_code("cat f | grep x", 1)
            .unwrap()
            .contains("No matches found"));
    }

    #[test]
    fn exit_note_terminasi_sinyal() {
        // Kode negatif subprocess = pasti sinyal.
        let note = interpret_exit_code("sleep 99", -9).unwrap();
        assert!(note.contains("signal 9"), "catatan salah: {note}");
        assert!(note.contains("SIGKILL"), "catatan salah: {note}");
        // Konvensi shell 128+signum = heuristik ("usually"), mis. 137 = OOM kill.
        let note = interpret_exit_code("sleep 99", 137).unwrap();
        assert!(note.contains("usually means"), "catatan salah: {note}");
        assert!(note.contains("SIGKILL"), "catatan salah: {note}");
        // SIGINT milik jalur interrupt-marker: tanpa catatan.
        assert_eq!(interpret_exit_code("sleep 99", -2), None);
    }

    #[test]
    fn exit_note_kode_tak_dikenal_tanpa_catatan() {
        assert_eq!(interpret_exit_code("myapp", 3), None);
        assert_eq!(interpret_exit_code("", 1), None);
    }
}
