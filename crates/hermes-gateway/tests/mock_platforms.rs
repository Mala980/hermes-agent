#![allow(clippy::await_holding_lock)]
//! Test integrasi platform gateway melawan mock HTTP server lokal.
//!
//! Memverifikasi request/response handling tanpa kredensial asli.
//! Env var diserialkan via ENV_LOCK karena std::env bersifat global per-proses.
use hermes_core::Platform;
use hermes_gateway::{
    run_whatsapp_gateway, serve_matrix, DiscordPlatform, MatrixPlatform, SlackPlatform,
    TeamsPlatform, TelegramPlatform, WhatsAppPlatform,
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    auth: Option<String>,
    body: String,
}

struct MockServer {
    addr: std::net::SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockServer {
    /// routes: (method, path_prefix, status, body_json)
    async fn start(routes: Vec<(String, String, u16, String)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let rec = recorded.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let rec = rec.clone();
                let routes = routes.clone();
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
                    let mut auth = None;
                    for line in lines.by_ref() {
                        if line.is_empty() {
                            break;
                        }
                        if let Some(idx) = line.find(':') {
                            let (name, val) = line.split_at(idx);
                            let val = val[1..].trim();
                            match name.to_lowercase().as_str() {
                                "authorization" => auth = Some(val.to_string()),
                                "content-length" => content_length = val.parse().unwrap_or(0),
                                _ => {}
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
                        auth,
                        body: String::from_utf8_lossy(&body).to_string(),
                    });
                    let route = routes
                        .iter()
                        .find(|(m, p, _, _)| *m == method && path_no_query.starts_with(p.as_str()));
                    let (status, resp_body) = match route {
                        Some((_, _, s, b)) => (*s, b.clone()),
                        None => (404, r#"{"error":"no mock route"}"#.to_string()),
                    };
                    let reason = match status {
                        200 => "OK",
                        400 => "Bad Request",
                        401 => "Unauthorized",
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

    /// Tunggu hingga request cocok tercatat (atau timeout).
    async fn wait_for(
        &self,
        method: &str,
        path_prefix: &str,
        timeout_secs: u64,
    ) -> Option<Recorded> {
        let start = std::time::Instant::now();
        loop {
            {
                let rec = self.recorded.lock().unwrap();
                if let Some(r) = rec.iter().find(|r| {
                    r.method == method
                        && r.path
                            .split('?')
                            .next()
                            .unwrap_or("")
                            .starts_with(path_prefix)
                }) {
                    return Some(r.clone());
                }
            }
            if start.elapsed().as_secs() > timeout_secs {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}

fn set_env(k: &str, v: &str) {
    // Diserialkan via ENV_LOCK di tiap test.
    std::env::set_var(k, v);
}
fn remove_env(k: &str) {
    std::env::remove_var(k);
}

/// Hitung header `X-Hub-Signature-256` (HMAC-SHA256, hex) untuk body webhook.
fn whatsapp_sig_header(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("sha256={hex}")
}

// ---------------------------------------------------------------------------
// Telegram
// ---------------------------------------------------------------------------

#[tokio::test]
async fn telegram_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTESTTOKEN/sendMessage".into(),
        200,
        r#"{"ok":true,"result":{"message_id":42}}"#.into(),
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TESTTOKEN");
    let res = p.send_message("123", "halo mock").await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert!(rec[0].body.contains("halo mock"), "body: {}", rec[0].body);
    assert!(
        rec[0].body.contains("\"chat_id\":\"123\"") || rec[0].body.contains("\"chat_id\":123"),
        "body: {}",
        rec[0].body
    );
}

#[tokio::test]
async fn telegram_send_rejected_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botBADTOKEN/sendMessage".into(),
        200,
        r#"{"ok":false,"description":"bot was blocked by the user"}"#.into(),
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("BADTOKEN");
    let res = p.send_message("123", "halo").await;
    remove_env("TELEGRAM_API_BASE");
    assert!(res.is_err(), "seharusnya error saat ok=false");
}

/// callback_query masuk (tombol inline ditekan) → tepat satu request
/// `answerCallbackQuery` yang membawa `callback_query_id` yang benar.
/// Menjalankan jalur yang sama dipakai `serve_telegram` untuk update non-teks:
/// payload getUpdates → parse_updates → ack_callback_query.
#[tokio::test]
async fn telegram_callback_query_acked() {
    use hermes_gateway::telegram::{ack_callback_query, parse_updates};

    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTESTTOKEN/answerCallbackQuery".into(),
        200,
        r#"{"ok":true,"result":true}"#.into(),
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TESTTOKEN");

    let payload = serde_json::json!({
        "ok": true,
        "result": [
            { "update_id": 77,
              "callback_query": {
                  "id": "cq-abc-123",
                  "from": { "id": 565903951, "username": "chewy" },
                  "data": "menu:refresh",
              } },
        ]
    });
    let updates = parse_updates(&payload);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].callback_query_id.as_deref(), Some("cq-abc-123"));

    ack_callback_query(&p, &updates[0]).await;
    remove_env("TELEGRAM_API_BASE");

    let rec = mock.recorded();
    assert_eq!(
        rec.len(),
        1,
        "tepat satu request answerCallbackQuery, tercatat: {rec:?}"
    );
    assert_eq!(rec[0].method, "POST");
    assert!(
        rec[0].path.contains("/botTESTTOKEN/answerCallbackQuery"),
        "path: {}",
        rec[0].path
    );
    assert!(
        rec[0].body.contains(r#""callback_query_id":"cq-abc-123""#),
        "body: {}",
        rec[0].body
    );
}

/// Update non-teks yang BUKAN callback_query (mis. foto tanpa caption)
/// tidak memicu request `answerCallbackQuery` apa pun.
#[tokio::test]
async fn telegram_non_callback_tidak_diack() {
    use hermes_gateway::telegram::{ack_callback_query, parse_updates};

    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTESTTOKEN/answerCallbackQuery".into(),
        200,
        r#"{"ok":true,"result":true}"#.into(),
    )])
    .await;
    set_env("TELEGRAM_API_BASE", &mock.url());
    let p = TelegramPlatform::new("TESTTOKEN");

    let payload = serde_json::json!({
        "ok": true,
        "result": [
            { "update_id": 78,
              "message": { "message_id": 2,
                           "chat": { "id": 100 },
                           "from": { "id": 555 },
                           "photo": [ { "file_id": "abc" } ] } },
        ]
    });
    let updates = parse_updates(&payload);
    assert_eq!(updates.len(), 1);
    assert!(updates[0].callback_query_id.is_none());

    ack_callback_query(&p, &updates[0]).await;
    remove_env("TELEGRAM_API_BASE");

    assert!(
        mock.recorded().is_empty(),
        "tidak boleh ada request answerCallbackQuery"
    );
}

// ---------------------------------------------------------------------------
// Discord
// ---------------------------------------------------------------------------

#[tokio::test]
async fn discord_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/channels/C123/messages".into(),
        200,
        r#"{"id":"m1"}"#.into(),
    )])
    .await;
    set_env("DISCORD_API_BASE", &mock.url());
    // Token via env (konstruktor DiscordPlatform::new membaca argumen? cek: new(token))
    let p = DiscordPlatform::new("MOCKDISCORDTOKEN");
    let res = p.send_message("C123", "halo discord").await;
    remove_env("DISCORD_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert_eq!(
        rec[0].auth.as_deref(),
        Some("Bot MOCKDISCORDTOKEN"),
        "auth: {:?}",
        rec[0].auth
    );
    assert!(
        rec[0].body.contains("halo discord"),
        "body: {}",
        rec[0].body
    );
}

// ---------------------------------------------------------------------------
// Slack
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slack_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/chat.postMessage".into(),
        200,
        r#"{"ok":true,"ts":"1234.5678"}"#.into(),
    )])
    .await;
    set_env("SLACK_API_BASE", &mock.url());
    let p = SlackPlatform::new("xoxb-mock");
    let res = p.send_message("C999", "halo slack").await;
    remove_env("SLACK_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].auth.as_deref(), Some("Bearer xoxb-mock"));
    assert!(
        rec[0].body.contains("\"channel\":\"C999\""),
        "body: {}",
        rec[0].body
    );
}

// ---------------------------------------------------------------------------
// Matrix: kirim + terima via /sync
// ---------------------------------------------------------------------------

#[tokio::test]
async fn matrix_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "PUT".into(),
        "/_matrix/client/v3/rooms/".into(),
        200,
        r#"{"event_id":"$ev1"}"#.into(),
    )])
    .await;
    let p = MatrixPlatform::new(mock.url(), "MOCKTOKEN");
    let res = p.send_message("!room:x", "halo matrix").await;
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert!(
        rec[0].path.contains("/send/m.room.message/"),
        "path: {}",
        rec[0].path
    );
    assert_eq!(rec[0].auth.as_deref(), Some("Bearer MOCKTOKEN"));
    assert!(rec[0].body.contains("halo matrix"), "body: {}", rec[0].body);
}

#[tokio::test]
async fn matrix_sync_receives_and_replies() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let sync_body = r#"{
        "next_batch": "s2",
        "rooms": {"join": {"!room:x": {"timeline": {"events": [
            {"type": "m.room.message", "sender": "@alice:x", "event_id": "$m1",
             "content": {"msgtype": "m.text", "body": "ping dari alice"}}
        ]}}}}
    }"#;
    let mock = MockServer::start(vec![
        (
            "GET".into(),
            "/_matrix/client/v3/sync".into(),
            200,
            sync_body.into(),
        ),
        (
            "PUT".into(),
            "/_matrix/client/v3/rooms/".into(),
            200,
            r#"{"event_id":"$r1"}"#.into(),
        ),
    ])
    .await;
    let url = mock.url();
    // Allowlist matrix kini fail-closed: izinkan pengirim mock agar jalur
    // terima-dan-balas tetap teruji.
    set_env("MATRIX_ALLOWED_USERS", "@alice:x");
    let task = tokio::spawn(async move {
        serve_matrix(
            url.clone(),
            "MOCKTOKEN".into(),
            "@bot:x".into(),
            |m| async move {
                assert_eq!(m.body, "ping dari alice");
                Ok::<_, hermes_core::HermesError>("pong untuk alice".into())
            },
        )
        .await
    });
    // Tunggu balasan terkirim ke mock.
    let reply = mock.wait_for("PUT", "/_matrix/client/v3/rooms/", 15).await;
    task.abort();
    remove_env("MATRIX_ALLOWED_USERS");
    let reply = reply.expect("balasan matrix tidak terkirim ke mock");
    assert!(
        reply.body.contains("pong untuk alice"),
        "body: {}",
        reply.body
    );
}

// ---------------------------------------------------------------------------
// Teams: kirim via Bot Framework (token + activities di-mock)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn teams_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![
        (
            "POST".into(),
            "/token".into(),
            200,
            r#"{"access_token":"MOCKTEAMSTOKEN","expires_in":3600,"token_type":"Bearer"}"#.into(),
        ),
        (
            "POST".into(),
            "/v3/conversations/conv1/activities".into(),
            200,
            r#"{"id":"act1"}"#.into(),
        ),
    ])
    .await;
    set_env("TEAMS_APP_ID", "mock-app-id");
    set_env("TEAMS_APP_PASSWORD", "mock-secret");
    set_env("TEAMS_TOKEN_URL", &format!("{}/token", mock.url()));
    let p = TeamsPlatform::from_env().expect("from_env");
    let target = format!("{}|conv1", mock.url());
    let res = p.send_message(&target, "halo teams").await;
    remove_env("TEAMS_APP_ID");
    remove_env("TEAMS_APP_PASSWORD");
    remove_env("TEAMS_TOKEN_URL");
    // Mock HTTP lokal bukan https -> serviceUrl wajib ditolak SEBELUM request
    // apa pun (termasuk pengambilan token) dikirim.
    let err = res.expect_err("http mock harus ditolak validator serviceUrl");
    let msg = err.to_string();
    assert!(
        msg.contains("blocked serviceUrl host") && msg.contains("127.0.0.1"),
        "pesan tak terduga: {msg}"
    );
    assert!(
        mock.recorded().is_empty(),
        "tidak boleh ada request keluar ke host yang diblokir"
    );
}

// ---------------------------------------------------------------------------
// WhatsApp: kirim + webhook verify + terima pesan
// ---------------------------------------------------------------------------

#[tokio::test]
async fn whatsapp_send_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/PNID123/messages".into(),
        200,
        r#"{"messages":[{"id":"wamid.1"}]}"#.into(),
    )])
    .await;
    set_env("WHATSAPP_API_BASE", &mock.url());
    let p = WhatsAppPlatform::new("PNID123", "MOCKWATOKEN");
    let res = p.send_message("62812", "halo whatsapp").await;
    remove_env("WHATSAPP_API_BASE");
    assert!(res.is_ok(), "send gagal: {res:?}");
    let rec = mock.recorded();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].auth.as_deref(), Some("Bearer MOCKWATOKEN"));
    assert!(
        rec[0].body.contains("halo whatsapp"),
        "body: {}",
        rec[0].body
    );
}

#[tokio::test]
async fn whatsapp_webhook_verify_and_receive() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Mock untuk pengiriman balasan.
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/PNID123/messages".into(),
        200,
        r#"{"messages":[{"id":"wamid.2"}]}"#.into(),
    )])
    .await;
    set_env("WHATSAPP_API_BASE", &mock.url());
    // App secret untuk verifikasi HMAC webhook (fail-closed bila tidak diset).
    set_env("WHATSAPP_APP_SECRET", "TESTAPPSECRET");

    let port: u16 = 18765;
    let server = tokio::spawn(async move {
        run_whatsapp_gateway(
            "PNID123".into(),
            "MOCKWATOKEN".into(),
            "VERIFYME".into(),
            port,
            |m| async move {
                assert_eq!(m.from, "62812");
                assert_eq!(m.text, "halo dari meta");
                Ok::<_, hermes_core::HermesError>("balasan wa!".into())
            },
        )
        .await
    });
    // Tunggu server siap.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let client = reqwest::Client::new();

    // 1. Verifikasi Meta (token benar).
    let resp = client
        .get(format!("http://127.0.0.1:{port}/webhook?hub.mode=subscribe&hub.verify_token=VERIFYME&hub.challenge=CHALLENGE123"))
        .send()
        .await
        .expect("GET verify");
    assert_eq!(resp.text().await.unwrap(), "CHALLENGE123");

    // 2. Verifikasi dengan token salah → 403.
    let resp = client
        .get(format!("http://127.0.0.1:{port}/webhook?hub.mode=subscribe&hub.verify_token=SALAH&hub.challenge=X"))
        .send()
        .await
        .expect("GET verify salah");
    assert_eq!(resp.status().as_u16(), 403);

    // 3. Pesan masuk tanpa signature → 403 (body tidak diproses).
    let payload = serde_json::json!({
        "entry": [{"changes": [{"value": {"messages": [
            {"type": "text", "from": "62812", "text": {"body": "halo dari meta"}}
        ]}}]}]
    });
    let body_bytes = serde_json::to_vec(&payload).unwrap();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/webhook"))
        .header("content-type", "application/json")
        .body(body_bytes.clone())
        .send()
        .await
        .expect("POST webhook tanpa signature");
    assert_eq!(resp.status().as_u16(), 403);

    // 4. Signature salah → 403.
    let resp = client
        .post(format!("http://127.0.0.1:{port}/webhook"))
        .header("content-type", "application/json")
        .header(
            "X-Hub-Signature-256",
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .body(body_bytes.clone())
        .send()
        .await
        .expect("POST webhook signature salah");
    assert_eq!(resp.status().as_u16(), 403);

    // 5. Signature valid → handler → balasan terkirim ke mock.
    let sig = whatsapp_sig_header("TESTAPPSECRET", &body_bytes);
    let resp = client
        .post(format!("http://127.0.0.1:{port}/webhook"))
        .header("content-type", "application/json")
        .header("X-Hub-Signature-256", sig)
        .body(body_bytes)
        .send()
        .await
        .expect("POST webhook");
    assert!(resp.status().is_success());
    let reply = mock.wait_for("POST", "/PNID123/messages", 10).await;
    server.abort();
    remove_env("WHATSAPP_API_BASE");
    remove_env("WHATSAPP_APP_SECRET");
    let reply = reply.expect("balasan whatsapp tidak terkirim ke mock");
    assert!(reply.body.contains("balasan wa!"), "body: {}", reply.body);
}

#[tokio::test]
async fn whatsapp_webhook_503_when_app_secret_unset() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Fail-closed: secret tidak dikonfigurasi → 503, body tidak diproses.
    remove_env("WHATSAPP_APP_SECRET");
    let port: u16 = 18766;
    let server = tokio::spawn(async move {
        run_whatsapp_gateway(
            "PNID123".into(),
            "MOCKWATOKEN".into(),
            "VERIFYME".into(),
            port,
            |_m| async move { Ok::<_, hermes_core::HermesError>("x".into()) },
        )
        .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/webhook"))
        .header("content-type", "application/json")
        .body(r#"{"entry":[]}"#)
        .send()
        .await
        .expect("POST webhook tanpa secret");
    assert_eq!(resp.status().as_u16(), 503);
    server.abort();
}
