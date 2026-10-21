//! Integration test: agent loop end-to-end dengan mock provider.
//!
//! Memverifikasi alur inti Hermes: model memanggil tool → tool dieksekusi →
//! hasil kembali ke model → balasan akhir. Tanpa API key sungguhan.
use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, HermesConfig, LlmProvider,
    MemoryConfig, ProfileConfig, Result, StreamEvent, ToolRegistry, Usage,
};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Mock provider: panggilan pertama → tool call `exec`; kedua → teks final.
struct MockProvider {
    calls: std::sync::Mutex<u32>,
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn default_model(&self) -> &str {
        "mock-model"
    }
    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (tx, rx) = mpsc::channel(16);
        // chat_simple (dipakai kompresi/judul/learning) → kembalikan "NONE"/ringkas.
        let is_simple = req.tools.is_empty();
        let mut n = self.calls.lock().unwrap();
        *n += 1;
        let call_no = *n;
        drop(n);
        tokio::spawn(async move {
            if is_simple {
                let _ = tx.send(StreamEvent::TextDelta("NONE".to_string())).await;
            } else if call_no == 1 {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: 0,
                        id: Some("call_1".to_string()),
                        name: Some("exec".to_string()),
                        arguments_delta: r#"{"command": "echo halo-hermes"}"#.to_string(),
                    })
                    .await;
            } else {
                let _ = tx
                    .send(StreamEvent::TextDelta(
                        "perintah selesai dijalankan.".to_string(),
                    ))
                    .await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }
}

fn test_factory() -> (AgentDeps, tempfile_dir::TempDir) {
    let dir = tempfile_dir::TempDir::new().unwrap();
    let home = dir.path().join(".hermes");
    std::fs::create_dir_all(&home).unwrap();
    let memory: Arc<dyn hermes_core::MemoryStore> =
        Arc::new(hermes_memory::SqliteMemoryStore::open_in_memory().unwrap());
    let skills: Arc<dyn hermes_core::SkillStore> =
        Arc::new(hermes_skills::SkillManager::with_dirs(vec![
            home.join("skills")
        ]));
    let mut reg = ToolRegistry::new();
    hermes_tools::register_builtin(&mut reg);
    let (event_tx, _event_rx) = mpsc::channel(64);
    let profile = ProfileConfig {
        working_dir: dir.path().to_string_lossy().into_owned(),
        learning_loop: false, // matikan agar mock tidak dipanggil ekstra
        ..Default::default()
    };
    let deps = AgentDeps {
        provider: Arc::new(MockProvider {
            calls: std::sync::Mutex::new(0),
        }),
        tools: Arc::new(reg),
        memory,
        skills,
        approval: Arc::new(AllowAllApproval),
        profile,
        hermes_home: home,
        cancel: CancelFlag::new(),
        memory_config: MemoryConfig::default(),
        events: event_tx,
    };
    (deps, dir)
}

/// TempDir mini tanpa crate tambahan.
mod tempfile_dir {
    use std::path::PathBuf;
    pub struct TempDir {
        path: PathBuf,
    }
    impl TempDir {
        pub fn new() -> std::io::Result<Self> {
            let path = std::env::temp_dir().join(format!(
                "hermes-it-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path)?;
            Ok(Self { path })
        }
        pub fn path(&self) -> &PathBuf {
            &self.path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[tokio::test]
async fn agent_runs_tool_and_replies() {
    let (deps, _dir) = test_factory();
    let session = hermes_core::Session::new("default");
    let mut agent = Agent::new(deps, session);
    let reply = agent.run_turn("jalankan echo halo-hermes").await.unwrap();
    assert!(reply.contains("selesai"), "balasan tak terduga: {reply}");
    // Sesi tersimpan di memory.
    let id = agent.session().id.clone();
    assert!(!id.is_empty());
    // Ada tool call di riwayat.
    let has_tool = agent
        .session()
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .any(|b| matches!(b, hermes_core::ContentBlock::ToolCall { .. }));
    assert!(has_tool, "tidak ada tool call di riwayat");
}

#[tokio::test]
async fn memory_remember_and_recall_tools() {
    use hermes_core::{MemoryEntry, ToolCall};
    let (deps, _dir) = test_factory();
    // Simulasi ToolContext seperti yang dipakai registry.
    let ctx = hermes_core::ToolContext {
        working_dir: std::path::PathBuf::from("/tmp"),
        session_cwd: Arc::new(std::sync::Mutex::new(std::path::PathBuf::from("/tmp"))),
        approval: Arc::new(AllowAllApproval),
        memory: deps.memory.clone(),
        skills: deps.skills.clone(),
        cancel: CancelFlag::new(),
        env: Default::default(),
        allow_outside_workdir: false,
        provider: None,
        model: "mock".to_string(),
    };
    let call = ToolCall {
        id: "t1".into(),
        name: "memory_remember".into(),
        arguments: serde_json::json!({"text": "Chewy suka kopi tubruk", "kind": "preference"}),
    };
    let res = deps.tools.execute_call(&call, &ctx).await;
    assert!(!res.is_error, "remember gagal: {}", res.content);
    // Verifikasi langsung via store.
    let hits = deps.memory.recall("kopi", 5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].text.contains("kopi"));
    let _ = MemoryEntry::new("x", "y");
    let _ = HermesConfig::default();
}
