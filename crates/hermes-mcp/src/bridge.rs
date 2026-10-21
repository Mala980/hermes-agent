//! Jembatan: tool MCP → registry tool Hermes.
//!
//! Setiap tool dari server MCP didaftarkan sebagai
//! `mcp__<server>__<tool>` sehingga model bisa memanggilnya seperti tool biasa.
use crate::McpClient;
use async_trait::async_trait;
use hermes_core::{Result, RiskLevel, Tool, ToolContext, ToolResult};
use std::sync::Arc;

struct McpBridgedTool {
    server: String,
    tool_name: String,
    description: String,
    schema: serde_json::Value,
    client: Arc<McpClient>,
}

#[async_trait]
impl Tool for McpBridgedTool {
    fn name(&self) -> &str {
        // Nama stabil dipakai untuk registrasi; disimpan via wrapper di bawah.
        &self.tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn risk(&self) -> RiskLevel {
        // Tool eksternal: selalu minta persetujuan.
        RiskLevel::Medium
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let content = self.client.call_tool(&self.tool_name, args).await?;
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: format!("mcp__{}__{}", self.server, self.tool_name),
            content,
            is_error: false,
        })
    }
}

/// Wrapper agar nama registrasi memakai prefix `mcp__<server>__`.
struct NamedTool {
    registered_name: String,
    inner: McpBridgedTool,
}

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        &self.registered_name
    }
    fn description(&self) -> &str {
        &self.inner.description
    }
    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.schema.clone()
    }
    fn risk(&self) -> RiskLevel {
        RiskLevel::Medium
    }
    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        self.inner.execute(args, ctx).await
    }
}

/// Daftarkan semua tool dari klien-klien MCP ke registry.
pub async fn register_mcp_tools(
    reg: &mut hermes_core::ToolRegistry,
    clients: &[(String, Arc<McpClient>)],
) -> Result<()> {
    for (server, client) in clients {
        let tools = client.list_tools().await?;
        for t in tools {
            let registered_name = format!("mcp__{server}__{}", t.name);
            let desc = t
                .description
                .clone()
                .unwrap_or_else(|| format!("Tool MCP `{}` dari server `{server}`", t.name));
            let schema = if t.input_schema.is_object() {
                t.input_schema.clone()
            } else {
                serde_json::json!({"type": "object", "properties": {}})
            };
            reg.register(NamedTool {
                registered_name,
                inner: McpBridgedTool {
                    server: server.clone(),
                    tool_name: t.name,
                    description: desc,
                    schema,
                    client: client.clone(),
                },
            });
        }
        tracing::info!(server = %server, "tool MCP didaftarkan");
    }
    Ok(())
}
