//! Tool skills: skill_list, skill_read, skill_create, skill_improve, skill_delete.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{
    with_retries, ChatRequest, HermesError, Message, Result, Tool, ToolContext, ToolEffect,
    ToolResult,
};

pub struct SkillListTool;

/// Aturan nama skill (batch 8, dari aturan namespace aslinya):
/// harus cocok `[a-zA-Z0-9_-]+` — menolak spasi, slash, titik, dsb.
pub fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
#[async_trait]
impl Tool for SkillListTool {
    fn name(&self) -> &str {
        "skill_list"
    }
    fn description(&self) -> &str {
        "Menampilkan semua skill yang tersedia beserta deskripsinya."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(vec![], vec![])
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, _args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let skills = ctx.skills.list().await?;
        if skills.is_empty() {
            return Ok(ok("", self.name(), "belum ada skill.".to_string()));
        }
        let body = skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ok("", self.name(), body))
    }
}

pub struct SkillReadTool;

#[async_trait]
impl Tool for SkillReadTool {
    fn name(&self) -> &str {
        "skill_read"
    }
    fn description(&self) -> &str {
        "Membaca isi lengkap sebuah skill (prosedur markdown) untuk dipakai."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(vec![("name", string_prop("Nama skill."))], vec!["name"])
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let name = get_str(&args, "name")?;
        match ctx.skills.get(name).await? {
            Some(s) => Ok(ok("", self.name(), format!("# {}\n\n{}", s.name, s.body))),
            None => Err(HermesError::tool(
                "skill_read",
                format!("skill `{name}` tidak ditemukan"),
            )),
        }
    }
}

pub struct SkillCreateTool;

#[async_trait]
impl Tool for SkillCreateTool {
    fn name(&self) -> &str {
        "skill_create"
    }
    fn description(&self) -> &str {
        "Membuat skill baru yang reusable (procedural memory). Dipakai untuk \
         mengabadikan alur kerja/prosedur yang baru dipelajari agar sesi berikutnya lebih cepat."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "name",
                    string_prop(
                        "Nama skill: huruf/angka/underscore/dash saja \
                         ([a-zA-Z0-9_-]+), mis. deploy-fly-io.",
                    ),
                ),
                (
                    "description",
                    string_prop("Satu baris: kapan skill ini dipakai."),
                ),
                (
                    "body",
                    string_prop("Prosedur markdown: langkah, perintah, contoh, jebakan."),
                ),
            ],
            vec!["name", "description", "body"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let name = get_str(&args, "name")?;
        let description = get_str(&args, "description")?;
        let body = get_str(&args, "body")?;
        // Batch 8: aturan nama seperti namespace aslinya ([a-zA-Z0-9_-]+).
        if !is_valid_skill_name(name) {
            return Err(HermesError::tool(
                "skill_create",
                format!(
                    "nama skill tidak valid: '{name}'. Nama harus cocok [a-zA-Z0-9_-]+ \
                     (huruf, angka, underscore, dash saja)."
                ),
            ));
        }
        if ctx.skills.get(name).await?.is_some() {
            return Err(HermesError::tool(
                "skill_create",
                format!("skill `{name}` sudah ada; gunakan skill_improve"),
            ));
        }
        ctx.skills.create(name, description, body).await?;
        Ok(ok("", self.name(), format!("skill `{name}` dibuat")))
    }
}

pub struct SkillImproveTool;

#[async_trait]
impl Tool for SkillImproveTool {
    fn name(&self) -> &str {
        "skill_improve"
    }
    fn description(&self) -> &str {
        "Memperbaiki skill yang ada berdasarkan umpan balik/pengalaman baru (self-improvement)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("name", string_prop("Nama skill.")),
                (
                    "feedback",
                    string_prop("Apa yang dipelajari / perlu diperbaiki."),
                ),
            ],
            vec!["name", "feedback"],
        )
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let name = get_str(&args, "name")?;
        let feedback = get_str(&args, "feedback")?;
        let skill = ctx.skills.get(name).await?.ok_or_else(|| {
            HermesError::tool("skill_improve", format!("skill `{name}` tidak ditemukan"))
        })?;
        let provider = ctx.provider.clone().ok_or_else(|| {
            HermesError::tool("skill_improve", "tidak ada provider LLM untuk improvement")
        })?;
        let prompt = format!(
            "Improve the following skill based on this feedback. Output ONLY the improved full markdown body.\n\n\
             ## Skill: {name}\n{body}\n\n## Feedback:\n{feedback}",
            body = skill.body,
        );
        let req = ChatRequest {
            model: ctx.model.clone(),
            messages: vec![Message::user(prompt)],
            tools: vec![],
            max_tokens: Some(2000),
            temperature: Some(0.3),
            reasoning_effort: None,
        };
        // Retry dengan semantik yang sama seperti turn path agen
        // (retry hanya untuk 429/5xx/gangguan jaringan).
        let (improved, _) = with_retries(|| provider.chat_simple(req.clone())).await?;
        ctx.skills.update(name, improved.trim()).await?;
        Ok(ok("", self.name(), format!("skill `{name}` diperbarui")))
    }
}

pub struct SkillDeleteTool;

#[async_trait]
impl Tool for SkillDeleteTool {
    fn name(&self) -> &str {
        "skill_delete"
    }
    fn description(&self) -> &str {
        "Menghapus sebuah skill."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(vec![("name", string_prop("Nama skill."))], vec!["name"])
    }
    fn risk(&self) -> hermes_core::RiskLevel {
        hermes_core::RiskLevel::Medium
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let name = get_str(&args, "name")?;
        ctx.skills.delete(name).await?;
        Ok(ok("", self.name(), format!("skill `{name}` dihapus")))
    }
}

#[cfg(test)]
mod retry_wiring_tests {
    use super::*;
    use hermes_core::{LlmProvider, StreamEvent, Usage};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::mpsc;

    /// Provider palsu: gagal `fail_times` kali pertama dengan `fail_status`,
    /// lalu berhasil. Menghitung total percobaan `chat_simple`.
    struct FlakyProvider {
        attempts: AtomicU32,
        fail_times: u32,
        fail_status: u16,
    }

    #[async_trait]
    impl LlmProvider for FlakyProvider {
        fn name(&self) -> &str {
            "flaky"
        }
        fn default_model(&self) -> &str {
            "flaky-model"
        }
        async fn chat_stream(&self, _req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
            let (tx, rx) = mpsc::channel(1);
            drop(tx);
            Ok(rx)
        }
        async fn chat_simple(&self, _req: ChatRequest) -> Result<(String, Usage)> {
            let n = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_times {
                Err(HermesError::http(
                    self.fail_status,
                    None,
                    format!("gagal sintetis {n}"),
                ))
            } else {
                Ok(("OK".to_string(), Usage::default()))
            }
        }
    }

    fn simple_req() -> ChatRequest {
        ChatRequest {
            model: "flaky-model".to_string(),
            messages: vec![Message::user("halo")],
            tools: vec![],
            max_tokens: Some(8),
            temperature: None,
            reasoning_effort: None,
        }
    }

    /// Pola pembungkus yang dipakai call site skill_improve & vision_analyze:
    /// `with_retries(|| provider.chat_simple(req.clone()))`.
    async fn wrapped(provider: &FlakyProvider, req: &ChatRequest) -> Result<(String, Usage)> {
        with_retries(|| provider.chat_simple(req.clone())).await
    }

    #[tokio::test]
    async fn retry_terlibat_setelah_satu_gagal_retryable() {
        let provider = FlakyProvider {
            attempts: AtomicU32::new(0),
            fail_times: 1,
            fail_status: 503,
        };
        let req = simple_req();
        let (text, _) = wrapped(&provider, &req)
            .await
            .expect("harus berhasil setelah satu retry");
        assert_eq!(text, "OK");
        assert_eq!(
            provider.attempts.load(Ordering::SeqCst),
            2,
            "harus tepat 2 percobaan"
        );
    }

    #[tokio::test]
    async fn error_non_retryable_tidak_di_retry() {
        let provider = FlakyProvider {
            attempts: AtomicU32::new(0),
            fail_times: 5,
            fail_status: 400,
        };
        let req = simple_req();
        let err = wrapped(&provider, &req)
            .await
            .expect_err("400 tak boleh di-retry");
        assert!(
            err.to_string().contains("400"),
            "error asli harus diteruskan, dapat: {err}"
        );
        assert_eq!(
            provider.attempts.load(Ordering::SeqCst),
            1,
            "error 400 harus langsung dikembalikan tanpa retry"
        );
    }
}

/// Batch 8: aturan nama skill (vs aturan namespace aslinya [a-zA-Z0-9_-]+).
#[cfg(test)]
mod name_rules_tests {
    use super::*;
    use crate::test_support::test_ctx;

    #[test]
    fn nama_valid_diterima() {
        for n in ["deploy-fly-io", "skill_1", "A-Z-9", "x"] {
            assert!(is_valid_skill_name(n), "ditolak padahal valid: {n}");
        }
    }

    #[test]
    fn nama_tidak_valid_ditolak() {
        for n in ["", "ada spasi", "a/b", "a.b", "sk!ll", "../x", "ünï"] {
            assert!(!is_valid_skill_name(n), "diterima padahal tidak valid: {n}");
        }
    }

    #[tokio::test]
    async fn skill_create_menolak_nama_tidak_valid() {
        let tool = SkillCreateTool;
        let ctx = test_ctx(std::path::PathBuf::from("/tmp"));
        let err = tool
            .execute(
                serde_json::json!({
                    "name": "nama buruk!",
                    "description": "d",
                    "body": "b",
                }),
                &ctx,
            )
            .await
            .expect_err("nama tidak valid harus ditolak");
        assert!(
            err.to_string().contains("[a-zA-Z0-9_-]+"),
            "pesan aturan hilang: {err}"
        );
    }
}
