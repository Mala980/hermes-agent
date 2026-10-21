//! Test integrasi batch 8 (Worker G): `memory_patch`, pemisahan target
//! `memory`/`user`, dan empat bentuk `session_search` (discover/read/scroll/
//! browse) — memakai `SqliteMemoryStore` in-memory.

use hermes_core::{
    AllowAllApproval, CancelFlag, MemoryStore, MemoryTarget, Message, Session, Skill, SkillStore,
    Tool, ToolContext,
};
use hermes_memory::SqliteMemoryStore;
use hermes_tools::memory_tools::{MemoryPatchTool, MemoryRememberTool, SessionSearchTool};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

struct StubSkills;
#[async_trait::async_trait]
impl SkillStore for StubSkills {
    async fn list(&self) -> hermes_core::Result<Vec<Skill>> {
        Ok(vec![])
    }
    async fn get(&self, _n: &str) -> hermes_core::Result<Option<Skill>> {
        Ok(None)
    }
    async fn create(&self, _n: &str, _d: &str, _b: &str) -> hermes_core::Result<Skill> {
        unimplemented!()
    }
    async fn update(&self, _n: &str, _b: &str) -> hermes_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _n: &str) -> hermes_core::Result<()> {
        Ok(())
    }
    async fn find_relevant(&self, _t: &str, _l: usize) -> hermes_core::Result<Vec<Skill>> {
        Ok(vec![])
    }
}

fn test_ctx(store: Arc<SqliteMemoryStore>) -> ToolContext {
    let dir = std::env::temp_dir().join(format!(
        "hmem-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    ToolContext {
        working_dir: dir.clone(),
        session_cwd: Arc::new(Mutex::new(dir)),
        approval: Arc::new(AllowAllApproval),
        memory: store,
        skills: Arc::new(StubSkills),
        cancel: CancelFlag::new(),
        env: HashMap::new(),
        allow_outside_workdir: true,
        provider: None,
        model: "test".into(),
    }
}

fn new_store() -> Arc<SqliteMemoryStore> {
    Arc::new(SqliteMemoryStore::open_in_memory().unwrap())
}

#[tokio::test]
async fn memory_patch_tool_happy_path() {
    let store = new_store();
    let ctx = test_ctx(store.clone());

    MemoryRememberTool
        .execute(json!({"text": "warna favorit adalah biru langit"}), &ctx)
        .await
        .unwrap();

    let res = MemoryPatchTool
        .execute(
            json!({"old_text": "biru langit", "new_text": "hijau daun"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        res.content.contains("warna favorit adalah hijau daun"),
        "konten: {}",
        res.content
    );

    // Verifikasi di store: satu entri, teks sudah ter-patch.
    let entries = store
        .recall_for("hijau", 5, MemoryTarget::Memory)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "warna favorit adalah hijau daun");
}

#[tokio::test]
async fn memory_patch_tool_rejects_ambiguous() {
    let store = new_store();
    let ctx = test_ctx(store);
    MemoryRememberTool
        .execute(json!({"text": "apel merah dan apel hijau"}), &ctx)
        .await
        .unwrap();
    let err = MemoryPatchTool
        .execute(json!({"old_text": "apel", "new_text": "jeruk"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("2 kali"), "dapat: {err}");
}

#[tokio::test]
async fn memory_remember_respects_target() {
    let store = new_store();
    let ctx = test_ctx(store.clone());

    MemoryRememberTool
        .execute(
            json!({"text": "pengguna alergi kacang", "target": "user"}),
            &ctx,
        )
        .await
        .unwrap();
    MemoryRememberTool
        .execute(json!({"text": "agen berjalan di linux"}), &ctx)
        .await
        .unwrap();

    // Target user terisolasi dari target memory.
    let user = store
        .recall_for("alergi", 5, MemoryTarget::User)
        .await
        .unwrap();
    assert_eq!(user.len(), 1);
    let mem = store
        .recall_for("alergi", 5, MemoryTarget::Memory)
        .await
        .unwrap();
    assert!(mem.is_empty());

    // Target tidak valid ditolak dengan pesan jelas.
    let err = MemoryRememberTool
        .execute(json!({"text": "x", "target": "bogus"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tidak dikenal"), "dapat: {err}");
}

async fn save_session_with_messages(
    store: &SqliteMemoryStore,
    title: &str,
    texts: &[&str],
) -> String {
    let mut s = Session::new("default");
    s.title = title.to_string();
    for (i, t) in texts.iter().enumerate() {
        if i % 2 == 0 {
            s.messages.push(Message::user(*t));
        } else {
            s.messages.push(Message::assistant_text(*t));
        }
    }
    store.save_session(&s).await.unwrap();
    s.id.clone()
}

#[tokio::test]
async fn session_search_discover() {
    let store = new_store();
    let ctx = test_ctx(store.clone());
    save_session_with_messages(
        &store,
        "Rencana liburan",
        &["mau liburan ke bali bulan depan", "oke, catat bali"],
    )
    .await;
    save_session_with_messages(&store, "Lainnya", &["resep rendang ayam", "oke"]).await;

    let res = SessionSearchTool
        .execute(json!({"query": "bali liburan"}), &ctx)
        .await
        .unwrap();
    assert!(
        res.content.contains("mode=discover"),
        "konten: {}",
        res.content
    );
    assert!(
        res.content.contains("Rencana liburan"),
        "konten: {}",
        res.content
    );
}

#[tokio::test]
async fn session_search_read_and_scroll() {
    let store = new_store();
    let ctx = test_ctx(store.clone());
    let texts: Vec<String> = (0..10).map(|i| format!("pesan nomor {i}")).collect();
    let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let sid = save_session_with_messages(&store, "Sesi panjang", &refs).await;

    // Read: seluruh pesan (10 <= 30, tanpa pemotongan head/tail).
    let res = SessionSearchTool
        .execute(json!({"session_id": sid}), &ctx)
        .await
        .unwrap();
    assert!(res.content.contains("mode=read"), "konten: {}", res.content);
    assert!(
        res.content.contains("pesan nomor 0"),
        "konten: {}",
        res.content
    );
    assert!(
        res.content.contains("pesan nomor 9"),
        "konten: {}",
        res.content
    );

    // Scroll: jendela ±2 di sekitar indeks 5 -> indeks 3..=7, jangkar ditandai.
    let res = SessionSearchTool
        .execute(json!({"session_id": sid, "around": 5, "window": 2}), &ctx)
        .await
        .unwrap();
    assert!(
        res.content.contains("mode=scroll"),
        "konten: {}",
        res.content
    );
    assert!(
        res.content.contains("[5]") && res.content.contains("anchor"),
        "konten: {}",
        res.content
    );
    assert!(
        !res.content.contains("pesan nomor 0"),
        "konten: {}",
        res.content
    );
    assert!(
        !res.content.contains("pesan nomor 9"),
        "konten: {}",
        res.content
    );

    // Jangkar di luar jangkauan -> error.
    let err = SessionSearchTool
        .execute(json!({"session_id": sid, "around": 99}), &ctx)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("di luar jangkauan"),
        "dapat: {err}"
    );

    // Sesi tidak ada -> error.
    let err = SessionSearchTool
        .execute(json!({"session_id": "tidak-ada"}), &ctx)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tidak ditemukan"), "dapat: {err}");
}

#[tokio::test]
async fn session_search_browse() {
    let store = new_store();
    let ctx = test_ctx(store.clone());
    save_session_with_messages(&store, "Sesi A", &["halo a"]).await;
    save_session_with_messages(&store, "Sesi B", &["halo b"]).await;

    // Tanpa argumen -> browse.
    let res = SessionSearchTool.execute(json!({}), &ctx).await.unwrap();
    assert!(
        res.content.contains("mode=browse"),
        "konten: {}",
        res.content
    );
    assert!(res.content.contains("Sesi A"), "konten: {}", res.content);
    assert!(res.content.contains("Sesi B"), "konten: {}", res.content);
}

#[tokio::test]
async fn session_search_query_optional_for_browse() {
    // Skema: query tidak lagi wajib (browse tanpa query).
    let schema = SessionSearchTool.parameters_schema();
    let required = schema
        .get("required")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        !required.iter().any(|v| v == "query"),
        "query harus opsional, dapat: {required:?}"
    );
    for param in ["session_id", "around", "window", "limit", "profile"] {
        assert!(
            schema["properties"].get(param).is_some(),
            "skema harus punya param {param}"
        );
    }
}
