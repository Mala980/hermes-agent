use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Mode persetujuan untuk eksekusi tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Tanya pengguna setiap kali (default, paling aman).
    #[default]
    Ask,
    /// Izinkan semua tanpa bertanya.
    Allow,
    /// Tolak semua (read-only).
    Deny,
}

/// Batas waktu default (detik) menunggu jawaban pada prompt persetujuan
/// interaktif sebelum permintaan ditolak (fail-closed, audit W6-B15).
pub const DEFAULT_APPROVAL_TIMEOUT_SECS: u64 = 300;

fn default_approval_timeout_secs() -> u64 {
    DEFAULT_APPROVAL_TIMEOUT_SECS
}

/// Kebijakan keamanan eksekusi tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    #[serde(default)]
    pub default_mode: ApprovalMode,
    /// Aturan per pola nama tool, mis. "exec:rm *" -> "deny".
    #[serde(default)]
    pub rules: Vec<ApprovalRule>,
    /// Izinkan tool menulis di luar working dir.
    #[serde(default)]
    pub allow_outside_workdir: bool,
    /// Batas waktu (detik) menunggu jawaban pada prompt persetujuan
    /// interaktif; lewat batas tanpa jawaban -> DITOLAK (fail-closed).
    /// 0 = tolak langsung tanpa menunggu. Dapat disetel lewat config,
    /// mis. `[profiles.default.security] approval_timeout_secs = 600`.
    #[serde(default = "default_approval_timeout_secs")]
    pub approval_timeout_secs: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            default_mode: ApprovalMode::Ask,
            rules: vec![
                ApprovalRule {
                    pattern: "exec".to_string(),
                    mode: ApprovalMode::Ask,
                },
                ApprovalRule {
                    pattern: "write_file".to_string(),
                    mode: ApprovalMode::Ask,
                },
                ApprovalRule {
                    pattern: "edit_file".to_string(),
                    mode: ApprovalMode::Ask,
                },
            ],
            allow_outside_workdir: false,
            approval_timeout_secs: DEFAULT_APPROVAL_TIMEOUT_SECS,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRule {
    pub pattern: String,
    pub mode: ApprovalMode,
}

/// Konfigurasi satu provider LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Nama logis: "openrouter", "openai", "nous", "ollama", ...
    pub name: String,
    /// Tipe protokol: "openai" (OpenAI-compatible) atau "anthropic".
    #[serde(default = "default_protocol")]
    pub protocol: String,
    pub base_url: String,
    /// Nama env var yang menyimpan API key, mis. "OPENROUTER_API_KEY".
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Daftar API key untuk rotasi (credential pool).
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub default_model: Option<String>,
}

fn default_protocol() -> String {
    "openai".to_string()
}

/// Satu profil Hermes: instansi independen dengan config/skills/memory sendiri.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(default = "default_profile_name")]
    pub name: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_workdir")]
    pub working_dir: String,
    #[serde(default)]
    pub system_prompt_extra: String,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    /// Batas output per turn. `None` (default) = otomatis dari ceiling
    /// per-model (`max_output_ceiling_for_model`); `Some(v)` = eksplisit dan
    /// selalu menang, tetapi dibatasi ceiling (`min(config, ceiling)`).
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: f32,
    /// Pemicu kompresi konteks absolut (estimasi token). 0 = otomatis: diturunkan dari
    /// `compact_threshold_ratio` × context window model (semantik original
    /// `compression.threshold` / `compression.threshold_tokens`). Nilai > 0 dipakai
    /// apa adanya (kompatibel dengan config lama yang menyetel angka fixed).
    #[serde(default)]
    pub compact_threshold: usize,
    /// Rasio ambang kompresi terhadap context window model (default 0.50 — sama seperti
    /// original `compression.threshold`). Window < 512K dinaikkan (raise-only) ke 0.75.
    #[serde(default = "default_compact_threshold_ratio")]
    pub compact_threshold_ratio: f64,
    /// Context window model dalam token. None = diturunkan dari tabel per-model
    /// `context_window_for_model` (fallback 128K untuk model tak dikenal).
    #[serde(default)]
    pub context_window: Option<usize>,
    /// Model LLM khusus untuk kompresi konteks (auxiliary, paritas dengan
    /// `auxiliary.compression.model` di original). Kosong = pakai model utama.
    /// Nama `compression_model` juga diterima (alias config).
    #[serde(default, alias = "compression_model")]
    pub compact_model: String,
    /// Nyalakan learning loop (pembuatan skill otomatis).
    #[serde(default = "default_true")]
    pub learning_loop: bool,
    #[serde(default)]
    pub security: SecurityConfig,
    /// File konteks proyek yang selalu dimuat (AGENTS.md, SOUL.md, ...).
    #[serde(default = "default_context_files")]
    pub context_files: Vec<String>,
    /// Env tambahan untuk tool.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Upaya reasoning default profil (`none`, `minimal`, `low`, `medium`,
    /// `high`, `xhigh`, `max`, `ultra`; paritas `agent.reasoning_effort` di
    /// original). `None` = default provider. Flag CLI `--reasoning`
    /// menimpanya untuk sekali jalan tanpa menulis config.
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// Batas waktu (detik) per batch tool call dalam satu putaran tool:
    /// satu tool yang hang tidak boleh menahan turn selamanya (paritas
    /// `timeouts.tools.concurrent_batch` di original, default 420s).
    /// Saat deadline lewat, call yang belum selesai ditandai timeout
    /// (hasil error jelas) dan batch lanjut dengan sisanya — future yang
    /// macet dibatalkan tanpa ditunggu, seperti `DaemonThreadPoolExecutor`
    /// di original yang melepas worker macet tanpa join. 0 = nonaktif
    /// (tanpa deadline, perilaku lama). Dapat disetel lewat config,
    /// mis. `[profiles.default] tool_batch_timeout_secs = 600`.
    #[serde(default = "default_tool_batch_timeout_secs")]
    pub tool_batch_timeout_secs: u64,
}

/// Batas waktu default (detik) per batch tool call — paritas
/// `_DEFAULT_CONCURRENT_TOOL_TIMEOUT_S = 420.0` di
/// `agent/tool_executor.py` original ("Generous: slow-but-valid tool work
/// must never be preempted by the batch guard").
pub const DEFAULT_TOOL_BATCH_TIMEOUT_SECS: u64 = 420;

fn default_tool_batch_timeout_secs() -> u64 {
    DEFAULT_TOOL_BATCH_TIMEOUT_SECS
}

fn default_profile_name() -> String {
    "default".to_string()
}
fn default_workdir() -> String {
    ".".to_string()
}
fn default_max_iterations() -> u32 {
    // Paritas original: maksimum iterasi tool-calling per turn default 500
    // (lihat --max-turns di hermes_cli/_parser.py dan docstring
    // agent/iteration_budget.py).
    500
}
fn default_compact_threshold() -> usize {
    // 0 = otomatis (rasio × context window), bukan fixed 100K.
    // Lihat ProfileConfig::compaction_trigger_tokens.
    0
}
fn default_compact_threshold_ratio() -> f64 {
    0.50
}
fn default_true() -> bool {
    true
}
fn default_context_files() -> Vec<String> {
    vec![
        "SOUL.md".to_string(),
        "AGENTS.md".to_string(),
        "USER.md".to_string(),
        "MEMORY.md".to_string(),
    ]
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            name: default_profile_name(),
            provider: "openrouter".to_string(),
            model: "nousresearch/hermes-4-70b".to_string(),
            working_dir: default_workdir(),
            system_prompt_extra: String::new(),
            max_iterations: default_max_iterations(),
            max_tokens: None,
            temperature: 0.7,
            compact_threshold: default_compact_threshold(),
            compact_threshold_ratio: default_compact_threshold_ratio(),
            context_window: None,
            compact_model: String::new(),
            learning_loop: true,
            security: SecurityConfig::default(),
            context_files: default_context_files(),
            env: HashMap::new(),
            reasoning_effort: None,
            tool_batch_timeout_secs: default_tool_batch_timeout_secs(),
        }
    }
}

impl ProfileConfig {
    /// Perkiraan context window (token) dari nama model — katalog kecil
    /// per-keluarga, paritas dengan katalog keluarga model hardcoded di
    /// original (`agent/model_metadata.py`, #83324).
    ///
    /// `atmo/*` dan model tak dikenal → 128K (default konservatif).
    /// Nilai eksplisit `context_window` di config selalu menang atas tabel ini.
    pub fn context_window_for_model(model: &str) -> usize {
        let name = model.to_lowercase();
        let mut parts = name.split('/');
        let namespace = parts.next().unwrap_or("");
        // Namespace `atmo/*` = model AtmoRouter (deepseek dkk.): 128K.
        if namespace == "atmo" {
            return 128_000;
        }
        let base = name.rsplit('/').next().unwrap_or(&name);
        if base.starts_with("claude-") {
            200_000
        } else if base.starts_with("gpt-") {
            128_000
        } else if base.starts_with("gemini-") {
            1_000_000
        } else {
            128_000
        }
    }

    /// Ceiling output-token per-model untuk `max_tokens` — pola yang sama
    /// dengan `context_window_for_model`. Model tak dikenal -> 8192
    /// (perilaku lama dipertahankan sebagai lantai).
    pub fn max_output_ceiling_for_model(model: &str) -> u32 {
        let name = model.to_lowercase();
        let base = name.rsplit('/').next().unwrap_or(&name);
        if base.starts_with("claude-") {
            64_000
        } else if base.starts_with("gpt-4o") || base.starts_with("gpt-4.1") {
            128_000
        } else if base.starts_with("gemini-") {
            65_536
        } else if base.starts_with("deepseek") {
            64_000
        } else {
            8_192
        }
    }

    /// `max_tokens` efektif untuk turn LLM: nilai eksplisit di config selalu
    /// menang atas default, tetapi tidak pernah melewati ceiling per-model
    /// (`min(config, ceiling)`).
    pub fn effective_max_tokens(&self, model: &str) -> u32 {
        let ceiling = Self::max_output_ceiling_for_model(model);
        match self.max_tokens {
            Some(v) => v.min(ceiling),
            None => ceiling,
        }
    }

    /// Token pemicu kompresi konteks — padanan `ContextCompressor.threshold_tokens`
    /// di original (bukan angka fixed).
    ///
    /// Bila `compact_threshold` = 0 (default), pemicu diturunkan dari
    /// `compact_threshold_ratio` × (context_window − max_tokens), dengan aturan
    /// yang sama seperti original:
    /// - lantai raise-only 0.75 untuk window < 512K (`_SMALL_CTX_*`),
    /// - lantai minimum 64K token (`MINIMUM_CONTEXT_LENGTH`),
    /// - batas 85% dari budget input efektif saat lantai minimum yang mengikat
    ///   (`_MIN_CTX_TRIGGER_RATIO`).
    ///
    /// Bila `compact_threshold` > 0, artinya override absolut eksplisit
    /// (kompatibel dengan config lama).
    pub fn compaction_trigger_tokens(&self) -> usize {
        if self.compact_threshold > 0 {
            return self.compact_threshold;
        }
        // Override eksplisit menang; selebihnya tabel per-model (128K default).
        let window = self
            .context_window
            .unwrap_or_else(|| Self::context_window_for_model(&self.model));
        let effective = window
            .saturating_sub(self.effective_max_tokens(&self.model) as usize)
            .max(1);
        let mut pct = self.compact_threshold_ratio.clamp(0.0, 1.0);
        if window < 512_000 {
            pct = pct.max(0.75);
        }
        let pct_value = (effective as f64 * pct) as usize;
        let mut trigger = pct_value.max(64_000);
        let cap85 = (effective as f64 * 0.85) as usize;
        if trigger > pct_value && trigger > cap85 {
            trigger = pct_value.max(cap85);
        }
        if trigger >= effective {
            trigger = cap85.min(effective.saturating_sub(1)).max(1);
        }
        trigger
    }
}

/// Konfigurasi global ~/.hermes/config.toml
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HermesConfig {
    #[serde(default)]
    pub providers: Vec<ProviderEntry>,
    #[serde(default)]
    pub profiles: HashMap<String, ProfileConfig>,
    #[serde(default)]
    pub active_profile: Option<String>,
    /// MCP server eksternal: nama -> { command, args, env } atau { url }.
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    /// Gateway: platform -> konfigurasi.
    #[serde(default)]
    pub gateway: HashMap<String, serde_json::Value>,
    /// Daftar bot (mode multi-bot): tiap bot punya platform, profil, dan token sendiri.
    #[serde(default)]
    pub bots: Vec<BotConfig>,
    /// Webhook keluar: daftar URL yang menerima event (balasan gateway, cron selesai).
    #[serde(default)]
    pub webhooks: WebhookConfig,
    /// Konfigurasi skill ([skills] di config.toml).
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Anggaran karakter memori ([memory] di config.toml).
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Ambang lulus/gagal evaluasi ([evals] di config.toml).
    #[serde(default)]
    pub evals: EvalsConfig,
    /// Opsi konfigurasi bernama generik ([acp_config_options] di
    /// config.toml), diisi saat runtime — mis. via ACP
    /// `session/set_config_option`. Paritas "no typed ACP config surface
    /// yet" di original: diterima + dipersist, belum dikonsumsi komponen
    /// mana pun.
    #[serde(default)]
    pub acp_config_options: HashMap<String, String>,
    /// Konfigurasi TUI ([tui] di config.toml): preferensi tampilan yang
    /// bertahan antar-restart (saat ini: tema F2).
    #[serde(default)]
    pub tui: TuiConfig,
}

/// Konfigurasi memori: bagian `[memory]` di ~/.hermes/config.toml.
///
/// Meniru `memory:` di config.yaml aslinya (`memory_char_limit` /
/// `user_char_limit`, default 2200/1375). Port ini menerapkan anggaran
/// sebagai BATAS INJEKSI system prompt (bukan penolakan tulis seperti
/// aslinya): tiap entri dipotong ke `memory_max_chars_per_entry`, lalu bila
/// total masih melebihi `memory_max_total_chars`, entri tertua dibuang dulu
/// dengan catatan `[truncated]`. Nilai 0 = tanpa batas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Batas karakter per entri yang disuntikkan ke system prompt.
    #[serde(default = "default_memory_max_chars_per_entry")]
    pub memory_max_chars_per_entry: usize,
    /// Batas total karakter memori yang disuntikkan ke system prompt.
    #[serde(default = "default_memory_max_total_chars")]
    pub memory_max_total_chars: usize,
}

fn default_memory_max_chars_per_entry() -> usize {
    2000
}

fn default_memory_max_total_chars() -> usize {
    8000
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            memory_max_chars_per_entry: default_memory_max_chars_per_entry(),
            memory_max_total_chars: default_memory_max_total_chars(),
        }
    }
}

/// Konfigurasi evaluasi: bagian `[evals]` di ~/.hermes/config.toml.
///
/// Ambang lulus/gagal untuk `hermes eval run` (persen 0–100): suite
/// dinyatakan LULUS bila skor >= ambang. Flag `--threshold` menimpa nilai
/// config ini. Fitur ini orisinal-port (aslinya tidak punya runner
/// suite JSONL semacam ini).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalsConfig {
    /// Ambang kelulusan dalam persen (0.0–100.0).
    #[serde(default = "default_eval_threshold")]
    pub threshold: f64,
}

fn default_eval_threshold() -> f64 {
    100.0
}

impl Default for EvalsConfig {
    fn default() -> Self {
        Self {
            threshold: default_eval_threshold(),
        }
    }
}

/// Konfigurasi skill: bagian `[skills]` di ~/.hermes/config.toml.
///
/// Meniru `skills:` di config.yaml aslinya (`disabled`, `external_dirs`,
/// plus `platform_disabled`), dipetakan ke nama TOML/snake_case Rust.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillsConfig {
    /// Nama skill yang tidak pernah dimuat/didaftar (case-insensitive).
    /// Skill esensial (`hermes-agent`) tidak bisa dinonaktifkan — meniru
    /// ESSENTIAL_SKILLS aslinya.
    #[serde(default)]
    pub disabled_skills: Vec<String>,
    /// Direktori tambahan yang dipindai untuk file SKILL.md, dengan parsing
    /// yang sama seperti direktori bawaan. Entri boleh absolut, `~`-prefixed,
    /// memakai `$VAR`/`${VAR}`, atau relatif terhadap ~/.hermes.
    #[serde(default)]
    pub extra_skill_dirs: Vec<String>,
}

/// Konfigurasi TUI: bagian `[tui]` di ~/.hermes/config.toml.
///
/// Menyimpan preferensi tampilan TUI antar-restart (saat ini: tema aktif
/// dari toggle F2). Semua field `Option`/default agar config lama tanpa
/// bagian `[tui]` tetap termuat tanpa perubahan.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TuiConfig {
    /// Nama tema TUI aktif (`"gelap"` / `"terang"` / `"kontras"`).
    /// `None` = default (`"gelap"`).
    #[serde(default)]
    pub theme: Option<String>,
}

/// Konfigurasi satu bot dalam mode multi-bot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    /// Nama bot (dipakai `--bot <nama>`).
    pub name: String,
    /// Platform: telegram, discord, slack, whatsapp.
    pub platform: String,
    /// Nama profil agent untuk bot ini.
    #[serde(default)]
    pub profile: Option<String>,
    /// Nama env var berisi token (atau langsung token di `token`).
    #[serde(default)]
    pub token_env: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

/// Konfigurasi webhook keluar.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebhookConfig {
    /// URL tujuan event keluar.
    #[serde(default)]
    pub outgoing: Vec<String>,
    /// Token rahasia bersama untuk webhook masuk (opsional).
    #[serde(default)]
    pub secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServerConfig {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl HermesConfig {
    /// Direktori data Hermes: $HOME/.hermes (atau $HERMES_HOME).
    pub fn home_dir() -> PathBuf {
        if let Ok(h) = std::env::var("HERMES_HOME") {
            return PathBuf::from(h);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".hermes")
    }

    pub fn profile(&self) -> ProfileConfig {
        let name = self.active_profile.as_deref().unwrap_or("default");
        self.profiles
            .get(name)
            .cloned()
            .unwrap_or_else(ProfileConfig::default)
    }

    pub fn provider(&self, name: &str) -> Option<&ProviderEntry> {
        self.providers.iter().find(|p| p.name == name)
    }

    /// Provider bawaan bila config kosong.
    pub fn default_providers() -> Vec<ProviderEntry> {
        vec![
            ProviderEntry {
                name: "openrouter".to_string(),
                protocol: "openai".to_string(),
                base_url: "https://openrouter.ai/api/v1".to_string(),
                api_key_env: Some("OPENROUTER_API_KEY".to_string()),
                api_keys: vec![],
                default_model: Some("nousresearch/hermes-4-70b".to_string()),
            },
            ProviderEntry {
                name: "nous".to_string(),
                protocol: "openai".to_string(),
                // Paritas original: `_NOUS_DEFAULT_BASE_URL` di
                // `agent/auxiliary_client.py` memakai host inference-api.
                base_url: "https://inference-api.nousresearch.com/v1".to_string(),
                api_key_env: Some("NOUS_API_KEY".to_string()),
                api_keys: vec![],
                default_model: Some("hermes-4-70b".to_string()),
            },
            ProviderEntry {
                name: "openai".to_string(),
                protocol: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                api_key_env: Some("OPENAI_API_KEY".to_string()),
                api_keys: vec![],
                default_model: Some("gpt-4o".to_string()),
            },
            ProviderEntry {
                name: "anthropic".to_string(),
                protocol: "anthropic".to_string(),
                base_url: "https://api.anthropic.com".to_string(),
                api_key_env: Some("ANTHROPIC_API_KEY".to_string()),
                api_keys: vec![],
                default_model: Some("claude-sonnet-4-5".to_string()),
            },
            ProviderEntry {
                name: "gemini".to_string(),
                protocol: "gemini".to_string(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".to_string(),
                api_key_env: Some("GEMINI_API_KEY".to_string()),
                api_keys: vec![],
                // Paritas default model native client original
                // (agent/gemini_native_adapter.py).
                default_model: Some("gemini-3.7-flash".to_string()),
            },
            ProviderEntry {
                name: "azure".to_string(),
                protocol: "azure".to_string(),
                base_url: String::new(), // diisi via env AZURE_OPENAI_RESOURCE dkk.
                api_key_env: Some("AZURE_OPENAI_API_KEY".to_string()),
                api_keys: vec![],
                default_model: None,
            },
            ProviderEntry {
                name: "bedrock".to_string(),
                protocol: "bedrock".to_string(),
                base_url: String::new(), // region via AWS_REGION
                api_key_env: Some("AWS_ACCESS_KEY_ID".to_string()),
                api_keys: vec![],
                default_model: Some("anthropic.claude-3-5-sonnet-20241022-v2:0".to_string()),
            },
            ProviderEntry {
                name: "ollama".to_string(),
                protocol: "openai".to_string(),
                base_url: "http://localhost:11434/v1".to_string(),
                api_key_env: None,
                api_keys: vec![],
                default_model: Some("llama3.1".to_string()),
            },
        ]
    }
}
