//! Uji integrasi W1-B5 (persist-before-execute + per-result flush):
//! `Agent::run_turn` harus mem-persist tool-call turn SEBELUM eksekusi tool
//! dan mem-flush tiap hasil tool satu per satu — meniru invariant original
//! (`agent/turn_tool_round.py`: "persist the tool-call turn before any tool
//! side effects"). Crash (panic) di tengah batch tidak boleh menghilangkan
//! blok yang sudah tereksekusi, dan kegagalan persist tidak boleh
//! menggagalkan turn (fail-open, didokumentasikan di `Agent::persist_message`).

use async_trait::async_trait;
use futures::FutureExt;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, HermesError,
    LlmProvider, MemoryConfig, MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Session,
    Skill, SkillStore, StreamEvent, Tool, ToolContext, ToolRegistry, ToolResult, Usage,
};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type OrderLog = Arc<Mutex<Vec<String>>>;
type Snapshots = Arc<Mutex<Vec<Vec<Message>>>>;

/// MemoryStore mock: menyimpan snapshot penuh setiap `save_session` agar isi
/// store bisa diperiksa setelah kegagalan parsial; `fail_saves` memaksa semua
/// save gagal (untuk uji fail-open).
struct SnapshotMemory {
    log: OrderLog,
    snapshots: Snapshots,
    fail_saves: bool,
}

#[async_trait]
impl MemoryStore for SnapshotMemory {
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
        if self.fail_saves {
            return Err(HermesError::Memory("db terkunci (simulasi)".to_string()));
        }
        let has_tool_call = has_tool_call(&session.messages);
        let n_results = tool_results(&session.messages).len();
        self.log.lock().unwrap().push(format!(
            "save:msgs={}:toolcall={has_tool_call}:results={n_results}",
            session.messages.len()
        ));
        self.snapshots
            .lock()
            .unwrap()
            .push(session.messages.clone());
        Ok(())
    }
    async fn load_session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .last()
            .map(|messages| Session {
                id: id.to_string(),
                messages: messages.clone(),
                ..Session::new("default")
            }))
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

/// Tool mock yang selalu sukses.
struct OkTool {
    log: OrderLog,
}

#[async_trait]
impl Tool for OkTool {
    fn name(&self) -> &str {
        "ok_tool"
    }
    fn description(&self) -> &str {
        "tool mock yang selalu sukses"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        self.log.lock().unwrap().push("exec:ok_tool".to_string());
        Ok(ToolResult {
            tool_call_id: "call-1".to_string(),
            name: "ok_tool".to_string(),
            content: "hasil-ok".to_string(),
            is_error: false,
        })
    }
}

/// Tool mock yang mensimulasikan crash: panic di tengah batch.
struct BoomTool {
    log: OrderLog,
}

#[async_trait]
impl Tool for BoomTool {
    fn name(&self) -> &str {
        "boom_tool"
    }
    fn description(&self) -> &str {
        "tool mock yang panic (simulasi crash)"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        self.log.lock().unwrap().push("exec:boom_tool".to_string());
        panic!("boom: simulasi crash di tengah batch tool");
    }
}

#[derive(Clone, Copy)]
enum Plan {
    /// Iterasi 1: ok_tool lalu boom_tool (panic); iterasi berikut: teks.
    PanicBatch,
    /// Iterasi 1: ok_tool saja; iterasi berikut: teks.
    OkBatch,
}

/// Provider mock: iterasi pertama mengembalikan tool call sesuai plan,
/// iterasi berikutnya menjawab teks sehingga turn berakhir.
struct PlanProvider {
    calls: AtomicU32,
    plan: Plan,
}

#[async_trait]
impl LlmProvider for PlanProvider {
    fn name(&self) -> &str {
        "mock-plan"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let plan = self.plan;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if n == 1 {
                let calls: &[(&str, &str)] = match plan {
                    Plan::PanicBatch => &[("call-1", "ok_tool"), ("call-2", "boom_tool")],
                    Plan::OkBatch => &[("call-1", "ok_tool")],
                };
                for (i, (id, name)) in calls.iter().enumerate() {
                    let _ = tx
                        .send(StreamEvent::ToolCallDelta {
                            index: i,
                            id: Some(id.to_string()),
                            name: Some(name.to_string()),
                            arguments_delta: "{}".to_string(),
                        })
                        .await;
                }
            } else {
                let _ = tx.send(StreamEvent::TextDelta("selesai".to_string())).await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }
}

fn has_tool_call(messages: &[Message]) -> bool {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
}

fn tool_results(messages: &[Message]) -> Vec<ToolResult> {
    messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { result } => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn test_agent(
    log: &OrderLog,
    snapshots: &Snapshots,
    plan: Plan,
    fail_saves: bool,
) -> (Agent, Arc<SnapshotMemory>) {
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let session = Session {
        title: "tes-flush".to_string(), // lewati maybe_generate_title
        ..Session::new("default")
    };
    let mut registry = ToolRegistry::new();
    registry.register(OkTool { log: log.clone() });
    registry.register(BoomTool { log: log.clone() });
    let (events, _rx) = tokio::sync::mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-flush-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let memory = Arc::new(SnapshotMemory {
        log: log.clone(),
        snapshots: snapshots.clone(),
        fail_saves,
    });
    let agent = Agent::new(
        AgentDeps {
            provider: Arc::new(PlanProvider {
                calls: AtomicU32::new(0),
                plan,
            }),
            tools: Arc::new(registry),
            memory: memory.clone(),
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
    (agent, memory)
}

/// Crash (panic) di tengah batch: tool-call turn harus tetap bisa ditemukan
/// di store, dan persist-nya harus terjadi SEBELUM tool kedua dieksekusi.
#[tokio::test]
async fn tool_call_turn_survives_mid_batch_panic() {
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let snapshots: Snapshots = Arc::new(Mutex::new(Vec::new()));
    let (mut agent, memory) = test_agent(&log, &snapshots, Plan::PanicBatch, false);
    let sid = agent.session().id.clone();

    let outcome = AssertUnwindSafe(agent.run_turn("hai")).catch_unwind().await;
    assert!(outcome.is_err(), "boom_tool harus panic di tengah batch");

    // Tool-call turn tetap ada di store walau turn abort.
    let stored = memory
        .load_session(&sid)
        .await
        .expect("load_session gagal")
        .expect("sesi harus ada di store");
    assert!(
        has_tool_call(&stored.messages),
        "tool-call turn harus tersimpan di store: {stored:?}"
    );

    // Ordering: persist berisi tool call pending terjadi sebelum eksekusi tool.
    let log = log.lock().unwrap();
    let save_idx = log
        .iter()
        .position(|e| e.contains("toolcall=true"))
        .expect("harus ada persist berisi tool call pending");
    let boom_idx = log
        .iter()
        .position(|e| e == "exec:boom_tool")
        .expect("boom_tool harus sempat dieksekusi");
    assert!(
        save_idx < boom_idx,
        "persist tool-call turn harus mendahului eksekusi: {log:?}"
    );
}

/// Flush per-hasil bersifat inkremental: setelah kegagalan parsial, store
/// memuat hasil tool yang sudah selesai (bukan hanya save akhir turn yang
/// tak pernah terjadi karena panic).
#[tokio::test]
async fn tool_results_flushed_incrementally_after_partial_failure() {
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let snapshots: Snapshots = Arc::new(Mutex::new(Vec::new()));
    let (mut agent, memory) = test_agent(&log, &snapshots, Plan::PanicBatch, false);
    let sid = agent.session().id.clone();

    let outcome = AssertUnwindSafe(agent.run_turn("hai")).catch_unwind().await;
    assert!(outcome.is_err(), "boom_tool harus panic di tengah batch");

    let stored = memory
        .load_session(&sid)
        .await
        .expect("load_session gagal")
        .expect("sesi harus ada di store");
    let results = tool_results(&stored.messages);
    assert_eq!(
        results.len(),
        1,
        "tepat satu hasil tool (ok_tool) harus sudah ter-flush: {stored:?}"
    );
    assert_eq!(results[0].name, "ok_tool");
    assert_eq!(results[0].content, "hasil-ok");
    assert!(!results[0].is_error);

    // Bukti inkremental: ada save dengan tepat 1 hasil tool — save akhir
    // turn tak pernah terjadi karena panic, jadi ini pasti flush per-hasil.
    let log = log.lock().unwrap();
    assert!(
        log.iter()
            .any(|e| e == "save:msgs=3:toolcall=true:results=1"),
        "harus ada flush per-hasil (msgs=3, results=1): {log:?}"
    );
}

/// Fail-open: kegagalan persist (dicatat sebagai warning) tidak boleh
/// menggagalkan turn — tool tetap dieksekusi dan turn selesai normal.
#[tokio::test]
async fn persistence_failure_does_not_crash_turn() {
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let snapshots: Snapshots = Arc::new(Mutex::new(Vec::new()));
    let (mut agent, _memory) = test_agent(&log, &snapshots, Plan::OkBatch, true);

    let reply = agent
        .run_turn("hai")
        .await
        .expect("turn harus tetap selesai walau persist gagal");
    assert_eq!(reply, "selesai");

    let log = log.lock().unwrap();
    assert!(
        log.iter().any(|e| e == "exec:ok_tool"),
        "tool harus tetap dieksekusi walau persist gagal: {log:?}"
    );
}
