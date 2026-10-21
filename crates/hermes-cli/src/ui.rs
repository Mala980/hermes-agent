//! ui.rs: render event agen ke terminal + REPL interaktif.
use hermes_core::AgentEvent;
use std::io::Write;
use tokio::sync::mpsc;

/// Render event ke stdout/stderr dengan format ringkas nan cantik.
pub async fn render_events(mut rx: mpsc::Receiver<AgentEvent>) {
    let mut first_text = true;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::SessionStart { session_id } => {
                eprintln!("💬 sesi {session_id}");
            }
            AgentEvent::TextDelta(d) => {
                if first_text {
                    first_text = false;
                }
                print!("{d}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::IterationStart { .. } => {}
            AgentEvent::ToolCallStart { name, arguments } => {
                println!();
                let preview: String = serde_json::to_string(&arguments)
                    .unwrap_or_default()
                    .chars()
                    .take(200)
                    .collect();
                println!("🔧 {name} {preview}");
                first_text = true;
            }
            AgentEvent::ToolCallResult {
                name,
                ok,
                preview,
                diff_stat,
            } => {
                let icon = if ok { "✓" } else { "✗" };
                let one_line = preview.replace('\n', " ");
                let diff = diff_stat
                    .map(|d| format!(" +{} −{}", d.added, d.removed))
                    .unwrap_or_default();
                println!("  {icon} {name}{diff}: {one_line}");
            }
            AgentEvent::Compaction {
                before_tokens,
                after_tokens,
            } => {
                eprintln!("🗜️  konteks dikompresi: {before_tokens} → {after_tokens} token");
            }
            AgentEvent::SkillCreated { name } => {
                eprintln!("📚 skill baru dibuat: {name}");
            }
            AgentEvent::MemorySaved { kind } => {
                eprintln!("🧠 memori tersimpan ({kind})");
            }
            AgentEvent::Warning(w) => {
                eprintln!("⚠️  {w}");
            }
            AgentEvent::Done { .. } => {
                println!();
            }
            AgentEvent::Error(e) => {
                eprintln!("\n❌ {e}");
            }
        }
    }
}

/// Aksi yang dikembalikan handler tiap baris REPL.
pub enum ReplAction {
    Continue,
    /// Keluar dari loop REPL (dipakai mode `--oneshot`: satu turn lalu selesai).
    Exit,
}

/// Loop REPL async: readline berjalan di blocking thread, handler async di runtime.
pub async fn repl_loop_async<F, Fut>(mut on_line: F) -> anyhow::Result<()>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<ReplAction>>,
{
    use rustyline::error::ReadlineError;
    use rustyline::DefaultEditor;

    let mut rl = DefaultEditor::new()?;
    let history_path = hermes_core::HermesConfig::home_dir().join("history.txt");
    let _ = rl.load_history(&history_path);
    println!("Hermes (Rust) — ketik /help untuk perintah, /quit untuk keluar.\n");
    let mut editor: Option<DefaultEditor> = Some(rl);
    loop {
        let mut ed = editor.take().expect("editor hilang");
        let (ed_back, line) = tokio::task::spawn_blocking(move || {
            let res = ed.readline("❯ ");
            (ed, res)
        })
        .await?;
        editor = Some(ed_back);
        let ed = editor.as_mut().unwrap();
        match line {
            Ok(input) => {
                let input = input.trim().to_string();
                if input.is_empty() {
                    continue;
                }
                let _ = ed.add_history_entry(input.as_str());
                match input.as_str() {
                    "/quit" | "/exit" | "/q" => break,
                    "/help" => {
                        println!(
                            "Perintah REPL:\n  /quit   keluar\n  /clear  sesi baru\n  /help   bantuan ini"
                        );
                        continue;
                    }
                    _ => {}
                }
                match on_line(input).await? {
                    ReplAction::Continue => {}
                    ReplAction::Exit => break,
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }
    if let Some(ed) = editor.as_mut() {
        let _ = ed.save_history(&history_path);
    }
    Ok(())
}
