//! Platform WhatsApp via Meta Cloud API.
//!
//! Kirim: POST ke graph.facebook.com (butuh `WHATSAPP_TOKEN` + `WHATSAPP_PHONE_NUMBER_ID`).
//! Terima: webhook — jalankan `hermes gateway --platform whatsapp` dengan URL publik
//! yang diarahkan ke port webhook (lihat `--webhook-port`).
//!
//! Webhook `POST /webhook` diverifikasi dengan HMAC-SHA256 dari body mentah
//! berkunci `WHATSAPP_APP_SECRET` (app secret Meta, dibanding constant-time dengan
//! header `X-Hub-Signature-256`). Bila `WHATSAPP_APP_SECRET` tidak diset, webhook
//! ditolak dengan 503 (fail-closed) — jangan samakan dengan `WHATSAPP_VERIFY_TOKEN`
//! yang hanya dipakai untuk verifikasi subscription (GET /webhook).
use async_trait::async_trait;
use hermes_core::{HermesError, Platform, Result};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;
use std::future::Future;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::split::{split_message, WHATSAPP_MESSAGE_LIMIT};

/// Base URL Graph API; bisa di-override via WHATSAPP_API_BASE untuk pengujian.
fn api_base() -> String {
    std::env::var("WHATSAPP_API_BASE")
        .unwrap_or_else(|_| "https://graph.facebook.com/v21.0".to_string())
}

pub struct WhatsAppPlatform {
    phone_number_id: String,
    access_token: String,
    client: reqwest::Client,
}

impl WhatsAppPlatform {
    pub fn new(phone_number_id: impl Into<String>, access_token: impl Into<String>) -> Self {
        Self {
            phone_number_id: phone_number_id.into(),
            access_token: access_token.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WhatsAppMessage {
    pub from: String,
    pub text: String,
}

#[async_trait]
impl Platform for WhatsAppPlatform {
    fn id(&self) -> &str {
        "whatsapp"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: nomor/wa_id penerima (atau "penerima:message_id" untuk
        // mengutip pesan tertentu).
        let (to, reply_to) = match target.split_once(':') {
            Some((t, m)) if !m.is_empty() => (t, Some(m)),
            _ => (target, None),
        };
        // Batas 4096 karakter seperti `MAX_MESSAGE_LENGTH` aslinya; pecah
        // sadar-pagar agar blok ``` tidak terpotong antar pesan.
        // Markdown dikonversi ke sintaks WhatsApp dulu (*tebal*, _miring_,
        // ~coret~) seperti `format_message()` aslinya.
        let formatted = format_whatsapp(text);
        for (i, chunk) in split_message(&formatted, Some(WHATSAPP_MESSAGE_LIMIT))
            .iter()
            .enumerate()
        {
            let mut body = json!({
                "messaging_product": "whatsapp",
                "to": to,
                "type": "text",
                "text": { "body": chunk, "preview_url": true },
            });
            // Kutip pesan pengguna hanya pada chunk pertama — cermin
            // `reply_to if idx == 0 else None` aslinya.
            if i == 0 {
                if let Some(mid) = reply_to {
                    body["context"] = json!({ "message_id": mid });
                }
            }
            // Bungkus sebagai closure agar request identik bisa dikirim ulang
            // bila API menjawab 429 (lihat crate::rate_limit).
            let send = || async {
                self.client
                    .post(format!("{}/{}/messages", api_base(), self.phone_number_id))
                    .bearer_auth(&self.access_token)
                    .json(&body)
                    .send()
                    .await
                    .map_err(|e| HermesError::Gateway(format!("whatsapp send gagal: {e}")))
            };
            let resp = send().await?;
            // 429 → tidur sesuai Retry-After (cap 60 dtk), retry sekali.
            let resp = crate::rate_limit::handle_429("whatsapp", resp, send).await?;
            if !resp.status().is_success() {
                let b = resp.text().await.unwrap_or_default();
                return Err(HermesError::Gateway(format!("whatsapp menolak pesan: {b}")));
            }
        }
        Ok(())
    }
}

/// Kirim voice note (audio ogg/opus) ke chat WhatsApp.
pub async fn send_voice_note(
    platform: &WhatsAppPlatform,
    target: &str,
    audio_bytes: &[u8],
    mime: &str,
) -> Result<()> {
    // 1. Upload media.
    let send_upload = || async {
        let part = reqwest::multipart::Part::bytes(audio_bytes.to_vec())
            .file_name("voice.ogg")
            .mime_str(mime)
            .map_err(|e| HermesError::Gateway(format!("mime invalid: {e}")))?;
        // multipart::Form dikonsumsi oleh send(); bangun ulang tiap percobaan
        // agar retry 429 mengunggah body yang identik.
        let form = reqwest::multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .part("file", part);
        platform
            .client
            .post(format!("{}/media", api_base()))
            .bearer_auth(&platform.access_token)
            .multipart(form)
            .send()
            .await
            .map_err(|e| HermesError::Gateway(format!("upload media gagal: {e}")))
    };
    let resp = send_upload().await?;
    // 429 → tidur sesuai Retry-After (cap 60 dtk), retry sekali.
    let resp = crate::rate_limit::handle_429("whatsapp", resp, send_upload).await?;
    let v: Value = resp
        .json()
        .await
        .map_err(|e| HermesError::Gateway(format!("parse media gagal: {e}")))?;
    let media_id = v
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| HermesError::Gateway(format!("upload ditolak: {v}")))?;
    // 2. Kirim sebagai audio.
    let body = json!({
        "messaging_product": "whatsapp",
        "to": target,
        "type": "audio",
        "audio": { "id": media_id },
    });
    let send_audio = || async {
        platform
            .client
            .post(format!(
                "{}/{}/messages",
                api_base(),
                platform.phone_number_id
            ))
            .bearer_auth(&platform.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| HermesError::Gateway(format!("kirim audio gagal: {e}")))
    };
    let resp = send_audio().await?;
    // 429 → tidur sesuai Retry-After (cap 60 dtk), retry sekali.
    let resp = crate::rate_limit::handle_429("whatsapp", resp, send_audio).await?;
    if !resp.status().is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(HermesError::Gateway(format!("whatsapp menolak audio: {b}")));
    }
    Ok(())
}

/// Verifikasi signature webhook WhatsApp.
///
/// Menghitung HMAC-SHA256 dari body mentah memakai app secret, lalu
/// membandingkannya dengan nilai header `X-Hub-Signature-256` secara
/// constant-time (lewat `Hmac::verify`, bukan `==` byte).
///
/// `header` harus berbentuk `sha256=<hex HMAC 64 karakter>`; bentuk lain
/// (tanpa prefix, hex invalid, panjang salah) ditolak.
pub fn verify_whatsapp_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let hex = match header.strip_prefix("sha256=") {
        Some(h) => h,
        None => return false,
    };
    let expected = match hex_decode_32(hex) {
        Some(b) => b,
        None => return false,
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    // `Mac::verify` memakai perbandingan constant-time (subtle).
    mac.verify((&expected).into()).is_ok()
}

/// Decode hex 64 karakter menjadi 32 byte; `None` bila malformed.
fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Keputusan verifikasi webhook POST sebelum body diproses.
///
/// - `WHATSAPP_APP_SECRET` tidak diset/kosong → `Err((503, _))` fail-closed + warn log.
/// - signature hilang/salah → `Err((403, _))`.
/// - cocok → `Ok(())`.
fn verify_webhook_post(
    body: &[u8],
    signature_header: Option<&str>,
) -> std::result::Result<(), (u16, &'static str)> {
    let secret = match std::env::var("WHATSAPP_APP_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            tracing::warn!(
                "WHATSAPP_APP_SECRET tidak diset; menolak webhook whatsapp (fail-closed, 503)"
            );
            return Err((503, "webhook whatsapp belum dikonfigurasi"));
        }
    };
    let header = signature_header.unwrap_or("");
    if verify_whatsapp_signature(&secret, body, header) {
        Ok(())
    } else {
        tracing::warn!("signature webhook whatsapp tidak valid; menolak (403)");
        Err((403, "signature webhook tidak valid"))
    }
}

/// Daftarkan handler + platform lalu jalankan server webhook.
///
/// Meta memverifikasi via `GET /webhook?hub.mode=subscribe&hub.verify_token=...`,
/// lalu mengirim pesan masuk via `POST /webhook`.
pub async fn run_whatsapp_gateway<F, Fut>(
    phone_number_id: String,
    access_token: String,
    verify_token: String,
    port: u16,
    mut handler: F,
) -> Result<()>
where
    F: FnMut(WhatsAppMessage) -> Fut + Send + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let platform = Arc::new(WhatsAppPlatform::new(phone_number_id, access_token));
    *WEBHOOK_PLATFORM.lock().unwrap() = Some(platform);
    *WEBHOOK_HANDLER.lock().unwrap() = Some(Box::new(move |m: WhatsAppMessage| {
        Box::pin(handler(m)) as std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send>>
    }));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| HermesError::Gateway(format!("bind webhook gagal: {e}")))?;
    tracing::info!("webhook whatsapp di port {port}");
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| HermesError::Gateway(format!("accept gagal: {e}")))?;
        let verify_token = verify_token.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_webhook_conn(&mut socket, &verify_token).await {
                tracing::warn!("webhook error: {e}");
            }
        });
    }
}

/// Bandingkan dua token dalam waktu konstan (cermin `hmac.compare_digest`
/// pada `_handle_verify` aslinya).
fn tokens_equal(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    ab.len() == bb.len() && ab.ct_eq(bb).into()
}

/// Tanggapi handshake berlangganan Meta `GET /webhook?hub.mode=subscribe&
/// hub.verify_token=...&hub.challenge=...`.
///
/// Cermin `_handle_verify` di `plugins/platforms/whatsapp_cloud/adapter.py`
/// aslinya: tanpa verify_token yang dikonfigurasi → 503 (fail-closed,
/// menolak menerima token apa pun); mode selain `subscribe` → 400;
/// `hub.verify_token` dibandingkan constant-time → 403 bila tak cocok;
/// challenge kosong → 400; bila cocok, gema `hub.challenge`.
pub fn verify_subscription(path: &str, verify_token: &str) -> Vec<u8> {
    if verify_token.is_empty() {
        // Menolak daripada menerima token apa pun — penyerang bisa
        // berlangganan webhook bila token kosong diterima.
        tracing::warn!(
            "webhook whatsapp: verify_token tidak dikonfigurasi; handshake ditolak (503)"
        );
        return http_status(503, b"verify_token not configured");
    }
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    // `form_urlencoded::parse` men-decode persen seperti query parser
    // aiohttp aslinya.
    let params: std::collections::HashMap<String, String> =
        url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
    let mode = params.get("hub.mode").map(|s| s.as_str()).unwrap_or("");
    if mode != "subscribe" {
        return http_status(400, b"bad mode");
    }
    let got = params
        .get("hub.verify_token")
        .map(|s| s.as_str())
        .unwrap_or("");
    if !tokens_equal(got, verify_token) {
        return http_status(403, b"verify_token mismatch");
    }
    match params.get("hub.challenge") {
        Some(ch) if !ch.is_empty() => http_ok(ch.as_bytes()),
        _ => http_status(400, b"missing challenge"),
    }
}

async fn handle_webhook_conn(socket: &mut tokio::net::TcpStream, verify_token: &str) -> Result<()> {
    let mut buf = vec![0u8; 65536];
    let n = socket
        .read(&mut buf)
        .await
        .map_err(|e| HermesError::Gateway(format!("read gagal: {e}")))?;
    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    // Kumpulkan header + body.
    let mut content_length = 0usize;
    let mut signature_header: Option<String> = None;
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        let (name, value) = match line.split_once(':') {
            Some((n, v)) => (n.trim(), v.trim()),
            None => continue,
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("x-hub-signature-256") {
            signature_header = Some(value.to_string());
        }
    }
    // Body mungkin sudah terbaca sebagian di buf; baca sisanya bila perlu.
    let header_end = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
    let mut body = buf[header_end..n].to_vec();
    while body.len() < content_length {
        let mut tmp = vec![0u8; 8192];
        let m = socket
            .read(&mut tmp)
            .await
            .map_err(|e| HermesError::Gateway(format!("read body gagal: {e}")))?;
        if m == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..m]);
    }

    let response = if method == "GET" && path.starts_with("/webhook") {
        // Verifikasi Meta — cermin `_handle_verify` aslinya:
        // tanpa verify_token → 503 (fail-closed, menolak handshake);
        // mode != subscribe → 400; challenge kosong → 400;
        // token dibandingkan constant-time → 403 bila tak cocok.
        verify_subscription(path, verify_token)
    } else if method == "POST" && path.starts_with("/webhook") {
        // Verifikasi HMAC-SHA256 Meta (fail-closed) SEBELUM body diproses.
        if let Err((code, msg)) = verify_webhook_post(&body, signature_header.as_deref()) {
            return write_response(socket, http_status(code, msg.as_bytes())).await;
        }
        let body_str = String::from_utf8_lossy(&body);
        let v: Value = serde_json::from_str(&body_str).unwrap_or(Value::Null);
        let mut replies: Vec<(String, String, String)> = Vec::new();
        for entry in v
            .get("entry")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default()
        {
            for change in entry
                .get("changes")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default()
            {
                let value = change.get("value").unwrap_or(&Value::Null);
                for msg in value
                    .get("messages")
                    .and_then(|m| m.as_array())
                    .cloned()
                    .unwrap_or_default()
                {
                    // Dedup wamid SEBELUM pemrosesan apa pun: Meta me-retry
                    // delivery hingga 7 hari, jadi pesan yang sama bisa tiba
                    // berulang kali.
                    let wamid = msg.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    if is_duplicate_wamid(wamid) {
                        tracing::debug!(
                            wamid = %wamid,
                            "pesan whatsapp duplikat (wamid sudah diproses); dibuang"
                        );
                        continue;
                    }
                    if msg.get("type").and_then(|t| t.as_str()) != Some("text") {
                        continue;
                    }
                    let from = msg
                        .get("from")
                        .and_then(|f| f.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = msg
                        .pointer("/text/body")
                        .and_then(|b| b.as_str())
                        .unwrap_or("")
                        .to_string();
                    if from.is_empty() || text.is_empty() {
                        continue;
                    }
                    // Panggil handler global (lepaskan lock sebelum await agar Send).
                    let fut_opt = {
                        let mut slot = WEBHOOK_HANDLER.lock().unwrap();
                        slot.as_mut().map(|h| {
                            h(WhatsAppMessage {
                                from: from.clone(),
                                text,
                            })
                        })
                    };
                    let reply = match fut_opt {
                        Some(fut) => fut.await,
                        None => Ok(String::new()),
                    };
                    match reply {
                        // Sertakan wamid agar balasan mengutip pesan pemicu
                        // (context.message_id pada chunk pertama).
                        Ok(r) if !r.is_empty() => replies.push((from, wamid.to_string(), r)),
                        Err(e) => replies.push((
                            from,
                            wamid.to_string(),
                            format!("Maaf, terjadi error: {e}"),
                        )),
                        _ => {}
                    }
                }
            }
        }
        // Kirim balasan (platform diambil dari state global kedua).
        if let Some(p) = webhook_platform() {
            for (to, wamid, text) in replies {
                let target = if wamid.is_empty() {
                    to.clone()
                } else {
                    format!("{to}:{wamid}")
                };
                if let Err(e) = p.send_message(&target, &text).await {
                    tracing::warn!("gagal membalas whatsapp: {e}");
                }
            }
        }
        http_ok(b"OK")
    } else {
        http_status(404, b"not found")
    };

    write_response(socket, response).await
}

/// Simpan `value` di balik token placeholder (cermin `_stash` aslinya).
fn stash_wa(ph: &mut Vec<(String, String)>, value: String) -> String {
    let key = format!("\u{0}WA{}\u{0}", ph.len());
    ph.push((key.clone(), value));
    key
}

/// Bersihkan karakter tak terlihat / spasi aneh sebelum dikirim —
/// cermin `_sanitize_outbound_text` aslinya (WhatsApp merendernya sebagai
/// mojibake). Zero-width joiner emoji dipertahankan.
fn sanitize_outbound_wa(content: &str) -> String {
    content
        .chars()
        .filter_map(|c| match c {
            '\u{200b}' | '\u{2060}' | '\u{2063}' | '\u{feff}' => None,
            '\u{00a0}' | '\u{1680}' | '\u{180e}' | '\u{2000}' | '\u{2001}' | '\u{2002}'
            | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}' | '\u{2007}' | '\u{2008}'
            | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => Some(' '),
            _ => Some(c),
        })
        .collect()
}

/// Konversi markdown menjadi sintaks WhatsApp (`*tebal*`, `_miring_`,
/// `~coret~`); kode pagar/inline dilindungi placeholder.
///
/// Cermin `format_message()` di `gateway/platforms/whatsapp_common.py`
/// aslinya.
pub fn format_whatsapp(content: &str) -> String {
    use regex::Regex;
    if content.is_empty() {
        return content.to_string();
    }
    let mut ph: Vec<(String, String)> = Vec::new();
    let mut text = sanitize_outbound_wa(content);
    // Lindungi pagar kode dan inline code.
    let fence_re = Regex::new(r"```[\s\S]*?```").unwrap();
    text = fence_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_wa(&mut ph, caps[0].to_string())
        })
        .into_owned();
    let code_re = Regex::new(r"`[^`\n]+`").unwrap();
    text = code_re
        .replace_all(&text, |caps: &regex::Captures| {
            stash_wa(&mut ph, caps[0].to_string())
        })
        .into_owned();
    // Miring *teks* → _teks_ SEBELUM tebal agar **tebal** tak jadi miring;
    // look-around aslinya (lewati bullet list dan pembatas bold) diperiksa
    // manual karena crate regex tak mendukungnya.
    let italic_re = Regex::new(r"\*([^*\n]*?\S[^*\n]*?)\*").unwrap();
    text = {
        let src = text;
        let mut res = String::with_capacity(src.len());
        let mut last = 0;
        for caps in italic_re.captures_iter(&src) {
            let m = caps.get(0).unwrap();
            let prev_star = src[..m.start()].ends_with('*');
            let next_star = src[m.end()..].starts_with('*');
            let after_open = src[m.start() + 1..].chars().next();
            let bad_open = matches!(after_open, Some(c) if c.is_whitespace() || c == '*');
            if prev_star || next_star || bad_open {
                continue;
            }
            res.push_str(&src[last..m.start()]);
            res.push('_');
            res.push_str(&caps[1]);
            res.push('_');
            last = m.end();
        }
        res.push_str(&src[last..]);
        res
    };
    // **tebal** → *tebal*; __tebal__ → *tebal*; ~~coret~~ → ~coret~.
    let bold_re = Regex::new(r"\*\*(.+?)\*\*").unwrap();
    text = bold_re.replace_all(&text, "*$1*").into_owned();
    let bold2_re = Regex::new(r"__(.+?)__").unwrap();
    text = bold2_re.replace_all(&text, "*$1*").into_owned();
    let strike_re = Regex::new(r"~~(.+?)~~").unwrap();
    text = strike_re.replace_all(&text, "~$1~").into_owned();
    // Header → *tebal* (kupas *...* yang sudah ada).
    let header_re = Regex::new(r"(?m)^#{1,6}\s+(.+)$").unwrap();
    text = header_re
        .replace_all(&text, |caps: &regex::Captures| {
            let mut inner = caps[1].trim().to_string();
            while inner.len() > 1 && inner.starts_with('*') && inner.ends_with('*') {
                inner = inner[1..inner.len() - 1].trim().to_string();
            }
            format!("*{inner}*")
        })
        .into_owned();
    // [teks](url) → teks (url).
    let link_re = Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap();
    text = link_re.replace_all(&text, "$1 ($2)").into_owned();
    // Kembalikan placeholder.
    for (key, value) in ph.iter().rev() {
        text = text.replace(key, value);
    }
    text
}

/// Tulis respons HTTP mentah ke socket lalu tutup.
async fn write_response(socket: &mut tokio::net::TcpStream, response: Vec<u8>) -> Result<()> {
    socket
        .write_all(&response)
        .await
        .map_err(|e| HermesError::Gateway(format!("write gagal: {e}")))?;
    Ok(())
}

use std::sync::Mutex as StdMutex;

/// Kapasitas maksimum wamid yang diingat untuk dedup.
const MAX_SEEN_WAMIDS: usize = 5000;

/// Set bounded wamid yang sudah diproses (FIFO eviction).
///
/// Meta me-retry pengiriman webhook hingga 7 hari, sehingga pesan yang sama
/// bisa tiba berulang kali. `check` mengembalikan `true` bila wamid sudah
/// pernah terlihat (duplikat → buang sebelum diproses).
///
/// In-memory: restart proses menghapus seluruh set ini.
struct WamidDedup {
    order: std::collections::VecDeque<String>,
    seen: std::collections::HashSet<String>,
    capacity: usize,
}

impl WamidDedup {
    fn new(capacity: usize) -> Self {
        Self {
            order: std::collections::VecDeque::new(),
            seen: std::collections::HashSet::new(),
            capacity,
        }
    }

    /// `true` = duplikat (sudah pernah diproses); `false` = baru, dicatat.
    /// wamid kosong tidak pernah didedup (selalu dianggap baru).
    fn check(&mut self, wamid: &str) -> bool {
        if wamid.is_empty() {
            return false;
        }
        if self.seen.contains(wamid) {
            return true;
        }
        while self.order.len() >= self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            } else {
                break;
            }
        }
        self.order.push_back(wamid.to_string());
        self.seen.insert(wamid.to_string());
        false
    }
}

/// wamid yang sudah diproses di proses ini (lihat [`WamidDedup`]).
static SEEN_WAMIDS: std::sync::LazyLock<StdMutex<WamidDedup>> =
    std::sync::LazyLock::new(|| StdMutex::new(WamidDedup::new(MAX_SEEN_WAMIDS)));

/// Cek dedup wamid terhadap set global; `true` = duplikat, buang pesan.
fn is_duplicate_wamid(wamid: &str) -> bool {
    SEEN_WAMIDS.lock().unwrap().check(wamid)
}

/// State global untuk handler webhook (diset sekali saat serve dimulai).
static WEBHOOK_HANDLER: StdMutex<Option<WebhookHandlerBox>> = StdMutex::new(None);
type WebhookHandlerBox = Box<
    dyn FnMut(WhatsAppMessage) -> std::pin::Pin<Box<dyn Future<Output = Result<String>> + Send>>
        + Send,
>;

static WEBHOOK_PLATFORM: StdMutex<Option<Arc<WhatsAppPlatform>>> = StdMutex::new(None);

fn webhook_platform() -> Option<Arc<WhatsAppPlatform>> {
    WEBHOOK_PLATFORM.lock().unwrap().clone()
}

fn http_ok(body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}

fn http_status(code: u16, body: &[u8]) -> Vec<u8> {
    let mut r = format!(
        "HTTP/1.1 {code} \r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    r.extend_from_slice(body);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vektor known-answer RFC 4231 test case 2:
    // key = "Jefe", msg = "what do ya want for nothing?".
    const RFC4231_TC2: &str = "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843";

    // Lock agar tes yang menyentuh env var berjalan serial (tes Rust paralel).
    static ENV_LOCK: StdMutex<()> = StdMutex::new(());
    // Counter agar tiap tes dedup memakai wamid unik (set global dishare antar tes).
    static WAMID_TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn unique_wamid() -> String {
        let n = WAMID_TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("wamid-test-{n}")
    }

    #[test]
    fn signature_valid_passes() {
        let header = format!("sha256={RFC4231_TC2}");
        assert!(verify_whatsapp_signature(
            "Jefe",
            b"what do ya want for nothing?",
            &header
        ));
    }

    #[test]
    fn signature_wrong_secret_and_body_fail() {
        let header = format!("sha256={RFC4231_TC2}");
        // Kunci salah.
        assert!(!verify_whatsapp_signature(
            "WrongKey",
            b"what do ya want for nothing?",
            &header
        ));
        // Body salah.
        assert!(!verify_whatsapp_signature("Jefe", b"other body", &header));
        // Satu nibble diubah.
        let tampered =
            "sha256=4bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843".to_string();
        assert!(!verify_whatsapp_signature(
            "Jefe",
            b"what do ya want for nothing?",
            &tampered
        ));
    }

    #[test]
    fn signature_malformed_headers_fail() {
        let body = b"what do ya want for nothing?";
        // Header kosong.
        assert!(!verify_whatsapp_signature("Jefe", body, ""));
        // Tanpa prefix sha256=.
        assert!(!verify_whatsapp_signature("Jefe", body, RFC4231_TC2));
        // Prefix salah.
        let sha1 = format!("sha1={RFC4231_TC2}");
        assert!(!verify_whatsapp_signature("Jefe", body, &sha1));
        // Hex invalid.
        let bad_hex = format!("sha256={}", "zz".repeat(32));
        assert!(!verify_whatsapp_signature("Jefe", body, &bad_hex));
        // Panjang salah (terlalu pendek).
        assert!(!verify_whatsapp_signature("Jefe", body, "sha256=deadbeef"));
        // Panjang salah (terlalu panjang).
        let too_long = format!("sha256={RFC4231_TC2}00");
        assert!(!verify_whatsapp_signature("Jefe", body, &too_long));
        // Prefix saja.
        assert!(!verify_whatsapp_signature("Jefe", body, "sha256="));
        // Uppercase hex tetap valid (hex decode case-insensitive).
        let upper = format!("sha256={}", RFC4231_TC2.to_ascii_uppercase());
        assert!(verify_whatsapp_signature("Jefe", body, &upper));
    }

    #[test]
    fn webhook_post_503_when_secret_unset_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("WHATSAPP_APP_SECRET");
        // Signature valid pun tetap ditolak bila secret tidak dikonfigurasi.
        let header = format!("sha256={RFC4231_TC2}");
        let err = verify_webhook_post(b"what do ya want for nothing?", Some(&header))
            .expect_err("harus 503 bila secret tidak diset");
        assert_eq!(err.0, 503);
    }

    #[test]
    fn webhook_post_503_when_secret_empty_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("WHATSAPP_APP_SECRET", "");
        let err = verify_webhook_post(b"body", None).expect_err("secret kosong harus 503");
        assert_eq!(err.0, 503);
        std::env::remove_var("WHATSAPP_APP_SECRET");
    }

    #[test]
    fn webhook_post_403_when_signature_bad_and_200_path_ok() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("WHATSAPP_APP_SECRET", "Jefe");
        let body = b"what do ya want for nothing?";
        // Signature salah → 403.
        let err = verify_webhook_post(
            body,
            Some("sha256=deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"),
        )
        .expect_err("harus 403");
        assert_eq!(err.0, 403);
        // Header hilang → 403.
        let err = verify_webhook_post(body, None).expect_err("harus 403");
        assert_eq!(err.0, 403);
        // Signature benar → Ok.
        let header = format!("sha256={RFC4231_TC2}");
        assert!(verify_webhook_post(body, Some(&header)).is_ok());
        std::env::remove_var("WHATSAPP_APP_SECRET");
    }

    #[test]
    fn wamid_dedup_same_id_twice_second_dropped() {
        let mut d = WamidDedup::new(10);
        assert!(!d.check("wamid.abc"), "kemunculan pertama harus diproses");
        assert!(d.check("wamid.abc"), "kemunculan kedua harus dibuang");
        assert!(d.check("wamid.abc"), "kemunculan ketiga tetap duplikat");
        assert!(!d.check("wamid.def"), "wamid berbeda harus diproses");
    }

    #[test]
    fn wamid_dedup_empty_id_never_deduped() {
        let mut d = WamidDedup::new(10);
        // Pesan tanpa wamid (malformed) tidak boleh saling menduplikat.
        assert!(!d.check(""));
        assert!(!d.check(""));
    }

    #[test]
    fn wamid_dedup_fifo_eviction() {
        let mut d = WamidDedup::new(3);
        assert!(!d.check("a"));
        assert!(!d.check("b"));
        assert!(!d.check("c"));
        assert!(!d.check("d"), "evict 'a' (paling lama)");
        assert!(d.check("b"), "'b' masih diingat → duplikat");
        assert!(d.check("c"), "'c' masih diingat → duplikat");
        assert!(d.check("d"), "'d' masih diingat → duplikat");
        assert!(!d.check("a"), "'a' sudah dilupakan → dianggap baru lagi");
    }

    #[test]
    fn wamid_dedup_global_state_drops_repeat() {
        // Memakai set global sungguhan (id unik per tes agar tidak bentrok).
        let id = unique_wamid();
        assert!(!is_duplicate_wamid(&id));
        assert!(is_duplicate_wamid(&id));
    }

    // --- Verifikasi handshake GET (audit batch 8) ---

    /// Ekstrak (status code, body) dari respons HTTP mentah `verify_subscription`.
    fn verify_parts(resp: &[u8]) -> (u16, String) {
        let s = String::from_utf8_lossy(resp).into_owned();
        let code: u16 = s
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("0")
            .parse()
            .unwrap_or(0);
        let body = s.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        (code, body)
    }

    #[test]
    fn verify_ok_menggemakan_challenge() {
        let (code, body) = verify_parts(&verify_subscription(
            "/webhook?hub.mode=subscribe&hub.verify_token=rahasia&hub.challenge=CH123",
            "rahasia",
        ));
        assert_eq!(code, 200);
        assert_eq!(body, "CH123");
    }

    #[test]
    fn verify_token_kosong_503_fail_closed() {
        // Tanpa verify_token yang dikonfigurasi, handshake DITOLAK (503) —
        // bukan diterima dengan token apa pun (cermin aslinya).
        let (code, body) = verify_parts(&verify_subscription(
            "/webhook?hub.mode=subscribe&hub.verify_token=apa-saja&hub.challenge=C",
            "",
        ));
        assert_eq!(code, 503);
        assert_eq!(body, "verify_token not configured");
    }

    #[test]
    fn verify_salah_token_403() {
        let (code, body) = verify_parts(&verify_subscription(
            "/webhook?hub.mode=subscribe&hub.verify_token=salah&hub.challenge=C",
            "rahasia",
        ));
        assert_eq!(code, 403);
        assert_eq!(body, "verify_token mismatch");
    }

    #[test]
    fn verify_mode_salah_400() {
        let (code, _) = verify_parts(&verify_subscription(
            "/webhook?hub.mode=unsubscribe&hub.verify_token=rahasia&hub.challenge=C",
            "rahasia",
        ));
        assert_eq!(code, 400);
    }

    #[test]
    fn verify_challenge_kosong_400() {
        let (code, _) = verify_parts(&verify_subscription(
            "/webhook?hub.mode=subscribe&hub.verify_token=rahasia",
            "rahasia",
        ));
        assert_eq!(code, 400);
    }

    #[test]
    fn verify_token_percent_encoded() {
        // Query di-decode persen seperti parser aiohttp aslinya.
        let (code, body) = verify_parts(&verify_subscription(
            "/webhook?hub.mode=subscribe&hub.verify_token=raha%20sia&hub.challenge=C",
            "raha sia",
        ));
        assert_eq!(code, 200);
        assert_eq!(body, "C");
    }

    #[test]
    fn tokens_equal_constant_time_semantik() {
        assert!(tokens_equal("rahasia", "rahasia"));
        assert!(!tokens_equal("rahasia", "rahasib"));
        assert!(!tokens_equal("rahasia", "rahasia!"));
        assert!(!tokens_equal("rahasia", ""));
        assert!(tokens_equal("", ""));
    }

    // --- Format WhatsApp (audit batch 8) ---

    #[test]
    fn wa_format_bold_italic_strike() {
        assert_eq!(format_whatsapp("**tebal**"), "*tebal*");
        assert_eq!(format_whatsapp("__tebal__"), "*tebal*");
        assert_eq!(format_whatsapp("*miring*"), "_miring_");
        assert_eq!(format_whatsapp("~~coret~~"), "~coret~");
        assert_eq!(format_whatsapp("# Judul"), "*Judul*");
        assert_eq!(format_whatsapp("# **Judul**"), "*Judul*");
        assert_eq!(
            format_whatsapp("[klik](https://contoh.id)"),
            "klik (https://contoh.id)"
        );
    }

    #[test]
    fn wa_format_kode_dilindungi_dari_konversi() {
        let out = format_whatsapp("pakai `*bukan-miring*` dan ```**bukan-tebal**```");
        assert!(out.contains("`*bukan-miring*`"), "inline utuh: {out}");
        assert!(out.contains("```**bukan-tebal**```"), "pagar utuh: {out}");
    }

    #[test]
    fn wa_format_bullet_tidak_jadi_miring() {
        // "* item" (bullet list) tidak punya penutup → bukan italic.
        let out = format_whatsapp("* item satu\n* item dua");
        assert_eq!(out, "* item satu\n* item dua", "dapat: {out}");
    }

    #[test]
    fn wa_sanitize_hapus_invisible_normalisasi_spasi() {
        assert_eq!(sanitize_outbound_wa("a\u{200b}b"), "ab");
        assert_eq!(sanitize_outbound_wa("a\u{00a0}b"), "a b");
        // Zero-width joiner emoji dipertahankan.
        assert_eq!(sanitize_outbound_wa("👨\u{200d}👩"), "👨\u{200d}👩");
    }
}
