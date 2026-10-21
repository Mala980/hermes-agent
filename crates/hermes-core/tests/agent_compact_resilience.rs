//! Uji integrasi (W1-B7): ketahanan kompresi.
//!
//! - (c) kegagalan ringkasan TIDAK menggagalkan turn: transkrip ter-prune
//!   dipertahankan, turn lanjut;
//! - (d) upaya ringkasan dibatasi `MAX_COMPRESSION_ATTEMPTS` (3) per sesi;
//! - alias config `compression_model` diterima sebagai `compact_model`.

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, HermesError,
    LlmProvider, MemoryConfig, MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Role,
    Session, Skill, SkillStore, StreamEvent, ToolRegistry, ToolResult, Usage,
};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Provider mock: panggilan ringkasan kompresi SELALU gagal, turn utama
/// selalu berhasil. Mencatat jumlah upaya ringkasan.
struct FailingSummaryProvider {
    summary_calls: Mutex<u32>,
}

impl FailingSummaryProvider {
    fn summary_attempts(&self) -> u32 {
        *self.summary_calls.lock().unwrap()
    }
}

#[async_trait]
impl LlmProvider for FailingSummaryProvider {
    fn name(&self) -> &str {
        "mock-fail-summary"
    }

    fn default_model(&self) -> &str {
        "mock-main"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let _ = req;
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(StreamEvent::TextDelta("ok".to_string())).await;
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        // Dalam tes ini chat_simple hanya dipakai untuk ringkasan kompresi
        // (title di-set, learning loop mati) -> selalu gagal, non-retryable.
        *self.summary_calls.lock().unwrap() += 1;
        Err(HermesError::Provider("ringkasan mock gagal".to_string()))
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

fn tool_message(id: &str, content: &str) -> Message {
    Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            result: ToolResult {
                tool_call_id: id.to_string(),
                name: "exec".to_string(),
                content: content.to_string(),
                is_error: false,
            },
        }],
        name: None,
    }
}

/// Agen dengan 15 tool result lama yang besar + threshold paksa, siap memicu
/// kompresi tiap turn. Ringkasan selalu gagal (mock).
fn failing_agent() -> (Agent, Arc<FailingSummaryProvider>) {
    let provider = Arc::new(FailingSummaryProvider {
        summary_calls: Mutex::new(0),
    });
    let profile = ProfileConfig {
        model: "main-model".to_string(),
        compact_model: "aux-model".to_string(),
        compact_threshold: 1, // paksa kompresi tiap turn (setup tes saja)
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let mut session = Session::new("default");
    session.title = "tes-compact-resilience".to_string(); // lewati maybe_generate_title
    for i in 0..15 {
        session
            .messages
            .push(Message::user(format!("pesan lama {i}")));
        session.messages.push(tool_message(
            &format!("call_{i}"),
            &format!("hasil tool lama {i} ").repeat(200),
        ));
    }
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-compact-resilience-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let agent = Agent::new(
        AgentDeps {
            provider: provider.clone(),
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
    );
    (agent, provider)
}

fn has_compressed_summary(session: &Session) -> bool {
    // W1-B15: ringkasan kompresi BUKAN pesan system (paritas
    // `_summary_placement` di original) — cari headernya di role apa pun
    // kecuali System.
    session.messages.iter().any(|m| {
        m.role != Role::System && m.text().contains("## Conversation summary (compressed)")
    })
}

fn count_pruned_tool_results(session: &Session) -> usize {
    session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| {
            matches!(
                b,
                ContentBlock::ToolResult { result }
                    if result.content.contains("pruned — content dropped")
            )
        })
        .count()
}

#[tokio::test]
async fn summarize_failure_does_not_abort_turn() {
    let (mut agent, provider) = failing_agent();
    // Turn HARUS selesai Ok walau ringkasan kompresi gagal total.
    let reply = agent
        .run_turn("halo")
        .await
        .expect("turn tidak boleh abort saat ringkasan gagal");
    assert_eq!(reply, "ok");
    // Driver me-restart pass pra-provider setelah kompresi dengan refund
    // budget (paritas original `_refund_api_call` + `_done("continue")`), jadi
    // ringkasan yang gagal bisa dicoba ulang sampai cap/cooldown sesi — tetap
    // dibatasi MAX_COMPRESSION_ATTEMPTS (3) per sesi.
    let attempts = provider.summary_attempts();
    assert!(
        attempts <= 3,
        "upaya ringkasan per turn dibatasi cap sesi (3), dapat {attempts}"
    );
    assert!(attempts >= 1, "seharusnya ada upaya ringkasan");
    // Tidak ada pesan ringkasan yang disisipkan ...
    assert!(
        !has_compressed_summary(agent.session()),
        "ringkasan gagal tidak boleh menyisipkan pesan summary"
    );
    // ... tetapi transkrip ter-prune dipertahankan (prune deterministik jalan).
    assert!(
        count_pruned_tool_results(agent.session()) > 0,
        "tool result lama harus sudah di-prune"
    );
    // Turn tetap tercatat: pesan user + balasan asisten ada.
    assert!(agent
        .session()
        .messages
        .iter()
        .any(|m| m.role == Role::User && m.text().contains("halo")));
}

#[tokio::test]
async fn compression_attempts_capped_at_three_per_session() {
    let (mut agent, provider) = failing_agent();
    // 6 turn dengan ringkasan yang selalu gagal: cooldown 10 menit aktif
    // setelah 2 kegagalan beruntun, jadi upaya berhenti jauh sebelum
    // melampaui cap — total tidak boleh lebih dari 3.
    for i in 0..6 {
        agent
            .run_turn(&format!("turn {i}"))
            .await
            .expect("turn tidak boleh abort");
    }
    let attempts = provider.summary_attempts();
    assert!(
        attempts <= 3,
        "upaya ringkasan per sesi tidak boleh melebihi 3, dapat {attempts}"
    );
    assert!(
        attempts >= 1,
        "seharusnya ada upaya ringkasan sebelum cooldown/cap"
    );
}

#[test]
fn compression_model_alias_deserializes_to_compact_model() {
    // Nama config `compression_model` (seperti di tugas) diterima sebagai
    // alias dari `compact_model`.
    let p: ProfileConfig = serde_json::from_str(r#"{"compression_model": "aux-123"}"#).unwrap();
    assert_eq!(p.compact_model, "aux-123");
    let p2: ProfileConfig = serde_json::from_str(r#"{"compact_model": "aux-456"}"#).unwrap();
    assert_eq!(p2.compact_model, "aux-456");
}
