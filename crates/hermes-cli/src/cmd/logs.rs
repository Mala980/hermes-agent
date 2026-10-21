//! Handler logs + filter log.
//! Dipindah dari main.rs (refactor batas ukuran file).

// ---------------------------------------------------------------------------
// B7 (tier 1): `hermes logs` — lihat & filter file log.
//
// Paritas hermes_cli/logs.py (LOG_FILES, --level/--session/--since, -f,
// `logs list`). File log tinggal di <HERMES_HOME>/logs/.
// ---------------------------------------------------------------------------

/// Nama log yang dikenal → nama file (paritas LOG_FILES aslinya).
pub(crate) fn log_file_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "agent" => "agent.log",
        "errors" => "errors.log",
        "gateway" => "gateway.log",
        "gui" => "gui.log",
        "desktop" => "desktop.log",
        "update" => "update.log",
        "handoff" => "desktop-update-handoff.log",
        "mcp" => "mcp-stderr.log",
        _ => return None,
    })
}

pub(crate) const KNOWN_LOGS: [&str; 8] = [
    "agent", "errors", "gateway", "gui", "desktop", "update", "handoff", "mcp",
];

/// Urutan level log (paritas _LEVEL_ORDER aslinya; terima WARN maupun WARNING).
pub(crate) fn log_level_order(level: &str) -> Option<u8> {
    Some(match level {
        "DEBUG" => 0,
        "INFO" => 1,
        "WARN" | "WARNING" => 2,
        "ERROR" => 3,
        "CRITICAL" => 4,
        _ => return None,
    })
}

/// Ekstrak token level dari sebuah baris log (asli memakai regex).
pub(crate) fn extract_log_level(line: &str) -> Option<&str> {
    line.split(|c: char| !c.is_ascii_alphabetic()).find(|tok| {
        matches!(
            *tok,
            "DEBUG" | "INFO" | "WARN" | "WARNING" | "ERROR" | "CRITICAL"
        )
    })
}

/// Ekstrak timestamp `YYYY-MM-DD[T ]HH:MM:SS` dari sebuah baris (paritas _TS_RE).
pub(crate) fn extract_log_timestamp(line: &str) -> Option<chrono::NaiveDateTime> {
    let b = line.as_bytes();
    if b.len() < 19 {
        return None;
    }
    for i in 0..=b.len() - 19 {
        let s = &b[i..i + 19];
        let digits = |r: std::ops::Range<usize>| s[r].iter().all(|c| c.is_ascii_digit());
        if s[4] == b'-'
            && s[7] == b'-'
            && (s[10] == b'T' || s[10] == b' ')
            && s[13] == b':'
            && s[16] == b':'
            && digits(0..4)
            && digits(5..7)
            && digits(8..10)
            && digits(11..13)
            && digits(14..16)
            && digits(17..19)
        {
            let norm: String = s
                .iter()
                .map(|&c| char::from(if c == b'T' { b' ' } else { c }))
                .collect();
            if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(&norm, "%Y-%m-%d %H:%M:%S") {
                return Some(dt);
            }
        }
    }
    None
}

/// Parse rentang relatif ala `--since` aslinya: `30s`, `30m`, `1h`, `2d`.
pub(crate) fn parse_log_since(s: &str) -> Option<chrono::Duration> {
    let s = s.trim().to_lowercase();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.trim().parse().ok()?;
    match unit {
        "s" => Some(chrono::Duration::seconds(n)),
        "m" => Some(chrono::Duration::minutes(n)),
        "h" => Some(chrono::Duration::hours(n)),
        "d" => Some(chrono::Duration::days(n)),
        _ => None,
    }
}

pub(crate) struct LogFilter {
    pub(crate) min_level: Option<u8>,
    pub(crate) session: Option<String>,
    pub(crate) since: Option<chrono::NaiveDateTime>,
}

pub(crate) fn log_line_matches(line: &str, f: &LogFilter) -> bool {
    // Paritas _matches_filters aslinya: baris tanpa timestamp/level LOLOS
    // filter waktu/level (hanya baris berstempel yang bisa tersaring).
    if let Some(cutoff) = f.since {
        if let Some(ts) = extract_log_timestamp(line) {
            if ts < cutoff {
                return false;
            }
        }
    }
    if let Some(min) = f.min_level {
        if let Some(ord) = extract_log_level(line).and_then(log_level_order) {
            if ord < min {
                return false;
            }
        }
    }
    if let Some(sess) = &f.session {
        if !line.contains(sess.as_str()) {
            return false;
        }
    }
    true
}

/// N baris terakhir yang lolos filter (tanpa memuat file raksasa ke memori
/// dua kali: iterasi mundur lalu balik).
pub(crate) fn tail_matching_lines<'a>(text: &'a str, n: usize, f: &LogFilter) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines().rev() {
        if log_line_matches(line, f) {
            out.push(line);
            if out.len() >= n.max(1) {
                break;
            }
        }
    }
    out.reverse();
    out
}

pub(crate) async fn cmd_logs(
    home: &std::path::Path,
    log: &str,
    lines: usize,
    follow: bool,
    level: Option<&str>,
    session: Option<&str>,
    since: Option<&str>,
) -> anyhow::Result<()> {
    let log_dir = home.join("logs");

    // `hermes logs list` — daftar file log yang ada beserta ukurannya.
    if log == "list" {
        for name in KNOWN_LOGS {
            let path = log_dir.join(log_file_name(name).unwrap_or("?"));
            match std::fs::metadata(&path) {
                Ok(m) => println!("{name:<10} {:>10} byte  {}", m.len(), path.display()),
                Err(_) => println!("{name:<10} {:>10}  {}", "-", path.display()),
            }
        }
        return Ok(());
    }

    let filename = log_file_name(log).ok_or_else(|| {
        anyhow::anyhow!(
            "log tak dikenal: `{log}` (coba: agent, errors, gateway, gui, desktop, update, handoff, mcp, list)"
        )
    })?;
    let min_level = match level {
        Some(l) => Some(log_level_order(&l.to_uppercase()).ok_or_else(|| {
            anyhow::anyhow!("level tak dikenal: `{l}` (DEBUG, INFO, WARNING, ERROR, CRITICAL)")
        })?),
        None => None,
    };
    let cutoff = match since {
        Some(s) => {
            let d = parse_log_since(s).ok_or_else(|| {
                anyhow::anyhow!("rentang --since tak valid: `{s}` (contoh: 30m, 1h, 2d)")
            })?;
            Some(chrono::Local::now().naive_local() - d)
        }
        None => None,
    };
    let filter = LogFilter {
        min_level,
        session: session.map(str::to_string),
        since: cutoff,
    };

    let path = log_dir.join(filename);
    if !path.is_file() {
        println!(
            "Tidak ada file log di {} (logging file belum aktif untuk sesi ini).",
            path.display()
        );
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    for line in tail_matching_lines(&text, lines, &filter) {
        println!("{line}");
    }

    // -f: tail -f sederhana via polling ukuran file.
    if follow {
        use std::io::{Read, Seek, SeekFrom};
        let mut pos = std::fs::metadata(&path)?.len();
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(pos);
            if len < pos {
                pos = len; // file di-rotate / di-truncate
            } else if len > pos {
                let mut f = std::fs::File::open(&path)?;
                f.seek(SeekFrom::Start(pos))?;
                let mut buf = String::new();
                f.read_to_string(&mut buf)?;
                for line in buf.lines() {
                    if log_line_matches(line, &filter) {
                        println!("{line}");
                    }
                }
                pos = len;
            }
        }
    }
    Ok(())
}
