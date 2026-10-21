//! Platform Signal via `signal-cli` (subproses).
//!
//! Butuh `signal-cli` terinstal dan terdaftar. Env: `SIGNAL_SENDER` (nomor
//! pengirim, mis. +6281234567890), `SIGNAL_CLI` (path biner, default
//! `signal-cli`), `SIGNAL_POLL_SECS` (default 10),
//! `SIGNAL_GROUP_ALLOWED_USERS` (ID *grup* — `groupId` signal-cli — yang boleh
//! memicu balasan untuk pesan *grup*, dipisah koma; `"*"` = semua grup;
//! kosong/tak diset = pesan grup dilewati/fail-closed; DM tidak terpengaruh).
//! NOTE: kunci izin adalah groupId, BUKAN nomor pengirim — menyamai gateway
//! aslinya (`_group_allowed(group_id)`). Sengaja tidak ada fallback ke
//! sender-ID.
use async_trait::async_trait;
use hermes_core::{HermesError, Platform, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::future::Future;

use crate::allowlist::{env_truthy, read_allow_csv};
use crate::split::{split_message, SIGNAL_MESSAGE_LIMIT};

fn cli_bin() -> String {
    std::env::var("SIGNAL_CLI").unwrap_or_else(|_| "signal-cli".to_string())
}

fn sender() -> Result<String> {
    std::env::var("SIGNAL_SENDER")
        .map_err(|_| HermesError::Gateway("set SIGNAL_SENDER (nomor signal-cli)".into()))
}

pub struct SignalPlatform;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalMessage {
    pub from: String,
    pub text: String,
    pub timestamp: i64,
    /// Terisi bila pesan berasal dari grup (`groupInfo` ada di `dataMessage`).
    pub group_id: Option<String>,
}

/// Daftar ID *grup* Signal (`groupId` dari signal-cli) yang diizinkan memicu
/// balasan untuk pesan *grup*: gabungan (union) `SIGNAL_GROUP_ALLOWED_USERS`
/// dan `GATEWAY_ALLOWED_USERS` (dipisah koma; `"*"` = semua grup).
/// Mengembalikan `None` bila keduanya kosong → pesan grup dilewati semua
/// (fail-closed). DM tidak terpengaruh.
pub fn signal_group_allowlist() -> Option<HashSet<String>> {
    let mut ids = read_allow_csv("SIGNAL_GROUP_ALLOWED_USERS");
    ids.extend(read_allow_csv("GATEWAY_ALLOWED_USERS"));
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

/// Saklar "izinkan semua pesan grup": `SIGNAL_ALLOW_ALL_USERS` (khusus dev)
/// atau `GATEWAY_ALLOW_ALL_USERS` bernilai true/1/yes.
pub fn signal_group_allow_all() -> bool {
    env_truthy("SIGNAL_ALLOW_ALL_USERS") || env_truthy("GATEWAY_ALLOW_ALL_USERS")
}

/// Keputusan izin pesan grup murni (mudah diunit-tes): `group_id` adalah
/// `groupId` signal-cli dari pesan grup. Urutan: allow-all dulu, lalu
/// allowlist, default tolak. NOTE: kunci izin adalah groupId SEMATA —
/// menyamai gateway aslinya; nomor pengirim (`msg.from`) sengaja tidak
/// diperiksa di sini.
pub fn is_signal_group_allowed(group_id: &str, allowlist: &Option<HashSet<String>>) -> bool {
    if signal_group_allow_all() {
        return true;
    }
    match allowlist {
        None => false,
        Some(set) => set.contains("*") || set.contains(group_id),
    }
}

/// Keputusan pemrosesan satu pesan masuk murni (mudah diunit-tes): DM selalu
/// diproses (perilaku lama tidak berubah); pesan grup (`groupInfo` ada) hanya
/// diproses bila groupId-nya diizinkan `signal_group_allowlist()`.
pub fn is_signal_message_allowed(
    msg: &SignalMessage,
    group_allowlist: &Option<HashSet<String>>,
) -> bool {
    match &msg.group_id {
        None => true,
        Some(gid) => is_signal_group_allowed(gid, group_allowlist),
    }
}

#[async_trait]
impl Platform for SignalPlatform {
    fn id(&self) -> &str {
        "signal"
    }

    async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        // target: nomor telepon penerima (DM). Batas Signal 8000 karakter;
        // pesan lebih panjang dikirim sebagai beberapa pesan berurutan.
        for chunk in split_message(text, Some(SIGNAL_MESSAGE_LIMIT)) {
            let out = tokio::process::Command::new(cli_bin())
                .args(["-u", &sender()?, "send", "-m", &chunk, target])
                .output()
                .await
                .map_err(|e| HermesError::Gateway(format!("signal-cli gagal dijalankan: {e}")))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(HermesError::Gateway(format!(
                    "signal-cli send gagal: {err}"
                )));
            }
        }
        Ok(())
    }
}

impl SignalPlatform {
    /// Kirim balasan ke *grup* Signal (`signal-cli send -g <groupId>`), agar
    /// konteks percakapan grup tidak bocor sebagai DM ke pengirim.
    pub async fn send_group_message(&self, group_id: &str, text: &str) -> Result<()> {
        // Sama seperti DM: pecah sadar-pagar pada batas 8000 karakter.
        for chunk in split_message(text, Some(SIGNAL_MESSAGE_LIMIT)) {
            let out = tokio::process::Command::new(cli_bin())
                .args(["-u", &sender()?, "send", "-g", group_id, "-m", &chunk])
                .output()
                .await
                .map_err(|e| HermesError::Gateway(format!("signal-cli gagal dijalankan: {e}")))?;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                return Err(HermesError::Gateway(format!(
                    "signal-cli send grup gagal: {err}"
                )));
            }
        }
        Ok(())
    }
}

/// Loop receive: panggil `signal-cli receive --json` berkala → handler → balas.
pub async fn serve_signal<F, Fut>(mut handler: F) -> Result<()>
where
    F: FnMut(SignalMessage) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let poll_secs: u64 = std::env::var("SIGNAL_POLL_SECS")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(10);
    let platform = SignalPlatform;
    let me = sender()?;
    let group_allowlist = signal_group_allowlist();
    if signal_group_allow_all() {
        tracing::warn!(
            "allowlist grup signal: DINONAKTIFKAN via *_ALLOW_ALL_USERS (semua grup diizinkan)"
        );
    }
    match &group_allowlist {
        Some(set) if set.contains("*") => {
            tracing::info!("allowlist grup signal: semua grup diizinkan (*)")
        }
        Some(set) => tracing::info!("allowlist grup signal: {} grup diizinkan", set.len()),
        None => tracing::warn!(
            "SIGNAL_GROUP_ALLOWED_USERS tidak diset: pesan grup signal akan dilewati (fail-closed)"
        ),
    }
    tracing::info!("signal: polling receive tiap {poll_secs}s sebagai {me}");
    loop {
        match receive_once().await {
            Ok(msgs) => {
                for m in msgs {
                    if m.from == me {
                        continue;
                    }
                    // Pesan grup dari groupId yang tak terdaftar: lewati tanpa
                    // balasan (tidak ada konteks grup yang boleh bocor ke DM).
                    if !is_signal_message_allowed(&m, &group_allowlist) {
                        tracing::warn!(group_id = ?m.group_id, from = %m.from, "pesan grup signal dari groupId tak diizinkan, dilewati");
                        continue;
                    }
                    tracing::info!(from = %m.from, "pesan signal masuk");
                    let reply = match handler(m.clone()).await {
                        Ok(r) => r,
                        Err(e) => format!("Maaf, terjadi error: {e}"),
                    };
                    // Balasan pesan grup kembali ke grup (bukan DM ke pengirim).
                    let sent = match &m.group_id {
                        Some(gid) => platform.send_group_message(gid, &reply).await,
                        None => platform.send_message(&m.from, &reply).await,
                    };
                    if let Err(e) = sent {
                        tracing::warn!("gagal membalas signal: {e}");
                    }
                }
            }
            Err(e) => tracing::warn!("signal receive error: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(poll_secs)).await;
    }
}

async fn receive_once() -> Result<Vec<SignalMessage>> {
    let out = tokio::process::Command::new(cli_bin())
        .args(["-u", &sender()?, "receive", "--json", "--timeout", "5"])
        .output()
        .await
        .map_err(|e| HermesError::Gateway(format!("signal-cli gagal dijalankan: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        // Bukan error fatal (mis. tidak ada pesan baru).
        if err.contains("No messages") || out.stdout.is_empty() {
            return Ok(vec![]);
        }
        return Err(HermesError::Gateway(format!(
            "signal-cli receive gagal: {err}"
        )));
    }
    let mut msgs = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(m) = parse_receive_line(line) {
            msgs.push(m);
        }
    }
    Ok(msgs)
}

/// Parse satu baris JSON `signal-cli receive --json` menjadi pesan; `None`
/// bila baris bukan pesan teks (receipt/typing/baris kosong). Murni agar
/// mudah diunit-tes. `groupInfo` di `dataMessage` menandai pesan grup.
fn parse_receive_line(line: &str) -> Option<SignalMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(line).unwrap_or(Value::Null);
    let env = v.get("envelope").unwrap_or(&Value::Null);
    let from = env
        .get("sourceNumber")
        .or_else(|| env.get("source"))
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let dm = env.get("dataMessage").unwrap_or(&Value::Null);
    let text = dm
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    // Abaikan receipt/typing/dsb.
    if from.is_empty() || text.trim().is_empty() {
        return None;
    }
    let group_id = dm
        .get("groupInfo")
        .and_then(|g| g.get("groupId"))
        .and_then(|g| g.as_str())
        .map(|s| s.to_string());
    Some(SignalMessage {
        from,
        text,
        timestamp: env.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0),
        group_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{clear_auth_env, restore_auth_env, set_var, ENV_LOCK};

    const AUTH_VARS: [&str; 4] = [
        "SIGNAL_GROUP_ALLOWED_USERS",
        "SIGNAL_ALLOW_ALL_USERS",
        "GATEWAY_ALLOWED_USERS",
        "GATEWAY_ALLOW_ALL_USERS",
    ];

    fn group_msg(from: &str) -> SignalMessage {
        group_msg_id(from, "Z3JvdXBJZA==")
    }

    fn group_msg_id(from: &str, group_id: &str) -> SignalMessage {
        SignalMessage {
            from: from.to_string(),
            text: "halo grup".to_string(),
            timestamp: 1,
            group_id: Some(group_id.to_string()),
        }
    }

    fn dm_msg() -> SignalMessage {
        SignalMessage {
            from: "+628111".to_string(),
            text: "halo".to_string(),
            timestamp: 1,
            group_id: None,
        }
    }

    #[test]
    fn group_allowlist_kosong_fail_closed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        assert!(signal_group_allowlist().is_none());
        assert!(!signal_group_allow_all());
        // Pesan grup: ditolak → dilewati tanpa balasan.
        assert!(!is_signal_message_allowed(
            &group_msg("+628111"),
            &signal_group_allowlist()
        ));
        // DM: tidak terpengaruh, tetap diproses.
        assert!(is_signal_message_allowed(
            &dm_msg(),
            &signal_group_allowlist()
        ));
        restore_auth_env(saved);
    }

    #[test]
    fn group_allowlist_parse_koma() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        // Kunci izin adalah groupId (bukan nomor pengirim).
        set_var(
            "SIGNAL_GROUP_ALLOWED_USERS",
            Some(" Z3JvdXBJZA== ,Z3JvdXBJZA2==,,"),
        );
        let allow = Some(signal_group_allowlist().expect("harus ter-parse"));
        assert_eq!(allow.as_ref().unwrap().len(), 2);
        // Pengirim tidak relevan: grup terdaftar diproses siapa pun pengirimnya.
        assert!(is_signal_message_allowed(
            &group_msg_id("+628999", "Z3JvdXBJZA=="),
            &allow
        ));
        assert!(is_signal_message_allowed(
            &group_msg_id("+628111", "Z3JvdXBJZA2=="),
            &allow
        ));
        // Grup tak terdaftar ditolak walau pengirimnya "dikenal".
        assert!(!is_signal_message_allowed(
            &group_msg_id("+628111", "Z3JvdXBJZA9=="),
            &allow
        ));
        // DM tetap tidak terpengaruh.
        assert!(is_signal_message_allowed(&dm_msg(), &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn group_allowlist_nomor_pengirim_bukan_kunci() {
        // Regresi semantik batch 2: SIGNAL_GROUP_ALLOWED_USERS berisi nomor
        // pengirim TIDAK boleh memberi akses ke grup mana pun — hanya groupId
        // yang cocok yang diizinkan (menyamai gateway aslinya).
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SIGNAL_GROUP_ALLOWED_USERS", Some("+628111,Z3JvdXBJZA=="));
        let allow = Some(signal_group_allowlist().expect("harus ter-parse"));
        // Pengirim "+628111" terdaftar sebagai *nomor* tetapi grupnya tidak
        // terdaftar → ditolak (tidak ada fallback ke sender-ID).
        assert!(!is_signal_message_allowed(
            &group_msg_id("+628111", "Z3JvdXBJZA9=="),
            &allow
        ));
        // Sebaliknya: pengirim tak dikenal di grup terdaftar → diizinkan.
        assert!(is_signal_message_allowed(
            &group_msg_id("+628000", "Z3JvdXBJZA=="),
            &allow
        ));
        restore_auth_env(saved);
    }

    #[test]
    fn group_allowlist_wildcard_buka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SIGNAL_GROUP_ALLOWED_USERS", Some("*"));
        let allow = Some(signal_group_allowlist().expect("harus ter-parse"));
        assert!(is_signal_message_allowed(&group_msg("+siapa-saja"), &allow));
        restore_auth_env(saved);
    }

    #[test]
    fn group_allow_all_membuka_semua() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("SIGNAL_ALLOW_ALL_USERS", Some("yes"));
        assert!(signal_group_allow_all());
        assert!(is_signal_message_allowed(&group_msg("+628111"), &None));
        restore_auth_env(saved);
    }

    #[test]
    fn group_allowlist_gateway_users_gabungan() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = clear_auth_env(&AUTH_VARS);
        set_var("GATEWAY_ALLOWED_USERS", Some("Z3JvdXBJZA=="));
        let allow = Some(signal_group_allowlist().expect("harus ter-parse"));
        assert!(is_signal_message_allowed(
            &group_msg_id("+628999", "Z3JvdXBJZA=="),
            &allow
        ));
        assert!(!is_signal_message_allowed(
            &group_msg_id("+628999", "Z3JvdXBJZA9=="),
            &allow
        ));
        restore_auth_env(saved);
    }

    #[test]
    fn parse_dm_tanpa_group_info() {
        let m = parse_receive_line(
            r#"{"envelope":{"source":"+628111","timestamp":1700000000000,"dataMessage":{"message":"halo"}}}"#,
        )
        .expect("harus ter-parse");
        assert_eq!(m.from, "+628111");
        assert_eq!(m.text, "halo");
        assert_eq!(m.group_id, None);
    }

    #[test]
    fn parse_pesan_grup_terdeteksi_group_info() {
        let m = parse_receive_line(
            r#"{"envelope":{"sourceNumber":"+628111","timestamp":1700000000000,"dataMessage":{"message":"halo grup","groupInfo":{"groupId":"Z3JvdXBJZA==","groupName":"Keluarga"}}}}"#,
        )
        .expect("harus ter-parse");
        assert_eq!(m.from, "+628111");
        assert_eq!(m.group_id.as_deref(), Some("Z3JvdXBJZA=="));
    }

    #[test]
    fn parse_bukan_pesan_teks_diskip() {
        // Receipt/typing: tidak ada dataMessage → None.
        assert_eq!(
            parse_receive_line(r#"{"envelope":{"source":"+628111","receiptMessage":{}}}"#),
            None
        );
        // Pesan kosong → None.
        assert_eq!(
            parse_receive_line(
                r#"{"envelope":{"source":"+628111","dataMessage":{"message":"  "}}}"#
            ),
            None
        );
        // Baris kosong / bukan JSON → None.
        assert_eq!(parse_receive_line(""), None);
        assert_eq!(parse_receive_line("bukan json"), None);
    }
}
