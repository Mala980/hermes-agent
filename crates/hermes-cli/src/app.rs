//! app.rs: merakit seluruh komponen Hermes menjadi satu App siap pakai.
use crate::approval::{ApprovalKind, InteractiveApproval};
use hermes_core::{
    ApprovalHandler, CancelFlag, HermesConfig, LlmProvider, MemoryStore, ProfileConfig, SkillStore,
    ToolRegistry,
};
use hermes_cron::CronStore;
use hermes_gateway::{PlatformRegistry, TelegramPlatform};
use hermes_mcp::{McpClient, McpServerConfigBridge};
use hermes_memory::SqliteMemoryStore;
use hermes_providers::build_provider;
use hermes_skills::SkillManager;
use std::path::PathBuf;
use std::sync::Arc;

pub struct App {
    /// Config EFEKTIF: nilai string sudah diekspansi dari env (`${VAR}`/`$VAR`,
    /// lihat `expand_config_env`). Inilah yang dipakai seluruh runtime.
    pub config: HermesConfig,
    /// Config MENTAH: persis seperti tertulis di config.toml (referensi
    /// `${VAR}` utuh). Dipakai saat menyimpan kembali agar nilai hasil
    /// ekspansi tidak "terbakar" permanen ke disk.
    pub config_raw: HermesConfig,
    pub profile_name: String,
    pub profile: ProfileConfig,
    pub hermes_home: PathBuf,
    pub provider: Arc<dyn LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub memory: Arc<dyn MemoryStore>,
    pub skills: Arc<dyn SkillStore>,
    pub platforms: Arc<PlatformRegistry>,
    pub cron_store: Arc<CronStore>,
    pub approval: Arc<dyn ApprovalHandler>,
    pub cancel: CancelFlag,
}

/// Muat .env sederhana (KEY=VALUE) dan pasang ke process env.
///
/// Rantai presedensi (meniru Hermes asli, audit W3-P1):
///   1. `~/.hermes/.env` — MENANG atas process env untuk kunci yang
///      didefinisikannya (asli: `load_hermes_dotenv(..., override=True)`,
///      "~/.hermes/.env overrides stale shell exports"; pembaca kredensial
///      `get_env_value_prefer_dotenv` memilih nilai .env dulu agar "a
///      deliberate .env edit beats a stale value inherited from the parent
///      shell").
///   2. Process env (export shell induk) — berlaku untuk kunci yang TIDAK
///      didefinisikan di .env.
///   3. `config.toml` — fallback terakhir, mis. `gateway_token()` memakai
///      `config.gateway.<platform>.<key>` hanya bila env tidak menyediakannya
///      (asli: env menimpa `PlatformConfig.token` dari yaml via `EnvCred`).
///
/// Catatan: lapisan lanjutan aslinya (managed .env, project .env, sumber
/// 1Password/`op://`, dan pengecualian `_SPAWN_CREDENTIAL_KEYS`) tidak
/// direplikasi di sini — port ini hanya mengimplementasikan inti rantai di
/// atas.
fn load_dotenv(hermes_home: &std::path::Path) {
    let path = hermes_home.join(".env");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches('"').trim_matches('\'');
            // .env menang atas process env untuk kunci yang didefinisikannya.
            if !k.is_empty() {
                std::env::set_var(k, v);
            }
        }
    }
}

fn load_or_init_config(
    hermes_home: &std::path::Path,
) -> anyhow::Result<(HermesConfig, HermesConfig)> {
    let path = hermes_home.join("config.toml");
    if path.is_file() {
        let text = std::fs::read_to_string(&path)?;
        let raw: HermesConfig =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("config.toml tidak valid: {e}"))?;
        // B11: ekspansi ${VAR}/$VAR saat load; variabel tak dikenal = error.
        let expanded = expand_config_env(raw.clone())?;
        return Ok((raw, expanded));
    }
    // Buat config default.
    let mut cfg = HermesConfig {
        providers: HermesConfig::default_providers(),
        ..Default::default()
    };
    cfg.profiles
        .insert("default".to_string(), ProfileConfig::default());
    let text = toml::to_string_pretty(&cfg)?;
    std::fs::write(
        &path,
        format!("# Konfigurasi Hermes Agent (Rust port)\n{text}"),
    )?;
    Ok((cfg.clone(), cfg))
}

/// Override sekali jalan dari flag CLI global `--model`/`--provider`/`--reasoning`
/// (audit W6-B3): menimpa profil aktif untuk SATU proses ini saja — flag menang
/// atas config, dan config di disk TIDAK PERNAH ditulis.
#[derive(Debug, Default, Clone)]
pub struct RunOverrides {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub reasoning: Option<String>,
}

impl RunOverrides {
    /// Bangun dari nilai flag CLI; level reasoning tak dikenal → error jelas
    /// (fail fast, sebelum App sempat dibangun).
    pub fn new(
        model: Option<String>,
        provider: Option<String>,
        reasoning: Option<String>,
    ) -> anyhow::Result<Self> {
        if let Some(level) = reasoning.as_deref() {
            validate_reasoning_level(level)?;
        }
        Ok(Self {
            model,
            provider,
            reasoning,
        })
    }

    /// Terapkan ke profil in-memory. Provider tak dikenal → error yang
    /// menyebut nama-nama terdaftar (meniru resolusi runtime original yang
    /// menolak nama provider asing).
    pub fn apply(&self, config: &HermesConfig, profile: &mut ProfileConfig) -> anyhow::Result<()> {
        if let Some(m) = &self.model {
            profile.model = m.clone();
        }
        if let Some(p) = &self.provider {
            if config.provider(p).is_none() {
                let known: Vec<&str> = config.providers.iter().map(|e| e.name.as_str()).collect();
                anyhow::bail!(
                    "provider `{p}` tidak dikenal di config (terdaftar: {})",
                    known.join(", ")
                );
            }
            profile.provider = p.clone();
        }
        if let Some(r) = &self.reasoning {
            profile.reasoning_effort = Some(r.clone());
        }
        Ok(())
    }
}

/// Level reasoning valid (paritas `EFFORT_LADDER` di `agent/reasoning_effort.py`
/// original).
pub fn validate_reasoning_level(level: &str) -> anyhow::Result<()> {
    if hermes_core::REASONING_EFFORTS.contains(&level) {
        Ok(())
    } else {
        anyhow::bail!(
            "level reasoning `{level}` tidak dikenal (valid: {})",
            hermes_core::REASONING_EFFORTS.join(", ")
        );
    }
}

/// Ekspansi `${VAR}` / `$VAR` di SEMUA nilai string config dari process env
/// (audit W6-B11). `$$` → `$` literal. Variabel tak terdefinisi → error yang
/// menyebut nama variabel (fail fast; tidak dikosongkan diam-diam).
/// Kunci (key) tidak disentuh — hanya nilai string.
pub fn expand_config_env(config: HermesConfig) -> anyhow::Result<HermesConfig> {
    let mut value = toml::Value::try_from(config)
        .map_err(|e| anyhow::anyhow!("config tidak bisa diserialisasi untuk ekspansi: {e}"))?;
    expand_env_value(&mut value, "$")?;
    value
        .try_into()
        .map_err(|e| anyhow::anyhow!("config tidak valid setelah ekspansi env: {e}"))
}

fn expand_env_value(value: &mut toml::Value, path: &str) -> anyhow::Result<()> {
    match value {
        toml::Value::String(s) => {
            *s = expand_env_str(s, path)?;
        }
        toml::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                expand_env_value(item, &format!("{path}[{i}]"))?;
            }
        }
        toml::Value::Table(map) => {
            for (k, v) in map.iter_mut() {
                expand_env_value(v, &format!("{path}.{k}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Ekspansi satu nilai string. Scanner manual (tanpa regex):
/// `${VAR}` / `$VAR` dari env, `$$` → `$` literal. `$` soliter, `${` tak
/// tertutup, dan `$` diikuti karakter non-nama dibiarkan literal agar nilai
/// seperti "USD$5" atau cuplikan shell di system prompt tidak rusak diam-diam.
fn expand_env_str(s: &str, path: &str) -> anyhow::Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('{') => {
                chars.next(); // konsumsi '{'
                let mut name = String::new();
                let mut closed = false;
                for ch in chars.by_ref() {
                    if ch == '}' {
                        closed = true;
                        break;
                    }
                    name.push(ch);
                }
                if !closed {
                    // `${` tak tertutup → pertahankan literal.
                    out.push_str("${");
                    out.push_str(&name);
                    continue;
                }
                let var = name.trim();
                if var.is_empty() {
                    anyhow::bail!("referensi env `${{}}` kosong di config ({path})");
                }
                out.push_str(&lookup_env_var(var, path)?);
            }
            Some('$') => {
                chars.next(); // konsumsi '$' kedua
                out.push('$');
            }
            Some(&ch) if ch.is_ascii_alphabetic() || ch == '_' => {
                let mut name = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_alphanumeric() || ch == '_' {
                        name.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push_str(&lookup_env_var(&name, path)?);
            }
            // `$` soliter / diikuti karakter non-nama → literal.
            _ => out.push('$'),
        }
    }
    Ok(out)
}

fn lookup_env_var(var: &str, path: &str) -> anyhow::Result<String> {
    std::env::var(var).map_err(|_| {
        anyhow::anyhow!(
            "config memakai `${{{var}}}` di {path} tetapi variabel env `{var}` tidak didefinisikan"
        )
    })
}

/// Bangun App lengkap untuk satu profil.
///
/// `overrides` (dari flag CLI `--model`/`--provider`/`--reasoning`) diterapkan
/// ke profil in-memory SETELAH config dibaca — flag menang atas config untuk
/// sekali jalan ini, tanpa menulis config.toml.
pub async fn build_app(
    profile_name: Option<String>,
    approval_kind: ApprovalKind,
    workdir_override: Option<String>,
    overrides: &RunOverrides,
) -> anyhow::Result<App> {
    let hermes_home = HermesConfig::home_dir();
    std::fs::create_dir_all(&hermes_home)?;
    std::fs::create_dir_all(hermes_home.join("skills"))?;
    load_dotenv(&hermes_home);

    let (config_raw, config) = load_or_init_config(&hermes_home)?;
    let profile_name = profile_name
        .or(config.active_profile.clone())
        .unwrap_or_else(|| "default".to_string());
    let mut profile = config
        .profiles
        .get(&profile_name)
        .cloned()
        .unwrap_or_else(ProfileConfig::default);
    profile.name = profile_name.clone();
    overrides.apply(&config, &mut profile)?;
    if let Some(w) = workdir_override {
        profile.working_dir = w;
    }
    // Working dir absolut.
    let wd = PathBuf::from(&profile.working_dir);
    let wd = if wd.is_absolute() {
        wd
    } else {
        std::env::current_dir()?.join(wd)
    };
    profile.working_dir = wd.to_string_lossy().into_owned();

    // Provider.
    let provider: Arc<dyn LlmProvider> = match config.provider(&profile.provider) {
        Some(entry) => build_provider(entry)?,
        None => hermes_providers::factory::default_provider(),
    };

    let memory: Arc<dyn MemoryStore> = Arc::new(SqliteMemoryStore::open_default(&hermes_home)?);
    let skills: Arc<dyn SkillStore> = Arc::new(
        SkillManager::new(&hermes_home, &wd).with_skills_config(&hermes_home, &config.skills),
    );

    // Registry tool.
    let mut registry = ToolRegistry::new();
    hermes_tools::register_builtin(&mut registry);
    let cron_store = Arc::new(CronStore::open_default(&hermes_home)?);
    hermes_cron::tools::register_cron_tools(&mut registry, cron_store.clone());

    // MCP servers dari config.
    let mut mcp_clients: Vec<(String, Arc<McpClient>)> = Vec::new();
    for (name, cfg) in &config.mcp_servers {
        let client = McpServerConfigBridge::connect(name, cfg).await;
        match client {
            Ok(c) => {
                tracing::info!(server = %name, "MCP terhubung");
                mcp_clients.push((name.clone(), Arc::new(c)));
            }
            Err(e) => tracing::warn!(server = %name, "MCP gagal terhubung: {e}"),
        }
    }
    if let Err(e) = hermes_mcp::register_mcp_tools(&mut registry, &mcp_clients).await {
        tracing::warn!("gagal mendaftarkan tool MCP: {e}");
    }

    // Gateway platforms.
    let mut platforms = PlatformRegistry::new();
    if let Some(token) = gateway_token(&config, "telegram", "TELEGRAM_BOT_TOKEN", "bot_token")
        .filter(|t| !t.is_empty())
    {
        platforms.register(Arc::new(TelegramPlatform::new(token)));
    }
    if let Some(token) = gateway_token(&config, "discord", "DISCORD_BOT_TOKEN", "bot_token")
        .filter(|t| !t.is_empty())
    {
        platforms.register(Arc::new(hermes_gateway::DiscordPlatform::new(token)));
    }
    if let Some(token) =
        gateway_token(&config, "slack", "SLACK_BOT_TOKEN", "bot_token").filter(|t| !t.is_empty())
    {
        platforms.register(Arc::new(hermes_gateway::SlackPlatform::new(token)));
    }
    let wa_token = gateway_token(&config, "whatsapp", "WHATSAPP_TOKEN", "token");
    let wa_pnid = gateway_token(
        &config,
        "whatsapp",
        "WHATSAPP_PHONE_NUMBER_ID",
        "phone_number_id",
    );
    if let (Some(token), Some(pnid)) = (wa_token, wa_pnid) {
        if !token.is_empty() && !pnid.is_empty() {
            platforms.register(Arc::new(hermes_gateway::WhatsAppPlatform::new(pnid, token)));
        }
    }
    // Matrix: butuh homeserver + access token.
    let mx_hs = std::env::var("MATRIX_HOMESERVER").ok();
    let mx_token = std::env::var("MATRIX_ACCESS_TOKEN").ok();
    if let (Some(hs), Some(tok)) = (mx_hs, mx_token) {
        if !tok.is_empty() {
            platforms.register(Arc::new(hermes_gateway::MatrixPlatform::new(hs, tok)));
        }
    }
    // Signal: selalu daftarkan (kredensial dicek saat serve via signal-cli).
    platforms.register(Arc::new(hermes_gateway::SignalPlatform));
    // Teams & email: kredensial dicek saat serve.
    if std::env::var("TEAMS_APP_ID").is_ok() {
        if let Ok(p) = hermes_gateway::TeamsPlatform::from_env() {
            platforms.register(Arc::new(p));
        }
    }
    if let Ok(p) = hermes_gateway::EmailPlatform::from_env() {
        platforms.register(Arc::new(p));
    }
    for (id, hint) in hermes_gateway::telegram::supported_platforms() {
        if platforms.get(id).is_none() {
            platforms.register_stub(id, hint);
        }
    }
    // Tool gateway_send butuh akses registry platform.
    let platforms_arc = Arc::new(platforms);
    register_gateway_send_tool(&mut registry, platforms_arc.clone());

    let approval: Arc<dyn ApprovalHandler> = Arc::new(InteractiveApproval::new(
        approval_kind,
        profile.security.clone(),
    ));

    Ok(App {
        config,
        config_raw,
        profile_name,
        profile,
        hermes_home,
        provider,
        tools: Arc::new(registry),
        memory,
        skills,
        platforms: platforms_arc,
        cron_store,
        approval,
        cancel: CancelFlag::new(),
    })
}

/// Tool `gateway_send`: kirim pesan ke platform messaging.
fn register_gateway_send_tool(reg: &mut ToolRegistry, platforms: Arc<PlatformRegistry>) {
    use async_trait::async_trait;
    use hermes_core::{Result, RiskLevel, Tool, ToolContext, ToolResult};
    use hermes_tools::schema::*;

    struct GatewaySendTool {
        platforms: Arc<PlatformRegistry>,
    }

    #[async_trait]
    impl Tool for GatewaySendTool {
        fn name(&self) -> &str {
            "gateway_send"
        }
        fn description(&self) -> &str {
            "Mengirim pesan teks ke platform messaging (telegram, ...) ke target chat tertentu."
        }
        fn parameters_schema(&self) -> serde_json::Value {
            object_schema(
                vec![
                    ("platform", string_prop("ID platform: telegram, ...")),
                    (
                        "target",
                        string_prop("Target: chat id / username / channel."),
                    ),
                    ("text", string_prop("Isi pesan.")),
                ],
                vec!["platform", "target", "text"],
            )
        }
        fn risk(&self) -> RiskLevel {
            RiskLevel::Medium
        }
        async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
            let platform = get_str(&args, "platform")?;
            let target = get_str(&args, "target")?;
            let text = get_str(&args, "text")?;
            self.platforms.send(platform, target, text).await?;
            Ok(ok(
                "",
                self.name(),
                format!("terkirim via {platform} ke {target}"),
            ))
        }
    }

    reg.register(GatewaySendTool { platforms });
}

/// Ambil token platform dari env atau config gateway.<platform>.<key>.
fn gateway_token(
    config: &HermesConfig,
    platform: &str,
    env_name: &str,
    config_key: &str,
) -> Option<String> {
    std::env::var(env_name).ok().or_else(|| {
        config
            .gateway
            .get(platform)
            .and_then(|v| v.get(config_key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
}

/// Simpan kembali config ke disk.
pub fn save_config(config: &HermesConfig, hermes_home: &std::path::Path) -> anyhow::Result<()> {
    let text = toml::to_string_pretty(config)?;
    std::fs::write(hermes_home.join("config.toml"), text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialisasi uji yang memutasi process env (env bersifat global per-proses).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// W3-P1: uji berlapis — .env mengalahkan process env untuk kunci yang
    /// sama, kunci yang hanya ada di process env tetap berlaku, dan kunci
    /// yang hanya ada di .env ikut terpasang.
    #[test]
    fn dotenv_beats_process_env_for_defined_keys() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("hermes-dotenv-precedence-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("buat dir uji");
        std::fs::write(
            dir.join(".env"),
            "# komentar diabaikan\nHERMES_TEST_PREC_A=from_dotenv\nHERMES_TEST_PREC_B=\"dotenv_only\"\n",
        )
        .expect("tulis .env uji");
        // Kunci yang sama di process env (simulasi "stale shell export").
        std::env::set_var("HERMES_TEST_PREC_A", "from_shell");
        // Kunci yang hanya ada di process env.
        std::env::set_var("HERMES_TEST_PREC_C", "shell_only");

        load_dotenv(&dir);

        assert_eq!(
            std::env::var("HERMES_TEST_PREC_A").as_deref(),
            Ok("from_dotenv"),
            ".env harus menang atas process env untuk kunci yang didefinisikannya"
        );
        assert_eq!(
            std::env::var("HERMES_TEST_PREC_B").as_deref(),
            Ok("dotenv_only"),
            "kunci yang hanya ada di .env harus terpasang (kutip dikupas)"
        );
        assert_eq!(
            std::env::var("HERMES_TEST_PREC_C").as_deref(),
            Ok("shell_only"),
            "kunci yang hanya ada di process env harus tetap berlaku"
        );

        // Bersih-bersih agar tidak bocor ke uji lain.
        std::env::remove_var("HERMES_TEST_PREC_A");
        std::env::remove_var("HERMES_TEST_PREC_B");
        std::env::remove_var("HERMES_TEST_PREC_C");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// W3-P1: tanpa file .env, process env tidak tersentuh.
    #[test]
    fn dotenv_missing_leaves_process_env_alone() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("hermes-dotenv-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("buat dir uji");
        std::env::set_var("HERMES_TEST_ABSENT_X", "shell_value");

        load_dotenv(&dir);

        assert_eq!(
            std::env::var("HERMES_TEST_ABSENT_X").as_deref(),
            Ok("shell_value"),
            "tanpa .env, process env harus tidak berubah"
        );
        std::env::remove_var("HERMES_TEST_ABSENT_X");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------------------------
    // W6-B11: ekspansi ${VAR} / $VAR di config
    // ------------------------------------------------------------------

    fn expand_str_for_test(s: &str) -> anyhow::Result<String> {
        let _g = ENV_LOCK.lock().unwrap();
        expand_env_str(s, "$.uji")
    }

    /// B11: variabel terdefinisi diekspansi (${VAR} dan $VAR).
    #[test]
    fn env_expansion_defined_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("HERMES_EXP_DEF", "nilai-x");
        let out = expand_env_str("${HERMES_EXP_DEF}/jalan", "$.uji").unwrap();
        assert_eq!(out, "nilai-x/jalan");
        let out = expand_env_str("awal-$HERMES_EXP_DEF-akhir", "$.uji").unwrap();
        assert_eq!(out, "awal-nilai-x-akhir");
        std::env::remove_var("HERMES_EXP_DEF");
    }

    /// B11: variabel tak terdefinisi → error jelas yang menyebut nama variabel.
    #[test]
    fn env_expansion_undefined_fails_fast() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("HERMES_EXP_TAK_ADA");
        let err = expand_env_str("x-${HERMES_EXP_TAK_ADA}-y", "$.profiles.default.model")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("HERMES_EXP_TAK_ADA"),
            "error harus menyebut nama variabel: {err}"
        );
        let err = expand_env_str("$HERMES_EXP_TAK_ADA", "$.uji")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("HERMES_EXP_TAK_ADA"),
            "bentuk $VAR juga: {err}"
        );
    }

    /// B11: `$$` → `$` literal; `$` soliter dan `${` tak tertutup dibiarkan.
    #[test]
    fn env_expansion_escaped_and_literal_dollar() {
        let r = expand_str_for_test("harga $$5 dan $${X}").unwrap();
        // `$$` → `$` literal; `{X}` sesudahnya adalah teks biasa (bukan ref).
        assert_eq!(r, "harga $5 dan ${X}");
        let r = expand_str_for_test("USD$5 tetap").unwrap();
        assert_eq!(r, "USD$5 tetap");
        let r = expand_str_for_test("buka ${kurung").unwrap();
        assert_eq!(r, "buka ${kurung");
        let r = expand_str_for_test("akhir$").unwrap();
        assert_eq!(r, "akhir$");
    }

    /// B11: `$$${VAR}` → `$` literal + nilai VAR (escape hanya memakan `$$` pertama).
    #[test]
    fn env_expansion_escaped_then_real_ref() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("HERMES_EXP_ESC", "nilai-y");
        let r = expand_env_str("$${HERMES_EXP_ESC}", "$.uji").unwrap();
        assert_eq!(r, "${HERMES_EXP_ESC}");
        let r = expand_env_str("$$${HERMES_EXP_ESC}", "$.uji").unwrap();
        assert_eq!(r, "$nilai-y");
        std::env::remove_var("HERMES_EXP_ESC");
    }

    /// B11: ekspansi menjangkau nilai string bersarang (profil, provider,
    /// array) tetapi tidak menyentuh kunci.
    #[test]
    fn env_expansion_nested_config_values() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("HERMES_EXP_HOST", "contoh.test");
        let mut cfg = HermesConfig::default();
        cfg.profiles.insert(
            "default".to_string(),
            ProfileConfig {
                working_dir: "proyek-${HERMES_EXP_HOST}".to_string(),
                ..ProfileConfig::default()
            },
        );
        cfg.providers.push(hermes_core::ProviderEntry {
            name: "kustom".to_string(),
            protocol: "openai".to_string(),
            base_url: "https://${HERMES_EXP_HOST}/v1".to_string(),
            api_key_env: Some("HERMES_EXP_HOST".to_string()),
            api_keys: vec![],
            default_model: None,
        });
        let expanded = expand_config_env(cfg).unwrap();
        let profile = expanded.profiles.get("default").unwrap();
        assert_eq!(profile.working_dir, "proyek-contoh.test");
        let entry = expanded
            .providers
            .iter()
            .find(|p| p.name == "kustom")
            .unwrap();
        assert_eq!(entry.base_url, "https://contoh.test/v1");
        std::env::remove_var("HERMES_EXP_HOST");
    }

    // ------------------------------------------------------------------
    // W6-B3: RunOverrides — flag menang atas config
    // ------------------------------------------------------------------

    fn test_config_with_provider() -> HermesConfig {
        let mut cfg = HermesConfig::default();
        cfg.providers.push(hermes_core::ProviderEntry {
            name: "openrouter".to_string(),
            protocol: "openai".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: None,
            api_keys: vec![],
            default_model: Some("model-lama".to_string()),
        });
        cfg.providers.push(hermes_core::ProviderEntry {
            name: "ollama".to_string(),
            protocol: "openai".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            api_key_env: None,
            api_keys: vec![],
            default_model: Some("llama3".to_string()),
        });
        cfg
    }

    /// B3: flag --model/--provider/--reasoning menimpa nilai profil; tanpa
    /// flag profil tidak berubah.
    #[test]
    fn run_overrides_flag_beats_config() {
        let cfg = test_config_with_provider();
        let mut profile = ProfileConfig {
            provider: "openrouter".to_string(),
            model: "model-lama".to_string(),
            ..ProfileConfig::default()
        };
        let ov = RunOverrides::new(
            Some("model-baru".to_string()),
            Some("ollama".to_string()),
            Some("high".to_string()),
        )
        .unwrap();
        ov.apply(&cfg, &mut profile).unwrap();
        assert_eq!(profile.model, "model-baru");
        assert_eq!(profile.provider, "ollama");
        assert_eq!(profile.reasoning_effort.as_deref(), Some("high"));

        // Tanpa flag: tidak ada perubahan.
        let mut profile2 = profile.clone();
        RunOverrides::default().apply(&cfg, &mut profile2).unwrap();
        assert_eq!(profile2.model, "model-baru");
        assert_eq!(profile2.provider, "ollama");
    }

    /// B3: provider tak dikenal → error yang menyebut nama terdaftar.
    #[test]
    fn run_overrides_unknown_provider_errors() {
        let cfg = test_config_with_provider();
        let mut profile = ProfileConfig::default();
        let ov = RunOverrides::new(None, Some("antah-berantah".to_string()), None).unwrap();
        let err = ov.apply(&cfg, &mut profile).unwrap_err().to_string();
        assert!(err.contains("antah-berantah"), "{err}");
        assert!(err.contains("openrouter"), "sebutkan yang terdaftar: {err}");
    }

    /// B3: level reasoning tak dikenal → error jelas saat konstruksi.
    #[test]
    fn run_overrides_invalid_reasoning_errors() {
        let err = RunOverrides::new(None, None, Some("super-tinggi".to_string()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("super-tinggi"), "{err}");
        assert!(err.contains("high"), "sebutkan level valid: {err}");
        // Semua level kanonis diterima.
        for level in hermes_core::REASONING_EFFORTS {
            RunOverrides::new(None, None, Some(level.to_string())).unwrap();
        }
    }

    /// Tema TUI bertahan antar-restart: save_config menulis bagian [tui],
    /// dan parse ulang mengembalikan nama tema yang disimpan.
    #[test]
    fn save_config_roundtrips_tui_theme() {
        let dir = std::env::temp_dir().join(format!("hermes-tui-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("buat dir uji");
        let mut config = HermesConfig::default();
        config.tui.theme = Some("terang".to_string());
        save_config(&config, &dir).expect("simpan config");
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("baca config");
        assert!(text.contains("[tui]"), "bagian [tui] tertulis: {text}");
        assert!(text.contains("theme = \"terang\""), "tema tertulis: {text}");
        let back: HermesConfig = toml::from_str(&text).expect("parse ulang config");
        assert_eq!(back.tui.theme.as_deref(), Some("terang"));
    }

    /// Kompatibel mundur: config lama tanpa bagian [tui] tetap termuat
    /// dengan tema None (= default gelap di TUI).
    #[test]
    fn legacy_config_without_tui_section_loads() {
        let legacy: HermesConfig =
            toml::from_str("active_profile = \"default\"\n").expect("parse config lama");
        assert_eq!(legacy.tui.theme, None);
    }
}
