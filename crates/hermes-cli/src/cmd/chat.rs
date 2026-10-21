//! Handler chat & run + helper agen (AgentFactory, sesi, output).
//! Dipindah dari main.rs (refactor batas ukuran file).

use crate::app::App;
use crate::ui;
use hermes_core::Agent;
use hermes_core::AgentDeps;
use hermes_core::AgentEvent;
use hermes_core::CancelFlag;
use hermes_core::Session;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Buat Agent untuk satu sesi; event di-render ke terminal via task background.
pub(crate) fn build_agent(app: &App, session: Session) -> Agent {
    AgentFactory::from_app(app).build(session)
}

/// Pabrik Agent: menyimpan semua dependensi agar sesi baru mudah dibuat
/// (dipakai chat /clear, gateway per-chat, cron per-job).
#[derive(Clone)]
pub(crate) struct AgentFactory {
    pub(crate) provider: Arc<dyn hermes_core::LlmProvider>,
    pub(crate) tools: Arc<hermes_core::ToolRegistry>,
    pub(crate) memory: Arc<dyn hermes_core::MemoryStore>,
    pub(crate) skills: Arc<dyn hermes_core::SkillStore>,
    pub(crate) approval: Arc<dyn hermes_core::ApprovalHandler>,
    pub(crate) profile: hermes_core::ProfileConfig,
    pub(crate) memory_config: hermes_core::MemoryConfig,
    pub(crate) hermes_home: std::path::PathBuf,
    pub(crate) cancel: CancelFlag,
}

impl AgentFactory {
    pub(crate) fn from_app(app: &App) -> Self {
        Self {
            provider: app.provider.clone(),
            tools: app.tools.clone(),
            memory: app.memory.clone(),
            skills: app.skills.clone(),
            approval: app.approval.clone(),
            profile: app.profile.clone(),
            memory_config: app.config.memory.clone(),
            hermes_home: app.hermes_home.clone(),
            cancel: app.cancel.clone(),
        }
    }

    pub(crate) fn with_approval(&self, approval: Arc<dyn hermes_core::ApprovalHandler>) -> Self {
        let mut s = self.clone();
        s.approval = approval;
        s
    }

    pub(crate) fn with_working_dir(&self, dir: &str) -> Self {
        let mut s = self.clone();
        s.profile.working_dir = dir.to_string();
        s
    }

    /// Varian dengan bendera pembatalan sendiri (untuk sesi ACP: tiap sesi
    /// punya CancelFlag independen agar `session/cancel` hanya membatalkan
    /// turn sesi tersebut, bukan seluruh proses).
    pub(crate) fn with_cancel(&self, cancel: CancelFlag) -> Self {
        let mut s = self.clone();
        s.cancel = cancel;
        s
    }

    pub(crate) fn build(&self, session: Session) -> Agent {
        let (event_tx, event_rx) = mpsc::channel(512);
        tokio::spawn(async move { ui::render_events(event_rx).await });
        self.build_with_channel(session, event_tx)
    }

    /// Bangun agen tanpa render ke terminal (untuk gateway/cron): warning saja yang di-log.
    pub(crate) fn build_quiet(&self, session: Session) -> Agent {
        let (event_tx, mut event_rx) = mpsc::channel(512);
        tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                if let AgentEvent::Warning(w) = ev {
                    tracing::warn!("{w}");
                }
            }
        });
        self.build_with_channel(session, event_tx)
    }

    pub(crate) fn build_with_channel(
        &self,
        session: Session,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Agent {
        Agent::new(
            AgentDeps {
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                memory: self.memory.clone(),
                skills: self.skills.clone(),
                approval: self.approval.clone(),
                profile: self.profile.clone(),
                memory_config: self.memory_config.clone(),
                hermes_home: self.hermes_home.clone(),
                cancel: self.cancel.clone(),
                events: event_tx,
            },
            session,
        )
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Format output untuk mode sekali-jalan (`-q`/`--query-file`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Text,
    Json,
}

impl OutputFormat {
    pub(crate) fn parse(s: &str) -> anyhow::Result<Self> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => anyhow::bail!("--format `{other}` tidak dikenal (valid: text, json)"),
        }
    }
}

/// Opsi `chat` yang sudah dinormalisasi dari argumen clap.
pub(crate) struct ChatOptions {
    pub(crate) resume: Option<String>,
    pub(crate) continue_last: bool,
    pub(crate) query: Option<String>,
    pub(crate) query_file: Option<String>,
    pub(crate) oneshot: bool,
    pub(crate) format: OutputFormat,
}

/// Batas pencarian substring judul sesi (cukup besar untuk CLI interaktif
/// tanpa memindai seluruh DB).
pub(crate) const RESUME_TITLE_SEARCH_LIMIT: usize = 500;

/// Resolve spesifikasi sesi ala `--resume` (audit W6-B4):
/// - `latest` → sesi terbaru profil ini,
/// - id persis → langsung dipakai (passthrough),
/// - substring judul (case-insensitive) → harus cocok TEPAT satu;
///   0 cocok → error jelas; >1 → error dengan daftar kandidat.
pub(crate) async fn resolve_session(
    memory: &Arc<dyn hermes_core::MemoryStore>,
    profile_name: &str,
    spec: &str,
) -> anyhow::Result<Session> {
    let spec = spec.trim();
    if spec.is_empty() {
        anyhow::bail!("spesifikasi sesi kosong");
    }
    if spec.eq_ignore_ascii_case("latest") {
        return memory
            .list_sessions(profile_name, 1)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!("tidak ada sesi tersimpan untuk profil `{profile_name}`")
            });
    }
    if let Some(s) = memory.load_session(spec).await? {
        return Ok(s);
    }
    let needle = spec.to_lowercase();
    let candidates: Vec<Session> = memory
        .list_sessions(profile_name, RESUME_TITLE_SEARCH_LIMIT)
        .await?
        .into_iter()
        .filter(|s| s.title.to_lowercase().contains(&needle))
        .collect();
    match candidates.len() {
        0 => anyhow::bail!(
            "tidak ada sesi yang cocok dengan `{spec}` (coba id sesi persis atau `--resume latest`)"
        ),
        1 => Ok(candidates.into_iter().next().expect("satu kandidat")),
        n => {
            let mut msg =
                format!("`{spec}` cocok dengan {n} sesi; persempit judul atau pakai id persis:\n");
            for s in candidates.iter().take(10) {
                msg.push_str(&format!("  - {} — {}\n", s.id, s.title));
            }
            if n > 10 {
                msg.push_str(&format!("  ... dan {} lainnya\n", n - 10));
            }
            anyhow::bail!("{msg}");
        }
    }
}

/// Baca prompt sekali-jalan dari file; `-` = stdin (paritas original).
pub(crate) fn read_query_file(path: &str) -> anyhow::Result<String> {
    if path == "-" {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
        if buf.trim().is_empty() {
            anyhow::bail!("--query-file -: stdin kosong");
        }
        Ok(buf)
    } else {
        std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("gagal membaca --query-file `{path}`: {e}"))
    }
}

/// Jalankan TEPAT satu turn agen lalu selesai — inti mode sekali-jalan
/// (`-q`/`--query-file`/`--oneshot`). Non-interaktif: tanpa loop REPL.
pub(crate) async fn run_single_turn(agent: &mut Agent, prompt: &str) -> anyhow::Result<String> {
    agent.run_turn(prompt).await.map_err(anyhow::Error::from)
}

/// Cetak jawaban mode sekali-jalan sesuai `--format`.
pub(crate) fn print_query_answer(
    reply: &str,
    session_id: &str,
    usage: &hermes_core::Usage,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Text => println!("{reply}"),
        OutputFormat::Json => {
            let v = serde_json::json!({
                "response": reply,
                "session_id": session_id,
                "usage": {
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                },
            });
            println!("{v}");
        }
    }
}

pub(crate) async fn cmd_chat(app: &mut App, opts: ChatOptions) -> anyhow::Result<()> {
    let factory = AgentFactory::from_app(app);
    // B4: --resume <id|latest|substring-judul>, atau -c/--continue (= latest).
    let session = match (&opts.resume, opts.continue_last) {
        (Some(spec), _) => resolve_session(&app.memory, &app.profile_name, spec).await?,
        (None, true) => resolve_session(&app.memory, &app.profile_name, "latest").await?,
        (None, false) => Session::new(app.profile_name.clone()),
    };

    // B5: mode sekali-jalan non-interaktif — -q / --query-file menjawab lalu
    // keluar (clap menjamin keduanya tidak dipakai bersamaan). Agen dibangun
    // quiet agar --format json tidak tercemar render streaming.
    let prompt = match (&opts.query, &opts.query_file) {
        (Some(q), _) => Some(q.clone()),
        (None, Some(path)) => Some(read_query_file(path)?),
        (None, None) => None,
    };
    if let Some(prompt) = prompt {
        let mut agent = factory.build_quiet(session);
        let reply = run_single_turn(&mut agent, &prompt).await?;
        let usage = agent.usage().clone();
        let session_id = agent.session().id.clone();
        print_query_answer(&reply, &session_id, &usage, opts.format);
        return Ok(());
    }

    println!("Sesi: {} — {}", session.id, session.title);
    let agent = Arc::new(Mutex::new(factory.build(session)));
    let profile_name = app.profile_name.clone();
    let oneshot = opts.oneshot;

    ui::repl_loop_async(|input| {
        let agent = agent.clone();
        let factory = factory.clone();
        let profile_name = profile_name.clone();
        async move {
            if input.trim() == "/clear" {
                let mut guard = agent.lock().await;
                *guard = factory.build(Session::new(profile_name));
                println!("Sesi baru: {}", guard.session().id);
                return Ok(ui::ReplAction::Continue);
            }
            let mut guard = agent.lock().await;
            if let Err(e) = guard.run_turn(&input).await {
                eprintln!("❌ {e}");
            }
            // B5: --oneshot — tepat satu turn aktual lalu keluar (perintah
            // /clear di atas tidak dihitung sebagai turn).
            if oneshot {
                return Ok(ui::ReplAction::Exit);
            }
            Ok(ui::ReplAction::Continue)
        }
    })
    .await?;
    let id = agent.lock().await.session().id.clone();
    println!("\nSampai jumpa! Sesi tersimpan: {id}");
    Ok(())
}

pub(crate) async fn cmd_run(
    app: &mut App,
    query: &str,
    session_id: Option<String>,
    print: bool,
    format: &str,
) -> anyhow::Result<()> {
    let format = OutputFormat::parse(format)?;
    let session = match session_id {
        Some(id) => resolve_session(&app.memory, &app.profile_name, &id).await?,
        None => Session::new(app.profile_name.clone()),
    };
    let session_id_out = session.id.clone();
    let mut agent = build_agent(app, session);
    let reply = agent.run_turn(query).await?;
    // Bila provider tidak streaming teks via event (mis. error awal), cetak reply.
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::json!({
                    "response": reply,
                    "session": session_id_out,
                })
            );
        }
        OutputFormat::Text => {
            if print || reply.trim().is_empty() {
                println!("{reply}");
            }
        }
    }
    Ok(())
}
