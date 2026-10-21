//! Tool kanban Worker C: review, komentar, attachment, dan link.
//!
//! Cerminan Rust dari tool asli (`tools/kanban_tools_schemas.py`,
//! `tools/kanban_tools.py`):
//!
//! | # | Nama | Skema asli |
//! |---|------|------------|
//! | 10 | `kanban_request_review` | `KANBAN_REQUEST_REVIEW_SCHEMA` |
//! | 11 | `kanban_request_changes` | `KANBAN_REQUEST_CHANGES_SCHEMA` |
//! | 13 | `kanban_comment` | `KANBAN_COMMENT_SCHEMA` |
//! | 14 | `kanban_attach` | `KANBAN_ATTACH_SCHEMA` |
//! | 15 | `kanban_attach_url` | `KANBAN_ATTACH_URL_SCHEMA` |
//! | 16 | `kanban_attachments` | `KANBAN_ATTACHMENTS_SCHEMA` |
//! | 18 | `kanban_link` | `KANBAN_LINK_SCHEMA` |
//!
//! Beda dengan aslinya: port ini tidak punya machinery
//! dispatcher/claim/run (`expected_run_id`, `claim_lock`, `task_runs`),
//! jadi guard kepemilikan worker (`_worker_guard` di aslinya) disederhanakan
//! menjadi resolusi `task_id` (argumen → env `HERMES_KANBAN_TASK`).
//! Transisi status tetap divalidasi lewat matriks workflow
//! (`BoardDb::move_task`).

use super::store::{
    kanban_home, normalize_board_slug, BoardDb, KanbanStore, TaskPatch, MAX_ATTACHMENT_BYTES,
};
use crate::schema::*;
use crate::web::ensure_url_allowed;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use futures::StreamExt;
use hermes_core::{HermesError, Result, Tool, ToolContext, ToolEffect, ToolRegistry, ToolResult};
use serde_json::{json, Value};
use std::path::Path;

// ---------------------------------------------------------------------------
// Helper internal (cerminan helper privat di `tools.rs`)
// ---------------------------------------------------------------------------

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

fn task_id_prop() -> Value {
    string_prop(
        "Id task. Bila kosong dipakai HERMES_KANBAN_TASK dari env \
         (task milik worker yang di-spawn dispatcher).",
    )
}

/// Resolusi `task_id` ala `_require_task_id`/`_worker_guard` di aslinya
/// (tanpa machinery claim dispatcher): argumen → env `HERMES_KANBAN_TASK`.
fn resolve_task_id(args: &Value, ctx: &ToolContext, tool: &str) -> Result<String> {
    if let Some(t) = opt_str(args, "task_id").filter(|s| !s.trim().is_empty()) {
        return Ok(t.to_string());
    }
    ctx.env
        .get("HERMES_KANBAN_TASK")
        .filter(|s| !s.trim().is_empty())
        .cloned()
        .ok_or_else(|| {
            HermesError::tool(
                tool,
                "task_id wajib diisi (atau set HERMES_KANBAN_TASK); \
                 temukan id dengan kanban_list",
            )
        })
}

fn open_db(args: &Value, ctx: &ToolContext, tool: &str) -> Result<BoardDb> {
    let store = store_for(ctx, tool)?;
    let board = resolve_board(args, ctx, &store, tool)?;
    store
        .board_db(&board)
        .map_err(|e| HermesError::tool(tool, e.to_string()))
}

fn get_task(db: &BoardDb, tool: &str, task_id: &str) -> Result<super::store::KanbanTask> {
    db.get_task(task_id)
        .map_err(|e| HermesError::tool(tool, e.to_string()))?
        .ok_or_else(|| HermesError::tool(tool, format!("task '{task_id}' tidak ada")))
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

// ---------------------------------------------------------------------------
// kanban_request_review — port dari _handle_request_review aslinya
// ---------------------------------------------------------------------------

pub struct KanbanRequestReviewTool;

#[async_trait]
impl Tool for KanbanRequestReviewTool {
    fn name(&self) -> &str {
        "kanban_request_review"
    }
    fn description(&self) -> &str {
        "Serahkan task untuk review: implementasi, self-review, dan verifikasi \
         selesai dan kamu ingin reviewer melihat sebelum task ditandai done. \
         Memindahkan task ke kolom 'review'. BUKAN blocker — tidak pernah \
         dihitung ke deteksi unblock-loop, jadi task boleh bolak-balik review \
         tanpa salah dieskalasi ke triage."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "summary",
                    string_prop(
                        "Apa yang diimplementasikan dan bagaimana diverifikasinya, \
                         satu-dua kalimat — ditampilkan ke reviewer. Jangan tempel \
                         seluruh diff; reviewer punya board dan PR.",
                    ),
                ),
                (
                    "reviewer",
                    string_prop(
                        "Profil reviewer opsional. Bila diisi, task di-assign ulang \
                         ke profil itu sebelum dispatch review.",
                    ),
                ),
                (
                    "metadata",
                    json!({"type": "object",
                           "description": "Fakta handoff terstruktur opsional untuk reviewer, \
                           mis. changed_files, tests_run, commit, atau decisions.",
                           "additionalProperties": true}),
                ),
                (
                    "artifacts",
                    json!({"type": "array", "items": {"type": "string"},
                           "description": "Daftar path absolut file deliverable yang \
                           dinamai handoff ini — chart, PDF, spreadsheet, gambar, arsip. \
                           File-file ini disalin ke attachment durable task sebelum \
                           transisi; path yang hilang membatalkan seluruh handoff \
                           (task tetap di tempat, bisa diperbaiki dan dicoba lagi)."}),
                ),
                ("board", board_prop()),
            ],
            vec!["summary"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let task_id = resolve_task_id(&args, ctx, self.name())?;
        let summary = opt_str(&args, "summary")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                HermesError::tool(
                    self.name(),
                    "summary wajib diisi — jelaskan apa yang diimplementasikan \
                     dan bagaimana diverifikasi agar reviewer punya konteks",
                )
            })?;
        let reviewer = opt_str(&args, "reviewer")
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.to_string());
        let metadata: Option<&Value> = match args.get("metadata") {
            None | Some(Value::Null) => None,
            Some(v @ Value::Object(_)) => Some(v),
            Some(_) => {
                return Err(HermesError::tool(
                    self.name(),
                    "`metadata` harus berupa object",
                ))
            }
        };
        let artifacts = opt_string_array(&args, "artifacts")?;

        let db = open_db(&args, ctx, self.name())?;
        let task = get_task(&db, self.name(), &task_id)?;
        if task.status != "running" && task.status != "ready" {
            return Err(HermesError::tool(
                self.name(),
                format!(
                    "tidak bisa request review untuk {task_id}: task tidak di \
                     running/ready (status: '{}')",
                    task.status
                ),
            ));
        }

        // Tahap dulu semua artifact SEBELUM ada perubahan state — cerminan
        // ArtifactPreservationError di aslinya: path yang hilang membatalkan
        // seluruh handoff, task tetap di tempat dan bisa dicoba lagi.
        let mut staged: Vec<(String, Vec<u8>)> = Vec::new();
        for path in &artifacts {
            let bytes = tokio::fs::read(path).await.map_err(|e| {
                HermesError::tool(
                    self.name(),
                    format!(
                        "kanban_request_review tidak bisa mengawetkan artifact \
                         '{path}': {e}. Task masih in-flight (tidak ada perubahan \
                         state). Perbaiki path-nya lalu ulangi handoff yang sama."
                    ),
                )
            })?;
            let filename = Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "artifact".to_string());
            staged.push((filename, bytes));
        }

        let implementer = task.assignee.clone();
        if let Some(ref r) = reviewer {
            db.update_task(
                &task_id,
                TaskPatch {
                    assignee: Some(Some(r.clone())),
                    ..Default::default()
                },
            )
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        }
        db.move_task(&task_id, "review", Some("review requested"))
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        let mut staged_names = Vec::new();
        for (filename, bytes) in &staged {
            let att = db
                .add_attachment_bytes(&task_id, filename, None, bytes, &created_by(ctx))
                .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
            staged_names.push(att.filename);
        }

        db.record_event(
            &task_id,
            "review_requested",
            Some(
                &json!({
                    "summary": summary,
                    "implementer": implementer,
                    "reviewer": reviewer,
                    "metadata": metadata,
                    "artifacts": staged_names,
                })
                .to_string(),
            ),
        )
        .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "task_id": task_id,
                "status": "review",
                "implementer": implementer,
                "reviewer": reviewer,
                "artifacts_staged": staged_names,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_request_changes — port dari _handle_request_changes aslinya
// ---------------------------------------------------------------------------

pub struct KanbanRequestChangesTool;

#[async_trait]
impl Tool for KanbanRequestChangesTool {
    fn name(&self) -> &str {
        "kanban_request_changes"
    }
    fn description(&self) -> &str {
        "Verdict reviewer: kembalikan review run yang aktif ke implementer \
         semula dengan daftar perubahan konkret yang wajib dikerjakan. Menutup \
         review run, menerapkan ulang parent dependency gating, dan me-requeue \
         task tanpa memakai akuntansi block-loop. Hanya dipakai dari task yang \
         di-claim dari kolom review; kanban_block hanya untuk blocker eksternal \
         yang asli."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "reason",
                    string_prop(
                        "Perubahan spesifik dan actionable yang harus dilakukan \
                         implementer sebelum meminta review lagi.",
                    ),
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
        let task_id = resolve_task_id(&args, ctx, self.name())?;
        let reason = opt_str(&args, "reason")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                HermesError::tool(
                    self.name(),
                    "reason wajib diisi — jelaskan perubahan yang dibutuhkan",
                )
            })?;

        let db = open_db(&args, ctx, self.name())?;
        let task = get_task(&db, self.name(), &task_id)?;
        if task.status != "review" {
            return Err(HermesError::tool(
                self.name(),
                format!(
                    "tidak bisa request changes untuk {task_id}: task tidak di \
                     kolom review (status: '{}')",
                    task.status
                ),
            ));
        }

        // Implementer diambil dari event review_requested terakhir — cerminan
        // provenance `implementer` di aslinya.
        let implementer = db
            .list_events(&task_id, 100)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?
            .into_iter()
            .find(|e| e.kind == "review_requested")
            .and_then(|e| {
                e.payload
                    .as_deref()
                    .and_then(|p| serde_json::from_str::<Value>(p).ok())
            })
            .and_then(|v| {
                v.get("implementer")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
            .ok_or_else(|| {
                HermesError::tool(
                    self.name(),
                    format!(
                        "tidak ada event review_requested dengan provenance \
                         implementer yang valid untuk {task_id} — jalankan \
                         kanban_request_review dulu"
                    ),
                )
            })?;
        let reviewer = task.assignee.clone();

        // Kembalikan assignee ke implementer, lalu hitung status pendaratan:
        // `ready` bila semua parent done, selain itu `todo` (parent gating
        // diterapkan ulang, seperti `_landing_status_after_parents` aslinya).
        db.update_task(
            &task_id,
            TaskPatch {
                assignee: Some(Some(implementer.clone())),
                ..Default::default()
            },
        )
        .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        let mut landing = "ready";
        for parent_id in db
            .parents_of(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?
        {
            match db
                .get_task(&parent_id)
                .map_err(|e| HermesError::tool(self.name(), e.to_string()))?
            {
                Some(p) if p.status == "done" => {}
                _ => {
                    landing = "todo";
                    break;
                }
            }
        }
        db.move_task(&task_id, landing, Some("changes requested"))
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;
        db.record_event(
            &task_id,
            "changes_requested",
            Some(
                &json!({
                    "reason": reason,
                    "implementer": implementer,
                    "reviewer": reviewer,
                    "status": landing,
                })
                .to_string(),
            ),
        )
        .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "task_id": task_id,
                "status": landing,
                "implementer": implementer,
                "reviewer": reviewer,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_comment — port dari _handle_comment aslinya
// ---------------------------------------------------------------------------

pub struct KanbanCommentTool;

#[async_trait]
impl Tool for KanbanCommentTool {
    fn name(&self) -> &str {
        "kanban_comment"
    }
    fn description(&self) -> &str {
        "Tambahkan komentar ke thread sebuah task. Untuk catatan durable yang \
         harus hidup lebih lama dari run ini (pertanyaan untuk worker \
         berikutnya, temuan parsial, rationale). Penalaran ephemeral tidak \
         cocok di sini — pakai respons normal saja."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                (
                    "task_id",
                    string_prop(
                        "Id task (wajib; boleh task sendiri atau milik orang \
                         lain — thread komentar bersifat per-task).",
                    ),
                ),
                ("body", string_prop("Isi komentar (mendukung Markdown).")),
                ("board", board_prop()),
            ],
            vec!["task_id", "body"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        // Seperti aslinya: komentar menulis ke board, jadi panggilan telanjang
        // tanpa task_id tetap error — arahkan ke kanban_list, bukan env var.
        let task_id = opt_str(&args, "task_id")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                HermesError::tool(
                    self.name(),
                    "task_id wajib diisi: komentar bersifat per-task dan sesi \
                     ini tidak punya task default. Temukan id task dengan \
                     kanban_list.",
                )
            })?;
        let body = get_str(&args, "body")?;
        // Author berasal dari identitas runtime (profil), TIDAK PERNAH dari
        // argumen — cerminan #19713 di aslinya: komentar disuntik ke system
        // prompt worker berikutnya, jadi override author bisa memalsukan
        // direktif dari nama otoritatif seperti `hermes-system`.
        let author = created_by(ctx);

        let db = open_db(&args, ctx, self.name())?;
        let comment = db
            .add_comment(task_id, &author, body)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "task_id": task_id,
                "comment_id": comment.id,
                "author": comment.author,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_attach — port dari _handle_attach aslinya
// ---------------------------------------------------------------------------

pub struct KanbanAttachTool;

#[async_trait]
impl Tool for KanbanAttachTool {
    fn name(&self) -> &str {
        "kanban_attach"
    }
    fn description(&self) -> &str {
        "Lampirkan file ke task dengan bytes inline (base64). Untuk artefak \
         file asli yang harus bisa diunduh worker berikutnya atau manusia — \
         laporan, gambar, ekspor. File disimpan sebagai attachment sungguhan \
         (bukan tautan komentar) di direktori attachment task, dibatasi 25 MB. \
         Bila hanya punya URL, pakai kanban_attach_url."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "filename",
                    string_prop(
                        "Nama file untuk menyimpannya (mis. 'report.pdf'). \
                         Komponen direktori dibuang; hanya leaf yang dipakai.",
                    ),
                ),
                (
                    "content_base64",
                    string_prop("Isi file, base64-encoded. Maks 25 MB setelah decode."),
                ),
                (
                    "content_type",
                    string_prop("Tipe MIME opsional (mis. 'application/pdf')."),
                ),
                ("board", board_prop()),
            ],
            vec!["filename", "content_base64"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let task_id = resolve_task_id(&args, ctx, self.name())?;
        let filename = get_str(&args, "filename")?;
        let content_b64 = get_str(&args, "content_base64")?;
        // Ketat seperti `base64.b64decode(validate=True)` di aslinya.
        let data = B64.decode(content_b64).map_err(|e| {
            HermesError::tool(
                self.name(),
                format!("content_base64 bukan base64 valid: {e}"),
            )
        })?;
        let content_type = opt_str(&args, "content_type");

        let db = open_db(&args, ctx, self.name())?;
        let att = db
            .add_attachment_bytes(&task_id, filename, content_type, &data, &created_by(ctx))
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "task_id": task_id,
                "attachment_id": att.id,
                "filename": att.filename,
                "size": att.size,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_attach_url — port dari _handle_attach_url aslinya
// ---------------------------------------------------------------------------

const MAX_ATTACH_URL_REDIRECTS: usize = 5;

/// Fetch `url` lewat http(s) dengan batas `max_bytes` — cerminan
/// `_download_url_with_cap` di aslinya. Setiap hop diperiksa SSRF
/// (`ensure_url_allowed`: skema http(s), tanpa kredensial, bukan
/// privat/internal) dan redirect diikuti manual (maks 5), sehingga URL yang
/// dikontrol model tidak bisa mencapai loopback/private/cloud-metadata.
/// Body dibaca streaming dan batas ukuran dicek saat streaming, jadi tidak
/// ada yang oversize yang di-buffer.
async fn download_url_with_cap(
    url: &str,
    max_bytes: usize,
) -> std::result::Result<(Vec<u8>, Option<String>), String> {
    let client = reqwest::Client::builder()
        .user_agent("hermes-kanban/attach")
        .timeout(std::time::Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("gagal membangun HTTP client: {e}"))?;
    let mut current = url.to_string();
    for _ in 0..=MAX_ATTACH_URL_REDIRECTS {
        ensure_url_allowed(&current).await?;
        let resp = client
            .get(&current)
            .send()
            .await
            .map_err(|e| format!("gagal fetch {current}: {e}"))?;
        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("redirect tanpa header Location dari {current}"))?;
            let base =
                url::Url::parse(&current).map_err(|e| format!("URL tidak valid {current}: {e}"))?;
            current = base
                .join(location)
                .map_err(|e| format!("gagal resolve redirect: {e}"))?
                .to_string();
            continue;
        }
        let resp = resp
            .error_for_status()
            .map_err(|e| format!("HTTP error dari {current}: {e}"))?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.split(';').next().unwrap_or("").trim().to_string())
            .filter(|s| !s.is_empty());
        let mut out = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("gagal membaca body dari {current}: {e}"))?;
            out.extend_from_slice(&chunk);
            if out.len() > max_bytes {
                return Err(format!(
                    "attachment melebihi batas {} MB",
                    max_bytes / (1024 * 1024)
                ));
            }
        }
        return Ok((out, content_type));
    }
    Err(format!("terlalu banyak redirect saat fetch {url}"))
}

/// Decode persen sederhana untuk nama file turunan URL
/// (cerminan `urllib.parse.unquote` di aslinya).
fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((h * 16 + l) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Tentukan nama file untuk `kanban_attach_url`: argumen `filename` eksplisit
/// (atau alias pra-skema `title` yang masih dihormati aslinya) → leaf path
/// URL → `"download"`.
fn derive_filename(url: &str, explicit: Option<&str>) -> String {
    if let Some(f) = explicit.filter(|s| !s.trim().is_empty()) {
        return f.trim().to_string();
    }
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut segs| segs.next_back())
                .map(|leaf| percent_decode(leaf).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "download".to_string())
}

pub struct KanbanAttachUrlTool;

#[async_trait]
impl Tool for KanbanAttachUrlTool {
    fn name(&self) -> &str {
        "kanban_attach_url"
    }
    fn description(&self) -> &str {
        "Lampirkan file ke task via URL — Hermes mengunduhnya server-side dan \
         menyimpannya sebagai attachment sungguhan (dibatasi 25 MB). Untuk \
         kasus kamu punya tautan, bukan bytes-nya. Hanya URL http/https yang \
         diterima; target privat/internal ditolak proteksi SSRF."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("task_id", task_id_prop()),
                (
                    "url",
                    string_prop("URL http(s) yang di-fetch dan disimpan."),
                ),
                (
                    "filename",
                    string_prop(
                        "Nama opsional untuk menyimpannya. Default: komponen \
                         leaf path URL.",
                    ),
                ),
                (
                    "content_type",
                    string_prop(
                        "Override tipe MIME opsional. Default: Content-Type \
                         yang dikembalikan server.",
                    ),
                ),
                ("board", board_prop()),
            ],
            vec!["url"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let task_id = resolve_task_id(&args, ctx, self.name())?;
        let url = get_str(&args, "url")?.trim().to_string();
        // `title` adalah alias pra-skema dari `filename` yang masih dihormati
        // aslinya.
        let explicit = opt_str(&args, "filename").or(opt_str(&args, "title"));
        let filename = derive_filename(&url, explicit);
        let content_type_arg = opt_str(&args, "content_type").map(|s| s.to_string());

        let (data, fetched_ct) = download_url_with_cap(&url, MAX_ATTACHMENT_BYTES)
            .await
            .map_err(|e| HermesError::tool(self.name(), format!("kanban_attach_url: {e}")))?;

        let db = open_db(&args, ctx, self.name())?;
        let ct = content_type_arg.or(fetched_ct);
        let att = db
            .add_attachment_bytes(&task_id, &filename, ct.as_deref(), &data, &created_by(ctx))
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "task_id": task_id,
                "attachment_id": att.id,
                "filename": att.filename,
                "size": att.size,
                "source_url": url,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_attachments — port dari _handle_attachments aslinya
// ---------------------------------------------------------------------------

pub struct KanbanAttachmentsTool;

#[async_trait]
impl Tool for KanbanAttachmentsTool {
    fn name(&self) -> &str {
        "kanban_attachments"
    }
    fn description(&self) -> &str {
        "Daftar file yang dilampirkan ke task: id, filename, content_type, \
         size, siapa pengunggahnya, dan path on-disk absolut yang bisa dibaca. \
         Read-only; tanpa batasan kepemilikan."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![("task_id", task_id_prop()), ("board", board_prop())],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let task_id = resolve_task_id(&args, ctx, self.name())?;
        let db = open_db(&args, ctx, self.name())?;
        get_task(&db, self.name(), &task_id)?;
        let attachments = db
            .list_attachments(&task_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "ok": true,
                "task_id": task_id,
                "attachments": attachments,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// kanban_link — port dari _handle_link aslinya
// ---------------------------------------------------------------------------

pub struct KanbanLinkTool;

#[async_trait]
impl Tool for KanbanLinkTool {
    fn name(&self) -> &str {
        "kanban_link"
    }
    fn description(&self) -> &str {
        "Tambah edge dependensi parent→child setelah kedua task sudah ada. \
         Child tidak akan promosi ke 'ready' sampai semua parent 'done'. \
         Siklus dan self-link ditolak."
    }
    fn parameters_schema(&self) -> Value {
        object_schema(
            vec![
                ("parent_id", string_prop("Id task parent.")),
                ("child_id", string_prop("Id task child.")),
                ("board", board_prop()),
            ],
            vec!["parent_id", "child_id"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }
    async fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolResult> {
        let parent_id = get_str(&args, "parent_id")?;
        let child_id = get_str(&args, "child_id")?;
        let db = open_db(&args, ctx, self.name())?;
        db.add_link(parent_id, child_id)
            .map_err(|e| HermesError::tool(self.name(), e.to_string()))?;

        Ok(ok(
            "",
            self.name(),
            json!({
                "parent_id": parent_id,
                "child_id": child_id,
            })
            .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Registrasi
// ---------------------------------------------------------------------------

/// Daftarkan ketujuh tool Worker C ke registry.
pub fn register_meta_tools(registry: &mut ToolRegistry) {
    registry.register(KanbanRequestReviewTool);
    registry.register(KanbanRequestChangesTool);
    registry.register(KanbanCommentTool);
    registry.register(KanbanAttachTool);
    registry.register(KanbanAttachUrlTool);
    registry.register(KanbanAttachmentsTool);
    registry.register(KanbanLinkTool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_ctx;
    use serde_json::json;

    fn kanban_ctx(tag: &str) -> ToolContext {
        let dir = std::env::temp_dir().join(format!(
            "hermes-kanban-meta-test-{}-{}",
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

    fn mk_task(ctx: &ToolContext, title: &str, assignee: &str) -> String {
        let store = store_for(ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        db.create_task(super::super::store::NewTask {
            title: title.to_string(),
            assignee: assignee.to_string(),
            ..Default::default()
        })
        .unwrap()
        .id
    }

    fn task_status(ctx: &ToolContext, id: &str) -> String {
        let store = store_for(ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        db.get_task(id).unwrap().unwrap().status
    }

    #[tokio::test]
    async fn request_review_happy_path() {
        let ctx = kanban_ctx("review-ok");
        let tool = KanbanRequestReviewTool;
        let tid = mk_task(&ctx, "implementasi x", "dev-a");

        let out = run(
            &tool,
            json!({"task_id": tid, "summary": "menambah endpoint /x, diuji 12 tes",
                   "reviewer": "reviewer-b",
                   "metadata": {"tests_run": 12, "commit": "abc123"}}),
            &ctx,
        )
        .await;
        assert_eq!(out["task_id"], tid);
        assert_eq!(out["status"], "review");
        assert_eq!(out["implementer"], "dev-a");
        assert_eq!(out["reviewer"], "reviewer-b");
        assert_eq!(task_status(&ctx, &tid), "review");

        // Event review_requested tercatat dengan provenance.
        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        let ev = db
            .list_events(&tid, 10)
            .unwrap()
            .into_iter()
            .find(|e| e.kind == "review_requested")
            .expect("event review_requested");
        let payload: Value = serde_json::from_str(ev.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["summary"], "menambah endpoint /x, diuji 12 tes");
        assert_eq!(payload["implementer"], "dev-a");
        assert_eq!(payload["reviewer"], "reviewer-b");
        assert_eq!(payload["metadata"]["tests_run"], 12);
    }

    #[tokio::test]
    async fn request_review_validates() {
        let ctx = kanban_ctx("review-valid");
        let tool = KanbanRequestReviewTool;
        let tid = mk_task(&ctx, "implementasi y", "dev-a");

        // summary wajib.
        assert!(tool.execute(json!({"task_id": tid}), &ctx).await.is_err());
        // metadata harus object.
        assert!(tool
            .execute(
                json!({"task_id": tid, "summary": "s", "metadata": "bukan-object"}),
                &ctx
            )
            .await
            .is_err());
        // task fiktif.
        assert!(tool
            .execute(json!({"task_id": "t_tidakada", "summary": "s"}), &ctx)
            .await
            .is_err());

        // Task di `todo` (bukan running/ready) ditolak — pindah manual ke todo.
        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        db.move_task(&tid, "todo", None).unwrap();
        assert!(tool
            .execute(json!({"task_id": tid, "summary": "s"}), &ctx)
            .await
            .is_err());
        assert_eq!(task_status(&ctx, &tid), "todo");
    }

    #[tokio::test]
    async fn request_review_stages_artifacts() {
        let ctx = kanban_ctx("review-art");
        let tool = KanbanRequestReviewTool;
        let tid = mk_task(&ctx, "laporan", "dev-a");

        let dir =
            std::env::temp_dir().join(format!("hermes-kanban-meta-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f1 = dir.join("laporan.pdf");
        std::fs::write(&f1, b"%PDF-1.4 isi laporan").unwrap();

        let out = run(
            &tool,
            json!({"task_id": tid, "summary": "laporan selesai",
                   "artifacts": [f1.to_string_lossy()]}),
            &ctx,
        )
        .await;
        assert_eq!(out["artifacts_staged"].as_array().unwrap().len(), 1);
        assert_eq!(out["artifacts_staged"][0], "laporan.pdf");

        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        let atts = db.list_attachments(&tid).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "laporan.pdf");

        // Path hilang → seluruh handoff dibatalkan, task tetap di tempat.
        let tid2 = mk_task(&ctx, "laporan 2", "dev-a");
        let err = tool
            .execute(
                json!({"task_id": tid2, "summary": "s",
                       "artifacts": ["/tidak/ada/file.pdf"]}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tidak bisa mengawetkan artifact"));
        assert_eq!(task_status(&ctx, &tid2), "running");
        assert!(db.list_attachments(&tid2).unwrap().is_empty());
        assert!(db
            .list_events(&tid2, 10)
            .unwrap()
            .iter()
            .all(|e| e.kind != "review_requested"));
    }

    #[tokio::test]
    async fn request_changes_roundtrip() {
        let ctx = kanban_ctx("changes-ok");
        let review = KanbanRequestReviewTool;
        let changes = KanbanRequestChangesTool;
        let tid = mk_task(&ctx, "fitur z", "dev-a");

        run(
            &review,
            json!({"task_id": tid, "summary": "fitur z selesai", "reviewer": "rev-b"}),
            &ctx,
        )
        .await;
        // Reviewer mengembalikan ke implementer.
        let out = run(
            &changes,
            json!({"task_id": tid, "reason": "tambah tes untuk edge case kosong"}),
            &ctx,
        )
        .await;
        assert_eq!(out["task_id"], tid);
        assert_eq!(out["implementer"], "dev-a");
        assert_eq!(out["reviewer"], "rev-b");
        // Tanpa parent → landing `ready`.
        assert_eq!(out["status"], "ready");
        assert_eq!(task_status(&ctx, &tid), "ready");

        // Assignee dikembalikan ke implementer.
        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        assert_eq!(
            db.get_task(&tid).unwrap().unwrap().assignee.as_deref(),
            Some("dev-a")
        );
        // Event changes_requested tercatat.
        assert!(db
            .list_events(&tid, 10)
            .unwrap()
            .iter()
            .any(|e| e.kind == "changes_requested"));
    }

    #[tokio::test]
    async fn request_changes_landing_todo_when_parent_open() {
        let ctx = kanban_ctx("changes-gated");
        let review = KanbanRequestReviewTool;
        let changes = KanbanRequestChangesTool;
        let parent = mk_task(&ctx, "fondasi", "dev-a");
        let child = mk_task(&ctx, "lantai 2", "dev-b");
        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        db.add_link(&parent, &child).unwrap();

        run(
            &review,
            json!({"task_id": child, "summary": "lantai 2 selesai"}),
            &ctx,
        )
        .await;
        let out = run(
            &changes,
            json!({"task_id": child, "reason": "perbaiki tangga"}),
            &ctx,
        )
        .await;
        // Parent belum done → landing `todo` (gating diterapkan ulang).
        assert_eq!(out["status"], "todo");
    }

    #[tokio::test]
    async fn request_changes_validates() {
        let ctx = kanban_ctx("changes-valid");
        let changes = KanbanRequestChangesTool;
        let tid = mk_task(&ctx, "tugas", "dev-a");

        // reason wajib.
        assert!(changes
            .execute(json!({"task_id": tid}), &ctx)
            .await
            .is_err());
        // Hanya dari kolom review.
        assert!(changes
            .execute(json!({"task_id": tid, "reason": "ubah"}), &ctx)
            .await
            .is_err());

        // Di review TANPA event review_requested → provenance hilang → error.
        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        db.move_task(&tid, "review", None).unwrap();
        assert!(changes
            .execute(json!({"task_id": tid, "reason": "ubah"}), &ctx)
            .await
            .is_err());
        assert_eq!(task_status(&ctx, &tid), "review");
    }

    #[tokio::test]
    async fn comment_uses_profile_author_and_requires_task() {
        let mut ctx = kanban_ctx("comment");
        ctx.env
            .insert("HERMES_KANBAN_PROFILE".to_string(), "peninjau".to_string());
        let tool = KanbanCommentTool;
        let tid = mk_task(&ctx, "tugas komentar", "dev-a");

        let out = run(
            &tool,
            json!({"task_id": tid, "body": "catatan **penting**"}),
            &ctx,
        )
        .await;
        assert_eq!(out["task_id"], tid);
        assert_eq!(out["author"], "peninjau");

        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        let comments = db.list_comments(&tid).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].author, "peninjau");
        assert_eq!(comments[0].body, "catatan **penting**");

        // Tanpa task_id → error (tidak ada default env untuk komentar).
        assert!(tool.execute(json!({"body": "x"}), &ctx).await.is_err());
        // body kosong → error.
        assert!(tool
            .execute(json!({"task_id": tid, "body": "   "}), &ctx)
            .await
            .is_err());
        // task fiktif → error.
        assert!(tool
            .execute(json!({"task_id": "t_tidakada", "body": "x"}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn attach_stores_bytes_with_strict_base64() {
        let ctx = kanban_ctx("attach");
        let tool = KanbanAttachTool;
        let tid = mk_task(&ctx, "tugas lampiran", "dev-a");

        let out = run(
            &tool,
            json!({"task_id": tid, "filename": "../laporan.pdf",
                   "content_base64": B64.encode(b"isi-pdf"),
                   "content_type": "application/pdf"}),
            &ctx,
        )
        .await;
        assert_eq!(out["task_id"], tid);
        // Komponen direktori dibuang.
        assert_eq!(out["filename"], "laporan.pdf");
        assert_eq!(out["size"], 7);

        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        let atts = db.list_attachments(&tid).unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].content_type.as_deref(), Some("application/pdf"));
        let bytes = std::fs::read(&atts[0].stored_path).unwrap();
        assert_eq!(bytes, b"isi-pdf");

        // base64 tidak valid → error (ketat seperti validate=True).
        assert!(tool
            .execute(
                json!({"task_id": tid, "filename": "x.bin",
                       "content_base64": "!!!bukan-base64!!!"}),
                &ctx
            )
            .await
            .is_err());
        // task fiktif → error.
        assert!(tool
            .execute(
                json!({"task_id": "t_tidakada", "filename": "x",
                       "content_base64": B64.encode(b"x")}),
                &ctx
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn attach_url_rejects_unsafe_and_derives_names() {
        let ctx = kanban_ctx("attach-url");
        let tool = KanbanAttachUrlTool;
        let tid = mk_task(&ctx, "tugas url", "dev-a");

        // Skema non-http ditolak.
        let err = tool
            .execute(
                json!({"task_id": tid, "url": "ftp://example.com/f.bin"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("hanya URL http(s)"), "{err}");
        // Loopback ditolak proteksi SSRF (tanpa jaringan tersentuh).
        let err = tool
            .execute(
                json!({"task_id": tid, "url": "http://127.0.0.1/f.bin"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blocked"), "{err}");

        // Derivasi nama file (murni, tanpa jaringan).
        assert_eq!(
            derive_filename("https://example.com/dir/laporan%20q3.pdf?x=1", None),
            "laporan q3.pdf"
        );
        assert_eq!(
            derive_filename("https://example.com/dir/", None),
            "download"
        );
        assert_eq!(
            derive_filename("https://example.com/a.pdf", Some("  custom.pdf ")),
            "custom.pdf"
        );
        // Alias pra-skema `title` masih dihormati.
        assert_eq!(
            derive_filename("https://example.com/a.pdf", None).as_str(),
            "a.pdf"
        );
        assert_eq!(percent_decode("a%20b%2Fc"), "a b/c");
    }

    #[tokio::test]
    async fn attachments_lists_with_fields() {
        let ctx = kanban_ctx("attachments");
        let attach = KanbanAttachTool;
        let list = KanbanAttachmentsTool;
        let tid = mk_task(&ctx, "tugas daftar", "dev-a");

        // Kosong → list kosong.
        let out = run(&list, json!({"task_id": tid}), &ctx).await;
        assert_eq!(out["ok"], true);
        assert_eq!(out["attachments"].as_array().unwrap().len(), 0);

        run(
            &attach,
            json!({"task_id": tid, "filename": "a.txt",
                   "content_base64": B64.encode(b"abc")}),
            &ctx,
        )
        .await;
        let out = run(&list, json!({"task_id": tid}), &ctx).await;
        let atts = out["attachments"].as_array().unwrap();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0]["filename"], "a.txt");
        assert_eq!(atts[0]["size"], 3);
        assert!(atts[0]["stored_path"].as_str().unwrap().contains(&tid));
        assert_eq!(atts[0]["uploaded_by"], "agent");

        // task fiktif → error.
        assert!(list
            .execute(json!({"task_id": "t_tidakada"}), &ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn link_adds_edge_and_rejects_bad_links() {
        let ctx = kanban_ctx("link");
        let tool = KanbanLinkTool;
        let a = mk_task(&ctx, "task a", "dev-a");
        let b = mk_task(&ctx, "task b", "dev-b");

        let out = run(&tool, json!({"parent_id": a, "child_id": b}), &ctx).await;
        assert_eq!(out["parent_id"], a);
        assert_eq!(out["child_id"], b);

        let store = store_for(&ctx, "test").unwrap();
        let db = store.board_db("default").unwrap();
        assert_eq!(db.parents_of(&b).unwrap(), vec![a.clone()]);

        // Self-link ditolak.
        assert!(tool
            .execute(json!({"parent_id": a, "child_id": a}), &ctx)
            .await
            .is_err());
        // Siklus ditolak.
        assert!(tool
            .execute(json!({"parent_id": b, "child_id": a}), &ctx)
            .await
            .is_err());
        // Task fiktif ditolak.
        assert!(tool
            .execute(json!({"parent_id": a, "child_id": "t_tidakada"}), &ctx)
            .await
            .is_err());
        // Argumen wajib.
        assert!(tool.execute(json!({"parent_id": a}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn meta_tools_register_all_seven() {
        let mut reg = ToolRegistry::new();
        register_meta_tools(&mut reg);
        for name in [
            "kanban_request_review",
            "kanban_request_changes",
            "kanban_comment",
            "kanban_attach",
            "kanban_attach_url",
            "kanban_attachments",
            "kanban_link",
        ] {
            assert!(reg.get(name).is_some(), "tool {name} terdaftar");
        }
        // Skema valid & efek benar.
        assert_eq!(
            reg.get("kanban_request_review").unwrap().effect(),
            ToolEffect::Write
        );
        assert_eq!(
            reg.get("kanban_attachments").unwrap().effect(),
            ToolEffect::Read
        );
        let defs = reg.definitions();
        assert_eq!(defs.len(), 7);
    }

    #[tokio::test]
    async fn task_id_defaults_to_env() {
        let mut ctx = kanban_ctx("env-task");
        let tid = mk_task(&ctx, "tugas env", "dev-a");
        ctx.env
            .insert("HERMES_KANBAN_TASK".to_string(), tid.clone());
        let out = run(&KanbanAttachmentsTool, json!({}), &ctx).await;
        assert_eq!(out["task_id"], tid);
        // kanban_comment sengaja TIDAK pakai default env.
        assert!(KanbanCommentTool
            .execute(json!({"body": "x"}), &ctx)
            .await
            .is_err());
    }
}
