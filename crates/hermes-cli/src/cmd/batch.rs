//! Handler batch.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::chat::AgentFactory;
use crate::app::App;
use crate::approval::unattended_approval;
use crate::approval::ApprovalKind;

/// Batch: jalankan banyak prompt dari file JSONL, tulis hasil sebagai JSONL.
///
/// Tiap baris input: {"prompt": "...", "session": "id-opsional"}.
/// Batch berjalan non-interaktif: flag CLI (--yes/--readonly) menang atas env,
/// tanpa flag dipakai default env (lihat GatewayApproval).
pub(crate) async fn cmd_batch(
    app: &App,
    input: &str,
    out: Option<&str>,
    approval_kind: ApprovalKind,
) -> anyhow::Result<()> {
    use hermes_core::Session;
    use std::io::Write;

    let data = std::fs::read_to_string(input)?;
    let factory = AgentFactory::from_app(app)
        .with_approval(unattended_approval(approval_kind, &app.profile.security));
    let mut out_buf: Vec<String> = Vec::new();
    let mut n = 0;
    for (i, line) in data.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| anyhow::anyhow!("baris {} bukan JSON valid: {e}", i + 1))?;
        let prompt = item
            .get("prompt")
            .and_then(|p| p.as_str())
            .ok_or_else(|| anyhow::anyhow!("baris {} tanpa field `prompt`", i + 1))?;
        let session_id = item
            .get("session")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("batch-{i}"));
        n += 1;
        eprintln!(
            "[{n}] menjalankan: {}…",
            prompt.chars().take(60).collect::<String>()
        );
        let started = std::time::Instant::now();
        let session = factory
            .memory
            .load_session(&session_id)
            .await?
            .unwrap_or_else(|| {
                let mut s = Session::new("batch");
                s.id = session_id.clone();
                s
            });
        let mut agent = factory.build_quiet(session);
        let result = agent.run_turn(prompt).await;
        let duration_ms = started.elapsed().as_millis();
        let usage = agent.usage();
        let record = match result {
            Ok(response) => serde_json::json!({
                "prompt": prompt,
                "session": session_id,
                "response": response,
                "usage": { "input_tokens": usage.input_tokens, "output_tokens": usage.output_tokens },
                "duration_ms": duration_ms,
            }),
            Err(e) => serde_json::json!({
                "prompt": prompt,
                "session": session_id,
                "error": e.to_string(),
                "duration_ms": duration_ms,
            }),
        };
        out_buf.push(record.to_string());
    }
    let output = out_buf.join("\n");
    match out {
        Some(path) => {
            std::fs::write(path, output)?;
            println!("Selesai: {n} prompt → {path}");
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(output.as_bytes())?;
        }
    }
    Ok(())
}
