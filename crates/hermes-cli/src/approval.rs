//! Handler persetujuan eksekusi tool.
//!
//! Meniru model keamanan bertingkat Hermes asli: setiap tool berisiko
//! (terutama `exec` dan tulis file) meminta persetujuan eksplisit kecuali
//! pengguna memilih mode --yes / allow.
use hermes_core::{
    ApprovalDecision, ApprovalHandler, ApprovalMode, RiskLevel, SecurityConfig, ToolCall,
};
use std::io::{self, Write};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// Interaktif: tanya bila perlu.
    Interactive,
    /// Setujui semua (flag --yes).
    Yes,
    /// Tolak semua yang berisiko (read-only).
    ReadOnly,
}

/// Persetujuan interaktif via prompt terminal.
pub struct InteractiveApproval {
    kind: ApprovalKind,
    security: SecurityConfig,
    /// Bila pengguna memilih "allow always" untuk sesi ini.
    allow_all: Mutex<bool>,
}

impl InteractiveApproval {
    pub fn new(kind: ApprovalKind, security: SecurityConfig) -> Self {
        Self {
            kind,
            security,
            allow_all: Mutex::new(false),
        }
    }

    fn mode_for(&self, call: &ToolCall) -> ApprovalMode {
        for rule in &self.security.rules {
            if glob_simple(&rule.pattern, &call.name) {
                return rule.mode;
            }
        }
        self.security.default_mode
    }
}

/// Pencocokan pola sederhana: `*` di akhir = prefix.
fn glob_simple(pattern: &str, name: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        name.starts_with(prefix)
    } else {
        pattern == name
    }
}

impl ApprovalHandler for InteractiveApproval {
    fn approve(&self, call: &ToolCall, risk: RiskLevel) -> ApprovalDecision {
        if *self.allow_all.lock().unwrap() {
            return ApprovalDecision::Allow;
        }
        match self.kind {
            ApprovalKind::Yes => return ApprovalDecision::Allow,
            ApprovalKind::ReadOnly => {
                return match risk {
                    RiskLevel::Low => ApprovalDecision::Allow,
                    _ => ApprovalDecision::Deny,
                }
            }
            ApprovalKind::Interactive => {}
        }
        let mode = self.mode_for(call);
        match mode {
            ApprovalMode::Allow => ApprovalDecision::Allow,
            ApprovalMode::Deny => {
                eprintln!("⛔ ditolak oleh kebijakan: {}", call.name);
                ApprovalDecision::Deny
            }
            ApprovalMode::Ask => {
                if risk == RiskLevel::Low {
                    return ApprovalDecision::Allow;
                }
                // Timeout diambil dari config (SecurityConfig::approval_timeout_secs,
                // default 300 dtk): tanpa jawaban dalam batas waktu -> DITOLAK.
                ask_user(
                    call,
                    risk,
                    &self.allow_all,
                    Duration::from_secs(self.security.approval_timeout_secs),
                )
            }
        }
    }
}

/// Baca satu baris dari stdin. Dipisah sebagai fungsi agar alur timeout
/// bisa diuji tanpa stdin sungguhan (lihat `ask_user_with_reader`).
fn read_stdin_line() -> io::Result<String> {
    let mut input = String::new();
    io::stdin().read_line(&mut input).map(|_| input)
}

fn ask_user(
    call: &ToolCall,
    risk: RiskLevel,
    allow_all: &Mutex<bool>,
    timeout: Duration,
) -> ApprovalDecision {
    ask_user_with_reader(call, risk, allow_all, timeout, read_stdin_line)
}

/// Prompt persetujuan dengan batas waktu (audit W6-B15): pembacaan jawaban
/// berjalan di thread terpisah dan thread utama menunggu paling lama
/// `timeout`. Bila tidak ada jawaban dalam batas waktu (atau pembacaan
/// gagal), permintaan DITOLAK — fail-closed, agar proses tanpa pengawasan
/// tidak menggantung selamanya.
///
/// `read` di-inject agar uji bisa memakai pembaca tiruan yang memblokir.
fn ask_user_with_reader<F>(
    call: &ToolCall,
    risk: RiskLevel,
    allow_all: &Mutex<bool>,
    timeout: Duration,
    read: F,
) -> ApprovalDecision
where
    F: FnOnce() -> io::Result<String> + Send + 'static,
{
    let risk_label = match risk {
        RiskLevel::Medium => "sedang",
        RiskLevel::High => "TINGGI",
        RiskLevel::Low => "rendah",
    };
    let args_preview: String = serde_json::to_string(&call.arguments)
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    let secs = timeout.as_secs();
    eprint!(
        "\n⚠️  Izin eksekusi (risiko {risk_label}):\n   {} {}\n   [y]a  [n]tidak  [a]selalu izinkan sesi ini (batas {secs} dtk): ",
        call.name, args_preview
    );
    let _ = io::stderr().flush();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        // Hasil kirim boleh gagal: penerima mungkin sudah menyerah karena timeout.
        let _ = tx.send(read());
    });
    let input = match rx.recv_timeout(timeout) {
        Ok(Ok(input)) => input,
        Ok(Err(e)) => {
            eprintln!("\n⚠️  gagal membaca jawaban persetujuan ({e}) — ditolak (fail-closed).");
            return ApprovalDecision::Deny;
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            eprintln!(
                "\n⏱️  batas waktu persetujuan ({secs} dtk) terlampaui tanpa jawaban — ditolak (fail-closed)."
            );
            return ApprovalDecision::Deny;
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            eprintln!("\n⚠️  pembaca persetujuan berhenti — ditolak (fail-closed).");
            return ApprovalDecision::Deny;
        }
    };
    match input.trim().to_lowercase().as_str() {
        "y" | "ya" | "yes" => ApprovalDecision::Allow,
        "a" | "always" | "selalu" => {
            *allow_all.lock().unwrap() = true;
            ApprovalDecision::Allow
        }
        _ => ApprovalDecision::Deny,
    }
}

/// Persetujuan untuk gateway/cron (non-interaktif): izinkan risiko rendah+sedang,
/// tolak risiko tinggi — dapat diubah via HERMES_GATEWAY_APPROVAL=allow|strict.
pub struct GatewayApproval {
    allow_medium: bool,
}

impl GatewayApproval {
    pub fn from_env() -> Self {
        let mode = std::env::var("HERMES_GATEWAY_APPROVAL").unwrap_or_default();
        Self {
            allow_medium: mode != "strict",
        }
    }
}

/// Pilih handler persetujuan untuk perintah non-interaktif (`gateway`, `batch`):
/// flag CLI eksplisit (--yes / --readonly) MENANG atas env; `from_env()`
/// hanya dipakai sebagai default bila TIDAK ada flag yang diberikan.
///
/// - `ApprovalKind::Yes` → semua persetujuan otomatis diberikan.
/// - `ApprovalKind::ReadOnly` → semua aksi berisiko (di atas Low) ditolak.
/// - `ApprovalKind::Interactive` (tanpa flag) → default env via `from_env()`.
pub fn unattended_approval(
    kind: ApprovalKind,
    security: &SecurityConfig,
) -> Arc<dyn ApprovalHandler> {
    match kind {
        ApprovalKind::Interactive => Arc::new(GatewayApproval::from_env()),
        other => Arc::new(InteractiveApproval::new(other, security.clone())),
    }
}

impl ApprovalHandler for GatewayApproval {
    fn approve(&self, call: &ToolCall, risk: RiskLevel) -> ApprovalDecision {
        match risk {
            RiskLevel::Low => ApprovalDecision::Allow,
            RiskLevel::Medium if self.allow_medium => {
                tracing::info!(tool = %call.name, "gateway: tool risiko sedang diizinkan");
                ApprovalDecision::Allow
            }
            _ => {
                tracing::warn!(tool = %call.name, "gateway: tool berisiko ditolak");
                ApprovalDecision::Deny
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::ApprovalMode;

    /// Serialisasi uji yang memutasi env (env bersifat global per-proses).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            arguments: serde_json::json!({}),
        }
    }

    /// SecurityConfig yang akan menanyakan (Ask) untuk tool tulis/exec.
    fn strict_security() -> SecurityConfig {
        SecurityConfig {
            default_mode: ApprovalMode::Ask,
            rules: vec![],
            allow_outside_workdir: false,
            approval_timeout_secs: hermes_core::DEFAULT_APPROVAL_TIMEOUT_SECS,
        }
    }

    /// (1) Flag --yes menang atas env: bahkan dengan env "strict", semua disetujui.
    #[test]
    fn flag_yes_beats_strict_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("HERMES_GATEWAY_APPROVAL", "strict");
        let h = unattended_approval(ApprovalKind::Yes, &strict_security());
        for risk in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High] {
            assert_eq!(
                h.approve(&call("exec"), risk),
                ApprovalDecision::Allow,
                "--yes harus menyetujui risiko {risk:?} walau env strict"
            );
        }
        std::env::remove_var("HERMES_GATEWAY_APPROVAL");
    }

    /// (1) Flag --readonly menang atas env: env default mengizinkan Medium,
    /// flag tetap menolaknya.
    #[test]
    fn flag_readonly_beats_permissive_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_GATEWAY_APPROVAL"); // default: Medium diizinkan
        let h = unattended_approval(ApprovalKind::ReadOnly, &strict_security());
        assert_eq!(
            h.approve(&call("exec"), RiskLevel::Low),
            ApprovalDecision::Allow
        );
        assert_eq!(
            h.approve(&call("exec"), RiskLevel::Medium),
            ApprovalDecision::Deny,
            "--readonly harus menolak Medium walau env mengizinkannya"
        );
        assert_eq!(
            h.approve(&call("write_file"), RiskLevel::High),
            ApprovalDecision::Deny
        );
    }

    /// (2) Tanpa flag: default env berlaku (default mengizinkan Medium,
    /// "strict" hanya mengizinkan Low).
    #[test]
    fn no_flags_uses_env_defaults() {
        let _g = ENV_LOCK.lock().unwrap();
        let tool = call("exec");
        std::env::remove_var("HERMES_GATEWAY_APPROVAL");
        let h = unattended_approval(ApprovalKind::Interactive, &strict_security());
        assert_eq!(h.approve(&tool, RiskLevel::Low), ApprovalDecision::Allow);
        assert_eq!(h.approve(&tool, RiskLevel::Medium), ApprovalDecision::Allow);
        assert_eq!(h.approve(&tool, RiskLevel::High), ApprovalDecision::Deny);

        std::env::set_var("HERMES_GATEWAY_APPROVAL", "strict");
        let h = unattended_approval(ApprovalKind::Interactive, &strict_security());
        assert_eq!(h.approve(&tool, RiskLevel::Low), ApprovalDecision::Allow);
        assert_eq!(
            h.approve(&tool, RiskLevel::Medium),
            ApprovalDecision::Deny,
            "env strict harus menolak Medium"
        );
        assert_eq!(h.approve(&tool, RiskLevel::High), ApprovalDecision::Deny);
        std::env::remove_var("HERMES_GATEWAY_APPROVAL");
    }

    /// (3) --readonly: tidak ada tool tulis (risiko di atas Low) yang disetujui.
    #[test]
    fn readonly_denies_write_actions() {
        let h = unattended_approval(ApprovalKind::ReadOnly, &strict_security());
        assert_eq!(
            h.approve(&call("read_file"), RiskLevel::Low),
            ApprovalDecision::Allow
        );
        for (name, risk) in [
            ("exec", RiskLevel::Medium),
            ("exec", RiskLevel::High),
            ("write_file", RiskLevel::High),
            ("edit_file", RiskLevel::Medium),
        ] {
            assert_eq!(
                h.approve(&call(name), risk),
                ApprovalDecision::Deny,
                "--readonly harus menolak {name} risiko {risk:?}"
            );
        }
    }

    /// (4) --yes: menyetujui otomatis semua prompt yang seharusnya ditanyakan
    /// (tanpa menyentuh stdin).
    #[test]
    fn yes_auto_approves_askable_prompts() {
        // SecurityConfig::default() memetakan exec/write_file/edit_file ke Ask.
        let h = unattended_approval(ApprovalKind::Yes, &SecurityConfig::default());
        for name in ["exec", "write_file", "edit_file"] {
            for risk in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High] {
                assert_eq!(
                    h.approve(&call(name), risk),
                    ApprovalDecision::Allow,
                    "--yes harus menyetujui {name} risiko {risk:?}"
                );
            }
        }
    }

    /// (4b) Tanpa flag + mode Interactive tetap menanyakan (tidak otomatis):
    /// di sini diuji melalui default env — Medium lolos karena non-interaktif,
    /// bukan karena flag.
    #[test]
    fn unattended_default_allows_low_and_medium_only() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_GATEWAY_APPROVAL");
        let h = unattended_approval(ApprovalKind::Interactive, &strict_security());
        assert_eq!(
            h.approve(&call("exec"), RiskLevel::High),
            ApprovalDecision::Deny
        );
        std::env::remove_var("HERMES_GATEWAY_APPROVAL");
    }

    /// (5) W6-B15: tanpa jawaban dalam batas waktu -> DITOLAK (fail-closed),
    /// dan pemanggil tidak menggantung selamanya. Pembaca tiruan memblokir
    /// selamanya (channel yang tak pernah dikirimi pesan).
    #[test]
    fn approval_timeout_denies_when_no_answer() {
        let allow_all = Mutex::new(false);
        let (_tx, rx) = std::sync::mpsc::channel::<()>();
        let start = std::time::Instant::now();
        let decision = ask_user_with_reader(
            &call("exec"),
            RiskLevel::High,
            &allow_all,
            Duration::from_secs(1),
            move || {
                rx.recv()
                    .map(|()| "y".to_string())
                    .map_err(|_| io::Error::other("channel ditutup"))
            },
        );
        let elapsed = start.elapsed();
        assert_eq!(
            decision,
            ApprovalDecision::Deny,
            "tanpa jawaban dalam 1 dtk harus ditolak (fail-closed)"
        );
        assert!(
            elapsed >= Duration::from_secs(1),
            "harus menunggu sampai batas waktu: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "harus kembali setelah timeout, bukan menggantung: {elapsed:?}"
        );
    }

    /// (5b) W6-B15: jawaban yang datang SEBELUM timeout tetap dihormati
    /// (timeout tidak merusak alur interaktif normal).
    #[test]
    fn approval_answer_before_timeout_still_honored() {
        let allow_all = Mutex::new(false);
        let decision = ask_user_with_reader(
            &call("exec"),
            RiskLevel::High,
            &allow_all,
            Duration::from_secs(5),
            || Ok("y".to_string()),
        );
        assert_eq!(decision, ApprovalDecision::Allow);

        let decision = ask_user_with_reader(
            &call("exec"),
            RiskLevel::High,
            &allow_all,
            Duration::from_secs(5),
            || Ok("n".to_string()),
        );
        assert_eq!(decision, ApprovalDecision::Deny);
    }

    /// (5c) W6-B15: timeout 0 = tolak langsung tanpa menunggu (fail-closed
    /// paling ketat).
    #[test]
    fn approval_zero_timeout_denies_immediately() {
        let allow_all = Mutex::new(false);
        let start = std::time::Instant::now();
        let decision = ask_user_with_reader(
            &call("exec"),
            RiskLevel::High,
            &allow_all,
            Duration::ZERO,
            || Ok("y".to_string()),
        );
        assert_eq!(decision, ApprovalDecision::Deny);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timeout 0 harus menolak tanpa menunggu"
        );
    }

    /// (5d) W6-B15: nilai config default 300 dtk, dan field ikut ter-parse
    /// dari TOML (wiring config -> handler).
    #[test]
    fn approval_timeout_config_default_and_parse() {
        assert_eq!(
            hermes_core::DEFAULT_APPROVAL_TIMEOUT_SECS,
            300,
            "default timeout harus 300 dtk"
        );
        assert_eq!(SecurityConfig::default().approval_timeout_secs, 300);
        let cfg: SecurityConfig =
            toml::from_str("approval_timeout_secs = 42").expect("parse TOML security");
        assert_eq!(cfg.approval_timeout_secs, 42);
        // Field absen -> default 300 (kompatibel dengan config lama).
        let cfg: SecurityConfig = toml::from_str("").expect("parse TOML kosong");
        assert_eq!(cfg.approval_timeout_secs, 300);
    }
}
