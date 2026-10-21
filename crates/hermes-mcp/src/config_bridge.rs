//! Jembatan config → koneksi MCP client.
use crate::McpClient;
use hermes_core::{McpServerConfig, Result};

pub struct McpServerConfigBridge;

impl McpServerConfigBridge {
    pub async fn connect(name: &str, cfg: &McpServerConfig) -> Result<McpClient> {
        match cfg {
            McpServerConfig::Stdio { command, args, env } => {
                McpClient::connect_stdio(name, command, args, env).await
            }
            McpServerConfig::Http { url, headers } => {
                McpClient::connect_http(name, url, headers).await
            }
        }
    }
}
