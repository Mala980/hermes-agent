//! Tool utilitas: datetime_now, execute_code.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};

pub struct DatetimeNowTool;

#[async_trait]
impl Tool for DatetimeNowTool {
    fn name(&self) -> &str {
        "datetime_now"
    }
    fn description(&self) -> &str {
        "Mengembalikan tanggal dan waktu saat ini (lokal + UTC)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(vec![], vec![])
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let local = chrono::Local::now();
        let utc = chrono::Utc::now();
        Ok(ok(
            "",
            self.name(),
            format!(
                "lokal: {}\nUTC:   {}",
                local.format("%Y-%m-%d %H:%M:%S %Z"),
                utc.format("%Y-%m-%d %H:%M:%S UTC")
            ),
        ))
    }
}

/// Tool `execute_code`: menjalankan kode python3/bash dari string.
/// Meniru kemampuan Hermes asli menggabungkan pipeline multi-langkah dalam satu tool call.
pub struct ExecuteCodeTool;

#[async_trait]
impl Tool for ExecuteCodeTool {
    fn name(&self) -> &str {
        "execute_code"
    }
    fn description(&self) -> &str {
        "Menjalankan kode program (python3 atau bash) dan mengembalikan outputnya. \
         Berguna menggabungkan pipeline multi-langkah dalam satu pemanggilan."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "language",
                    string_prop("Bahasa: python3 atau bash (default python3)."),
                ),
                ("code", string_prop("Kode yang dijalankan.")),
                (
                    "timeout_secs",
                    integer_prop("Batas waktu detik (default 60)."),
                ),
            ],
            vec!["code"],
        )
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let language = opt_str(&args, "language").unwrap_or("python3");
        let code = get_str(&args, "code")?;
        let timeout = opt_u64(&args, "timeout_secs").unwrap_or(60).clamp(1, 300);

        let dir = std::env::temp_dir().join("hermes-exec");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| HermesError::tool("execute_code", e.to_string()))?;
        let ext = if language == "bash" { "sh" } else { "py" };
        let path = dir.join(format!("code_{}.{}", std::process::id(), ext));
        tokio::fs::write(&path, code)
            .await
            .map_err(|e| HermesError::tool("execute_code", e.to_string()))?;

        let prog = match language {
            "bash" => "bash",
            "python3" | "python" => "python3",
            other => {
                return Err(HermesError::tool(
                    "execute_code",
                    format!("bahasa tak didukung: {other}"),
                ))
            }
        };
        let mut cmd = tokio::process::Command::new(prog);
        cmd.arg(&path).current_dir(&ctx.working_dir).envs(&ctx.env);
        let out = crate::run_command(&mut cmd, timeout, "execute_code").await?;
        let _ = tokio::fs::remove_file(&path).await;

        let mut combined = out.stdout;
        if !out.stderr.trim().is_empty() {
            combined.push_str("\n[stderr]\n");
            combined.push_str(&out.stderr);
        }
        let mut res = ok(
            "",
            self.name(),
            format!("[exit: {}]\n{}", out.code, truncate(&combined, 20000)),
        );
        res.is_error = !out.success;
        Ok(res)
    }
}
