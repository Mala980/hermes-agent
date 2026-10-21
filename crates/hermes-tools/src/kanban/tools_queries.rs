//! Tool kanban Worker B: query ringkas + transisi siklus hidup task.
//!
//! Port langsung dari aslinya (`tools/kanban_tools.py`,
//! `tools/kanban_tools_schemas.py`):
//!
//! | Tool | Cerminan asli |
//! |------|---------------|
//! | `kanban_list` | `_handle_list` — ringkasan + filter CLI, recompute ready, paginasi |
//! | `kanban_complete` | `_handle_complete` — handoff terstruktur, cek `created_cards` & `artifacts` |
//! | `kanban_block` | `_handle_block` + `block_task`/`_route_block` — routing kind + loop-breaker |
//! | `kanban_unblock` | `_handle_unblock` + `unblock_task` — re-gate parent |
//! | `kanban_schedule` | `_handle_schedule` + `schedule_task` — parkir ke `scheduled` |
//! | `kanban_heartbeat` | `_handle_heartbeat` — sinyal liveness |
//!
//! Penyederhanaan vs aslinya (port ini belum punya dispatcher/claim):
//! - tanpa worker-guard/run-claim (`_worker_guard`, `expected_run_id`,
//!   `LiveClaimError`): tidak ada sistem klaim di port ini;
//! - tanpa goal-judge gate di `kanban_complete`/`kanban_block` (tidak ada
//!   judge di port ini);
//! - `created_cards` hanya diverifikasi keberadaannya (cek phantom), bukan
//!   kepemilikan profil worker;
//! - `kanban_heartbeat` murni side effect liveness (tanpa claim-TTL).

use super::store::{
    kanban_home, normalize_board_slug, BoardDb, KanbanStore, TaskFilter, ALL_STATUSES,
    STATUS_ARCHIVED, STATUS_BLOCKED, STATUS_DONE, STATUS_READY, STATUS_RUNNING, STATUS_SCHEDULED,
    STATUS_TODO, STATUS_TRIAGE,
};
use crate::schema::*;
use crate::shell::redact_secrets;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, Tool, ToolContext, ToolEffect, ToolRegistry, ToolResult};
use serde_json::{json, Value};

/// Jenis block yang sah — cerminan `VALID_BLOCK_KINDS` di aslinya.
const VALID_BLOCK_KINDS: &[&str] = &["dependency", "needs_input", "capability", "transient"];
/// Batas re-block sejenis sebelum eskalasi ke triage — cerminan
/// `BLOCK_RECURRENCE_LIMIT = 2` di aslinya.
const BLOCK_RECURRENCE_LIMIT: i64 = 2;
/// Batas list default/maks — cerminan `KANBAN_LIST_DEFAULT_LIMIT` = 50 dan
/// `KANBAN_LIST_MAX_LIMIT` = 200 di aslinya.
const LIST_DEFAULT_LIMIT: usize = 50;
const LIST_MAX_LIMIT: usize = 200;

// ---------------------------------------------------------------------------
// Helper (cerminan pola tools.rs Worker A)
// ---------------------------------------------------------------------------

fn tool_err(tool: &str, msg: impl Into<String>) -> HermesError {
    HermesError::tool(tool, msg)
}

fn store_for(ctx: &ToolContext, tool: &str) -> Result<KanbanStore> {
    let home = kanban_home(&ctx.env, &ctx.working_dir);
    KanbanStore::open(home).map_err(|e| tool_err(tool, e.to_string()))
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
        return normalize_board_slug(b).map_err(|e| tool_err(tool, e.to_string()));
    }
    if let Some(b) = ctx
        .env
        .get("HERMES_KANBAN_BOARD")
        .filter(|s| !s.trim().is_empty())
    {
        return normalize_board_slug(b).map_err(|e| tool_err(tool, e.to_string()));
    }
    store
        .current_board()
        .map_err(|e| tool_err(tool, e.to_string()))
}

/// Resolusi task id ala `_DESC_TASK_ID_DEFAULT` aslinya: argumen `task_id` →
/// env `HERMES_KANBAN_TASK` (task milik worker yang di-spawn dispatcher).
fn resolve_task_id(args: &Value, ctx: &ToolContext, tool: &str) -> Result<String> {
    match opt_str(args, "task_id").filter(|s| !s.is_empty()) {
        Some(t) => Ok(t.to_string()),
        None => ctx
            .env
            .get("HERMES_KANBAN_TASK")
            .filter(|s| !s.is_empty())
            .cloned()
            .ok_or_else(|| {
                tool_err(
                    tool,
                    "task_id wajib diisi (atau set HERMES_KANBAN_TASK); \
                     pakai kanban_list untuk menemukan id",
                )
            }),
    }
}

fn board_db_for(ctx: &ToolContext, args: &Value, tool: &str) -> Result<(BoardDb, String)> {
    let store = store_for(ctx, tool)?;
    let board = resolve_board(args, ctx, &store, tool)?;
    let db = store
        .board_db(&board)
        .map_err(|e| tool_err(tool, e.to_string()))?;
    Ok((db, board))
}

fn board_prop() -> Value {
    string_prop(
        "Slug board kanban target. Bila kosong dipakai HERMES_KANBAN_BOARD, \
         lalu board aktif, lalu 'default'.",
    )
}

fn task_id_prop() -> Value {
    string_prop(
        "Id task. Bila kosong dipakai HERMES_KANBAN_TASK dari env \
         (task milik worker yang di-spawn dispatcher).",
    )
}

fn opt_string_array(args: &Value, name: &str, tool: &str) -> Result<Vec<String>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .ok_or_else(|| tool_err(tool, format!("`{name}` harus array string")))
            })
            .collect(),
        Some(_) => Err(tool_err(tool, format!("`{name}` harus array string"))),
    }
}

/// Redaksi secret pada metadata — cerminan `_redact_metadata` aslinya:
/// serialkan ke JSON, redaksi, parse ulang; bila gagal, pakai nilai asli.
fn redact_metadata(value: &Value) -> Value {
    let raw = serde_json::to_string(value).unwrap_or_default();
    let redacted = redact_secrets(&raw);
    serde_json::from_str(&redacted).unwrap_or_else(|_| value.clone())
}

/// `true` bila semua parent langsung berstatus `done`/`archived`
/// (cerminan `_parents_satisfied` di aslinya; parent yang hilang dihitung
/// belum selesai agar aman).
fn parents_satisfied(db: &BoardDb, task_id: &str, tool: &str) -> Result<bool> {
    let parents = db
        .parents_of(task_id)
        .map_err(|e| tool_err(tool, e.to_string()))?;
    for p in parents {
        let done = db
            .get_task(&p)
            .map_err(|e| tool_err(tool, e.to_string()))?
            .map(|t| t.status.as_str() == STATUS_DONE || t.status.as_str() == STATUS_ARCHIVED)
            .unwrap_or(false);
        if !done {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Promosikan task `todo` yang semua parent-nya done/archived menjadi
/// `ready` — cerminan `recompute_ready` aslinya (dipanggil `kanban_list`
/// sebelum listing, seperti CLI). Task `blocked` tidak disentuh: block
/// eksplisit bersifat sticky hingga `kanban_unblock` (cerminan
/// `_has_sticky_block`).
fn recompute_ready(db: &BoardDb, tool: &str) -> Result<usize> {
    let todos = db
        .list_tasks(&TaskFilter {
            status: Some(STATUS_TODO.to_string()),
            limit: Some(LIST_MAX_LIMIT),
            ..Default::default()
        })
        .map_err(|e| tool_err(tool, e.to_string()))?;
    let mut promoted = 0;
    for t in todos {
        if parents_satisfied(db, &t.id, tool)? {
            db.move_task(
                &t.id,
                STATUS_READY,
                Some("recompute_ready: semua parent done"),
            )
            .map_err(|e| tool_err(tool, e.to_string()))?;
            db.record_event(&t.id, "promoted", Some(r#"{"status":"ready"}"#))
                .map_err(|e| tool_err(tool, e.to_string()))?;
            promoted += 1;
        }
    }
    Ok(promoted)
}

// ---------------------------------------------------------------------------
// kanban_list — port langsung dari KANBAN_LIST_SCHEMA / _handle_list
// ---------------------------------------------------------------------------

pub struct KanbanListTool;

#[async_trait]
impl Tool for KanbanListTool {
    fn name(&self) -> &str {
        "kanban_list"
    }
    fn description(&self) -> &str {
        "Daftar ringkasan task kanban agar orchestrator bisa menemukan kerja \
         untuk di-route. Filter ala CLI: assignee, status, tenant, \
         include_archived, limit. Menghitung ulang task ready sebelum listing \
         (seperti CLI). Baris ringkas memuat id, judul, status, assignee, \
         priority, parent/child, dan jumlah komentar & attachment. Default 50 \
         baris, maks 200, dengan metadata truncation."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("assignee", string_prop("Filter assignee/profil opsional.")),
                (
                    "status",
                    json!({"type": "string",
                           "enum": ["triage", "todo", "scheduled", "ready",
                                    "running", "blocked", "review", "done", "archived"],
                           "description": "Filter status task opsional."}),
                ),
                (
                    "tenant",
                    string_prop("Filter namespace tenant/proyek opsional."),
                ),
                (
                    "include_archived",
                    boolean_prop("Sertakan task yang diarsipkan (default false)."),
                ),
                (
                    "limit",
                    integer_prop("Maksimum baris yang dikembalikan (default 50, maks 200)."),
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
        let limit = match args.get("limit") {
            None | Some(Value::Null) => LIST_DEFAULT_LIMIT,
            Some(v) => {
                let n = v
                    .as_i64()
                    .ok_or_else(|| tool_err(self.name(), "limit harus integer"))?;
                if n < 1 || n > LIST_MAX_LIMIT as i64 {
                    return Err(tool_err(
                        self.name(),
                        format!("limit harus 1..={LIST_MAX_LIMIT}"),
                    ));
                }
                n as usize
            }
        };
        if let Some(s) = opt_str(&args, "status") {
            if !ALL_STATUSES.contains(&s) {
                return Err(tool_err(
                    self.name(),
                    format!("status tidak dikenal: '{s}'"),
                ));
            }
        }
        let (db, _board) = board_db_for(ctx, &args, self.name())?;
        // Samakan dengan CLI: dependensi yang lunas sejak tick dispatcher
        // terakhir langsung terlihat orchestrator.
        let promoted = recompute_ready(&db, self.name())?;
        // Satu baris ekstra agar output bisa melaporkan truncation.
        let mut rows = db
            .list_tasks(&TaskFilter {
                assignee: opt_str(&args, "assignee").map(|s| s.to_string()),
                status: opt_str(&args, "status").map(|s| s.to_string()),
                tenant: opt_str(&args, "tenant").map(|s| s.to_string()),
                include_archived: opt_bool(&args, "include_archived").unwrap_or(false),
                limit: Some(limit + 1),
            })
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        let truncated = rows.len() > limit;
        rows.truncate(limit);
        let next_limit = if truncated && limit < LIST_MAX_LIMIT {
            Some((limit * 2).min(LIST_MAX_LIMIT))
        } else {
            None
        };
        Ok(ok(
            "",
            self.name(),
            json!({
                "tasks": rows,
                "count": rows.len(),
                "limit": limit,
                "truncated": truncated,
                "next_limit": next_limit,
                "promoted": promoted,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_complete — port langsung dari KANBAN_COMPLETE_SCHEMA / _handle_complete
// ---------------------------------------------------------------------------

pub struct KanbanCompleteTool;

#[async_trait]
impl Tool for KanbanCompleteTool {
    fn name(&self) -> &str {
        "kanban_complete"
    }
    fn description(&self) -> &str {
        "Tandai task saat ini done dengan handoff terstruktur untuk worker \
         dan manusia downstream. Utamakan `summary`: deskripsi 1-3 kalimat \
         yang human-readable; fakta machine-readable (changed_files, \
         tests_run, decisions, findings) di `metadata`. Wajib isi salah satu \
         dari `summary`/`result`. Bila membuat task via `kanban_create` \
         selama run, cantumkan id-nya di `created_cards` — keberadaan tiap id \
         diverifikasi; id phantom memblokir completion. Cantumkan path absolut \
         deliverable di `artifacts` — tiap path harus ada di disk saat \
         completion dan digabung ke metadata."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "summary",
                    string_prop(
                        "Handoff human-readable, 1-3 kalimat. Muncul di Run \
                         History dashboard dan konteks worker downstream.",
                    ),
                ),
                (
                    "metadata",
                    json!({"type": "object",
                           "description": "Dict fakta terstruktur tentang attempt ini, \
                           mis. {\"changed_files\": [...], \"tests_run\": 12}. \
                           Disertakan ke worker downstream bersama summary."}),
                ),
                (
                    "result",
                    string_prop(
                        "Satu baris log hasil (field legacy, dipetakan ke \
                         task.result). Utamakan `summary` bila memungkinkan.",
                    ),
                ),
                (
                    "created_cards",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Manifest id task yang dibuat via \
                           kanban_create selama run ini. Tiap id diverifikasi \
                           keberadaannya; id phantom memblokir completion. \
                           Kosongkan bila tidak membuat kartu."}),
                ),
                (
                    "artifacts",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Path absolut file deliverable yang \
                           dihasilkan selama run (grafik, PDF, spreadsheet, \
                           gambar). Tiap path harus ada di disk saat \
                           completion; path yang hilang membuat task tetap \
                           in-flight agar bisa diperbaiki lalu dicoba lagi."}),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let tid = resolve_task_id(&args, ctx, self.name())?;
        let summary_raw = opt_str(&args, "summary")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let result_raw = opt_str(&args, "result")
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let (db, _board) = board_db_for(ctx, &args, self.name())?;
        let task = db
            .get_task(&tid)
            .map_err(|e| tool_err(self.name(), e.to_string()))?
            .ok_or_else(|| tool_err(self.name(), format!("task '{tid}' tidak ada")))?;
        // Bukti penyelesaian: argumen, atau yang sudah tersimpan di kartu
        // (cerminan `_gate_empty_completion` di aslinya).
        let stored_evidence = task
            .summary
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
            || task
                .result
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
        if summary_raw.is_none() && result_raw.is_none() && !stored_evidence {
            return Err(tool_err(
                self.name(),
                "kanban_complete diblokir: berikan 'summary' atau 'result' yang \
                 tidak kosong tentang apa yang dikerjakan. Task masih in-flight \
                 (tidak ada perubahan state).",
            ));
        }
        // `metadata` harus object bila diberikan.
        let mut meta_map = match args.get("metadata") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(_)) => {
                let redacted = redact_metadata(args.get("metadata").unwrap_or(&Value::Null));
                redacted.as_object().cloned().unwrap_or_default()
            }
            Some(_) => {
                return Err(tool_err(self.name(), "'metadata' harus berupa object"));
            }
        };
        // `created_cards`: verifikasi keberadaan tiap id (cek phantom —
        // cerminan `HallucinatedCardsError` di aslinya).
        let created_cards = opt_string_array(&args, "created_cards", self.name())?;
        let mut phantom = vec![];
        for c in &created_cards {
            let exists = db
                .get_task(c)
                .map_err(|e| tool_err(self.name(), e.to_string()))?
                .is_some();
            if !exists {
                phantom.push(c.clone());
            }
        }
        if !phantom.is_empty() {
            return Err(tool_err(
                self.name(),
                format!(
                    "kanban_complete diblokir: created_cards berikut tidak ada: {}. \
                     Task masih in-flight (tidak ada perubahan state). Ulangi \
                     kanban_complete dengan summary/metadata yang sama dan buang \
                     id tersebut dari created_cards.",
                    phantom.join(", ")
                ),
            ));
        }
        // `artifacts`: tiap path harus ada di disk (cerminan
        // `ArtifactPreservationError` — task tetap in-flight).
        let artifacts = opt_string_array(&args, "artifacts", self.name())?;
        let missing: Vec<&String> = artifacts
            .iter()
            .filter(|a| !std::path::Path::new(a).exists())
            .collect();
        if !missing.is_empty() {
            return Err(tool_err(
                self.name(),
                format!(
                    "kanban_complete tidak bisa mengawetkan artifact: path tidak \
                     ada di disk: {}. Task masih in-flight dan workspace \
                     dipertahankan. Perbaiki path-nya lalu ulangi kanban_complete \
                     dengan handoff yang sama.",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if !artifacts.is_empty() {
            meta_map.insert("artifacts".to_string(), json!(artifacts));
        }
        // Parent harus terminal dulu (cerminan `complete_task` aslinya yang
        // mengembalikan False bila parent belum selesai).
        let mut blockers = vec![];
        for p in db
            .parents_of(&tid)
            .map_err(|e| tool_err(self.name(), e.to_string()))?
        {
            match db
                .get_task(&p)
                .map_err(|e| tool_err(self.name(), e.to_string()))?
            {
                Some(pt)
                    if pt.status.as_str() == STATUS_DONE
                        || pt.status.as_str() == STATUS_ARCHIVED => {}
                Some(pt) => blockers.push(format!("{p} ({})", pt.status)),
                None => blockers.push(format!("{p} (hilang)")),
            }
        }
        if !blockers.is_empty() {
            return Err(tool_err(
                self.name(),
                format!(
                    "tidak bisa complete {tid}: parent belum selesai: {}; \
                     selesaikan parent dulu (done atau archived)",
                    blockers.join(", ")
                ),
            ));
        }
        let metadata_json = if meta_map.is_empty() {
            None
        } else {
            Some(Value::Object(meta_map).to_string())
        };
        let summary = summary_raw.map(redact_secrets);
        let result = result_raw.map(redact_secrets);
        db.complete_task(
            &tid,
            summary.as_deref(),
            metadata_json.as_deref(),
            result.as_deref(),
        )
        .map_err(|e| tool_err(self.name(), e.to_string()))?;
        // Anak yang parent-nya kini semua done ikut dipromosikan
        // (dependency lane aslinya: auto-resume saat parent selesai).
        let promoted = db
            .promote_ready_children(&tid)
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        let attachments = db
            .list_attachments(&tid)
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        Ok(ok(
            "",
            self.name(),
            json!({
                "task_id": tid,
                "status": STATUS_DONE,
                "promoted_children": promoted,
                "attachments": attachments,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_block — port dari KANBAN_BLOCK_SCHEMA / _handle_block + _route_block
// ---------------------------------------------------------------------------

pub struct KanbanBlockTool;

#[async_trait]
impl Tool for KanbanBlockTool {
    fn name(&self) -> &str {
        "kanban_block"
    }
    fn description(&self) -> &str {
        "Hentikan kerja pada task dan route sesuai KENAPA stuck. `kind`: \
         'dependency' (menunggu task lain — parkir di todo dan auto-resume \
         saat parent selesai, tanpa manusia; bila tidak ada parent terbuka \
         dicatat sebagai needs_input), 'needs_input' (butuh keputusan/jawaban \
         manusia), 'capability' (tembok keras: tak ada akses/kredensial), atau \
         'transient' (gagal flaky yang mungkin pulih). `reason` tampil ke \
         manusia di board. Task yang di-unblock lalu di-block lagi untuk \
         alasan yang sama akan dieskalasi otomatis ke triage."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "reason",
                    string_prop(
                        "Apa yang dibutuhkan / apa yang menghentikanmu, satu \
                         dua kalimat. Tampil ke manusia di board.",
                    ),
                ),
                (
                    "kind",
                    json!({"type": "string",
                           "enum": ["dependency", "needs_input", "capability", "transient"],
                           "description": "Kenapa blocked. 'dependency' menunggu \
                           di todo dan lanjut otomatis saat parent yang belum \
                           selesai tuntas; bila tidak ada parent terbuka \
                           dicatat sebagai needs_input. Kosongkan bila tidak \
                           ada yang cocok."}),
                ),
                ("board", board_prop()),
            ],
            vec!["reason"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let tid = resolve_task_id(&args, ctx, self.name())?;
        let reason_raw = opt_str(&args, "reason").map(str::trim).unwrap_or("");
        if reason_raw.is_empty() {
            return Err(tool_err(
                self.name(),
                "reason wajib diisi — jelaskan input apa yang dibutuhkan",
            ));
        }
        let kind_arg = opt_str(&args, "kind");
        if let Some(k) = kind_arg {
            if !VALID_BLOCK_KINDS.contains(&k) {
                return Err(tool_err(
                    self.name(),
                    format!("kind harus salah satu dari {VALID_BLOCK_KINDS:?} (atau kosongkan)"),
                ));
            }
        }
        let (db, _board) = board_db_for(ctx, &args, self.name())?;
        let task = db
            .get_task(&tid)
            .map_err(|e| tool_err(self.name(), e.to_string()))?
            .ok_or_else(|| tool_err(self.name(), format!("task '{tid}' tidak ada")))?;
        if task.status.as_str() != STATUS_RUNNING && task.status.as_str() != STATUS_READY {
            return Err(tool_err(
                self.name(),
                format!("tidak bisa block {tid} (id tidak dikenal atau bukan running/ready)"),
            ));
        }
        let reason = redact_secrets(reason_raw);
        // `dependency` hanya menunggu parent yang belum selesai. Tanpa parent
        // terbuka, parkir di todo akan di-respawn context-free oleh
        // recompute_ready — re-kind menjadi needs_input yang sticky
        // (cerminan `block_task` di aslinya).
        let mut kind = kind_arg;
        let mut extra_note: Option<&str> = None;
        let mut requested_kind: Option<&str> = None;
        if kind == Some("dependency") && parents_satisfied(&db, &tid, self.name())? {
            kind = Some("needs_input");
            requested_kind = Some("dependency");
            extra_note = Some(
                "kind='dependency' hanya menunggu parent yang belum selesai; \
                 tidak ada parent terbuka, jadi dicatat sebagai needs_input \
                 (sticky hingga manusia unblock) alih-alih parkir di todo.",
            );
        }
        // Routing block (cerminan `_route_block`): dependency → todo tanpa
        // menyentuh recurrences; selainnya menghitung re-block sejenis dan
        // dieskalasi ke triage saat mencapai batas.
        let prev_kind = task.block_kind.as_deref();
        let (new_status, event_kind, recurrences) = if kind == Some("dependency") {
            (STATUS_TODO, "dependency_wait", task.block_recurrences)
        } else {
            let rec = if prev_kind == kind {
                task.block_recurrences + 1
            } else {
                1
            };
            if rec >= BLOCK_RECURRENCE_LIMIT {
                (STATUS_TRIAGE, "block_loop_detected", rec)
            } else {
                (STATUS_BLOCKED, "blocked", rec)
            }
        };
        db.set_block_state(&tid, kind, recurrences)
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        db.move_task(&tid, new_status, Some(&reason))
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        let mut payload = json!({
            "reason": reason,
            "kind": kind,
            "source_status": task.status,
        });
        if event_kind != "dependency_wait" {
            payload["recurrences"] = json!(recurrences);
        }
        if event_kind == "block_loop_detected" {
            payload["limit"] = json!(BLOCK_RECURRENCE_LIMIT);
        }
        db.record_event(&tid, event_kind, Some(&payload.to_string()))
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        let mut out = json!({
            "task_id": tid,
            "status": new_status,
            "block_kind": kind,
            "block_recurrences": recurrences,
        });
        if let Some(n) = extra_note {
            out["note"] = json!(n);
        }
        if let Some(r) = requested_kind {
            out["requested_kind"] = json!(r);
        }
        Ok(ok("", self.name(), out.to_string()))
    }
}

// ---------------------------------------------------------------------------
// kanban_unblock — port dari KANBAN_UNBLOCK_SCHEMA / _handle_unblock
// ---------------------------------------------------------------------------

pub struct KanbanUnblockTool;

#[async_trait]
impl Tool for KanbanUnblockTool {
    fn name(&self) -> &str {
        "kanban_unblock"
    }
    fn description(&self) -> &str {
        "Buka blockir task kanban. Pindah ke ready bila semua parent done, \
         atau todo selama ada parent yang masih terbuka."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                (
                    "task_id",
                    string_prop("Id task yang blocked untuk dipindah ke ready/todo."),
                ),
                ("board", board_prop()),
            ],
            vec!["task_id"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let tid = get_str(&args, "task_id")?;
        let (db, _board) = board_db_for(ctx, &args, self.name())?;
        let task = db
            .get_task(tid)
            .map_err(|e| tool_err(self.name(), e.to_string()))?
            .ok_or_else(|| {
                tool_err(
                    self.name(),
                    format!("tidak bisa unblock {tid} (tidak blocked atau tidak dikenal)"),
                )
            })?;
        if task.status.as_str() != STATUS_BLOCKED && task.status.as_str() != STATUS_SCHEDULED {
            return Err(tool_err(
                self.name(),
                format!("tidak bisa unblock {tid} (tidak blocked atau tidak dikenal)"),
            ));
        }
        // Re-gate ke parent sebelum kembali ke pool (cerminan
        // `_landing_status_after_parents`): ready bila semua parent terminal,
        // todo selama ada yang terbuka.
        let landing = if parents_satisfied(&db, tid, self.name())? {
            STATUS_READY
        } else {
            STATUS_TODO
        };
        // `block_kind`/`block_recurrences` sengaja dipertahankan melewati
        // unblock (cerminan aslinya): me-reset-nya adalah amnesia yang membuat
        // loop unblock <-> re-block tak terbatas; hanya kanban_complete yang
        // membersihkannya.
        db.move_task(tid, landing, Some("unblocked"))
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        db.record_event(
            tid,
            "unblocked",
            Some(&json!({"from": task.status, "to": landing}).to_string()),
        )
        .map_err(|e| tool_err(self.name(), e.to_string()))?;
        Ok(ok(
            "",
            self.name(),
            json!({"task_id": tid, "status": landing}).to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_schedule — port dari KANBAN_SCHEDULE_SCHEMA / _handle_schedule
// ---------------------------------------------------------------------------

pub struct KanbanScheduleTool;

#[async_trait]
impl Tool for KanbanScheduleTool {
    fn name(&self) -> &str {
        "kanban_schedule"
    }
    fn description(&self) -> &str {
        "Parkir task saat ini di status 'scheduled' selagi menunggu waktu atau \
         event eksternal. Ini mengakhiri run saat ini dan membuat task tidak \
         bisa di-dispatch hingga orchestrator unblock; tidak membuat timer. \
         Taruh penanda bangun seperti SCHEDULED_UNTIL=<ISO8601> di `reason`."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "reason",
                    string_prop(
                        "Alasan opsional atau penanda bangun machine-readable \
                         yang dicatat pada run yang selesai dan event scheduled.",
                    ),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let tid = resolve_task_id(&args, ctx, self.name())?;
        let reason = opt_str(&args, "reason")
            .map(|s| redact_secrets(s.trim()))
            .filter(|s| !s.is_empty());
        let (db, _board) = board_db_for(ctx, &args, self.name())?;
        if let Err(e) = db.move_task(&tid, STATUS_SCHEDULED, reason.as_deref()) {
            return Err(tool_err(
                self.name(),
                format!(
                    "tidak bisa schedule '{tid}' (id tidak dikenal atau bukan \
                     todo/ready/running/blocked): {e}",
                ),
            ));
        }
        db.record_event(
            &tid,
            "scheduled",
            Some(&json!({"reason": reason}).to_string()),
        )
        .map_err(|e| tool_err(self.name(), e.to_string()))?;
        Ok(ok(
            "",
            self.name(),
            json!({"task_id": tid, "status": STATUS_SCHEDULED, "reason": reason}).to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_heartbeat — port dari KANBAN_HEARTBEAT_SCHEMA / _handle_heartbeat
// ---------------------------------------------------------------------------

pub struct KanbanHeartbeatTool;

#[async_trait]
impl Tool for KanbanHeartbeatTool {
    fn name(&self) -> &str {
        "kanban_heartbeat"
    }
    fn description(&self) -> &str {
        "Sinyal bahwa kamu masih hidup selama operasi panjang (training, \
         encoding, crawl besar). Panggil tiap beberapa menit agar manusia \
         melihat liveness. Murni side effect — tidak mengubah state kerja."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "note",
                    string_prop(
                        "Catatan singkat opsional tentang progres saat ini. \
                         Muncul di event log.",
                    ),
                ),
                ("board", board_prop()),
            ],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let tid = resolve_task_id(&args, ctx, self.name())?;
        let note = opt_str(&args, "note")
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let (db, _board) = board_db_for(ctx, &args, self.name())?;
        db.heartbeat(&tid, note.as_deref())
            .map_err(|e| tool_err(self.name(), e.to_string()))?;
        Ok(ok("", self.name(), json!({"task_id": tid}).to_string()))
    }
}

/// Daftarkan 6 tool query/siklus-hidup Worker B ke registry.
pub fn register_query_tools(registry: &mut ToolRegistry) {
    registry.register(KanbanListTool);
    registry.register(KanbanCompleteTool);
    registry.register(KanbanBlockTool);
    registry.register(KanbanUnblockTool);
    registry.register(KanbanScheduleTool);
    registry.register(KanbanHeartbeatTool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_ctx;
    use serde_json::json;

    fn kanban_ctx(tag: &str) -> ToolContext {
        let dir = std::env::temp_dir().join(format!(
            "hermes-kanban-query-test-{}-{}",
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

    /// Buat task langsung via store (tanpa lewat kanban_create).
    fn mk(db: &BoardDb, title: &str, parents: Vec<String>) -> String {
        use super::super::store::NewTask;
        db.create_task(NewTask {
            title: title.into(),
            assignee: "worker".into(),
            parents,
            ..Default::default()
        })
        .unwrap()
        .id
    }

    fn db_of(ctx: &ToolContext) -> BoardDb {
        let store = store_for(ctx, "test").unwrap();
        store.board_db("default").unwrap()
    }

    #[tokio::test]
    async fn list_promotes_ready_and_paginates() {
        let ctx = kanban_ctx("list");
        let db = db_of(&ctx);
        let parent = mk(&db, "parent", vec![]);
        let child = mk(&db, "child", vec![parent.clone()]);
        // Child mendarat di todo karena parent belum done.
        assert_eq!(db.get_task(&child).unwrap().unwrap().status, STATUS_TODO);
        // Selesaikan parent via store agar child siap dipromosikan.
        db.complete_task(&parent, Some("selesai"), None, None)
            .unwrap();

        let list = KanbanListTool;
        let out = run(&list, json!({}), &ctx).await;
        assert_eq!(out["promoted"], 1);
        assert_eq!(db.get_task(&child).unwrap().unwrap().status, STATUS_READY);
        assert!(out["count"].as_u64().unwrap() >= 2);

        // Paginasi: limit=1 → truncated + next_limit.
        let out = run(&list, json!({"limit": 1}), &ctx).await;
        assert_eq!(out["count"], 1);
        assert_eq!(out["truncated"], true);
        assert_eq!(out["next_limit"], 2);

        // Validasi argumen.
        assert!(list.execute(json!({"limit": 0}), &ctx).await.is_err());
        assert!(list.execute(json!({"limit": 201}), &ctx).await.is_err());
        assert!(list
            .execute(json!({"limit": "banyak"}), &ctx)
            .await
            .is_err());
        assert!(list
            .execute(json!({"status": "mengambang"}), &ctx)
            .await
            .is_err());

        // Filter status.
        let out = run(&list, json!({"status": "ready"}), &ctx).await;
        let ids: Vec<&str> = out["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&child.as_str()));
    }

    #[tokio::test]
    async fn complete_handoff_promotes_children() {
        let ctx = kanban_ctx("complete");
        let db = db_of(&ctx);
        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        let complete = KanbanCompleteTool;

        // Tanpa summary/result dan tanpa bukti tersimpan → ditolak.
        assert!(complete
            .execute(json!({"task_id": parent}), &ctx)
            .await
            .is_err());
        // Task masih in-flight.
        assert_eq!(
            db.get_task(&parent).unwrap().unwrap().status,
            STATUS_RUNNING
        );

        // created_cards phantom → ditolak.
        assert!(complete
            .execute(
                json!({"task_id": parent, "summary": "ok", "created_cards": ["t_hantu123"]}),
                &ctx
            )
            .await
            .is_err());

        // Artifact hilang → ditolak, task tetap in-flight.
        assert!(complete
            .execute(
                json!({"task_id": parent, "summary": "ok",
                       "artifacts": ["/tmp/hermes-tidak-ada-xyz.pdf"]}),
                &ctx
            )
            .await
            .is_err());
        assert_eq!(
            db.get_task(&parent).unwrap().unwrap().status,
            STATUS_RUNNING
        );

        // Handoff valid: summary + metadata + artifact nyata.
        let art = std::env::temp_dir().join("hermes-artifact-ok.txt");
        std::fs::write(&art, "deliverable").unwrap();
        let out = run(
            &complete,
            json!({"task_id": parent, "summary": "pekerjaan selesai",
                   "metadata": {"tests_run": 3},
                   "artifacts": [art.to_string_lossy()]}),
            &ctx,
        )
        .await;
        assert_eq!(out["task_id"], parent);
        assert_eq!(out["status"], "done");
        assert_eq!(out["promoted_children"][0], child);
        // Child ikut terpromosi ke ready oleh dependency lane.
        assert_eq!(db.get_task(&child).unwrap().unwrap().status, STATUS_READY);
        let _ = std::fs::remove_file(&art);

        // Metadata tersimpan sebagai JSON dan memuat artifacts.
        let t = db.get_task(&parent).unwrap().unwrap();
        let meta: Value = serde_json::from_str(t.metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(meta["tests_run"], 3);
        assert_eq!(meta["artifacts"][0], art.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn complete_refuses_unsatisfied_parents() {
        let ctx = kanban_ctx("complete-parents");
        let db = db_of(&ctx);
        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        let complete = KanbanCompleteTool;
        let err = complete
            .execute(json!({"task_id": child, "summary": "coba"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("belum selesai"),
            "unexpected: {err}"
        );
    }

    #[tokio::test]
    async fn block_routing_and_loop_breaker() {
        let ctx = kanban_ctx("block");
        let db = db_of(&ctx);
        let block = KanbanBlockTool;
        let unblock = KanbanUnblockTool;

        // kind=dependency dengan parent terbuka → parkir di todo.
        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        db.move_task(&child, STATUS_READY, None).unwrap(); // simulasi dispatch
        let out = run(
            &block,
            json!({"task_id": child, "reason": "tunggu induk", "kind": "dependency"}),
            &ctx,
        )
        .await;
        assert_eq!(out["status"], "todo");
        assert_eq!(out["block_kind"], "dependency");

        // kind=dependency tanpa parent → re-kind needs_input (sticky blocked).
        let solo = mk(&db, "solo", vec![]);
        let out = run(
            &block,
            json!({"task_id": solo, "reason": "tunggu data", "kind": "dependency"}),
            &ctx,
        )
        .await;
        assert_eq!(out["status"], "blocked");
        assert_eq!(out["block_kind"], "needs_input");
        assert_eq!(out["requested_kind"], "dependency");
        assert!(out["note"].as_str().unwrap().contains("sticky"));

        // Loop-breaker: unblock lalu block lagi sejenis → triage (limit=2).
        let out = run(&unblock, json!({"task_id": solo}), &ctx).await;
        assert_eq!(out["status"], "ready");
        let out = run(
            &block,
            json!({"task_id": solo, "reason": "masih butuh data", "kind": "needs_input"}),
            &ctx,
        )
        .await;
        assert_eq!(out["status"], "triage");
        assert_eq!(out["block_recurrences"], 2);

        // kind tak dikenal / reason kosong / status tak bisa di-block.
        assert!(block
            .execute(
                json!({"task_id": parent, "reason": "x", "kind": "menunggu"}),
                &ctx
            )
            .await
            .is_err());
        assert!(block
            .execute(json!({"task_id": parent, "reason": "   "}), &ctx)
            .await
            .is_err());
        db.complete_task(&parent, Some("ok"), None, None).unwrap();
        assert!(block
            .execute(json!({"task_id": parent, "reason": "x"}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn unblock_regates_on_parents() {
        let ctx = kanban_ctx("unblock");
        let db = db_of(&ctx);
        let block = KanbanBlockTool;
        let unblock = KanbanUnblockTool;

        let parent = mk(&db, "induk", vec![]);
        let child = mk(&db, "anak", vec![parent.clone()]);
        db.move_task(&child, STATUS_READY, None).unwrap(); // simulasi dispatch
        run(
            &block,
            json!({"task_id": child, "reason": "butuh masukan", "kind": "needs_input"}),
            &ctx,
        )
        .await;
        // Parent masih terbuka → unblock mendarat di todo.
        let out = run(&unblock, json!({"task_id": child}), &ctx).await;
        assert_eq!(out["status"], "todo");

        // Selesaikan parent; block lagi dari ready lalu unblock → ready.
        db.complete_task(&parent, Some("ok"), None, None).unwrap();
        db.move_task(&child, STATUS_READY, None).unwrap();
        run(
            &block,
            json!({"task_id": child, "reason": "cek ulang", "kind": "transient"}),
            &ctx,
        )
        .await;
        let out = run(&unblock, json!({"task_id": child}), &ctx).await;
        assert_eq!(out["status"], "ready");
        // block_kind/recurrences dipertahankan melewati unblock (anti-amnesia).
        let t = db.get_task(&child).unwrap().unwrap();
        assert_eq!(t.block_kind.as_deref(), Some("transient"));

        // Unblock task yang tidak blocked → error.
        assert!(unblock
            .execute(json!({"task_id": child}), &ctx)
            .await
            .is_err());
        // task_id wajib.
        assert!(unblock.execute(json!({}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn schedule_parks_and_unblocks() {
        let ctx = kanban_ctx("schedule");
        let db = db_of(&ctx);
        let schedule = KanbanScheduleTool;
        let unblock = KanbanUnblockTool;

        let t = mk(&db, "tugas", vec![]);
        let out = run(
            &schedule,
            json!({"task_id": t, "reason": "SCHEDULED_UNTIL=2026-10-22T00:00:00Z"}),
            &ctx,
        )
        .await;
        assert_eq!(out["status"], "scheduled");
        assert!(out["reason"].as_str().unwrap().contains("SCHEDULED_UNTIL"));

        // Scheduled bisa di-unblock kembali ke ready.
        let out = run(&unblock, json!({"task_id": t}), &ctx).await;
        assert_eq!(out["status"], "ready");

        // Triage tidak bisa di-schedule (di luar todo/ready/running/blocked).
        let t2 = mk(&db, "tugas2", vec![]);
        db.move_task(&t2, STATUS_TRIAGE, None).unwrap();
        assert!(schedule
            .execute(json!({"task_id": t2}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn heartbeat_records_liveness() {
        let ctx = kanban_ctx("heartbeat");
        let db = db_of(&ctx);
        let hb = KanbanHeartbeatTool;

        let t = mk(&db, "panjang", vec![]);
        assert!(db
            .get_task(&t)
            .unwrap()
            .unwrap()
            .last_heartbeat_at
            .is_none());
        let out = run(&hb, json!({"task_id": t, "note": "50% jalan"}), &ctx).await;
        assert_eq!(out["task_id"], t);
        let task = db.get_task(&t).unwrap().unwrap();
        assert!(task.last_heartbeat_at.is_some());
        let events = db.list_events(&t, 5).unwrap();
        assert_eq!(events[0].kind, "heartbeat");
        assert!(events[0].payload.as_deref().unwrap().contains("50% jalan"));

        // Task fiktif → error.
        assert!(hb
            .execute(json!({"task_id": "t_tidakada"}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn task_id_env_fallback() {
        let mut ctx = kanban_ctx("env-fallback");
        let db = db_of(&ctx);
        let t = mk(&db, "env task", vec![]);
        ctx.env.insert("HERMES_KANBAN_TASK".to_string(), t.clone());

        let hb = KanbanHeartbeatTool;
        let out = run(&hb, json!(null), &ctx).await;
        assert_eq!(out["task_id"], t);

        let complete = KanbanCompleteTool;
        let out = run(&complete, json!({"summary": "selesai via env"}), &ctx).await;
        assert_eq!(out["task_id"], t);
        assert_eq!(out["status"], "done");
    }

    #[tokio::test]
    async fn registry_registers_all_six() {
        let mut reg = ToolRegistry::new();
        register_query_tools(&mut reg);
        let names = reg.names();
        for n in [
            "kanban_list",
            "kanban_complete",
            "kanban_block",
            "kanban_unblock",
            "kanban_schedule",
            "kanban_heartbeat",
        ] {
            assert!(names.contains(&n.to_string()), "hilang: {n}");
            let t = reg.get(n).unwrap();
            assert!(!t.description().is_empty());
            assert!(t.parameters_schema().is_object());
        }
        assert_eq!(reg.get("kanban_list").unwrap().effect(), ToolEffect::Read);
        assert_eq!(
            reg.get("kanban_complete").unwrap().effect(),
            ToolEffect::Write
        );
        assert_eq!(
            reg.get("kanban_heartbeat").unwrap().effect(),
            ToolEffect::Write
        );
    }
}
