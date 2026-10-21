//! Tool memori: memory_remember, memory_recall, memory_forget, memory_patch,
//! session_search.
//!
//! Paritas `tools/memory_tool.py` aslinya: `target` memilih store
//! (`memory` = catatan agen, `user` = profil pengguna; default `memory`),
//! dan `memory_patch` mengedit satu rentang teks dalam entri (pengganti
//! `replace` yang sudah dipensiunkan aslinya).
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{
    HermesError, MemoryTarget, Result, Role, Session, Tool, ToolContext, ToolEffect, ToolResult,
};
use std::str::FromStr;

/// Ambil `target` dari argumen (default `memory`, seperti aslinya).
fn parse_target(args: &serde_json::Value) -> Result<MemoryTarget> {
    let raw = opt_str(args, "target").unwrap_or("memory");
    MemoryTarget::from_str(raw).map_err(|e| HermesError::tool("memory", e))
}

fn target_prop() -> serde_json::Value {
    string_prop("Target store: 'memory' (catatan agen, default) atau 'user' (profil pengguna).")
}

pub struct MemoryRememberTool;

#[async_trait]
impl Tool for MemoryRememberTool {
    fn name(&self) -> &str {
        "memory_remember"
    }
    fn description(&self) -> &str {
        "Menyimpan fakta penting ke memori persisten (diingat lintas sesi). Target 'memory' = catatan agen, 'user' = profil pengguna."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("text", string_prop("Fakta yang disimpan.")),
                (
                    "kind",
                    string_prop("Kategori: fact, preference, project, lesson (default fact)."),
                ),
                ("target", target_prop()),
            ],
            vec!["text"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let text = get_str(&args, "text")?;
        let kind = opt_str(&args, "kind").unwrap_or("fact");
        let target = parse_target(&args)?;
        let mut entry = hermes_core::MemoryEntry::new(kind, text);
        entry.importance = 0.8;
        entry.target = target;
        let id = entry.id.clone();
        ctx.memory.remember(entry).await?;
        Ok(ok(
            "",
            self.name(),
            format!("tersimpan sebagai memori {id} (target {target})"),
        ))
    }
}

pub struct MemoryRecallTool;

#[async_trait]
impl Tool for MemoryRecallTool {
    fn name(&self) -> &str {
        "memory_recall"
    }
    fn description(&self) -> &str {
        "Mengambil memori persisten yang relevan dengan kueri (full-text search) dari satu target ('memory' default, atau 'user')."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("query", string_prop("Kueri pencarian.")),
                ("limit", integer_prop("Jumlah maks hasil (default 8).")),
                ("target", target_prop()),
            ],
            vec!["query"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let query = get_str(&args, "query")?;
        let limit = opt_u64(&args, "limit").unwrap_or(8) as usize;
        let target = parse_target(&args)?;
        let entries = ctx.memory.recall_for(query, limit, target).await?;
        if entries.is_empty() {
            return Ok(ok(
                "",
                self.name(),
                format!("tidak ada memori '{target}' yang cocok."),
            ));
        }
        let body = entries
            .iter()
            .map(|e| format!("[{}|{}] {}", e.id, e.kind, e.text))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok("", self.name(), body))
    }
}

pub struct MemoryForgetTool;

#[async_trait]
impl Tool for MemoryForgetTool {
    fn name(&self) -> &str {
        "memory_forget"
    }
    fn description(&self) -> &str {
        "Menghapus memori berdasarkan id atau kueri teks, dalam satu target ('memory' default, atau 'user')."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "id_or_query",
                    string_prop("ID memori atau teks yang dicari untuk dihapus."),
                ),
                ("target", target_prop()),
            ],
            vec!["id_or_query"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let q = get_str(&args, "id_or_query")?;
        let target = parse_target(&args)?;
        let n = ctx.memory.forget_for(q, target).await?;
        Ok(ok(
            "",
            self.name(),
            format!("dihapus {n} memori (target {target})"),
        ))
    }
}

pub struct MemoryPatchTool;

#[async_trait]
impl Tool for MemoryPatchTool {
    fn name(&self) -> &str {
        "memory_patch"
    }
    fn description(&self) -> &str {
        "Edit sebagian satu entri memori: cari entri yang memuat `old_text` (cocok toleran-tipografi), ganti SATU rentang itu dengan `new_text`; sisa entri dipertahankan. Rentang yang muncul >1 kali ditolak (ambigu)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "old_text",
                    string_prop("Substring penentu entri SEKALIGUS rentang yang diganti. Salin dari entri yang dimaksud."),
                ),
                (
                    "new_text",
                    string_prop("Teks pengganti untuk rentang old_text."),
                ),
                ("target", target_prop()),
            ],
            vec!["old_text", "new_text"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let old_text = get_str(&args, "old_text")?;
        let new_text = get_str(&args, "new_text")?;
        let target = parse_target(&args)?;
        let updated = ctx.memory.patch_entry(old_text, new_text, target).await?;
        Ok(ok(
            "",
            self.name(),
            format!(
                "entri {} di-patch (target {target}):\n{}",
                updated.id, updated.text
            ),
        ))
    }
}

// ---------------------------------------------------------------------------
// session_search: empat bentuk ala `tools/session_search_tool.py` aslinya.
// ---------------------------------------------------------------------------

/// Batas konten per pesan pada bentuk read/scroll (paritas `_READ_MAX_CONTENT`
/// aslinya: satu pesan arsip bisa puluhan ribu karakter).
const READ_MAX_CONTENT: usize = 2000;
/// Bentuk read: head + tail saat sesi panjang (aslinya head=20, tail=10).
const READ_HEAD: usize = 20;
const READ_TAIL: usize = 10;

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Satu pesan sebagai satu baris `[indeks] peran: teks` (teks dibatasi
/// `READ_MAX_CONTENT`).
fn shape_message(idx: usize, s: &Session, anchor: bool) -> String {
    let m = &s.messages[idx];
    let mut text = m.text();
    if text.chars().count() > READ_MAX_CONTENT {
        let total = text.chars().count();
        let cut: String = text.chars().take(READ_MAX_CONTENT).collect();
        text = format!("{cut}… [terpotong dari {total} karakter]");
    }
    // Satu baris per pesan agar mudah di-scroll; newline di dalam teks
    // dipertahankan apa adanya.
    let mark = if anchor { " → anchor" } else { "" };
    format!("[{idx}] {}{mark}: {text}", role_name(&m.role))
}

fn session_preview(s: &Session, max_chars: usize) -> String {
    s.messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join(" | ")
        .chars()
        .take(max_chars)
        .collect()
}

pub struct SessionSearchTool;

#[async_trait]
impl Tool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }
    fn description(&self) -> &str {
        "Mengingat percakapan lama dari database sesi. Empat bentuk (dipilih dari argumen): \
         `query` = discovery (sesi-sesi yang cocok, full-text search); \
         `session_id` + `around` = scroll (jendela pesan di sekitar indeks pesan); \
         `session_id` saja = read (baca satu sesi utuh); \
         tanpa argumen = browse (daftar sesi terbaru). \
         Penyimpangan dari aslinya: jangkar scroll adalah indeks pesan 0-based \
         (pesan port tidak punya id integer seperti aslinya)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("query", string_prop("Discovery: kueri pencarian full-text lintas sesi. Diabaikan bila session_id diset.")),
                ("session_id", string_prop("Read/scroll: id sesi yang dibaca. Dari hasil discovery atau browse.")),
                ("around", integer_prop("Scroll: indeks pesan (0-based) sebagai jangkar jendela. Wajib bersama session_id untuk scroll.")),
                ("window", integer_prop("Scroll: jumlah pesan di tiap sisi jangkar (default 5, maks 20).")),
                ("limit", integer_prop("Discovery/browse: jumlah maks hasil (default 5).")),
                ("profile", string_prop("Browse: batasi ke satu profil. Tanpa ini, browse lintas semua profil.")),
            ],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let session_id = opt_str(&args, "session_id");
        let around = opt_u64(&args, "around").map(|v| v as usize);
        let window = opt_u64(&args, "window").unwrap_or(5).clamp(1, 20) as usize;
        let limit = opt_u64(&args, "limit").unwrap_or(5).clamp(1, 50) as usize;

        let body = if let Some(sid) = session_id {
            // Scroll menang bila jangkar diset (seperti aslinya).
            match around {
                Some(a) => self.scroll(ctx, sid, a, window).await?,
                None => self.read(ctx, sid).await?,
            }
        } else if let Some(q) = opt_str(&args, "query").filter(|q| !q.trim().is_empty()) {
            self.discover(ctx, q, limit).await?
        } else {
            self.browse(ctx, opt_str(&args, "profile"), limit).await?
        };
        Ok(ok("", self.name(), body))
    }
}

impl SessionSearchTool {
    /// Bentuk discovery: FTS5 lintas sesi (perilaku lama tool ini).
    async fn discover(&self, ctx: &ToolContext, query: &str, limit: usize) -> Result<String> {
        let sessions = ctx.memory.search_sessions(query, limit).await?;
        if sessions.is_empty() {
            return Ok("tidak ada sesi yang cocok.".to_string());
        }
        let body = sessions
            .iter()
            .map(|s| format!("[{}] {} — {}", s.id, s.title, session_preview(s, 300)))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            "mode=discover, {} sesi cocok. Untuk membaca: session_search(session_id=..., around=<indeks>).\n{body}",
            sessions.len()
        ))
    }

    /// Bentuk read: satu sesi utuh (head+tail bila panjang), tiap pesan
    /// dibatasi `READ_MAX_CONTENT`.
    async fn read(&self, ctx: &ToolContext, session_id: &str) -> Result<String> {
        let s = ctx.memory.load_session(session_id).await?.ok_or_else(|| {
            HermesError::tool(
                "session_search",
                format!("session_id tidak ditemukan: {session_id}"),
            )
        })?;
        let total = s.messages.len();
        let head = format_header("read", &s, total);
        if total <= READ_HEAD + READ_TAIL {
            let msgs: Vec<String> = (0..total).map(|i| shape_message(i, &s, false)).collect();
            return Ok(format!("{head}\n{}", msgs.join("\n")));
        }
        let mut msgs: Vec<String> = (0..READ_HEAD)
            .map(|i| shape_message(i, &s, false))
            .collect();
        msgs.push(format!(
            "… [terpotong: sesi punya {total} pesan; menampilkan {READ_HEAD} pertama + {READ_TAIL} terakhir. \
             Untuk tengahnya: session_search(session_id=\"{sid}\", around=<indeks>)]",
            sid = s.id
        ));
        msgs.extend((total - READ_TAIL..total).map(|i| shape_message(i, &s, false)));
        Ok(format!("{head}\n{}", msgs.join("\n")))
    }

    /// Bentuk scroll: jendela ±window di sekitar indeks jangkar.
    async fn scroll(
        &self,
        ctx: &ToolContext,
        session_id: &str,
        around: usize,
        window: usize,
    ) -> Result<String> {
        let s = ctx.memory.load_session(session_id).await?.ok_or_else(|| {
            HermesError::tool(
                "session_search",
                format!("session_id tidak ditemukan: {session_id}"),
            )
        })?;
        let total = s.messages.len();
        if around >= total {
            return Err(HermesError::tool(
                "session_search",
                format!(
                    "around={around} di luar jangkauan: sesi punya {total} pesan (indeks 0–{})",
                    total.saturating_sub(1)
                ),
            ));
        }
        let lo = around.saturating_sub(window);
        let hi = (around + window + 1).min(total);
        let msgs: Vec<String> = (lo..hi)
            .map(|i| shape_message(i, &s, i == around))
            .collect();
        Ok(format!(
            "{}\nindeks {}–{} dari {total} pesan (jangkar: {around}). \
             Maju: around=<indeks pesan terakhir>; mundur: around=<indeks pesan pertama>.\n{}",
            format_header("scroll", &s, total),
            lo,
            hi.saturating_sub(1),
            msgs.join("\n")
        ))
    }

    /// Bentuk browse: metadata sesi terbaru (tanpa isi pesan).
    async fn browse(
        &self,
        ctx: &ToolContext,
        profile: Option<&str>,
        limit: usize,
    ) -> Result<String> {
        // Penyimpangan dari aslinya (yang membatasi ke profil aktif):
        // ToolContext port tidak membawa nama profil, dan search_sessions
        // port memang lintas profil — jadi browse tanpa `profile` melintasi
        // semua profil (hanya metadata, bukan isi pesan).
        let sessions = match profile {
            Some(p) => ctx.memory.list_sessions(p, limit).await?,
            None => ctx.memory.list_recent_sessions(limit).await?,
        };
        if sessions.is_empty() {
            return Ok("belum ada sesi tersimpan.".to_string());
        }
        let body = sessions
            .iter()
            .map(|s| {
                format!(
                    "[{}] {} (profil {}, {} pesan, {}) — {}",
                    s.id,
                    if s.title.is_empty() {
                        "(tanpa judul)"
                    } else {
                        &s.title
                    },
                    s.profile,
                    s.messages.len(),
                    s.updated_at.format("%Y-%m-%d %H:%M"),
                    session_preview(s, 200)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(format!(
            "mode=browse, {} sesi terbaru. Untuk membaca: session_search(session_id=...).\n{body}",
            sessions.len()
        ))
    }
}

fn format_header(mode: &str, s: &Session, total: usize) -> String {
    format!(
        "mode={mode}, session_id={}, judul=\"{}\", profil={}, {} pesan, diperbarui {}",
        s.id,
        s.title,
        s.profile,
        total,
        s.updated_at.format("%Y-%m-%d %H:%M")
    )
}
