//! Handler mcp.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::McpCmd;
use crate::app::App;

pub(crate) async fn cmd_mcp(app: &App, cmd: McpCmd) -> anyhow::Result<()> {
    match cmd {
        McpCmd::List => {
            let names = app.tools.names();
            let mcp_tools: Vec<_> = names.iter().filter(|n| n.starts_with("mcp__")).collect();
            if mcp_tools.is_empty() {
                println!("Tidak ada tool MCP. Tambahkan server di config.toml [mcp_servers].");
            }
            for n in mcp_tools {
                println!("- {n}");
            }
        }
    }
    Ok(())
}
