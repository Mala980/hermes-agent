//! Platform Slack: kirim via Web API, terima via Socket Mode (websocket).
//!
//! Butuh dua token: `xoxb-...` (bot token) untuk kirim, `xapp-...` (app token)
//! untuk Socket Mode. Aktifkan Socket Mode di api.slack.com/apps.
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hermes_core::{HermesError, Platform, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::allowlist::{env_truthy, read_allow_csv};
use crate::split::{split_message, SLACK_MESSAGE_LIMIT};

/// Kapasitas cache dedup envelope (FIFO): replay setelah reconnect
/// tidak pernah melebihi beberapa ratus event.
const DEDUP_CAPACITY: usize = 1000;
/// Batas atas backoff reconnect Socket Mode: 60 detik.
const RECONNECT_BACKOFF_CAP_SECS: u64 = 60;
/// Koneksi yang bertahan minimal selama ini dianggap sehat: hitungan
/// backoff di-reset agar flap berulang tidak terjebak di backoff panjang.
const BACKOFF_RESET_AFTER_SECS: u64 = 60;

/// Base URL Slack API; bisa di-override via SLACK_API_BASE untuk pengujian.
fn api(path: &str) -> String {
    let base =
        std::env::var("SLACK_API_BASE").unwrap_or_else(|_| "https://slack.com/api".to_string());
    format!("{}{}", base.trim_end_matches('/'), path)
}

pub struct SlackPlatform {
    bot_token: String,
    client: reqwest::Client,
}

impl SlackPlatform {
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            bot_token: bot_token.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SlackMessage {
    pub channel: String,
    pub user: String,
    pub text: String,
    pub ts: String,
}

#[async_trait]
impl Platform for SlackPlatform {
    fn id(&self) -> &str {
        "slack"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: channel id (atau "channel:ts" untuk reply dalam thread).
        let (channel, thread_ts) = match target.split_once(':') {
            Some((c, t)) if c.starts_with('C') || c.starts_with('D') => (c, Some(t)),
            _ => (target, None),
        };
        // Batas Slack 39000 karakter (API mengizinkan 40.000); pesan yang
        // lebih panjang dikirim sebagai beberapa pesan, pecah sadar-pagar.
        // Teks diformat ke mrkdwn dulu seperti aslinya (`_format_chunks`).
        let formatted = format_slack_mrkdwn(text);
        for chunk in split_message(&formatted, Some(SLACK_MESSAGE_LIMIT)) {
            // `"mrkdwn": true` — seperti `_post_chunks` aslinya.
            let mut body = json!({ "channel": channel, "text": chunk, "mrkdwn": true });
            if let Some(ts) = thread_ts {
                body["thread_ts"] = json!(ts);
            }
            // Bungkus sebagai closure agar request identik bisa dikirim ulang
            // bila API menjawab 429 (lihat crate::rate_limit).
            let send = || async {
                self.client
                    .post(api("/chat.postMessage"))
                    .bearer_auth(&self.bot_token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| HermesError::Gateway(format!("slack send gagal: {e}")))
            };
            let resp = send().await?;
            // 429 → tidur sesuai Retry-After (cap 60 dtk), retry sekali.
            let resp = crate::rate_limit::handle_429("slack", resp, send).await?;
            let v: Value = resp
                .json()
                .await
                .map_err(|e| HermesError::Gateway(format!("slack parse gagal: {e}")))?;
            if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
                return Err(HermesError::Gateway(format!(
                    "slack menolak pesan: {}",
                    v.get("error").and_then(|e| e.as_str()).unwrap_or("?")
                )));
            }
        }
        Ok(())
    }
}

/// Buka koneksi Socket Mode dan kembalikan URL websocket.
async fn open_socket(app_token: &str) -> Result<String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(api("/apps.connections.open"))
        .bearer_auth(app_token)
        .send()
        .await
        .map_err(|e| HermesError::Gateway(format!("apps.connections.open gagal: {e}")))?;
    let v: Value = resp
        .json()
        .await
        .map_err(|e| HermesError::Gateway(format!("parse gagal: {e}")))?;
    if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
        return Err(HermesError::Gateway(format!(
            "socket mode ditolak: {}",
            v.get("error").and_then(|e| e.as_str()).unwrap_or("?")
        )));
    }
    v.get("url")
        .and_then(|u| u.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| HermesError::Gateway("tidak ada ws url dari slack".into()))
}

/// Daftar ID pengguna Slack yang diizinkan: gabungan (union)
/// `SLACK_ALLOWED_USERS` dan `GATEWAY_ALLOWED_USERS` (dipisah koma;
/// `"*"` = semua pengguna). Mengembalikan `None` bila keduanya kosong →
/// semua pengguna ditolak (fail-closed).
pub fn slack_allowlist() -> Option<std::collections::HashSet<String>> {
    let mut ids = read_allow_csv("SLACK_ALLOWED_USERS");
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Saklar "izinkan semua": `SLACK_ALLOW_ALL_USERS` (khusus dev) atau
/// `GATEWAY_ALLOW_ALL_USERS` bernilai true/1/yes.
pub fn slack_allow_all() -> bool {
    env_truthy("SLACK_ALLOW_ALL_USERS") || env_truthy("GATEWAY_ALLOW_ALL_USERS")
}

/// Keputusan izin murni (mudah diunit-tes): `user_id` adalah Slack user ID
/// (mis. `U012ABC`). Urutan: allow-all dulu, lalu allowlist, default tolak.
pub fn is_slack_user_allowed(
    user_id: &str,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> bool {
    if slack_allow_all() {
        return true;
    }
    match allowlist {
        None => false,
        Some(set) => set.contains("*") || set.contains(user_id),
    }
}

/// Cache dedup envelope masuk Socket Mode: bounded, FIFO eviction.
///
/// Slack mengirim ulang envelope yang belum di-ack setelah reconnect;
/// envelope/event yang sudah diproses harus dilewati agar pesan tidak
/// dijawab dua kali (cermin `MessageDedup` di discord.rs).
struct EnvelopeDedup {
    ids: std::collections::HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl EnvelopeDedup {
    fn new(capacity: usize) -> Self {
        Self {
            ids: std::collections::HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Mengembalikan `true` bila `id` sudah pernah terlihat (duplikat →
    /// buang). ID kosong tidak pernah di-dedup.
    fn is_duplicate(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if self.ids.contains(id) {
            return true;
        }
        if self.ids.len() >= self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        self.ids.insert(id.to_string());
        self.order.push_back(id.to_string());
        false
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.ids.len()
    }
}

/// Backoff murni (deterministik, untuk pengujian): 2^attempt detik,
/// dibatasi [`RECONNECT_BACKOFF_CAP_SECS`]. Tidak pernah overflow.
fn backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt).min(RECONNECT_BACKOFF_CAP_SECS)
}

/// Delay aktual = backoff × (0.75 + 0.5·rand01), tidak pernah melebihi
/// [`RECONNECT_BACKOFF_CAP_SECS`]. `rand01` harus dalam [0, 1).
fn backoff_delay(attempt: u32, rand01: f64) -> Duration {
    let base = backoff_secs(attempt) as f64;
    let r = rand01.clamp(0.0, 1.0);
    let secs = (base * (0.75 + 0.5 * r)).min(RECONNECT_BACKOFF_CAP_SECS as f64);
    Duration::from_secs_f64(secs.max(0.25))
}

/// Sumber jitter ringan tanpa dependensi baru: sub-detik jam sistem → [0, 1).
fn jitter01() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos % 1000) as f64 / 1000.0
}

/// Loop Socket Mode dengan supervisi: terima event message → handler →
/// kirim balasan ke thread.
///
/// Setiap sesi koneksi yang putus (termasuk `apps.connections.open` yang
/// gagal) dihubungkan ulang dengan backoff eksponensial berjitter tanpa
/// batas — cermin supervisi gateway Discord ([`serve_discord`](crate::discord::serve_discord)).
/// Dedup bertahan lintas reconnect: replay envelope setelah reconnect
/// dilewati agar pesan tidak dijawab dua kali.
pub async fn serve_slack<F, Fut>(bot_token: String, app_token: String, mut handler: F) -> Result<()>
where
    F: FnMut(SlackMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let allowlist = slack_allowlist();
    if slack_allow_all() {
        tracing::warn!(
            "allowlist slack: DINONAKTIFKAN via *_ALLOW_ALL_USERS (semua pengguna diizinkan)"
        );
    }
    match &allowlist {
        Some(set) if set.contains("*") => {
            tracing::info!("allowlist slack: semua pengguna diizinkan (*)")
        }
        Some(set) => tracing::info!("allowlist slack: {} pengguna diizinkan", set.len()),
        None => tracing::warn!(
            "SLACK_ALLOWED_USERS tidak diset: semua pesan masuk akan ditolak (fail-closed)"
        ),
    }
    // Dedup bertahan lintas reconnect: replay setelah reconnect dilewati.
    let mut dedup = EnvelopeDedup::new(DEDUP_CAPACITY);
    let mut attempt: u32 = 0;
    loop {
        // Supervisi: setiap sesi koneksi yang putus dihubungkan ulang dengan
        // backoff eksponensial berjitter, tanpa batas (indefinite).
        let up_since = tokio::time::Instant::now();
        match run_socket_session(&bot_token, &app_token, &mut handler, &allowlist, &mut dedup).await
        {
            Ok(()) => return Ok(()), // sesi berakhir normal (saat ini tidak pernah terjadi)
            Err(e) => {
                if up_since.elapsed() >= Duration::from_secs(BACKOFF_RESET_AFTER_SECS) {
                    attempt = 0; // koneksi sempat sehat → reset hitungan backoff
                }
                let delay = backoff_delay(attempt, jitter01());
                attempt = attempt.saturating_add(1);
                tracing::warn!(
                    "socket mode slack terputus ({e}); reconnect dalam {delay:?} (upaya #{attempt})"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Satu sesi Socket Mode: `apps.connections.open` → connect → baca envelope.
/// Mengembalikan `Err` pada setiap kegagalan koneksi agar [`serve_slack`]
/// bisa menghubungkan ulang; state dedup dipertahankan oleh pemanggil
/// lintas sesi.
async fn run_socket_session<F, Fut>(
    bot_token: &str,
    app_token: &str,
    handler: &mut F,
    allowlist: &Option<std::collections::HashSet<String>>,
    dedup: &mut EnvelopeDedup,
) -> Result<()>
where
    F: FnMut(SlackMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let url = open_socket(app_token).await?;
    let (ws, _) = connect_async(&url)
        .await
        .map_err(|e| HermesError::Gateway(format!("slack ws connect gagal: {e}")))?;
    let (mut write, mut read) = ws.split();
    tracing::info!("gateway slack terhubung (socket mode)");
    let platform = SlackPlatform::new(bot_token);

    while let Some(msg) = read.next().await {
        let msg = msg.map_err(|e| HermesError::Gateway(format!("ws error: {e}")))?;
        let text = match &msg {
            WsMessage::Text(t) => t.to_string(),
            WsMessage::Close(_) => return Err(HermesError::Gateway("slack ws terputus".into())),
            _ => continue,
        };
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let envelope_id = v.get("envelope_id").and_then(|e| e.as_str()).unwrap_or("");
        // Ack setiap envelope yang ber-ID (seperti semula).
        if !envelope_id.is_empty() {
            let ack = json!({ "envelope_id": envelope_id });
            if write
                .send(WsMessage::Text(ack.to_string().into()))
                .await
                .is_err()
            {
                return Err(HermesError::Gateway("ack gagal".into()));
            }
        }
        let ev_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ev_type == "hello" {
            continue;
        }
        if ev_type != "events_api" {
            continue;
        }
        let payload = v.get("payload").unwrap_or(&Value::Null);
        let event = payload.get("event").unwrap_or(&Value::Null);
        if event.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        // Abaikan pesan bot / edit / join.
        if event.get("bot_id").is_some() || event.get("subtype").is_some() {
            continue;
        }
        let text = event
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if text.trim().is_empty() {
            continue;
        }
        let sm = SlackMessage {
            channel: event
                .get("channel")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            user: event
                .get("user")
                .and_then(|u| u.as_str())
                .unwrap_or("?")
                .to_string(),
            text,
            ts: event
                .get("ts")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
        };
        // Replay setelah reconnect: envelope/event yang sudah diproses
        // dilewati agar pesan tidak dijawab dua kali. Kedua kunci dicatat
        // (keduanya selalu dievaluasi): envelope_id — unik per pengiriman
        // Socket Mode — dan channel:ts — unik per pesan, menangkap kirim-ulang
        // dengan envelope_id baru setelah reconnect.
        // Envelope sudah di-ack di atas, jadi pesan yang dilewati di sini
        // tetap ter-ack (tidak diproses ulang).
        let env_dup = !envelope_id.is_empty() && dedup.is_duplicate(&format!("env:{envelope_id}"));
        let msg_dup = !sm.channel.is_empty()
            && !sm.ts.is_empty()
            && dedup.is_duplicate(&format!("msg:{}:{}", sm.channel, sm.ts));
        if env_dup || msg_dup {
            tracing::debug!(
                envelope_id = %envelope_id,
                channel = %sm.channel,
                "event slack duplikat, dilewati"
            );
            continue;
        }
        if !is_slack_user_allowed(&sm.user, allowlist) {
            tracing::warn!(user = %sm.user, "pesan slack dari pengguna tak diizinkan, dilewati");
            continue;
        }
        tracing::info!(channel = %sm.channel, user = %sm.user, "pesan slack masuk");
        let target = format!("{}:{}", sm.channel, sm.ts);
        match handler(sm).await {
            Ok(reply) => {
                if let Err(e) = platform.send_message(&target, &reply).await {
                    tracing::warn!("gagal membalas slack: {e}");
                }
            }
            Err(e) => {
                let _ = platform
                    .send_message(&target, &format!("Maaf, terjadi error: {e}"))
                    .await;
            }
        }
    }
    Ok(())
}

/// Simpan `value` di balik token placeholder yang kebal pass berikutnya.
fn stash_mrkdwn(ph: &mut Vec<(String, String)>, value: String) -> String {
    let key = format!("\u{0}SL{}\u{0}", ph.len());
    ph.push((key.clone(), value));
    key
}

/// Bungkus tabel pipe GFM dalam pagar ``` (mrkdwn Slack menampilkan pipe
/// tabel sebagai pipe literal). Cermin `_wrap_markdown_tables` aslinya
/// (versi ringkas: tanpa perataan kolom).
fn wrap_markdown_tables_slack(text: &str) -> String {
    if !text.contains('|') || !text.contains('-') {
        return text.to_string();
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            i += 1;
            continue;
        }
        if !in_fence
            && line.contains('|')
            && i + 1 < lines.len()
            && is_table_sep(line, lines[i + 1])
        {
            let mut j = i + 2;
            while j < lines.len() && lines[j].trim().contains('|') {
                j += 1;
            }
            let mut block = String::from("```\n");
            block.push_str(&lines[i..j].join("\n"));
            block.push_str("\n```");
            out.push(block);
            i = j;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    out.join("\n")
}

/// Detektor baris pemisah tabel GFM (duplikat kecil dari telegram.rs agar
/// modul tetap mandiri).
fn is_table_sep(_header: &str, sep: &str) -> bool {
    let t = sep.trim().trim_matches('|').trim();
    if t.is_empty() || !t.contains('-') {
        return false;
    }
    t.split('|').all(|cell| {
        let c = cell.trim();
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        !c.is_empty() && c.chars().all(|ch| ch == '-')
    })
}

/// Konversi markdown standar menjadi mrkdwn Slack.
///
/// Cermin `format_message()` di `plugins/platforms/slack/adapter.py`
/// aslinya: tabel dibungkus pagar dulu; kode dilindungi placeholder;
/// broadcast mention di-escape agar tak me-ping @channel; entitas HTML
/// di-unescape lalu di-escape ulang dalam satu pass; lalu konversi emphasis.
pub fn format_slack_mrkdwn(content: &str) -> String {
    use regex::Regex;
    if content.is_empty() {
        return content.to_string();
    }
    let mut ph: Vec<(String, String)> = Vec::new();
    // Tabel pipe → pagar kode (mrkdwn tak merender tabel).
    let mut text = wrap_markdown_tables_slack(content);
    // Broadcast mention <!everyone>/<!channel>/<!here> — escape '<' di depan
    // agar bot tak bisa me-ping massal.
    let special_re = Regex::new(r"(?i)<!(?:everyone|channel|here)(?:\|[^>\n]*)?>").unwrap();
    text = special_re
        .replace_all(&text, |caps: &regex::Captures| {
            caps[0].replacen('<', "&lt;", 1)
        })
        .into_owned();
    // Lindungi pagar kode; tag bahasa dihapus untuk pagar di awal baris
    // (Slack merendernya harfiah).
    let fence_re = Regex::new(r"```(?:[^\n]*\n)?[\s\S]*?```").unwrap();
    text = fence_re
        .replace_all(&text, |caps: &regex::Captures| {
            let block = &caps[0];
            let at_line_start = caps
                .get(0)
                .map(|m| m.start() == 0 || text.as_bytes()[m.start() - 1] == b'\n')
                .unwrap_or(false);
            let mut b = block.to_string();
            if at_line_start {
                let lang_re = Regex::new(r"\A```[^\s`]+[ \t]*(\r?\n)").unwrap();
                b = lang_re.replace(&b, "```$1").into_owned();
            }
            stash_mrkdwn(&mut ph, b)
        })
        .into_owned();
    // Lindungi inline code.
    let code_re = Regex::new(r"`[^`]+`").unwrap();
    text = code_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_mrkdwn(&mut ph, caps[0].to_string())
        })
        .into_owned();
    // Link markdown → <url|teks> (bukan gambar: "![" dilewati — diperiksa
    // manual karena crate regex tak mendukung look-behind).
    let link_re = Regex::new(r"\[([^\]]+)\]\(([^()]*(?:\([^()]*\)[^()]*)*)\)").unwrap();
    text = {
        let src = text;
        let mut res = String::with_capacity(src.len());
        let mut last = 0;
        for caps in link_re.captures_iter(&src) {
            let m = caps.get(0).unwrap();
            let prev_bang = src[..m.start()].ends_with('!');
            if prev_bang {
                continue;
            }
            let mut url = caps[2].trim().to_string();
            if url.starts_with('<') && url.ends_with('>') && url.len() >= 2 {
                url = url[1..url.len() - 1].trim().to_string();
            }
            res.push_str(&src[last..m.start()]);
            res.push_str(&stash_mrkdwn(&mut ph, format!("<{}|{}>", url, &caps[1])));
            last = m.end();
        }
        res.push_str(&src[last..]);
        res
    };
    // Lindungi entitas mrkdwn yang sudah ada (<@U...>, <#C...|...>, <http...>).
    let entity_re = Regex::new(r"<(?:[@#!]|(?:https?|mailto|tel):)[^>\n]+>").unwrap();
    text = entity_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_mrkdwn(&mut ph, caps[0].to_string())
        })
        .into_owned();
    // Lindungi awalan blockquote "> ".
    let quote_re = Regex::new(r"(?m)^(>+\s)").unwrap();
    text = quote_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_mrkdwn(&mut ph, caps[0].to_string())
        })
        .into_owned();
    // Unescape entitas HTML dalam SATU pass (replace berurutan akan
    // men-decode "&amp;lt;" dua kali), lalu escape ulang.
    let ent_re = Regex::new(r"&(amp|lt|gt);").unwrap();
    text = ent_re
        .replace_all(&text, |caps: &regex::Captures| match &caps[1] {
            "amp" => "&",
            "lt" => "<",
            _ => ">", // hanya "gt" yang tersisa
        })
        .into_owned();
    text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    // Header → *tebal* (kupas ** redundan di dalamnya).
    let header_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    let bold_in = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    text = header_re
        .replace_all(&text, |caps: &regex::Captures| {
            let inner = bold_in.replace_all(&caps[1], "$1");
            stash_mrkdwn(&mut ph, format!("*{}*", inner.trim()))
        })
        .into_owned();
    // ***x*** → *_x_*.
    let bi_re = Regex::new(r"\*\*\*(.+?)\*\*\*").unwrap();
    text = bi_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_mrkdwn(&mut ph, format!("*_{}_*", &caps[1]))
        })
        .into_owned();
    // **x** → *x* (Slack memenggal pesan bila '*' penutup mengikuti
    // karakter non-word: sisipkan U+200B sebelumnya).
    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    text = bold_re
        .replace_all(&text, |caps: &regex::Captures| {
            let inner = &caps[1];
            let last = inner.chars().last().unwrap_or(' ');
            let zw = if last.is_alphanumeric() || last == '_' {
                ""
            } else {
                "\u{200b}"
            };
            stash_mrkdwn(&mut ph, format!("*{inner}{zw}*"))
        })
        .into_owned();
    // *x* → _x_ hanya bila non-whitespace menempel di kedua pembatas
    // ("a * b * c" tidak diubah); '*' di sisi pembatas juga menolak
    // (pola asli memakai look-around yang tak didukung crate regex).
    let italic_re = Regex::new(r"\*(\S(?:[^*\n]*?\S)?)\*").unwrap();
    text = {
        let src = text;
        let mut res = String::with_capacity(src.len());
        let mut last = 0;
        for caps in italic_re.captures_iter(&src) {
            let m = caps.get(0).unwrap();
            let prev_star = src[..m.start()].ends_with('*');
            let next_star = src[m.end()..].starts_with('*');
            if prev_star || next_star {
                continue;
            }
            res.push_str(&src[last..m.start()]);
            res.push_str(&stash_mrkdwn(&mut ph, format!("_{}_", &caps[1])));
            last = m.end();
        }
        res.push_str(&src[last..]);
        res
    };
    // ~~x~~ → ~x~.
    let strike_re = Regex::new(r"~~(.+?)~~").unwrap();
    text = strike_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_mrkdwn(&mut ph, format!("~{}~", &caps[1]))
        })
        .into_owned();
    // Kembalikan placeholder dalam urutan terbalik.
    for (key, value) in ph.iter().rev() {
        text = text.replace(key, value);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{clear_auth_env, restore_auth_env, set_var, ENV_LOCK};

    const AUTH_VARS: [&str; 4] = [
        "SLACK_ALLOWED_USERS",
        "SLACK_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    #[test]
    fn allowlist_kosong_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        assert!(slack_allowlist().is_none());
        assert!(!slack_allow_all());
        assert!(!is_slack_user_allowed("U012ABC", &slack_allowlist()));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_parse_koma_dan_spasi() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SLACK_ALLOWED_USERS", Some("  U111 , U222,,"));
        let allow = Some(slack_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_slack_user_allowed("U111", &allow));
        assert!(is_slack_user_allowed("U222", &allow));
        assert!(!is_slack_user_allowed("U333", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_wildcard_buka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SLACK_ALLOWED_USERS", Some("*"));
        let allow = Some(slack_allowlist().expect("harus ter-parse"));
        assert!(is_slack_user_allowed("U999XYZ", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_id_tak_dikenal_ditolak() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SLACK_ALLOWED_USERS", Some("U111"));
        let allow = Some(slack_allowlist().expect("harus ter-parse"));
        assert!(!is_slack_user_allowed("U999", &allow));
        assert!(!is_slack_user_allowed("", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_gateway_users_gabungan() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SLACK_ALLOWED_USERS", Some("U111"));
        set_var("GATEWAY_ALLOWED_USERS", Some("U222"));
        let allow = Some(slack_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_slack_user_allowed("U111", &allow));
        assert!(is_slack_user_allowed("U222", &allow));
        assert!(!is_slack_user_allowed("U333", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SLACK_ALLOW_ALL_USERS", Some("1"));
        assert!(slack_allow_all());
        assert!(is_slack_user_allowed("siapa-saja", &None));
        restore_auth_env(saved);
    }

    // --- Supervisi Socket Mode (audit W4-B-S2) ---

    #[test]
    fn dedup_envelope_id_duplikat_dibuang() {
        // Kunci yang dipakai sesi: "env:<envelope_id>" / "msg:<channel>:<ts>".
        let mut d = EnvelopeDedup::new(1000);
        assert!(!d.is_duplicate("env:abc-123")); // baru → diproses
        assert!(d.is_duplicate("env:abc-123")); // duplikat → buang
        assert!(!d.is_duplicate("msg:C999:1234.5678")); // pesan lain → diproses
        assert!(d.is_duplicate("msg:C999:1234.5678")); // duplikat → buang
        assert!(!d.is_duplicate("env:def-456")); // envelope lain → diproses
        assert_eq!(d.len(), 3);
    }

    #[test]
    fn dedup_id_kosong_tidak_pernah_didedup() {
        let mut d = EnvelopeDedup::new(10);
        assert!(!d.is_duplicate(""));
        assert!(!d.is_duplicate(""));
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn dedup_eviksi_fifo_saat_penuh() {
        let mut d = EnvelopeDedup::new(2);
        assert!(!d.is_duplicate("env:a"));
        assert!(!d.is_duplicate("env:b"));
        assert_eq!(d.len(), 2);
        // "env:c" mengusir "env:a" (yang paling lama, FIFO).
        assert!(!d.is_duplicate("env:c"));
        assert_eq!(d.len(), 2);
        assert!(d.is_duplicate("env:b"));
        assert!(d.is_duplicate("env:c"));
        assert!(!d.is_duplicate("env:a")); // ter-eviksi → dianggap baru lagi
    }

    #[test]
    fn backoff_tumbuh_eksponensial_dibatasi_60s() {
        assert_eq!(backoff_secs(0), 1);
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(2), 4);
        assert_eq!(backoff_secs(5), 32);
        assert_eq!(backoff_secs(6), 60);
        assert_eq!(backoff_secs(7), 60);
        assert_eq!(backoff_secs(30), 60);
        // Tidak overflow pada attempt ekstrem.
        assert_eq!(backoff_secs(u32::MAX), 60);
    }

    #[test]
    fn backoff_delay_jitter_terbatas_cap() {
        // rand01=0 → 75% dari base; rand01→1 → 125% dari base.
        assert_eq!(
            backoff_delay(0, 0.0),
            Duration::from_secs_f64(0.75),
            "base 1 dtk × 0.75"
        );
        assert!(
            backoff_delay(0, 0.999) <= Duration::from_secs_f64(1.25),
            "tidak melebihi 125% base"
        );
        assert!(
            backoff_delay(0, 0.999) >= Duration::from_secs_f64(0.75),
            "tidak di bawah 75% base"
        );
        // Pada cap: jitter tidak pernah mendorong melewati 60 dtk.
        let d = backoff_delay(20, 0.99);
        assert!(d <= Duration::from_secs(60), "cap 60 dtk: {d:?}");
        assert!(
            d >= Duration::from_secs_f64(45.0),
            "jitter bawah 75%: {d:?}"
        );
        // rand01 di luar [0,1] di-clamp, bukan panic/NaN.
        assert_eq!(backoff_delay(0, 5.0), backoff_delay(0, 1.0));
        assert_eq!(backoff_delay(0, -2.0), backoff_delay(0, 0.0));
    }

    // --- Format mrkdwn (audit batch 8) ---

    #[test]
    fn mrkdwn_bold_italic_strike_header() {
        assert_eq!(format_slack_mrkdwn("**tebal**"), "*tebal*");
        assert_eq!(format_slack_mrkdwn("*miring*"), "_miring_");
        assert_eq!(format_slack_mrkdwn("~~coret~~"), "~coret~");
        assert_eq!(format_slack_mrkdwn("***keduanya***"), "*_keduanya_*");
        assert_eq!(format_slack_mrkdwn("# Judul"), "*Judul*");
        assert_eq!(format_slack_mrkdwn("## **Sub**"), "*Sub*");
        // "a * b * c" (spasi menempel pembatas) tidak diubah.
        assert_eq!(format_slack_mrkdwn("a * b * c"), "a * b * c");
    }

    #[test]
    fn mrkdwn_bold_akhir_nonword_dapat_zwsp() {
        // Cermin aslinya: '*' penutup setelah karakter non-word disisipi
        // U+200B agar Slack tak memenggal pesan.
        let out = format_slack_mrkdwn("**tebal!**");
        assert_eq!(out, "*tebal!\u{200b}*", "dapat: {out:?}");
        let out2 = format_slack_mrkdwn("**tebal**");
        assert_eq!(out2, "*tebal*");
    }

    #[test]
    fn mrkdwn_link_dan_kode_dilindungi() {
        assert_eq!(
            format_slack_mrkdwn("[klik](https://contoh.id/x)"),
            "<https://contoh.id/x|klik>"
        );
        // Kode tak tersentuh konversi emphasis.
        let out = format_slack_mrkdwn("pakai `**bukan-bold**` ya");
        assert!(out.contains("`**bukan-bold**`"), "kode utuh: {out}");
        let fence = format_slack_mrkdwn("```rust\n**x**\n```");
        // Tag bahasa dihapus untuk pagar di awal baris (Slack merendernya
        // harfiah); isi pagar utuh.
        assert!(fence.starts_with("```\n"), "dapat: {fence:?}");
        assert!(fence.contains("**x**"), "isi pagar utuh: {fence:?}");
    }

    #[test]
    fn mrkdwn_broadcast_mention_di_escape() {
        let out = format_slack_mrkdwn("halo <!everyone> dan <!channel|ch>");
        assert!(!out.contains("<!everyone>"), "dapat: {out}");
        assert!(out.contains("&lt;!everyone&gt;"), "dapat: {out}");
    }

    #[test]
    fn mrkdwn_tabel_dibungkus_pagar() {
        let tabel = "| A | B |\n|---|---|\n| 1 | 2 |";
        let out = format_slack_mrkdwn(tabel);
        assert!(out.contains("```\n| A | B |"), "dapat: {out:?}");
        assert!(out.ends_with("```"), "dapat: {out:?}");
    }

    #[test]
    fn mrkdwn_entitas_html_di_escape() {
        assert_eq!(
            format_slack_mrkdwn("a & b < c > d"),
            "a &amp; b &lt; c &gt; d"
        );
        // "&amp;lt;" hanya di-decode sekali: tak boleh menjadi "<" (titik
        // tetap — tidak double-decode).
        assert_eq!(format_slack_mrkdwn("&amp;lt;"), "&amp;lt;");
    }
}
