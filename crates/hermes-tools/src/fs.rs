//! Tool filesystem: read_file, write_file, edit_file, list_dir, glob, grep.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use regex::Regex;

// ---------- read_file ----------

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Membaca isi file teks. Mendukung offset/limit baris untuk file besar \
         (limit maks 2000; hasil juga dibatasi budget ~100K karakter). \
         Hasil menyertakan baris trailer `next_offset: N` (offset halaman \
         berikutnya; tidak ada bila sudah EOF) dan `content_hash: <token>` \
         (token hash isi file, bisa dipakai sebagai `expected_hash` write_file \
         agar tidak menimpa file yang sudah berubah)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "path",
                    string_prop("Path file (relatif terhadap working dir)."),
                ),
                (
                    "offset",
                    integer_prop(
                        "Baris mulai (1-based, default 1). Untuk halaman berikutnya, \
                         pakai nilai `next_offset` dari hasil sebelumnya.",
                    ),
                ),
                (
                    "limit",
                    integer_prop("Jumlah baris maks (default 2000, maks 2000)."),
                ),
            ],
            vec!["path"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = ctx.resolve_path(get_str(&args, "path")?)?;
        let offset = opt_i64(&args, "offset").unwrap_or(1).max(1) as usize;
        // Batch 8: maks 2000 seperti aslinya (normalize_read_pagination).
        let limit = opt_i64(&args, "limit").unwrap_or(2000).clamp(1, 2000) as usize;
        let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
            HermesError::tool(
                "read_file",
                format!("gagal membaca {}: {e}", path.display()),
            )
        })?;
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let end = (offset - 1 + limit).min(total);
        let slice = if offset - 1 < total {
            &lines[offset - 1..end]
        } else {
            &[]
        };
        let numbered: Vec<String> = slice
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:6}  {l}", offset + i))
            .collect();
        // Offset halaman berikutnya: offset + jumlah baris yang dikembalikan.
        // Absen (tidak ada baris trailer) bila sudah EOF.
        let next_offset = if end < total {
            Some(offset + slice.len())
        } else {
            None
        };
        let mut body = format!(
            "--- {} (baris {}-{} dari {}) ---\n{}",
            path.display(),
            offset,
            offset + slice.len().saturating_sub(1),
            total,
            // Batch 8: budget ~100K karakter seperti aslinya
            // (_DEFAULT_MAX_READ_CHARS), dengan trailer next_offset.
            truncate(&numbered.join("\n"), 100_000)
        );
        if let Some(n) = next_offset {
            body.push_str(&format!("\nnext_offset: {n}"));
        }
        body.push_str(&format!(
            "\ncontent_hash: {}",
            super::patch::content_hash(&content)
        ));
        Ok(ok("", self.name(), body))
    }
}

// ---------- write_file ----------

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Menulis (menimpa) file dengan konten baru. Membuat direktori induk bila perlu. \
         Guard anti-timpa-basi: bila `expected_hash` diisi (token `content_hash` dari \
         read_file sebelumnya), tulis ditolak bila isi file kini berbeda, kecuali \
         `force: true`."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("path", string_prop("Path file tujuan.")),
                ("content", string_prop("Isi file lengkap.")),
                (
                    "expected_hash",
                    string_prop(
                        "Opsional: token `content_hash` dari read_file sebelumnya. Bila \
                         file sudah berubah sejak dibaca dan `force` tidak true, tulis \
                         ditolak dengan error.",
                    ),
                ),
                (
                    "force",
                    boolean_prop(
                        "Bila true, tulis tetap dilakukan walau `expected_hash` tidak \
                         cocok (default false).",
                    ),
                ),
            ],
            vec!["path", "content"],
        )
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = ctx.resolve_path(get_str(&args, "path")?)?;
        let content = get_str(&args, "content")?;
        let force = opt_bool(&args, "force").unwrap_or(false);
        // Guard anti-timpa-basi (audit W2-B14): bila pemanggil menyertakan
        // expected_hash dari read_file sebelumnya dan file sudah berubah,
        // tolak kecuali force: true. Tanpa expected_hash, perilaku tak berubah.
        if let Some(expected) = opt_str(&args, "expected_hash") {
            if path.is_file() && !force {
                let current = tokio::fs::read_to_string(&path).await.map_err(|e| {
                    HermesError::tool(
                        "write_file",
                        format!("gagal membaca ulang {}: {e}", path.display()),
                    )
                })?;
                let current_hash = super::patch::content_hash(&current);
                if super::patch::normalize_hash_token(&current_hash)
                    != super::patch::normalize_hash_token(expected)
                {
                    return Err(HermesError::tool(
                        "write_file",
                        format!(
                            "menolak menimpa {}: file berubah sejak dibaca (hash tidak cocok). \
                             Baca ulang file untuk mendapatkan content_hash terbaru, atau \
                             pakai force:true untuk tetap menimpa.",
                            path.display()
                        ),
                    ));
                }
            }
        }
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| HermesError::tool("write_file", format!("gagal membuat dir: {e}")))?;
        }
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| HermesError::tool("write_file", format!("gagal menulis: {e}")))?;
        Ok(ok(
            "",
            self.name(),
            format!("ditulis {} ({} byte)", path.display(), content.len()),
        ))
    }
}

// ---------- edit_file ----------

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Mengganti old_text dengan new_text di dalam file. old_text harus unik (tepat satu \
         kecocokan): dicari secara persis dulu, lalu dengan fuzzy matching yang toleran \
         terhadap perbedaan whitespace bila tidak ada yang cocok persis. Error bila \
         old_text cocok lebih dari satu kali atau tidak cocok sama sekali — file tidak diubah. \
         Berhasil: mengembalikan unified diff (---/+++ + hunks @@) dari perubahan."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("path", string_prop("Path file.")),
                (
                    "old_text",
                    string_prop(
                        "Teks lama yang dicari dan diganti. Harus unik (tepat satu kecocokan, \
                         persis atau fuzzy whitespace); sertakan baris konteks bila perlu.",
                    ),
                ),
                ("new_text", string_prop("Teks pengganti.")),
            ],
            vec!["path", "old_text", "new_text"],
        )
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = ctx.resolve_path(get_str(&args, "path")?)?;
        let old_text = get_str(&args, "old_text")?;
        let new_text = get_str(&args, "new_text")?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| HermesError::tool("edit_file", format!("gagal membaca: {e}")))?;
        // Semantik penggantian dipakai bersama `patch` (audit W2-B18/B19):
        // tepat satu kecocokan (persis dulu, lalu fuzzy whitespace) diganti;
        // >1 atau 0 kecocokan = error dan file tidak diubah.
        let (new_content, kind) =
            super::patch::apply_replace(&content, old_text, new_text, "old_text", "new_text")
                .map_err(|e| HermesError::tool("edit_file", e))?;
        tokio::fs::write(&path, &new_content)
            .await
            .map_err(|e| HermesError::tool("edit_file", format!("gagal menulis: {e}")))?;
        let mode = match kind {
            super::patch::MatchKind::Exact => "persis",
            super::patch::MatchKind::Fuzzy => "fuzzy (whitespace)",
        };
        let diff = super::patch::unified_diff(
            &content,
            &new_content,
            &path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
        );
        Ok(ok(
            "",
            self.name(),
            format!("diganti di {} (kecocokan {mode})\n{diff}", path.display()),
        ))
    }
}

// ---------- list_dir ----------

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "Menampilkan isi direktori beserta tipe dan ukuran."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![("path", string_prop("Direktori (default: working dir)."))],
            vec![],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let dir = match opt_str(&args, "path") {
            Some(p) => ctx.resolve_path(p)?,
            None => ctx.working_dir.clone(),
        };
        let mut rd = tokio::fs::read_dir(&dir).await.map_err(|e| {
            HermesError::tool("list_dir", format!("gagal membuka {}: {e}", dir.display()))
        })?;
        let mut entries = Vec::new();
        while let Ok(Some(e)) = rd.next_entry().await {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let ft = e
                .file_type()
                .await
                .map_err(|er| HermesError::tool("list_dir", er.to_string()))?;
            let kind = if ft.is_dir() {
                "dir "
            } else if ft.is_symlink() {
                "link"
            } else {
                "file"
            };
            let size = if ft.is_file() {
                e.metadata().await.map(|m| m.len()).unwrap_or(0).to_string()
            } else {
                "-".to_string()
            };
            entries.push(format!("{kind}  {size:>10}  {name}"));
        }
        entries.sort();
        let body = format!(
            "--- {} ({} entri) ---\n{}",
            dir.display(),
            entries.len(),
            entries.join("\n")
        );
        Ok(ok("", self.name(), body))
    }
}

// ---------- glob ----------

pub struct GlobTool;

/// Pencocokan glob minimal: mendukung `*`, `**`, `?`.
fn glob_match(pattern: &str, path: &str) -> bool {
    fn rec(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (None, _) => false,
            (Some('*'), _) => {
                // `**` cocok lintas separator.
                let is_double = p.get(1) == Some(&'*');
                if is_double {
                    // Konsumsi `**` (+ `/` opsional) lalu coba semua posisi.
                    let mut rest = &p[2..];
                    if rest.first() == Some(&'/') {
                        rest = &rest[1..];
                    }
                    for i in 0..=s.len() {
                        if rec(rest, &s[i..]) {
                            return true;
                        }
                    }
                    false
                } else {
                    for i in 0..=s.len() {
                        if s.get(i) == Some(&'/') {
                            break;
                        }
                        if rec(&p[1..], &s[i..]) {
                            return true;
                        }
                    }
                    false
                }
            }
            (Some('?'), Some(&'/')) => false,
            (Some('?'), Some(_)) => rec(&p[1..], &s[1..]),
            (Some(a), Some(b)) => a == b && rec(&p[1..], &s[1..]),
            (Some(_), None) => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    rec(&p, &s)
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Mencari file dengan pola glob (mis. **/*.rs) dari direktori."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("pattern", string_prop("Pola glob, mis. **/*.md")),
                (
                    "path",
                    string_prop("Direktori dasar (default: working dir)."),
                ),
            ],
            vec!["pattern"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = get_str(&args, "pattern")?;
        let base = match opt_str(&args, "path") {
            Some(p) => ctx.resolve_path(p)?,
            None => ctx.working_dir.clone(),
        };
        let mut hits = Vec::new();
        for entry in walkdir::WalkDir::new(&base)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
        {
            let rel = entry
                .path()
                .strip_prefix(&base)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if glob_match(pattern, &rel)
                || glob_match(pattern, entry.file_name().to_string_lossy().as_ref())
            {
                hits.push(rel);
                if hits.len() >= 500 {
                    break;
                }
            }
        }
        hits.sort();
        Ok(ok(
            "",
            self.name(),
            format!(
                "{} cocok:\n{}",
                hits.len(),
                truncate(&hits.join("\n"), 20000)
            ),
        ))
    }
}

// ---------- grep ----------

pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Mencari pola regex di file-file teks, menampilkan file:baris."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("pattern", string_prop("Pola regex.")),
                (
                    "path",
                    string_prop("File atau direktori (default: working dir)."),
                ),
                (
                    "include",
                    string_prop("Filter glob file, mis. *.rs (hanya bila path direktori)."),
                ),
                (
                    "output_mode",
                    serde_json::json!({
                        "type": "string",
                        "enum": ["content", "files_only", "count"],
                        "description": "Format output: 'content' menampilkan baris cocok \
                                        beserta nomor baris; 'files_only' hanya menampilkan \
                                        path file; 'count' menampilkan jumlah cocok per file \
                                        (default: content).",
                        "default": "content"
                    }),
                ),
            ],
            vec!["pattern"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let pattern = get_str(&args, "pattern")?;
        let re = Regex::new(pattern)
            .map_err(|e| HermesError::tool("grep", format!("regex tidak valid: {e}")))?;
        let target = match opt_str(&args, "path") {
            Some(p) => ctx.resolve_path(p)?,
            None => ctx.working_dir.clone(),
        };
        let include = opt_str(&args, "include").unwrap_or("*");
        let output_mode = opt_str(&args, "output_mode").unwrap_or("content");

        let mut files = Vec::new();
        if target.is_file() {
            files.push(target.clone());
        } else {
            for entry in walkdir::WalkDir::new(&target)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                let name = entry.file_name().to_string_lossy();
                if name.starts_with('.') {
                    continue;
                }
                if glob_match(include, &name) || include == "*" {
                    files.push(entry.path().to_path_buf());
                }
                if files.len() >= 2000 {
                    break;
                }
            }
        }

        // Kumpulkan (file_rel, no_baris, isi_baris); render sesuai output_mode
        // seperti aslinya: content / files_only / count.
        let mut hits: Vec<(String, usize, String)> = Vec::new();
        'outer: for f in files {
            let Ok(content) = tokio::fs::read_to_string(&f).await else {
                continue;
            };
            // Lewati file biner.
            if content.as_bytes().iter().take(8000).any(|&b| b == 0) {
                continue;
            }
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    let rel = f
                        .strip_prefix(&ctx.working_dir)
                        .unwrap_or(&f)
                        .display()
                        .to_string();
                    hits.push((rel, i + 1, line.trim().to_string()));
                    if hits.len() >= 200 {
                        break 'outer;
                    }
                }
            }
        }
        let body = match output_mode {
            "files_only" => {
                let mut seen = std::collections::HashSet::new();
                let mut files_only = Vec::new();
                for (rel, _, _) in &hits {
                    if seen.insert(rel.clone()) {
                        files_only.push(rel.clone());
                    }
                }
                format!(
                    "{} file cocok:\n{}",
                    files_only.len(),
                    truncate(&files_only.join("\n"), 20000)
                )
            }
            "count" => {
                let mut counts: Vec<(String, usize)> = Vec::new();
                for (rel, _, _) in &hits {
                    match counts.iter_mut().find(|(p, _)| p == rel) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((rel.clone(), 1)),
                    }
                }
                let lines: Vec<String> = counts.iter().map(|(p, n)| format!("{p}: {n}")).collect();
                let total: usize = counts.iter().map(|(_, n)| n).sum();
                format!(
                    "{total} hasil di {} file:\n{}",
                    counts.len(),
                    truncate(&lines.join("\n"), 20000)
                )
            }
            _ => {
                let lines: Vec<String> = hits
                    .iter()
                    .map(|(rel, no, text)| format!("{rel}:{no}: {text}"))
                    .collect();
                format!(
                    "{} hasil:\n{}",
                    hits.len(),
                    truncate(&lines.join("\n"), 20000)
                )
            }
        };
        Ok(ok("", self.name(), body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_ctx;

    #[test]
    fn glob_basic() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("**/*.rs", "a/b/main.rs"));
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("?.rs", "a.rs"));
        assert!(!glob_match("?.rs", "ab.rs"));
    }

    /// Direktori kerja sementara per-test (aman untuk test paralel).
    fn edit_test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes_edit_file_test_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("gagal membuat dir uji");
        dir
    }

    fn edit_args(path: &str, old_text: &str, new_text: &str) -> serde_json::Value {
        serde_json::json!({ "path": path, "old_text": old_text, "new_text": new_text })
    }

    #[tokio::test]
    async fn edit_file_unique_occurrence_replaced() {
        let dir = edit_test_dir("unique");
        let file = dir.join("a.txt");
        std::fs::write(&file, "line1\nhello world\nline3\n").unwrap();
        let ctx = test_ctx(dir);
        let tool = EditFileTool;
        let res = tool
            .execute(edit_args("a.txt", "hello world", "halo dunia"), &ctx)
            .await
            .expect("edit_file gagal");
        assert!(!res.is_error);
        assert!(
            res.content.contains("persis"),
            "konten hasil: {}",
            res.content
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "line1\nhalo dunia\nline3\n"
        );
    }

    #[tokio::test]
    async fn edit_file_ambiguous_errors_with_count_and_keeps_file() {
        let dir = edit_test_dir("ambiguous");
        let file = dir.join("a.txt");
        let original = "foo\nbar\nfoo\nbaz\nfoo\n";
        std::fs::write(&file, original).unwrap();
        let ctx = test_ctx(dir);
        let err = EditFileTool
            .execute(edit_args("a.txt", "foo", "qux"), &ctx)
            .await
            .expect_err("harusnya error ambigu");
        let msg = format!("{err}");
        assert!(msg.contains('3'), "pesan harus menyebut jumlah: {msg}");
        assert!(
            msg.contains("ambigu") || msg.contains("ambiguous"),
            "pesan: {msg}"
        );
        // File tidak boleh berubah.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[tokio::test]
    async fn edit_file_fuzzy_whitespace_succeeds() {
        let dir = edit_test_dir("fuzzy");
        let file = dir.join("a.txt");
        std::fs::write(&file, "fn main() {\n    let x =  1;\n}\n").unwrap();
        let ctx = test_ctx(dir);
        // old_text memakai spasi tunggal; file memakai dua spasi + indentasi.
        let res = EditFileTool
            .execute(edit_args("a.txt", "let x = 1;", "let x = 2;"), &ctx)
            .await
            .expect("edit_file fuzzy gagal");
        assert!(!res.is_error);
        assert!(
            res.content.contains("fuzzy"),
            "konten hasil: {}",
            res.content
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "fn main() {\n    let x = 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn edit_file_zero_match_errors_and_keeps_file() {
        let dir = edit_test_dir("zeromatch");
        let file = dir.join("a.txt");
        let original = "line1\nline2\n";
        std::fs::write(&file, original).unwrap();
        let ctx = test_ctx(dir);
        // Tidak cocok persis maupun fuzzy.
        let err = EditFileTool
            .execute(edit_args("a.txt", "tidak ada di sini", "x"), &ctx)
            .await
            .expect_err("harusnya error no match");
        let msg = format!("{err}");
        assert!(msg.contains("no match found"), "pesan: {msg}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    /// Ambil baris trailer `next_offset: N` / `content_hash: T` dari body read_file.
    fn trailer(body: &str, key: &str) -> Option<String> {
        let prefix = format!("{key}: ");
        body.lines()
            .find_map(|l| l.strip_prefix(&prefix).map(|v| v.trim().to_string()))
    }

    /// Kumpulkan baris konten (tanpa nomor baris) dari body read_file.
    fn page_lines(body: &str) -> Vec<String> {
        body.lines()
            .skip(1) // baris header `--- ... ---`
            .take_while(|l| !l.starts_with("next_offset:") && !l.starts_with("content_hash:"))
            .map(|l| l.get(8..).unwrap_or("").to_string()) // prefix "{:6}  "
            .collect()
    }

    #[tokio::test]
    async fn read_file_pagination_walk_to_eof() {
        let dir = edit_test_dir("pagination");
        let file = dir.join("long.txt");
        let expected: Vec<String> = (1..=250).map(|i| format!("baris ke-{i}")).collect();
        std::fs::write(&file, expected.join("\n")).unwrap();
        let ctx = test_ctx(dir);
        let tool = ReadFileTool;

        let mut collected: Vec<String> = Vec::new();
        let mut offset = 1usize;
        let mut pages = 0;
        loop {
            let res = tool
                .execute(
                    serde_json::json!({ "path": "long.txt", "offset": offset, "limit": 100 }),
                    &ctx,
                )
                .await
                .expect("read_file gagal");
            assert!(!res.is_error);
            collected.extend(page_lines(&res.content));
            pages += 1;
            match trailer(&res.content, "next_offset") {
                Some(n) => offset = n.parse::<usize>().expect("next_offset bukan angka"),
                None => break, // EOF: trailer absen
            }
            assert!(pages < 10, "pagination tidak mencapai EOF");
        }
        assert_eq!(pages, 3, "250 baris / limit 100 harus 3 halaman");
        assert_eq!(collected, expected);
        // Halaman terakhir: tidak ada trailer next_offset (EOF).
        let last = tool
            .execute(
                serde_json::json!({ "path": "long.txt", "offset": 201, "limit": 100 }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(trailer(&last.content, "next_offset").is_none());
        // content_hash selalu ada dan stabil untuk isi yang sama.
        let h1 = trailer(&last.content, "content_hash").expect("content_hash hilang");
        assert!(h1.starts_with("sha256:"));
        let again = tool
            .execute(serde_json::json!({ "path": "long.txt" }), &ctx)
            .await
            .unwrap();
        assert_eq!(trailer(&again.content, "content_hash").unwrap(), h1);
    }

    #[tokio::test]
    async fn write_file_stale_hash_refused_then_forced() {
        let dir = edit_test_dir("stale");
        let file = dir.join("doc.txt");
        let ctx = test_ctx(dir);
        let write = WriteFileTool;
        let read = ReadFileTool;

        write
            .execute(
                serde_json::json!({ "path": "doc.txt", "content": "versi satu" }),
                &ctx,
            )
            .await
            .expect("tulis awal gagal");
        // Baca untuk mendapatkan token content_hash.
        let res = read
            .execute(serde_json::json!({ "path": "doc.txt" }), &ctx)
            .await
            .expect("baca gagal");
        let token = trailer(&res.content, "content_hash").expect("content_hash hilang");

        // File diubah pihak lain di luar alur baca->tulis.
        std::fs::write(&file, "versi dua (eksternal)").unwrap();

        // Tulis dengan token basi -> ditolak.
        let err = write
            .execute(
                serde_json::json!({
                    "path": "doc.txt",
                    "content": "versi tiga",
                    "expected_hash": token,
                }),
                &ctx,
            )
            .await
            .expect_err("tulis basi harusnya ditolak");
        let msg = format!("{err}");
        assert!(
            msg.contains("berubah") || msg.contains("tidak cocok"),
            "pesan: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "versi dua (eksternal)"
        );

        // Dengan force:true -> diizinkan.
        let ok_res = write
            .execute(
                serde_json::json!({
                    "path": "doc.txt",
                    "content": "versi tiga",
                    "expected_hash": token,
                    "force": true,
                }),
                &ctx,
            )
            .await
            .expect("tulis force gagal");
        assert!(!ok_res.is_error);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "versi tiga");

        // Tanpa expected_hash: perilaku lama tak berubah (langsung timpa).
        write
            .execute(
                serde_json::json!({ "path": "doc.txt", "content": "versi empat" }),
                &ctx,
            )
            .await
            .expect("tulis tanpa token gagal");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "versi empat");

        // Token masih cocok (file belum berubah): tulis dengan expected_hash lolos.
        let res2 = read
            .execute(serde_json::json!({ "path": "doc.txt" }), &ctx)
            .await
            .unwrap();
        let token2 = trailer(&res2.content, "content_hash").unwrap();
        write
            .execute(
                serde_json::json!({
                    "path": "doc.txt",
                    "content": "versi lima",
                    "expected_hash": token2,
                }),
                &ctx,
            )
            .await
            .expect("tulis dengan token cocok gagal");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "versi lima");
    }

    #[tokio::test]
    async fn edit_file_returns_unified_diff() {
        let dir = edit_test_dir("diff");
        let file = dir.join("a.txt");
        std::fs::write(&file, "baris1\nhello world\nbaris3\n").unwrap();
        let ctx = test_ctx(dir);
        let res = EditFileTool
            .execute(edit_args("a.txt", "hello world", "halo dunia"), &ctx)
            .await
            .expect("edit_file gagal");
        assert!(!res.is_error);
        // Info sukses lama tetap ada.
        assert!(
            res.content.contains("diganti di"),
            "konten: {}",
            res.content
        );
        // Marker unified diff.
        assert!(res.content.contains("---"), "konten: {}", res.content);
        assert!(res.content.contains("+++"), "konten: {}", res.content);
        assert!(res.content.contains("@@"), "konten: {}", res.content);
        // Baris yang berubah muncul sebagai - / +.
        assert!(
            res.content.contains("-hello world"),
            "konten: {}",
            res.content
        );
        assert!(
            res.content.contains("+halo dunia"),
            "konten: {}",
            res.content
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "baris1\nhalo dunia\nbaris3\n"
        );
    }

    // --- Batch 8: paritas minor read/grep ---

    #[tokio::test]
    async fn read_file_limit_dijepit_maks_2000() {
        // Aslinya: normalize_read_pagination menjepit limit ke maks 2000.
        let dir = edit_test_dir("limitclamp");
        let file = dir.join("big.txt");
        let lines: Vec<String> = (1..=2100).map(|i| format!("baris-{i}")).collect();
        std::fs::write(&file, lines.join("\n")).unwrap();
        let ctx = test_ctx(dir);
        let res = ReadFileTool
            .execute(
                serde_json::json!({ "path": "big.txt", "limit": 99999 }),
                &ctx,
            )
            .await
            .expect("read_file gagal");
        let body = res.content;
        // Header menyebut baris 1-2000 dari 2100.
        assert!(
            body.contains("(baris 1-2000 dari 2100)"),
            "header salah: {}",
            body.lines().next().unwrap_or("")
        );
        assert!(trailer(&body, "next_offset").is_some());
    }

    #[tokio::test]
    async fn grep_output_mode_files_only_dan_count() {
        let dir = edit_test_dir("grepmode");
        std::fs::write(dir.join("a.rs"), "fn main() {}\n// cari aku\n").unwrap();
        std::fs::write(dir.join("b.rs"), "// cari aku\n// cari aku lagi\n").unwrap();
        std::fs::write(dir.join("c.rs"), "tidak relevan\n").unwrap();
        let ctx = test_ctx(dir);
        let tool = GrepTool;

        // content (default): baris file:nomor: isi.
        let res = tool
            .execute(serde_json::json!({ "pattern": "cari aku" }), &ctx)
            .await
            .expect("grep gagal");
        assert!(
            res.content.starts_with("3 hasil:"),
            "konten: {}",
            res.content
        );
        assert!(res.content.contains("a.rs:2:"), "konten: {}", res.content);

        // files_only: hanya path unik.
        let res = tool
            .execute(
                serde_json::json!({ "pattern": "cari aku", "output_mode": "files_only" }),
                &ctx,
            )
            .await
            .expect("grep gagal");
        assert!(
            res.content.starts_with("2 file cocok:"),
            "konten: {}",
            res.content
        );
        assert!(res.content.contains("a.rs"), "konten: {}", res.content);
        assert!(!res.content.contains("cari aku"), "konten: {}", res.content);

        // count: jumlah cocok per file.
        let res = tool
            .execute(
                serde_json::json!({ "pattern": "cari aku", "output_mode": "count" }),
                &ctx,
            )
            .await
            .expect("grep gagal");
        assert!(
            res.content.starts_with("3 hasil di 2 file:"),
            "konten: {}",
            res.content
        );
        assert!(res.content.contains("b.rs: 2"), "konten: {}", res.content);
    }

    #[test]
    fn grep_schema_menyatakan_output_mode() {
        let s = GrepTool.parameters_schema().to_string();
        assert!(s.contains("output_mode"), "skema: {s}");
        for m in ["content", "files_only", "count"] {
            assert!(s.contains(m), "skema: {s}");
        }
    }

    #[test]
    fn read_schema_menyatakan_limit_maks_2000() {
        let s = ReadFileTool.parameters_schema().to_string();
        assert!(s.contains("maks 2000"), "skema: {s}");
    }
}
