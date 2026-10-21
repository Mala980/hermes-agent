//! Handler providers.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::ProvidersCmd;
use crate::app::App;

// ---------------------------------------------------------------------------
// B8 (tier 2): `hermes providers list` — daftar provider yang dikonfigurasi.
// ---------------------------------------------------------------------------

/// Daftar provider LLM dari config (tanpa network; nilai statis).
pub(crate) fn cmd_providers(app: &App, cmd: ProvidersCmd) -> anyhow::Result<()> {
    match cmd {
        ProvidersCmd::List => {
            let active = &app.profile.provider;
            if app.config.providers.is_empty() {
                println!("Belum ada provider yang dikonfigurasi.");
                return Ok(());
            }
            println!("Provider yang dikonfigurasi:");
            for p in &app.config.providers {
                let mark = if &p.name == active { "●" } else { " " };
                let model = p.default_model.as_deref().unwrap_or("-");
                println!(
                    "{mark} {:<12} protokol={:<9} model={}",
                    p.name, p.protocol, model
                );
                if !p.base_url.is_empty() {
                    println!("      {}", p.base_url);
                }
            }
            println!("\nProvider aktif profil `{}`: {active}", app.profile_name);
        }
    }
    Ok(())
}
