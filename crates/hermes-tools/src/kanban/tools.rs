//! Tool kanban Worker A: board CRUD + `kanban_create` + `kanban_show`.
//!
//! Nama, argumen, dan semantik mengikuti aslinya
//! (`tools/kanban_tools_schemas.py`); board CRUD (`kanban_board_*`)
//! mencerminkan `kanban_db_boards.{create_board,list_boards,remove_board}`
//! yang di aslinya diekspos lewat CLI `hermes kanban boards`.

use super::store::{kanban_home, normalize_board_slug, BoardOptions, KanbanStore, NewTask};
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, Tool, ToolContext, ToolEffect, ToolResult};
use serde_json::{json, Value};

fn store_for(ctx: &ToolContext, tool: &str) -> Result<KanbanStore> {
    let home = kanban_home(&ctx.env, &ctx.working_dir);
    KanbanStore::open(home).map_err(|e| HermesError::tool(tool, e.to_string()))
}

/// Resolusi board ala `_DESC_BOARD` di skema aslinya: argumen `board` →
/// env `HERMES_KANBAN_BOARD` → board aktif (`current`) → `"default"`.
fn resolve_board(
    args: &Value,
    ctx: &ToolContext,
    store: &KanbanStore,
    tool: &str,
) -> Result<String> {
    if let Some(b) = opt_str(args, "board").filter(|s| !s.trim().is_empty()) {
        return normalize_board_slug(b).map_err(|e| HermesError::tool(tool, e.to_string()));
    }
    if let Some(b) = ctx
        .env
        .get("HERMES_KANBAN_BOARD")
        .filter(|s| !s.trim().is_empty())
    {
        return normalize_board_slug(b).map_err(|e| HermesError::tool(tool, e.to_string()));
    }
    store
        .current_board()
        .map_err(|e| HermesError::tool(tool, e.to_string()))
}

fn created_by(ctx: &ToolContext) -> String {
    ctx.env
        .get("HERMES_KANBAN_PROFILE")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "agent".to_string())
}

fn board_prop() -> Value {
    string_prop(
        "Slug board kanban target. Bila kosong dipakai HERMES_KANBAN_BOARD, \
         lalu board aktif, lalu 'default'.",
    )
}

fn opt_string_array(args: &Value, name: &str) -> Result<Vec<String>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                    HermesError::tool("args", format!("`{name}` harus array string"))
                })
            })
            .collect(),
        Some(_) => Err(HermesError::tool(
            "args",
            format!("`{name}` harus array string"),
        )),
    }
}

pub struct KanbanBoardCreateTool;

#[async_trait]
impl Tool for KanbanBoardCreateTool {
    fn name(&self) -> &str {
        "kanban_board_create"
    }
    fn description(&self) -> &str {
        "Membuat board kanban baru (semantik mkdir -p: board yang sudah ada \
         dikembalikan metadatanya). Cerminan `hermes kanban boards create` di aslinya."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                (
                    "slug",
                    string_prop("Slug board, mis. 'proyek-web' (huruf kecil, angka, -, _)."),
                ),
                ("name", string_prop("Nama tampilan board (default = slug).")),
                ("description", string_prop("Deskripsi board.")),
                ("icon", string_prop("Ikon/emoji board.")),
                ("color", string_prop("Warna aksen board.")),
            ],
            vec!["slug"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let slug = get_str(&args, "slug")?;
        let store = store_for(ctx, self.name())?;
        let meta = store
            .create_board(
                slug,
                BoardOptions {
                    name: opt_str(&args, "name").map(|s| s.to_string()),
                    description: opt_str(&args, "description").map(|s| s.to_string()),
                    icon: opt_str(&args, "icon").map(|s| s.to_string()),
                    color: opt_str(&args, "color").map(|s| s.to_string()),
                    project_id: None,
                },
            )
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        Ok(ok(
            "",
            self.name(),
            serde_json::to_string_pretty(&meta).unwrap_or_default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_board_list
// ---------------------------------------------------------------------------

pub struct KanbanBoardListTool;

#[async_trait]
impl Tool for KanbanBoardListTool {
    fn name(&self) -> &str {
        "kanban_board_list"
    }
    fn description(&self) -> &str {
        "Daftar metadata semua board kanban ('default' selalu pertama). \
         Cerminan `hermes kanban boards list` di aslinya."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![(
                "include_archived",
                boolean_prop("Sertakan board yang diarsipkan (default true)."),
            )],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let include_archived = opt_bool(&args, "include_archived").unwrap_or(true);
        let store = store_for(ctx, self.name())?;
        let boards = store
            .list_boards(include_archived)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        Ok(ok(
            "",
            self.name(),
            serde_json::to_string_pretty(&boards).unwrap_or_default(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_board_remove
// ---------------------------------------------------------------------------

pub struct KanbanBoardRemoveTool;

#[async_trait]
impl Tool for KanbanBoardRemoveTool {
    fn name(&self) -> &str {
        "kanban_board_remove"
    }
    fn description(&self) -> &str {
        "Menghapus board kanban. Secara default diarsipkan ke \
         `boards/_archived/<slug>-<ts>/`; set `archive=false` untuk hapus \
         permanen. Board 'default' tidak boleh dihapus."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("slug", string_prop("Slug board yang dihapus.")),
                (
                    "archive",
                    boolean_prop("Arsipkan alih-alih hapus permanen (default true)."),
                ),
            ],
            vec!["slug"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let slug = get_str(&args, "slug")?;
        let archive = opt_bool(&args, "archive").unwrap_or(true);
        let store = store_for(ctx, self.name())?;
        let meta = store
            .remove_board(slug, archive)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let action = if archive { "archived" } else { "deleted" };
        Ok(ok(
            "",
            self.name(),
            json!({"slug": meta.slug, "action": action}).to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_create — port langsung dari KANBAN_CREATE_SCHEMA aslinya
// ---------------------------------------------------------------------------

pub struct KanbanCreateTool;

#[async_trait]
impl Tool for KanbanCreateTool {
    fn name(&self) -> &str {
        "kanban_create"
    }
    fn description(&self) -> &str {
        "Membuat task kanban baru, opsional sebagai child dari task lain \
         (isi `parents`). Dipakai orchestrator untuk fan-out: uraikan kerja \
         menjadi child task dengan assignee spesifik. Status awal: `triage` \
         bila `triage=true`, `todo` bila ada parent yang belum done, \
         `blocked` bila `initial_status=blocked`, selain itu `running`."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("title", string_prop("Judul singkat task (wajib).")),
                (
                    "assignee",
                    string_prop(
                        "Nama profil yang mengerjakan task (mis. 'researcher-a'). \
                         Wajib — task tanpa assignee tidak pernah di-dispatch.",
                    ),
                ),
                (
                    "body",
                    string_prop("Opening post: spek lengkap, kriteria terima, tautan."),
                ),
                (
                    "parents",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Id task parent. Task baru tetap di 'todo' \
                           hingga semua parent 'done', lalu auto-promote ke 'ready'."}),
                ),
                (
                    "tenant",
                    string_prop("Namespace opsional untuk isolasi multi-proyek."),
                ),
                (
                    "priority",
                    integer_prop("Tiebreaker dispatcher; lebih tinggi = diambil lebih dulu."),
                ),
                (
                    "workspace_kind",
                    json!({"type": "string", "enum": ["scratch", "dir", "worktree"],
                           "description": "Rasa workspace: 'scratch' (default), 'dir' \
                           (butuh workspace_path absolut), 'worktree' (git worktree)."}),
                ),
                (
                    "workspace_path",
                    string_prop("Path absolut untuk workspace 'dir'/'worktree'."),
                ),
                (
                    "project",
                    string_prop("Id/slug proyek untuk menautkan task."),
                ),
                (
                    "triage",
                    boolean_prop("True: task mendarat di 'triage', bukan jalur dispatch biasa."),
                ),
                (
                    "idempotency_key",
                    string_prop("Bila task non-arsip dengan kunci ini sudah ada, \
                     kembalikan id-nya alih-alih membuat duplikat."),
                ),
                (
                    "max_runtime_seconds",
                    integer_prop("Batas runtime per task."),
                ),
                (
                    "initial_status",
                    json!({"type": "string", "enum": ["running", "blocked"],
                           "description": "Status awal kartu. 'blocked' untuk task yang \
                           butuh human ops segera."}),
                ),
                (
                    "skills",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Nama skill yang di-force-load ke worker yang di-dispatch."}),
                ),
                (
                    "goal_mode",
                    boolean_prop("Jalankan worker dalam goal loop (default false)."),
                ),
                (
                    "completion_contract",
                    string_prop("Kontrak publikasi: local-only (default), OWNER/REPO, atau URL PR GitHub."),
                ),
                (
                    "goal_max_turns",
                    integer_prop("Budget turn untuk goal_mode (default 20)."),
                ),
                (
                    "model",
                    string_prop("Pin worker ke model ini (nama persis yang diharapkan provider)."),
                ),
                (
                    "provider",
                    string_prop("Provider pemilik 'model' (mis. 'openrouter', 'anthropic'). \
                     Wajib diisi bila model bukan dari provider default profil; membutuhkan 'model'."),
                ),
                ("board", board_prop()),
            ],
            vec!["title", "assignee"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let store = store_for(ctx, self.name())?;
        let board = resolve_board(&args, ctx, &store, self.name())?;
        let db = store
            .board_db(&board)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let new_task = NewTask {
            title: get_str(&args, "title")?.to_string(),
            assignee: get_str(&args, "assignee")?.to_string(),
            body: opt_str(&args, "body").map(|s| s.to_string()),
            parents: opt_string_array(&args, "parents")?,
            tenant: opt_str(&args, "tenant").map(|s| s.to_string()),
            priority: opt_i64(&args, "priority").unwrap_or(0),
            workspace_kind: opt_str(&args, "workspace_kind").map(|s| s.to_string()),
            workspace_path: opt_str(&args, "workspace_path").map(|s| s.to_string()),
            project_id: opt_str(&args, "project").map(|s| s.to_string()),
            triage: opt_bool(&args, "triage").unwrap_or(false),
            idempotency_key: opt_str(&args, "idempotency_key").map(|s| s.to_string()),
            max_runtime_seconds: opt_i64(&args, "max_runtime_seconds"),
            initial_status: opt_str(&args, "initial_status").map(|s| s.to_string()),
            skills: opt_string_array(&args, "skills")?,
            goal_mode: opt_bool(&args, "goal_mode").unwrap_or(false),
            goal_max_turns: opt_i64(&args, "goal_max_turns"),
            model: opt_str(&args, "model").map(|s| s.to_string()),
            provider: opt_str(&args, "provider").map(|s| s.to_string()),
            completion_contract: opt_str(&args, "completion_contract").map(|s| s.to_string()),
            created_by: Some(created_by(ctx)),
            session_id: None,
        };
        let task = db
            .create_task(new_task)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        Ok(ok(
            "",
            self.name(),
            json!({
                "id": task.id,
                "title": task.title,
                "status": task.status,
                "assignee": task.assignee,
                "board": board,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_show — port langsung dari KANBAN_SHOW_SCHEMA aslinya
// ---------------------------------------------------------------------------

pub struct KanbanShowTool;

#[async_trait]
impl Tool for KanbanShowTool {
    fn name(&self) -> &str {
        "kanban_show"
    }
    fn description(&self) -> &str {
        "Baca state lengkap sebuah task: judul, body, assignee, status, \
         parent/child, komentar, dan event-event terakhir. Respons memuat \
         string `worker_context` yang siap disisipkan verbatim ke penalaran."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                (
                    "task_id",
                    string_prop(
                        "Id task. Bila kosong dipakai HERMES_KANBAN_TASK dari env \
                         (task milik worker yang di-spawn dispatcher).",
                    ),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let task_id = match opt_str(&args, "task_id").filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => ctx
                .env
                .get("HERMES_KANBAN_TASK")
                .filter(|s| !s.is_empty())
                .cloned()
                .ok_or_else(|| {
                    HermesError::tool(
                        self.name(),
                        "task_id wajib diisi (atau set HERMES_KANBAN_TASK); \
                         pakai kanban_board_list/kanban_create untuk menemukan id",
                    )
                })?,
        };
        let store = store_for(ctx, self.name())?;
        let board = resolve_board(&args, ctx, &store, self.name())?;
        let db = store
            .board_db(&board)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let task = db
            .get_task(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?
            .ok_or_else(|| HermesError::tool(self.name(), format!("task '{task_id}' tidak ada")))?;
        let parents = db
            .parents_of(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let children = db
            .children_of(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let comments = db
            .list_comments(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let events = db
            .list_events(&task_id, 20)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let attachments = db
            .list_attachments(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        let worker_context = build_worker_context(&task, &parents, &children, &comments, &events);
        Ok(ok(
            "",
            self.name(),
            json!({
                "task": task,
                "board": board,
                "parents": parents,
                "children": children,
                "comments": comments,
                "recent_events": events,
                "attachments": attachments,
                "worker_context": worker_context,
            })
            .to_string(),
        ))
    }
}

fn build_worker_context(
    task: &super::store::KanbanTask,
    parents: &[String],
    children: &[String],
    comments: &[super::store::Comment],
    events: &[super::store::TaskEvent],
) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let _ = writeln!(s, "Task {}: {} [{}]", task.id, task.title, task.status);
    if let Some(a) = &task.assignee {
        let _ = writeln!(s, "Assignee: {a}");
    }
    if let Some(b) = &task.body {
        let _ = writeln!(s, "Body:\n{b}");
    }
    if !parents.is_empty() {
        let _ = writeln!(s, "Parents: {}", parents.join(", "));
    }
    if !children.is_empty() {
        let _ = writeln!(s, "Children: {}", children.join(", "));
    }
    if let Some(sum) = &task.summary {
        let _ = writeln!(s, "Last summary: {sum}");
    }
    if !comments.is_empty() {
        let _ = writeln!(s, "Comments ({}):", comments.len());
        for c in comments.iter().take(10) {
            let _ = writeln!(
                s,
                "- [{}] {}: {}",
                c.id,
                c.author,
                truncate_one(&c.body, 300)
            );
        }
    }
    if !events.is_empty() {
        let _ = writeln!(s, "Recent events:");
        for e in events.iter().take(10) {
            let _ = writeln!(s, "- {} {}", e.kind, e.payload.as_deref().unwrap_or(""));
        }
    }
    s
}

fn truncate_one(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_ctx;
    use serde_json::json;

    fn kanban_ctx(tag: &str) -> ToolContext {
        let dir = std::env::temp_dir().join(format!(
            "hermes-kanban-tool-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut ctx = test_ctx(dir.clone());
        ctx.env.insert(
            "HERMES_KANBAN_HOME".to_string(),
            dir.join("kanban").to_string_lossy().into_owned(),
        );
        ctx
    }

    async fn run(tool: &impl Tool, args: Value, ctx: &ToolContext) -> Value {
        let res = tool.execute(args, ctx).await.unwrap();
        assert!(!res.is_error, "tool error: {}", res.content);
        serde_json::from_str(&res.content).unwrap_or(json!({"raw": res.content}))
    }

    #[tokio::test]
    async fn board_tools_roundtrip() {
        let ctx = kanban_ctx("board-tools");
        let create = KanbanBoardCreateTool;
        let out = run(
            &create,
            json!({"slug": "web", "name": "Web", "description": "board web"}),
            &ctx,
        )
        .await;
        assert_eq!(out["slug"], "web");
        assert_eq!(out["name"], "Web");

        // Idempoten: buat ulang slug yang sama.
        let out2 = run(&create, json!({"slug": "web"}), &ctx).await;
        assert_eq!(out2["slug"], "web");

        let list = KanbanBoardListTool;
        let out = run(&list, json!({}), &ctx).await;
        let slugs: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["slug"].as_str().unwrap())
            .collect();
        assert_eq!(slugs[0], "default");
        assert!(slugs.contains(&"web"));

        // Slug tidak valid ditolak.
        let err = create.execute(json!({"slug": "tidak valid!"}), &ctx).await;
        assert!(err.is_err());

        // Hapus board: default dilindungi.
        let remove = KanbanBoardRemoveTool;
        assert!(remove
            .execute(json!({"slug": "default"}), &ctx)
            .await
            .is_err());
        let out = run(&remove, json!({"slug": "web"}), &ctx).await;
        assert_eq!(out["action"], "archived");
        let out = run(&list, json!({"include_archived": false}), &ctx).await;
        let slugs: Vec<&str> = out
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["slug"].as_str().unwrap())
            .collect();
        assert!(!slugs.contains(&"web"));
    }

    #[tokio::test]
    async fn kanban_create_and_show() {
        let ctx = kanban_ctx("create-show");
        let create = KanbanCreateTool;
        let show = KanbanShowTool;

        // Argumen wajib.
        assert!(create
            .execute(json!({"title": "tanpa assignee"}), &ctx)
            .await
            .is_err());

        let parent = run(
            &create,
            json!({"title": "riset", "assignee": "researcher-a", "priority": 5,
                   "body": "kumpulkan data", "tenant": "t1"}),
            &ctx,
        )
        .await;
        let pid = parent["id"].as_str().unwrap().to_string();
        assert!(pid.starts_with("t_"));
        assert_eq!(parent["status"], "running");

        // Child dengan parent → todo.
        let child = run(
            &create,
            json!({"title": "sintesis", "assignee": "writer",
                   "parents": [pid], "idempotency_key": "sintesis-1"}),
            &ctx,
        )
        .await;
        assert_eq!(child["status"], "todo");
        let cid = child["id"].as_str().unwrap().to_string();

        // Idempotency: kunci sama → id sama.
        let child2 = run(
            &create,
            json!({"title": "sintesis", "assignee": "writer",
                   "parents": [pid], "idempotency_key": "sintesis-1"}),
            &ctx,
        )
        .await;
        assert_eq!(child2["id"].as_str().unwrap(), cid);

        // kanban_show memuat state lengkap + worker_context.
        let shown = run(&show, json!({"task_id": cid}), &ctx).await;
        assert_eq!(shown["task"]["title"], "sintesis");
        assert_eq!(shown["parents"].as_array().unwrap().len(), 1);
        assert!(shown["worker_context"]
            .as_str()
            .unwrap()
            .contains("sintesis"));

        // Task fiktif → error.
        assert!(show
            .execute(json!({"task_id": "t_tidakada"}), &ctx)
            .await
            .is_err());

        // Board override via argumen.
        let other = run(
            &create,
            json!({"title": "di board lain", "assignee": "w", "board": "lain"}),
            &ctx,
        )
        .await;
        assert_eq!(other["board"], "lain");
        let shown = run(
            &show,
            json!({"task_id": other["id"], "board": "lain"}),
            &ctx,
        )
        .await;
        assert_eq!(shown["task"]["title"], "di board lain");
    }

    #[tokio::test]
    async fn kanban_create_validates_workspace_and_provider() {
        let ctx = kanban_ctx("create-valid");
        let create = KanbanCreateTool;
        // workspace_kind fiktif.
        assert!(create
            .execute(
                json!({"title": "x", "assignee": "w", "workspace_kind": "awan"}),
                &ctx
            )
            .await
            .is_err());
        // provider tanpa model.
        assert!(create
            .execute(
                json!({"title": "x", "assignee": "w", "provider": "anthropic"}),
                &ctx
            )
            .await
            .is_err());
        // triage → triage.
        let out = run(
            &create,
            json!({"title": "spek dulu", "assignee": "w", "triage": true}),
            &ctx,
        )
        .await;
        assert_eq!(out["status"], "triage");
    }

    #[tokio::test]
    async fn show_defaults_to_env_task() {
        let mut ctx = kanban_ctx("show-env");
        let create = KanbanCreateTool;
        let show = KanbanShowTool;
        let out = run(&create, json!({"title": "env task", "assignee": "w"}), &ctx).await;
        let id = out["id"].as_str().unwrap().to_string();
        ctx.env.insert("HERMES_KANBAN_TASK".to_string(), id.clone());
        // Tanpa task_id eksplisit → ambil dari env.
        let shown = run(&show, json!({}), &ctx).await;
        assert_eq!(shown["task"]["id"].as_str().unwrap(), id);
    }

    #[tokio::test]
    async fn home_resolution_order() {
        use std::collections::HashMap;
        let wd = std::path::PathBuf::from("/tmp/wd-uji");
        // 1. HERMES_KANBAN_HOME menang.
        let mut env = HashMap::new();
        env.insert("HERMES_KANBAN_HOME".to_string(), "/tmp/kh".to_string());
        env.insert("HERMES_HOME".to_string(), "/tmp/hh".to_string());
        assert_eq!(kanban_home(&env, &wd), std::path::PathBuf::from("/tmp/kh"));
        // 2. HERMES_HOME/kanban.
        env.remove("HERMES_KANBAN_HOME");
        assert_eq!(
            kanban_home(&env, &wd),
            std::path::PathBuf::from("/tmp/hh/kanban")
        );
        // 3. Fallback working_dir.
        env.remove("HERMES_HOME");
        assert_eq!(
            kanban_home(&env, &wd),
            std::path::PathBuf::from("/tmp/wd-uji/.hermes/kanban")
        );
    }
}
