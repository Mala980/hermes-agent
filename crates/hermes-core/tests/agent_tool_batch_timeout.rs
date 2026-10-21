//! Uji integrasi (W1-B13): deadline per batch tool call.
//!
//! Paritas `timeouts.tools.concurrent_batch` (`_DEFAULT_CONCURRENT_TOOL_TIMEOUT_S`)
//! di `agent/tool_executor.py` original: satu tool yang hang tidak boleh
//! menahan turn selamanya — call yang macet ditandai timeout (hasil error
//! jelas) dan batch lanjut dengan sisanya.

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, ProfileConfig, Result, Session, Skill, SkillStore,
    StreamEvent, Tool, ToolContext, ToolEffect, ToolRegistry, ToolResult, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

struct ScriptedTurn {
    text: &'static str,
    calls: Vec<(&'static str, &'static str)>, // (id, nama)
}

struct ScriptedProvider {
    calls: AtomicU32,
    script: Vec<ScriptedTurn>,
}

#[async_trait]
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "mock-scripted"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, _req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
        let turn = &self.script[n.min(self.script.len() - 1)];
        let (tx, rx) = mpsc::channel(16);
        let text = turn.text.to_string();
        let calls = turn.calls.clone();
        tokio::spawn(async move {
            if !text.is_empty() {
                let _ = tx.send(StreamEvent::TextDelta(text)).await;
            }
            for (i, (id, name)) in calls.into_iter().enumerate() {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: i,
                        id: Some(id.to_string()),
                        name: Some(name.to_string()),
                        arguments_delta: "{}".to_string(),
                    })
                    .await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        Ok(("ringkasan".to_string(), Usage::default()))
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

/// Tool yang hang selamanya — mensimulasikan tool macet.
struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str {
        "slow_tool"
    }
    fn description(&self) -> &str {
        "mock yang hang"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read // reader → segmen paralel bersama fast_tool
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        std::future::pending().await
    }
}

struct FastTool;

#[async_trait]
impl Tool for FastTool {
    fn name(&self) -> &str {
        "fast_tool"
    }
    fn description(&self) -> &str {
        "mock cepat"
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: "fast_tool".to_string(),
            content: "hasil cepat".to_string(),
            is_error: false,
        })
    }
}

fn make_agent(provider: Arc<ScriptedProvider>, timeout_secs: u64) -> Agent {
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-batch-timeout-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let profile = ProfileConfig {
        model: "mock-model".to_string(),
        learning_loop: false,
        tool_batch_timeout_secs: timeout_secs,
        ..ProfileConfig::default()
    };
    let mut tools = ToolRegistry::new();
    tools.register(SlowTool);
    tools.register(FastTool);
    let mut session = Session::new("default");
    session.title = "tes-batch-timeout".to_string();
    Agent::new(
        AgentDeps {
            provider,
            tools: Arc::new(tools),
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

fn tool_results(session: &Session) -> Vec<(String, String, bool)> {
    session
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { result } => {
                Some((result.name.clone(), result.content.clone(), result.is_error))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn hung_tool_does_not_stall_turn() {
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicU32::new(0),
        script: vec![
            ScriptedTurn {
                text: "",
                calls: vec![("c1", "slow_tool"), ("c2", "fast_tool")],
            },
            ScriptedTurn {
                text: "selesai",
                calls: vec![],
            },
        ],
    });
    let mut agent = make_agent(provider.clone(), 1);

    let start = Instant::now();
    let reply = agent
        .run_turn("kerjakan dua tool")
        .await
        .expect("turn harus selesai walau satu tool hang");
    let elapsed = start.elapsed();

    assert_eq!(reply, "selesai");
    // Deadline 1 detik: turn harus selesai jauh sebelum macet selamanya.
    assert!(elapsed < Duration::from_secs(20), "turn macet: {elapsed:?}");
    assert!(
        elapsed >= Duration::from_millis(900),
        "deadline 1 detik seharusnya ditunggu: {elapsed:?}"
    );

    let results = tool_results(agent.session());
    let slow = results
        .iter()
        .find(|(name, _, _)| name == "slow_tool")
        .expect("hasil slow_tool harus ada");
    assert!(slow.2, "hasil timeout harus is_error");
    assert_eq!(
        slow.1, "Error executing tool 'slow_tool': timed out after 1.0s",
        "format pesan timeout paritas original"
    );
    let fast = results
        .iter()
        .find(|(name, _, _)| name == "fast_tool")
        .expect("hasil fast_tool harus ada");
    assert_eq!(fast.1, "hasil cepat");
    assert!(!fast.2);
}

#[tokio::test]
async fn batch_timeout_config_default_matches_original() {
    // Paritas `_DEFAULT_CONCURRENT_TOOL_TIMEOUT_S = 420.0` di original.
    assert_eq!(
        ProfileConfig::default().tool_batch_timeout_secs,
        420,
        "default harus 420 detik seperti original"
    );
    assert_eq!(
        hermes_core::config::DEFAULT_TOOL_BATCH_TIMEOUT_SECS,
        420,
        "konstanta default harus 420"
    );
}
