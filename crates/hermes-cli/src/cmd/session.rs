//! Handler session (list/show/delete/import/...).
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::chat::resolve_session;
use super::cli::SessionCmd;
use crate::app::App;
use hermes_core::Session;

/// Export sesi tersimpan sebagai trajectory (format riset) atau JSONL mentah.
pub(crate) async fn cmd_session(app: &App, cmd: SessionCmd) -> anyhow::Result<()> {
    use hermes_core::{ContentBlock, Role};

    match cmd {
        SessionCmd::Export {
            session,
            format,
            out,
        } => {
            let sess = app
                .memory
                .load_session(&session)
                .await?
                .ok_or_else(|| anyhow::anyhow!("sesi `{session}` tidak ditemukan"))?;
            let output = match format.as_str() {
                "jsonl" => sess
                    .messages
                    .iter()
                    .map(|m| serde_json::to_string(m).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => {
                    let mut steps: Vec<serde_json::Value> = Vec::new();
                    for m in &sess.messages {
                        match m.role {
                            Role::System => continue,
                            Role::User => {
                                let text = text_of(&m.content);
                                if !text.is_empty() {
                                    steps
                                        .push(serde_json::json!({"role": "user", "content": text}));
                                }
                            }
                            Role::Assistant => {
                                let text = text_of(&m.content);
                                let calls: Vec<_> = m
                                    .content
                                    .iter()
                                    .filter_map(|b| match b {
                                        ContentBlock::ToolCall { call } => {
                                            Some(serde_json::json!({
                                                "id": call.id, "name": call.name,
                                                "arguments": call.arguments,
                                            }))
                                        }
                                        _ => None,
                                    })
                                    .collect();
                                if !calls.is_empty() {
                                    steps.push(serde_json::json!({
                                        "role": "assistant", "tool_calls": calls,
                                    }));
                                }
                                if !text.is_empty() {
                                    steps.push(
                                        serde_json::json!({"role": "assistant", "content": text}),
                                    );
                                }
                            }
                            Role::Tool => {
                                for b in &m.content {
                                    if let ContentBlock::ToolResult { result } = b {
                                        steps.push(serde_json::json!({
                                            "role": "tool",
                                            "tool_call_id": result.tool_call_id,
                                            "name": result.name,
                                            "content": result.content,
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session_id": sess.id,
                        "profile": sess.profile,
                        "steps": steps,
                    }))
                    .unwrap_or_default()
                }
            };
            match out {
                Some(path) => {
                    std::fs::write(&path, output)?;
                    println!("Sesi diekspor ke {path}");
                }
                None => println!("{output}"),
            }
            Ok(())
        }
        // B7 (tier 1): paritas `sessions list` — sesi terbaru dulu.
        SessionCmd::List { limit, source } => {
            let mut sessions = app.memory.list_sessions(&app.profile_name, limit).await?;
            if let Some(src) = source {
                sessions.retain(|s| s.profile.contains(src.as_str()));
            }
            if sessions.is_empty() {
                println!("Tidak ada sesi tersimpan.");
            }
            for s in sessions {
                println!(
                    "{}  {}  [{}]  {} pesan  (diperbarui {})",
                    s.id,
                    if s.title.is_empty() {
                        "Untitled"
                    } else {
                        &s.title
                    },
                    s.profile,
                    s.messages.len(),
                    s.updated_at.format("%Y-%m-%d %H:%M")
                );
            }
            Ok(())
        }
        // B7 (tier 1): ringkasan + riwayat pesan satu sesi.
        SessionCmd::Show { session } => {
            let sess = app
                .memory
                .load_session(&session)
                .await?
                .ok_or_else(|| anyhow::anyhow!("sesi `{session}` tidak ditemukan"))?;
            println!("ID      : {}", sess.id);
            println!(
                "Judul   : {}",
                if sess.title.is_empty() {
                    "Untitled"
                } else {
                    &sess.title
                }
            );
            println!("Profil  : {}", sess.profile);
            println!("Dibuat  : {}", sess.created_at.format("%Y-%m-%d %H:%M:%S"));
            println!(
                "Diperbarui: {}",
                sess.updated_at.format("%Y-%m-%d %H:%M:%S")
            );
            println!("Pesan   : {}", sess.messages.len());
            println!("---");
            for m in &sess.messages {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "asisten",
                    Role::Tool => "tool",
                };
                let calls: Vec<String> = m
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolCall { call } => Some(call.name.clone()),
                        _ => None,
                    })
                    .collect();
                let text = text_of(&m.content);
                let ringkas: String = text.chars().take(160).collect();
                if calls.is_empty() {
                    println!("[{role}] {ringkas}");
                } else {
                    println!("[{role}] {ringkas} (tool: {})", calls.join(", "));
                }
            }
            Ok(())
        }
        // B7 (tier 1): paritas `sessions delete <id> --yes`.
        SessionCmd::Delete { session_id, yes } => {
            if !yes && !confirm_delete(&session_id) {
                println!("Dibatalkan.");
                return Ok(());
            }
            app.memory.delete_session(&session_id).await?;
            println!("Sesi `{session_id}` dihapus.");
            Ok(())
        }
        // B8 (tier 2): impor sesi dari file hasil `session export`.
        SessionCmd::Import {
            file,
            format,
            id,
            title,
        } => cmd_session_import(app, &file, &format, id, title).await,
        // B8 (tier 2): paritas `sessions rename` asli.
        SessionCmd::Rename { session, title } => {
            let title = title.join(" ").trim().to_string();
            if title.is_empty() {
                anyhow::bail!("judul baru tidak boleh kosong");
            }
            let mut sess = resolve_session(&app.memory, &app.profile_name, &session).await?;
            let old = std::mem::replace(&mut sess.title, title.clone());
            sess.updated_at = chrono::Utc::now();
            app.memory.save_session(&sess).await?;
            println!(
                "Sesi `{}` diganti judulnya:\n  lama : {old}\n  baru : {title}",
                sess.id
            );
            Ok(())
        }
        // B8 (tier 2): paritas `sessions stats` asli.
        SessionCmd::Stats => cmd_session_stats(app).await,
        // B8 (tier 2): paritas `sessions prune` asli (inti berbasis usia).
        SessionCmd::Prune {
            older_than,
            dry_run,
            yes,
        } => cmd_session_prune(app, &older_than, dry_run, yes).await,
    }
}

/// Parse usia untuk `session prune`: "5h", "2d", "1w", atau angka polos
/// (= hari), meniru format AGE aslinya.
pub(crate) fn parse_age(s: &str) -> Option<chrono::Duration> {
    let s = s.trim();
    let (num, mult): (&str, i64) = if let Some(h) = s.strip_suffix('h') {
        (h, 3_600)
    } else if let Some(d) = s.strip_suffix('d') {
        (d, 86_400)
    } else if let Some(w) = s.strip_suffix('w') {
        (w, 604_800)
    } else {
        (s, 86_400)
    };
    num.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| chrono::Duration::seconds(n * mult))
}

/// Konfirmasi generik y/N via stdin; aman di non-TTY (EOF = batal).
pub(crate) fn confirm_action(prompt: &str) -> bool {
    use std::io::{self, Write};
    print!("{prompt} [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "ya" | "yes"),
        Err(_) => false,
    }
}

/// Semua nama profil yang dikenal config (+ profil aktif), tanpa duplikat.
pub(crate) fn all_profile_names(app: &App) -> Vec<String> {
    let mut names: Vec<String> = app.config.profiles.keys().cloned().collect();
    if !names.contains(&app.profile_name) {
        names.push(app.profile_name.clone());
    }
    names.sort();
    names
}

/// B8: `session import` — baca kembali output `session export` (jsonl atau
/// trajectory) ke dalam session store.
pub(crate) async fn cmd_session_import(
    app: &App,
    file: &str,
    format: &str,
    id: Option<String>,
    title: Option<String>,
) -> anyhow::Result<()> {
    use hermes_core::{ContentBlock, Message, Role, ToolCall, ToolResult};

    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("gagal membaca file impor `{file}`: {e}"))?;
    let (messages, file_title): (Vec<Message>, Option<String>) = match format {
        "jsonl" => {
            let mut msgs = Vec::new();
            for (i, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let m: Message = serde_json::from_str(line).map_err(|e| {
                    anyhow::anyhow!("baris {} bukan Message JSON valid: {e}", i + 1)
                })?;
                msgs.push(m);
            }
            (msgs, None)
        }
        "trajectory" => {
            let v: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| anyhow::anyhow!("file trajectory bukan JSON valid: {e}"))?;
            let mut msgs = Vec::new();
            let steps = v
                .get("steps")
                .and_then(|s| s.as_array())
                .ok_or_else(|| anyhow::anyhow!("trajectory tidak punya array `steps`"))?;
            for step in steps {
                let role = step.get("role").and_then(|r| r.as_str()).unwrap_or("");
                match role {
                    "user" => {
                        let t = step.get("content").and_then(|c| c.as_str()).unwrap_or("");
                        msgs.push(Message::user(t));
                    }
                    "assistant" => {
                        let mut blocks = Vec::new();
                        if let Some(calls) = step.get("tool_calls").and_then(|c| c.as_array()) {
                            for c in calls {
                                blocks.push(ContentBlock::ToolCall {
                                    call: ToolCall {
                                        id: c
                                            .get("id")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        name: c
                                            .get("name")
                                            .and_then(|x| x.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        arguments: c
                                            .get("arguments")
                                            .cloned()
                                            .unwrap_or(serde_json::Value::Null),
                                    },
                                });
                            }
                        }
                        if let Some(t) = step.get("content").and_then(|c| c.as_str()) {
                            if !t.is_empty() {
                                blocks.push(ContentBlock::Text {
                                    text: t.to_string(),
                                });
                            }
                        }
                        msgs.push(Message {
                            role: Role::Assistant,
                            content: blocks,
                            name: None,
                        });
                    }
                    "tool" => {
                        msgs.push(Message {
                            role: Role::Tool,
                            content: vec![ContentBlock::ToolResult {
                                result: ToolResult {
                                    tool_call_id: step
                                        .get("tool_call_id")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    name: step
                                        .get("name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    content: step
                                        .get("content")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("")
                                        .to_string(),
                                    is_error: false,
                                },
                            }],
                            name: None,
                        });
                    }
                    other => anyhow::bail!("role tak dikenal di trajectory: `{other}`"),
                }
            }
            let t = v
                .get("session_id")
                .and_then(|x| x.as_str())
                .map(|s| format!("imported-{s}"));
            (msgs, t)
        }
        other => anyhow::bail!("--format `{other}` tidak dikenal (valid: jsonl, trajectory)"),
    };
    if messages.is_empty() {
        anyhow::bail!("tidak ada pesan yang bisa diimpor dari `{file}`");
    }
    let mut sess = Session::new(app.profile_name.clone());
    if let Some(want_id) = id {
        if app.memory.load_session(&want_id).await?.is_some() {
            anyhow::bail!("ID sesi `{want_id}` sudah dipakai");
        }
        sess.id = want_id;
    }
    sess.title = title
        .or(file_title)
        .unwrap_or_else(|| "Imported".to_string());
    sess.messages = messages;
    let n = sess.messages.len();
    app.memory.save_session(&sess).await?;
    println!(
        "Diimpor {n} pesan sebagai sesi `{}` (\"{}\").",
        sess.id, sess.title
    );
    Ok(())
}

/// B8: `session stats` — agregat sesi/pesan per profil.
pub(crate) async fn cmd_session_stats(app: &App) -> anyhow::Result<()> {
    let mut total_sessions = 0usize;
    let mut total_messages = 0usize;
    let mut rows = Vec::new();
    for profile in all_profile_names(app) {
        let sessions = app.memory.list_sessions(&profile, 1_000_000).await?;
        let msgs: usize = sessions.iter().map(|s| s.messages.len()).sum();
        total_sessions += sessions.len();
        total_messages += msgs;
        rows.push((profile, sessions.len(), msgs));
    }
    println!("Statistik session store:");
    for (profile, ns, nm) in rows {
        println!("  {profile:<16} {ns:>5} sesi   {nm:>7} pesan");
    }
    println!(
        "  {0:<16} {total_sessions:>5} sesi   {total_messages:>7} pesan",
        "TOTAL"
    );
    Ok(())
}

/// B8: `session prune` — hapus sesi yang `updated_at`-nya lebih tua dari usia.
pub(crate) async fn cmd_session_prune(
    app: &App,
    older_than: &str,
    dry_run: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let age = parse_age(older_than).ok_or_else(|| {
        anyhow::anyhow!("usia tidak valid: `{older_than}` (contoh: 90, 30d, 5h, 1w)")
    })?;
    let cutoff = chrono::Utc::now() - age;
    let mut victims: Vec<(String, String, String)> = Vec::new();
    for profile in all_profile_names(app) {
        for s in app.memory.list_sessions(&profile, 1_000_000).await? {
            if s.updated_at < cutoff {
                victims.push((
                    s.id.clone(),
                    if s.title.is_empty() {
                        "Untitled".to_string()
                    } else {
                        s.title.clone()
                    },
                    s.updated_at.format("%Y-%m-%d %H:%M").to_string(),
                ));
            }
        }
    }
    if victims.is_empty() {
        println!("Tidak ada sesi yang lebih tua dari `{older_than}`.");
        return Ok(());
    }
    println!("{} sesi lebih tua dari `{older_than}`:", victims.len());
    for (id, title, updated) in &victims {
        println!("  {id}  {title}  (diperbarui {updated})");
    }
    if dry_run {
        println!("(dry-run: tidak ada yang dihapus)");
        return Ok(());
    }
    if !yes && !confirm_action(&format!("Hapus {} sesi ini?", victims.len())) {
        println!("Dibatalkan.");
        return Ok(());
    }
    for (id, _, _) in &victims {
        app.memory.delete_session(id).await?;
    }
    println!("Dihapus {} sesi.", victims.len());
    Ok(())
}

/// Konfirmasi hapus sesi: y/N via stdin; aman di non-TTY (EOF = batal).
pub(crate) fn confirm_delete(session_id: &str) -> bool {
    use std::io::{self, Write};
    print!("Hapus sesi `{session_id}`? [y/N] ");
    let _ = io::stdout().flush();
    let mut line = String::new();
    match io::stdin().read_line(&mut line) {
        Ok(_) => matches!(line.trim().to_lowercase().as_str(), "y" | "ya" | "yes"),
        Err(_) => false,
    }
}

pub(crate) fn text_of(blocks: &[hermes_core::ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            hermes_core::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
