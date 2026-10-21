//! Live test Telegram melawan Bot API asli.
//!
//! Memakai surrogate kredensial `custom.telegram` dari authd (unix socket).
//! Surrogate disisipkan MENTAH ke path URL; egress proxy menukarnya dengan
//! token asli. Dijalankan hanya dengan:
//!
//!     HERMES_TEST_LIVE_TELEGRAM=1 cargo test -p hermes-gateway --test live_telegram
//!
//! Prasyarat: sudah ada minimal satu pesan masuk ke bot (kirim pesan apa saja
//! ke bot), karena test membaca chat_id dari getUpdates.
use hermes_core::Platform;
use hermes_gateway::TelegramPlatform;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Ambil surrogate `access_token` untuk custom.telegram dari authd.
async fn fetch_surrogate() -> String {
    let sock_path = std::env::var("JARVIS_AUTHD_SOCK")
        .unwrap_or_else(|_| "/run/hatch/auth/authd.sock".to_string());
    let mut sock = tokio::net::UnixStream::connect(&sock_path)
        .await
        .expect("tidak bisa konek ke authd.sock");
    let body = r#"{"name":"custom.telegram"}"#;
    let req = format!(
        "POST /v1/credentials/surrogate HTTP/1.1\r\nHost: authd.local\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    sock.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.unwrap();
    let text = String::from_utf8_lossy(&raw);
    let json_part = text.find("\r\n\r\n").map(|i| &text[i + 4..]).unwrap_or("");
    let v: Value = serde_json::from_str(json_part).expect("respons authd bukan JSON");
    v.get("credentials")
        .and_then(|c| c.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("access_token"))
        })
        .and_then(|e| e.get("surrogate"))
        .and_then(|s| s.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| s.starts_with("hsurr:"))
        .expect("surrogate access_token tidak ditemukan")
}

fn api_url(surrogate: &str, method: &str) -> String {
    // MENTAH tanpa quote: proxy mencocokkan bentuk hsurr:... apa adanya.
    format!("https://api.telegram.org/bot{surrogate}/{method}")
}

/// Samarkan surrogate di pesan error agar tidak bocor ke log.
fn redact(msg: &str) -> String {
    let mut out = msg.to_string();
    let mut start = 0;
    while let Some(i) = out[start..].find("hsurr:") {
        let abs = start + i + 6;
        let end = out[abs..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
            .map(|e| abs + e)
            .unwrap_or(out.len());
        out.replace_range(abs..end, "***");
        start = abs + 3;
    }
    out
}

async fn tg_post(surrogate: &str, method: &str, params: &Value) -> Value {
    let client = reqwest::Client::new();
    let resp = client
        .post(api_url(surrogate, method))
        .json(params)
        .send()
        .await
        .map_err(|e| redact(&e.to_string()))
        .expect("request gagal");
    resp.json::<Value>()
        .await
        .map_err(|e| redact(&e.to_string()))
        .expect("respons bukan JSON")
}

#[tokio::test]
async fn live_telegram_end_to_end() {
    if std::env::var("HERMES_TEST_LIVE_TELEGRAM").is_err() {
        return;
    }
    // Pastikan tidak kena mock.
    std::env::remove_var("TELEGRAM_API_BASE");

    let surrogate = fetch_surrogate().await;

    // 1. getMe lewat kode Rust? getMe tidak diekspos platform; panggil langsung.
    let me = tg_post(&surrogate, "getMe", &Value::Null).await;
    assert_eq!(
        me.get("ok").and_then(|o| o.as_bool()),
        Some(true),
        "getMe: {me}"
    );
    let username = me
        .pointer("/result/username")
        .and_then(|u| u.as_str())
        .unwrap_or("?");
    println!("bot: @{username}");

    // 2. Ambil chat_id dari pesan masuk terakhir.
    let updates = tg_post(&surrogate, "getUpdates", &serde_json::json!({"limit": 10})).await;
    let chat_id = updates
        .get("result")
        .and_then(|r| r.as_array())
        .and_then(|arr| arr.iter().rev().find_map(|u| u.pointer("/message/chat/id")))
        .and_then(|id| id.as_i64())
        .expect("tidak ada pesan masuk; kirim dulu pesan apa saja ke bot, lalu ulangi test")
        .to_string();
    println!("chat_id: {chat_id}");

    // 3. Kirim balasan MELALUI kode Rust TelegramPlatform (jalur yang diuji).
    //    Hapus env override agar memakai api.telegram.org asli.
    let platform = TelegramPlatform::new(surrogate.clone());
    platform
        .send_message(
            &chat_id,
            "Tes live hermes-rs: kode Rust berhasil mengirim via Bot API asli.",
        )
        .await
        .map_err(|e| redact(&e.to_string()))
        .expect("send_message via TelegramPlatform gagal");

    // 4. Verifikasi pesan benar-benar terkirim (baca kembali via getUpdates? tidak;
    //    cukup pastikan tidak error — Telegram mengembalikan ok:false sebagai error
    //    dan itu sudah ditangani platform).
    println!("OK: pesan terkirim melalui TelegramPlatform ke chat {chat_id}");
}
