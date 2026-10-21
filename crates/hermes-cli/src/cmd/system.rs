//! Handler doctor & setup.
//! Dipindah dari main.rs (refactor batas ukuran file).

use crate::app::save_config;

pub(crate) async fn doctor() -> anyhow::Result<()> {
    println!("🩺 hermes doctor\n");
    let home = hermes_core::HermesConfig::home_dir();
    println!("HERMES_HOME: {}", home.display());
    let cfg_path = home.join("config.toml");
    let mut ok = true;

    if cfg_path.is_file() {
        let text = std::fs::read_to_string(&cfg_path).unwrap_or_default();
        match toml::from_str::<hermes_core::HermesConfig>(&text) {
            Ok(cfg) => {
                println!(
                    "✓ config.toml valid ({} provider, {} profil)",
                    cfg.providers.len(),
                    cfg.profiles.len()
                );
                let profile = cfg.profile();
                match cfg.provider(&profile.provider) {
                    Some(p) => {
                        let has_key = p
                            .api_key_env
                            .as_ref()
                            .map(|e| std::env::var(e).is_ok())
                            .unwrap_or(false)
                            || !p.api_keys.is_empty()
                            || p.name == "ollama";
                        if has_key {
                            println!("✓ provider `{}`: kredensial tersedia", p.name);
                        } else {
                            println!(
                                "✗ provider `{}`: API key tidak ditemukan (env {:?})",
                                p.name, p.api_key_env
                            );
                            ok = false;
                        }
                    }
                    None => {
                        println!("✗ provider `{}` tidak ada di config", profile.provider);
                        ok = false;
                    }
                }
            }
            Err(e) => {
                println!("✗ config.toml rusak: {e}");
                ok = false;
            }
        }
    } else {
        println!("! config.toml belum ada (akan dibuat otomatis saat pertama jalan)");
    }

    // SQLite bisa ditulis?
    match hermes_memory::SqliteMemoryStore::open_default(&home) {
        Ok(_) => println!(
            "✓ database memori OK ({})",
            home.join("memory.db").display()
        ),
        Err(e) => {
            println!("✗ database memori: {e}");
            ok = false;
        }
    }
    // Direktori skills.
    let skills = home.join("skills");
    if skills.is_dir() {
        println!("✓ direktori skills OK");
    } else {
        println!("! direktori skills belum ada");
    }
    // Toolchain.
    println!("✓ hermes {}", env!("CARGO_PKG_VERSION"));
    if ok {
        println!("\nSemua pemeriksaan lolos. 🎉");
    } else {
        println!("\nAda masalah — jalankan `hermes setup` untuk memperbaikinya.");
    }
    Ok(())
}

pub(crate) async fn setup() -> anyhow::Result<()> {
    use std::io::{self, Write};
    println!("🧭 Hermes setup\n");
    let home = hermes_core::HermesConfig::home_dir();
    std::fs::create_dir_all(&home)?;

    let providers = hermes_core::HermesConfig::default_providers();
    println!("Pilih provider:");
    for (i, p) in providers.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, p.name, p.base_url);
    }
    print!("Pilihan [1]: ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let idx: usize = input
        .trim()
        .parse::<usize>()
        .unwrap_or(1)
        .saturating_sub(1)
        .min(providers.len() - 1);
    let chosen = &providers[idx];

    if chosen.name != "ollama" {
        print!("API key untuk {}: ", chosen.name);
        io::stdout().flush()?;
        let mut key = String::new();
        io::stdin().read_line(&mut key)?;
        let key = key.trim();
        if !key.is_empty() {
            let env_path = home.join(".env");
            let mut content = std::fs::read_to_string(&env_path).unwrap_or_default();
            let var = chosen
                .api_key_env
                .clone()
                .unwrap_or_else(|| "HERMES_API_KEY".into());
            // Hapus baris lama dengan var yang sama.
            content = content
                .lines()
                .filter(|l| !l.trim_start().starts_with(&format!("{var}=")))
                .collect::<Vec<_>>()
                .join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!("{var}={key}\n"));
            std::fs::write(&env_path, content)?;
            println!("✓ API key disimpan di {}", env_path.display());
        }
    }

    print!(
        "Model [{}]: ",
        chosen.default_model.clone().unwrap_or_default()
    );
    io::stdout().flush()?;
    let mut model = String::new();
    io::stdin().read_line(&mut model)?;
    let model = model.trim();
    let model = if model.is_empty() {
        chosen.default_model.clone().unwrap_or_default()
    } else {
        model.to_string()
    };

    // Tulis config.
    let chosen_name = chosen.name.clone();
    let mut cfg = hermes_core::HermesConfig {
        providers,
        ..Default::default()
    };
    let profile = hermes_core::ProfileConfig {
        provider: chosen_name,
        model,
        ..Default::default()
    };
    cfg.profiles.insert("default".to_string(), profile);
    save_config(&cfg, &home)?;
    println!("\n✓ Setup selesai! Coba:  hermes chat");
    Ok(())
}
