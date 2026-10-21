//! Test integrasi tool kanban Worker C (`kanban_request_review`,
//! `kanban_request_changes`, `kanban_comment`, `kanban_attach`,
//! `kanban_attach_url`, `kanban_attachments`, `kanban_link`).
//!
//! Menjalankan siklus review penuh lewat trait `Tool` + registry publik,
//! memakai board SQLite sungguhan di direktori temp (`HERMES_KANBAN_HOME`).
//! `kanban_create` Worker A dipakai untuk menyiapkan task agar alur yang
//! diuji sama dengan pemakaian nyata.

use hermes_core::{
    AllowAllApproval, CancelFlag, MemoryEntry, MemoryStore, Result, Session, Skill, SkillStore,
    ToolContext, ToolRegistry,
};
use hermes_tools::kanban::tools::KanbanCreateTool;
use hermes_tools::kanban::tools_meta::register_meta_tools;
use serde_json::{json, Value};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Stub ToolContext (pola sama seperti tests/kanban_query_tools.rs)
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
        "hermes-kanban-meta-it-{tag}-{}-{}",
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
    register_meta_tools(&mut reg);
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

async fn create_task(reg: &ToolRegistry, ctx: &ToolContext, title: &str, assignee: &str) -> String {
    call(
        reg,
        "kanban_create",
        json!({"title": title, "assignee": assignee}),
        ctx,
    )
    .await["id"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// Siklus review penuh: request_review → request_changes → request_review lagi
// ---------------------------------------------------------------------------

#[tokio::test]
async fn review_cycle_end_to_end() {
    let reg = registry();
    let ctx = test_ctx("review-cycle");
    let tid = create_task(&reg, &ctx, "tulis parser", "dev-a").await;

    // Implementer menyerahkan untuk review, dengan artifact + metadata.
    let art_dir =
        std::env::temp_dir().join(format!("hermes-kanban-meta-it-art-{}", std::process::id()));
    std::fs::create_dir_all(&art_dir).unwrap();
    let art = art_dir.join("parser.png");
    std::fs::write(&art, b"\x89PNG data").unwrap();
    let out = call(
        &reg,
        "kanban_request_review",
        json!({"task_id": tid, "summary": "parser selesai, 20 tes hijau",
               "reviewer": "rev-b",
               "metadata": {"tests_run": 20},
               "artifacts": [art.to_string_lossy()]}),
        &ctx,
    )
    .await;
    assert_eq!(out["status"], "review");
    assert_eq!(out["implementer"], "dev-a");
    assert_eq!(out["reviewer"], "rev-b");
    assert_eq!(out["artifacts_staged"][0], "parser.png");

    // Reviewer meminta perubahan → kembali ke implementer, landing `ready`.
    let out = call(
        &reg,
        "kanban_request_changes",
        json!({"task_id": tid, "reason": "tangani input kosong"}),
        &ctx,
    )
    .await;
    assert_eq!(out["status"], "ready");
    assert_eq!(out["implementer"], "dev-a");

    // Implementer meminta review ulang tanpa reviewer eksplisit.
    let out = call(
        &reg,
        "kanban_request_review",
        json!({"task_id": tid, "summary": "input kosong ditangani"}),
        &ctx,
    )
    .await;
    assert_eq!(out["status"], "review");
    // Assignee tetap implementer bila reviewer tidak diisi.
    assert_eq!(out["reviewer"], Value::Null);
}

// ---------------------------------------------------------------------------
// Komentar + attachment + daftar attachment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn comment_attach_and_list() {
    let reg = registry();
    let mut ctx = test_ctx("comment-attach");
    ctx.env
        .insert("HERMES_KANBAN_PROFILE".to_string(), "penulis".to_string());
    let tid = create_task(&reg, &ctx, "riset pasar", "analis").await;

    let out = call(
        &reg,
        "kanban_comment",
        json!({"task_id": tid, "body": "temuan awal: pasar tumbuh 12%"}),
        &ctx,
    )
    .await;
    assert_eq!(out["author"], "penulis");
    assert!(out["comment_id"].as_i64().unwrap() > 0);

    // Lampirkan bytes inline (base64).
    let b64 = base64_simple_encode(b"laporan-kuartal");
    let out = call(
        &reg,
        "kanban_attach",
        json!({"task_id": tid, "filename": "laporan.txt",
               "content_base64": b64, "content_type": "text/plain"}),
        &ctx,
    )
    .await;
    assert_eq!(out["filename"], "laporan.txt");
    assert_eq!(out["size"], 15);

    // Daftar attachment memuat field yang dijanjikan skema aslinya.
    let out = call(&reg, "kanban_attachments", json!({"task_id": tid}), &ctx).await;
    let atts = out["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0]["filename"], "laporan.txt");
    assert_eq!(atts[0]["content_type"], "text/plain");
    assert_eq!(atts[0]["size"], 15);
    assert_eq!(atts[0]["uploaded_by"], "penulis");
    assert!(atts[0]["stored_path"].as_str().unwrap().contains(&tid));
    // Isi file di disk cocok.
    let bytes = std::fs::read(atts[0]["stored_path"].as_str().unwrap()).unwrap();
    assert_eq!(bytes, b"laporan-kuartal");
}

/// Encode base64 standar tanpa crate tambahan (hanya untuk test).
fn base64_simple_encode(data: &[u8]) -> String {
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let mut n: u32 = 0;
        for (i, b) in chunk.iter().enumerate() {
            n |= (*b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        for i in 0..4 - pad {
            out.push(ALPH[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Link dependensi: edge, self-link, dan siklus
// ---------------------------------------------------------------------------

#[tokio::test]
async fn link_dependency_edge() {
    let reg = registry();
    let ctx = test_ctx("link");
    let parent = create_task(&reg, &ctx, "fondasi", "dev-a").await;
    let child = create_task(&reg, &ctx, "atap", "dev-b").await;

    let out = call(
        &reg,
        "kanban_link",
        json!({"parent_id": parent, "child_id": child}),
        &ctx,
    )
    .await;
    assert_eq!(out["parent_id"], parent);
    assert_eq!(out["child_id"], child);

    // Self-link ditolak.
    let tool = reg.get("kanban_link").unwrap();
    assert!(tool
        .execute(json!({"parent_id": parent, "child_id": parent}), &ctx)
        .await
        .is_err());
    // Siklus ditolak.
    assert!(tool
        .execute(json!({"parent_id": child, "child_id": parent}), &ctx)
        .await
        .is_err());
}

// ---------------------------------------------------------------------------
// attach_url: penolakan SSRF tanpa menyentuh jaringan
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attach_url_ssrf_rejections() {
    let reg = registry();
    let ctx = test_ctx("attach-url");
    let tid = create_task(&reg, &ctx, "tugas unduh", "dev-a").await;
    let tool = reg.get("kanban_attach_url").unwrap();

    for url in [
        "ftp://example.com/f.bin",
        "file:///etc/passwd",
        "http://127.0.0.1/f.bin",
        "http://localhost/f.bin",
        "http://10.0.0.5/f.bin",
    ] {
        let err = tool
            .execute(json!({"task_id": tid, "url": url}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("blocked") || err.to_string().contains("hanya URL http(s)"),
            "url {url} tidak ditolak: {err}"
        );
    }
}
