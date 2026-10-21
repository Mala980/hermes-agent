//! Uji paritas B9: budget iterasi default 500 + refund + one-turn grace.
//!
//! Original (`~/workspace/hermes-orig`):
//! - default parent `max_iterations` = 500 (`hermes_cli/_parser.py`: --max-turns
//!   "default: 500"; docstring `agent/iteration_budget.py`).
//! - `agent/turn_tool_round.py`: "Refund the iteration when the ONLY tool was
//!   execute_code (programmatic tool calling) — cheap RPC-style calls
//!   shouldn't eat the budget."
//! - `agent/turn_context_compaction.py::_refund_api_call`: pass yang tak
//!   mencapai provider (pra-provider: hanya kompresi / preflight-only)
//!   di-refund lalu iterasi diulang.
//! - `agent/turn_iteration_prep.py` (`_budget_grace_call`): budget habis di
//!   tengah turn -> SATU panggilan pamungkas tanpa tool agar turn selesai;
//!   pass berikutnya diblokir.

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Role, Session, Skill,
    SkillStore, StreamEvent, Tool, ToolContext, ToolRegistry, ToolResult, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Provider mock: `code_turns` panggilan pertama mengembalikan tool call,
/// panggilan berikutnya mengembalikan teks biasa. Bila `mixed` true, tiap
/// turn tool mengembalikan `execute_code` + `other_tool` (bukan refundable).
/// `tools_empty` mencatat per panggilan apakah request datang tanpa tool
/// (tanda grace call pamungkas).
struct ScriptProvider {
    calls: AtomicU32,
    code_turns: u32,
    mixed: bool,
    tools_empty: Mutex<Vec<bool>>,
}

#[async_trait]
impl LlmProvider for ScriptProvider {
    fn name(&self) -> &str {
        "mock-script"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.tools_empty.lock().unwrap().push(req.tools.is_empty());
        let code_turns = self.code_turns;
        let mixed = self.mixed;
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            if n <= code_turns {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".to_string()),
                        name: Some("execute_code".to_string()),
                        arguments_delta: "{}".to_string(),
                    })
                    .await;
                if mixed {
                    let _ = tx
                        .send(StreamEvent::ToolCallDelta {
                            index: 1,
                            id: Some("call-2".to_string()),
                            name: Some("other_tool".to_string()),
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

/// Mock tool generik: nama terdaftar sehingga lolos validasi tool-call
/// (paritas B10 — registry kosong membuat `execute_code` ditolak sebagai
/// "tak dikenal" sebelum refund B9 tercapai).
struct MockTool {
    name: &'static str,
}

#[async_trait]
impl Tool for MockTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mock tool untuk tes refund budget"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: self.name.to_string(),
            content: "ok".to_string(),
            is_error: false,
        })
    }
}

fn test_agent(provider: Arc<ScriptProvider>, max_iterations: u32) -> Agent {
    test_agent_with(provider, max_iterations, 0, Vec::new())
}

/// Helper umum: provider generik, `max_iterations`, ambang kompresi absolut,
/// dan pesan awal sesi (untuk memicu kompresi).
fn test_agent_with(
    provider: Arc<dyn LlmProvider>,
    max_iterations: u32,
    compact_threshold: usize,
    seed_messages: Vec<Message>,
) -> Agent {
    let profile = ProfileConfig {
        learning_loop: false,
        max_iterations,
        compact_threshold,
        ..ProfileConfig::default()
    };
    let mut session = Session::new("default");
    // Judul non-"Untitled" agar maybe_generate_title dilewati.
    session.title = "tes-budget".to_string();
    session.messages.extend(seed_messages);
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-budget-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let mut registry = ToolRegistry::new();
    registry.register(MockTool {
        name: "execute_code",
    });
    registry.register(MockTool { name: "other_tool" });
    Agent::new(
        AgentDeps {
            provider,
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

#[test]
fn default_max_iterations_is_500() {
    // Paritas original: default 500, bukan 50.
    assert_eq!(ProfileConfig::default().max_iterations, 500);
    // Default serde juga 500 (config file yang tidak menyebut max_iterations).
    let cfg: ProfileConfig = serde_json::from_str("{}").expect("parse");
    assert_eq!(cfg.max_iterations, 500);
}

#[tokio::test]
async fn execute_code_only_turns_are_refunded() {
    // 2 turn execute_code-only + 1 turn teks dengan budget 2: tanpa refund,
    // tick ketiga akan gagal (BudgetExhausted). Dengan refund paritas
    // original, turn selesai — dan TIDAK lewat grace call (semua panggilan
    // membawa tool).
    let provider = Arc::new(ScriptProvider {
        calls: AtomicU32::new(0),
        code_turns: 2,
        mixed: false,
        tools_empty: Mutex::new(Vec::new()),
    });
    let mut agent = test_agent(provider.clone(), 2);
    let reply = agent
        .run_turn("hai")
        .await
        .expect("turn execute_code-only harus selesai tanpa menghabiskan budget");
    assert_eq!(reply, "selesai");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    let tools_empty = provider.tools_empty.lock().unwrap();
    assert_eq!(
        *tools_empty,
        vec![false, false, false],
        "refund (bukan grace) yang harus membawa turn: semua pass normal"
    );
}

#[tokio::test]
async fn mixed_tool_turns_are_not_refunded() {
    // Kontrol negatif: turn yang memanggil execute_code + tool lain TIDAK
    // di-refund (kondisi original: set nama tool == {"execute_code"}).
    // 2 turn campuran menghabiskan budget 2; dengan one-turn grace, turn
    // tetap selesai via SATU panggilan pamungkas tanpa tool — panggilan
    // ke-3 harus toolless (tanda grace), bukan pass normal.
    let provider = Arc::new(ScriptProvider {
        calls: AtomicU32::new(0),
        code_turns: 2,
        mixed: true,
        tools_empty: Mutex::new(Vec::new()),
    });
    let mut agent = test_agent(provider.clone(), 2);
    let reply = agent
        .run_turn("hai")
        .await
        .expect("turn campuran harus selesai via grace call pamungkas");
    assert_eq!(reply, "selesai");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    let tools_empty = provider.tools_empty.lock().unwrap();
    assert_eq!(
        *tools_empty,
        vec![false, false, true],
        "tanpa refund, budget habis -> pass ke-3 harus grace call tanpa tool"
    );
}

/// Provider untuk tes kompresi: `chat_simple` (ringkasan kompresi) dihitung
/// terpisah dari `chat_stream` utama; panggilan utama selalu teks final.
/// Mencatat apakah tiap panggilan utama datang tanpa tool.
struct CompactProvider {
    stream_calls: AtomicU32,
    compact_calls: AtomicU32,
    main_tools_empty: Mutex<Vec<bool>>,
}

#[async_trait]
impl LlmProvider for CompactProvider {
    fn name(&self) -> &str {
        "mock-compact"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.main_tools_empty
            .lock()
            .unwrap()
            .push(req.tools.is_empty());
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(StreamEvent::TextDelta("selesai".to_string())).await;
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        self.compact_calls.fetch_add(1, Ordering::SeqCst);
        Ok(("ringkasan".to_string(), Usage::default()))
    }
}

/// Provider untuk tes grace: panggilan pertama = tool call `other_tool`
/// (satu-satunya tool, jadi tidak refundable), sisanya = teks final.
struct GraceProvider {
    calls: AtomicU32,
    tools_empty: Mutex<Vec<bool>>,
}

#[async_trait]
impl LlmProvider for GraceProvider {
    fn name(&self) -> &str {
        "mock-grace"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.tools_empty.lock().unwrap().push(req.tools.is_empty());
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            if n == 1 {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call-1".to_string()),
                        name: Some("other_tool".to_string()),
                        arguments_delta: "{}".to_string(),
                    })
                    .await;
            } else {
                let _ = tx
                    .send(StreamEvent::TextDelta("wrap-up selesai".to_string()))
                    .await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }
}

#[tokio::test]
async fn compaction_only_pass_does_not_consume_budget() {
    // (b) max_iterations=1 dengan konteks di atas ambang kompresi: turn butuh
    // 3 pass kompresi pra-provider + 1 pass utama. Pass kompresi di-refund
    // (paritas `_refund_api_call` original), sehingga pass utama tetap pass
    // NORMAL (dengan tool) — bukan grace call. Tanpa refund, pass ke-2 sudah
    // menjadi grace call tanpa tool.
    let provider = Arc::new(CompactProvider {
        stream_calls: AtomicU32::new(0),
        compact_calls: AtomicU32::new(0),
        main_tools_empty: Mutex::new(Vec::new()),
    });
    // >10 pesan non-system agar head kompresi tak kosong (ringkasan LLM jalan).
    let seed: Vec<Message> = (0..15)
        .map(|i| Message::user(format!("pesan lama {i}")))
        .collect();
    let mut agent = test_agent_with(provider.clone(), 1, 1, seed);
    let reply = agent
        .run_turn("hai")
        .await
        .expect("turn dengan kompresi harus selesai");
    assert_eq!(reply, "selesai");
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        1,
        "hanya satu panggilan provider utama"
    );
    assert_eq!(
        provider.compact_calls.load(Ordering::SeqCst),
        1,
        "ringkasan kompresi berjalan sekali"
    );
    assert_eq!(
        *provider.main_tools_empty.lock().unwrap(),
        vec![false],
        "pass utama harus normal (dengan tool): refund kompresi bekerja"
    );
}

#[tokio::test]
async fn grace_lets_inflight_turn_finish_and_blocks_next() {
    // (f) Budget 1 tetapi turn butuh 2 pass: pass pertama memakai tool dan
    // menghabiskan budget; pass kedua adalah grace call pamungkas tanpa tool
    // — turn selesai dengan jawaban wrap-up, dan tidak ada pass ketiga
    // (panggilan provider berhenti di 2: pass berikutnya diblokir).
    let provider = Arc::new(GraceProvider {
        calls: AtomicU32::new(0),
        tools_empty: Mutex::new(Vec::new()),
    });
    let mut agent = test_agent_with(provider.clone(), 1, 0, Vec::new());
    let reply = agent
        .run_turn("hai")
        .await
        .expect("turn harus selesai via grace call pamungkas");
    assert_eq!(reply, "wrap-up selesai");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "tidak boleh ada pass ketiga setelah grace"
    );
    assert_eq!(
        *provider.tools_empty.lock().unwrap(),
        vec![false, true],
        "pass pertama normal (dengan tool), pass kedua grace tanpa tool"
    );
}

/// Pesan tool result besar (untuk memicu prune deterministik).
fn big_tool_message(id: &str) -> Message {
    Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            result: ToolResult {
                tool_call_id: id.to_string(),
                name: "exec".to_string(),
                content: format!("hasil tool lama {id} ").repeat(200),
                is_error: false,
            },
        }],
        name: None,
    }
}

/// Provider untuk tes preflight-only: `chat_simple` PANIK bila dipanggil —
/// membuktikan pass pra-provider tidak melakukan LLM call sama sekali.
struct PruneOnlyProvider {
    stream_calls: AtomicU32,
    main_tools_empty: Mutex<Vec<bool>>,
}

#[async_trait]
impl LlmProvider for PruneOnlyProvider {
    fn name(&self) -> &str {
        "mock-prune-only"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.main_tools_empty
            .lock()
            .unwrap()
            .push(req.tools.is_empty());
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(StreamEvent::TextDelta("selesai".to_string())).await;
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        panic!("pass preflight-only tidak boleh melakukan LLM call");
    }
}

#[tokio::test]
async fn preflight_only_pass_does_not_consume_budget() {
    // (d) Definisi preflight-only: pass yang menick budget tetapi keluar
    // SEBELUM panggilan provider utama diterbitkan DAN tidak mengeksekusi
    // tool apa pun (tidak ada LLM call, tidak ada tool side effect).
    // Di sini: 12 tool result besar di-prune deterministik (tanpa LLM —
    // chat_simple panik bila dipanggil) sehingga pass pertama murni
    // preflight; ia di-refund (paritas `_refund_api_call` original) dan
    // pass utama berikutnya tetap normal (dengan tool), bukan grace call.
    let provider = Arc::new(PruneOnlyProvider {
        stream_calls: AtomicU32::new(0),
        main_tools_empty: Mutex::new(Vec::new()),
    });
    let mut seed: Vec<Message> = (0..12)
        .map(|i| big_tool_message(&format!("call_{i}")))
        .collect();
    // 10 pesan kecil di ekor: prune melindungi 10 pesan terakhir, jadi hanya
    // tool result lama yang di-prune — cukup untuk turun di bawah ambang.
    for i in 0..10 {
        seed.push(Message::user(format!("ok {i}")));
    }
    let mut agent = test_agent_with(provider.clone(), 1, 1000, seed);
    let reply = agent
        .run_turn("hai")
        .await
        .expect("turn preflight-only harus selesai");
    assert_eq!(reply, "selesai");
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        1,
        "hanya satu panggilan provider utama; chat_simple tak pernah dipanggil"
    );
    assert_eq!(
        *provider.main_tools_empty.lock().unwrap(),
        vec![false],
        "pass utama harus normal (dengan tool): refund preflight bekerja"
    );
}
