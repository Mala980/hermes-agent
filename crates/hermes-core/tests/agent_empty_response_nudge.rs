//! Uji integrasi (W1-B11): empty-response nudge.
//!
//! Paritas `agent/turn_empty_response.py` + `_EMPTY_TOOL_RESPONSE_NUDGE` di
//! original: respons kosong TANPA tool call bukan akhir turn yang senyap —
//! model didorong sekali per turn (nudge) lalu dicoba lagi; kosong kedua
//! berturut-turut mengakhiri turn dengan sentinel terminal "(empty)".

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, ContentBlock, HermesError,
    LlmProvider, MemoryConfig, MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Role,
    Session, Skill, SkillStore, StreamEvent, Tool, ToolContext, ToolEffect, ToolRegistry,
    ToolResult, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Satu respons scripted: teks + daftar tool call (id, nama, args-mentah).
struct ScriptedTurn {
    text: &'static str,
    calls: Vec<(&'static str, &'static str, &'static str)>,
}

/// Provider mock yang memutar script respons per panggilan.
struct ScriptedProvider {
    calls: AtomicU32,
    script: Vec<ScriptedTurn>,
}

impl ScriptedProvider {
    fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
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
            for (i, (id, name, args)) in calls.into_iter().enumerate() {
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: i,
                        id: Some(id.to_string()),
                        name: Some(name.to_string()),
                        arguments_delta: args.to_string(),
                    })
                    .await;
            }
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
    }

    async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
        Err(HermesError::Provider("tak dipakai di tes ini".to_string()))
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

struct FastTool;

#[async_trait]
impl Tool for FastTool {
    fn name(&self) -> &str {
        "fast_tool"
    }
    fn description(&self) -> &str {
        "mock cepat untuk uji nudge"
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

fn make_agent(provider: Arc<ScriptedProvider>, tools: ToolRegistry) -> Agent {
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-empty-nudge-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let profile = ProfileConfig {
        model: "mock-model".to_string(),
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let mut session = Session::new("default");
    session.title = "tes-empty-nudge".to_string(); // lewati maybe_generate_title
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

fn nudge_messages(session: &Session) -> Vec<&Message> {
    session
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User && m.text().contains("empty") && m.text().contains("continue")
        })
        .collect()
}

#[tokio::test]
async fn empty_response_gets_one_nudge_then_retries() {
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicU32::new(0),
        script: vec![
            ScriptedTurn {
                text: "",
                calls: vec![],
            },
            ScriptedTurn {
                text: "jawaban akhir",
                calls: vec![],
            },
        ],
    });
    let mut agent = make_agent(provider.clone(), ToolRegistry::new());

    let reply = agent.run_turn("halo").await.expect("turn harus selesai");
    assert_eq!(reply, "jawaban akhir");
    assert_eq!(provider.call_count(), 2, "nudge harus memicu satu retry");

    // Jejak nudge di transkrip: baris assistant "(empty)" + pesan user nudge.
    let nudges = nudge_messages(agent.session());
    assert_eq!(nudges.len(), 1, "tepat satu nudge per turn");
    let has_empty_marker = agent
        .session()
        .messages
        .iter()
        .any(|m| m.role == Role::Assistant && m.text() == "(empty)");
    assert!(has_empty_marker, "baris assistant (empty) harus ada");
}

#[tokio::test]
async fn double_empty_ends_turn_with_sentinel() {
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicU32::new(0),
        script: vec![
            ScriptedTurn {
                text: "",
                calls: vec![],
            },
            ScriptedTurn {
                text: "   ",
                calls: vec![],
            },
            // Tak boleh tercapai: turn harus sudah berakhir.
            ScriptedTurn {
                text: "tidak boleh dipakai",
                calls: vec![],
            },
        ],
    });
    let mut agent = make_agent(provider.clone(), ToolRegistry::new());

    let reply = agent.run_turn("halo").await.expect("turn harus selesai");
    assert_eq!(
        reply, "(empty)",
        "sentinel terminal paritas _terminal_empty"
    );
    assert_eq!(
        provider.call_count(),
        2,
        "hanya satu nudge: tidak boleh retry selamanya"
    );
    assert_eq!(
        nudge_messages(agent.session()).len(),
        1,
        "nudge tepat satu walau dua kosong"
    );
}

#[tokio::test]
async fn post_tool_empty_uses_tool_nudge() {
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicU32::new(0),
        script: vec![
            ScriptedTurn {
                text: "",
                calls: vec![("c1", "fast_tool", "{}")],
            },
            // Kosong setelah tool: nudge pasca-tool lalu retry.
            ScriptedTurn {
                text: "",
                calls: vec![],
            },
            ScriptedTurn {
                text: "selesai setelah tool",
                calls: vec![],
            },
        ],
    });
    let mut tools = ToolRegistry::new();
    tools.register(FastTool);
    let mut agent = make_agent(provider.clone(), tools);

    let reply = agent
        .run_turn("kerjakan")
        .await
        .expect("turn harus selesai");
    assert_eq!(reply, "selesai setelah tool");

    // Nudge pasca-tool memakai teks verbatim original.
    let nudges = nudge_messages(agent.session());
    assert_eq!(nudges.len(), 1);
    assert!(
        nudges[0]
            .text()
            .contains("You just executed tool calls but returned an empty response"),
        "nudge pasca-tool harus verbatim original: {}",
        nudges[0].text()
    );
    // Hasil tool tetap ada di transkrip sebelum nudge.
    let tool_results: Vec<_> = agent
        .session()
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect();
    assert!(
        tool_results.iter().any(|r| r.content == "hasil cepat"),
        "hasil tool harus tercatat"
    );
}

#[tokio::test]
async fn non_empty_response_unchanged() {
    // Regresi: respons normal tak boleh memicu nudge.
    let provider = Arc::new(ScriptedProvider {
        calls: AtomicU32::new(0),
        script: vec![ScriptedTurn {
            text: "halo juga",
            calls: vec![],
        }],
    });
    let mut agent = make_agent(provider.clone(), ToolRegistry::new());
    let reply = agent.run_turn("halo").await.expect("turn harus selesai");
    assert_eq!(reply, "halo juga");
    assert_eq!(provider.call_count(), 1);
    assert!(nudge_messages(agent.session()).is_empty());
}
