//! Handler eval + suite bawaan.
//! Dipindah dari main.rs (refactor batas ukuran file).

use super::chat::AgentFactory;
use super::cli::EvalCmd;
use crate::app::App;
use crate::approval::GatewayApproval;
use hermes_core::Session;
use std::sync::Arc;

/// Evaluasi: jalankan suite kasus uji (JSONL) lewat agen dan skor hasilnya.
///
/// Format tiap baris suite:
/// {"name": "...", "prompt": "...", "checks": ["contains:teks", "not_contains:teks",
///  "regex:pola", "file_exists:path", "file_contains:path:teks"]}
/// `suite` boleh `builtin:tools` untuk suite bawaan (operasi file/exec/memory/todo).
pub(crate) async fn cmd_eval(app: &App, cmd: &EvalCmd) -> anyhow::Result<()> {
    match cmd {
        // B8 (tier 2): daftar suite bawaan tanpa menjalankannya.
        EvalCmd::List => {
            let tools = builtin_tool_suite();
            println!("Suite bawaan:");
            println!(
                "  builtin:tools — {} kasus (operasi file/exec/memory/todo)",
                tools.len()
            );
            println!("\nJalankan: hermes eval run builtin:tools");
            Ok(())
        }
        EvalCmd::Run {
            suite,
            out,
            threshold,
        } => cmd_eval_run(app, suite, out.as_deref(), *threshold).await,
    }
}

/// Ambang lulus/gagal evaluasi: `true` bila `score_pct` mencapai `threshold_pct`.
/// Toleransi epsilon kecil untuk galat pembulatan float.
pub(crate) fn eval_passes(score_pct: f64, threshold_pct: f64) -> bool {
    score_pct + 1e-9 >= threshold_pct
}

/// Normalisasi ambang ke rentang 0–100 (nilai di luar rentang dijepit).
pub(crate) fn clamp_threshold(t: f64) -> f64 {
    t.clamp(0.0, 100.0)
}

/// Isi lama `cmd_eval`: jalankan suite dan skor hasilnya.
///
/// `threshold_override`: ambang kelulusan persen dari flag `--threshold`;
/// bila `None`, dipakai `[evals].threshold` dari config (default 100).
pub(crate) async fn cmd_eval_run(
    app: &App,
    suite: &str,
    out: Option<&str>,
    threshold_override: Option<f64>,
) -> anyhow::Result<()> {
    let cases: Vec<serde_json::Value> = if suite == "builtin:tools" {
        builtin_tool_suite()
    } else {
        let text = std::fs::read_to_string(suite)?;
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("suite tidak valid: {e}"))?
    };
    if cases.is_empty() {
        anyhow::bail!("suite kosong");
    }
    println!("Menjalankan {} kasus evaluasi...", cases.len());
    let mut results = Vec::new();
    let mut passed = 0usize;
    let factory = AgentFactory::from_app(app).with_approval(Arc::new(GatewayApproval::from_env()));
    for (i, case) in cases.iter().enumerate() {
        let name = case.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let prompt = case.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
        let checks: Vec<String> = case
            .get("checks")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let session = Session::new(app.profile_name.clone());
        let mut agent = factory.build_quiet(session);
        let reply = agent
            .run_turn(prompt)
            .await
            .unwrap_or_else(|e| format!("[error: {e}]"));
        let mut failures = Vec::new();
        for check in &checks {
            if let Err(reason) = eval_check(check, &reply) {
                failures.push(format!("{check} → {reason}"));
            }
        }
        let ok = failures.is_empty();
        if ok {
            passed += 1;
        }
        println!(
            "[{}/{}] {} — {}",
            i + 1,
            cases.len(),
            name,
            if ok { "LULUS ✅" } else { "GAGAL ❌" }
        );
        for f in &failures {
            println!("    - {f}");
        }
        results.push(serde_json::json!({
            "name": name, "passed": ok, "failures": failures,
            "reply_preview": reply.chars().take(300).collect::<String>(),
        }));
    }
    let score = passed as f64 / cases.len() as f64 * 100.0;
    // Ambang lulus/gagal: flag --threshold menimpa [evals].threshold config.
    let threshold = clamp_threshold(threshold_override.unwrap_or(app.config.evals.threshold));
    let suite_passed = eval_passes(score, threshold);
    println!(
        "\nSkor: {passed}/{} ({score:.1}%) — ambang {threshold:.1}% → {}",
        cases.len(),
        if suite_passed {
            "LULUS ✅"
        } else {
            "GAGAL ❌"
        }
    );
    if let Some(out_path) = out {
        let mut text = String::new();
        for r in &results {
            text.push_str(&serde_json::to_string(r)?);
            text.push('\n');
        }
        text.push_str(&serde_json::to_string(&serde_json::json!({
            "summary": {
                "passed": passed, "total": cases.len(), "score": score,
                "threshold": threshold, "suite_passed": suite_passed,
            }
        }))?);
        text.push('\n');
        std::fs::write(out_path, text)?;
        println!("Hasil disimpan di {out_path}");
    }
    if !suite_passed {
        anyhow::bail!("evaluasi GAGAL: skor {score:.1}% di bawah ambang {threshold:.1}%");
    }
    Ok(())
}

pub(crate) fn eval_check(check: &str, reply: &str) -> std::result::Result<(), String> {
    if let Some(needle) = check.strip_prefix("contains:") {
        if reply.contains(needle) {
            Ok(())
        } else {
            Err("tidak mengandung teks".into())
        }
    } else if let Some(needle) = check.strip_prefix("not_contains:") {
        if !reply.contains(needle) {
            Ok(())
        } else {
            Err("mengandung teks terlarang".into())
        }
    } else if let Some(pattern) = check.strip_prefix("regex:") {
        // Regex sederhana: dukung pola literal + `.*` bila feature penuh tidak tersedia.
        if simple_regex_match(pattern, reply) {
            Ok(())
        } else {
            Err("regex tidak cocok".into())
        }
    } else if let Some(path) = check.strip_prefix("file_exists:") {
        if std::path::Path::new(path).exists() {
            Ok(())
        } else {
            Err("file tidak ada".into())
        }
    } else if let Some(rest) = check.strip_prefix("file_contains:") {
        let (path, needle) = rest
            .split_once(':')
            .ok_or("format file_contains:path:teks")?;
        let content =
            std::fs::read_to_string(path).map_err(|_| "file tidak bisa dibaca".to_string())?;
        if content.contains(needle) {
            Ok(())
        } else {
            Err("file tidak mengandung teks".into())
        }
    } else {
        Err("check tidak dikenal".to_string())
    }
}

/// Pencocokan regex minimal tanpa dependensi: literal dengan `.*` sebagai wildcard.
pub(crate) fn simple_regex_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split(".*").collect();
    if parts.len() == 1 {
        return text.contains(pattern);
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
        if i == 0 && !text.starts_with(part) && pattern.starts_with(part) {
            // `.*` di awal sudah ditangani oleh pencarian bebas.
        }
    }
    true
}

/// Suite bawaan: kemampuan tool dasar (file, exec, todo, memory).
pub(crate) fn builtin_tool_suite() -> Vec<serde_json::Value> {
    let dir = std::env::temp_dir().join("hermes-eval-tools");
    let f1 = dir.join("eval_test.txt");
    vec![
        serde_json::json!({
            "name": "write_and_read_file",
            "prompt": format!("Tulis file {} berisi tepat kalimat HELLO EVAL, lalu baca kembali dan laporkan isinya.", f1.display()),
            "checks": [
                format!("file_contains:{}:HELLO EVAL", f1.display()),
                "contains:HELLO EVAL",
            ],
        }),
        serde_json::json!({
            "name": "exec_echo",
            "prompt": "Jalankan perintah shell: echo EVAL_OK_123. Laporkan outputnya apa adanya.",
            "checks": ["contains:EVAL_OK_123"],
        }),
        serde_json::json!({
            "name": "todo_list_track",
            "prompt": "Buat todo_list dengan 2 tugas: 'tugas eval A' dan 'tugas eval B', tandai yang pertama in_progress, lalu tampilkan daftarnya.",
            "checks": ["contains:tugas eval A", "contains:in_progress"],
        }),
        serde_json::json!({
            "name": "memory_remember",
            "prompt": "Ingat fakta ini dengan memory: 'warna favorit eval adalah biru langit'. Lalu jawab: apa warna favorit eval?",
            "checks": ["contains:biru langit"],
        }),
    ]
}

#[cfg(test)]
mod eval_threshold_tests {
    use super::*;

    #[test]
    fn passes_at_or_above_threshold() {
        assert!(eval_passes(100.0, 100.0));
        assert!(eval_passes(75.0, 75.0));
        assert!(eval_passes(80.0, 75.0));
        assert!(eval_passes(0.0, 0.0));
    }

    #[test]
    fn fails_below_threshold() {
        assert!(!eval_passes(74.9, 75.0));
        assert!(!eval_passes(0.0, 100.0));
        assert!(!eval_passes(99.99, 100.0));
    }

    #[test]
    fn tolerates_float_rounding() {
        // 2/3*100 = 66.666...: tidak boleh gagal hanya karena pembulatan.
        assert!(eval_passes(2.0 / 3.0 * 100.0, 2.0 / 3.0 * 100.0));
    }

    #[test]
    fn clamp_keeps_range() {
        assert_eq!(clamp_threshold(75.0), 75.0);
        assert_eq!(clamp_threshold(-5.0), 0.0);
        assert_eq!(clamp_threshold(150.0), 100.0);
    }

    #[test]
    fn default_eval_threshold_is_strict() {
        // Tanpa config/flag: semua kasus harus lulus.
        assert_eq!(hermes_core::EvalsConfig::default().threshold, 100.0);
    }
}
