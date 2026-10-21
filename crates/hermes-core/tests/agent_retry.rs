//! Uji integrasi: provider mock yang gagal 2x (HTTP 503, retryable) lalu
//! berhasil — `Agent::run_turn` harus menyelesaikan turn, bukan abort.

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, HermesError, LlmProvider,
    MemoryConfig, MemoryEntry, MemoryStore, ProfileConfig, Result, Session, Skill, SkillStore,
    StreamEvent, ToolRegistry, Usage,
};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Provider mock: gagal dengan HTTP 503 sebanyak `fail_times` panggilan
/// pertama, lalu mengembalikan jawaban teks biasa.
struct FlakyProvider {
    calls: AtomicU32,
    fail_times: u32,
}

#[async_trait]
impl LlmProvider for FlakyProvider {
    fn name(&self) -> &str {
        "mock-flaky"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, _req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n <= self.fail_times {
            return Err(HermesError::http(503, None, "Service Unavailable"));
        }
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(StreamEvent::TextDelta("halo".to_string())).await;
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

fn test_agent(provider: Arc<FlakyProvider>) -> (Agent, Arc<FlakyProvider>) {
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let session = Session {
        title: "tes-retry".to_string(), // lewati maybe_generate_title
        ..Session::new("default")
    };
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-retry-test");
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

#[tokio::test]
async fn turn_completes_after_two_transient_failures() {
    let provider = Arc::new(FlakyProvider {
        calls: AtomicU32::new(0),
        fail_times: 2,
    });
    let (mut agent, provider) = test_agent(provider);
    let reply = agent.run_turn("hai").await.expect("turn harus selesai");
    assert_eq!(reply, "halo");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        3,
        "harus 3x percobaan: 2 gagal + 1 sukses"
    );
}

#[tokio::test]
async fn turn_fails_fast_on_deterministic_error() {
    struct BadRequestProvider;
    #[async_trait]
    impl LlmProvider for BadRequestProvider {
        fn name(&self) -> &str {
            "mock-bad"
        }
        fn default_model(&self) -> &str {
            "mock-model"
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
            Err(HermesError::http(400, None, "bad request"))
        }
    }
    let profile = ProfileConfig {
        learning_loop: false,
        ..ProfileConfig::default()
    };
    let session = Session {
        title: "tes-retry".to_string(),
        ..Session::new("default")
    };
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-retry-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let mut agent = Agent::new(
        AgentDeps {
            provider: Arc::new(BadRequestProvider),
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
    let err = agent.run_turn("hai").await.expect_err("400 harus gagal");
    assert!(
        err.to_string().contains("400"),
        "error harus menyebut 400: {err}"
    );
}
