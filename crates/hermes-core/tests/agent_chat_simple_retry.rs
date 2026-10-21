//! Uji integrasi: retry `chat_simple` di tiga path `Agent` — `compact()`,
//! `maybe_generate_title()`, dan `learning_loop()` — meniru
//! `agent_retry.rs` dari Batch 2.
//!
//! Semantik error tiap path dipertahankan persis seperti sebelum retry:
//! - `compact`: error TIDAK menggagalkan turn (W1-B7) — transkrip ter-prune
//!   dipertahankan, peringatan dicatat, turn lanjut;
//! - `maybe_generate_title`: error diabaikan setelah retry habis (`if let Ok`);
//! - `learning_loop`: error -> return diam-diam setelah retry habis.
//!
//! Yang berubah hanya: error retryable (menurut `classify_error`) dicoba
//! ulang dulu hingga 3x percobaan total.

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, HermesError, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Session, Skill,
    SkillStore, StreamEvent, ToolRegistry, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Provider mock: `chat_simple` gagal sebanyak `simple_fail_times` panggilan
/// pertama dengan `simple_status`, lalu mengembalikan `simple_text`.
/// `chat_stream` selalu sukses ("halo"), kecuali `emit_tool_call`: panggilan
/// stream pertama memancarkan satu `ToolCallDelta` agar `learning_loop` aktif.
struct SimpleMock {
    simple_calls: AtomicU32,
    simple_fail_times: u32,
    simple_status: u16,
    simple_text: String,
    stream_calls: AtomicU32,
    emit_tool_call: bool,
}

impl SimpleMock {
    /// Gagal 2x dengan 503 (retryable) lalu sukses.
    fn retryable(text: &str, emit_tool_call: bool) -> Self {
        Self {
            simple_calls: AtomicU32::new(0),
            simple_fail_times: 2,
            simple_status: 503,
            simple_text: text.to_string(),
            stream_calls: AtomicU32::new(0),
            emit_tool_call,
        }
    }

    /// Selalu gagal dengan 400 (non-retryable).
    fn fatal(emit_tool_call: bool) -> Self {
        Self {
            simple_calls: AtomicU32::new(0),
            simple_fail_times: u32::MAX,
            simple_status: 400,
            simple_text: String::new(),
            stream_calls: AtomicU32::new(0),
            emit_tool_call,
        }
    }

    fn simple_call_count(&self) -> u32 {
        self.simple_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for SimpleMock {
    fn name(&self) -> &str {
        "mock-simple"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, _req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let n = self.stream_calls.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::channel(16);
        let emit_tool = self.emit_tool_call && n == 1;
        tokio::spawn(async move {
            if emit_tool {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_string()),
                        name: Some("echo".to_string()),
                        arguments_delta: "{}".to_string(),
                    })
                    .await;
            } else {
                let _ = tx.send(StreamEvent::TextDelta("halo".to_string())).await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        let n = self.simple_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.simple_fail_times {
            return Err(HermesError::http(self.simple_status, None, "boom"));
        }
        Ok((self.simple_text.clone(), Usage::default()))
    }
}

struct NoopMemory;

#[async_trait]
impl MemoryStore for NoopMemory {
    async fn remember(&self, _entry: MemoryEntry) -> Result<()> {
        Ok(())
    }
    async fn recall(&self, _query: &str, _limit: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn forget(&self, _id_or_query: &str) -> Result<usize> {
        Ok(0)
    }
    async fn list_memories(&self, _limit: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn save_session(&self, _session: &Session) -> Result<()> {
        Ok(())
    }
    async fn load_session(&self, _id: &str) -> Result<Option<Session>> {
        Ok(None)
    }
    async fn list_sessions(&self, _profile: &str, _limit: usize) -> Result<Vec<Session>> {
        Ok(vec![])
    }
    async fn delete_session(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn search_sessions(&self, _query: &str, _limit: usize) -> Result<Vec<Session>> {
        Ok(vec![])
    }
}

struct NoopSkills;

#[async_trait]
impl SkillStore for NoopSkills {
    async fn list(&self) -> Result<Vec<Skill>> {
        Ok(vec![])
    }
    async fn get(&self, _name: &str) -> Result<Option<Skill>> {
        Ok(None)
    }
    async fn create(&self, _name: &str, _description: &str, _body: &str) -> Result<Skill> {
        unimplemented!()
    }
    async fn update(&self, _name: &str, _body: &str) -> Result<()> {
        Ok(())
    }
    async fn delete(&self, _name: &str) -> Result<()> {
        Ok(())
    }
    async fn find_relevant(&self, _text: &str, _limit: usize) -> Result<Vec<Skill>> {
        Ok(vec![])
    }
}

fn make_agent(provider: Arc<SimpleMock>, profile: ProfileConfig, session: Session) -> Agent {
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-simple-retry-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    Agent::new(
        AgentDeps {
            provider,
            tools: Arc::new(ToolRegistry::new()),
            memory: Arc::new(NoopMemory),
            skills: Arc::new(NoopSkills),
            approval: Arc::new(AllowAllApproval),
            profile,
            hermes_home,
            cancel: CancelFlag::new(),
            memory_config: MemoryConfig::default(),
            events,
        },
        session,
    )
}

/// Sesi dengan riwayat cukup panjang agar `compact()` terpicu
/// (`summarize_for_compaction` butuh > 10 pesan non-system agar head tak kosong).
fn long_session(title: &str) -> Session {
    let mut messages = Vec::new();
    for i in 0..15 {
        messages.push(Message::user(format!(
            "pesan pengguna nomor {i} untuk mengisi konteks percakapan agar kompresi terpicu"
        )));
        messages.push(Message::assistant_text(format!(
            "balasan asisten nomor {i} dengan cukup kata agar estimasi token melewati ambang"
        )));
    }
    Session {
        title: title.to_string(),
        messages,
        ..Session::new("default")
    }
}

fn compact_profile() -> ProfileConfig {
    ProfileConfig {
        compact_threshold: 50,
        learning_loop: false,
        ..ProfileConfig::default()
    }
}

// --- compact() ---------------------------------------------------------------

#[tokio::test]
async fn compact_retries_then_succeeds() {
    let provider = Arc::new(SimpleMock::retryable("RINGKASAN", false));
    let mut agent = make_agent(provider.clone(), compact_profile(), long_session("preset"));
    agent
        .run_turn("hai")
        .await
        .expect("turn harus selesai setelah retry kompresi");
    assert_eq!(
        provider.simple_call_count(),
        3,
        "kompresi: harus 3x percobaan (2x 503 + 1 sukses)"
    );
    assert!(
        agent
            .session()
            .messages
            .iter()
            .any(|m| m.text().contains("RINGKASAN")),
        "ringkasan harus masuk ke pesan sesi"
    );
}

#[tokio::test]
async fn compact_swallows_non_retryable_without_retry() {
    // W1-B7: kegagalan ringkasan TIDAK menggagalkan turn — transkrip
    // dipertahankan, peringatan dicatat, turn lanjut seperti biasa.
    let provider = Arc::new(SimpleMock::fatal(false));
    let mut agent = make_agent(provider.clone(), compact_profile(), long_session("preset"));
    agent
        .run_turn("hai")
        .await
        .expect("400 pada kompresi tidak boleh menggagalkan turn");
    assert!(
        provider.simple_call_count() <= 3,
        "upaya ringkasan per sesi dibatasi 3 (cap + cooldown setelah 2 gagal \
         beruntun); dapat {}",
        provider.simple_call_count(),
    );
    assert!(
        !agent
            .session()
            .messages
            .iter()
            .any(|m| m.text().contains("## Conversation summary (compressed)")),
        "ringkasan yang gagal tidak boleh disisipkan ke sesi"
    );
}

// --- maybe_generate_title() --------------------------------------------------

#[tokio::test]
async fn title_retries_then_succeeds() {
    let provider = Arc::new(SimpleMock::retryable("Judul Tes", false));
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let mut agent = make_agent(provider.clone(), profile, Session::new("default"));
    agent
        .run_turn("hai")
        .await
        .expect("turn harus selesai setelah retry judul");
    assert_eq!(
        provider.simple_call_count(),
        3,
        "judul: harus 3x percobaan (2x 503 + 1 sukses)"
    );
    assert_eq!(agent.session().title, "Judul Tes");
}

#[tokio::test]
async fn title_ignores_non_retryable_without_retry() {
    let provider = Arc::new(SimpleMock::fatal(false));
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let mut agent = make_agent(provider.clone(), profile, Session::new("default"));
    // Semantik lama: error judul diabaikan -> turn tetap sukses.
    agent
        .run_turn("hai")
        .await
        .expect("turn harus tetap sukses walau judul gagal");
    assert_eq!(
        provider.simple_call_count(),
        1,
        "400 tidak boleh di-retry pada pembuatan judul"
    );
    assert_eq!(agent.session().title, "Untitled");
}

// --- learning_loop() ---------------------------------------------------------

#[tokio::test]
async fn learning_loop_retries_then_returns_silently() {
    let provider = Arc::new(SimpleMock::retryable("NONE", true));
    let profile = ProfileConfig {
        learning_loop: true,
        ..ProfileConfig::default()
    };
    let mut agent = make_agent(provider.clone(), profile, long_session_title("preset"));
    agent
        .run_turn("hai")
        .await
        .expect("turn harus selesai setelah retry learning loop");
    assert_eq!(
        provider.simple_call_count(),
        3,
        "learning loop: harus 3x percobaan (2x 503 + 1 sukses)"
    );
}

#[tokio::test]
async fn learning_loop_returns_silently_on_non_retryable() {
    let provider = Arc::new(SimpleMock::fatal(true));
    let profile = ProfileConfig {
        learning_loop: true,
        ..ProfileConfig::default()
    };
    let mut agent = make_agent(provider.clone(), profile, long_session_title("preset"));
    // Semantik lama: error learning loop -> return diam-diam, turn tetap sukses.
    agent
        .run_turn("hai")
        .await
        .expect("turn harus tetap sukses walau learning loop gagal");
    assert_eq!(
        provider.simple_call_count(),
        1,
        "400 tidak boleh di-retry pada learning loop"
    );
}

/// Sesi pendek dengan judul preset (lewati pembuatan judul); learning loop
/// butuh tool call dari stream mock (`emit_tool_call: true`).
fn long_session_title(title: &str) -> Session {
    Session {
        title: title.to_string(),
        ..Session::new("default")
    }
}
