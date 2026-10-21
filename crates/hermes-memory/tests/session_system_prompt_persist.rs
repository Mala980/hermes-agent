//! Uji integrasi W1-B6: system prompt dibekukan per sesi.
//!
//! - Turn pertama membangun system prompt dan menyimpannya (byte persis) ke
//!   kolom `system_prompt` tabel sessions.
//! - Sesi yang di-resume (instance agen baru, DB sama) memakai ulang byte
//!   persis tersebut — BUKAN membangun ulang — walau hasil recall memori
//!   sudah berubah (kasus yang membuat rebuild menghasilkan byte berbeda).

use async_trait::async_trait;
use hermes_core::{
    Agent, AgentDeps, AllowAllApproval, CancelFlag, ChatRequest, LlmProvider, MemoryConfig,
    MemoryEntry, MemoryStore, ProfileConfig, Result, Session, Skill, SkillStore, StreamEvent,
    ToolRegistry, Usage,
};
use hermes_memory::SqliteMemoryStore;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Provider mock: mencatat teks pesan system tiap pemanggilan chat_stream,
/// lalu menjawab teks biasa tanpa tool call.
struct CaptureProvider {
    systems: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl LlmProvider for CaptureProvider {
    fn name(&self) -> &str {
        "mock-capture"
    }

    fn default_model(&self) -> &str {
        "mock-model"
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let system = req.messages.first().map(|m| m.text()).unwrap_or_default();
        self.systems.lock().unwrap().push(system);
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(async move {
            let _ = tx.send(StreamEvent::TextDelta("ok".to_string())).await;
            let _ = tx.send(StreamEvent::Done(Usage::default())).await;
        });
        Ok(rx)
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

fn build_agent(
    provider: Arc<CaptureProvider>,
    memory: Arc<SqliteMemoryStore>,
    profile: ProfileConfig,
    hermes_home: std::path::PathBuf,
    session: Session,
) -> Agent {
    let (events, _rx) = mpsc::channel(64);
    Agent::new(
        AgentDeps {
            provider,
            tools: Arc::new(ToolRegistry::new()),
            memory,
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
async fn resume_restores_exact_system_prompt_bytes() {
    let dir = std::env::temp_dir().join(format!("hermes-sysprompt-agent-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let db_path = dir.join("memory.db");
    let _ = std::fs::remove_file(&db_path);

    let profile = ProfileConfig {
        learning_loop: false,
        working_dir: dir.to_string_lossy().into_owned(),
        context_files: vec![],
        ..ProfileConfig::default()
    };
    let sid = "sysprompt-persist-test";

    // --- Turn 1: sesi baru, satu memori cocok dengan recall. ---
    let memory = Arc::new(SqliteMemoryStore::open(db_path.clone()).unwrap());
    memory
        .remember(MemoryEntry::new("fact", "Chewy suka kopi tubruk"))
        .await
        .unwrap();
    let provider1 = Arc::new(CaptureProvider {
        systems: Arc::new(Mutex::new(Vec::new())),
    });
    let mut session = Session::new("default");
    session.id = sid.to_string();
    session.title = "tes-sysprompt".to_string(); // lewati maybe_generate_title
    let mut agent1 = build_agent(
        provider1.clone(),
        memory.clone(),
        profile.clone(),
        dir.clone(),
        session,
    );
    agent1.run_turn("kopi").await.unwrap();

    let first = {
        let systems1 = provider1.systems.lock().unwrap();
        assert_eq!(systems1.len(), 1, "turn 1 harus memanggil provider sekali");
        let first = systems1[0].clone();
        assert!(
            first.contains("Chewy suka kopi tubruk"),
            "prompt turn 1 harus memuat hasil recall"
        );
        first
    };

    // Byte prompt turn 1 tersimpan persis di DB.
    let saved = memory.load_session(sid).await.unwrap().unwrap();
    assert_eq!(
        saved.system_prompt,
        Some(first.clone()),
        "kolom system_prompt harus menyimpan byte persis hasil build pertama"
    );

    // --- Ubah hasil recall: rebuild sekarang AKAN menghasilkan byte berbeda. ---
    memory
        .remember(MemoryEntry::new("fact", "kopi susu gula aren juga nikmat"))
        .await
        .unwrap();
    let entries = memory.recall("kopi", 5).await.unwrap();
    assert_eq!(entries.len(), 2, "setup: recall harus berubah");
    let hint = entries
        .iter()
        .map(|e| format!("- [{}] {}", e.kind, e.text))
        .collect::<Vec<_>>()
        .join("\n");
    let rebuilt = hermes_core::context::build_system_message(sid, &profile, &[], "", &hint).text();
    assert_ne!(
        rebuilt, first,
        "setup: membangun ulang prompt dengan recall baru harus berbeda"
    );

    // --- Resume: instance agen + koneksi DB baru terhadap file yang sama. ---
    drop(agent1);
    let memory2 = Arc::new(SqliteMemoryStore::open(db_path.clone()).unwrap());
    let resumed = memory2.load_session(sid).await.unwrap().unwrap();
    assert_eq!(
        resumed.system_prompt,
        Some(first.clone()),
        "sesi resume harus membawa byte prompt beku"
    );
    let provider2 = Arc::new(CaptureProvider {
        systems: Arc::new(Mutex::new(Vec::new())),
    });
    let mut agent2 = build_agent(provider2.clone(), memory2.clone(), profile, dir, resumed);
    agent2.run_turn("kopi").await.unwrap();

    {
        let systems2 = provider2.systems.lock().unwrap();
        assert_eq!(
            systems2.len(),
            1,
            "turn resume harus memanggil provider sekali"
        );
        assert_eq!(
            systems2[0], first,
            "turn resume harus memakai byte system prompt PERSIS dari DB, bukan rebuild"
        );
    }

    // DB tetap menyimpan byte yang sama setelah turn resume.
    let saved2 = memory2.load_session(sid).await.unwrap().unwrap();
    assert_eq!(saved2.system_prompt, Some(first));

    let _ = std::fs::remove_file(&db_path);
}
