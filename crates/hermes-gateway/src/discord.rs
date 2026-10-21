//! Platform Discord: kirim via REST API, terima via Gateway WebSocket.
//!
//! Bot Discord perlu intent `MESSAGE_CONTENT` (privileged) untuk membaca isi pesan.
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hermes_core::{HermesError, Platform, Result};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

use crate::allowlist::{env_truthy, read_allow_csv};
use crate::split::{split_message, DISCORD_MESSAGE_LIMIT};

/// Base URL Discord API; bisa di-override via DISCORD_API_BASE untuk pengujian.
fn api() -> String {
    std::env::var("DISCORD_API_BASE").unwrap_or_else(|_| "https://discord.com/api/v10".to_string())
}
const GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Kapasitas cache dedup pesan masuk (FIFO): replay setelah resume
/// tidak pernah melebihi beberapa ratus event.
const DEDUP_CAPACITY: usize = 1000;
/// Batas atas backoff reconnect gateway: 60 detik.
const RECONNECT_BACKOFF_CAP_SECS: u64 = 60;
/// Koneksi yang bertahan minimal selama ini dianggap sehat: hitungan
/// backoff di-reset agar flap berulang tidak terjebak di backoff panjang.
const BACKOFF_RESET_AFTER_SECS: u64 = 60;

/// Intent gateway Discord: GUILDS(1) + GUILD_MESSAGES(512) +
/// MESSAGE_CONTENT(32768) + DM_MESSAGES(4096). Bit DM_MESSAGES (4096)
/// diperlukan agar pesan DM ikut diterima, seperti gateway aslinya.
const DISCORD_INTENTS: u64 = 33281 | 4096; // = 37377

pub struct DiscordPlatform {
    token: String,
    client: reqwest::Client,
}

impl DiscordPlatform {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            client: reqwest::Client::new(),
        }
    }

    fn auth(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("Authorization", format!("Bot {}", self.token))
    }
}

#[derive(Debug, Clone)]
pub struct DiscordMessage {
    pub channel_id: String,
    /// Username penulis (untuk tampilan/log).
    pub author: String,
    /// Discord user ID penulis (snowflake); dipakai untuk allowlist.
    pub author_id: String,
    pub content: String,
    /// Balas sebagai reply ke pesan ini (opsional).
    pub message_id: String,
}

/// Bangun payload JSON untuk satu chunk pengiriman pesan Discord.
///
/// `allowed_mentions: {"parse": ["users"]}` SELALU disetel: ping @everyone,
/// @here, dan role selalu dimatikan (seperti default aman gateway aslinya
/// `AllowedMentions(everyone=False, roles=False)`), tetapi ping pengguna
/// individu tetap diizinkan (`users=True`) agar balasan agen bisa menyebut
/// pengguna yang diajak bicara.
fn build_send_body(chunk: &str, reply_to: Option<&str>) -> Value {
    let mut body = json!({
        "content": chunk,
        "allowed_mentions": { "parse": ["users"] },
    });
    if let Some(mid) = reply_to {
        body["message_reference"] = json!({ "message_id": mid });
    }
    body
}

#[async_trait]
impl Platform for DiscordPlatform {
    fn id(&self) -> &str {
        "discord"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: channel id (atau "channel_id:message_id" untuk reply).
        let (channel_id, reply_to) = match target.split_once(':') {
            Some((c, m)) => (c, Some(m)),
            None => (target, None),
        };
        // Anchor balasan mengikuti reply_to_mode ("off"/"first"/"all",
        // default "first"): hanya chunk pertama membawa message_reference —
        // cermin `_reply_reference_for_send` aslinya.
        let mode = crate::threading::ReplyThreadMode::from_env("DISCORD_REPLY_TO_MODE");
        // Batas Discord 2000 karakter; pecah sadar-pagar agar blok ``` utuh.
        for (i, chunk) in split_message(text, Some(DISCORD_MESSAGE_LIMIT))
            .iter()
            .enumerate()
        {
            let anchor = if mode.applies(reply_to.is_some(), i) {
                reply_to
            } else {
                None
            };
            let body = build_send_body(chunk, anchor);
            // Bungkus sebagai closure agar request identik bisa dikirim ulang
            // bila API menjawab 429 (lihat crate::rate_limit).
            let send = || async {
                self.auth(
                    self.client
                        .post(format!("{}/channels/{channel_id}/messages", api())),
                )
                .json(&body)
                .send()
                .await
                .map_err(|e| HermesError::Gateway(format!("discord send gagal: {e}")))
            };
            let resp = send().await?;
            // 429 → tidur sesuai Retry-After/retry_after (cap 60 dtk), retry sekali.
            let resp = crate::rate_limit::handle_429("discord", resp, send).await?;
            if !resp.status().is_success() {
                let b = resp.text().await.unwrap_or_default();
                return Err(HermesError::Gateway(format!("discord menolak pesan: {b}")));
            }
        }
        Ok(())
    }
}

/// Daftar ID pengguna Discord yang diizinkan: gabungan (union)
/// `DISCORD_ALLOWED_USERS` dan `GATEWAY_ALLOWED_USERS` (dipisah koma;
/// `"*"` = semua pengguna). Mengembalikan `None` bila keduanya kosong →
/// semua pengguna ditolak (fail-closed).
pub fn discord_allowlist() -> Option<std::collections::HashSet<String>> {
    let mut ids = read_allow_csv("DISCORD_ALLOWED_USERS");
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Saklar "izinkan semua": `DISCORD_ALLOW_ALL_USERS` (khusus dev) atau
/// `GATEWAY_ALLOW_ALL_USERS` bernilai true/1/yes.
pub fn discord_allow_all() -> bool {
    env_truthy("DISCORD_ALLOW_ALL_USERS") || env_truthy("GATEWAY_ALLOW_ALL_USERS")
}

/// Keputusan izin murni (mudah diunit-tes): `user_id` adalah Discord user ID
/// (snowflake string). Urutan: allow-all dulu, lalu allowlist, default tolak.
pub fn is_discord_user_allowed(
    user_id: &str,
    allowlist: &Option<std::collections::HashSet<String>>,
) -> bool {
    if discord_allow_all() {
        return true;
    }
    match allowlist {
        None => false,
        Some(set) => set.contains("*") || set.contains(user_id),
    }
}

/// Cache dedup ID pesan masuk: bounded, FIFO eviction.
///
/// Discord me-replay event setelah reconnect/resume; ID yang sudah diproses
/// harus dilewati agar pesan tidak dijawab dua kali (seperti
/// `MessageDeduplicator` pada gateway aslinya).
struct MessageDedup {
    ids: std::collections::HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl MessageDedup {
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

/// Loop Gateway Discord: terima MESSAGE_CREATE → handler → kirim balasan.
pub async fn serve_discord<F, Fut>(token: String, mut handler: F) -> Result<()>
where
    F: FnMut(DiscordMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let allowlist = discord_allowlist();
    if discord_allow_all() {
        tracing::warn!(
            "allowlist discord: DINONAKTIFKAN via *_ALLOW_ALL_USERS (semua pengguna diizinkan)"
        );
    }
    match &allowlist {
        Some(set) if set.contains("*") => {
            tracing::info!("allowlist discord: semua pengguna diizinkan (*)")
        }
        Some(set) => tracing::info!("allowlist discord: {} pengguna diizinkan", set.len()),
        None => tracing::warn!(
            "DISCORD_ALLOWED_USERS tidak diset: semua pesan masuk akan ditolak (fail-closed)"
        ),
    }
    // Dedup bertahan lintas reconnect: replay setelah resume dilewati.
    let mut dedup = MessageDedup::new(DEDUP_CAPACITY);
    let mut attempt: u32 = 0;
    loop {
        // Supervisi: setiap sesi koneksi yang putus dihubungkan ulang dengan
        // backoff eksponensial berjitter, tanpa batas (indefinite).
        let up_since = tokio::time::Instant::now();
        match run_gateway_session(&token, &mut handler, &allowlist, &mut dedup).await {
            Ok(()) => return Ok(()), // sesi berakhir normal (saat ini tidak pernah terjadi)
            Err(e) => {
                if up_since.elapsed() >= Duration::from_secs(BACKOFF_RESET_AFTER_SECS) {
                    attempt = 0; // koneksi sempat sehat → reset hitungan backoff
                }
                let delay = backoff_delay(attempt, jitter01());
                attempt = attempt.saturating_add(1);
                tracing::warn!(
                    "gateway discord terputus ({e}); reconnect dalam {delay:?} (upaya #{attempt})"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Satu sesi koneksi gateway: connect → HELLO → IDENTIFY → heartbeat +
/// dispatch event. Mengembalikan `Err` pada setiap kegagalan koneksi agar
/// [`serve_discord`] bisa menghubungkan ulang; state dedup dipertahankan
/// oleh pemanggil lintas sesi.
async fn run_gateway_session<F, Fut>(
    token: &str,
    handler: &mut F,
    allowlist: &Option<std::collections::HashSet<String>>,
    dedup: &mut MessageDedup,
) -> Result<()>
where
    F: FnMut(DiscordMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let (ws, _) = connect_async(GATEWAY)
        .await
        .map_err(|e| HermesError::Gateway(format!("discord gateway connect gagal: {e}")))?;
    let (mut write, mut read) = ws.split();
    tracing::info!("gateway discord terhubung, menunggu HELLO...");

    // Tunggu HELLO untuk dapat heartbeat_interval.
    let hello = read
        .next()
        .await
        .ok_or_else(|| HermesError::Gateway("discord gateway menutup koneksi".into()))?
        .map_err(|e| HermesError::Gateway(format!("ws error: {e}")))?;
    let hello_v: Value = serde_json::from_str(hello.to_text().unwrap_or("{}"))
        .map_err(|e| HermesError::Gateway(format!("hello invalid: {e}")))?;
    let interval_ms = hello_v
        .pointer("/d/heartbeat_interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(41250);

    // IDENTIFY.
    write
        .send(WsMessage::Text(
            json!({
                "op": 2,
                "d": {
                    "token": token,
                    "intents": DISCORD_INTENTS,
                    "properties": { "os": "linux", "browser": "hermes-rs", "device": "hermes-rs" }
                }
            })
            .to_string()
            .into(),
        ))
        .await
        .map_err(|e| HermesError::Gateway(format!("identify gagal: {e}")))?;

    // Heartbeat loop.
    let (hb_tx, mut hb_rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
        loop {
            ticker.tick().await;
            if hb_tx.send(()).await.is_err() {
                break;
            }
        }
    });

    let platform = DiscordPlatform::new(token.to_owned());
    let mut seq: Option<u64> = None;

    loop {
        tokio::select! {
            _ = hb_rx.recv() => {
                let payload = json!({ "op": 1, "d": seq });
                if write.send(WsMessage::Text(payload.to_string().into())).await.is_err() {
                    return Err(HermesError::Gateway("heartbeat gagal".into()));
                }
            }
            msg = read.next() => {
                let Some(msg) = msg else {
                    return Err(HermesError::Gateway("discord gateway terputus".into()));
                };
                let msg = msg.map_err(|e| HermesError::Gateway(format!("ws error: {e}")))?;
                let text = match &msg {
                    WsMessage::Text(t) => t.to_string(),
                    WsMessage::Close(_) => return Err(HermesError::Gateway("gateway close".into())),
                    _ => continue,
                };
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if let Some(s) = v.get("s").and_then(|x| x.as_u64()) {
                    seq = Some(s);
                }
                let op = v.get("op").and_then(|o| o.as_u64()).unwrap_or(0);
                if op == 11 {
                    continue; // HEARTBEAT_ACK
                }
                if op != 0 {
                    continue;
                }
                let ev = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
                if ev == "READY" {
                    tracing::info!("discord READY");
                    continue;
                }
                if ev != "MESSAGE_CREATE" {
                    continue;
                }
                let d = v.get("d").unwrap_or(&Value::Null);
                let message_id = d
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                // Replay setelah reconnect: ID yang sudah diproses dilewati
                // agar pesan tidak dijawab dua kali.
                if dedup.is_duplicate(&message_id) {
                    tracing::debug!(message_id = %message_id, "pesan discord duplikat, dilewati");
                    continue;
                }
                // Abaikan pesan dari bot (termasuk diri sendiri).
                if d.pointer("/author/bot").and_then(|b| b.as_bool()).unwrap_or(false) {
                    continue;
                }
                let content = d.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                if content.trim().is_empty() {
                    continue;
                }
                let dm = DiscordMessage {
                    channel_id: d.get("channel_id").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                    author: d.pointer("/author/username").and_then(|u| u.as_str()).unwrap_or("?").to_string(),
                    author_id: d.pointer("/author/id").and_then(|i| i.as_str()).unwrap_or("").to_string(),
                    content,
                    message_id,
                };
                // Kursor seq sudah dimajukan untuk setiap event di atas, jadi
                // pesan yang dilewati di sini tetap ter-ack (tidak diproses ulang).
                if !is_discord_user_allowed(&dm.author_id, allowlist) {
                    tracing::warn!(user_id = %dm.author_id, author = %dm.author, "pesan discord dari pengguna tak diizinkan, dilewati");
                    continue;
                }
                tracing::info!(channel = %dm.channel_id, author = %dm.author, "pesan discord masuk");
                match handler(dm.clone()).await {
                    Ok(reply) => {
                        let target = format!("{}:{}", dm.channel_id, dm.message_id);
                        if let Err(e) = platform.send_message(&target, &reply).await {
                            tracing::warn!("gagal membalas discord: {e}");
                        }
                    }
                    Err(e) => {
                        let target = format!("{}:{}", dm.channel_id, dm.message_id);
                        let _ = platform.send_message(&target, &format!("Maaf, terjadi error: {e}")).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{clear_auth_env, restore_auth_env, set_var, ENV_LOCK};

    const AUTH_VARS: [&str; 4] = [
        "DISCORD_ALLOWED_USERS",
        "DISCORD_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    #[test]
    fn allowlist_kosong_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        assert!(discord_allowlist().is_none());
        assert!(!discord_allow_all());
        assert!(!is_discord_user_allowed("123456789", &discord_allowlist()));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_parse_koma_dan_spasi() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("DISCORD_ALLOWED_USERS", Some("  111 , 222,,"));
        let allow = Some(discord_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_discord_user_allowed("111", &allow));
        assert!(is_discord_user_allowed("222", &allow));
        assert!(!is_discord_user_allowed("333", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_wildcard_buka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("DISCORD_ALLOWED_USERS", Some("*"));
        let allow = Some(discord_allowlist().expect("harus ter-parse"));
        assert!(is_discord_user_allowed("siapa-saja-123", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_id_tak_dikenal_ditolak() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("DISCORD_ALLOWED_USERS", Some("111"));
        let allow = Some(discord_allowlist().expect("harus ter-parse"));
        assert!(!is_discord_user_allowed("999", &allow));
        assert!(!is_discord_user_allowed("", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allowlist_gateway_users_gabungan() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("DISCORD_ALLOWED_USERS", Some("111"));
        set_var("GATEWAY_ALLOWED_USERS", Some("222"));
        let allow = Some(discord_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        assert!(is_discord_user_allowed("111", &allow));
        assert!(is_discord_user_allowed("222", &allow));
        assert!(!is_discord_user_allowed("333", &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn allow_all_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("DISCORD_ALLOW_ALL_USERS", Some("true"));
        assert!(discord_allow_all());
        assert!(is_discord_user_allowed("siapa-saja", &None));
        restore_auth_env(saved);
    }

    #[test]
    fn intents_mencakup_dm_tanpa_mengubah_bit_lain() {
        // DM_MESSAGES = bit 4096; bit lain harus sama seperti semula (33281).
        assert_eq!(DISCORD_INTENTS, 37377);
        assert_eq!(DISCORD_INTENTS & 4096, 4096);
        assert_eq!(DISCORD_INTENTS & !4096, 33281);
    }

    #[test]
    fn dedup_id_baru_diproses_duplikat_dibuang() {
        let mut d = MessageDedup::new(1000);
        assert!(!d.is_duplicate("111")); // baru → diproses
        assert!(d.is_duplicate("111")); // duplikat → buang
        assert!(!d.is_duplicate("222")); // id lain → diproses
        assert!(d.is_duplicate("222"));
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn dedup_id_kosong_tidak_pernah_didedup() {
        let mut d = MessageDedup::new(10);
        assert!(!d.is_duplicate(""));
        assert!(!d.is_duplicate(""));
        assert_eq!(d.len(), 0);
    }

    #[test]
    fn dedup_eviksi_fifo_saat_penuh() {
        let mut d = MessageDedup::new(3);
        assert!(!d.is_duplicate("a"));
        assert!(!d.is_duplicate("b"));
        assert!(!d.is_duplicate("c"));
        assert_eq!(d.len(), 3);
        // "d" mengusir "a" (yang paling lama, FIFO).
        assert!(!d.is_duplicate("d"));
        assert_eq!(d.len(), 3);
        assert!(d.is_duplicate("b"));
        assert!(d.is_duplicate("c"));
        assert!(d.is_duplicate("d"));
        assert!(!d.is_duplicate("a")); // "a" ter-eviksi → dianggap baru lagi
    }

    #[test]
    fn payload_kirim_selalu_batasi_mention() {
        // Reply: guard ada + message_reference tetap utuh.
        let body = build_send_body("halo @everyone dan @here", Some("mid123"));
        assert_eq!(body["content"], json!("halo @everyone dan @here"));
        // Pengguna boleh di-ping (users=True seperti aslinya); everyone/here/role
        // selalu dimatikan sehingga @everyone/@here di konten tidak mass-ping.
        assert_eq!(body["allowed_mentions"], json!({"parse": ["users"]}));
        assert_eq!(body["message_reference"]["message_id"], json!("mid123"));
        // Tanpa reply: guard tetap ada, tanpa message_reference.
        let body2 = build_send_body("tanpa reply", None);
        assert_eq!(body2["allowed_mentions"], json!({"parse": ["users"]}));
        assert!(body2.get("message_reference").is_none());
    }

    #[test]
    fn reply_anchor_hanya_chunk_pertama_default_first() {
        // Cermin reply_to_mode="first" (default) aslinya: anchor balasan
        // hanya dibawa chunk pertama; "all" membawa di semua chunk.
        use crate::threading::ReplyThreadMode;
        let reply_to = Some("mid123");
        for (mode, idx, want_anchor) in [
            (ReplyThreadMode::First, 0, true),
            (ReplyThreadMode::First, 1, false),
            (ReplyThreadMode::First, 2, false),
            (ReplyThreadMode::All, 0, true),
            (ReplyThreadMode::All, 1, true),
            (ReplyThreadMode::Off, 0, false),
        ] {
            let anchor = if mode.applies(reply_to.is_some(), idx) {
                reply_to
            } else {
                None
            };
            let body = build_send_body("chunk", anchor);
            assert_eq!(
                body.get("message_reference").is_some(),
                want_anchor,
                "mode={mode:?} idx={idx}"
            );
        }
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
}
