//! Tool `patch`: edit find-and-replace terarah dengan fuzzy matching.
//!
//! Port dari tool `patch` milik original (mode `replace`, lihat
//! `tools/file_tools.py`): argumen `path`, `old_string`, `new_string`.
//! Semantik:
//!   1. Cari `old_string` secara persis; bila cocok tepat satu kali, ganti.
//!   2. Bila tidak ada yang cocok persis, ulangi dengan pencocokan fuzzy:
//!      semua run whitespace (spasi, tab, baris baru) diperlakukan setara.
//!   3. Gagal dengan jelas bila tidak ada yang cocok ("no match found") atau
//!      bila cocok lebih dari sekali (ambigu; jumlah kecocokan disebutkan).
//!
//! Logika penggantian dipakai bersama `edit_file` lewat [`apply_replace`];
//! keduanya hanya berbeda nama parameter (`old_string`/`new_string` vs
//! `old_text`/`new_text`).
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};

pub struct PatchTool;

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &str {
        "patch"
    }

    fn description(&self) -> &str {
        "Edit find-and-replace terarah di file. Mencari old_string secara persis dulu; \
         bila tidak ketemu, memakai fuzzy matching yang toleran terhadap perbedaan \
         whitespace/indentasi. old_string harus unik (tepat satu kecocokan) — sertakan \
         baris konteks bila perlu."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("path", string_prop("Path file yang diedit.")),
                (
                    "old_string",
                    string_prop(
                        "Teks yang dicari dan diganti. Harus unik di file (tepat satu \
                         kecocokan, persis atau fuzzy); sertakan baris konteks di sekitarnya \
                         bila perlu.",
                    ),
                ),
                (
                    "new_string",
                    string_prop(
                        "Teks pengganti; harus berbeda dari old_string. String kosong \"\" \
                         menghapus teks yang cocok.",
                    ),
                ),
            ],
            vec!["path", "old_string", "new_string"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let rel = get_str(&args, "path")?;
        let old_string = get_str(&args, "old_string")?;
        let new_string = get_str(&args, "new_string")?;
        let path = ctx.resolve_path(rel)?;
        let original = std::fs::read_to_string(&path)
            .map_err(|e| HermesError::tool("patch", format!("gagal membaca: {e}")))?;
        let (new_content, kind) = apply_replace(
            &original,
            old_string,
            new_string,
            "old_string",
            "new_string",
        )
        .map_err(|e| HermesError::tool("patch", e))?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| HermesError::tool("patch", format!("mkdir gagal: {e}")))?;
        }
        std::fs::write(&path, &new_content)
            .map_err(|e| HermesError::tool("patch", format!("tulis gagal: {e}")))?;
        let mode = match kind {
            MatchKind::Exact => "persis",
            MatchKind::Fuzzy => "fuzzy (whitespace)",
        };
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: self.name().to_string(),
            content: format!("patch: diganti di {} (kecocokan {mode})", path.display()),
            is_error: false,
        })
    }
}

/// Token hash konten file: `sha256:<hex>` dari seluruh isi file (UTF-8).
/// Dipakai `read_file` sebagai token baca dan `write_file` sebagai
/// `expected_hash` untuk guard anti-timpa-basi (audit W2-B14).
pub(crate) fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("sha256:{:x}", h.finalize())
}

/// Normalisasi token hash dari pemanggil: terima `sha256:<hex>` maupun hex
/// polos (tetap dibedakan dari awalan lain).
pub(crate) fn normalize_hash_token(token: &str) -> &str {
    token.strip_prefix("sha256:").unwrap_or(token)
}

/// Render unified diff (`---`/`+++` + hunks `@@`) antara isi lama dan baru,
/// dipakai `edit_file` untuk melaporkan perubahan (audit W2-B20).
pub(crate) fn unified_diff(old: &str, new: &str, name: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    let header_old = format!("a/{name}");
    let header_new = format!("b/{name}");
    diff.unified_diff()
        .header(&header_old, &header_new)
        .context_radius(3)
        .to_string()
}

/// Bagaimana `old_string`/`old_text` cocok dengan isi file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchKind {
    Exact,
    Fuzzy,
}

/// Terapkan penggantian `old` -> `new` pada `content` sesuai semantik `patch`,
/// dipakai bersama oleh `edit_file`: cocok persis dulu (harus unik), lalu
/// fuzzy whitespace bila tidak ada yang cocok persis. `old_name`/`new_name`
/// adalah nama parameter pada masing-masing tool (`patch`: "old_string"/
/// "new_string"; `edit_file`: "old_text"/"new_text"), dipakai di pesan error.
/// Mengembalikan konten baru beserta jenis kecocokan.
pub(crate) fn apply_replace(
    content: &str,
    old: &str,
    new: &str,
    old_name: &str,
    new_name: &str,
) -> std::result::Result<(String, MatchKind), String> {
    if old.is_empty() {
        return Err(format!("{old_name} tidak boleh kosong"));
    }
    if old == new {
        return Err(format!(
            "{new_name} harus berbeda dari {old_name} (gunakan \"\" untuk menghapus)"
        ));
    }
    let exact = content.matches(old).count();
    if exact == 1 {
        return Ok((content.replacen(old, new, 1), MatchKind::Exact));
    }
    if exact > 1 {
        return Err(format!(
            "ambiguous: {old_name} cocok {exact} kali secara persis; sertakan baris \
             konteks agar unik"
        ));
    }
    // Tidak ada yang cocok persis: coba fuzzy — token-token old_string boleh
    // dipisahkan run whitespace apa pun (spasi, tab, baris baru).
    let tokens: Vec<&str> = old.split_whitespace().collect();
    if tokens.is_empty() {
        return Err(format!(
            "no match found: {old_name} tidak ditemukan di file, baik secara persis \
             maupun dengan fuzzy matching"
        ));
    }
    let pattern = tokens
        .iter()
        .map(|t| regex::escape(t))
        .collect::<Vec<_>>()
        .join(r"\s+");
    let re = regex::Regex::new(&pattern).map_err(|e| format!("pola fuzzy tidak valid: {e}"))?;
    let spans: Vec<_> = re.find_iter(content).collect();
    match spans.len() {
        0 => Err(format!(
            "no match found: {old_name} tidak ditemukan di file, baik secara persis \
             maupun dengan fuzzy matching"
        )),
        1 => {
            let m = spans[0];
            let mut out = String::with_capacity(content.len() + new.len());
            out.push_str(&content[..m.start()]);
            out.push_str(new);
            out.push_str(&content[m.end()..]);
            Ok((out, MatchKind::Fuzzy))
        }
        n => Err(format!(
            "ambiguous: {old_name} cocok {n} kali dengan fuzzy matching; sertakan baris \
             konteks agar unik"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_replacement() {
        let content = "line1\nhello world\nline3\n";
        let (out, kind) = apply_replace(
            content,
            "hello world",
            "halo dunia",
            "old_string",
            "new_string",
        )
        .unwrap();
        assert_eq!(kind, MatchKind::Exact);
        assert_eq!(out, "line1\nhalo dunia\nline3\n");
    }

    #[test]
    fn fuzzy_whitespace_tolerant() {
        // Indentasi dan spasi berbeda tetap cocok secara fuzzy.
        let content = "fn main() {\n    let x =  1;\n}\n";
        let (out, kind) = apply_replace(
            content,
            "let x = 1;",
            "let x = 2;",
            "old_string",
            "new_string",
        )
        .unwrap();
        assert_eq!(kind, MatchKind::Fuzzy);
        assert_eq!(out, "fn main() {\n    let x = 2;\n}\n");
    }

    #[test]
    fn fuzzy_across_lines() {
        // Blok multiline: newline dan indentasi diabaikan dalam pencocokan.
        let content = "if cond {\n        do_a();\n    do_b();\n}\n";
        let (out, kind) = apply_replace(
            content,
            "do_a(); do_b();",
            "do_c();",
            "old_string",
            "new_string",
        )
        .unwrap();
        assert_eq!(kind, MatchKind::Fuzzy);
        assert_eq!(out, "if cond {\n        do_c();\n}\n");
    }

    #[test]
    fn zero_match_errors() {
        let content = "line1\nline2\n";
        let err = apply_replace(content, "tidak ada", "x", "old_string", "new_string").unwrap_err();
        assert!(err.contains("no match found"), "pesan: {err}");
    }

    #[test]
    fn zero_match_all_whitespace_old_string() {
        let content = "line1\n";
        let err = apply_replace(content, "   ", "x", "old_string", "new_string").unwrap_err();
        assert!(err.contains("no match found"), "pesan: {err}");
    }

    #[test]
    fn ambiguous_exact_errors_with_count() {
        let content = "foo\nbar\nfoo\n";
        let err = apply_replace(content, "foo", "x", "old_string", "new_string").unwrap_err();
        assert!(err.contains("2"), "pesan: {err}");
        assert!(
            err.contains("ambigu") || err.contains("ambiguous"),
            "pesan: {err}"
        );
    }

    #[test]
    fn ambiguous_fuzzy_errors_with_count() {
        // Dua varian dengan whitespace berbeda, keduanya cocok secara fuzzy,
        // dan tidak ada yang cocok persis.
        let content = "let  x = 1;\nlet   x   =   1;\n";
        let err = apply_replace(
            content,
            "let x = 1;",
            "let x = 2;",
            "old_string",
            "new_string",
        )
        .unwrap_err();
        assert!(err.contains("2"), "pesan: {err}");
        assert!(err.contains("fuzzy"), "pesan: {err}");
    }

    #[test]
    fn empty_new_string_deletes() {
        let content = "abc\ndef\n";
        let (out, _) = apply_replace(content, "def\n", "", "old_string", "new_string").unwrap();
        assert_eq!(out, "abc\n");
    }

    #[test]
    fn empty_old_string_rejected() {
        assert!(apply_replace("abc", "", "x", "old_string", "new_string").is_err());
    }

    #[test]
    fn identical_old_new_rejected() {
        assert!(apply_replace("abc", "abc", "abc", "old_string", "new_string").is_err());
    }
}
