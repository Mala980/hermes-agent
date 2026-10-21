//! Handler memory.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::MemoryCmd;
use super::session::all_profile_names;
use crate::app::App;

pub(crate) async fn cmd_memory(app: &App, cmd: MemoryCmd) -> anyhow::Result<()> {
    // Parse target CLI ('memory' default, seperti tool-nya).
    fn parse_cli_target(raw: &str) -> anyhow::Result<hermes_core::MemoryTarget> {
        raw.parse().map_err(|e: String| anyhow::anyhow!("{e}"))
    }
    match cmd {
        MemoryCmd::List { limit, target } => {
            let t = parse_cli_target(&target)?;
            for e in app.memory.list_memories_for(limit, t).await? {
                println!("[{}|{}|{}] {}", e.id, e.kind, e.target, e.text);
            }
        }
        MemoryCmd::Recall {
            query,
            limit,
            target,
        } => {
            let t = parse_cli_target(&target)?;
            for e in app.memory.recall_for(&query, limit, t).await? {
                println!("[{}|{}|{}] {}", e.id, e.kind, e.target, e.text);
            }
        }
        MemoryCmd::Forget {
            id_or_query,
            target,
        } => {
            let t = parse_cli_target(&target)?;
            let n = app.memory.forget_for(&id_or_query, t).await?;
            println!("Dihapus {n} memori (target {t}).");
        }
        // B8 (tier 2): paritas `memory status` asli — ringkasan backend.
        MemoryCmd::Status => {
            let db_path = app.hermes_home.join("memory.db");
            let db_info = if db_path.is_file() {
                match std::fs::metadata(&db_path) {
                    Ok(m) => format!("ada ({} byte)", m.len()),
                    Err(e) => format!("error baca metadata: {e}"),
                }
            } else {
                "belum ada (dibuat saat pertama dipakai)".to_string()
            };
            let entries = app.memory.list_memories(1_000_000).await?;
            let mut sessions = 0usize;
            for profile in all_profile_names(app) {
                sessions += app.memory.list_sessions(&profile, 1_000_000).await?.len();
            }
            println!("Backend memori : SQLite");
            println!("Database       : {} — {db_info}", db_path.display());
            println!("Entri memori   : {}", entries.len());
            println!("Sesi tersimpan : {sessions}");
            println!("Profil aktif   : {}", app.profile_name);
        }
    }
    Ok(())
}
