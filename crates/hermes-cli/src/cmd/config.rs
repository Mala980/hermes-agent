//! Handler config.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::ConfigCmd;
use crate::app::expand_config_env;
use crate::app::save_config;
use crate::app::App;

pub(crate) async fn cmd_config(app: &mut App, cmd: ConfigCmd) -> anyhow::Result<()> {
    match cmd {
        ConfigCmd::Show => {
            // Tampilkan config MENTAH (referensi ${VAR} utuh) — nilai hasil
            // ekspansi env tidak dibocorkan ke output.
            println!("{}", toml::to_string_pretty(&app.config_raw)?);
        }
        ConfigCmd::Path => {
            println!("{}", app.hermes_home.join("config.toml").display());
        }
        ConfigCmd::Set { key, value } => {
            let profile = app
                .config_raw
                .profiles
                .entry(app.profile_name.clone())
                .or_default();
            match key.as_str() {
                "model" => profile.model = value,
                "provider" => profile.provider = value,
                "working_dir" => profile.working_dir = value,
                "temperature" => profile.temperature = value.parse().unwrap_or(0.7),
                "max_iterations" => profile.max_iterations = value.parse().unwrap_or(500),
                "reasoning" => profile.reasoning_effort = Some(value),
                other => anyhow::bail!("kunci tidak dikenal: {other} (model, provider, working_dir, temperature, max_iterations, reasoning)"),
            }
            // Simpan versi mentah agar referensi ${VAR} tidak "terbakar",
            // lalu segarkan salinan efektif untuk sisa proses ini.
            save_config(&app.config_raw, &app.hermes_home)?;
            app.config = expand_config_env(app.config_raw.clone())?;
            println!("Diset {key} untuk profil `{}`.", app.profile_name);
        }
        // B7 (tier 1): paritas `config get <key>` — cetak nilai TER-RESOLVE
        // (hasil ekspansi env), seperti aslinya.
        ConfigCmd::Get { key, json } => {
            let profile = app.config.profile();
            let pairs: Vec<(&str, String)> = vec![
                ("profile", app.profile_name.clone()),
                ("provider", profile.provider.clone()),
                ("model", profile.model.clone()),
                ("working_dir", profile.working_dir.clone()),
                ("temperature", profile.temperature.to_string()),
                ("max_iterations", profile.max_iterations.to_string()),
                (
                    "reasoning",
                    profile.reasoning_effort.clone().unwrap_or_default(),
                ),
            ];
            match key {
                Some(k) => {
                    let val = pairs
                        .iter()
                        .find(|(name, _)| *name == k.as_str())
                        .map(|(_, v)| v.clone());
                    match val {
                        Some(v) => {
                            if json {
                                let mut m = serde_json::Map::new();
                                m.insert(k.clone(), serde_json::Value::String(v));
                                println!("{}", serde_json::Value::Object(m));
                            } else {
                                println!("{v}");
                            }
                        }
                        None => anyhow::bail!(
                            "kunci tidak dikenal: {k} (model, provider, working_dir, temperature, max_iterations, reasoning, profile)"
                        ),
                    }
                }
                None => {
                    if json {
                        let map: std::collections::BTreeMap<&str, String> =
                            pairs.into_iter().collect();
                        println!("{}", serde_json::to_string_pretty(&map)?);
                    } else {
                        for (name, v) in &pairs {
                            println!("{name} = {v}");
                        }
                    }
                }
            }
        }
        // B7 (tier 1): paritas `config unset <key>` — kembalikan ke default.
        ConfigCmd::Unset { key } => {
            let profile = app
                .config_raw
                .profiles
                .entry(app.profile_name.clone())
                .or_default();
            let defaults = hermes_core::ProfileConfig::default();
            match key.as_str() {
                "model" => profile.model = defaults.model,
                "provider" => profile.provider = defaults.provider,
                "working_dir" => profile.working_dir = defaults.working_dir,
                "temperature" => profile.temperature = defaults.temperature,
                "max_iterations" => profile.max_iterations = defaults.max_iterations,
                "reasoning" => profile.reasoning_effort = None,
                other => anyhow::bail!("kunci tidak dikenal: {other} (model, provider, working_dir, temperature, max_iterations, reasoning)"),
            }
            save_config(&app.config_raw, &app.hermes_home)?;
            app.config = expand_config_env(app.config_raw.clone())?;
            println!(
                "`{key}` dikembalikan ke default untuk profil `{}`.",
                app.profile_name
            );
        }
        // B8 (tier 2): paritas `config check` — validasi profil aktif.
        ConfigCmd::Check => {
            let profile = app.config.profile();
            let mut failures = 0u32;
            let mut check = |ok: bool, warn: bool, label: &str, detail: &str| {
                let tag = if ok {
                    "OK  "
                } else if warn {
                    "WARN"
                } else {
                    "FAIL"
                };
                if !ok && !warn {
                    failures += 1;
                }
                println!("{tag}  {label:<14} {detail}");
            };
            check(
                !profile.model.trim().is_empty(),
                false,
                "model",
                &format!("`{}`", profile.model),
            );
            match app.config.provider(&profile.provider) {
                Some(entry) => {
                    check(true, false, "provider", &format!("`{}`", entry.name));
                    let known = matches!(
                        entry.protocol.as_str(),
                        "openai" | "anthropic" | "gemini" | "azure" | "bedrock"
                    );
                    check(known, false, "protokol", &format!("`{}`", entry.protocol));
                }
                None if app.config.providers.is_empty() => {
                    check(
                        false,
                        true,
                        "provider",
                        &format!(
                            "`{}` (daftar provider kosong — pakai default)",
                            profile.provider
                        ),
                    );
                }
                None => {
                    check(
                        false,
                        false,
                        "provider",
                        &format!("`{}` tidak ada di config", profile.provider),
                    );
                }
            }
            let wd = std::path::Path::new(&profile.working_dir);
            check(wd.is_dir(), true, "working_dir", &profile.working_dir);
            check(
                (0.0..=2.0).contains(&profile.temperature),
                true,
                "temperature",
                &profile.temperature.to_string(),
            );
            check(
                profile.max_iterations > 0,
                true,
                "max_iterations",
                &profile.max_iterations.to_string(),
            );
            if failures > 0 {
                anyhow::bail!("{failures} pemeriksaan gagal — perbaiki config lalu ulangi");
            }
            println!("Konfigurasi profil `{}` sehat.", app.profile_name);
        }
    }
    Ok(())
}
