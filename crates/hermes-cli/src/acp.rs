//! Server ACP (Agent Client Protocol) via stdio untuk integrasi IDE (Zed, dsb.).
//!
//! Berbicara JSON-RPC 2.0 dengan framing `Content-Length` seperti LSP.
//! Metode yang didukung: `initialize`, `authenticate`, `session/new`,
//! `session/load`, `session/resume`, `session/fork`, `session/list`,
//! `session/prompt`, `session/cancel`, `session/set_model`,
//! `session/set_mode`, `session/set_config_option`, `shutdown`, `exit`.
//!
//! Paritas dengan `acp_adapter/server.py` di Hermes asli: nama metode,
//! bentuk params, dan semantik load/resume/fork/list/set_model/set_mode/
//! authenticate/set_config_option mengikuti original (termasuk replay
//! riwayat via `session/update` SEBELUM respons load/resume, sesuai spek
//! ACP). Penyimpangan yang disengaja didokumentasikan di tiap handler.
use crate::app::save_config;
use crate::app::App;
use crate::cmd::chat::AgentFactory;
use hermes_core::{
    Agent, CancelFlag, HermesConfig, HermesError, LlmProvider, MemoryStore, Role, Session,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

/// Satu sesi ACP: agen yang turn-nya berjalan di bawah mutex per-sesi
/// (bukan lock global), bendera pembatalan independen per sesi, dan
/// penanda sibuk untuk `session/set_model`.
struct AcpSession {
    agent: Mutex<Agent>,
    cancel: CancelFlag,
    busy: AtomicBool,
}

type SessionMap = Arc<Mutex<HashMap<String, Arc<AcpSession>>>>;

/// Id metode auth terminal di original (`acp_adapter/auth.py`).
const TERMINAL_SETUP_AUTH_METHOD_ID: &str = "hermes-setup";
/// Id opsi kebijakan edit approval di original (`acp_adapter/modes.py`).
const EDIT_APPROVAL_POLICY_CONFIG_ID: &str = "edit_approval_policy";

/// Server ACP: dispatch request JSON-RPC ke handler per metode.
///
/// `dispatch` mengembalikan notifikasi yang harus dikirim BERURUTAN
/// sebelum respons — ini membuat replay riwayat (load/resume) dan
/// chunk prompt dapat diuji tanpa stdio.
struct AcpServer {
    factory: AgentFactory,
    provider: Arc<dyn LlmProvider>,
    memory: Arc<dyn MemoryStore>,
    sessions: SessionMap,
    counter: AtomicU64,
    /// Salinan config mentah untuk `session/set_config_option`: opsi
    /// bernama disimpan di `acp_config_options` lalu dipersist ke
    /// config.toml via `save_config` (alur config app.rs yang sudah ada).
    /// `config_raw` (bukan `config`) dipakai agar referensi `${VAR}`
    /// tidak terbakar ke disk — paritas `App.config_raw`.
    config_raw: Arc<Mutex<HermesConfig>>,
    hermes_home: PathBuf,
}

/// Hasil dispatch satu request.
struct Dispatch {
    /// Notifikasi `session/update` yang harus dikirim sebelum respons.
    notifications: Vec<Value>,
    response: AcpResponse,
}

/// Respons satu request.
#[derive(Debug)]
enum AcpResponse {
    /// Kirim `result` lalu lanjut melayani.
    Result(Value),
    /// Kirim `error` lalu lanjut melayani.
    Error(i64, String),
    /// Hentikan loop server (shutdown/exit).
    Stop,
}

impl AcpServer {
    fn new(app: &App) -> Self {
        Self {
            factory: AgentFactory::from_app(app),
            provider: app.provider.clone(),
            memory: app.memory.clone(),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            counter: AtomicU64::new(0),
            config_raw: Arc::new(Mutex::new(app.config_raw.clone())),
            hermes_home: app.hermes_home.clone(),
        }
    }

    fn next_session_id(&self) -> String {
        format!("acp-{}", self.counter.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// Dispatch satu request; tidak pernah panic — selalu AcpResponse.
    async fn dispatch(&self, method: &str, params: &Value) -> Dispatch {
        let mut notifications = Vec::new();
        let response = match method {
            "initialize" => AcpResponse::Result(self.initialize()),
            "authenticate" => self.authenticate(params),
            "session/new" => self.session_new(params).await,
            "session/load" => self.session_load(params, &mut notifications).await,
            "session/resume" => self.session_resume(params, &mut notifications).await,
            "session/fork" => self.session_fork(params).await,
            "session/list" => self.session_list(params).await,
            "session/prompt" => self.session_prompt(params, &mut notifications).await,
            "session/cancel" => self.session_cancel(params).await,
            "session/set_model" => self.session_set_model(params).await,
            "session/set_mode" => self.session_set_mode(params).await,
            "session/set_config_option" => self.session_set_config_option(params).await,
            "shutdown" | "exit" => AcpResponse::Stop,
            _ => AcpResponse::Error(-32601, "metode tidak didukung".to_string()),
        };
        Dispatch {
            notifications,
            response,
        }
    }

    /// Paritas `HermesACPAgent.initialize`: laporkan protocolVersion,
    /// kapabilitas agen (termasuk loadSession + kapabilitas sesi
    /// fork/list/resume), metode autentikasi, dan info agen. Metode auth
    /// yang diiklankan di sini adalah satu-satunya yang diakui
    /// `authenticate` (paritas `build_auth_methods` + komentar
    /// "Only acknowledge the method_id advertised in initialize()").
    fn initialize(&self) -> Value {
        json!({
            "protocolVersion": 1,
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": false, "audio": false, "embeddedContext": true },
                "sessionCapabilities": {
                    "fork": {},
                    "list": {},
                    "resume": {},
                },
                "mcpCapabilities": { "http": true, "sse": false }
            },
            "authMethods": self.auth_methods(),
            "agentInfo": { "name": "hermes-rs", "version": env!("CARGO_PKG_VERSION") },
        })
    }

    /// Paritas `build_auth_methods()` di `acp_adapter/auth.py`: metode
    /// provider runtime + metode terminal setup. Kredensial selalu
    /// "ter-resolve" saat server ACP berjalan (provider dibangun saat
    /// startup, analog `detect_provider()` yang sukses), jadi kedua
    /// metode selalu diiklankan. Penyimpangan: argumen terminal adalah
    /// `["setup"]` — subcommand `hermes setup` — bukan `["--setup"]`
    /// seperti original.
    fn auth_methods(&self) -> Value {
        let mut methods = Vec::new();
        let provider = self.provider.name().trim().to_lowercase();
        if !provider.is_empty() {
            methods.push(json!({
                "id": provider,
                "name": format!("{provider} runtime credentials"),
                "description": format!(
                    "Authenticate hermes-rs using the currently configured {provider} runtime credentials."
                ),
            }));
        }
        methods.push(json!({
            "id": TERMINAL_SETUP_AUTH_METHOD_ID,
            "name": "Configure Hermes provider",
            "type": "terminal",
            "args": ["setup"],
            "description": "Open Hermes' interactive model/provider setup in a terminal. \
                           Use this when Hermes has not been configured on this machine yet.",
        }));
        Value::Array(methods)
    }

    /// Paritas `HermesACPAgent.authenticate` (`acp_adapter/server.py`):
    /// hanya akui `methodId` yang diiklankan di `initialize()` — id
    /// provider runtime atau `hermes-setup`. Kredensial sudah ada di
    /// runtime (provider dibangun saat startup), jadi tidak ada materi
    /// auth yang perlu dikumpulkan/direkam; kecocokan → result kosong.
    /// Tidak cocok / bukan string → result null (paritas `return None`
    /// di original, BUKAN error).
    fn authenticate(&self, params: &Value) -> AcpResponse {
        let normalized = param_str(params, "methodId").trim().to_lowercase();
        let provider_id = self.provider.name().trim().to_lowercase();
        let acknowledged = normalized == TERMINAL_SETUP_AUTH_METHOD_ID
            || (!provider_id.is_empty() && normalized == provider_id);
        if acknowledged {
            tracing::info!("acp: authenticate diakui untuk methodId '{normalized}'");
            AcpResponse::Result(json!({}))
        } else {
            AcpResponse::Result(Value::Null)
        }
    }

    /// Pabrik per-request: teruskan cwd dari IDE bila diberikan
    /// (paritas argumen `cwd` di new/load/resume/fork original).
    fn factory_for(&self, params: &Value) -> AgentFactory {
        match param_str(params, "cwd") {
            "" => self.factory.clone(),
            cwd => self.factory.with_working_dir(cwd),
        }
    }

    /// Blok `models` untuk respons sesi (paritas `_build_model_state`):
    /// satu entri model yang diketahui + model aktif sesi ini.
    /// `modes` SENGAJA tidak diiklankan: `session/set_mode` tidak
    /// didukung port ini (lihat handler-nya).
    fn model_state(&self, current_model: &str) -> Value {
        json!({
            "models": [{ "modelId": self.provider.default_model(), "name": self.provider.name() }],
            "currentModelId": current_model,
        })
    }

    /// Bangun agen untuk sebuah `Session` lalu daftarkan di map dengan
    /// CancelFlag independen. Mengembalikan handle yang sudah terdaftar.
    async fn install(&self, session: Session, params: &Value) -> Arc<AcpSession> {
        let session_id = session.id.clone();
        let factory = self.factory_for(params);
        let cancel = CancelFlag::new();
        let agent = factory.with_cancel(cancel.clone()).build_quiet(session);
        let handle = Arc::new(AcpSession {
            agent: Mutex::new(agent),
            cancel,
            busy: AtomicBool::new(false),
        });
        // Sisipkan secara atomik: bila dua request balapan untuk id yang
        // sama, hanya satu handle yang menang (paritas catatan original
        // "overlapping session/load for one id must share a single agent build").
        let mut guard = self.sessions.lock().await;
        if let Some(existing) = guard.get(&session_id) {
            return existing.clone();
        }
        guard.insert(session_id, handle.clone());
        handle
    }

    /// Simpan snapshot sesi milik handle ke session store (best-effort;
    /// kegagalan hanya di-log, paritas `Agent::persist` yang fail-open).
    async fn persist(&self, handle: &AcpSession) {
        let snapshot = { handle.agent.lock().await.session().clone() };
        if let Err(e) = self.memory.save_session(&snapshot).await {
            tracing::warn!("acp: gagal menyimpan sesi {}: {e}", snapshot.id);
        }
    }

    /// Ambil sesi dari map; bila belum ada di memori, muat dari session
    /// store lalu bangun agennya (paritas `SessionManager.get_session`
    /// yang "checks DB too"). `Ok(None)` = tidak dikenal di mana pun.
    async fn get_or_restore(
        &self,
        session_id: &str,
        params: &Value,
    ) -> Result<Option<Arc<AcpSession>>, AcpResponse> {
        {
            let guard = self.sessions.lock().await;
            if let Some(handle) = guard.get(session_id) {
                return Ok(Some(handle.clone()));
            }
        }
        let stored = self
            .memory
            .load_session(session_id)
            .await
            .map_err(|e| AcpResponse::Error(-32003, format!("gagal memuat sesi: {e}")))?;
        match stored {
            Some(session) => Ok(Some(self.install(session, params).await)),
            None => Ok(None),
        }
    }

    fn unknown_session(session_id: &str) -> AcpResponse {
        AcpResponse::Error(-32000, format!("sesi tidak dikenal: {session_id}"))
    }

    async fn session_new(&self, params: &Value) -> AcpResponse {
        let mut session = Session::new("acp");
        session.id = self.next_session_id();
        let session_id = session.id.clone();
        let handle = self.install(session, params).await;
        self.persist(&handle).await;
        let model = { handle.agent.lock().await.model() };
        AcpResponse::Result(json!({
            "sessionId": session_id,
            "models": self.model_state(&model),
        }))
    }

    /// Paritas `load_session`: sesi harus sudah ada (di memori atau di
    /// store); riwayat di-replay via `session/update` SEBELUM merespons.
    /// Penyimpangan: id tak dikenal → error -32000 (original mengembalikan
    /// result null, yang tidak berguna bagi klien).
    async fn session_load(&self, params: &Value, out: &mut Vec<Value>) -> AcpResponse {
        let session_id = param_str(params, "sessionId");
        if session_id.is_empty() {
            return AcpResponse::Error(-32602, "sessionId wajib diisi".to_string());
        }
        let handle = match self.get_or_restore(session_id, params).await {
            Ok(Some(h)) => h,
            Ok(None) => return Self::unknown_session(session_id),
            Err(e) => return e,
        };
        let (model, sid) = {
            let agent = handle.agent.lock().await;
            replay_history(agent.session(), out);
            (agent.model(), agent.session().id.clone())
        };
        AcpResponse::Result(json!({ "sessionId": sid, "models": self.model_state(&model) }))
    }

    /// Paritas `resume_session`: seperti load, tetapi id tak dikenal →
    /// sesi BARU dengan id baru (bukan error).
    async fn session_resume(&self, params: &Value, out: &mut Vec<Value>) -> AcpResponse {
        let session_id = param_str(params, "sessionId");
        if session_id.is_empty() {
            return AcpResponse::Error(-32602, "sessionId wajib diisi".to_string());
        }
        let handle = match self.get_or_restore(session_id, params).await {
            Ok(Some(h)) => h,
            Ok(None) => {
                let mut session = Session::new("acp");
                session.id = self.next_session_id();
                let handle = self.install(session, params).await;
                self.persist(&handle).await;
                handle
            }
            Err(e) => return e,
        };
        let (model, sid) = {
            let agent = handle.agent.lock().await;
            replay_history(agent.session(), out);
            (agent.model(), agent.session().id.clone())
        };
        AcpResponse::Result(json!({ "sessionId": sid, "models": self.model_state(&model) }))
    }

    /// Paritas `fork_session`: deep-copy riwayat ke sesi baru ber-id baru.
    /// Penyimpangan: sumber tak dikenal → error -32000 (original
    /// mengembalikan session_id kosong).
    async fn session_fork(&self, params: &Value) -> AcpResponse {
        let source_id = param_str(params, "sessionId");
        if source_id.is_empty() {
            return AcpResponse::Error(-32602, "sessionId wajib diisi".to_string());
        }
        let source = match self.get_or_restore(source_id, params).await {
            Ok(Some(h)) => h,
            Ok(None) => return Self::unknown_session(source_id),
            Err(e) => return e,
        };
        let (messages, title, profile, system_prompt) = {
            let agent = source.agent.lock().await;
            let s = agent.session();
            (
                s.messages.clone(),
                s.title.clone(),
                s.profile.clone(),
                s.system_prompt.clone(),
            )
        };
        let mut session = Session::new(profile);
        session.id = self.next_session_id();
        session.messages = messages;
        session.title = title;
        session.system_prompt = system_prompt;
        let handle = self.install(session, params).await;
        self.persist(&handle).await;
        let (model, sid) = {
            let agent = handle.agent.lock().await;
            (agent.model(), agent.session().id.clone())
        };
        AcpResponse::Result(json!({ "sessionId": sid, "models": self.model_state(&model) }))
    }

    /// Paritas `list_sessions`: halaman 50 sesi, `cursor` = session_id
    /// terakhir halaman sebelumnya (cursor tak dikenal → halaman kosong,
    /// bukan daftar penuh). Penyimpangan: Session Rust tidak menyimpan
    /// cwd, jadi filter `cwd` diabaikan dan respons membawa `profile`,
    /// bukan `cwd`.
    async fn session_list(&self, params: &Value) -> AcpResponse {
        const PAGE_SIZE: usize = 50;
        let cursor = param_str(params, "cursor");
        let mut sessions = match self.memory.list_sessions("acp", PAGE_SIZE + 1).await {
            Ok(s) => s,
            Err(e) => {
                return AcpResponse::Error(-32003, format!("gagal membaca daftar sesi: {e}"));
            }
        };
        if !cursor.is_empty() {
            match sessions.iter().position(|s| s.id == cursor) {
                Some(i) => {
                    sessions.drain(..=i);
                }
                None => sessions.clear(),
            }
        }
        let has_more = sessions.len() > PAGE_SIZE;
        sessions.truncate(PAGE_SIZE);
        let infos: Vec<Value> = sessions
            .iter()
            .map(|s| {
                json!({
                    "sessionId": s.id,
                    "title": s.title,
                    "updatedAt": s.updated_at.to_rfc3339(),
                    "profile": s.profile,
                })
            })
            .collect();
        let next_cursor = if has_more {
            infos
                .last()
                .and_then(|v| v.get("sessionId"))
                .cloned()
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        AcpResponse::Result(json!({ "sessions": infos, "nextCursor": next_cursor }))
    }

    async fn session_prompt(&self, params: &Value, out: &mut Vec<Value>) -> AcpResponse {
        let session_id = param_str(params, "sessionId").to_string();
        let handle = match self.get_or_restore(&session_id, params).await {
            Ok(Some(h)) => h,
            Ok(None) => return Self::unknown_session(&session_id),
            Err(e) => return e,
        };
        // Hanya satu turn per sesi dalam satu waktu. Original mengantre
        // prompt; antrean belum diimplementasikan di port ini → tolak
        // dengan jelas alih-alih menumpuk turn.
        if handle.busy.swap(true, Ordering::SeqCst) {
            return AcpResponse::Error(
                -32603,
                "sesi sedang sibuk; tunggu turn yang berjalan selesai".to_string(),
            );
        }
        // Paritas original (`cancel_event.clear()` di awal prompt):
        // pembatalan turn sebelumnya tidak membekukan sesi ini.
        handle.cancel.reset();
        let text = extract_prompt_text(params);
        out.push(session_update_value(
            &session_id,
            "agent_thought_chunk",
            "Memproses permintaan…",
        ));
        let result = {
            let mut agent = handle.agent.lock().await;
            agent.run_turn(&text).await
        };
        handle.busy.store(false, Ordering::SeqCst);
        match result {
            Ok(reply) => {
                out.push(session_update_value(
                    &session_id,
                    "agent_message_chunk",
                    &reply,
                ));
                AcpResponse::Result(json!({ "stopReason": "end_turn" }))
            }
            // Paritas `_finish_turn`: turn yang dibatalkan dilaporkan
            // sebagai "cancelled", bukan error.
            Err(e) if matches!(e, HermesError::Interrupted) && handle.cancel.is_cancelled() => {
                AcpResponse::Result(json!({ "stopReason": "cancelled" }))
            }
            Err(e) => AcpResponse::Error(-32001, format!("agent error: {e}")),
        }
    }

    /// Pembatalan NYATA (paritas `HermesACPAgent.cancel`): menyalakan
    /// CancelFlag milik sesi tersebut. Loop agen di hermes-core memeriksa
    /// flag ini tiap iterasi dan tiap chunk stream, sehingga turn yang
    /// sedang berjalan abort — bukan sekadar diakui. Idempoten: sesi tak
    /// dikenal (tak ada turn berjalan) tetap dijawab null.
    async fn session_cancel(&self, params: &Value) -> AcpResponse {
        let session_id = param_str(params, "sessionId");
        let handle = { self.sessions.lock().await.get(session_id).cloned() };
        if let Some(handle) = handle {
            handle.cancel.cancel();
        }
        AcpResponse::Result(Value::Null)
    }

    /// Paritas `set_session_model`: ganti model sesi; ditolak dengan
    /// -32603 bila turn sedang berjalan (pesan error disalin dari original).
    async fn session_set_model(&self, params: &Value) -> AcpResponse {
        let session_id = param_str(params, "sessionId");
        let model_id = param_str(params, "modelId");
        if model_id.is_empty() {
            return AcpResponse::Error(-32602, "modelId wajib diisi".to_string());
        }
        let handle = match self.get_or_restore(session_id, params).await {
            Ok(Some(h)) => h,
            Ok(None) => return Self::unknown_session(session_id),
            Err(e) => return e,
        };
        // Tolak SEGERA bila turn sedang berjalan (paritas original) — jangan
        // antre di belakang lock agen. Cek cepat dulu, lalu try_lock agar
        // tidak menunggu turn yang sedang berjalan selesai.
        if handle.busy.load(Ordering::SeqCst) {
            return Self::busy_error();
        }
        {
            let mut agent = match handle.agent.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Self::busy_error(),
            };
            // Cek ulang di bawah guard: prompt yang mulai tepat setelah cek
            // cepat tetap tertangkap sebelum model diganti.
            if handle.busy.load(Ordering::SeqCst) {
                return Self::busy_error();
            }
            agent.set_model(model_id);
        }
        self.persist(&handle).await;
        AcpResponse::Result(json!({}))
    }

    fn busy_error() -> AcpResponse {
        AcpResponse::Error(
            -32603,
            "session is busy; switch models while the session is idle".to_string(),
        )
    }

    /// Keputusan desain: hermes-core TIDAK memiliki konsep mode per-sesi
    /// ala original (`default`/`accept_edits`/`dont_ask` = kebijakan
    /// persetujuan edit per sesi). Kebijakan persetujuan di port ini
    /// dikendalikan server-wide oleh approval handler yang dipakai
    /// membangun semua agen, sehingga tidak ada mode per sesi yang bisa
    /// diganti — kembalikan error yang jelas, bukan mode palsu.
    async fn session_set_mode(&self, params: &Value) -> AcpResponse {
        let session_id = param_str(params, "sessionId");
        match self.get_or_restore(session_id, params).await {
            Ok(Some(_)) => {}
            Ok(None) => return Self::unknown_session(session_id),
            Err(e) => return e,
        }
        AcpResponse::Error(
            -32002,
            "session/set_mode tidak didukung: hermes-rs tidak memiliki mode per-sesi; \
             kebijakan persetujuan edit dikendalikan server-wide oleh approval handler"
                .to_string(),
        )
    }

    /// Paritas `HermesACPAgent.set_config_option`
    /// (`acp_adapter/server.py`): terima pembaruan opsi konfigurasi
    /// bernama (`configId`, `sessionId`, `value`). Sesi tak dikenal →
    /// result null (paritas `return None`, bukan error).
    ///
    /// Penyimpangan penyimpanan: original menyimpan per-sesi di
    /// `state.config_options` lalu `save_session`; port ini tidak punya
    /// permukaan config per-sesi, jadi opsi disimpan server-wide di
    /// `HermesConfig::acp_config_options` dan dipersist ke config.toml
    /// melalui `save_config` (alur config app.rs yang sudah ada).
    /// `edit_approval_policy` DITOLAK dengan -32002: original
    /// memetakannya ke mode per-sesi, yang tidak ada di port ini
    /// (konsisten dengan `session/set_mode` — tolak dengan jelas
    /// alih-alih merekam opsi yang tak berpengaruh).
    ///
    /// Respons: `{"configOptions": []}` seperti original
    /// (`SetSessionConfigOptionResponse(config_options=[])`).
    async fn session_set_config_option(&self, params: &Value) -> AcpResponse {
        let session_id = param_str(params, "sessionId");
        match self.get_or_restore(session_id, params).await {
            Ok(Some(_)) => {}
            Ok(None) => return AcpResponse::Result(Value::Null),
            Err(e) => return e,
        }
        let config_id = param_str(params, "configId");
        if config_id.is_empty() {
            return AcpResponse::Error(-32602, "configId wajib diisi".to_string());
        }
        if config_id == EDIT_APPROVAL_POLICY_CONFIG_ID {
            return AcpResponse::Error(
                -32002,
                "set_config_option untuk edit_approval_policy tidak didukung: \
                 hermes-rs tidak memiliki mode per-sesi; kebijakan persetujuan \
                 edit dikendalikan server-wide oleh approval handler"
                    .to_string(),
            );
        }
        let value = match params.get("value").and_then(|v| v.as_str()) {
            Some(v) => v.to_string(),
            None => {
                return AcpResponse::Error(-32602, "value wajib diisi (string)".to_string());
            }
        };
        let mut guard = self.config_raw.lock().await;
        guard
            .acp_config_options
            .insert(config_id.to_string(), value);
        if let Err(e) = save_config(&guard, &self.hermes_home) {
            return AcpResponse::Error(-32003, format!("gagal menyimpan config: {e}"));
        }
        tracing::info!("acp: config option '{config_id}' diperbarui via set_config_option");
        AcpResponse::Result(json!({ "configOptions": [] }))
    }
}

/// Jalankan server ACP di stdio hingga `shutdown`/`exit`/EOF.
pub async fn serve_acp(app: &App) -> anyhow::Result<()> {
    let server = AcpServer::new(app);
    let stdout = Arc::new(Mutex::new(tokio::io::stdout()));

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);

    loop {
        let msg = read_message(&mut reader).await?;
        let Some(msg) = msg else {
            break; // EOF
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // Notifikasi (tanpa id) → abaikan.
        if id.is_null() && method != "shutdown" && method != "exit" {
            continue;
        }

        let dispatch = server.dispatch(method, &params).await;
        // Notifikasi (mis. replay riwayat) SELALU mendahului respons —
        // spek ACP mewajibkan ini untuk load/resume.
        for notification in &dispatch.notifications {
            write_message(&stdout, &notification.to_string()).await?;
        }
        match dispatch.response {
            AcpResponse::Result(result) => respond(&stdout, &id, result).await?,
            AcpResponse::Error(code, message) => {
                respond_error(&stdout, &id, code, &message).await?
            }
            AcpResponse::Stop => {
                if !id.is_null() {
                    respond(&stdout, &id, Value::Null).await?;
                }
                break;
            }
        }
    }
    Ok(())
}

/// Ambil string param; "" bila absen/bukan string.
fn param_str<'a>(params: &'a Value, key: &str) -> &'a str {
    params.get(key).and_then(|v| v.as_str()).unwrap_or("")
}

/// Ambil teks dari `params.prompt: [{type: "text", text}]`.
fn extract_prompt_text(params: &Value) -> String {
    params
        .get("prompt")
        .and_then(|p| p.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Satu notifikasi `session/update` (belum dikirim; dikumpulkan agar
/// urutan terhadap respons terjamin dan dapat diuji).
fn session_update_value(session_id: &str, update_kind: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": update_kind,
                "content": { "type": "text", "text": text },
            }
        }
    })
}

/// Replay riwayat sesi sebagai notifikasi `session/update` — user dan
/// asisten menjadi bubble teks; pesan system/tool dilewati. Paritas
/// `_replay_session_history` di original.
fn replay_history(session: &Session, out: &mut Vec<Value>) {
    for m in &session.messages {
        let kind = match m.role {
            Role::User => "user_message_chunk",
            Role::Assistant => "agent_message_chunk",
            _ => continue,
        };
        let text = m.text();
        if text.trim().is_empty() {
            continue;
        }
        out.push(session_update_value(&session.id, kind, &text));
    }
}

/// Baca satu pesan JSON-RPC berbingkai Content-Length. `None` = EOF.
async fn read_message(reader: &mut BufReader<tokio::io::Stdin>) -> anyhow::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok();
        }
    }
    let len = content_length.ok_or_else(|| anyhow::anyhow!("pesan tanpa Content-Length"))?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let v: Value = serde_json::from_slice(&buf)?;
    Ok(Some(v))
}

async fn write_message(stdout: &Arc<Mutex<tokio::io::Stdout>>, body: &str) -> anyhow::Result<()> {
    let mut out = stdout.lock().await;
    out.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    out.write_all(body.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

async fn respond(
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    id: &Value,
    result: Value,
) -> anyhow::Result<()> {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    write_message(stdout, &msg.to_string()).await
}

async fn respond_error(
    stdout: &Arc<Mutex<tokio::io::Stdout>>,
    id: &Value,
    code: i64,
    message: &str,
) -> anyhow::Result<()> {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } });
    write_message(stdout, &msg.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use hermes_core::{
        AllowAllApproval, ChatRequest, ContentBlock, HermesConfig, Message, ProfileConfig, Skill,
        StreamEvent, ToolRegistry, Usage,
    };
    use hermes_cron::CronStore;
    use hermes_gateway::PlatformRegistry;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    /// Provider mock: streaming N chunk teks dengan jeda antar chunk.
    struct ChunkMockProvider {
        chunks: usize,
        delay_ms: u64,
    }

    #[async_trait]
    impl LlmProvider for ChunkMockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(
            &self,
            _req: ChatRequest,
        ) -> hermes_core::Result<mpsc::Receiver<StreamEvent>> {
            let (tx, rx) = mpsc::channel(32);
            let chunks = self.chunks;
            let delay = self.delay_ms;
            tokio::spawn(async move {
                for i in 0..chunks {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    if tx
                        .send(StreamEvent::TextDelta(format!("chunk{i} ")))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = tx.send(StreamEvent::Done(Usage::default())).await;
            });
            Ok(rx)
        }
    }

    /// Memory store in-memory untuk uji.
    #[derive(Default)]
    struct MockMemory {
        sessions: Mutex<HashMap<String, Session>>,
    }

    #[async_trait]
    impl MemoryStore for MockMemory {
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
        async fn save_session(&self, s: &Session) -> hermes_core::Result<()> {
            self.sessions.lock().await.insert(s.id.clone(), s.clone());
            Ok(())
        }
        async fn load_session(&self, id: &str) -> hermes_core::Result<Option<Session>> {
            Ok(self.sessions.lock().await.get(id).cloned())
        }
        async fn list_sessions(
            &self,
            profile: &str,
            limit: usize,
        ) -> hermes_core::Result<Vec<Session>> {
            let mut v: Vec<Session> = self
                .sessions
                .lock()
                .await
                .values()
                .filter(|s| s.profile == profile)
                .cloned()
                .collect();
            v.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
            v.truncate(limit);
            Ok(v)
        }
        async fn delete_session(&self, id: &str) -> hermes_core::Result<()> {
            self.sessions.lock().await.remove(id);
            Ok(())
        }
        async fn search_sessions(&self, _q: &str, _l: usize) -> hermes_core::Result<Vec<Session>> {
            Ok(vec![])
        }
    }

    #[derive(Default)]
    struct MockSkills;

    #[async_trait]
    impl hermes_core::SkillStore for MockSkills {
        async fn list(&self) -> hermes_core::Result<Vec<Skill>> {
            Ok(vec![])
        }
        async fn get(&self, _n: &str) -> hermes_core::Result<Option<Skill>> {
            Ok(None)
        }
        async fn create(&self, _n: &str, _d: &str, _b: &str) -> hermes_core::Result<Skill> {
            Err(hermes_core::HermesError::Other(
                "tidak didukung di uji".to_string(),
            ))
        }
        async fn update(&self, _n: &str, _b: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn delete(&self, _n: &str) -> hermes_core::Result<()> {
            Ok(())
        }
        async fn find_relevant(&self, _t: &str, _l: usize) -> hermes_core::Result<Vec<Skill>> {
            Ok(vec![])
        }
    }

    fn test_app_with_memory(provider: Arc<dyn LlmProvider>, memory: Arc<dyn MemoryStore>) -> App {
        let dir = std::env::temp_dir().join(format!("hermes-acp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = ProfileConfig {
            model: String::new(), // pakai default provider ("mock-model")
            ..ProfileConfig::default()
        };
        App {
            config: HermesConfig::default(),
            config_raw: HermesConfig::default(),
            profile_name: "test".to_string(),
            profile,
            hermes_home: dir.clone(),
            provider,
            tools: Arc::new(ToolRegistry::new()),
            memory,
            skills: Arc::new(MockSkills),
            platforms: Arc::new(PlatformRegistry::new()),
            cron_store: Arc::new(CronStore::open(dir.join("cron.db")).unwrap()),
            approval: Arc::new(AllowAllApproval),
            cancel: CancelFlag::new(),
        }
    }

    fn test_app(provider: Arc<dyn LlmProvider>) -> App {
        test_app_with_memory(provider, Arc::new(MockMemory::default()))
    }

    fn fast_server() -> AcpServer {
        let provider: Arc<dyn LlmProvider> = Arc::new(ChunkMockProvider {
            chunks: 3,
            delay_ms: 5,
        });
        AcpServer::new(&test_app(provider))
    }

    /// Dispatch dan kembalikan (notifications, result) — panic bila error.
    async fn call_ok(server: &AcpServer, method: &str, params: Value) -> (Vec<Value>, Value) {
        let d = server.dispatch(method, &params).await;
        match d.response {
            AcpResponse::Result(v) => (d.notifications, v),
            AcpResponse::Error(code, msg) => panic!("{method} error {code}: {msg}"),
            AcpResponse::Stop => panic!("{method} berhenti"),
        }
    }

    /// Dispatch dan kembalikan (code, message) — panic bila bukan error.
    async fn call_err(server: &AcpServer, method: &str, params: Value) -> (i64, String) {
        let d = server.dispatch(method, &params).await;
        match d.response {
            AcpResponse::Error(code, msg) => (code, msg),
            AcpResponse::Result(v) => panic!("{method} sukses tak terduga: {v}"),
            AcpResponse::Stop => panic!("{method} berhenti"),
        }
    }

    #[tokio::test]
    async fn initialize_advertises_capabilities() {
        let server = fast_server();
        let (_, result) = call_ok(&server, "initialize", json!({})).await;
        assert_eq!(result["protocolVersion"], 1);
        let caps = &result["agentCapabilities"];
        assert!(
            caps.is_object(),
            "initialize harus membawa agentCapabilities"
        );
        assert_eq!(caps["loadSession"], true);
        assert!(caps["sessionCapabilities"]["fork"].is_object());
        assert!(caps["sessionCapabilities"]["list"].is_object());
        assert!(caps["sessionCapabilities"]["resume"].is_object());
        assert!(caps["promptCapabilities"].is_object());
        assert_eq!(result["agentInfo"]["name"], "hermes-rs");
    }

    #[tokio::test]
    async fn session_new_assigns_ids() {
        let server = fast_server();
        let (_, r1) = call_ok(&server, "session/new", json!({ "cwd": "/tmp" })).await;
        let (_, r2) = call_ok(&server, "session/new", json!({})).await;
        assert_eq!(r1["sessionId"], "acp-1");
        assert_eq!(r2["sessionId"], "acp-2");
        assert_eq!(r1["models"]["currentModelId"], "mock-model");
    }

    #[tokio::test]
    async fn session_load_unknown_errors() {
        let server = fast_server();
        let (code, _) = call_err(&server, "session/load", json!({ "sessionId": "acp-99" })).await;
        assert_eq!(code, -32000);
    }

    #[tokio::test]
    async fn session_load_replays_history_before_responding() {
        let server = fast_server();
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        // Satu turn cepat agar ada riwayat user+asisten di store.
        let (prompt_notes, _) = call_ok(
            &server,
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "halo dunia" }] }),
        )
        .await;
        assert!(prompt_notes
            .iter()
            .any(|n| n["params"]["update"]["sessionUpdate"] == "agent_message_chunk"));

        // Server BARU dengan memory store yang sama (simulasi restart proses)
        // → load dari store, bukan dari map.
        let memory: Arc<dyn MemoryStore> = server.memory.clone();
        let server2 = AcpServer::new(&test_app_with_memory(
            Arc::new(ChunkMockProvider {
                chunks: 1,
                delay_ms: 1,
            }),
            memory,
        ));
        let (notes, result) = call_ok(&server2, "session/load", json!({ "sessionId": sid })).await;
        assert_eq!(result["sessionId"], sid);
        let kinds: Vec<&str> = notes
            .iter()
            .map(|n| n["params"]["update"]["sessionUpdate"].as_str().unwrap())
            .collect();
        assert!(
            kinds.contains(&"user_message_chunk"),
            "riwayat user harus di-replay, dapat: {kinds:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("halo dunia")),
            "teks prompt asli harus muncul di replay"
        );
    }

    #[tokio::test]
    async fn session_resume_unknown_creates_new() {
        let server = fast_server();
        let (_, result) = call_ok(
            &server,
            "session/resume",
            json!({ "sessionId": "tidak-ada" }),
        )
        .await;
        let sid = result["sessionId"].as_str().unwrap();
        assert_ne!(sid, "tidak-ada");
        assert!(sid.starts_with("acp-"));
    }

    #[tokio::test]
    async fn session_fork_copies_history() {
        let server = fast_server();
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        call_ok(
            &server,
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "topik fork" }] }),
        )
        .await;
        let (_, fork_r) = call_ok(&server, "session/fork", json!({ "sessionId": sid })).await;
        let fork_id = fork_r["sessionId"].as_str().unwrap().to_string();
        assert_ne!(fork_id, sid);
        // Fork membawa salinan riwayat: load fork → replay memuat prompt asli.
        let (notes, _) = call_ok(&server, "session/load", json!({ "sessionId": fork_id })).await;
        assert!(notes
            .iter()
            .any(|n| n["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap_or("")
                .contains("topik fork")));
        let (code, _) = call_err(&server, "session/fork", json!({ "sessionId": "acp-404" })).await;
        assert_eq!(code, -32000);
    }

    #[tokio::test]
    async fn session_list_paginates() {
        let server = fast_server();
        for _ in 0..3 {
            call_ok(&server, "session/new", json!({})).await;
        }
        let (_, result) = call_ok(&server, "session/list", json!({})).await;
        let sessions = result["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 3);
        assert!(result["nextCursor"].is_null());
        assert!(sessions
            .iter()
            .all(|s| s["sessionId"].as_str().unwrap().starts_with("acp-")));
        // Cursor tak dikenal → halaman kosong (paritas original).
        let (_, empty) = call_ok(&server, "session/list", json!({ "cursor": "acp-zzz" })).await;
        assert!(empty["sessions"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_prompt_returns_end_turn() {
        let server = fast_server();
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        let (notes, result) = call_ok(
            &server,
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hai" }] }),
        )
        .await;
        assert_eq!(result["stopReason"], "end_turn");
        assert!(notes
            .iter()
            .any(|n| n["params"]["update"]["sessionUpdate"] == "agent_message_chunk"));
        // prompt ke sesi tak dikenal → error
        let (code, _) = call_err(
            &server,
            "session/prompt",
            json!({ "sessionId": "acp-404", "prompt": [] }),
        )
        .await;
        assert_eq!(code, -32000);
    }

    #[tokio::test]
    async fn session_set_model_switches_and_persists() {
        let server = fast_server();
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        let (_, result) = call_ok(
            &server,
            "session/set_model",
            json!({ "sessionId": sid, "modelId": "model-baru-1" }),
        )
        .await;
        assert!(result.as_object().unwrap().is_empty());
        // Terlihat di load berikutnya (currentModelId) → berarti tersimpan.
        let (_, loaded) = call_ok(&server, "session/load", json!({ "sessionId": sid })).await;
        assert_eq!(loaded["models"]["currentModelId"], "model-baru-1");
        // modelId kosong → invalid params
        let (code, _) = call_err(
            &server,
            "session/set_model",
            json!({ "sessionId": sid, "modelId": "" }),
        )
        .await;
        assert_eq!(code, -32602);
    }

    #[tokio::test]
    async fn session_set_model_rejected_while_busy() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ChunkMockProvider {
            chunks: 40,
            delay_ms: 100,
        });
        let server = Arc::new(AcpServer::new(&test_app(provider)));
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();

        let server2 = server.clone();
        let sid2 = sid.clone();
        let prompt_task = tokio::spawn(async move {
            server2
                .dispatch(
                    "session/prompt",
                    &json!({ "sessionId": sid2, "prompt": [{ "type": "text", "text": "lambat" }] }),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        // Ganti model saat turn berjalan → ditolak seperti original.
        let d = server
            .dispatch(
                "session/set_model",
                &json!({ "sessionId": sid, "modelId": "x" }),
            )
            .await;
        match d.response {
            AcpResponse::Error(code, _) => assert_eq!(code, -32603),
            other => panic!("set_model saat sibuk harus ditolak, dapat: {other:?}"),
        }
        // Bersihkan: batalkan turn yang berjalan.
        server
            .dispatch("session/cancel", &json!({ "sessionId": sid }))
            .await;
        let done = prompt_task.await.unwrap();
        match done.response {
            AcpResponse::Result(v) => assert_eq!(v["stopReason"], "cancelled"),
            other => panic!("turn harus cancelled, dapat: {other:?}"),
        }
    }

    #[tokio::test]
    async fn session_set_mode_is_unsupported() {
        let server = fast_server();
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        let (code, msg) = call_err(
            &server,
            "session/set_mode",
            json!({ "sessionId": sid, "modeId": "accept_edits" }),
        )
        .await;
        assert_eq!(code, -32002);
        assert!(msg.contains("tidak didukung") || msg.contains("tidak memiliki"));
    }

    #[tokio::test]
    async fn initialize_advertises_auth_methods() {
        let server = fast_server();
        let (_, result) = call_ok(&server, "initialize", json!({})).await;
        let methods = result["authMethods"].as_array().unwrap();
        let ids: Vec<&str> = methods.iter().map(|m| m["id"].as_str().unwrap()).collect();
        // Paritas build_auth_methods(): metode provider runtime + terminal setup.
        assert!(
            ids.contains(&"mock"),
            "id provider harus diiklankan, dapat: {ids:?}"
        );
        assert!(ids.contains(&"hermes-setup"));
        let terminal = methods.iter().find(|m| m["id"] == "hermes-setup").unwrap();
        assert_eq!(terminal["type"], "terminal");
        // Penyimpangan: subcommand `hermes setup`, bukan `--setup`.
        assert_eq!(terminal["args"], json!(["setup"]));
    }

    #[tokio::test]
    async fn authenticate_acknowledges_advertised_methods() {
        let server = fast_server();
        // Id provider yang diiklankan → result kosong (paritas AuthenticateResponse()).
        let (_, ok) = call_ok(&server, "authenticate", json!({ "methodId": "mock" })).await;
        assert!(ok.as_object().unwrap().is_empty());
        // Normalisasi seperti original: strip + lowercase.
        let (_, ok2) = call_ok(&server, "authenticate", json!({ "methodId": "  MOCK " })).await;
        assert!(ok2.as_object().unwrap().is_empty());
        let (_, ok3) = call_ok(
            &server,
            "authenticate",
            json!({ "methodId": "hermes-setup" }),
        )
        .await;
        assert!(ok3.as_object().unwrap().is_empty());
        // Tak dikenal / bukan string → result null (paritas `return None`, BUKAN error).
        let (_, null1) =
            call_ok(&server, "authenticate", json!({ "methodId": "unknown-id" })).await;
        assert!(null1.is_null());
        let (_, null2) = call_ok(&server, "authenticate", json!({})).await;
        assert!(null2.is_null());
        let (_, null3) = call_ok(&server, "authenticate", json!({ "methodId": 42 })).await;
        assert!(null3.is_null());
    }

    /// Server dari App yang hermes_home-nya dipegang agar persistensi
    /// config.toml bisa diverifikasi.
    fn server_with_home(preset: &[(&str, &str)]) -> (AcpServer, std::path::PathBuf) {
        let provider: Arc<dyn LlmProvider> = Arc::new(ChunkMockProvider {
            chunks: 1,
            delay_ms: 1,
        });
        let mut app = test_app(provider);
        for (k, v) in preset {
            app.config_raw
                .acp_config_options
                .insert(k.to_string(), v.to_string());
        }
        let home = app.hermes_home.clone();
        (AcpServer::new(&app), home)
    }

    fn read_acp_options(home: &std::path::Path) -> HashMap<String, String> {
        let text = std::fs::read_to_string(home.join("config.toml"))
            .expect("config.toml harus ditulis oleh set_config_option");
        let cfg: HermesConfig = toml::from_str(&text).expect("config.toml harus valid");
        cfg.acp_config_options
    }

    #[tokio::test]
    async fn set_config_option_stores_and_persists() {
        // Opsi preset di config_raw harus dipertahankan (merge, bukan timpa).
        let (server, home) = server_with_home(&[("preset", "1")]);
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();

        let (_, result) = call_ok(
            &server,
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "acp_test_option", "value": "42" }),
        )
        .await;
        // Paritas SetSessionConfigOptionResponse(config_options=[]).
        assert_eq!(result, json!({ "configOptions": [] }));

        let opts = read_acp_options(&home);
        assert_eq!(opts.get("acp_test_option").map(|s| s.as_str()), Some("42"));
        assert_eq!(
            opts.get("preset").map(|s| s.as_str()),
            Some("1"),
            "opsi yang sudah ada di config tidak boleh hilang"
        );

        // Set kedua: opsi pertama tetap ada.
        call_ok(
            &server,
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "opsi_kedua", "value": "x" }),
        )
        .await;
        let opts = read_acp_options(&home);
        assert_eq!(opts.len(), 3);
    }

    #[tokio::test]
    async fn set_config_option_unknown_session_returns_null() {
        let (server, _) = server_with_home(&[]);
        // Paritas original: sesi tak dikenal → result null, bukan error.
        let (_, result) = call_ok(
            &server,
            "session/set_config_option",
            json!({ "sessionId": "acp-404", "configId": "x", "value": "y" }),
        )
        .await;
        assert!(result.is_null());
    }

    #[tokio::test]
    async fn set_config_option_edit_approval_policy_unsupported() {
        let (server, home) = server_with_home(&[]);
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        // Original memetakan ini ke mode per-sesi; port ini tidak punya
        // mode per-sesi (konsisten dengan session/set_mode → -32002).
        let (code, msg) = call_err(
            &server,
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "edit_approval_policy", "value": "ask" }),
        )
        .await;
        assert_eq!(code, -32002);
        assert!(msg.contains("tidak didukung"));
        // Tidak ada yang tertulis ke disk untuk id yang ditolak.
        assert!(!home.join("config.toml").exists());
    }

    #[tokio::test]
    async fn set_config_option_validates_params() {
        let (server, _) = server_with_home(&[]);
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();
        let (code, _) = call_err(
            &server,
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "", "value": "y" }),
        )
        .await;
        assert_eq!(code, -32602);
        let (code, _) = call_err(
            &server,
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "x" }),
        )
        .await;
        assert_eq!(code, -32602, "value yang hilang harus invalid params");
        // String kosong adalah nilai yang sah (bukan "hilang").
        let (_, result) = call_ok(
            &server,
            "session/set_config_option",
            json!({ "sessionId": sid, "configId": "boleh_kosong", "value": "" }),
        )
        .await;
        assert_eq!(result, json!({ "configOptions": [] }));
    }

    #[tokio::test]
    async fn session_cancel_unknown_is_idempotent() {
        let server = fast_server();
        let (_, result) =
            call_ok(&server, "session/cancel", json!({ "sessionId": "acp-404" })).await;
        assert!(result.is_null());
    }

    /// B14: cancel harus BENAR-BENAR menghentikan turn yang sedang berjalan,
    /// bukan sekadar diakui. Provider lambat (40 chunk × 100ms ≈ 4 dtk);
    /// cancel di tengah → turn abort jauh sebelum 4 dtk dengan
    /// stopReason "cancelled".
    #[tokio::test]
    async fn session_cancel_aborts_inflight_turn() {
        let provider: Arc<dyn LlmProvider> = Arc::new(ChunkMockProvider {
            chunks: 40,
            delay_ms: 50,
        });
        let server = Arc::new(AcpServer::new(&test_app(provider)));
        let (_, new_r) = call_ok(&server, "session/new", json!({})).await;
        let sid = new_r["sessionId"].as_str().unwrap().to_string();

        let server2 = server.clone();
        let sid2 = sid.clone();
        let start = Instant::now();
        let prompt_task = tokio::spawn(async move {
            server2
                .dispatch(
                    "session/prompt",
                    &json!({ "sessionId": sid2, "prompt": [{ "type": "text", "text": "lambat" }] }),
                )
                .await
        });
        // Biarkan turn berjalan sebentar, lalu batalkan.
        tokio::time::sleep(Duration::from_millis(350)).await;
        let (_, cancel_r) = call_ok(&server, "session/cancel", json!({ "sessionId": sid })).await;
        assert!(cancel_r.is_null());

        let done = tokio::time::timeout(Duration::from_secs(10), prompt_task)
            .await
            .expect("turn harus selesai setelah cancel")
            .unwrap();
        let elapsed = start.elapsed();
        match done.response {
            AcpResponse::Result(v) => assert_eq!(
                v["stopReason"], "cancelled",
                "turn yang dibatalkan harus stopReason=cancelled"
            ),
            other => panic!("turn harus selesai dengan result, dapat: {other:?}"),
        }
        assert!(
            elapsed < Duration::from_millis(1500),
            "cancel harus abort dengan cepat (< 1.5 dtk dari mulai turn), butuh {elapsed:?}"
        );
        // Sesi bisa dipakai lagi setelah cancel: flag di-reset di awal
        // turn baru (paritas cancel_event.clear() di original).
        let (_, result) = call_ok(
            &server,
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "lagi" }] }),
        )
        .await;
        assert_eq!(result["stopReason"], "end_turn");
    }

    #[tokio::test]
    async fn unknown_method_errors() {
        let server = fast_server();
        let (code, _) = call_err(&server, "session/nope", json!({})).await;
        assert_eq!(code, -32601);
    }

    #[test]
    fn extract_prompt_text_joins_text_blocks() {
        let p = json!({ "prompt": [
            { "type": "text", "text": "satu" },
            { "type": "image", "data": "xxx" },
            { "type": "text", "text": "dua" },
        ] });
        assert_eq!(extract_prompt_text(&p), "satu\ndua");
        assert_eq!(extract_prompt_text(&json!({})), "");
    }

    #[test]
    fn replay_history_skips_system_and_empty() {
        let mut session = Session::new("acp");
        session.id = "s1".to_string();
        session.messages = vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text { text: "sys".into() }],
                name: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "tanya".into(),
                }],
                name: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "jawab".into(),
                }],
                name: None,
            },
        ];
        let mut out = vec![];
        replay_history(&session, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            out[1]["params"]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(out[0]["params"]["sessionId"], "s1");
    }
}
