//! Test integrasi `browser_vault`: vault file-backed + fill via mock CDP.
//!
//! Tanpa keychain asli dan tanpa network: vault memakai direktori temporer
//! (HERMES_HOME), dan sesi browser diganti mock WebSocket CDP lokal.
//! Env var diserialkan via ENV_LOCK karena std::env bersifat global per-proses.
#![allow(clippy::await_holding_lock)]

use hermes_core::{
    AllowAllApproval, CancelFlag, MemoryEntry, MemoryStore, Result, Session, Skill, SkillStore,
    Tool, ToolContext,
};
use hermes_tools::browser_vault::{BrowserVaultTool, VaultStore};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct StubMemory;
#[async_trait::async_trait]
impl MemoryStore for StubMemory {
    async fn remember(&self, _e: MemoryEntry) -> Result<()> {
        Ok(())
    }
    async fn recall(&self, _q: &str, _l: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn forget(&self, _q: &str) -> Result<usize> {
        Ok(0)
    }
    async fn list_memories(&self, _l: usize) -> Result<Vec<MemoryEntry>> {
        Ok(vec![])
    }
    async fn save_session(&self, _s: &Session) -> Result<()> {
        Ok(())
    }
    async fn load_session(&self, _id: &str) -> Result<Option<Session>> {
        Ok(None)
    }
    async fn list_sessions(&self, _p: &str, _l: usize) -> Result<Vec<Session>> {
        Ok(vec![])
    }
    async fn delete_session(&self, _id: &str) -> Result<()> {
        Ok(())
    }
    async fn search_sessions(&self, _q: &str, _l: usize) -> Result<Vec<Session>> {
        Ok(vec![])
    }
}

struct StubSkills;
#[async_trait::async_trait]
impl SkillStore for StubSkills {
    async fn list(&self) -> Result<Vec<Skill>> {
        Ok(vec![])
    }
    async fn get(&self, _n: &str) -> Result<Option<Skill>> {
        Ok(None)
    }
    async fn create(&self, n: &str, d: &str, b: &str) -> Result<Skill> {
        Ok(Skill {
            name: n.into(),
            description: d.into(),
            body: b.into(),
            path: Default::default(),
            triggers: vec![],
        })
    }
    async fn update(&self, _n: &str, _b: &str) -> Result<()> {
        Ok(())
    }
    async fn delete(&self, _n: &str) -> Result<()> {
        Ok(())
    }
    async fn find_relevant(&self, _t: &str, _l: usize) -> Result<Vec<Skill>> {
        Ok(vec![])
    }
}

fn test_ctx() -> (ToolContext, tempfile_guard::Guard) {
    let dir = std::env::temp_dir().join(format!(
        "hvault-it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ctx = ToolContext {
        working_dir: dir.clone(),
        session_cwd: Arc::new(std::sync::Mutex::new(dir.clone())),
        approval: Arc::new(AllowAllApproval),
        memory: Arc::new(StubMemory),
        skills: Arc::new(StubSkills),
        cancel: CancelFlag::new(),
        env: Default::default(),
        allow_outside_workdir: true,
        provider: None,
        model: "test".into(),
    };
    (ctx, tempfile_guard::Guard(dir))
}

mod tempfile_guard {
    pub struct Guard(pub std::path::PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn secret(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// HERMES_HOME temporer; kembalikan guard + path. ENV_LOCK harus dipegang.
fn temp_home() -> (tempfile_guard::Guard, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "hvault-home-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("HERMES_HOME", &dir);
    (tempfile_guard::Guard(dir.clone()), dir)
}

// ---------------------------------------------------------------------------
// Mock CDP WebSocket server
// ---------------------------------------------------------------------------

/// Mock server CDP: menjawab Runtime.evaluate. `page_href` = URL halaman saat
/// ini; ekspresi lain dijawab `{"filled":1}`. Mengembalikan (ws_url, pesan
/// yang direkam, handle).
async fn mock_cdp(
    page_href: &'static str,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    use futures::{SinkExt, StreamExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let rec2 = recorded.clone();
    let handle = tokio::spawn(async move {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let (mut sink, mut stream) = ws.split();
        while let Some(Ok(msg)) = stream.next().await {
            if !msg.is_text() {
                continue;
            }
            let text = msg.to_text().unwrap().to_string();
            rec2.lock().unwrap().push(text.clone());
            let v: serde_json::Value =
                serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            let id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let resp = if method == "Runtime.evaluate" {
                let expr = v
                    .get("params")
                    .and_then(|p| p.get("expression"))
                    .and_then(|e| e.as_str())
                    .unwrap_or("");
                let value = if expr.contains("window.location.href") {
                    serde_json::Value::String(page_href.to_string())
                } else {
                    serde_json::Value::String("{\"filled\":1}".to_string())
                };
                serde_json::json!({"id": id, "result": {"result": {"value": value}}})
            } else {
                serde_json::json!({"id": id, "result": {}})
            };
            let _ = sink
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    resp.to_string().into(),
                ))
                .await;
        }
    });
    (format!("ws://{addr}"), recorded, handle)
}

/// URL ws:// ke port yang pasti tertutup (untuk jalur fail-closed).
async fn closed_ws_url() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("ws://{addr}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_vault_kosong_memberi_hint() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, _home) = temp_home();
    std::env::remove_var("BROWSER_CDP_URL");
    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;
    let r = tool
        .execute(serde_json::json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    assert_eq!(v["success"], true);
    assert_eq!(v["items"].as_array().unwrap().len(), 0);
    assert!(v["hint"].is_string(), "hint: {}", r.content);
    std::env::remove_var("HERMES_HOME");
}

#[tokio::test]
async fn list_hanya_metadata_tanpa_password() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, home) = temp_home();
    std::env::remove_var("BROWSER_CDP_URL");
    // Tulis langsung via store ke vault milik HERMES_HOME temporer.
    let store = VaultStore::new(home.join("vault"));
    let meta = store
        .add_item(
            "login",
            "Example",
            &secret(&[
                ("identifier_type", "email"),
                ("identifier", "user@example.com"),
                ("password", "s3cr3t-p@ssw0rd"),
            ]),
            Some("https://example.com/login"),
        )
        .unwrap();

    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;
    let r = tool
        .execute(serde_json::json!({"action": "list"}), &ctx)
        .await
        .unwrap();
    assert!(
        !r.content.contains("s3cr3t-p@ssw0rd"),
        "password bocor di hasil list!"
    );
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["handle"], meta.id);
    assert_eq!(items[0]["identifier"], "user@example.com");
    assert_eq!(items[0]["origin"], "https://example.com");
    std::env::remove_var("HERMES_HOME");
}

#[tokio::test]
async fn fill_fail_closed_tanpa_cdp() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, home) = temp_home();
    let store = VaultStore::new(home.join("vault"));
    let meta = store
        .add_item(
            "login",
            "Example",
            &secret(&[
                ("identifier_type", "email"),
                ("identifier", "user@example.com"),
                ("password", "s3cr3t-p@ssw0rd"),
            ]),
            Some("https://example.com"),
        )
        .unwrap();
    // CDP tidak ada: arahkan ke port tertutup (deterministik, tanpa network).
    std::env::set_var("BROWSER_CDP_URL", closed_ws_url().await);

    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;
    let r = tool
        .execute(
            serde_json::json!({"action": "fill", "handle": meta.id}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !r.content.contains("s3cr3t-p@ssw0rd"),
        "password bocor di hasil fill yang gagal!"
    );
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    assert_eq!(v["success"], false);
    assert_eq!(v["error_type"], "supervisor_required");
    std::env::remove_var("BROWSER_CDP_URL");
    std::env::remove_var("HERMES_HOME");
}

#[tokio::test]
async fn fill_ditolak_bila_origin_tidak_cocok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, home) = temp_home();
    let store = VaultStore::new(home.join("vault"));
    let meta = store
        .add_item(
            "login",
            "Example",
            &secret(&[
                ("identifier_type", "email"),
                ("identifier", "user@example.com"),
                ("password", "s3cr3t-p@ssw0rd"),
            ]),
            Some("https://example.com"),
        )
        .unwrap();
    // Halaman jahat: origin berbeda.
    let (ws_url, recorded, _srv) = mock_cdp("https://evil.test/phish").await;
    std::env::set_var("BROWSER_CDP_URL", &ws_url);

    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;
    let r = tool
        .execute(
            serde_json::json!({"action": "fill", "handle": meta.id}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(
        !r.content.contains("s3cr3t-p@ssw0rd"),
        "password bocor saat origin mismatch!"
    );
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    assert_eq!(v["success"], false);
    assert_eq!(v["error_type"], "origin_mismatch");
    // Skrip fill ber-rahasia TIDAK PERNAH dikirim (penolakan terjadi sebelum resolve).
    let sent = recorded.lock().unwrap().join("\n");
    assert!(
        !sent.contains("s3cr3t-p@ssw0rd"),
        "rahasia terkirim ke halaman yang ditolak!"
    );
    std::env::remove_var("BROWSER_CDP_URL");
    std::env::remove_var("HERMES_HOME");
}

#[tokio::test]
async fn fill_login_berhasil_via_mock_cdp() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, home) = temp_home();
    let store = VaultStore::new(home.join("vault"));
    let meta = store
        .add_item(
            "login",
            "Example",
            &secret(&[
                ("identifier_type", "email"),
                ("identifier", "user@example.com"),
                ("password", "s3cr3t-p@ssw0rd"),
            ]),
            Some("https://example.com"),
        )
        .unwrap();
    let (ws_url, recorded, _srv) = mock_cdp("https://example.com/login").await;
    std::env::set_var("BROWSER_CDP_URL", &ws_url);

    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;
    let r = tool
        .execute(
            serde_json::json!({"action": "fill", "handle": meta.id}),
            &ctx,
        )
        .await
        .unwrap();
    // Hasil tool: metadata saja, tanpa password.
    assert!(
        !r.content.contains("s3cr3t-p@ssw0rd"),
        "password bocor di hasil fill!"
    );
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    assert_eq!(v["success"], true, "content: {}", r.content);
    assert_eq!(v["filled_fields"], 1);
    assert_eq!(v["backend"], "local");
    assert_eq!(v["kind"], "login");
    assert_eq!(v["origin"], "https://example.com");
    // Skrip yang dikirim menegaskan ulang origin secara sinkron.
    // (Pesan CDP terekam dalam bentuk JSON-encoded, jadi decode dulu.)
    let sent = recorded.lock().unwrap().join("\n");
    let mut found_reassert = false;
    for line in sent.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let expr = v
            .get("params")
            .and_then(|p| p.get("expression"))
            .and_then(|e| e.as_str())
            .unwrap_or("");
        if expr.contains("window.location.href") {
            continue;
        }
        if expr.contains(r#"window.location.origin !== "https://example.com""#) {
            found_reassert = true;
        }
    }
    assert!(
        found_reassert,
        "skrip fill tidak me-reassert origin; terkirim: {sent}"
    );
    std::env::remove_var("BROWSER_CDP_URL");
    std::env::remove_var("HERMES_HOME");
}

#[tokio::test]
async fn enter_code_otomatis_dari_seed_tanpa_bocor() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, home) = temp_home();
    let store = VaultStore::new(home.join("vault"));
    let meta = store
        .add_item(
            "login",
            "OTP Site",
            &secret(&[
                ("identifier_type", "username"),
                ("identifier", "u"),
                ("password", "p"),
                ("otp_secret", "JBSWY3DPEHPK3PXP"),
            ]),
            Some("https://otp.test"),
        )
        .unwrap();
    let (ws_url, recorded, _srv) = mock_cdp("https://otp.test/2fa").await;
    std::env::set_var("BROWSER_CDP_URL", &ws_url);

    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;
    let r = tool
        .execute(
            serde_json::json!({"action": "enter_code", "handle": meta.id}),
            &ctx,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    assert_eq!(v["success"], true, "content: {}", r.content);
    assert_eq!(v["filled_fields"], 1);
    // Kode yang dibuat (6 digit dalam skrip terkirim) tidak boleh ada di output.
    let sent = recorded.lock().unwrap().join("\n");
    let re = regex::Regex::new(r"\b\d{6}\b").unwrap();
    let codes: Vec<String> = re
        .find_iter(&sent)
        .map(|m| m.as_str().to_string())
        .collect();
    assert!(!codes.is_empty(), "tidak ada kode TOTP yang dikirim?");
    for code in &codes {
        assert!(
            !r.content.contains(code),
            "kode 2FA {code} bocor di hasil tool!"
        );
    }
    std::env::remove_var("BROWSER_CDP_URL");
    std::env::remove_var("HERMES_HOME");
}

#[tokio::test]
async fn save_login_dan_unlock_fail_closed() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_home_guard, _home) = temp_home();
    std::env::remove_var("BROWSER_CDP_URL");
    let (ctx, _guard) = test_ctx();
    let tool = BrowserVaultTool;

    let r = tool
        .execute(serde_json::json!({"action": "save_login"}), &ctx)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    assert_eq!(v["success"], false);
    assert_eq!(v["error_type"], "prompt_unavailable");

    // unlock backend tak dikenal → error; backend lokal tak butuh unlock.
    let r = tool
        .execute(
            serde_json::json!({"action": "unlock", "backend": "bitwarden"}),
            &ctx,
        )
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    // bitwarden tidak terdeteksi di PATH sandbox → unknown_backend; bila ada
    // (terkunci) → unlock_unavailable. Keduanya fail-closed.
    assert_eq!(v["success"], false);
    assert!(
        v["error_type"] == "unknown_backend" || v["error_type"] == "unlock_unavailable",
        "content: {}",
        r.content
    );
    std::env::remove_var("HERMES_HOME");
}
