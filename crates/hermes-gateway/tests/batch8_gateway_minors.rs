#![allow(clippy::await_holding_lock)]
//! Test integrasi batch 8 (gateway minors): formatting per platform,
//! reply-threading defaults, dan batas chunk — melawan mock HTTP lokal.
//!
//! Env var diserialkan via ENV_LOCK karena std::env bersifat global per-proses.
use hermes_core::Platform;
use hermes_gateway::{
    DiscordPlatform, MatrixPlatform, SlackPlatform, TelegramPlatform, WhatsAppPlatform,
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Satu route mock: (method, path_prefix, respons berurutan).
type MockRoute = (String, String, Vec<(u16, String)>);

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct Recorded {
    method: String,
    path: String,
    body: String,
}

/// Mock HTTP dengan respons berurutan: setiap request ke `path_prefix`
/// mendapat respons berikutnya dari `responses` (bila habis, pakai terakhir).
struct MockServer {
    addr: std::net::SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    async fn start(routes: Vec<MockRoute>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        let handle = tokio::spawn(async move {
            // Hitungan respons per route (berurutan).
            let counters: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![0; routes.len()]));
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let rec = rec.clone();
                let routes = routes.clone();
                let counters = counters.clone();
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
                    let path_no_query = path.split('?').next().unwrap_or("").to_string();
                    rec.lock().unwrap().push(Recorded {
                        method: method.clone(),
                        path: path.clone(),
                        body: String::from_utf8_lossy(&body).to_string(),
                    });
                    let route_idx = routes.iter().position(|(m, p, _)| {
                        *m == method && path_no_query.starts_with(p.as_str())
                    });
                    let (status, resp_body) = match route_idx {
                        Some(i) => {
                            let mut c = counters.lock().unwrap();
                            let n = c[i].min(routes[i].2.len().saturating_sub(1));
                            c[i] += 1;
                            routes[i].2[n].clone()
                        }
                        None => (404, r#"{"error":"no mock route"}"#.to_string()),
                    };
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        _ => "Error",
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
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

    fn recorded(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }
}

fn set_env(k: &str, v: &str) {
    // SAFETY: diserialkan via ENV_LOCK oleh setiap tes.
    unsafe { std::env::set_var(k, v) };
}

fn remove_env(k: &str) {
    unsafe { std::env::remove_var(k) };
}

fn ok_true() -> Vec<(u16, String)> {
    vec![(200, r#"{"ok":true,"result":{"message_id":1}}"#.into())]
}

// ---------------------------------------------------------------------------
// Telegram: MarkdownV2 + reply threading + fallback
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telegram_mengirim_markdownv2() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTG/sendMessage".into(),
        ok_true(),
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TG");
    let res = p.send_message("123", "halo **dunia**").await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    // parse_mode MarkdownV2 + markdown terkonversi (** → *).
    assert!(
        rec[0].body.contains(r#""parse_mode":"MarkdownV2""#),
        "body: {}",
        rec[0].body
    );
    assert!(
        rec[0].body.contains(r#"halo *dunia*"#),
        "body: {}",
        rec[0].body
    );
}

#[tokio::test]
async fn telegram_reply_hanya_chunk_pertama() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTG/sendMessage".into(),
        ok_true(),
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TG");
    // 5000 karakter ASCII → 2 chunk (batas 4096 unit UTF-16).
    let res = p.send_message("123:456", &"x".repeat(5000)).await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2, "harusnya 2 chunk");
    assert!(
        rec[0]
            .body
            .contains(r#""reply_parameters":{"message_id":"456"}"#),
        "chunk 0 harus bawa reply_parameters: {}",
        rec[0].body
    );
    assert!(
        !rec[1].body.contains("reply_parameters"),
        "chunk 1 tak boleh bawa reply_parameters: {}",
        rec[1].body
    );
}

#[tokio::test]
async fn telegram_fallback_teks_polos_saat_parse_gagal() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Request pertama: Telegram menolak parse MarkdownV2; kedua: sukses.
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTG/sendMessage".into(),
        vec![
            (
                200,
                r#"{"ok":false,"description":"Bad Request: can't parse entities"}"#.into(),
            ),
            (200, r#"{"ok":true,"result":{"message_id":2}}"#.into()),
        ],
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TG");
    let res = p.send_message("123", "**halo** (x)").await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_ok(), "fallback harus sukses: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2);
    assert!(
        rec[0].body.contains("MarkdownV2"),
        "percobaan 1 MarkdownV2: {}",
        rec[0].body
    );
    // Percobaan 2: tanpa parse_mode, teks polos terkupas.
    assert!(
        !rec[1].body.contains("parse_mode"),
        "fallback tanpa parse_mode: {}",
        rec[1].body
    );
    assert!(
        rec[1].body.contains("halo"),
        "teks polos terkirim: {}",
        rec[1].body
    );
}

// ---------------------------------------------------------------------------
// Discord: reply anchor hanya chunk pertama
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discord_reply_hanya_chunk_pertama() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/channels/chan1/messages".into(),
        vec![(200, r#"{"id":"m1"}"#.into())],
    )])
    .await;
    // DISCORD_API_BASE: cek nama env yang dipakai discord.rs
    set_env("DISCORD_API_BASE", &mock.url());
    let p = DiscordPlatform::new("DCTOKEN");
    let res = p.send_message("chan1:mid9", &"y".repeat(2500)).await;
    remove_env("DISCORD_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2, "2500 char → 2 chunk @2000");
    assert!(
        rec[0]
            .body
            .contains(r#""message_reference":{"message_id":"mid9"}"#),
        "chunk 0 bawa reference: {}",
        rec[0].body
    );
    assert!(
        !rec[1].body.contains("message_reference"),
        "chunk 1 tanpa reference: {}",
        rec[1].body
    );
}

// ---------------------------------------------------------------------------
// Slack: mrkdwn=true + format mrkdwn
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slack_mrkdwn_true_dan_format() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/chat.postMessage".into(),
        vec![(200, r#"{"ok":true,"ts":"1.2"}"#.into())],
    )])
    .await;
    set_env("SLACK_API_BASE", &mock.url());
    let p = SlackPlatform::new("xoxb-mock");
    let res = p.send_message("C999", "**halo** slack").await;
    remove_env("SLACK_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert!(
        rec[0].body.contains(r#""mrkdwn":true"#),
        "body: {}",
        rec[0].body
    );
    assert!(
        rec[0].body.contains(r#"*halo* slack"#),
        "markdown terkonversi: {}",
        rec[0].body
    );
}

// ---------------------------------------------------------------------------
// WhatsApp: context reply hanya chunk pertama
// ---------------------------------------------------------------------------

#[tokio::test]
async fn whatsapp_context_hanya_chunk_pertama() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/PNID123/messages".into(),
        vec![(200, r#"{"messages":[{"id":"wamid.1"}]}"#.into())],
    )])
    .await;
    set_env("WHATSAPP_API_BASE", &mock.url());
    let p = WhatsAppPlatform::new("PNID123", "MOCKWATOKEN");
    // 5000 karakter → 2 chunk @4096.
    let res = p.send_message("62812:wamid.9", &"z".repeat(5000)).await;
    remove_env("WHATSAPP_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2);
    assert!(
        rec[0]
            .body
            .contains(r#""context":{"message_id":"wamid.9"}"#),
        "chunk 0 mengutip: {}",
        rec[0].body
    );
    assert!(
        !rec[1].body.contains("\"context\""),
        "chunk 1 tanpa context: {}",
        rec[1].body
    );
    assert!(
        rec[0].body.contains(r#""preview_url":true"#),
        "preview_url seperti aslinya: {}",
        rec[0].body
    );
}

// ---------------------------------------------------------------------------
// Matrix: formatted_body + relasi reply + split byte
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matrix_formatted_body_terkirim() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "PUT".into(),
        "/_matrix/client/v3/rooms/".into(),
        vec![(200, r#"{"event_id":"$ev1"}"#.into())],
    )])
    .await;
    let p = MatrixPlatform::new(mock.url(), "MOCKTOKEN");
    let res = p.send_message("!room:x", "**halo** matrix").await;
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert!(
        rec[0].body.contains(r#""format":"org.matrix.custom.html""#),
        "body: {}",
        rec[0].body
    );
    assert!(
        rec[0].body.contains("<strong>halo</strong>"),
        "formatted_body: {}",
        rec[0].body
    );
}

#[tokio::test]
async fn matrix_reply_membawa_relasi() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "PUT".into(),
        "/_matrix/client/v3/rooms/".into(),
        vec![(200, r#"{"event_id":"$ev2"}"#.into())],
    )])
    .await;
    let p = MatrixPlatform::new(mock.url(), "MOCKTOKEN");
    let res = p.send_message("!room:x|$ev123", "halo").await;
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert!(
        rec[0]
            .body
            .contains(r#""m.relates_to":{"m.in_reply_to":{"event_id":"$ev123"}}"#),
        "body: {}",
        rec[0].body
    );
}

#[tokio::test]
async fn matrix_split_per_byte_utf8() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "PUT".into(),
        "/_matrix/client/v3/rooms/".into(),
        vec![(200, r#"{"event_id":"$ev3"}"#.into())],
    )])
    .await;
    let p = MatrixPlatform::new(mock.url(), "MOCKTOKEN");
    // 6000 CJK = 18000 byte > 15000 → 2 chunk.
    let res = p.send_message("!room:x", &"日".repeat(6000)).await;
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 2, "harusnya 2 chunk");
    // Teks tiap chunk ≤ 15000 byte UTF-8 (body JSON sendiri membawa
    // formatted_body ganda — seperti aslinya — jadi yang diukur adalah
    // field "body").
    for r in &rec {
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        let body = v.get("body").and_then(|b| b.as_str()).unwrap();
        assert!(
            body.len() <= 15000,
            "chunk {} byte melebihi 15000",
            body.len()
        );
    }
}
