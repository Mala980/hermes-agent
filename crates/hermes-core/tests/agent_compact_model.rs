//! Uji integrasi: `Agent::compact` memakai model aux (`ProfileConfig::compact_model`)
//! untuk permintaan ringkasan kompresi — bukan model utama — dengan fallback
//! ke model utama bila field tersebut kosong (belum dikonfigurasi).
//!
//! Paritas dengan original: `auxiliary.compression.model` di config.yaml.

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, LlmProvider, MemoryConfig,
    MemoryEntry, MemoryStore, Message, ProfileConfig, Result, Session, Skill, SkillStore,
    StreamEvent, ToolRegistry, Usage,
};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::mpsc;

/// Provider mock yang mencatat `req.model` pada setiap panggilan.
struct ModelRecordingProvider {
    models: Mutex<Vec<String>>,
}

#[async_trait]
impl LlmProvider for ModelRecordingProvider {
    fn name(&self) -> &str {
        "mock-recording"
    }

    fn default_model(&self) -> &str {
        "mock-main"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        self.models.lock().unwrap().push(req.model.clone());
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx
                .send(StreamEvent::TextDelta("ringkasan ok".to_string()))
                .await;
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

/// Jalankan satu turn dengan 15 pesan lama dan `compact_threshold` sangat kecil
/// sehingga `compact()` pasti terpicu; kembalikan daftar model yang dipakai
/// provider per panggilan (panggilan pertama = permintaan ringkasan kompresi).
async fn recorded_models(compact_model: &str) -> Vec<String> {
    let provider = Arc::new(ModelRecordingProvider {
        models: Mutex::new(vec![]),
    });
    let profile = ProfileConfig {
        model: "main-model".to_string(),
        compact_model: compact_model.to_string(),
        compact_threshold: 1, // paksa kompresi tiap turn (setup tes saja)
        learning_loop: false, // matikan agar mock tidak dipanggil ekstra
        ..ProfileConfig::default()
    };
    let mut session = Session::new("default");
    session.title = "tes-compact-model".to_string(); // lewati maybe_generate_title
    for i in 0..15 {
        session
            .messages
            .push(Message::user(format!("pesan lama {i}")));
    }
    let (events, _rx) = mpsc::channel(64);
    let hermes_home = std::env::temp_dir().join("hermes-compact-model-test");
    let _ = std::fs::create_dir_all(&hermes_home);
    let mut agent = Agent::new(
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
    agent.run_turn("halo").await.expect("turn harus selesai");
    let models = provider.models.lock().unwrap().clone();
    models
}

#[tokio::test]
async fn compact_uses_aux_model_when_configured() {
    let models = recorded_models("aux-model").await;
    assert!(!models.is_empty(), "provider harus dipanggil");
    assert_eq!(
        models[0], "aux-model",
        "permintaan ringkasan kompresi harus memakai model aux, bukan model utama"
    );
    assert!(
        models.iter().any(|m| m == "main-model"),
        "turn utama tetap memakai model utama, bukan aux"
    );
}

#[tokio::test]
async fn compact_falls_back_to_main_model_when_unset() {
    let models = recorded_models("").await;
    assert!(!models.is_empty(), "provider harus dipanggil");
    assert_eq!(
        models[0], "main-model",
        "tanpa compact_model, kompresi harus fallback ke model utama"
    );
}

#[test]
fn compact_model_defaults_to_empty_string() {
    // Field baru harus default kosong (fallback ke model utama) agar profil
    // lama yang tidak mengenalnya tidak berubah perilakunya.
    assert_eq!(ProfileConfig::default().compact_model, "");
}
