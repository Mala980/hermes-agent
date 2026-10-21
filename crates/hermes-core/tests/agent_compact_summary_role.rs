//! Uji integrasi (W1-B15): ringkasan kompresi bukan pesan `system` kedua.
//!
//! Paritas `ContextCompressor._summary_placement` di
//! `agent/context_compressor.py` original: ringkasan disisipkan dengan role
//! user/assistant (atau digabung ke tail) agar alternation ketat terjaga —
//! TIDAK PERNAH sebagai pesan `system` kedua yang merusak alternation dan
//! menabrak system prompt beku (prompt caching).

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Role, Session, Skill,
    SkillStore, StreamEvent, ToolRegistry, ToolResult, Usage,
};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Provider mock: turn utama menjawab "ok", ringkasan kompresi berhasil.
struct SummarizingProvider {
    summary_calls: Mutex<u32>,
}

#[async_trait]
impl LlmProvider for SummarizingProvider {
    fn name(&self) -> &str {
        "mock-summarizing"
    }

    fn default_model(&self) -> &str {
        "mock-main"
    }

    async fn chat_stream(&self, _req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(StreamEvent::TextDelta("ok".to_string())).await;
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        *self.summary_calls.lock().unwrap() += 1;
        Ok(("RINGKASAN MOCK".to_string(), Usage::default()))
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

fn make_agent(provider: Arc<SummarizingProvider>) -> Agent {
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-compact-summary-role-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let profile = ProfileConfig {
        model: "mock-model".to_string(),
        compact_threshold: 1, // paksa kompresi tiap turn (setup tes saja)
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let mut session = Session::new("default");
    session.title = "tes-compact-summary-role".to_string();
    // Riwayat panjang: user/tool bergantian → head kaya, tail 10 pesan.
    for i in 0..15 {
        session
            .messages
            .push(Message::user(format!("pesan lama {i}")));
        session.messages.push(tool_message(
            &format!("call_{i}"),
            &format!("hasil tool lama {i} "),
        ));
    }
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

#[tokio::test]
async fn compaction_summary_is_never_a_second_system_message() {
    let provider = Arc::new(SummarizingProvider {
        summary_calls: Mutex::new(0),
    });
    let mut agent = make_agent(provider.clone());

    let reply = agent.run_turn("halo").await.expect("turn harus selesai");
    assert_eq!(reply, "ok");
    assert!(
        *provider.summary_calls.lock().unwrap() >= 1,
        "ringkasan kompresi harus terpanggil"
    );

    let messages = &agent.session().messages;
    // 1) Tidak ada pesan system di session.messages — system prompt live
    // selalu di-prepend terpisah saat menyusun request (B15).
    assert!(
        messages.iter().all(|m| m.role != Role::System),
        "tak boleh ada pesan system kedua di transkrip sesi"
    );
    // 2) Ringkasan ada, ber-role user atau assistant.
    let summary = messages
        .iter()
        .find(|m| m.text().contains("## Conversation summary (compressed)"))
        .expect("pesan ringkasan harus ada");
    assert!(
        matches!(summary.role, Role::User | Role::Assistant),
        "ringkasan harus user/assistant, dapat {:?}",
        summary.role
    );
    assert!(
        summary.text().contains("RINGKASAN MOCK"),
        "isi ringkasan harus dari summarizer"
    );
    // 3) Alternation ketat terjaga di seluruh transkrip sesi: tidak ada dua
    // pesan BERURUTAN dengan role sama (baris tool di antaranya tidak
    // melanggar — sama seperti urutan user → assistant(tool_calls) → tool).
    let roles: Vec<Role> = messages.iter().map(|m| m.role).collect();
    for w in roles.windows(2) {
        assert_ne!(
            w[0], w[1],
            "alternation ketat rusak setelah kompresi: {roles:?}"
        );
    }
}
