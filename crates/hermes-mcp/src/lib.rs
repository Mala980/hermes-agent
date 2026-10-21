//! hermes-mcp: klien Model Context Protocol.
//!
//! Meniru `mcp_serve.py` / integrasi MCP Hermes asli: Hermes dapat menjadi
//! MCP *client* (memakai tool dari server MCP eksternal) lewat stdio maupun
//! HTTP, dan tool-tool itu dijembatani ke registry tool Hermes.

pub mod bridge;
pub mod client;
pub mod config_bridge;

pub use bridge::register_mcp_tools;
pub use client::{McpClient, McpTool};
pub use config_bridge::McpServerConfigBridge;
