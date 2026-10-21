#![allow(clippy::await_holding_lock)]
//! Test integrasi penanganan HTTP 429 melawan mock HTTP server lokal.
//!
//! Memverifikasi: 429 → tidur sesuai Retry-After/retry_after (cap 60 dtk) →
//! kirim ulang SEKALI → sukses bila percobaan kedua OK, atau error
//! rate-limited yang jelas bila masih 429. `Retry-After: 0` dipakai agar
//! tes tidak benar-benar tidur. Env var diserialkan via ENV_LOCK.
use hermes_core::Platform;
use hermes_gateway::{
    DiscordPlatform, MatrixPlatform, SlackPlatform, TelegramPlatform, WhatsAppPlatform,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct Scripted {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Scripted {
    fn new(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![],
            body: body.to_string(),
        }
    }

    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

/// Respons 429 ala Telegram: header Retry-After + body {"retry_after": N}.
fn telegram_429(retry_after_secs: u32) -> Scripted {
    Scripted::new(
        429,
        &format!(
            r#"{{"ok":false,"error_code":429,"description":"Too Many Requests: retry after {retry_after_secs}","retry_after":{retry_after_secs}}}"#
        ),
    )
    .header("Retry-After", &retry_after_secs.to_string())
}

/// Respons 429 ala Discord: retry_after float di body, tanpa header.
fn discord_429() -> Scripted {
    Scripted::new(
        429,
        r#"{"message":"You are being rate limited.","retry_after":0.0,"global":false}"#,
    )
}

/// Respons 429 generik dengan header Retry-After saja.
fn header_only_429(body: &str) -> Scripted {
    Scripted::new(429, body).header("Retry-After", "0")
}

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
}

struct MockServer {
    addr: std::net::SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// `repeat_last`: bila true, respons pertama dipakai untuk SEMUA request
    /// (untuk skenario "tetap 429").
    async fn start(script: Vec<Scripted>, repeat_last: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let script: Arc<Mutex<VecDeque<Scripted>>> =
            Arc::new(Mutex::new(script.into_iter().collect()));
        let rec = recorded.clone();
        let scr = script.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let rec = rec.clone();
                let scr = scr.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let mut lines = req.lines();
                    let rl = lines.next().unwrap_or("");
                    let mut parts = rl.split_whitespace();
                    let method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let mut content_length = 0usize;
                    for line in lines.by_ref() {
                        if line.is_empty() {
                            break;
                        }
                        if let Some(idx) = line.find(':') {
                            let (name, val) = line.split_at(idx);
                            if name.eq_ignore_ascii_case("content-length") {
                                content_length = val[1..].trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    let header_end = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
                    let mut body = buf[header_end..n].to_vec();
                    while body.len() < content_length {
                        let mut tmp = vec![0u8; 8192];
                        let m = socket.read(&mut tmp).await.unwrap_or(0);
                        if m == 0 {
                            break;
                        }
                        body.extend_from_slice(&tmp[..m]);
                    }
                    rec.lock().unwrap().push(Recorded { method, path });
                    let s = {
                        let mut q = scr.lock().unwrap();
                        if repeat_last {
                            q.front().cloned()
                        } else {
                            q.pop_front()
                        }
                    };
                    let s = s.unwrap_or_else(|| Scripted::new(200, "{}"));
                    let reason = match s.status {
                        200 => "OK",
                        429 => "Too Many Requests",
                        _ => "Error",
                    };
                    let mut head = format!(
                        "HTTP/1.1 {} {reason}\r\nContent-Type: application/json\r\n",
                        s.status
                    );
                    for (k, v) in &s.headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str(&format!(
                        "Content-Length: {}\r\nConnection: close\r\n\r\n",
                        s.body.len()
                    ));
                    let mut resp = head.into_bytes();
                    resp.extend_from_slice(s.body.as_bytes());
                    let _ = socket.write_all(&resp).await;
                });
            }
        });
        Self {
            addr,
            recorded,
            _handle: handle,
        }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn request_count(&self) -> usize {
        self.recorded.lock().unwrap().len()
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }
}

fn set_env(k: &str, v: &str) {
    std::env::set_var(k, v);
}
fn remove_env(k: &str) {
    std::env::remove_var(k);
}

// ---------------------------------------------------------------------------
// Telegram: 429 (header + body retry_after) → retry sekali → sukses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telegram_429_retry_sekali_lalu_sukses() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(
        vec![
            telegram_429(0),
            Scripted::new(200, r#"{"ok":true,"result":{"message_id":1}}"#),
        ],
        false,
    )
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("T429");
    let res = p.send_message("123", "halo").await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_ok(), "seharusnya sukses setelah satu retry: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2, "tepat 2 request (asli + satu retry): {rec:?}");
    assert!(
        rec.iter()
            .all(|r| r.method == "POST" && r.path.contains("/botT429/sendMessage")),
        "kedua request harus ke sendMessage: {rec:?}"
    );
}

#[tokio::test]
async fn telegram_429_terus_menerus_error_jelas() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![telegram_429(0)], true).await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("T429X");
    let res = p.send_message("123", "halo").await;
    remove_env("TELEGRAM_API_BASE");
    let err = res.expect_err("masih 429 setelah retry harus error");
    let msg = err.to_string();
    assert!(
        msg.contains("rate-limited") && msg.contains("429"),
        "pesan error harus jelas menyebut rate limit: {msg}"
    );
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2, "hanya SATU retry, tidak loop: {rec:?}");
    assert!(
        rec.iter().all(|r| r.path.contains("/botT429X/sendMessage")),
        "{rec:?}"
    );
}

// ---------------------------------------------------------------------------
// Discord: 429 dengan retry_after float di body (tanpa header)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discord_429_body_float_retry_sekali_lalu_sukses() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(
        vec![discord_429(), Scripted::new(200, r#"{"id":"m1"}"#)],
        false,
    )
    .await;
    set_env("DISCORD_API_BASE", &mock.url());
    let p = DiscordPlatform::new("D429");
    let res = p.send_message("C123", "halo").await;
    remove_env("DISCORD_API_BASE");
    assert!(res.is_ok(), "seharusnya sukses setelah satu retry: {res:?}");
    assert_eq!(mock.request_count(), 2, "{:?}", mock.recorded());
}

// ---------------------------------------------------------------------------
// Slack: 429 (header Retry-After) → retry sekali → sukses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slack_429_retry_sekali_lalu_sukses() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(
        vec![
            header_only_429(r#"{"ok":false,"error":"ratelimited"}"#),
            Scripted::new(200, r#"{"ok":true,"ts":"1.2"}"#),
        ],
        false,
    )
    .await;
    set_env("SLACK_API_BASE", &mock.url());
    let p = SlackPlatform::new("xoxb-429");
    let res = p.send_message("C999", "halo").await;
    remove_env("SLACK_API_BASE");
    assert!(res.is_ok(), "seharusnya sukses setelah satu retry: {res:?}");
    assert_eq!(mock.request_count(), 2, "{:?}", mock.recorded());
}

// ---------------------------------------------------------------------------
// WhatsApp: 429 → retry sekali → sukses
// ---------------------------------------------------------------------------

#[tokio::test]
async fn whatsapp_429_retry_sekali_lalu_sukses() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(
        vec![
            header_only_429(
                r#"{"error":{"message":"rate limited","type":"OAuthException","code":80007}}"#,
            ),
            Scripted::new(200, r#"{"messages":[{"id":"wamid.1"}]}"#),
        ],
        false,
    )
    .await;
    set_env("WHATSAPP_API_BASE", &mock.url());
    let p = WhatsAppPlatform::new("PNID429", "WATOKEN");
    let res = p.send_message("62812", "halo").await;
    remove_env("WHATSAPP_API_BASE");
    assert!(res.is_ok(), "seharusnya sukses setelah satu retry: {res:?}");
    assert_eq!(mock.request_count(), 2, "{:?}", mock.recorded());
}

// ---------------------------------------------------------------------------
// Matrix: 429 → retry sekali → sukses (tanpa env var; URL langsung)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matrix_429_retry_sekali_lalu_sukses() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(
        vec![
            header_only_429(r#"{"errcode":"M_LIMIT_EXCEEDED","error":"Too many requests"}"#),
            Scripted::new(200, r#"{"event_id":"$ev1"}"#),
        ],
        false,
    )
    .await;
    let p = MatrixPlatform::new(mock.url(), "MTOKEN");
    let res = p.send_message("!room:x", "halo").await;
    assert!(res.is_ok(), "seharusnya sukses setelah satu retry: {res:?}");
    assert_eq!(mock.request_count(), 2, "{:?}", mock.recorded());
}

// ---------------------------------------------------------------------------
// Non-429 tetap lewat tanpa retry (jalur normal tidak berubah)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tanpa_429_tidak_ada_retry() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(
        vec![Scripted::new(
            200,
            r#"{"ok":true,"result":{"message_id":1}}"#,
        )],
        false,
    )
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TOK");
    let res = p.send_message("123", "halo").await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_ok(), "{res:?}");
    assert_eq!(
        mock.request_count(),
        1,
        "respons non-429 tidak boleh memicu retry: {:?}",
        mock.recorded()
    );
}
