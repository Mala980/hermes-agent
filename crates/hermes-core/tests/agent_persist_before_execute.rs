//! Uji integrasi B5 (persist-before-execute): `Agent::run_turn` harus
//! memanggil `MemoryStore::save_session` SETELAH pesan assistant berisi tool
//! call disimpan tetapi SEBELUM eksekusi tool dimulai — meniru invariant
//! original (`agent/turn_tool_round.py`: "persist the tool-call turn BEFORE
//! any side effect").

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, ProfileConfig, Result, Session, Skill, SkillStore,
    StreamEvent, Tool, ToolContext, ToolRegistry, ToolResult, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type OrderLog = Arc<Mutex<Vec<String>>>;

/// MemoryStore mock yang mencatat setiap pemanggilan `save_session` beserta
/// jumlah pesan sesi dan apakah tool call pending sudah ada di sesi.
struct OrderMemory {
    log: OrderLog,
}

#[async_trait]
impl MemoryStore for OrderMemory {
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
    async fn save_session(&self, session: &Session) -> Result<()> {
        let has_tool_call = session.messages.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
        });
        self.log.lock().unwrap().push(format!(
            "save:msgs={}:toolcall={has_tool_call}",
            session.messages.len()
        ));
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

/// Tool mock: mencatat momen eksekusi dimulai.
struct ProbeTool {
    log: OrderLog,
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "probe"
    }
    fn description(&self) -> &str {
        "tool mock untuk uji persist-before-execute"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        self.log.lock().unwrap().push("tool_exec".to_string());
        Ok(ToolResult {
            tool_call_id: "call-1".to_string(),
            name: "probe".to_string(),
            content: "ok".to_string(),
            is_error: false,
        })
    }
}

/// Provider mock: iterasi pertama mengembalikan satu tool call `probe`,
/// iterasi berikutnya menjawab teks sehingga turn berakhir.
struct ToolCallProvider {
    calls: AtomicU32,
}

#[async_trait]
impl LlmProvider for ToolCallProvider {
    fn name(&self) -> &str {
        "mock-toolcall"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if n == 1 {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".to_string()),
                        name: Some("probe".to_string()),
                        arguments_delta: "{}".to_string(),
                    })
                    .await;
            } else {
                let _ = tx.send(StreamEvent::TextDelta("selesai".to_string())).await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }
}

fn test_agent(log: &OrderLog) -> Agent {
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let session = Session {
        title: "tes-persist".to_string(), // lewati maybe_generate_title
        ..Session::new("default")
    };
    let mut registry = ToolRegistry::new();
    registry.register(ProbeTool { log: log.clone() });
    let (events, _rx) = tokio::sync::mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-persist-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    Agent::new(
        AgentDeps {
            provider: Arc::new(ToolCallProvider {
                calls: AtomicU32::new(0),
            }),
            tools: Arc::new(registry),
            memory: Arc::new(OrderMemory { log: log.clone() }),
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
async fn save_session_happens_before_tool_execution() {
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let mut agent = test_agent(&log);
    let reply = agent.run_turn("hai").await.expect("turn harus selesai");
    assert_eq!(reply, "selesai");

    let log = log.lock().unwrap();
    let exec_idx = log
        .iter()
        .position(|e| e == "tool_exec")
        .expect("tool harus dieksekusi");
    // Persist pertama yang sudah memuat tool call pending (user + pesan
    // assistant berisi tool call = 2 pesan) harus mendahului eksekusi tool.
    let pre_exec_save_idx = log
        .iter()
        .position(|e| e == "save:msgs=2:toolcall=true")
        .expect("harus ada persist sesi berisi tool call pending");
    assert!(
        pre_exec_save_idx < exec_idx,
        "save_session harus dipanggil SEBELUM eksekusi tool: {log:?}"
    );
}
