//! Test integrasi tool kanban Worker B (`kanban_list`, `kanban_complete`,
//! `kanban_block`, `kanban_unblock`, `kanban_schedule`, `kanban_heartbeat`).
//!
//! Menjalankan siklus hidup penuh lewat trait `Tool` + registry publik,
//! memakai board SQLite sungguhan di direktori temp (`HERMES_KANBAN_HOME`).
//! `kanban_create` Worker A dipakai untuk menyiapkan task agar alur yang
//! diuji sama dengan pemakaian nyata.

use hermes_core::{
    AllowAllApproval, CancelFlag, MemoryEntry, MemoryStore, Result, Session, Skill, SkillStore,
    ToolContext, ToolRegistry,
};
use hermes_tools::kanban::tools::KanbanCreateTool;
use hermes_tools::kanban::tools_queries::register_query_tools;
use serde_json::{json, Value};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Stub ToolContext (pola sama seperti tests/mock_integrations.rs)
// ---------------------------------------------------------------------------

struct StubMemory;
#[async_trait::async_trait]
impl MemoryStore for StubMemory {
    async fn remember(&self, _e: MemoryEntry) -> Result<()> {
        Ok(())
    }
    async fn recall(&self, _q: &str, _l: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn forget(&self, _q: &str) -> Result<usize> {
        Ok(0)
    }
    async fn list_memories(&self, _l: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn save_session(&self, _s: &Session) -> Result<()> {
        Ok(())
    }
    async fn load_session(&self, _id: &str) -> Result<Option<Session>> {
        Ok(None)
    }
    async fn list_sessions(&self, _p: &str, _l: usize) -> Result<Vec<Session>> {
        Ok(vec![])
    }
    async fn delete_session(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn search_sessions(&self, _q: &str, _l: usize) -> Result<Vec<Session>> {
        Ok(vec![])
    }
}

struct StubSkills;
#[async_trait::async_trait]
impl SkillStore for StubSkills {
    async fn list(&self) -> Result<Vec<Skill>> {
        Ok(vec![])
    }
    async fn get(&self, _n: &str) -> Result<Option<Skill>> {
        Ok(None)
    }
    async fn create(&self, n: &str, d: &str, b: &str) -> Result<Skill> {
        Ok(Skill {
            name: n.into(),
            description: d.into(),
            body: b.into(),
            path: Default::default(),
            triggers: vec![],
        })
    }
    async fn update(&self, _n: &str, _b: &str) -> Result<()> {
        Ok(())
    }
    async fn delete(&self, _n: &str) -> Result<()> {
        Ok(())
    }
    async fn find_relevant(&self, _t: &str, _l: usize) -> Result<Vec<Skill>> {
        Ok(vec![])
    }
}

fn test_ctx(tag: &str) -> ToolContext {
    let dir = std::env::temp_dir().join(format!(
        "hermes-kanban-it-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut env = std::collections::HashMap::new();
    env.insert(
        "HERMES_KANBAN_HOME".to_string(),
        dir.join("kanban").to_string_lossy().into_owned(),
    );
    ToolContext {
        working_dir: dir.clone(),
        session_cwd: Arc::new(std::sync::Mutex::new(dir)),
        approval: Arc::new(AllowAllApproval),
        memory: Arc::new(StubMemory),
        skills: Arc::new(StubSkills),
        cancel: CancelFlag::new(),
        env,
        allow_outside_workdir: true,
        provider: None,
        model: "test".into(),
    }
}

fn registry() -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    reg.register(KanbanCreateTool);
    register_query_tools(&mut reg);
    reg
}

async fn call(reg: &ToolRegistry, name: &str, args: Value, ctx: &ToolContext) -> Value {
    let tool = reg
        .get(name)
        .unwrap_or_else(|| panic!("tool {name} tidak terdaftar"));
    let res = tool.execute(args, ctx).await.expect("tool gagal");
    assert!(!res.is_error, "{name} error: {}", res.content);
    serde_json::from_str(&res.content).unwrap_or(json!({"raw": res.content}))
}

fn task_ids(tasks: &Value) -> Vec<String> {
    tasks
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn lifecycle_block_unblock_schedule_complete() {
    let reg = registry();
    let ctx = test_ctx("lifecycle");

    // Buat parent + child via kanban_create (Worker A).
    let parent = call(
        &reg,
        "kanban_create",
        json!({"title": "riset integrasi", "assignee": "researcher"}),
        &ctx,
    )
    .await;
    let pid = parent["id"].as_str().unwrap().to_string();
    let child = call(
        &reg,
        "kanban_create",
        json!({"title": "tulis integrasi", "assignee": "writer", "parents": [pid]}),
        &ctx,
    )
    .await;
    let cid = child["id"].as_str().unwrap().to_string();

    // kanban_list melihat keduanya; child terpromosi? belum — parent masih running.
    let listed = call(&reg, "kanban_list", json!({}), &ctx).await;
    assert_eq!(listed["promoted"], 0);
    let ids = task_ids(&listed["tasks"]);
    assert!(ids.contains(&pid) && ids.contains(&cid));

    // Block parent yang running (needs_input) → blocked.
    let blocked = call(
        &reg,
        "kanban_block",
        json!({"task_id": pid, "reason": "butuh arahan", "kind": "needs_input"}),
        &ctx,
    )
    .await;
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["block_kind"], "needs_input");

    // Unblock → ready karena tidak punya parent.
    let unblocked = call(&reg, "kanban_unblock", json!({"task_id": pid}), &ctx).await;
    assert_eq!(unblocked["status"], "ready");

    // Schedule parent → scheduled; list dengan filter status melihatnya.
    let scheduled = call(
        &reg,
        "kanban_schedule",
        json!({"task_id": pid, "reason": "tunggu slot"}),
        &ctx,
    )
    .await;
    assert_eq!(scheduled["status"], "scheduled");
    let listed = call(&reg, "kanban_list", json!({"status": "scheduled"}), &ctx).await;
    assert_eq!(task_ids(&listed["tasks"]), vec![pid.clone()]);

    // Unblock parent yang di-schedule → ready (tanpa parent).
    let unblocked = call(&reg, "kanban_unblock", json!({"task_id": pid}), &ctx).await;
    assert_eq!(unblocked["status"], "ready");

    // Heartbeat di parent.
    let hb = call(
        &reg,
        "kanban_heartbeat",
        json!({"task_id": pid, "note": "jalan"}),
        &ctx,
    )
    .await;
    assert_eq!(hb["task_id"], pid);

    // Complete parent → done; child (todo, parent kini done) terpromosi ke ready
    // dan terlihat di kanban_list berikutnya.
    let done = call(
        &reg,
        "kanban_complete",
        json!({"task_id": pid, "summary": "riset selesai"}),
        &ctx,
    )
    .await;
    assert_eq!(done["status"], "done");
    assert_eq!(done["promoted_children"][0], cid);
    // kanban_complete sudah mempromosikan child (cerminan recompute_ready di
    // complete_task aslinya), jadi list berikutnya tak ada yang dipromosikan.
    let listed = call(&reg, "kanban_list", json!({}), &ctx).await;
    assert_eq!(listed["promoted"], 0);
    let ready = call(&reg, "kanban_list", json!({"status": "ready"}), &ctx).await;
    assert!(task_ids(&ready["tasks"]).contains(&cid));

    // Child kini ready: block dependency tanpa parent terbuka → re-kind
    // needs_input (sticky), lalu unblock → ready lagi.
    let blocked = call(
        &reg,
        "kanban_block",
        json!({"task_id": cid, "reason": "tunggu data", "kind": "dependency"}),
        &ctx,
    )
    .await;
    assert_eq!(blocked["status"], "blocked");
    assert_eq!(blocked["block_kind"], "needs_input");
    let unblocked = call(&reg, "kanban_unblock", json!({"task_id": cid}), &ctx).await;
    assert_eq!(unblocked["status"], "ready");

    // Complete child → done.
    let done = call(
        &reg,
        "kanban_complete",
        json!({"task_id": cid, "summary": "tulis selesai"}),
        &ctx,
    )
    .await;
    assert_eq!(done["status"], "done");
}

#[tokio::test]
async fn complete_rejects_phantom_cards_and_missing_artifacts() {
    let reg = registry();
    let ctx = test_ctx("guards");

    let created = call(
        &reg,
        "kanban_create",
        json!({"title": "tugas guard", "assignee": "w"}),
        &ctx,
    )
    .await;
    let tid = created["id"].as_str().unwrap();

    // created_cards phantom → tool error, task tetap running.
    let tool = reg.get("kanban_complete").unwrap();
    let err = tool
        .execute(
            json!({"task_id": tid, "summary": "ok", "created_cards": ["t_phantom1"]}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("t_phantom1"));

    // Artifact hilang → tool error.
    let err = tool
        .execute(
            json!({"task_id": tid, "summary": "ok",
                   "artifacts": ["/tmp/hermes-it-tidak-ada.pdf"]}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("tidak bisa mengawetkan"));

    // Status masih running (in-flight).
    let listed = call(&reg, "kanban_list", json!({"status": "running"}), &ctx).await;
    assert!(task_ids(&listed["tasks"]).contains(&tid.to_string()));
}

#[tokio::test]
async fn list_truncation_metadata() {
    let reg = registry();
    let ctx = test_ctx("truncation");

    for i in 0..3 {
        call(
            &reg,
            "kanban_create",
            json!({"title": format!("tugas {i}"), "assignee": "w"}),
            &ctx,
        )
        .await;
    }
    let out = call(&reg, "kanban_list", json!({"limit": 2}), &ctx).await;
    assert_eq!(out["count"], 2);
    assert_eq!(out["limit"], 2);
    assert_eq!(out["truncated"], true);
    assert_eq!(out["next_limit"], 4);
    assert_eq!(out["tasks"].as_array().unwrap().len(), 2);

    let out = call(&reg, "kanban_list", json!({"limit": 200}), &ctx).await;
    assert_eq!(out["truncated"], false);
    assert!(out["next_limit"].is_null());
}
