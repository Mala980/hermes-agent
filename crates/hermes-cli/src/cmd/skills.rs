//! Handler skills & skills hub.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::cli::{HubCmd, SkillsCmd};
use crate::app::App;

pub(crate) async fn cmd_skills(app: &App, cmd: SkillsCmd) -> anyhow::Result<()> {
    match cmd {
        SkillsCmd::List => {
            let skills = app.skills.list().await?;
            if skills.is_empty() {
                println!("Belum ada skill. Agen akan membuatnya otomatis saat bekerja.");
            }
            for s in skills {
                println!("- {}: {}", s.name, s.description);
            }
        }
        SkillsCmd::Read { name } => match app.skills.get(&name).await? {
            Some(s) => println!("# {}\n\n{}", s.name, s.body),
            None => println!("Skill `{name}` tidak ditemukan."),
        },
        SkillsCmd::Create {
            name,
            description,
            body,
        } => {
            let s = app.skills.create(&name, &description, &body).await?;
            println!("Skill `{}` dibuat di {}", s.name, s.path.display());
        }
        SkillsCmd::Delete { name } => {
            app.skills.delete(&name).await?;
            println!("Skill `{name}` dihapus.");
        }
        // B8 (tier 2): paritas `skills inspect` asli — metadata + pratinjau.
        SkillsCmd::Inspect { name } => match app.skills.get(&name).await? {
            Some(s) => {
                println!("Nama       : {}", s.name);
                println!("Deskripsi  : {}", s.description);
                println!("Path       : {}", s.path.display());
                println!(
                    "Trigger    : {}",
                    if s.triggers.is_empty() {
                        "-".to_string()
                    } else {
                        s.triggers.join(", ")
                    }
                );
                let lines: Vec<&str> = s.body.lines().collect();
                println!("Baris      : {}", lines.len());
                println!("--- pratinjau (8 baris pertama) ---");
                for l in lines.iter().take(8) {
                    println!("{l}");
                }
                if lines.len() > 8 {
                    println!("... ({} baris lagi)", lines.len() - 8);
                }
            }
            None => println!("Skill `{name}` tidak ditemukan."),
        },
        SkillsCmd::Hub { cmd } => cmd_skills_hub(app, cmd).await?,
    }
    Ok(())
}

/// Skills hub: cari & pasang skill dari sumber eksternal.
pub(crate) async fn cmd_skills_hub(app: &App, cmd: HubCmd) -> anyhow::Result<()> {
    match cmd {
        HubCmd::Search { query, limit } => {
            // Pencarian repositori GitHub (tanpa auth; rate limit publik berlaku).
            let client = reqwest::Client::builder()
                .user_agent("hermes-rs/0.1")
                .build()?;
            let resp = client
                .get("https://api.github.com/search/repositories")
                .query(&[
                    ("q", format!("{query} in:name,description skill")),
                    ("per_page", limit.clamp(1, 20).to_string()),
                ])
                .send()
                .await?;
            if !resp.status().is_success() {
                anyhow::bail!("GitHub API menolak: HTTP {}", resp.status());
            }
            let v: serde_json::Value = resp.json().await?;
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            if items.is_empty() {
                println!("Tidak ada hasil untuk `{query}`.");
                return Ok(());
            }
            for it in items {
                let full = it.get("full_name").and_then(|n| n.as_str()).unwrap_or("?");
                let desc = it.get("description").and_then(|d| d.as_str()).unwrap_or("");
                let stars = it
                    .get("stargazers_count")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0);
                println!("- {full} (★{stars}): {desc}");
                println!("  pasang: hermes skills hub install github:{full}");
            }
        }
        HubCmd::Install { source } => {
            let tmp = std::env::temp_dir().join(format!(
                "hermes-hub-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            ));
            let skill_dirs: Vec<std::path::PathBuf> = if source.starts_with("github:") {
                let rest = source.trim_start_matches("github:");
                let (repo, subpath) = match rest.split_once('/') {
                    Some((owner, tail)) => match tail.split_once('/') {
                        Some((repo, path)) => (format!("{owner}/{repo}"), Some(path)),
                        None => (format!("{owner}/{tail}"), None),
                    },
                    None => anyhow::bail!("format github:owner/repo[/path]"),
                };
                let url = format!("https://github.com/{repo}.git");
                println!("Mengkloning {url} ...");
                let st = tokio::process::Command::new("git")
                    .args(["clone", "--depth", "1", &url, &tmp.display().to_string()])
                    .status()
                    .await?;
                if !st.success() {
                    anyhow::bail!("git clone gagal");
                }
                let base = match subpath {
                    Some(p) => tmp.join(p),
                    None => tmp.clone(),
                };
                find_skill_dirs(&base)
            } else if source.starts_with("http://")
                || source.starts_with("https://")
                || source.ends_with(".git")
            {
                println!("Mengkloning {source} ...");
                let st = tokio::process::Command::new("git")
                    .args(["clone", "--depth", "1", &source, &tmp.display().to_string()])
                    .status()
                    .await?;
                if !st.success() {
                    anyhow::bail!("git clone gagal");
                }
                find_skill_dirs(&tmp)
            } else {
                find_skill_dirs(std::path::Path::new(&source))
            };
            if skill_dirs.is_empty() {
                anyhow::bail!("tidak ada SKILL.md ditemukan di sumber tersebut");
            }
            for dir in skill_dirs {
                let name = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("skill")
                    .to_string();
                let body = std::fs::read_to_string(dir.join("SKILL.md"))?;
                // Ambil deskripsi dari baris frontmatter/deskripsi pertama bila ada.
                let description = body
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("description:"))
                    .map(|l| {
                        l.trim_start_matches(|c: char| c.is_alphabetic() || c == ':')
                            .trim()
                            .to_string()
                    })
                    .unwrap_or_else(|| format!("Skill dari hub: {name}"));
                match app.skills.create(&name, &description, &body).await {
                    Ok(s) => println!("Terpasang: `{}` → {}", s.name, s.path.display()),
                    Err(e) => println!("Gagal memasang `{name}`: {e}"),
                }
            }
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }
    Ok(())
}

/// Cari direktori berisi SKILL.md (rekursif, maks kedalaman 3).
pub(crate) fn find_skill_dirs(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    fn walk(dir: &std::path::Path, depth: u8, out: &mut Vec<std::path::PathBuf>) {
        if depth > 3 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut has_skill = false;
        let mut subdirs = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // Lewati direktori git/node_modules yang besar.
                if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                    if n == ".git" || n == "node_modules" || n == "target" {
                        continue;
                    }
                }
                subdirs.push(p);
            } else if e.file_name() == "SKILL.md" {
                has_skill = true;
            }
        }
        if has_skill {
            out.push(dir.to_path_buf());
        } else {
            for s in subdirs {
                walk(&s, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(base, 0, &mut out);
    out
}
