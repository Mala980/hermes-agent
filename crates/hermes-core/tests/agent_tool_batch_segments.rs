//! Uji integrasi B4 (barrier sekuensial tool-call): `Agent::run_turn` harus
//! mengeksekusi batch tool call sebagai segmen berurutan ala original
//! (`agent/tool_dispatch_helpers.py::_plan_tool_batch_segments`) — run call
//! aman-paralel berjalan konkuren, barrier sekuensial memisahkan segmen, dan
//! urutan emisi dipertahankan.

use async_trait::async_trait;
use hermes_core::batch_segments::{plan_tool_batch_segments, SegmentKind};
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, ProfileConfig, Result, Session, Skill, SkillStore,
    StreamEvent, Tool, ToolContext, ToolEffect, ToolRegistry, ToolResult, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type OrderLog = Arc<Mutex<Vec<String>>>;

fn ok_result(content: &str) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        name: String::new(),
        content: content.to_string(),
        is_error: false,
    }
}

// ---------------------------------------------------------------------------
// Mock generik
// ---------------------------------------------------------------------------

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

/// Satu tool call yang akan diemisikan provider mock pada iterasi pertama.
#[derive(Clone)]
struct PlannedCall {
    name: &'static str,
    arguments: serde_json::Value,
}

/// Provider mock: iterasi pertama mengemisikan semua `planned` tool call
/// sekaligus (satu blok), iterasi berikutnya menjawab teks.
struct BatchProvider {
    calls: AtomicU32,
    planned: Vec<PlannedCall>,
}

#[async_trait]
impl LlmProvider for BatchProvider {
    fn name(&self) -> &str {
        "mock-batch"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let planned = self.planned.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if n == 1 {
                for (i, pc) in planned.iter().enumerate() {
                    let _ = tx
                        .send(StreamEvent::ToolCallDelta {
                            index: i,
                            id: Some(format!("call-{i}")),
                            name: Some(pc.name.to_string()),
                            arguments_delta: pc.arguments.to_string(),
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

/// Tool mock "write_file": mencatat start/end, tidur sejenak agar interleave
/// antar call paralel terlihat.
struct SlowWrite {
    log: OrderLog,
}

#[async_trait]
impl Tool for SlowWrite {
    fn name(&self) -> &str {
        "write_file"
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    fn description(&self) -> &str {
        "mock write_file untuk uji barrier"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let marker = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        self.log.lock().unwrap().push(format!("start:{marker}"));
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.log.lock().unwrap().push(format!("end:{marker}"));
        Ok(ok_result("written"))
    }
}

/// Tool mock "read_file": mencatat start/end.
struct SlowRead {
    log: OrderLog,
}

#[async_trait]
impl Tool for SlowRead {
    fn name(&self) -> &str {
        "read_file"
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }
    fn description(&self) -> &str {
        "mock read_file untuk uji barrier"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let marker = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        self.log
            .lock()
            .unwrap()
            .push(format!("start:read:{marker}"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        self.log.lock().unwrap().push(format!("end:read:{marker}"));
        Ok(ok_result("contents"))
    }
}

/// Tool mock "web_search": kedua eksekusi harus bertemu di barrier — bukti
/// keduanya benar-benar berjalan konkuren. Bila ternyata sekuensial, barrier
/// tak pernah penuh dan timeout membuat tes gagal.
struct RendezvousSearch {
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl Tool for RendezvousSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }
    fn description(&self) -> &str {
        "mock web_search untuk uji paralelisme"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        tokio::time::timeout(Duration::from_secs(10), self.barrier.wait())
            .await
            .expect("kedua web_search harus tiba di barrier: segmen paralel");
        Ok(ok_result("hits"))
    }
}

fn test_agent(planned: Vec<PlannedCall>, registry: ToolRegistry) -> Agent {
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let session = Session {
        title: "tes-segmen".to_string(), // lewati maybe_generate_title
        ..Session::new("default")
    };
    let (events, _rx) = tokio::sync::mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-segmen-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    Agent::new(
        AgentDeps {
            provider: Arc::new(BatchProvider {
                calls: AtomicU32::new(0),
                planned,
            }),
            tools: Arc::new(registry),
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

fn planned(name: &'static str, arguments: serde_json::Value) -> PlannedCall {
    PlannedCall { name, arguments }
}

// ---------------------------------------------------------------------------
// Tes
// ---------------------------------------------------------------------------

/// Dua write ke path yang SAMA harus menjadi barrier: eksekusi strictly
/// berurutan, tanpa interleave — meniru original di mana writer menutup run
/// paralel sebelumnya.
#[tokio::test]
async fn dua_write_path_sama_dieksekusi_sekuensial() {
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(SlowWrite { log: log.clone() });
    let mut agent = test_agent(
        vec![
            planned(
                "write_file",
                serde_json::json!({"path": "sama.txt", "content": "C1"}),
            ),
            planned(
                "write_file",
                serde_json::json!({"path": "sama.txt", "content": "C2"}),
            ),
        ],
        registry,
    );

    let reply = agent
        .run_turn("tulis dua kali")
        .await
        .expect("turn selesai");
    assert_eq!(reply, "selesai");

    let log = log.lock().unwrap().clone();
    assert_eq!(
        log,
        vec!["start:C1", "end:C1", "start:C2", "end:C2"],
        "writer bertabrakan harus sekuensial penuh, tanpa interleave: {log:?}"
    );
}

/// Dua web_search (aman-paralel) harus berjalan konkuren — dibuktikan dengan
/// rendezvous di barrier.
#[tokio::test]
async fn dua_web_search_berjalan_paralel() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut registry = ToolRegistry::new();
    registry.register(RendezvousSearch {
        barrier: barrier.clone(),
    });
    let mut agent = test_agent(
        vec![
            planned("web_search", serde_json::json!({"query": "satu"})),
            planned("web_search", serde_json::json!({"query": "dua"})),
        ],
        registry,
    );

    let reply = agent.run_turn("cari dua hal").await.expect("turn selesai");
    assert_eq!(reply, "selesai");
    // barrier.wait() di dalam tool akan panic (via expect) bila eksekusi
    // ternyata sekuensial — lolos berarti benar-benar paralel.
}

/// Batch campuran: read atas path yang baru ditulis harus menunggu segmen
/// paralel sebelumnya selesai (barrier), sementara hasil tetap sesuai urutan
/// emisi.
#[tokio::test]
async fn segmen_campuran_barrier_dan_urutan_hasil() {
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(SlowWrite { log: log.clone() });
    registry.register(SlowRead { log: log.clone() });
    let mut agent = test_agent(
        vec![
            planned(
                "write_file",
                serde_json::json!({"path": "f.txt", "content": "W"}),
            ),
            planned("read_file", serde_json::json!({"path": "f.txt"})),
        ],
        registry,
    );

    let reply = agent
        .run_turn("tulis lalu baca")
        .await
        .expect("turn selesai");
    assert_eq!(reply, "selesai");

    let log = log.lock().unwrap().clone();
    // write (W) selesai dulu, baru read dimulai — barrier writer→reader.
    let end_w = log
        .iter()
        .position(|e| e == "end:W")
        .expect("write selesai");
    let start_r = log
        .iter()
        .position(|e| e == "start:read:f.txt")
        .expect("read dimulai");
    assert!(
        end_w < start_r,
        "read atas path yang ditulis harus menunggu write selesai: {log:?}"
    );

    // Hasil tool tetap sesuai urutan emisi: pesan tool pertama = write.
    let tool_names: Vec<&str> = agent
        .session()
        .messages
        .iter()
        .filter(|m| matches!(m.role, hermes_core::Role::Tool))
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { result } => Some(result.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_names, vec!["write_file", "read_file"]);
}

/// Rencana segmen campuran lewat API publik: [paralel, sekuensial].
#[test]
fn rencana_segmen_campuran_sesuai_original() {
    use hermes_core::ToolCall;
    let cwd = std::path::Path::new("/kerja");
    let mk = |id: &str, name: &str, args: serde_json::Value| ToolCall {
        id: id.to_string(),
        name: name.to_string(),
        arguments: args,
    };
    let calls = vec![
        mk("a", "web_search", serde_json::json!({"query": "1"})),
        mk("b", "web_search", serde_json::json!({"query": "2"})),
        mk("c", "clarify", serde_json::json!({"question": "lanjut?"})),
        mk("d", "read_file", serde_json::json!({"path": "x.txt"})),
    ];
    // Registry uji: web_search/read_file = Read via anotasi effect();
    // clarify tak terdaftar → Write konservatif (barrier).
    let log: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let mut registry = ToolRegistry::new();
    registry.register(RendezvousSearch {
        barrier: Arc::new(tokio::sync::Barrier::new(1)),
    });
    registry.register(SlowRead { log });
    let segments = plan_tool_batch_segments(&calls, cwd, &registry);
    let shape: Vec<(SegmentKind, Vec<&str>)> = segments
        .iter()
        .map(|(k, cs)| (*k, cs.iter().map(|c| c.id.as_str()).collect()))
        .collect();
    assert_eq!(
        shape,
        vec![
            (SegmentKind::Parallel, vec!["a", "b"]),
            (SegmentKind::Sequential, vec!["c", "d"]),
        ]
    );
}
