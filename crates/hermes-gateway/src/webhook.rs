//! Webhook masuk & keluar.
//!
//! Masuk: `hermes webhook serve` menjalankan server HTTP mini. Kirim
//! `POST /hook` dengan body `{"text": "...", "session": "id-opsional"}` dan
//! terima `{"response": "..."}`.
//! Keluar: `notify_outgoing` mengirim event JSON ke URL yang dikonfigurasi
//! (dipakai gateway & cron).
use hermes_core::{HermesError, Result};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Helper HMAC-SHA256 constant-time (`Hmac::verify`, bukan `==`) dipakai ulang
// dari whatsapp.rs agar satu implementasi kanonis di crate ini.
use crate::whatsapp::verify_whatsapp_signature;

/// Batas replay window webhook masuk, 5 menit — sama seperti original
/// (`_V2_REPLAY_WINDOW_SECONDS = 300` di `gateway/platforms/webhook.py`).
const REPLAY_WINDOW_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct WebhookRequest {
    pub text: String,
    pub session: Option<String>,
}

/// Jalankan server webhook masuk di `port`.
///
/// Body: `{"text": "...", "session": "..."}`. Otentikasi (fail-closed, menutup
/// temuan audit W6-B2): bila `secret` diset, request harus membawa
/// `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 dari body mentah>` plus
/// timestamp dalam replay window 5 menit (`X-Webhook-Timestamp` header, atau
/// field `timestamp` di body). Bila `secret` tidak diset/kosong, semua
/// `POST /hook` ditolak dengan 503 — tidak ada lagi mode tanpa otentikasi.
pub async fn serve_webhook<F, Fut>(port: u16, secret: Option<String>, handler: F) -> Result<()>
where
    F: FnMut(WebhookRequest) -> Fut + Send + Clone + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| HermesError::Gateway(format!("bind webhook gagal: {e}")))?;
    tracing::info!("webhook masuk di port {port} (POST /hook)");
    let secret = Arc::new(secret);
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| HermesError::Gateway(format!("accept gagal: {e}")))?;
        let secret = secret.clone();
        let mut handler = handler.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(&mut socket, &secret, &mut handler).await {
                tracing::warn!("webhook error: {e}");
            }
        });
    }
}

async fn handle_conn<F, Fut>(
    socket: &mut tokio::net::TcpStream,
    secret: &Option<String>,
    handler: &mut F,
) -> Result<()>
where
    F: FnMut(WebhookRequest) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let mut buf = vec![0u8; 65536];
    let n = socket
        .read(&mut buf)
        .await
        .map_err(|e| HermesError::Gateway(format!("read gagal: {e}")))?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut lines = req.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/").to_string();
    let path_no_query = path.split('?').next().unwrap_or("").to_string();

    let mut content_length = 0usize;
    let mut signature_header = String::new();
    let mut timestamp_header = String::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.trim().parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("x-hub-signature-256") {
            signature_header = value.trim().to_string();
        } else if name.eq_ignore_ascii_case("x-webhook-timestamp") {
            timestamp_header = value.trim().to_string();
        }
    }
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

    let response = if method == "GET" && path_no_query == "/health" {
        json_response(200, &json!({ "ok": true }))
    } else if method == "POST" && path_no_query == "/hook" {
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        // Otentikasi HMAC + replay window; fail-closed bila secret tak dikonfigurasi.
        if let Err((status, msg)) = verify_incoming_webhook(
            secret.as_deref(),
            &body,
            non_empty(&signature_header),
            non_empty(&timestamp_header),
            &v,
        ) {
            let resp = json_response(status, &json!({ "error": msg }));
            write_all(socket, &resp).await?;
            return Ok(());
        }
        let text = v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if text.trim().is_empty() {
            json_response(400, &json!({ "error": "field `text` wajib diisi" }))
        } else {
            let wr = WebhookRequest {
                text,
                session: v
                    .get("session")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string()),
            };
            match handler(wr).await {
                Ok(reply) => json_response(200, &json!({ "response": reply })),
                Err(e) => json_response(500, &json!({ "error": e.to_string() })),
            }
        }
    } else {
        json_response(404, &json!({ "error": "not found" }))
    };
    write_all(socket, &response).await?;
    Ok(())
}

/// Keputusan otentikasi webhook masuk (POST /hook) sebelum body diproses.
///
/// Skema (menutup temuan audit W6-B2):
/// 1. HMAC-SHA256 dari body MENTAH dengan secret, dibandingkan constant-time
///    (lewat `Hmac::verify`, bukan `==`) terhadap header
///    `X-Hub-Signature-256` dalam bentuk `sha256=<hex>`.
/// 2. Replay window 5 menit: diambil dari header `X-Webhook-Timestamp` (unix
///    seconds); bila tidak ada, dari field `timestamp` di body JSON (angka
///    atau string numerik); bila keduanya tidak ada → tolak. Pilihan fallback
///    ini mengikuti gaya original: V2 generik meminta `X-Webhook-Timestamp`
///    wajib, tetapi klien sederhana (curl, cron) lebih mudah mengirim
///    timestamp di body. Timestamp diterima bila |now - ts| <= 300 detik —
///    menolak replay lama maupun skew jam ekstrem ke depan, seperti
///    `_timestamp_fresh` di original.
/// 3. Fail-closed: secret tidak dikonfigurasi/kosong → `Err((503, _))` + warn
///    log, mengikuti pola batch-1 `verify_webhook_post` di `whatsapp.rs`.
///
/// KETIDAKKOMPATIBELAN TERENCANA: pemeriksaan token plaintext lama
/// (`body["token"] != secret`) DIHAPUS dan tidak dipertahankan — token di body
/// tidak menandatangani isi body dan tidak bisa dibandingkan constant-time,
/// sehingga mempertahankannya sebagai alternatif akan menggagalkan tujuan
/// perbaikan ini. Klien lama yang hanya mengirim `{"token": ...}` kini
/// mendapat 403 dan harus beralih ke signature HMAC + timestamp.
fn verify_incoming_webhook(
    secret: Option<&str>,
    body: &[u8],
    signature_header: Option<&str>,
    timestamp_header: Option<&str>,
    json_body: &Value,
) -> std::result::Result<(), (u16, &'static str)> {
    let secret = match secret {
        Some(s) if !s.is_empty() => s,
        _ => {
            tracing::warn!(
                "secret webhook masuk tidak dikonfigurasi; menolak POST /hook (fail-closed, 503)"
            );
            return Err((503, "webhook masuk belum dikonfigurasi"));
        }
    };
    if !verify_whatsapp_signature(secret, body, signature_header.unwrap_or("")) {
        tracing::warn!("signature webhook masuk tidak valid; menolak (403)");
        return Err((403, "signature webhook tidak valid"));
    }
    let ts: Option<u64> = timestamp_header
        .and_then(|h| h.parse().ok())
        .or_else(|| body_timestamp(json_body));
    match ts {
        Some(t) if timestamp_fresh(t) => Ok(()),
        _ => {
            tracing::warn!("timestamp webhook masuk hilang/kedaluwarsa; menolak (403)");
            Err((403, "timestamp webhook tidak valid atau kedaluwarsa"))
        }
    }
}

/// Ambil field `timestamp` dari body JSON (angka atau string numerik).
fn body_timestamp(v: &Value) -> Option<u64> {
    match v.get("timestamp") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

/// `true` bila timestamp unix-seconds berada dalam replay window (|now - ts| <= 300).
fn timestamp_fresh(ts: u64) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.abs_diff(ts) <= REPLAY_WINDOW_SECS
}

fn non_empty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
}

async fn write_all(socket: &mut tokio::net::TcpStream, data: &[u8]) -> Result<()> {
    socket
        .write_all(data)
        .await
        .map_err(|e| HermesError::Gateway(format!("write gagal: {e}")))
}

fn json_response(status: u16, body: &Value) -> Vec<u8> {
    let b = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "",
    };
    let mut r = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        b.len()
    )
    .into_bytes();
    r.extend_from_slice(b.as_bytes());
    r
}

/// Kirim event JSON ke semua URL webhook keluar (best-effort, async).
pub fn notify_outgoing(urls: &[String], event: Value) {
    if urls.is_empty() {
        return;
    }
    let urls: Vec<String> = urls.to_vec();
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build();
        let Ok(client) = client else { return };
        for url in urls {
            if let Err(e) = client.post(&url).json(&event).send().await {
                tracing::warn!("webhook keluar ke {url} gagal: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Tandatangani body seperti klien yang sah: `sha256=<hex HMAC-SHA256>`.
    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let mut s = String::with_capacity(7 + 64);
        s.push_str("sha256=");
        for b in mac.finalize().into_bytes() {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    fn ok_request(secret: &str, body: &serde_json::Value) -> (Vec<u8>, String, u64) {
        let raw = serde_json::to_vec(body).unwrap();
        let ts = now_secs();
        let sig = sign(secret, &raw);
        (raw, sig, ts)
    }

    #[test]
    fn hmac_known_answer_vector() {
        // RFC 4231 case 2: HMAC-SHA256(key="key",
        // "The quick brown fox jumps over the lazy dog").
        let body = b"The quick brown fox jumps over the lazy dog";
        let header = "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8";
        assert!(verify_whatsapp_signature("key", body, header));
        assert!(!verify_whatsapp_signature("key", body, "sha256=00"));
    }

    #[test]
    fn wrong_signature_rejected_403() {
        let body = json!({"text": "halo"});
        let (raw, _, ts) = ok_request("rahasia", &body);
        let err = verify_incoming_webhook(
            Some("rahasia"),
            &raw,
            Some("sha256=0000000000000000000000000000000000000000000000000000000000000000"),
            Some(&ts.to_string()),
            &body,
        )
        .expect_err("signature salah harus ditolak");
        assert_eq!(err.0, 403);
    }

    #[test]
    fn missing_secret_rejected_503_fail_closed() {
        let body = json!({"text": "halo"});
        let (raw, sig, ts) = ok_request("rahasia", &body);
        let err = verify_incoming_webhook(None, &raw, Some(&sig), Some(&ts.to_string()), &body)
            .expect_err("secret hilang harus 503 fail-closed");
        assert_eq!(err.0, 503);
    }

    #[test]
    fn empty_secret_rejected_503_fail_closed() {
        let body = json!({"text": "halo"});
        let (raw, sig, ts) = ok_request("rahasia", &body);
        let err = verify_incoming_webhook(Some(""), &raw, Some(&sig), Some(&ts.to_string()), &body)
            .expect_err("secret kosong harus 503 fail-closed");
        assert_eq!(err.0, 503);
    }

    #[test]
    fn stale_timestamp_rejected_403() {
        let secret = "rahasia";
        let body = json!({"text": "halo"});
        let raw = serde_json::to_vec(&body).unwrap();
        let sig = sign(secret, &raw);
        let stale = now_secs() - 600;
        let err = verify_incoming_webhook(
            Some(secret),
            &raw,
            Some(&sig),
            Some(&stale.to_string()),
            &body,
        )
        .expect_err("timestamp 10 menit lalu harus ditolak");
        assert_eq!(err.0, 403);
    }

    #[test]
    fn fresh_timestamp_header_accepted() {
        let secret = "rahasia";
        let body = json!({"text": "halo"});
        let (raw, sig, ts) = ok_request(secret, &body);
        verify_incoming_webhook(Some(secret), &raw, Some(&sig), Some(&ts.to_string()), &body)
            .expect("timestamp segar harus diterima");
    }

    #[test]
    fn fresh_timestamp_in_body_accepted() {
        let secret = "rahasia";
        let ts = now_secs();
        let body = json!({"text": "halo", "timestamp": ts});
        let raw = serde_json::to_vec(&body).unwrap();
        let sig = sign(secret, &raw);
        verify_incoming_webhook(Some(secret), &raw, Some(&sig), None, &body)
            .expect("timestamp di body harus diterima");
    }

    #[test]
    fn missing_timestamp_rejected_403() {
        let secret = "rahasia";
        let body = json!({"text": "halo"});
        let (raw, sig, _) = ok_request(secret, &body);
        let err = verify_incoming_webhook(Some(secret), &raw, Some(&sig), None, &body)
            .expect_err("tanpa timestamp harus ditolak");
        assert_eq!(err.0, 403);
    }

    #[test]
    fn legacy_body_token_no_longer_accepted() {
        // Klien lama {"token": secret} tanpa signature harus ditolak (403).
        let body = json!({"text": "halo", "token": "rahasia"});
        let (raw, _, ts) = ok_request("rahasia", &body);
        let err =
            verify_incoming_webhook(Some("rahasia"), &raw, None, Some(&ts.to_string()), &body)
                .expect_err("token body lama tanpa signature harus ditolak");
        assert_eq!(err.0, 403);
    }
}
