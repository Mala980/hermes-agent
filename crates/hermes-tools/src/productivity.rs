//! Tools produktivitas: todo_list, clarify, delegate_task.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex as StdMutex;

// ---------------------------------------------------------------------------
// todo_list — daftar tugas untuk pekerjaan multi-langkah
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TodoItem {
    id: String,
    content: String,
    status: String, // pending | in_progress | completed
    /// Batch 8: id tugas induk; item menjadi subtugas (seperti aslinya).
    #[serde(default)]
    parent: Option<String>,
}

/// Bersihkan relasi parent: kosong/self/dangling → root; siklus diputus
/// (seperti `_sanitize_parents` aslinya).
fn sanitize_todo_parents(list: &mut [TodoItem]) {
    // Kumpulkan id sebagai String milik sendiri agar closure di bawah tidak
    // meminjam `list` saat iterasi mutabel berjalan.
    let ids: std::collections::HashSet<String> = list.iter().map(|t| t.id.clone()).collect();
    for t in list.iter_mut() {
        let keep = t
            .parent
            .as_deref()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty() && *p != t.id && ids.contains(*p));
        t.parent = keep.map(|p| p.to_string());
    }
    // Putus siklus: item yang parent-nya membentuk lingkaran menjadi root.
    for i in 0..list.len() {
        let mut seen = std::collections::HashSet::new();
        seen.insert(list[i].id.as_str());
        let mut cur = list[i].parent.as_deref();
        let mut cyclic = false;
        while let Some(p) = cur {
            if !seen.insert(p) {
                cyclic = true;
                break;
            }
            cur = list
                .iter()
                .find(|t| t.id == p)
                .and_then(|t| t.parent.as_deref());
        }
        if cyclic {
            list[i].parent = None;
        }
    }
}

/// Render nested: root dulu (urutan authored dipertahankan), anak menjorok
/// 2 spasi per kedalaman (seperti `format_for_injection` aslinya).
fn render_todo_list(items: &[TodoItem]) -> String {
    let mark = |t: &TodoItem| -> &'static str {
        match t.status.as_str() {
            "completed" => "✅",
            "in_progress" => "🔄",
            "cancelled" => "🚫",
            _ => "⬜",
        }
    };
    fn render_into(
        items: &[TodoItem],
        id: &str,
        depth: usize,
        out: &mut Vec<String>,
        mark: &dyn Fn(&TodoItem) -> &'static str,
    ) {
        let item = match items.iter().find(|t| t.id == id) {
            Some(t) => t,
            None => return,
        };
        out.push(format!(
            "{}{} [{}] {}",
            "  ".repeat(depth),
            mark(item),
            item.id,
            item.content
        ));
        for kid in items.iter().filter(|t| t.parent.as_deref() == Some(id)) {
            render_into(items, &kid.id, depth + 1, out, mark);
        }
    }
    let mut lines = Vec::new();
    for root in items.iter().filter(|t| t.parent.is_none()) {
        render_into(items, &root.id, 0, &mut lines, &mark);
    }
    lines.join("\n")
}

static TODOS: StdMutex<Option<HashMap<String, Vec<TodoItem>>>> = StdMutex::new(None);

fn todo_store() -> std::sync::MutexGuard<'static, Option<HashMap<String, Vec<TodoItem>>>> {
    TODOS.lock().unwrap()
}

pub struct TodoListTool;

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "todo_list"
    }

    fn description(&self) -> &str {
        "Kelola daftar tugas untuk pekerjaan multi-langkah (3+ langkah). \
         Pakai `write` untuk menyimpan/memperbarui daftar secara upsert per id: item dengan id \
         yang sudah ada diperbarui di tempat (urutan dipertahankan), id baru ditambahkan di akhir; \
         `list` untuk melihat, `clear` untuk menghapus. Tandai tepat satu tugas sebagai in_progress. \
         Item boleh punya `parent` (id tugas induk) untuk membuat subtugas bersarang."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "action",
                    string_prop("write | list | clear (default: list)."),
                ),
                (
                    "todos",
                    serde_json::json!({
                        "type": "array",
                        "description": "Daftar tugas (wajib untuk action=write). Tiap item: {id, content, status, parent?} — `parent` (id tugas induk, opsional) membuat subtugas bersarang.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                                "parent": {"type": "string", "description": "Id tugas induk (opsional); item menjadi subtugas."}
                            },
                            "required": ["id", "content", "status"]
                        }
                    }),
                ),
            ],
            vec!["action"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let action = opt_str(&args, "action").unwrap_or("list");
        let key = ctx.working_dir.display().to_string();
        let mut store = todo_store();
        let map = store.get_or_insert_with(HashMap::new);
        let content = match action {
            "write" => {
                let arr = args
                    .get("todos")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        HermesError::tool("todo_list", "action=write butuh array `todos`")
                    })?;
                let mut incoming = Vec::new();
                for (i, v) in arr.iter().enumerate() {
                    let item: TodoItem = serde_json::from_value(v.clone()).map_err(|e| {
                        HermesError::tool("todo_list", format!("item {i} tidak valid: {e}"))
                    })?;
                    // B31: `cancelled` adalah status yang sah (seperti aslinya).
                    if !["pending", "in_progress", "completed", "cancelled"]
                        .contains(&item.status.as_str())
                    {
                        return Err(HermesError::tool(
                            "todo_list",
                            format!("status item {} tidak valid", item.id),
                        ));
                    }
                    incoming.push(item);
                }
                // B30: upsert per id — item dengan id yang sudah ada diperbarui
                // di tempat (urutan dipertahankan), id baru ditambahkan di akhir.
                // Duplikat id dalam satu write: kemunculan terakhir menang
                // (seperti aslinya).
                let list = map.entry(key).or_default();
                let mut seen = HashSet::new();
                let mut deduped: Vec<TodoItem> = Vec::with_capacity(incoming.len());
                for item in incoming.into_iter().rev() {
                    if seen.insert(item.id.clone()) {
                        deduped.push(item);
                    }
                }
                deduped.reverse();
                let (mut updated, mut added) = (0usize, 0usize);
                for item in deduped {
                    match list.iter_mut().find(|t| t.id == item.id) {
                        Some(slot) => {
                            *slot = item;
                            updated += 1;
                        }
                        None => {
                            list.push(item);
                            added += 1;
                        }
                    }
                }
                // Batch 8: sanitasi parent (dangling/self/siklus → root),
                // seperti aslinya.
                sanitize_todo_parents(list);
                // B31: >1 in_progress hanya panduan, bukan error (seperti
                // aslinya) — write tetap diterima dengan catatan.
                let n_ip = list.iter().filter(|t| t.status == "in_progress").count();
                let mut content = format!(
                    "Daftar tugas disimpan ({} item: {updated} diperbarui, {added} baru).",
                    list.len()
                );
                if n_ip > 1 {
                    content.push_str(
                        " Catatan: sebaiknya hanya satu tugas berstatus in_progress dalam satu waktu.",
                    );
                }
                content
            }
            "clear" => {
                map.remove(&key);
                "Daftar tugas dihapus.".to_string()
            }
            _ => {
                let items = map.get(&key).cloned().unwrap_or_default();
                if items.is_empty() {
                    "Daftar tugas kosong.".to_string()
                } else {
                    // Batch 8: render bersarang (subtugas menjorok).
                    render_todo_list(&items)
                }
            }
        };
        Ok(ok_result(self, content))
    }
}

// ---------------------------------------------------------------------------
// clarify — tanya pengguna bila butuh keputusan
// ---------------------------------------------------------------------------

pub struct ClarifyTool;

#[async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str {
        "clarify"
    }

    fn description(&self) -> &str {
        "Ajukan 1-4 pertanyaan klarifikasi ke pengguna saat butuh keputusan sebelum lanjut. \
         Tiap pertanyaan boleh punya opsi pilihan. Di sesi non-interaktif, tool mengembalikan \
         instruksi untuk lanjut dengan asumsi terbaik."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![(
                "questions",
                serde_json::json!({
                    "type": "array",
                    "description": "Daftar pertanyaan (1-4).",
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "header": {"type": "string"},
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": {"type": "string"},
                                        "description": {"type": "string"}
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["question"]
                    }
                }),
            )],
            vec!["questions"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let questions = args
            .get("questions")
            .and_then(|v| v.as_array())
            .ok_or_else(|| HermesError::tool("clarify", "butuh array `questions`"))?;
        if questions.is_empty() || questions.len() > 4 {
            return Err(HermesError::tool("clarify", "ajukan 1-4 pertanyaan"));
        }
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            return Ok(ok_result(
                self,
                "Sesi non-interaktif: tidak bisa bertanya ke pengguna. \
                 Lanjut dengan asumsi terbaik dan catat asumsi itu di jawaban akhir."
                    .to_string(),
            ));
        }
        // Mode interaktif: tampilkan pertanyaan, baca jawaban dari stdin.
        use tokio::io::AsyncBufReadExt;
        let stdin = tokio::io::stdin();
        let mut reader = tokio::io::BufReader::new(stdin);
        let mut answers = Vec::new();
        for (i, q) in questions.iter().enumerate() {
            let question = q.get("question").and_then(|s| s.as_str()).unwrap_or("?");
            let header = q
                .get("header")
                .and_then(|s| s.as_str())
                .unwrap_or("Pertanyaan");
            eprintln!("\n❓ [{header}] {question}");
            if let Some(opts) = q.get("options").and_then(|o| o.as_array()) {
                for (j, o) in opts.iter().enumerate() {
                    let label = o.get("label").and_then(|l| l.as_str()).unwrap_or("?");
                    let desc = o.get("description").and_then(|d| d.as_str()).unwrap_or("");
                    eprintln!("   {}. {label} {desc}", j + 1);
                }
                eprint!("   Jawab (nomor/teks, Enter = lewati): ");
            } else {
                eprint!("   Jawab (Enter = lewati): ");
            }
            use std::io::Write as _;
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            // Timeout 5 menit agar tidak menggantung selamanya.
            let read = tokio::time::timeout(
                std::time::Duration::from_secs(300),
                reader.read_line(&mut line),
            )
            .await
            .map_err(|_| HermesError::tool("clarify", "waktu habis menunggu jawaban"))?
            .map_err(|e| HermesError::tool("clarify", format!("baca stdin gagal: {e}")))?;
            if read == 0 {
                break;
            }
            answers.push(serde_json::json!({
                "question": question,
                "answer": line.trim(),
            }));
            let _ = i;
        }
        Ok(ok_result(
            self,
            format!(
                "Jawaban pengguna:\n{}",
                serde_json::to_string_pretty(&answers).unwrap_or_default()
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// delegate_task — jalankan subagen terisolasi via subproses biner yang sama
// ---------------------------------------------------------------------------

pub struct DelegateTaskTool;

#[async_trait]
impl Tool for DelegateTaskTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn description(&self) -> &str {
        "Delegasikan satu tugas mandiri ke subagen dalam konteks terisolasi. \
         Subagen berjalan sebagai proses `hermes run` terpisah dengan sesi sendiri \
         dan mengembalikan hasil akhirnya sebagai teks. Cocok untuk riset paralel, \
         eksplorasi kode, atau tugas yang tidak butuh konteks percakapan ini."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "description",
                    string_prop("Deskripsi singkat tugas (3-5 kata)."),
                ),
                (
                    "prompt",
                    string_prop("Instruksi lengkap untuk subagen, mandiri tanpa konteks ini."),
                ),
                (
                    "timeout_secs",
                    integer_prop("Batas waktu detik (default 600, maks 1800)."),
                ),
            ],
            vec!["description", "prompt"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let description = get_str(&args, "description")?;
        let prompt = get_str(&args, "prompt")?;
        let timeout = opt_u64(&args, "timeout_secs")
            .unwrap_or(600)
            .clamp(30, 1800);

        let exe = std::env::current_exe()
            .map_err(|e| HermesError::tool("delegate_task", format!("current_exe gagal: {e}")))?;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let session_id = format!("child-{ts}");

        eprintln!("🔄 mendelegasikan: {description} (sesi {session_id})");
        let mut cmd = tokio::process::Command::new(&exe);
        cmd.args(["run", "--session", &session_id, "--print", "-q", prompt])
            .current_dir(&ctx.working_dir)
            .envs(&ctx.env)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let output = tokio::time::timeout(std::time::Duration::from_secs(timeout), cmd.output())
            .await
            .map_err(|_| HermesError::tool("delegate_task", "subagen kehabisan waktu"))?
            .map_err(|e| {
                HermesError::tool("delegate_task", format!("gagal menjalankan subagen: {e}"))
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if !output.status.success() {
            return Err(HermesError::tool(
                "delegate_task",
                format!(
                    "subagen gagal (exit {}): {}",
                    output.status,
                    truncate(&stderr, 2000)
                ),
            ));
        }
        let content = if stdout.is_empty() {
            "(subagen tidak mengembalikan teks)".to_string()
        } else {
            stdout
        };
        Ok(ok_result(
            self,
            format!(
                "Hasil subagen [{description}]:\n{}",
                truncate(&content, 12000)
            ),
        ))
    }
}

// ---------------------------------------------------------------------------

fn ok_result(tool: &dyn Tool, content: String) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        name: tool.name().to_string(),
        content,
        is_error: false,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [dipotong]", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_ctx;

    /// Konteks uji dengan working_dir unik per tes — store todo di-key per
    /// working_dir dan bersifat global, jadi isolasi ini mencegah tes paralel
    /// saling menimpa.
    fn ctx_for(name: &str) -> ToolContext {
        test_ctx(std::path::PathBuf::from(format!(
            "/tmp/hermes-todo-test-{}-{name}",
            std::process::id()
        )))
    }

    fn write_args(items: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"action": "write", "todos": items})
    }

    fn item(id: &str, content: &str, status: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "content": content, "status": status})
    }

    /// B30: write adalah upsert — id yang ada diperbarui di tempat (urutan
    /// dipertahankan), id baru ditambahkan di akhir.
    #[tokio::test]
    async fn todo_write_merges_by_id_preserving_order() {
        let tool = TodoListTool;
        let ctx = ctx_for("merge");
        let r = tool
            .execute(
                write_args(serde_json::json!([
                    item("a", "tugas A", "pending"),
                    item("b", "tugas B", "pending"),
                    item("c", "tugas C", "pending"),
                ])),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r.is_error, "write awal: {}", r.content);

        let r = tool
            .execute(
                write_args(serde_json::json!([
                    item("b", "tugas B revisi", "completed"),
                    item("d", "tugas D", "pending"),
                ])),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!r.is_error, "write upsert: {}", r.content);
        assert!(
            r.content.contains("1 diperbarui"),
            "write upsert: {}",
            r.content
        );
        assert!(r.content.contains("1 baru"), "write upsert: {}", r.content);

        let r = tool
            .execute(serde_json::json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines.len(), 4, "list: {}", r.content);
        assert!(
            lines[0].contains("[a]") && lines[0].contains("tugas A"),
            "urutan a: {}",
            r.content
        );
        assert!(
            lines[1].contains("[b]")
                && lines[1].contains("tugas B revisi")
                && lines[1].contains('✅'),
            "b diperbarui di tempat: {}",
            r.content
        );
        assert!(lines[2].contains("[c]"), "urutan c: {}", r.content);
        assert!(lines[3].contains("[d]"), "d di akhir: {}", r.content);

        // clear tetap wholesale.
        let r = tool
            .execute(serde_json::json!({"action": "clear"}), &ctx)
            .await
            .unwrap();
        assert!(!r.is_error);
        let r = tool
            .execute(serde_json::json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        assert!(r.content.contains("kosong"), "pasca-clear: {}", r.content);
    }

    /// B31: status `cancelled` diterima (bukan error).
    #[tokio::test]
    async fn todo_cancelled_status_accepted() {
        let tool = TodoListTool;
        let ctx = ctx_for("cancelled");
        tool.execute(
            write_args(serde_json::json!([item("a", "tugas A", "pending")])),
            &ctx,
        )
        .await
        .unwrap();
        let r = tool
            .execute(
                write_args(serde_json::json!([item("a", "tugas A", "cancelled")])),
                &ctx,
            )
            .await
            .expect("status cancelled harus diterima");
        assert!(!r.is_error, "{}", r.content);
        let r = tool
            .execute(serde_json::json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        assert!(r.content.contains("[a]"), "list: {}", r.content);
        assert!(r.content.contains('🚫'), "penanda cancelled: {}", r.content);
    }

    /// B31: >1 in_progress hanya panduan (catatan), bukan error — write tetap
    /// diterima.
    #[tokio::test]
    async fn todo_multiple_in_progress_is_guidance_not_error() {
        let tool = TodoListTool;
        let ctx = ctx_for("inprog");
        let r = tool
            .execute(
                write_args(serde_json::json!([
                    item("a", "tugas A", "in_progress"),
                    item("b", "tugas B", "in_progress"),
                ])),
                &ctx,
            )
            .await
            .expect(">1 in_progress harus diterima (panduan, bukan error)");
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("in_progress"),
            "catatan panduan hilang: {}",
            r.content
        );
    }

    /// Status yang benar-benar tidak dikenal tetap ditolak.
    #[tokio::test]
    async fn todo_invalid_status_still_rejected() {
        let tool = TodoListTool;
        let ctx = ctx_for("badstatus");
        let r = tool
            .execute(
                write_args(serde_json::json!([item("a", "tugas A", "bogus")])),
                &ctx,
            )
            .await;
        assert!(r.is_err(), "status bogus harus ditolak");
    }

    /// Batch 8: `parent` membuat subtugas bersarang; render menjorok.
    #[tokio::test]
    async fn todo_subtasks_nested_render() {
        let tool = TodoListTool;
        let ctx = ctx_for("subtasks");
        tool.execute(
            write_args(serde_json::json!([
                item("a", "induk A", "pending"),
                {"id": "a1", "content": "anak A1", "status": "completed", "parent": "a"},
                {"id": "a2", "content": "anak A2", "status": "in_progress", "parent": "a"},
                {"id": "a2i", "content": "cucu A2i", "status": "pending", "parent": "a2"},
                item("b", "induk B", "pending"),
            ])),
            &ctx,
        )
        .await
        .expect("write subtugas gagal");
        let r = tool
            .execute(serde_json::json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        let lines: Vec<&str> = r.content.lines().collect();
        assert_eq!(lines.len(), 5, "list: {}", r.content);
        assert!(lines[0].starts_with("⬜ [a]"), "root: {}", r.content);
        assert!(
            lines[1].starts_with("  ✅ [a1]"),
            "anak menjorok: {}",
            r.content
        );
        assert!(
            lines[2].starts_with("  🔄 [a2]"),
            "anak menjorok: {}",
            r.content
        );
        assert!(
            lines[3].starts_with("    ⬜ [a2i]"),
            "cucu menjorok 2x: {}",
            r.content
        );
        assert!(lines[4].starts_with("⬜ [b]"), "root kedua: {}", r.content);
    }

    /// Batch 8: parent dangling/self/siklus disanitasi. Dangling dan self
    /// menjadi root; siklus diputus seperti aslinya (item pertama dalam
    /// urutan menjadi root, pasangannya tetap menjadi anaknya).
    #[tokio::test]
    async fn todo_parent_dangling_dan_siklus_disanitasi() {
        let tool = TodoListTool;
        let ctx = ctx_for("sanitize");
        tool.execute(
            write_args(serde_json::json!([
                {"id": "x", "content": "parent hilang", "status": "pending", "parent": "ghost"},
                {"id": "y", "content": "parent diri sendiri", "status": "pending", "parent": "y"},
                {"id": "p", "content": "siklus p", "status": "pending", "parent": "q"},
                {"id": "q", "content": "siklus q", "status": "pending", "parent": "p"},
            ])),
            &ctx,
        )
        .await
        .expect("write gagal");
        let r = tool
            .execute(serde_json::json!({"action": "list"}), &ctx)
            .await
            .unwrap();
        let lines: Vec<&str> = r.content.lines().collect();
        // x, y, p menjadi root; q tetap anak p (semantik asli).
        assert_eq!(lines.len(), 4, "list: {}", r.content);
        assert!(lines[0].starts_with("⬜ [x]"), "x root: {}", r.content);
        assert!(lines[1].starts_with("⬜ [y]"), "y root: {}", r.content);
        assert!(lines[2].starts_with("⬜ [p]"), "p root: {}", r.content);
        assert!(lines[3].starts_with("  ⬜ [q]"), "q anak p: {}", r.content);
    }

    /// Batch 8: skema menyatakan `parent` opsional.
    #[test]
    fn todo_schema_menyatakan_parent() {
        let s = TodoListTool.parameters_schema().to_string();
        assert!(s.contains("\"parent\""), "skema: {s}");
    }
}
