//! hermes — CLI Hermes Agent (Rust port).
//!
//! Meniru perintah CLI Hermes asli: chat, run, skills, memory, config,
//! cron, gateway, mcp, model, doctor, setup, version, serta batch 7
//! (tier 1): session list/show/delete (alias `sessions`), config get/unset,
//! logs, status, tools list/enable/disable; batch 8 (tier 2): session
//! import/rename/stats/prune, config check, providers list, run --format,
//! eval list, skills inspect, memory status.

mod acp;
mod app;
mod approval;
mod cmd;
mod tui;
mod ui;

use app::{build_app, RunOverrides};
use approval::ApprovalKind;
use clap::Parser;
use cmd::chat::{ChatOptions, OutputFormat};
use cmd::cli::{Cli, Commands};
use cmd::{
    batch::cmd_batch,
    chat::{cmd_chat, cmd_run},
    config::cmd_config,
    cron::cmd_cron,
    eval::cmd_eval,
    gateway::cmd_gateway,
    logs::cmd_logs,
    mcp::cmd_mcp,
    memory::cmd_memory,
    model::cmd_model,
    providers::cmd_providers,
    session::cmd_session,
    skills::cmd_skills,
    status::cmd_status,
    system::{doctor, setup},
    tools::cmd_tools,
    voice::cmd_voice,
    webhook::cmd_webhook,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose);

    // Tanpa subcommand → masuk mode chat (seperti `hermes` asli).
    let command = cli.command.unwrap_or(Commands::Chat {
        resume: None,
        continue_last: false,
        query: None,
        query_file: None,
        oneshot: false,
        format: "text".to_string(),
    });

    let approval_kind = if cli.yes {
        ApprovalKind::Yes
    } else if cli.readonly {
        ApprovalKind::ReadOnly
    } else {
        ApprovalKind::Interactive
    };

    // Perintah yang tidak butuh App penuh.
    match &command {
        Commands::Version => {
            println!(
                "hermes {} (Rust port dari NousResearch/hermes-agent)",
                env!("CARGO_PKG_VERSION")
            );
            return Ok(());
        }
        Commands::Doctor => return doctor().await,
        Commands::Setup => return setup().await,
        // B7: logs hanya membaca file di HERMES_HOME — tidak butuh App.
        Commands::Logs {
            log,
            lines,
            follow,
            level,
            session,
            since,
        } => {
            return cmd_logs(
                &hermes_core::HermesConfig::home_dir(),
                log,
                *lines,
                *follow,
                level.as_deref(),
                session.as_deref(),
                since.as_deref(),
            )
            .await
        }
        _ => {}
    }

    // B3: override sekali jalan (--model/--provider/--reasoning); level tak
    // dikenal gagal cepat di sini, sebelum App dibangun.
    let overrides = RunOverrides::new(
        cli.model.clone(),
        cli.provider.clone(),
        cli.reasoning.clone(),
    )?;
    let mut app = build_app(
        cli.profile.clone(),
        approval_kind,
        cli.workdir.clone(),
        &overrides,
    )
    .await?;

    // Ctrl-C → batalkan loop agen.
    let cancel = app.cancel.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("\n(membatalkan...)");
        cancel.cancel();
    });

    match command {
        Commands::Chat {
            resume,
            continue_last,
            query,
            query_file,
            oneshot,
            format,
        } => {
            let format = OutputFormat::parse(&format)?;
            cmd_chat(
                &mut app,
                ChatOptions {
                    resume,
                    continue_last,
                    query,
                    query_file,
                    oneshot,
                    format,
                },
            )
            .await?
        }
        Commands::Run {
            query,
            session,
            print,
            format,
        } => cmd_run(&mut app, &query, session, print, &format).await?,
        Commands::Skills { cmd } => cmd_skills(&app, cmd).await?,
        Commands::Memory { cmd } => cmd_memory(&app, cmd).await?,
        Commands::Config { cmd } => cmd_config(&mut app, cmd).await?,
        Commands::Cron { cmd } => cmd_cron(&mut app, cmd).await?,
        Commands::Gateway {
            platform,
            webhook_port,
            bot,
        } => cmd_gateway(&app, &platform, webhook_port, bot.as_deref(), approval_kind).await?,
        Commands::Mcp { cmd } => cmd_mcp(&app, cmd).await?,
        Commands::Voice { cmd } => cmd_voice(cmd).await?,
        Commands::Acp => acp::serve_acp(&app).await?,
        Commands::Batch { input, out } => {
            cmd_batch(&app, &input, out.as_deref(), approval_kind).await?
        }
        Commands::Session { cmd } => cmd_session(&app, cmd).await?,
        Commands::Webhook { cmd } => cmd_webhook(&app, cmd).await?,
        Commands::Tui => tui::run_tui(&mut app).await?,
        Commands::Eval { cmd } => cmd_eval(&app, &cmd).await?,
        Commands::Model { name } => cmd_model(&mut app, name).await?,
        Commands::Status { full, deep } => cmd_status(&app, full, deep).await?,
        Commands::Tools { cmd } => cmd_tools(&mut app, &cmd).await?,
        Commands::Providers { cmd } => cmd_providers(&app, cmd)?,
        Commands::Logs { .. } => unreachable!(),
        Commands::Doctor | Commands::Setup | Commands::Version => unreachable!(),
    }
    Ok(())
}

fn init_logging(verbose: u8) {
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(format!("hermes_cli={level},hermes_core={level},hermes_providers={level},hermes_gateway={level},hermes_agent={level},hermes_tools={level}"))
        .with_target(false)
        .compact()
        .init();
}

#[cfg(test)]
mod w6_cli_tests {
    //! Uji batch 6 (audit W6): flag CLI `--model`/`--provider`/`--reasoning`
    //! (B3), resolusi `--resume` (B4), dan mode sekali-jalan chat (B5).
    use super::*;
    use crate::cmd::chat::{read_query_file, resolve_session, run_single_turn};
    use async_trait::async_trait;
    use clap::Parser;
    use hermes_core::Agent;
    use hermes_core::{
        AllowAllApproval, CancelFlag, ChatRequest, LlmProvider, MemoryEntry, MemoryStore,
        Result as CoreResult, Session, Skill, SkillStore, StreamEvent, ToolRegistry, Usage,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("hermes").chain(args.iter().copied()))
    }

    fn chat_cmd(
        cli: Cli,
    ) -> (
        Option<String>,
        bool,
        Option<String>,
        Option<String>,
        bool,
        String,
    ) {
        match cli.command {
            Some(Commands::Chat {
                resume,
                continue_last,
                query,
                query_file,
                oneshot,
                format,
            }) => (resume, continue_last, query, query_file, oneshot, format),
            _ => panic!("diharapkan subcommand chat"),
        }
    }

    // ---------------------------------------------------------------
    // B3: parse flag global --model / --provider / --reasoning
    // ---------------------------------------------------------------

    #[test]
    fn global_override_flags_parse_sebelum_subcommand() {
        let cli = parse(&[
            "--model",
            "model-x",
            "--provider",
            "ollama",
            "--reasoning",
            "high",
            "chat",
        ])
        .expect("parse harus berhasil");
        assert_eq!(cli.model.as_deref(), Some("model-x"));
        assert_eq!(cli.provider.as_deref(), Some("ollama"));
        assert_eq!(cli.reasoning.as_deref(), Some("high"));
    }

    #[test]
    fn global_override_flags_parse_setelah_subcommand_dan_short_m() {
        let cli =
            parse(&["chat", "-m", "model-y", "--reasoning", "low"]).expect("parse harus berhasil");
        assert_eq!(cli.model.as_deref(), Some("model-y"));
        assert_eq!(cli.reasoning.as_deref(), Some("low"));
        assert_eq!(cli.provider, None);
    }

    #[test]
    fn invalid_reasoning_ditolak_saat_konstruksi_overrides() {
        let err = RunOverrides::new(None, None, Some("ngawur".to_string()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("ngawur"), "{err}");
    }

    // ---------------------------------------------------------------
    // B5: parse flag chat satu-jalan
    // ---------------------------------------------------------------

    #[test]
    fn chat_resume_continue_dan_oneshot_parse() {
        let cli = parse(&["chat", "--resume", "latest"]).expect("parse");
        let (resume, continue_last, ..) = chat_cmd(cli);
        assert_eq!(resume.as_deref(), Some("latest"));
        assert!(!continue_last);

        let cli = parse(&["chat", "-c"]).expect("parse");
        let (_, continue_last, ..) = chat_cmd(cli);
        assert!(continue_last);

        let cli = parse(&["chat", "-r", "abc123", "--oneshot"]).expect("parse");
        let (resume, _, _, _, oneshot, _) = chat_cmd(cli);
        assert_eq!(resume.as_deref(), Some("abc123"));
        assert!(oneshot);
    }

    #[test]
    fn chat_query_dan_query_file_saling_menolak() {
        match parse(&["chat", "-q", "halo", "--query-file", "/tmp/x"]) {
            Ok(_) => panic!("--query dan --query-file harus konflik"),
            Err(e) => assert_eq!(e.kind(), clap::error::ErrorKind::ArgumentConflict),
        }
    }

    #[test]
    fn chat_query_file_dan_format_parse() {
        let cli = parse(&["chat", "--query-file", "-", "--format", "json"]).expect("parse");
        let (_, _, query, query_file, _, format) = chat_cmd(cli);
        assert_eq!(query, None);
        assert_eq!(query_file.as_deref(), Some("-"));
        assert_eq!(OutputFormat::parse(&format).unwrap(), OutputFormat::Json);
    }

    #[test]
    fn output_format_tak_dikenal_ditolak() {
        let err = OutputFormat::parse("yaml").unwrap_err().to_string();
        assert!(err.contains("yaml"), "{err}");
        assert_eq!(OutputFormat::parse("text").unwrap(), OutputFormat::Text);
    }

    #[test]
    fn read_query_file_dari_path() {
        let dir = std::env::temp_dir().join(format!("hermes-qf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prompt.txt");
        std::fs::write(&path, "tanya\njawab").unwrap();
        let out = read_query_file(path.to_str().unwrap()).unwrap();
        assert_eq!(out, "tanya\njawab");
        let err = read_query_file(dir.join("tak-ada.txt").to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("query-file"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------
    // B4: resolusi --resume (stub MemoryStore)
    // ---------------------------------------------------------------

    struct StubMemory {
        sessions: Vec<Session>,
    }

    impl StubMemory {
        fn with(sessions: Vec<Session>) -> Arc<Self> {
            Arc::new(Self { sessions })
        }
    }

    fn session_with(id: &str, title: &str, updated_secs_ago: i64) -> Session {
        let mut s = Session::new("default");
        s.id = id.to_string();
        s.title = title.to_string();
        s.updated_at = chrono::Utc::now() - chrono::Duration::seconds(updated_secs_ago);
        s
    }

    #[async_trait]
    impl MemoryStore for StubMemory {
        async fn remember(&self, _e: MemoryEntry) -> CoreResult<()> {
            Ok(())
        }
        async fn recall(&self, _q: &str, _l: usize) -> CoreResult<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _q: &str) -> CoreResult<usize> {
            Ok(0)
        }
        async fn list_memories(&self, _l: usize) -> CoreResult<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn save_session(&self, _s: &Session) -> CoreResult<()> {
            Ok(())
        }
        async fn load_session(&self, id: &str) -> CoreResult<Option<Session>> {
            Ok(self.sessions.iter().find(|s| s.id == id).cloned())
        }
        async fn list_sessions(&self, profile: &str, limit: usize) -> CoreResult<Vec<Session>> {
            let mut out: Vec<Session> = self
                .sessions
                .iter()
                .filter(|s| s.profile == profile)
                .cloned()
                .collect();
            out.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
            out.truncate(limit);
            Ok(out)
        }
        async fn delete_session(&self, _id: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn search_sessions(&self, _q: &str, _l: usize) -> CoreResult<Vec<Session>> {
            Ok(vec![])
        }
    }

    fn three_sessions() -> Arc<StubMemory> {
        StubMemory::with(vec![
            session_with("id-1", "Riset pasar kripto", 300),
            session_with("id-2", "Riset pasar saham", 200),
            session_with("id-3", "Catatan harian", 100),
        ])
    }

    #[tokio::test]
    async fn resume_latest_memilih_sesi_terbaru() {
        let mem: Arc<dyn MemoryStore> = three_sessions();
        let s = resolve_session(&mem, "default", "latest").await.unwrap();
        assert_eq!(s.id, "id-3");
        // varian kapitalisasi juga diterima
        let s = resolve_session(&mem, "default", "LATEST").await.unwrap();
        assert_eq!(s.id, "id-3");
    }

    #[tokio::test]
    async fn resume_latest_tanpa_sesi_error_jelas() {
        let mem: Arc<dyn MemoryStore> = StubMemory::with(vec![]);
        let err = resolve_session(&mem, "default", "latest")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("tidak ada sesi"), "{err}");
    }

    #[tokio::test]
    async fn resume_id_passthrough() {
        let mem: Arc<dyn MemoryStore> = three_sessions();
        let s = resolve_session(&mem, "default", "id-1").await.unwrap();
        assert_eq!(s.title, "Riset pasar kripto");
    }

    #[tokio::test]
    async fn resume_substring_judul_unik() {
        let mem: Arc<dyn MemoryStore> = three_sessions();
        let s = resolve_session(&mem, "default", "harian").await.unwrap();
        assert_eq!(s.id, "id-3");
        // case-insensitive
        let s = resolve_session(&mem, "default", "HARIAN").await.unwrap();
        assert_eq!(s.id, "id-3");
    }

    #[tokio::test]
    async fn resume_substring_ambigu_error_menyebut_kandidat() {
        let mem: Arc<dyn MemoryStore> = three_sessions();
        let err = resolve_session(&mem, "default", "riset")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("2 sesi"), "{err}");
        assert!(err.contains("id-1") && err.contains("id-2"), "{err}");
    }

    #[tokio::test]
    async fn resume_tanpa_cocok_error_jelas() {
        let mem: Arc<dyn MemoryStore> = three_sessions();
        let err = resolve_session(&mem, "default", "zzz-tak-ada")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("tidak ada sesi yang cocok"), "{err}");
        assert!(err.contains("latest"), "{err}");
    }

    // ---------------------------------------------------------------
    // B5: sekali-jalan tepat satu turn (mock provider)
    // ---------------------------------------------------------------

    /// Provider mock yang menghitung pemanggilan chat_stream.
    struct CountingProvider {
        calls: AtomicUsize,
        reply: String,
    }

    impl CountingProvider {
        fn new(reply: &str) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                reply: reply.to_string(),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for CountingProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(&self, _req: ChatRequest) -> CoreResult<mpsc::Receiver<StreamEvent>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel(8);
            let reply = self.reply.clone();
            tokio::spawn(async move {
                let _ = tx.send(StreamEvent::TextDelta(reply)).await;
                let _ = tx.send(StreamEvent::Done(Usage::default())).await;
            });
            Ok(rx)
        }
    }

    struct StubSkills;

    #[async_trait]
    impl SkillStore for StubSkills {
        async fn list(&self) -> CoreResult<Vec<Skill>> {
            Ok(vec![])
        }
        async fn get(&self, _n: &str) -> CoreResult<Option<Skill>> {
            Ok(None)
        }
        async fn create(&self, n: &str, d: &str, b: &str) -> CoreResult<Skill> {
            Ok(Skill {
                name: n.to_string(),
                description: d.to_string(),
                path: std::path::PathBuf::from(n),
                body: b.to_string(),
                triggers: vec![],
            })
        }
        async fn update(&self, _n: &str, _b: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn delete(&self, _n: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn find_relevant(&self, _t: &str, _l: usize) -> CoreResult<Vec<Skill>> {
            Ok(vec![])
        }
    }

    fn test_agent(provider: Arc<CountingProvider>) -> (Agent, Arc<CountingProvider>) {
        let (tx, _rx) = mpsc::channel(512);
        // Judul non-"Untitled" agar maybe_generate_title tidak menambah
        // panggilan provider kedua.
        let mut session = Session::new("default");
        session.title = "Sesi Uji".to_string();
        let deps = hermes_core::AgentDeps {
            provider: provider.clone(),
            tools: Arc::new(ToolRegistry::new()),
            memory: StubMemory::with(vec![]),
            skills: Arc::new(StubSkills),
            approval: Arc::new(AllowAllApproval),
            profile: hermes_core::ProfileConfig::default(),
            hermes_home: std::env::temp_dir(),
            cancel: CancelFlag::new(),
            memory_config: hermes_core::MemoryConfig::default(),
            events: tx,
        };
        (Agent::new(deps, session), provider)
    }

    /// B5: mode sekali-jalan menjalankan TEPAT satu turn lalu selesai —
    /// provider mock hanya dipanggil sekali dan jawabannya dikembalikan.
    #[tokio::test]
    async fn oneshot_berjalan_tepat_satu_turn() {
        let provider = CountingProvider::new("jawaban mock");
        let (mut agent, provider) = test_agent(provider);
        let reply = run_single_turn(&mut agent, "halo").await.unwrap();
        assert_eq!(reply, "jawaban mock");
        assert_eq!(
            provider.calls(),
            1,
            "satu turn = tepat satu panggilan provider"
        );
        // Sesi tetap dipersist (satu pesan user + satu balasan asisten).
        assert_eq!(agent.session().messages.len(), 2);
    }

    /// B3 (ujung ke ujung level ChatRequest): effort reasoning dari profil
    /// diteruskan agen ke provider.
    #[tokio::test]
    async fn reasoning_effort_diteruskan_ke_provider() {
        use std::sync::Mutex as StdMutex;
        struct CaptureProvider {
            seen: StdMutex<Vec<Option<String>>>,
        }
        #[async_trait]
        impl LlmProvider for CaptureProvider {
            fn name(&self) -> &str {
                "cap"
            }
            fn default_model(&self) -> &str {
                "cap-model"
            }
            async fn chat_stream(
                &self,
                req: ChatRequest,
            ) -> CoreResult<mpsc::Receiver<StreamEvent>> {
                self.seen.lock().unwrap().push(req.reasoning_effort.clone());
                let (tx, rx) = mpsc::channel(8);
                tokio::spawn(async move {
                    let _ = tx.send(StreamEvent::TextDelta("ok".to_string())).await;
                    let _ = tx.send(StreamEvent::Done(Usage::default())).await;
                });
                Ok(rx)
            }
        }
        let provider = Arc::new(CaptureProvider {
            seen: StdMutex::new(vec![]),
        });
        let (tx, _rx) = mpsc::channel(512);
        let mut session = Session::new("default");
        session.title = "Sesi Uji".to_string();
        let profile = hermes_core::ProfileConfig {
            reasoning_effort: Some("xhigh".to_string()),
            ..Default::default()
        };
        let deps = hermes_core::AgentDeps {
            provider: provider.clone(),
            tools: Arc::new(ToolRegistry::new()),
            memory: StubMemory::with(vec![]),
            skills: Arc::new(StubSkills),
            approval: Arc::new(AllowAllApproval),
            profile,
            hermes_home: std::env::temp_dir(),
            cancel: CancelFlag::new(),
            memory_config: hermes_core::MemoryConfig::default(),
            events: tx,
        };
        let mut agent = Agent::new(deps, session);
        agent.run_turn("halo").await.unwrap();
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].as_deref(), Some("xhigh"));
    }
}
#[cfg(test)]
mod b7_cli_tests {
    //! Uji batch 7 (tier 1): subcommand CLI yang hilang dari audit paritas —
    //! `session list/show/delete` (alias `sessions`), `config get/unset`,
    //! `logs`, `status`, `tools list/enable/disable`.
    //! Plus batch 8 (tier 2): `session import/rename/stats/prune`,
    //! `config check`, `providers list`, `run --format`, `eval list`,
    //! `skills inspect`, `memory status`.
    use super::*;
    use crate::app::{expand_config_env, App};
    use crate::cmd::cli::{
        ConfigCmd, EvalCmd, MemoryCmd, ProvidersCmd, SessionCmd, SkillsCmd, ToolsCmd,
    };
    use crate::cmd::logs::{
        extract_log_level, extract_log_timestamp, log_file_name, log_level_order, parse_log_since,
        tail_matching_lines, LogFilter,
    };
    use crate::cmd::session::parse_age;
    use clap::Parser;
    use hermes_core::{
        ChatRequest, LlmProvider, MemoryEntry, MemoryStore, Result as CoreResult, Session, Skill,
        SkillStore, StreamEvent,
    };
    use std::sync::Arc;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("hermes").chain(args.iter().copied()))
    }

    fn session_cmd_of(cli: Cli) -> SessionCmd {
        match cli.command {
            Some(Commands::Session { cmd }) => cmd,
            other => panic!("diharapkan subcommand session, dapat {other:?}"),
        }
    }

    // ---- parsing ----

    #[test]
    fn sessions_alias_list_diterima() {
        // Nama asli (`hermes sessions list`) tersedia sebagai alias.
        let cli = parse(&["sessions", "list", "--limit", "5"]).expect("parse harus berhasil");
        match session_cmd_of(cli) {
            SessionCmd::List { limit, source } => {
                assert_eq!(limit, 5);
                assert_eq!(source, None);
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn session_list_source_filter_diterima() {
        let cli =
            parse(&["session", "list", "--source", "telegram"]).expect("parse harus berhasil");
        match session_cmd_of(cli) {
            SessionCmd::List { source, .. } => assert_eq!(source.as_deref(), Some("telegram")),
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn session_show_dan_delete_parse() {
        let cli = parse(&["session", "show", "abc123"]).expect("parse harus berhasil");
        match session_cmd_of(cli) {
            SessionCmd::Show { session } => assert_eq!(session, "abc123"),
            other => panic!("varian tak terduga: {other:?}"),
        }
        let cli = parse(&["session", "delete", "abc123", "--yes"]).expect("parse harus berhasil");
        match session_cmd_of(cli) {
            SessionCmd::Delete { session_id, yes } => {
                assert_eq!(session_id, "abc123");
                assert!(yes);
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn config_get_unset_parse() {
        let cli = parse(&["config", "get", "model"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Config {
                cmd: ConfigCmd::Get { key, json },
            }) => {
                assert_eq!(key.as_deref(), Some("model"));
                assert!(!json);
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
        let cli = parse(&["config", "get", "--json"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Config {
                cmd: ConfigCmd::Get { key, json },
            }) => {
                assert_eq!(key, None);
                assert!(json);
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
        let cli = parse(&["config", "unset", "reasoning"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Config {
                cmd: ConfigCmd::Unset { key },
            }) => assert_eq!(key, "reasoning"),
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn logs_parse_lengkap() {
        let cli = parse(&[
            "logs",
            "errors",
            "-n",
            "100",
            "-f",
            "--level",
            "WARNING",
            "--session",
            "abc",
            "--since",
            "1h",
        ])
        .expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Logs {
                log,
                lines,
                follow,
                level,
                session,
                since,
            }) => {
                assert_eq!(log, "errors");
                assert_eq!(lines, 100);
                assert!(follow);
                assert_eq!(level.as_deref(), Some("WARNING"));
                assert_eq!(session.as_deref(), Some("abc"));
                assert_eq!(since.as_deref(), Some("1h"));
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn logs_list_dan_default_parse() {
        let cli = parse(&["logs", "list"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Logs { log, .. }) => assert_eq!(log, "list"),
            other => panic!("varian tak terduga: {other:?}"),
        }
        let cli = parse(&["logs"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Logs { log, lines, .. }) => {
                assert_eq!(log, "agent");
                assert_eq!(lines, 50);
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn status_parse_full_deep() {
        let cli = parse(&["status", "--full", "--deep"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Status { full, deep }) => {
                assert!(full);
                assert!(deep);
            }
            other => panic!("varian tak terduga: {other:?}"),
        }
        // `--all` adalah alias `--full` seperti aslinya.
        let cli = parse(&["status", "--all"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Status { full, .. }) => assert!(full),
            other => panic!("varian tak terduga: {other:?}"),
        }
    }

    #[test]
    fn tools_parse() {
        let cli = parse(&["tools", "list"]).expect("parse harus berhasil");
        assert!(matches!(
            cli.command,
            Some(Commands::Tools {
                cmd: ToolsCmd::List
            })
        ));
        let cli = parse(&["tools", "disable", "kulit-pisang"]).expect("parse harus berhasil");
        match cli.command {
            Some(Commands::Tools {
                cmd: ToolsCmd::Disable { name },
            }) => assert_eq!(name, "kulit-pisang"),
            other => panic!("varian tak terduga: {other:?}"),
        }
        let cli = parse(&["tools", "enable", "kulit-pisang"]).expect("parse harus berhasil");
        assert!(matches!(
            cli.command,
            Some(Commands::Tools {
                cmd: ToolsCmd::Enable { .. }
            })
        ));
    }

    // ---- helper log murni ----

    #[test]
    fn log_helpers_parse_level_timestamp_since() {
        assert_eq!(log_level_order("WARNING"), Some(2));
        assert_eq!(log_level_order("warn"), None); // case-sensitive di level fungsi ini
        assert_eq!(log_level_order("BOGUS"), None);
        // Gaya tracing (Rust) dan gaya logging (Python) sama-sama dikenali.
        assert_eq!(
            extract_log_level("2026-10-21T17:00:00.1Z  WARN hermes_cli: halo"),
            Some("WARN")
        );
        assert_eq!(
            extract_log_level("2026-10-21 17:00:00,123 INFO gateway.run: halo"),
            Some("INFO")
        );
        assert_eq!(extract_log_level("baris tanpa level"), None);
        // Timestamp bentuk T dan spasi.
        assert!(extract_log_timestamp("2026-10-21T17:00:00 mulai").is_some());
        assert!(extract_log_timestamp("2026-10-21 17:00:00 mulai").is_some());
        assert!(extract_log_timestamp("tanpa stempel").is_none());
        assert_eq!(parse_log_since("30m"), Some(chrono::Duration::minutes(30)));
        assert_eq!(parse_log_since("2d"), Some(chrono::Duration::days(2)));
        assert_eq!(parse_log_since("kemarin"), None);
        assert_eq!(log_file_name("agent"), Some("agent.log"));
        assert_eq!(log_file_name("bogus"), None);
    }

    #[test]
    fn tail_matching_lines_filter_level_session_since() {
        let text = "2026-10-21 10:00:00 INFO agent: mulai [sess_aaa]\n\
                    2026-10-21 10:01:00 WARNING agent: lambat [sess_aaa]\n\
                    2026-10-21 10:02:00 ERROR agent: gagal [sess_bbb]\n\
                    baris tanpa stempel tetap lolos filter waktu\n";
        let f = LogFilter {
            min_level: Some(2),
            session: None,
            since: None,
        };
        let out = tail_matching_lines(text, 10, &f);
        assert_eq!(out.len(), 3); // INFO tersaring; baris tanpa level lolos (paritas aslinya)
        assert!(out[0].contains("WARNING"));

        let f = LogFilter {
            min_level: None,
            session: Some("sess_bbb".to_string()),
            since: None,
        };
        let out = tail_matching_lines(text, 10, &f);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("ERROR"));

        // N terakhir saja.
        let f = LogFilter {
            min_level: None,
            session: None,
            since: None,
        };
        let out = tail_matching_lines(text, 2, &f);
        assert_eq!(out.len(), 2);
        assert!(out[1].contains("tanpa stempel"));
    }

    // ---- perilaku dengan direktori sementara ----

    fn temp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-b7-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        dir
    }

    #[tokio::test]
    async fn cmd_logs_baca_file_dan_list() {
        let home = temp_home("logs");
        std::fs::write(
            home.join("logs").join("agent.log"),
            "2026-10-21 10:00:00 INFO agent: satu\n2026-10-21 10:01:00 ERROR agent: dua\n",
        )
        .unwrap();
        cmd_logs(&home, "agent", 50, false, None, None, None)
            .await
            .expect("baca log harus Ok");
        cmd_logs(&home, "list", 50, false, None, None, None)
            .await
            .expect("logs list harus Ok");
        // Filter level: hanya ERROR yang lolos.
        cmd_logs(&home, "agent", 50, false, Some("ERROR"), None, None)
            .await
            .expect("filter level harus Ok");
        // Nama tak dikenal → error jelas.
        assert!(cmd_logs(&home, "bogus", 50, false, None, None, None)
            .await
            .is_err());
        // Level tak dikenal → error jelas.
        assert!(
            cmd_logs(&home, "agent", 50, false, Some("BOGUS"), None, None)
                .await
                .is_err()
        );
        // File belum ada → Ok dengan catatan (bukan error).
        cmd_logs(&home, "gateway", 50, false, None, None, None)
            .await
            .expect("log hilang harus Ok");
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---- App minimal untuk cmd_session / cmd_config / cmd_status / cmd_tools ----

    struct B7Provider;
    #[async_trait::async_trait]
    impl LlmProvider for B7Provider {
        fn name(&self) -> &str {
            "b7"
        }
        fn default_model(&self) -> &str {
            "b7-model"
        }
        async fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> CoreResult<tokio::sync::mpsc::Receiver<StreamEvent>> {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            drop(tx);
            Ok(rx)
        }
    }

    struct B7Memory {
        sessions: std::sync::Mutex<Vec<Session>>,
    }
    #[async_trait::async_trait]
    impl MemoryStore for B7Memory {
        async fn remember(&self, _e: MemoryEntry) -> CoreResult<()> {
            Ok(())
        }
        async fn recall(&self, _q: &str, _l: usize) -> CoreResult<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn forget(&self, _q: &str) -> CoreResult<usize> {
            Ok(0)
        }
        async fn list_memories(&self, _l: usize) -> CoreResult<Vec<MemoryEntry>> {
            Ok(vec![])
        }
        async fn save_session(&self, s: &Session) -> CoreResult<()> {
            // Upsert seperti SqliteMemoryStore (ON CONFLICT DO UPDATE).
            let mut guard = self.sessions.lock().unwrap();
            guard.retain(|x| x.id != s.id);
            guard.push(s.clone());
            Ok(())
        }
        async fn load_session(&self, id: &str) -> CoreResult<Option<Session>> {
            Ok(self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.id == id)
                .cloned())
        }
        async fn list_sessions(&self, profile: &str, limit: usize) -> CoreResult<Vec<Session>> {
            let mut out: Vec<Session> = self
                .sessions
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.profile == profile)
                .cloned()
                .collect();
            out.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
            out.truncate(limit);
            Ok(out)
        }
        async fn delete_session(&self, id: &str) -> CoreResult<()> {
            self.sessions.lock().unwrap().retain(|s| s.id != id);
            Ok(())
        }
        async fn search_sessions(&self, _q: &str, _l: usize) -> CoreResult<Vec<Session>> {
            Ok(vec![])
        }
    }

    struct B7Skills;
    #[async_trait::async_trait]
    impl SkillStore for B7Skills {
        async fn list(&self) -> CoreResult<Vec<Skill>> {
            Ok(vec![])
        }
        async fn get(&self, n: &str) -> CoreResult<Option<Skill>> {
            // Skill kalengan untuk uji `skills inspect`.
            if n == "demo" {
                return Ok(Some(Skill {
                    name: "demo".to_string(),
                    description: "Skill demo untuk tes".to_string(),
                    path: std::path::PathBuf::from("demo/SKILL.md"),
                    body: "# Demo\n\nBaris 1\nBaris 2\n".to_string(),
                    triggers: vec!["demo".to_string()],
                }));
            }
            Ok(None)
        }
        async fn create(&self, n: &str, d: &str, b: &str) -> CoreResult<Skill> {
            Ok(Skill {
                name: n.to_string(),
                description: d.to_string(),
                path: std::path::PathBuf::from(n),
                body: b.to_string(),
                triggers: vec![],
            })
        }
        async fn update(&self, _n: &str, _b: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn delete(&self, _n: &str) -> CoreResult<()> {
            Ok(())
        }
        async fn find_relevant(&self, _t: &str, _l: usize) -> CoreResult<Vec<Skill>> {
            Ok(vec![])
        }
    }

    fn b7_session(id: &str, title: &str, with_message: bool) -> Session {
        let mut s = Session::new("default");
        s.id = id.to_string();
        s.title = title.to_string();
        if with_message {
            s.messages.push(hermes_core::Message::user("halo dunia"));
        }
        s
    }

    fn b7_app(home: &std::path::Path, sessions: Vec<Session>) -> App {
        let cfg = hermes_core::HermesConfig::default();
        App {
            config: cfg.clone(),
            config_raw: cfg.clone(),
            profile_name: "default".to_string(),
            profile: cfg.profile(),
            hermes_home: home.to_path_buf(),
            provider: Arc::new(B7Provider),
            tools: Arc::new(hermes_core::ToolRegistry::new()),
            memory: Arc::new(B7Memory {
                sessions: std::sync::Mutex::new(sessions),
            }),
            skills: Arc::new(B7Skills),
            platforms: Arc::new(hermes_gateway::PlatformRegistry::new()),
            cron_store: Arc::new(
                hermes_cron::CronStore::open_default(home).expect("cron store temp"),
            ),
            approval: Arc::new(hermes_core::AllowAllApproval),
            cancel: hermes_core::CancelFlag::new(),
        }
    }

    #[tokio::test]
    async fn session_list_show_delete_berperilaku() {
        let home = temp_home("session");
        let app = b7_app(
            &home,
            vec![
                b7_session("s1", "Sesi Satu", true),
                b7_session("s2", "Sesi Dua", false),
            ],
        );
        // list: dua sesi, tanpa error
        cmd_session(
            &app,
            SessionCmd::List {
                limit: 20,
                source: None,
            },
        )
        .await
        .expect("session list harus Ok");
        // list dengan filter source yang tak cocok → tetap Ok
        cmd_session(
            &app,
            SessionCmd::List {
                limit: 20,
                source: Some("telegram".to_string()),
            },
        )
        .await
        .expect("session list filter harus Ok");
        // show: ada
        cmd_session(
            &app,
            SessionCmd::Show {
                session: "s1".to_string(),
            },
        )
        .await
        .expect("session show harus Ok");
        // show: tak ada → error jelas
        assert!(cmd_session(
            &app,
            SessionCmd::Show {
                session: "hilang".to_string(),
            }
        )
        .await
        .is_err());
        // delete dengan --yes: sesi benar-benar hilang dari store
        cmd_session(
            &app,
            SessionCmd::Delete {
                session_id: "s1".to_string(),
                yes: true,
            },
        )
        .await
        .expect("session delete harus Ok");
        assert!(app.memory.load_session("s1").await.unwrap().is_none());
        assert!(app.memory.load_session("s2").await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn config_get_unset_berperilaku() {
        let home = temp_home("config");
        let mut app = b7_app(&home, vec![]);
        app.config_raw
            .profiles
            .entry("default".to_string())
            .or_default()
            .model = "model-uji".to_string();
        app.config = expand_config_env(app.config_raw.clone()).unwrap();
        app.profile = app.config.profile();
        // get kunci yang dikenal
        cmd_config(
            &mut app,
            ConfigCmd::Get {
                key: Some("model".to_string()),
                json: false,
            },
        )
        .await
        .expect("config get harus Ok");
        // get tanpa kunci → daftar semua
        cmd_config(
            &mut app,
            ConfigCmd::Get {
                key: None,
                json: true,
            },
        )
        .await
        .expect("config get semua harus Ok");
        // get kunci tak dikenal → error
        assert!(cmd_config(
            &mut app,
            ConfigCmd::Get {
                key: Some("bogus".to_string()),
                json: false,
            }
        )
        .await
        .is_err());
        // unset mengembalikan default dan tersimpan ke disk
        cmd_config(
            &mut app,
            ConfigCmd::Unset {
                key: "model".to_string(),
            },
        )
        .await
        .expect("config unset harus Ok");
        assert_eq!(
            app.config_raw.profiles["default"].model,
            hermes_core::ProfileConfig::default().model
        );
        assert!(home.join("config.toml").is_file());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn status_dan_tools_berperilaku() {
        let home = temp_home("status");
        let mut app = b7_app(&home, vec![b7_session("s1", "Sesi Satu", false)]);
        cmd_status(&app, false, false)
            .await
            .expect("status harus Ok");
        cmd_status(&app, true, true)
            .await
            .expect("status --full --deep harus Ok");
        // tools list kosong (registry baru) → Ok
        cmd_tools(&mut app, &ToolsCmd::List)
            .await
            .expect("tools list harus Ok");
        // disable lalu enable: tertulis ke config.toml temp
        cmd_tools(
            &mut app,
            &ToolsCmd::Disable {
                name: "skill-x".to_string(),
            },
        )
        .await
        .expect("tools disable harus Ok");
        assert!(app
            .config_raw
            .skills
            .disabled_skills
            .contains(&"skill-x".to_string()));
        let raw = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(raw.contains("skill-x"), "disable harus tersimpan ke disk");
        cmd_tools(
            &mut app,
            &ToolsCmd::Enable {
                name: "skill-x".to_string(),
            },
        )
        .await
        .expect("tools enable harus Ok");
        assert!(app.config_raw.skills.disabled_skills.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---- batch 8 (tier 2): parsing ----

    fn providers_cmd_of(cli: Cli) -> ProvidersCmd {
        match cli.command {
            Some(Commands::Providers { cmd }) => cmd,
            other => panic!("diharapkan subcommand providers, dapat {other:?}"),
        }
    }

    #[test]
    fn b8_parsing_subcommand_baru() {
        // providers list
        assert!(matches!(
            providers_cmd_of(parse(&["providers", "list"]).expect("parse")),
            ProvidersCmd::List
        ));
        // session import: default dan semua flag
        match session_cmd_of(parse(&["session", "import", "sesi.jsonl"]).expect("parse")) {
            SessionCmd::Import {
                file,
                format,
                id,
                title,
            } => {
                assert_eq!(file, "sesi.jsonl");
                assert_eq!(format, "jsonl");
                assert!(id.is_none() && title.is_none());
            }
            other => panic!("diharapkan Import, dapat {other:?}"),
        }
        match session_cmd_of(
            parse(&[
                "session",
                "import",
                "sesi.json",
                "--format",
                "trajectory",
                "--id",
                "imp-1",
                "--title",
                "Judul Impor",
            ])
            .expect("parse"),
        ) {
            SessionCmd::Import {
                format, id, title, ..
            } => {
                assert_eq!(format, "trajectory");
                assert_eq!(id.as_deref(), Some("imp-1"));
                assert_eq!(title.as_deref(), Some("Judul Impor"));
            }
            other => panic!("diharapkan Import, dapat {other:?}"),
        }
        // session rename: judul multi-kata
        match session_cmd_of(parse(&["session", "rename", "s1", "Judul", "Baru"]).expect("parse")) {
            SessionCmd::Rename { session, title } => {
                assert_eq!(session, "s1");
                assert_eq!(title, vec!["Judul".to_string(), "Baru".to_string()]);
            }
            other => panic!("diharapkan Rename, dapat {other:?}"),
        }
        // session stats
        assert!(matches!(
            session_cmd_of(parse(&["session", "stats"]).expect("parse")),
            SessionCmd::Stats
        ));
        // session prune: default 90 hari
        match session_cmd_of(parse(&["session", "prune"]).expect("parse")) {
            SessionCmd::Prune {
                older_than,
                dry_run,
                yes,
            } => {
                assert_eq!(older_than, "90");
                assert!(!dry_run && !yes);
            }
            other => panic!("diharapkan Prune, dapat {other:?}"),
        }
        match session_cmd_of(
            parse(&[
                "session",
                "prune",
                "--older-than",
                "30d",
                "--dry-run",
                "--yes",
            ])
            .expect("parse"),
        ) {
            SessionCmd::Prune {
                older_than,
                dry_run,
                yes,
            } => {
                assert_eq!(older_than, "30d");
                assert!(dry_run && yes);
            }
            other => panic!("diharapkan Prune, dapat {other:?}"),
        }
        // config check
        assert!(matches!(
            parse(&["config", "check"]).expect("parse").command,
            Some(Commands::Config {
                cmd: ConfigCmd::Check
            })
        ));
        // run --format json
        match parse(&["run", "-q", "halo", "--format", "json"])
            .expect("parse")
            .command
        {
            Some(Commands::Run { format, .. }) => assert_eq!(format, "json"),
            other => panic!("diharapkan Run, dapat {other:?}"),
        }
        // run default: text
        match parse(&["run", "-q", "halo"]).expect("parse").command {
            Some(Commands::Run { format, .. }) => assert_eq!(format, "text"),
            other => panic!("diharapkan Run, dapat {other:?}"),
        }
        // eval list
        assert!(matches!(
            parse(&["eval", "list"]).expect("parse").command,
            Some(Commands::Eval { cmd: EvalCmd::List })
        ));
        // skills inspect
        match parse(&["skills", "inspect", "demo"])
            .expect("parse")
            .command
        {
            Some(Commands::Skills {
                cmd: SkillsCmd::Inspect { name },
            }) => assert_eq!(name, "demo"),
            other => panic!("diharapkan Skills::Inspect, dapat {other:?}"),
        }
        // memory status
        assert!(matches!(
            parse(&["memory", "status"]).expect("parse").command,
            Some(Commands::Memory {
                cmd: MemoryCmd::Status
            })
        ));
    }

    // ---- batch 8 (tier 2): perilaku ----

    #[test]
    fn parse_age_menerima_format_asli() {
        use chrono::Duration;
        assert_eq!(parse_age("90"), Some(Duration::days(90)));
        assert_eq!(parse_age("2d"), Some(Duration::days(2)));
        assert_eq!(parse_age("5h"), Some(Duration::hours(5)));
        assert_eq!(parse_age("1w"), Some(Duration::weeks(1)));
        assert_eq!(parse_age(" 30d "), Some(Duration::days(30)));
        assert_eq!(parse_age("abc"), None);
        assert_eq!(parse_age("0"), None);
        assert_eq!(parse_age("-5d"), None);
        assert_eq!(parse_age(""), None);
    }

    #[tokio::test]
    async fn session_import_jsonl_round_trip() {
        let home = temp_home("import-jsonl");
        let app = b7_app(&home, vec![]);
        // Tulis file jsonl: dua pesan.
        let m1 = hermes_core::Message::user("halo");
        let m2 = hermes_core::Message::assistant_text("hai juga");
        let path = home.join("sesi.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&m1).unwrap(),
                serde_json::to_string(&m2).unwrap()
            ),
        )
        .unwrap();
        cmd_session(
            &app,
            SessionCmd::Import {
                file: path.display().to_string(),
                format: "jsonl".to_string(),
                id: Some("imp-1".to_string()),
                title: Some("Sesi Impor".to_string()),
            },
        )
        .await
        .expect("import jsonl harus Ok");
        let s = app
            .memory
            .load_session("imp-1")
            .await
            .unwrap()
            .expect("sesi hasil impor harus ada");
        assert_eq!(s.messages.len(), 2);
        assert_eq!(s.title, "Sesi Impor");
        assert_eq!(s.profile, "default");
        // ID bentrok → error jelas.
        let err = cmd_session(
            &app,
            SessionCmd::Import {
                file: path.display().to_string(),
                format: "jsonl".to_string(),
                id: Some("imp-1".to_string()),
                title: None,
            },
        )
        .await;
        assert!(err.is_err(), "id duplikat harus ditolak");
        // File tak ada → error jelas.
        assert!(cmd_session(
            &app,
            SessionCmd::Import {
                file: home.join("hilang.jsonl").display().to_string(),
                format: "jsonl".to_string(),
                id: None,
                title: None,
            },
        )
        .await
        .is_err());
        // Format tak dikenal → error jelas.
        assert!(cmd_session(
            &app,
            SessionCmd::Import {
                file: path.display().to_string(),
                format: "xml".to_string(),
                id: None,
                title: None,
            },
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn session_import_trajectory_membangun_pesan() {
        let home = temp_home("import-traj");
        let app = b7_app(&home, vec![]);
        let traj = serde_json::json!({
            "session_id": "abc123",
            "profile": "default",
            "steps": [
                {"role": "user", "content": "halo"},
                {"role": "assistant", "content": "baik"},
                {"role": "assistant", "tool_calls": [
                    {"id": "c1", "name": "exec", "arguments": {"cmd": "ls"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "name": "exec", "content": "ok"},
            ],
        });
        let path = home.join("sesi.trajectory.json");
        std::fs::write(&path, serde_json::to_string_pretty(&traj).unwrap()).unwrap();
        cmd_session(
            &app,
            SessionCmd::Import {
                file: path.display().to_string(),
                format: "trajectory".to_string(),
                id: None,
                title: None,
            },
        )
        .await
        .expect("import trajectory harus Ok");
        let sessions = app.memory.list_sessions("default", 10).await.unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].messages.len(), 4);
        assert_eq!(sessions[0].title, "imported-abc123");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn session_rename_mengganti_judul() {
        let home = temp_home("rename");
        let app = b7_app(&home, vec![b7_session("s1", "Sesi Satu", false)]);
        cmd_session(
            &app,
            SessionCmd::Rename {
                session: "s1".to_string(),
                title: vec!["Judul".to_string(), "Baru".to_string()],
            },
        )
        .await
        .expect("rename harus Ok");
        let s = app.memory.load_session("s1").await.unwrap().unwrap();
        assert_eq!(s.title, "Judul Baru");
        // Judul kosong → error.
        assert!(cmd_session(
            &app,
            SessionCmd::Rename {
                session: "s1".to_string(),
                title: vec![],
            },
        )
        .await
        .is_err());
        // Sesi tak ada → error.
        assert!(cmd_session(
            &app,
            SessionCmd::Rename {
                session: "hilang".to_string(),
                title: vec!["x".to_string()],
            },
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn session_stats_dan_prune() {
        let home = temp_home("stats-prune");
        let mut old = b7_session("lama", "Sesi Lama", true);
        old.updated_at = chrono::Utc::now() - chrono::Duration::days(100);
        let mut other = b7_session("lain", "Sesi Lain", true);
        other.profile = "kerja".to_string();
        let app = b7_app(
            &home,
            vec![old, b7_session("baru", "Sesi Baru", true), other],
        );
        // stats: agregat tanpa error
        cmd_session(&app, SessionCmd::Stats)
            .await
            .expect("stats harus Ok");
        // prune dry-run: tidak ada yang terhapus
        cmd_session(
            &app,
            SessionCmd::Prune {
                older_than: "90".to_string(),
                dry_run: true,
                yes: true,
            },
        )
        .await
        .expect("prune dry-run harus Ok");
        assert!(app.memory.load_session("lama").await.unwrap().is_some());
        // prune nyata: hanya sesi 100-hari yang hilang
        cmd_session(
            &app,
            SessionCmd::Prune {
                older_than: "90d".to_string(),
                dry_run: false,
                yes: true,
            },
        )
        .await
        .expect("prune harus Ok");
        assert!(app.memory.load_session("lama").await.unwrap().is_none());
        assert!(app.memory.load_session("baru").await.unwrap().is_some());
        assert!(app.memory.load_session("lain").await.unwrap().is_some());
        // usia tak valid → error jelas
        assert!(cmd_session(
            &app,
            SessionCmd::Prune {
                older_than: "kemarin".to_string(),
                dry_run: true,
                yes: true,
            },
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn config_check_memvalidasi_profil() {
        let home = temp_home("check");
        let mut app = b7_app(&home, vec![]);
        // Config default: model terisi, provider tak dikenal tapi daftar
        // provider kosong → WARN, bukan FAIL → Ok.
        cmd_config(&mut app, ConfigCmd::Check)
            .await
            .expect("config check default harus Ok");
        // Model kosong → FAIL.
        app.config_raw
            .profiles
            .entry("default".to_string())
            .or_default()
            .model = String::new();
        app.config = expand_config_env(app.config_raw.clone()).unwrap();
        app.profile = app.config.profile();
        assert!(cmd_config(&mut app, ConfigCmd::Check).await.is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn providers_list_eval_list_skills_inspect_memory_status() {
        let home = temp_home("misc-b8");
        let mut app = b7_app(&home, vec![b7_session("s1", "Sesi Satu", true)]);
        // providers: kosong → Ok; terisi default → Ok
        cmd_providers(&app, ProvidersCmd::List).expect("providers list kosong harus Ok");
        app.config.providers = hermes_core::HermesConfig::default_providers();
        app.profile = app.config.profile();
        cmd_providers(&app, ProvidersCmd::List).expect("providers list terisi harus Ok");
        // eval list: tanpa menjalankan agen
        cmd_eval(&app, &EvalCmd::List)
            .await
            .expect("eval list harus Ok");
        // skills inspect: ada dan tidak ada
        cmd_skills(
            &app,
            SkillsCmd::Inspect {
                name: "demo".to_string(),
            },
        )
        .await
        .expect("skills inspect ada harus Ok");
        cmd_skills(
            &app,
            SkillsCmd::Inspect {
                name: "hilang".to_string(),
            },
        )
        .await
        .expect("skills inspect hilang harus Ok");
        // memory status
        cmd_memory(&app, MemoryCmd::Status)
            .await
            .expect("memory status harus Ok");
        let _ = std::fs::remove_dir_all(&home);
    }
}
