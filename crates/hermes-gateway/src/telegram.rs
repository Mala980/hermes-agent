use async_trait::async_trait;
use hermes_core::{HermesError, Platform, Result};
use serde_json::Value;
use std::future::Future;

use crate::allowlist::{env_truthy, read_allow_csv};
use crate::split::TELEGRAM_MESSAGE_LIMIT;

/// Platform Telegram via Bot API (long-polling).
pub struct TelegramPlatform {
    token: String,
    client: reqwest::Client,
}

impl TelegramPlatform {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }

    fn api(&self, method: &str) -> String {
        // TELEGRAM_API_BASE memungkinkan pengujian terhadap mock server.
        let base = std::env::var("TELEGRAM_API_BASE")
            .unwrap_or_else(|_| "https://api.telegram.org".to_string());
        format!(
            "{}/bot{}/{}",
            base.trim_end_matches('/'),
            self.token,
            method
        )
    }

    /// Kirim voice note (ogg/opus) ke chat. Audio harus OGG Opus untuk tampil
    /// sebagai voice note; format lain tetap terkirim sebagai audio biasa.
    pub async fn send_voice(&self, chat_id: i64, audio: &[u8], filename: &str) -> Result<()> {
        let send = || async {
            let part = reqwest::multipart::Part::bytes(audio.to_vec())
                .file_name(filename.to_string())
                .mime_str("audio/ogg")
                .map_err(|e| HermesError::Gateway(format!("mime invalid: {e}")))?;
            // multipart::Form dikonsumsi oleh send(); bangun ulang tiap percobaan
            // agar retry 429 mengirim body yang identik.
            let form = reqwest::multipart::Form::new()
                .text("chat_id", chat_id.to_string())
                .part("voice", part);
            self.client
                .post(self.api("sendVoice"))
                .multipart(form)
                .send()
                .await
                .map_err(|e| HermesError::Gateway(format!("sendVoice gagal: {e}")))
        };
        let resp = send().await?;
        // 429 → tidur sesuai Retry-After/retry_after (cap 60 dtk), retry sekali.
        let resp = crate::rate_limit::handle_429("telegram", resp, send).await?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HermesError::Gateway(format!("parse sendVoice gagal: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            return Err(HermesError::Gateway(format!("telegram menolak voice: {v}")));
        }
        Ok(())
    }

    /// Akui callback_query (tombol inline) via `answerCallbackQuery` agar
    /// indikator "loading" di klien Telegram berhenti. Tidak mengirim teks
    /// balasan dan tidak merutekan `data` ke mana pun.
    pub async fn answer_callback_query(&self, callback_query_id: &str) -> Result<()> {
        let send = || async {
            self.client
                .post(self.api("answerCallbackQuery"))
                .json(&serde_json::json!({
                    "callback_query_id": callback_query_id,
                }))
                .send()
                .await
                .map_err(|e| HermesError::Gateway(format!("answerCallbackQuery gagal: {e}")))
        };
        let resp = send().await?;
        // 429 → tidur sesuai Retry-After/retry_after (cap 60 dtk), retry sekali.
        let resp = crate::rate_limit::handle_429("telegram", resp, send).await?;
        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(HermesError::Gateway(format!(
                "telegram menolak answerCallbackQuery: {body}"
            )));
        }
        // Telegram mengembalikan HTTP 200 dengan {"ok": false} untuk error aplikasi.
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HermesError::Gateway(format!("parse respons gagal: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown");
            return Err(HermesError::Gateway(format!(
                "telegram menolak answerCallbackQuery: {desc}"
            )));
        }
        Ok(())
    }

    async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<TelegramUpdate>> {
        let resp = self
            .client
            .get(self.api("getUpdates"))
            .query(&[
                ("offset", offset.to_string()),
                ("timeout", timeout.to_string()),
            ])
            .send()
            .await
            .map_err(|e| HermesError::Gateway(format!("getUpdates gagal: {e}")))?;
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HermesError::Gateway(format!("parse updates gagal: {e}")))?;
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            return Err(HermesError::Gateway(format!("telegram api error: {v}")));
        }
        Ok(parse_updates(&v))
    }

    /// Kirim satu chunk (sudah diformat) ke Bot API; kembalikan `Ok(())` bila
    /// sukses atau `Err(description)` berisi deskripsi error Telegram.
    async fn post_chunk(&self, body: &serde_json::Value) -> std::result::Result<(), String> {
        // Bungkus sebagai closure agar request identik bisa dikirim ulang
        // bila API menjawab 429 (lihat crate::rate_limit).
        let send = || async {
            self.client
                .post(self.api("sendMessage"))
                .json(body)
                .send()
                .await
                .map_err(|e| HermesError::Gateway(format!("sendMessage gagal: {e}")))
        };
        let resp = send().await.map_err(|e| e.to_string())?;
        let resp = crate::rate_limit::handle_429("telegram", resp, send)
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(format!("telegram menolak pesan: {body_text}"));
        }
        // Telegram mengembalikan HTTP 200 dengan {"ok": false} untuk error aplikasi.
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("parse respons gagal: {e}"))?;
        if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown");
            return Err(desc.to_string());
        }
        Ok(())
    }

    /// Apakah deskripsi error Telegram menandakan parse MarkdownV2 gagal
    /// (cermin `_send_chunk_markdown_or_plain` aslinya: "parse"/"markdown").
    fn is_parse_error(desc: &str) -> bool {
        let lower = desc.to_lowercase();
        lower.contains("parse") || lower.contains("markdown")
    }
}

/// Parse mentah respons getUpdates menjadi daftar update.
///
/// SEMUA update yang dikembalikan API dicatat — termasuk yang non-teks
/// (callback_query, foto tanpa caption, channel_post, my_chat_member, dsb.)
/// — agar `serve_telegram` selalu memajukan offset melewati mereka. Tanpa
/// ini, satu update non-teks akan dikembalikan API selamanya (offset macet).
/// Hanya update berteks yang diteruskan ke handler (lihat `serve_telegram`).
///
/// `pub` agar alur update (parse → ack) dapat diuji dari integration test.
///
/// Update `callback_query` (tombol inline ditekan) dicatat dengan
/// `callback_query_id`/`callback_data` terisi; `serve_telegram` mengakuinya
/// via `answerCallbackQuery` agar spinner "loading" di klien berhenti —
/// tanpa merutekan `data` ke agen.
pub fn parse_updates(v: &Value) -> Vec<TelegramUpdate> {
    let mut out = Vec::new();
    for u in v
        .get("result")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let update_id = u.get("update_id").and_then(|i| i.as_i64()).unwrap_or(0);
        if let Some(cq) = u.get("callback_query") {
            let from_id = cq
                .get("from")
                .and_then(|f| f.get("id"))
                .and_then(|i| i.as_i64())
                .unwrap_or(0);
            let from = cq
                .get("from")
                .and_then(|f| f.get("username"))
                .and_then(|u| u.as_str())
                .unwrap_or("?")
                .to_string();
            out.push(TelegramUpdate {
                update_id,
                chat_id: 0,
                message_id: 0,
                from,
                from_id,
                text: String::new(),
                callback_query_id: cq.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()),
                callback_data: cq
                    .get("data")
                    .and_then(|d| d.as_str())
                    .map(|s| s.to_string()),
            });
            continue;
        }
        let msg = u.get("message").or_else(|| u.get("edited_message"));
        match msg {
            Some(m) => {
                let chat_id = m
                    .get("chat")
                    .and_then(|c| c.get("id"))
                    .and_then(|i| i.as_i64())
                    .unwrap_or(0);
                let text = m
                    .get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
                let from_id = m
                    .get("from")
                    .and_then(|f| f.get("id"))
                    .and_then(|i| i.as_i64())
                    .unwrap_or(0);
                let from = m
                    .get("from")
                    .and_then(|f| f.get("username"))
                    .and_then(|u| u.as_str())
                    .unwrap_or("?")
                    .to_string();
                let message_id = m.get("message_id").and_then(|i| i.as_i64()).unwrap_or(0);
                out.push(TelegramUpdate {
                    update_id,
                    chat_id,
                    message_id,
                    from,
                    from_id,
                    text,
                    callback_query_id: None,
                    callback_data: None,
                });
            }
            None => {
                // Update non-pesan (channel_post, foto tanpa caption, ...):
                // catat update_id-nya saja agar offset maju (ack).
                out.push(TelegramUpdate {
                    update_id,
                    chat_id: 0,
                    message_id: 0,
                    from: "?".to_string(),
                    from_id: 0,
                    text: String::new(),
                    callback_query_id: None,
                    callback_data: None,
                });
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct TelegramUpdate {
    pub update_id: i64,
    pub chat_id: i64,
    /// ID pesan pemicu (untuk `reply_parameters`; 0 bila tak ada).
    pub message_id: i64,
    pub from: String,
    pub from_id: i64,
    pub text: String,
    /// Terisi hanya untuk update `callback_query` (tombol inline ditekan).
    pub callback_query_id: Option<String>,
    /// Payload `data` dari tombol inline; hanya dicatat di log, tidak dirutekan.
    pub callback_data: Option<String>,
}

/// Akui satu update `callback_query`: panggil `answerCallbackQuery` agar
/// spinner "loading" di klien Telegram berhenti. `data` hanya dicatat di log
/// — tidak ada teks balasan, tidak ada pemanggilan tool. Bukan callback_query
/// (id `None`) → tidak melakukan apa-apa. Error API dicatat sebagai warn,
/// bukan crash.
///
/// `pub` agar jalur yang dipakai `serve_telegram` dapat diuji langsung.
pub async fn ack_callback_query(platform: &TelegramPlatform, update: &TelegramUpdate) {
    let Some(cq_id) = update.callback_query_id.as_deref() else {
        return;
    };
    match update.callback_data.as_deref() {
        Some(data) => tracing::info!(
            callback_query_id = %cq_id,
            from = %update.from,
            "callback_query diterima; data: {data}"
        ),
        None => tracing::info!(
            callback_query_id = %cq_id,
            from = %update.from,
            "callback_query diterima"
        ),
    }
    if let Err(e) = platform.answer_callback_query(cq_id).await {
        tracing::warn!(callback_query_id = %cq_id, "answerCallbackQuery gagal: {e}");
    }
}

#[async_trait]
impl Platform for TelegramPlatform {
    fn id(&self) -> &str {
        "telegram"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: chat id (atau "chat_id:message_id" untuk reply ke pesan
        // tertentu).
        let (chat_id, reply_to) = match target.split_once(':') {
            Some((c, m)) if !m.is_empty() => (c, Some(m)),
            _ => (target, None),
        };
        let mode = crate::threading::ReplyThreadMode::from_env("TELEGRAM_REPLY_TO_MODE");
        // Format dulu (MarkdownV2), lalu pecah: batas 4096 dihitung dalam
        // unit UTF-16 seperti aslinya (`truncate_message(format_message(...),
        // len_fn=utf16_len)`).
        let formatted = format_telegram_mdv2(text);
        for (i, chunk) in crate::split::split_message_with_len(
            &formatted,
            Some(TELEGRAM_MESSAGE_LIMIT),
            crate::split::utf16_len,
        )
        .iter()
        .enumerate()
        {
            let mut body = serde_json::json!({
                "chat_id": chat_id,
                "text": chunk,
                "parse_mode": "MarkdownV2",
            });
            // Anchor balasan mengikuti reply_to_mode: "first" (default)
            // hanya pada chunk pertama — cermin `_should_thread_reply`.
            if mode.applies(reply_to.is_some(), i) {
                body["reply_parameters"] =
                    serde_json::json!({ "message_id": reply_to.unwrap_or("") });
            }
            match self.post_chunk(&body).await {
                Ok(()) => {}
                Err(desc) if Self::is_parse_error(&desc) => {
                    // MarkdownV2 ditolak → kirim ulang sebagai teks polos
                    // (cermin `_send_chunk_markdown_or_plain` aslinya).
                    tracing::warn!("telegram menolak MarkdownV2 ({desc}); kirim ulang polos");
                    let mut plain = body.clone();
                    plain["text"] = serde_json::json!(strip_mdv2(chunk));
                    plain
                        .as_object_mut()
                        .expect("body json object")
                        .remove("parse_mode");
                    self.post_chunk(&plain).await.map_err(|d| {
                        HermesError::Gateway(format!("telegram menolak pesan: {d}"))
                    })?;
                }
                Err(desc) => {
                    return Err(HermesError::Gateway(format!(
                        "telegram menolak pesan: {desc}"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Daftar ID pengguna Telegram yang diizinkan: gabungan (union)
/// `TELEGRAM_ALLOWED_USERS` dan `GATEWAY_ALLOWED_USERS` (dipisah koma;
/// `"*"` = semua pengguna). Mengembalikan `None` bila keduanya kosong →
/// semua pengguna ditolak (fail-closed, mengikuti perilaku asli
/// hermes-agent: tanpa allowlist, orang asing tidak bisa memakai bot).
pub fn telegram_allowlist() -> Option<std::collections::HashSet<String>> {
    let mut ids = read_allow_csv("TELEGRAM_ALLOWED_USERS");
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Saklar "izinkan semua": `TELEGRAM_ALLOW_ALL_USERS` (khusus dev, seperti
/// aslinya) atau `GATEWAY_ALLOW_ALL_USERS` bernilai true/1/yes.
pub fn telegram_allow_all() -> bool {
    env_truthy("TELEGRAM_ALLOW_ALL_USERS") || env_truthy("GATEWAY_ALLOW_ALL_USERS")
}

/// Keputusan izin murni (mudah diunit-tes): `user_id` dalam bentuk desimal string.
/// Urutan mengikuti aslinya: allow-all dulu, lalu allowlist, default tolak.
pub fn is_telegram_user_allowed(
    user_id: &str,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> bool {
    if telegram_allow_all() {
        return true;
    }
    match allowlist {
        None => false,
        Some(set) => set.contains("*") || set.contains(user_id),
    }
}

pub async fn serve_telegram<F, Fut>(token: String, mut handler: F) -> Result<()>
where
    F: FnMut(i64, String, String) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let platform = TelegramPlatform::new(token);
    let mut offset: i64 = 0;
    let allowlist = telegram_allowlist();
    if telegram_allow_all() {
        tracing::warn!(
            "allowlist telegram: DINONAKTIFKAN via *_ALLOW_ALL_USERS (semua pengguna diizinkan)"
        );
    }
    match &allowlist {
        Some(set) if set.contains("*") => {
            tracing::info!("allowlist telegram: semua pengguna diizinkan (*)")
        }
        Some(set) => tracing::info!("allowlist telegram: {} pengguna diizinkan", set.len()),
        None => tracing::warn!(
            "TELEGRAM_ALLOWED_USERS tidak diset: semua pesan masuk akan ditolak (fail-closed)"
        ),
    }
    tracing::info!("gateway telegram berjalan (long-polling)");
    loop {
        let updates = match platform.get_updates(offset, 30).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!("getUpdates error: {e}; coba lagi...");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        for u in updates {
            offset = offset.max(u.update_id + 1);
            if u.text.is_empty() {
                // Update non-teks (callback_query, foto tanpa caption, dsb.):
                // offset sudah maju di atas (ack offset). callback_query butuh
                // ack eksplisit via answerCallbackQuery agar spinner "loading"
                // di klien berhenti; tidak diteruskan ke handler.
                ack_callback_query(&platform, &u).await;
                continue;
            }
            if !is_telegram_user_allowed(&u.from_id.to_string(), &allowlist) {
                tracing::warn!(user_id = u.from_id, from = %u.from, "pesan dari pengguna tak diizinkan, dilewati");
                continue;
            }
            tracing::info!(chat = u.chat_id, from = %u.from, "pesan masuk");
            // Balas sebagai reply ke pesan pemicu (reply_parameters pada
            // chunk pertama, mengikuti reply_to_mode default "first"
            // seperti aslinya).
            let target = if u.message_id != 0 {
                format!("{}:{}", u.chat_id, u.message_id)
            } else {
                u.chat_id.to_string()
            };
            match handler(u.chat_id, u.from.clone(), u.text.clone()).await {
                Ok(reply) => {
                    if let Err(e) = platform.send_message(&target, &reply).await {
                        tracing::warn!("gagal mengirim balasan: {e}");
                    }
                }
                Err(e) => {
                    let _ = platform
                        .send_message(&target, &format!("Maaf, terjadi error: {e}"))
                        .await;
                }
            }
        }
    }
}

/// Stub platform lain: terdaftar di registry agar `hermes gateway` memberi
/// pesan konfigurasi yang jelas, bukan gagal diam-diam.
pub struct UnconfiguredPlatform {
    id: String,
    setup_hint: String,
}

impl UnconfiguredPlatform {
    pub fn new(id: impl Into<String>, setup_hint: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            setup_hint: setup_hint.into(),
        }
    }
}

#[async_trait]
impl Platform for UnconfiguredPlatform {
    fn id(&self) -> &str {
        &self.id
    }

    async fn send_message(&self, _target: &str, _text: &str) -> Result<()> {
        Err(HermesError::Gateway(format!(
            "platform `{}` belum dikonfigurasi. {}",
            self.id, self.setup_hint
        )))
    }
}

/// Daftar platform yang didukung gateway.
pub fn supported_platforms() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "telegram",
            "Set TELEGRAM_BOT_TOKEN atau gateway.telegram.bot_token di config.",
        ),
        (
            "discord",
            "Set DISCORD_BOT_TOKEN (butuh intent MESSAGE_CONTENT).",
        ),
        (
            "slack",
            "Set SLACK_BOT_TOKEN (xoxb) dan SLACK_APP_TOKEN (xapp) + Socket Mode.",
        ),
        (
            "whatsapp",
            "Set WHATSAPP_TOKEN, WHATSAPP_PHONE_NUMBER_ID; terima via webhook --webhook-port.",
        ),
        (
            "matrix",
            "Set MATRIX_HOMESERVER, MATRIX_ACCESS_TOKEN, MATRIX_USER_ID.",
        ),
        ("signal", "Butuh signal-cli terinstal; set SIGNAL_SENDER."),
        (
            "teams",
            "Set TEAMS_APP_ID, TEAMS_APP_PASSWORD; terima via webhook --webhook-port.",
        ),
        (
            "email",
            "Set EMAIL_SMTP_HOST/EMAIL_IMAP_HOST + kredensial; polling IMAP.",
        ),
    ]
}

// ---------------------------------------------------------------------------
// Format MarkdownV2 (audit batch 8).
//
// Paritas dengan `format_message()` di `plugins/platforms/telegram/adapter.py`
// aslinya: markdown standar dikonversi ke sintaks MarkdownV2 Telegram; kode
// (pagar maupun inline) dilindungi di balik placeholder agar tak tersentuh
// pass berikutnya; tabel pipe ditulis ulang menjadi grup bullet (MarkdownV2
// tak punya sintaks tabel); sisanya di-escape.
//
// Pengiriman memakai `parse_mode: "MarkdownV2"`; bila Telegram menolak
// karena parse gagal, chunk dikirim ulang sebagai teks polos
// (`strip_mdv2()`) — cermin `_send_chunk_markdown_or_plain` aslinya.

/// Escape semua karakter spesial MarkdownV2 dengan backslash di depannya.
///
/// Cermin `_escape_mdv2` aslinya:
/// `_ * [ ] ( ) ~ ` > # + - = | { } . ! \` + backslash itu sendiri.
pub fn escape_mdv2(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if matches!(
            c,
            '_' | '*'
                | '['
                | ']'
                | '('
                | ')'
                | '~'
                | '`'
                | '>'
                | '#'
                | '+'
                | '-'
                | '='
                | '|'
                | '{'
                | '}'
                | '.'
                | '!'
                | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Kupas penanda MarkdownV2/escape menjadi teks polos — fallback bila
/// Telegram menolak parse MarkdownV2. Cermin `_strip_mdv2` aslinya
/// (`**bold**` dikupas SEBELUM `*bold*` ala MarkdownV2).
pub fn strip_mdv2(text: &str) -> String {
    use regex::Regex;
    // Hapus backslash escape di depan karakter spesial.
    let esc_re = Regex::new(r"\\([_*\[\]()~`>#\+=\-|{}.!\\])").unwrap();
    let mut out = esc_re.replace_all(text, "$1").into_owned();
    // Bold Markdown-style maupun MarkdownV2, lalu italic.
    let bold2 = Regex::new(r"\*\*([^*]+)\*\*").unwrap();
    out = bold2.replace_all(&out, "$1").into_owned();
    let bold1 = Regex::new(r"\*([^*]+)\*").unwrap();
    out = bold1.replace_all(&out, "$1").into_owned();
    // Italic MarkdownV2 `_x_` hanya bila tidak menempel word-char di kedua
    // sisi (agar snake_case selamat) — crate regex tak mendukung look-around,
    // jadi tetangga diperiksa manual.
    let italic_re = Regex::new(r"_([^_]+)_").unwrap();
    out = {
        let src = out;
        let mut res = String::with_capacity(src.len());
        let mut last = 0;
        for caps in italic_re.captures_iter(&src) {
            let m = caps.get(0).unwrap();
            let prev_word = src[..m.start()]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
            let next_word = src[m.end()..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
            if !prev_word && !next_word {
                res.push_str(&src[last..m.start()]);
                res.push_str(&caps[1]);
                last = m.end();
            }
        }
        res.push_str(&src[last..]);
        res
    };
    let strike = Regex::new(r"~([^~]+)~").unwrap();
    out = strike.replace_all(&out, "$1").into_owned();
    let spoiler = Regex::new(r"\|\|([^|]+)\|\|").unwrap();
    out = spoiler.replace_all(&out, "$1").into_owned();
    out
}

/// Simpan `value` di balik token placeholder yang kebal pass escape
/// berikutnya; kembalikan tokennya. Cermin `_ph()` aslinya.
fn stash(ph: &mut Vec<(String, String)>, value: String) -> String {
    let key = format!("\u{0}PH{}\u{0}", ph.len());
    ph.push((key.clone(), value));
    key
}

/// Pecah satu baris tabel GFM menjadi sel-sel yang di-trim (pipa luar
/// opsional).
fn split_table_row(line: &str) -> Vec<String> {
    let t = line.trim().trim_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

/// Detektor baris pemisah tabel GFM (`|---|:---:|...`).
fn is_table_separator(line: &str) -> bool {
    let t = line.trim().trim_matches('|').trim();
    if t.is_empty() {
        return false;
    }
    t.split('|').all(|cell| {
        let c = cell.trim();
        let c = c.strip_prefix(':').unwrap_or(c);
        let c = c.strip_suffix(':').unwrap_or(c);
        !c.is_empty() && c.chars().all(|ch| ch == '-')
    }) && t.contains('-')
}

/// Render satu blok tabel GFM menjadi heading tebal + grup bullet —
/// cermin `_render_table_block` di `gateway/platforms/helpers.py` aslinya.
fn render_table_block(block: &[&str]) -> String {
    let headers = split_table_row(block[0]);
    if headers.len() < 2 || block.len() < 3 {
        return block.join("\n");
    }
    let has_row_label = split_table_row(block[2]).len() == headers.len() + 1;
    let mut groups: Vec<String> = Vec::new();
    for (idx, row) in block.iter().skip(2).enumerate() {
        let cells = split_table_row(row);
        let (heading, data): (String, Vec<String>) = if has_row_label {
            let h = cells
                .first()
                .cloned()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("Row {}", idx + 1));
            (h, cells.into_iter().skip(1).collect())
        } else {
            let h = cells
                .iter()
                .find(|c| !c.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("Row {}", idx + 1));
            (h.clone(), cells)
        };
        let mut data = data;
        data.resize(headers.len(), String::new());
        let bullets: Vec<String> = headers
            .iter()
            .zip(data.iter())
            .filter(|(_, v)| has_row_label || **v != heading)
            .map(|(h, v)| format!("\u{2022} {h}: {v}"))
            .collect();
        groups.push(format!("**{heading}**\n{}", bullets.join("\n")));
    }
    groups.join("\n\n")
}

/// Tulis ulang tabel pipe GFM menjadi grup bullet (MarkdownV2 tak punya
/// sintaks tabel); kode berpagar dibiarkan. Cermin
/// `convert_table_to_bullets` di `gateway/platforms/helpers.py` aslinya
/// (versi ringkas).
fn convert_table_to_bullets(text: &str) -> String {
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
            && is_table_separator(lines[i + 1])
        {
            let mut j = i + 2;
            while j < lines.len() && lines[j].trim().contains('|') {
                j += 1;
            }
            out.push(render_table_block(&lines[i..j]));
            i = j;
            continue;
        }
        out.push(line.to_string());
        i += 1;
    }
    out.join("\n")
}

/// Lindungi blok kode berpagar di balik placeholder (isi `\` dan `` ` ``
/// di-escape sesuai spek MarkdownV2); kembalikan teks dengan placeholder.
///
/// Cermin langkah 1 `format_message()` aslinya (ditulis manual karena
/// crate `regex` tak mendukung lookahead yang dipakai pola aslinya).
fn stash_fences(text: &str, ph: &mut Vec<(String, String)>) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        // Run ``` pertama pada baris ini (pagar inline "```x```" ditolak:
        // sisa baris setelah pembuka harus bebas backtick — cermin aturan
        // aslinya bahwa pembuka harus run triple-backtick pertama di baris).
        let mut handled = false;
        if let Some(pos) = line.find("```") {
            let (prefix, rest) = line.split_at(pos);
            if !rest[3..].contains("```") {
                // Cari baris penutup tersendiri.
                let mut j = i + 1;
                while j < lines.len() && lines[j].trim() != "```" {
                    j += 1;
                }
                if j < lines.len() {
                    let body: Vec<String> = lines[i + 1..j]
                        .iter()
                        .map(|l| l.replace('\\', "\\\\").replace('`', "\\`"))
                        .collect();
                    let mut fenced = String::new();
                    fenced.push_str(rest); // "```lang" s.d. akhir baris
                    fenced.push('\n');
                    fenced.push_str(&body.join("\n"));
                    fenced.push('\n');
                    fenced.push_str(lines[j].trim_end());
                    let key = stash(ph, fenced);
                    out.push(format!("{prefix}{key}"));
                    i = j + 1;
                    handled = true;
                }
            }
        }
        if !handled {
            out.push(line.to_string());
            i += 1;
        }
    }
    out.join("\n")
}

/// Konversi markdown standar menjadi MarkdownV2 Telegram.
///
/// Cermin `format_message()` di `plugins/platforms/telegram/adapter.py`
/// aslinya (langkah 0–11). Penyederhanaan terdokumentasi: kutip expandable
/// `**>`/`||` diperlakukan sebagai blockquote biasa.
pub fn format_telegram_mdv2(content: &str) -> String {
    use regex::Regex;
    if content.is_empty() {
        return content.to_string();
    }
    let mut ph: Vec<(String, String)> = Vec::new();

    // 0) Tabel pipe → grup bullet.
    let mut text = convert_table_to_bullets(content);
    // 1) Lindungi pagar kode.
    text = stash_fences(&text, &mut ph);
    // 2) Lindungi inline code (`...`), escape `\` di dalamnya.
    let inline_re = Regex::new(r"`[^`]+`").unwrap();
    text = inline_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash(&mut ph, caps[0].replace('\\', "\\\\"))
        })
        .into_owned();
    // 3) Link markdown → [teks](url); teks di-escape, di URL hanya
    //    ')' dan '\' yang perlu di-escape (spek MarkdownV2).
    let link_re = Regex::new(r"\[([^\]]+)\]\(([^()]*(?:\([^()]*\)[^()]*)*)\)").unwrap();
    text = link_re
        .replace_all(&text, |caps: &regex::Captures| {
            let url = caps[2].replace('\\', "\\\\").replace(')', "\\)");
            stash(&mut ph, format!("[{}]({url})", escape_mdv2(&caps[1])))
        })
        .into_owned();
    // 4) Header → *tebal*, kupas ** redundan di dalamnya.
    let header_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    let bold_in = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    text = header_re
        .replace_all(&text, |caps: &regex::Captures| {
            let inner = bold_in.replace_all(&caps[1], "$1");
            stash(&mut ph, format!("*{}*", escape_mdv2(inner.trim())))
        })
        .into_owned();
    // 5) **tebal** → *tebal*; 6) *miring* → _miring_;
    //    7) ~~coret~~ → ~coret~; 8) ||spoiler|| dipertahankan.
    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    text = bold_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash(&mut ph, format!("*{}*", escape_mdv2(&caps[1])))
        })
        .into_owned();
    let italic_re = Regex::new(r"\*([^*\n]+)\*").unwrap();
    text = italic_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash(&mut ph, format!("_{}_", escape_mdv2(&caps[1])))
        })
        .into_owned();
    let strike_re = Regex::new(r"~~(.+?)~~").unwrap();
    text = strike_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash(&mut ph, format!("~{}~", escape_mdv2(&caps[1])))
        })
        .into_owned();
    let spoiler_re = Regex::new(r"\|\|(.+?)\|\|").unwrap();
    text = spoiler_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash(&mut ph, format!("||{}||", escape_mdv2(&caps[1])))
        })
        .into_owned();
    // 9) Blockquote: lindungi '>' di depan, escape sisanya.
    let quote_re = Regex::new(r"(?m)^((?:\*\*)?>{1,3}) (.+)$").unwrap();
    text = quote_re
        .replace_all(&text, |caps: &regex::Captures| {
            let (prefix, content) = (&caps[1], &caps[2]);
            let rendered = if prefix.starts_with("**") && content.ends_with("||") {
                format!("{prefix} {}||", escape_mdv2(&content[..content.len() - 2]))
            } else {
                format!("{prefix} {}", escape_mdv2(content))
            };
            stash(&mut ph, rendered)
        })
        .into_owned();
    // 10) Escape sisa karakter spesial pada teks biasa.
    text = escape_mdv2(&text);
    // 11) Kembalikan placeholder dalam urutan terbalik (placeholder
    //     bersarang ikut terurai).
    for (key, value) in ph.iter().rev() {
        text = text.replace(key, value);
    }
    // 12) Jaring pengaman: escape bare ( ) { } di luar span kode —
    //     cermin `_escape_bare_bracket` aslinya (versi ringkas: lewati yang
    //     sudah di-escape dan bagian link [teks](url)).
    text = escape_bare_brackets(&text);
    text
}

/// Escape `(`, `)`, `{`, `}` yang telanjang di luar span kode, kecuali yang
/// sudah di-escape atau menjadi bagian link `[teks](url)`.
fn escape_bare_brackets(text: &str) -> String {
    // Belah pada span kode agar isinya tak tersentuh.
    // Penanda placeholder memakai \x00 — tak pernah mengandung kurung.
    let mut rest = text;
    let mut out = String::with_capacity(text.len() + 8);
    while let Some(start) = rest.find("```") {
        // Cari juga inline code di segmen ini.
        let seg = &rest[..start];
        out.push_str(&escape_bare_brackets_plain(seg));
        // Temukan akhir pagar/inline dimulai di `start`.
        let after = &rest[start..];
        let end = if after.starts_with("```") {
            after
                .strip_prefix("```")
                .and_then(|rest| rest.find("```").map(|p| start + 3 + p + 3))
        } else {
            None
        };
        match end {
            Some(e) => {
                out.push_str(&rest[start..e]);
                rest = &rest[e..];
            }
            None => {
                // Tak ada penutup — anggap inline code sampai backtick berikut.
                match after[1..].find('`').map(|p| start + 1 + p + 1) {
                    Some(e) => {
                        out.push_str(&rest[start..e]);
                        rest = &rest[e..];
                    }
                    None => {
                        out.push_str(after);
                        rest = "";
                    }
                }
            }
        }
    }
    out.push_str(&escape_bare_brackets_plain(rest));
    out
}

/// Escape bare bracket pada teks biasa (tanpa span kode): lewati span
/// inline code `` `...` `` agar isinya tak tersentuh.
fn escape_bare_brackets_plain(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len() + 8);
    let mut rest = seg;
    while let Some(start) = rest.find('`') {
        out.push_str(&escape_brackets_in_text(&rest[..start]));
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(e) => {
                out.push('`');
                out.push_str(&after[..e]);
                out.push('`');
                rest = &after[e + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            }
        }
    }
    out.push_str(&escape_brackets_in_text(rest));
    out
}

/// Escape `(`, `)`, `{`, `}` yang telanjang pada teks tanpa kode apa pun.
fn escape_brackets_in_text(seg: &str) -> String {
    let chars: Vec<char> = seg.chars().collect();
    let mut out = String::with_capacity(seg.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '(' | ')' | '{' | '}') {
            let prev = if i > 0 { chars[i - 1] } else { '\0' };
            if prev == '\\' {
                out.push(c); // sudah di-escape
            } else if c == '(' && prev == ']' {
                out.push(c); // membuka URL link [teks](url)
            } else if c == ')' && closes_link(&chars[..i]) {
                out.push(c); // menutup URL link
            } else {
                out.push('\\');
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// Apakah `)` pada posisi akhir `before` menutup URL sebuah link
/// `[teks](...)` — cermin walk-back `_escape_bare_bracket` aslinya.
fn closes_link(before: &[char]) -> bool {
    let mut depth: i32 = 0;
    let mut j = before.len();
    let mut scanned = 0;
    while j > 0 && scanned < 2000 {
        j -= 1;
        scanned += 1;
        match before[j] {
            '(' => {
                depth -= 1;
                if depth < 0 {
                    return j > 0 && before[j - 1] == ']';
                }
            }
            ')' => depth += 1,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{clear_auth_env, restore_auth_env, set_var, ENV_LOCK};

    const AUTH_VARS: [&str; 4] = [
        "TELEGRAM_ALLOWED_USERS",
        "TELEGRAM_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    #[test]
    fn allowlist_kosong_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        assert!(telegram_allowlist().is_none());
        assert!(!telegram_allow_all());
        assert!(!is_telegram_user_allowed(
            "565903951",
            &telegram_allowlist()
        ));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_parse_koma_dan_spasi() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("TELEGRAM_ALLOWED_USERS", Some(" 565903951 , 12345,,"));
        let allow = Some(telegram_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_telegram_user_allowed("565903951", &allow));
        assert!(is_telegram_user_allowed("12345", &allow));
        assert!(!is_telegram_user_allowed("99999", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_wildcard_buka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("TELEGRAM_ALLOWED_USERS", Some("*"));
        let allow = Some(telegram_allowlist().expect("harus ter-parse"));
        assert!(is_telegram_user_allowed("siapa-saja-123", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_id_tak_dikenal_ditolak() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("TELEGRAM_ALLOWED_USERS", Some("565903951"));
        let allow = Some(telegram_allowlist().expect("harus ter-parse"));
        assert!(!is_telegram_user_allowed("0", &allow));
        assert!(!is_telegram_user_allowed("", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_gateway_users_gabungan() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("TELEGRAM_ALLOWED_USERS", Some("555"));
        set_var("GATEWAY_ALLOWED_USERS", Some("777"));
        let allow = Some(telegram_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_telegram_user_allowed("555", &allow));
        assert!(is_telegram_user_allowed("777", &allow));
        assert!(!is_telegram_user_allowed("888", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_telegram_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("TELEGRAM_ALLOW_ALL_USERS", Some("true"));
        assert!(telegram_allow_all());
        assert!(is_telegram_user_allowed("siapa-saja", &None));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_gateway_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("GATEWAY_ALLOW_ALL_USERS", Some("1"));
        assert!(telegram_allow_all());
        assert!(is_telegram_user_allowed("siapa-saja", &None));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_nilai_salah_tidak_membuka() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for v in ["false", "0", "", "no"] {
            let saved = clear_auth_env(&AUTH_VARS);
            set_var("TELEGRAM_ALLOW_ALL_USERS", Some(v));
            assert!(!telegram_allow_all(), "nilai: {v:?}");
            assert!(!is_telegram_user_allowed("x", &None), "nilai: {v:?}");
            restore_auth_env(saved);
        }
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("TELEGRAM_ALLOW_ALL_USERS", Some("YES"));
        assert!(telegram_allow_all());
        restore_auth_env(saved);
    }

    #[test]
    fn parse_updates_mencakup_non_teks_agar_offset_maju() {
        // Payload campuran: pesan teks, callback_query, dan pesan foto
        // (tanpa teks). Semuanya harus dicatat agar offset maju melewatinya.
        let v: Value = serde_json::json!({
            "ok": true,
            "result": [
                { "update_id": 10,
                  "message": { "message_id": 1,
                               "chat": { "id": 100 },
                               "from": { "id": 555, "username": "chewy" },
                               "text": "halo" } },
                { "update_id": 11,
                  "callback_query": { "id": "cq1",
                                      "from": { "id": 555 },
                                      "data": "klik" } },
                { "update_id": 12,
                  "message": { "message_id": 2,
                               "chat": { "id": 100 },
                               "from": { "id": 555 },
                               "photo": [ { "file_id": "abc" } ] } },
            ]
        });
        let updates = parse_updates(&v);
        assert_eq!(updates.len(), 3, "semua update harus dicatat");
        // Tiru logika offset serve_telegram: harus melewati update 12.
        let mut offset: i64 = 0;
        for u in &updates {
            offset = offset.max(u.update_id + 1);
        }
        assert_eq!(offset, 13);
        // Hanya update berteks yang sampai ke handler.
        let berteks: Vec<&TelegramUpdate> = updates.iter().filter(|u| !u.text.is_empty()).collect();
        assert_eq!(berteks.len(), 1);
        assert_eq!(berteks[0].text, "halo");
        assert_eq!(berteks[0].from_id, 555);
        assert_eq!(berteks[0].chat_id, 100);
        // Update callback_query mencatat id + data untuk ack answerCallbackQuery.
        let cq = &updates[1];
        assert_eq!(cq.callback_query_id.as_deref(), Some("cq1"));
        assert_eq!(cq.callback_data.as_deref(), Some("klik"));
        assert_eq!(cq.from_id, 555);
        assert!(cq.text.is_empty());
        // Pesan foto (non-teks, bukan callback) tidak membawa id callback.
        assert!(updates[2].callback_query_id.is_none());
    }

    // --- Format MarkdownV2 (audit batch 8) ---

    #[test]
    fn mdv2_bold_italic_header_link() {
        assert_eq!(format_telegram_mdv2("**tebal**"), "*tebal*");
        assert_eq!(format_telegram_mdv2("*miring*"), "_miring_");
        assert_eq!(format_telegram_mdv2("~~coret~~"), "~coret~");
        assert_eq!(format_telegram_mdv2("||rahasia||"), "||rahasia||");
        assert_eq!(format_telegram_mdv2("# Judul"), "*Judul*");
        assert_eq!(format_telegram_mdv2("## Sub"), "*Sub*");
        assert_eq!(
            format_telegram_mdv2("[klik](https://contoh.id/x)"),
            "[klik](https://contoh.id/x)"
        );
        // Teks tautan di-escape, URL hanya ')' dan '\'.
        // ')' penutup link dipertahankan; ')' telanjang setelahnya di-escape.
        assert_eq!(
            format_telegram_mdv2("[a*b](https://x.id/a)b)"),
            "[a\\*b](https://x.id/a)b\\)"
        );
    }

    #[test]
    fn mdv2_teks_biasa_di_escape() {
        // Semua karakter spesial MarkdownV2 di-escape di luar kode.
        assert_eq!(
            format_telegram_mdv2("harga: 100.000 (diskon 50%)!"),
            "harga: 100\\.000 \\(diskon 50%\\)\\!"
        );
        assert_eq!(format_telegram_mdv2("a_b-c=d"), "a\\_b\\-c\\=d");
    }

    #[test]
    fn mdv2_kode_dilindungi() {
        // Isi pagar kode tak tersentuh kecuali '\' dan '`' (spek MarkdownV2).
        let src = "lihat:\n```rust\nlet x = a < b && c > d;\n```\nselesai";
        let out = format_telegram_mdv2(src);
        assert!(
            out.contains("```rust\nlet x = a < b && c > d;\n```"),
            "kode harus utuh: {out}"
        );
        assert!(out.contains("selesai"), "teks luar pagar tetap ada");
        // Inline code juga dilindungi.
        let out2 = format_telegram_mdv2("pakai `a*b` ya");
        assert!(out2.contains("`a*b`"), "inline code utuh: {out2}");
    }

    #[test]
    fn mdv2_blockquote_dan_tabel() {
        assert_eq!(format_telegram_mdv2("> kutipan"), "> kutipan");
        let tabel = "| Nama | Nilai |\n|---|---|\n| a | 1 |\n| b | 2 |";
        let out = format_telegram_mdv2(tabel);
        assert!(!out.contains('|'), "tabel harus jadi bullet: {out}");
        // Baris tanpa kolom label: heading = sel pertama non-kosong,
        // bullet hanya untuk sel yang nilainya beda dari heading.
        assert!(out.contains("• Nilai: 1"), "bullet: {out}");
        assert!(out.contains("• Nilai: 2"), "bullet: {out}");
    }

    #[test]
    fn mdv2_strip_jadi_teks_polos() {
        assert_eq!(strip_mdv2("*tebal*"), "tebal");
        assert_eq!(strip_mdv2("**tebal**"), "tebal");
        assert_eq!(strip_mdv2("_miring_"), "miring");
        assert_eq!(strip_mdv2("a\\_b"), "a_b");
        assert_eq!(strip_mdv2("~coret~ dan ||spoiler||"), "coret dan spoiler");
    }

    #[test]
    fn mdv2_deteksi_parse_error() {
        assert!(TelegramPlatform::is_parse_error(
            "Bad Request: can't parse entities: can't find end of the entity"
        ));
        assert!(TelegramPlatform::is_parse_error(
            "Bad Request: wrong MarkdownV2 entity"
        ));
        assert!(!TelegramPlatform::is_parse_error(
            "Bad Request: chat not found"
        ));
        assert!(!TelegramPlatform::is_parse_error(
            "Forbidden: bot was blocked"
        ));
    }

    #[test]
    fn mdv2_bare_bracket_di_escape_di_luar_kode() {
        // Kurung telanjang di teks biasa di-escape; di dalam kode tidak.
        let out = format_telegram_mdv2("fungsi (x) dan {y}");
        assert_eq!(out, "fungsi \\(x\\) dan \\{y\\}", "dapat: {out}");
        let link = format_telegram_mdv2("[t](https://x.id/a(b))");
        // '(' telanjang di dalam URL di-escape jaring pengaman; ')' penutup
        // link dipertahankan (walk-back menemukan '(' setelah ']').
        assert_eq!(link, "[t](https://x.id/a\\(b\\))", "link utuh: {link}");
    }
}
