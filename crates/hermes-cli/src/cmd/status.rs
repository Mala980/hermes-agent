//! Handler status.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::logs::{log_file_name, KNOWN_LOGS};
use crate::app::App;

// ---------------------------------------------------------------------------
// B7 (tier 1): `hermes status` — status ringkas komponen (paritas
// `hermes status --full/--deep` aslinya, satu layar + bagian detail).
// ---------------------------------------------------------------------------

pub(crate) async fn cmd_status(app: &App, full: bool, deep: bool) -> anyhow::Result<()> {
    let full = full || deep;
    let profile = &app.profile;
    println!("hermes {} — status", env!("CARGO_PKG_VERSION"));
    println!("HERMES_HOME : {}", app.hermes_home.display());
    println!(
        "Profil aktif: {} (provider `{}`, model `{}`)",
        app.profile_name, profile.provider, profile.model
    );

    let sessions = app
        .memory
        .list_sessions(&app.profile_name, 1000)
        .await
        .unwrap_or_default();
    println!("Sesi tersimpan (profil ini): {}", sessions.len());

    let jobs = app.cron_store.list().unwrap_or_default();
    let active = jobs.iter().filter(|j| j.enabled).count();
    println!("Cron job    : {} ({} aktif)", jobs.len(), active);

    println!("Tool agen   : {}", app.tools.names().len());
    let skills = app.skills.list().await.unwrap_or_default();
    println!("Skill       : {}", skills.len());

    if full {
        println!("--- detail ---");
        println!(
            "Config      : {}",
            app.hermes_home.join("config.toml").display()
        );
        for name in KNOWN_LOGS {
            let p = app
                .hermes_home
                .join("logs")
                .join(log_file_name(name).unwrap_or("?"));
            if p.is_file() {
                let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                println!("Log {name:<8}: {size} byte");
            }
        }
        let disabled = &app.config_raw.skills.disabled_skills;
        if !disabled.is_empty() {
            println!("Skill nonaktif: {}", disabled.join(", "));
        }
    }

    if deep {
        // Pemeriksaan kredensial provider seperti `hermes doctor`.
        let prof = app.config.profile();
        match app.config.provider(&prof.provider) {
            Some(p) => {
                let has_key = p
                    .api_key_env
                    .as_ref()
                    .map(|e| std::env::var(e).is_ok())
                    .unwrap_or(false)
                    || !p.api_keys.is_empty()
                    || p.name == "ollama";
                if has_key {
                    println!("Kredensial  : provider `{}` OK", p.name);
                } else {
                    println!(
                        "Kredensial  : provider `{}` TANPA API key (env {:?})",
                        p.name, p.api_key_env
                    );
                }
            }
            None => println!(
                "Kredensial  : provider `{}` tidak ada di config",
                prof.provider
            ),
        }
    }
    Ok(())
}
