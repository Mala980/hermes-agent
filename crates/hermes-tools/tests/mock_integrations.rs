//! Test integrasi tanpa token: backend exec cloud + tools pihak ketiga.
//!
//! Memverifikasi request/response handling melawan mock HTTP server lokal
//! dan mock CLI via PATH, tanpa kredensial asli. Env var diserialkan via
//! ENV_LOCK karena std::env bersifat global per-proses.
#![allow(clippy::await_holding_lock)]

use hermes_core::{
    AllowAllApproval, CancelFlag, MemoryEntry, MemoryStore, Result, Session, Skill, SkillStore,
    Tool, ToolContext,
};
use hermes_tools::feishu::{
    FeishuDocReadTool, FeishuDriveAddCommentTool, FeishuDriveListCommentsTool,
};
use hermes_tools::shell::ExecTool;
use hermes_tools::social::{ReactToMessageTool, XSearchTool};
use hermes_tools::video::{VideoAnalyzeTool, VideoGenerateTool};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

static ENV_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Mock HTTP server (pola sama seperti hermes-gateway/tests/mock_platforms.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    auth: Option<String>,
    user_agent: Option<String>,
    content_type: Option<String>,
    body: String,
}

struct MockServer {
    addr: std::net::SocketAddr,
    recorded: Arc<Mutex<Vec<Recorded>>>,
    listener: Option<tokio::net::TcpListener>,
    _handle: Option<tokio::task::JoinHandle<()>>,
}

impl MockServer {
    /// Bind saja (tanpa melayani); URL sudah diketahui sebelum route didaftar.
    async fn bind() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        Self {
            addr,
            recorded: Arc::new(Mutex::new(Vec::new())),
            listener: Some(listener),
            _handle: None,
        }
    }

    /// Mulai melayani dengan route yang diberikan (panggil sekali setelah bind).
    /// routes: (method, path_prefix, status, body_json)
    fn serve(&mut self, routes: Vec<(String, String, u16, String)>) {
        let listener = self.listener.take().expect("serve dipanggil dua kali");
        let rec = self.recorded.clone();
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
                    let mut user_agent = None;
                    let mut content_type = None;
                    for line in lines.by_ref() {
                        if line.is_empty() {
                            break;
                        }
                        if let Some(idx) = line.find(':') {
                            let (name, val) = line.split_at(idx);
                            let val = val[1..].trim();
                            match name.to_lowercase().as_str() {
                                "authorization" => auth = Some(val.to_string()),
                                "user-agent" => user_agent = Some(val.to_string()),
                                "content-type" => content_type = Some(val.to_string()),
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
                        user_agent,
                        content_type,
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
                        201 => "Created",
                        204 => "No Content",
                        400 => "Bad Request",
                        401 => "Unauthorized",
                        403 => "Forbidden",
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
        self._handle = Some(handle);
    }

    /// routes: (method, path_prefix, status, body_json)
    async fn start(routes: Vec<(String, String, u16, String)>) -> Self {
        let mut s = Self::bind().await;
        s.serve(routes);
        s
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn recorded(&self) -> Vec<Recorded> {
        self.recorded.lock().unwrap().clone()
    }

    fn find(&self, method: &str, path_prefix: &str) -> Option<Recorded> {
        self.recorded().into_iter().find(|r| {
            r.method == method
                && r.path
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .starts_with(path_prefix)
        })
    }
}

// ---------------------------------------------------------------------------
// Stub ToolContext
// ---------------------------------------------------------------------------

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
        "hermes-test-{}-{}",
        std::process::id(),
        rand_suffix()
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

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{n:x}")
}

/// Penjaga direktori sementara: hapus saat drop.
mod tempfile_guard {
    pub struct Guard(pub std::path::PathBuf);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn set_env(k: &str, v: &str) {
    std::env::set_var(k, v);
}
fn remove_env(k: &str) {
    std::env::remove_var(k);
}

fn args(json: serde_json::Value) -> serde_json::Value {
    json
}

// ---------------------------------------------------------------------------
// Daytona
// ---------------------------------------------------------------------------

#[tokio::test]
async fn daytona_exec_ok_and_cleans_up() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![
        (
            "POST".into(),
            "/sandbox".into(),
            201,
            r#"{"id":"sbx-mock1"}"#.into(),
        ),
        (
            "POST".into(),
            "/toolbox/sbx-mock1/process/execute".into(),
            200,
            r#"{"exitCode":0,"result":"hello daytona"}"#.into(),
        ),
        (
            "DELETE".into(),
            "/sandbox/sbx-mock1".into(),
            200,
            r#"{}"#.into(),
        ),
    ])
    .await;
    set_env("DAYTONA_API_KEY", "DAYTONA-MOCK-KEY");
    set_env("DAYTONA_API_URL", &mock.url());

    let (ctx, _guard) = test_ctx();
    let res = ExecTool
        .execute(
            args(serde_json::json!({"command": "echo hi", "backend": "daytona"})),
            &ctx,
        )
        .await;

    remove_env("DAYTONA_API_KEY");
    remove_env("DAYTONA_API_URL");
    let res = res.expect("exec daytona gagal");
    assert!(!res.is_error, "content: {}", res.content);
    assert!(
        res.content.contains("[exit: 0]"),
        "content: {}",
        res.content
    );
    assert!(
        res.content.contains("hello daytona"),
        "content: {}",
        res.content
    );

    let rec = mock.recorded();
    assert_eq!(rec.len(), 3, "recorded: {rec:?}");
    let create = mock.find("POST", "/sandbox").expect("tanpa create sandbox");
    assert_eq!(create.auth.as_deref(), Some("Bearer DAYTONA-MOCK-KEY"));
    let exec = mock.find("POST", "/toolbox/").expect("tanpa execute");
    assert!(exec.body.contains("echo hi"), "body: {}", exec.body);
    assert!(
        mock.find("DELETE", "/sandbox/sbx-mock1").is_some(),
        "sandbox tidak dihapus: {rec:?}"
    );
}

#[tokio::test]
async fn daytona_exec_rejected_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/sandbox".into(),
        401,
        r#"{"message":"invalid api key"}"#.into(),
    )])
    .await;
    set_env("DAYTONA_API_KEY", "SALAH");
    set_env("DAYTONA_API_URL", &mock.url());

    let (ctx, _guard) = test_ctx();
    let res = ExecTool
        .execute(
            args(serde_json::json!({"command": "echo hi", "backend": "daytona"})),
            &ctx,
        )
        .await;

    remove_env("DAYTONA_API_KEY");
    remove_env("DAYTONA_API_URL");
    let err = res.expect_err("seharusnya error saat API menolak");
    assert!(err.to_string().contains("ditolak"), "err: {err}");
}

#[tokio::test]
async fn daytona_missing_key_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    remove_env("DAYTONA_API_KEY");
    let (ctx, _guard) = test_ctx();
    let res = ExecTool
        .execute(
            args(serde_json::json!({"command": "echo hi", "backend": "daytona"})),
            &ctx,
        )
        .await;
    let err = res.expect_err("seharusnya error tanpa API key");
    assert!(err.to_string().contains("DAYTONA_API_KEY"), "err: {err}");
}

// ---------------------------------------------------------------------------
// Feishu
// ---------------------------------------------------------------------------

const FEISHU_AUTH_OK: &str = r#"{"code":0,"tenant_access_token":"TAT-MOCK"}"#;

fn feishu_env(mock: &MockServer) {
    set_env("FEISHU_APP_ID", "cli_mock");
    set_env("FEISHU_APP_SECRET", "sec_mock");
    set_env("FEISHU_API_BASE", &mock.url());
}
fn feishu_unenv() {
    remove_env("FEISHU_APP_ID");
    remove_env("FEISHU_APP_SECRET");
    remove_env("FEISHU_API_BASE");
}

#[tokio::test]
async fn feishu_doc_read_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![
        (
            "POST".into(),
            "/open-apis/auth/v3/tenant_access_token/internal".into(),
            200,
            FEISHU_AUTH_OK.into(),
        ),
        (
            "GET".into(),
            "/open-apis/docx/v1/documents/DOC1/raw_content".into(),
            200,
            r#"{"code":0,"data":{"content":"isi dokumen mock"}}"#.into(),
        ),
    ])
    .await;
    feishu_env(&mock);

    let (ctx, _guard) = test_ctx();
    let res = FeishuDocReadTool
        .execute(args(serde_json::json!({"document_id": "DOC1"})), &ctx)
        .await;
    feishu_unenv();
    let res = res.expect("feishu_doc_read gagal");
    assert!(
        res.content.contains("isi dokumen mock"),
        "content: {}",
        res.content
    );

    let auth = mock.find("POST", "/open-apis/auth").expect("tanpa auth");
    assert!(auth.body.contains("cli_mock"), "body: {}", auth.body);
    let doc = mock
        .find("GET", "/open-apis/docx")
        .expect("tanpa baca dokumen");
    assert_eq!(
        doc.auth.as_deref(),
        Some("Bearer TAT-MOCK"),
        "auth: {:?}",
        doc.auth
    );
}

#[tokio::test]
async fn feishu_add_and_list_comments_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![
        ("POST".into(), "/open-apis/auth/v3/tenant_access_token/internal".into(), 200, FEISHU_AUTH_OK.into()),
        (
            "POST".into(),
            "/open-apis/drive/v1/files/FT1/comments".into(),
            200,
            r#"{"code":0,"data":{"comment":{"comment_id":"cmt-9"}}}"#.into(),
        ),
        (
            "GET".into(),
            "/open-apis/drive/v1/files/FT1/comments".into(),
            200,
            r#"{"code":0,"data":{"items":[
                {"comment_id":"cmt-1","content":{"elements":[{"type":"text_run","text_run":{"content":"halo"}}]}},
                {"comment_id":"cmt-2","content":{"elements":[{"type":"text_run","text_run":{"content":"dunia"}}]}}
            ]}}"#.into(),
        ),
    ])
    .await;
    feishu_env(&mock);

    let (ctx, _guard) = test_ctx();
    let res = FeishuDriveAddCommentTool
        .execute(
            args(serde_json::json!({"file_token": "FT1", "text": "komentar tes"})),
            &ctx,
        )
        .await;
    feishu_unenv();
    let res = res.expect("feishu add comment gagal");
    assert!(res.content.contains("cmt-9"), "content: {}", res.content);

    // Catatan: list dipanggil setelah unenv — set ulang env untuk panggilan kedua.
    feishu_env(&mock);
    let res = FeishuDriveListCommentsTool
        .execute(args(serde_json::json!({"file_token": "FT1"})), &ctx)
        .await;
    feishu_unenv();
    let res = res.expect("feishu list comments gagal");
    assert!(
        res.content.contains("[cmt-1] halo"),
        "content: {}",
        res.content
    );
    assert!(
        res.content.contains("[cmt-2] dunia"),
        "content: {}",
        res.content
    );

    let add = mock
        .recorded()
        .into_iter()
        .find(|r| {
            r.method == "POST"
                && r.path.contains("/drive/v1/files/FT1/comments")
                && !r.path.contains("auth")
        })
        .expect("tanpa add comment");
    assert!(add.body.contains("komentar tes"), "body: {}", add.body);
    assert_eq!(add.auth.as_deref(), Some("Bearer TAT-MOCK"));
}

#[tokio::test]
async fn feishu_auth_rejected_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/open-apis/auth/v3/tenant_access_token/internal".into(),
        200,
        r#"{"code":99991663,"msg":"app secret invalid"}"#.into(),
    )])
    .await;
    feishu_env(&mock);
    let (ctx, _guard) = test_ctx();
    let res = FeishuDocReadTool
        .execute(args(serde_json::json!({"document_id": "DOC1"})), &ctx)
        .await;
    feishu_unenv();
    let err = res.expect_err("seharusnya error saat auth ditolak");
    assert!(err.to_string().contains("ditolak"), "err: {err}");
}

#[tokio::test]
async fn feishu_missing_creds_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    remove_env("FEISHU_APP_ID");
    remove_env("LARK_APP_ID");
    let (ctx, _guard) = test_ctx();
    let res = FeishuDocReadTool
        .execute(args(serde_json::json!({"document_id": "DOC1"})), &ctx)
        .await;
    let err = res.expect_err("seharusnya error tanpa kredensial");
    assert!(err.to_string().contains("FEISHU_APP_ID"), "err: {err}");
}

// ---------------------------------------------------------------------------
// x_search (xAI Responses API)
// ---------------------------------------------------------------------------

/// Bentuk request persis seperti original: POST {base}/responses dengan
/// header Bearer + User-Agent dan body {model, input, tools, store:false}.
#[tokio::test]
async fn x_search_responses_api_request_shape() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/v1/responses".into(),
        200,
        r#"{
            "output_text": "Orang di X membahas peluncuran terbaru xAI.",
            "citations": [{"url": "https://x.com/contoh/status/1", "title": "Contoh postingan"}]
        }"#
        .into(),
    )])
    .await;
    set_env("XAI_API_KEY", "xai-test-key");
    // XAI_BASE_URL menggantikan seluruh base default https://api.x.ai/v1,
    // jadi mock perlu sufiks /v1 agar path menjadi /v1/responses.
    set_env("XAI_BASE_URL", &format!("{}/v1", mock.url()));
    remove_env("XAI_X_SEARCH_MODEL");

    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(
            args(serde_json::json!({
                "query": "Apa kata orang tentang xAI di X?",
                "allowed_x_handles": ["xai", "@grok"],
                "from_date": "2026-04-01",
                "to_date": "2026-04-10",
                "enable_image_understanding": true
            })),
            &ctx,
        )
        .await;
    remove_env("XAI_API_KEY");
    remove_env("XAI_BASE_URL");
    let res = res.expect("x_search gagal");
    assert!(!res.is_error, "content: {}", res.content);

    let rec = mock.find("POST", "/v1/responses").expect("tanpa request");
    assert_eq!(rec.path, "/v1/responses");
    assert_eq!(rec.auth.as_deref(), Some("Bearer xai-test-key"));
    assert_eq!(
        rec.user_agent.as_deref(),
        Some(format!("hermes-rs/{}", env!("CARGO_PKG_VERSION")).as_str()),
        "user_agent: {:?}",
        rec.user_agent
    );
    assert_eq!(
        rec.content_type.as_deref(),
        Some("application/json"),
        "content_type: {:?}",
        rec.content_type
    );
    let body: serde_json::Value = serde_json::from_str(&rec.body).expect("body harus JSON");
    assert_eq!(body["model"], "grok-4.5");
    assert_eq!(
        body["input"],
        serde_json::json!([{"role": "user", "content": "Apa kata orang tentang xAI di X?"}])
    );
    assert_eq!(body["store"], false);
    assert!(body.get("reasoning").is_none(), "body: {body}");
    let tool_def = &body["tools"][0];
    assert_eq!(tool_def["type"], "x_search");
    // "@" di depan handle dibuang seperti original.
    assert_eq!(
        tool_def["allowed_x_handles"],
        serde_json::json!(["xai", "grok"])
    );
    assert_eq!(tool_def["from_date"], "2026-04-01");
    assert_eq!(tool_def["to_date"], "2026-04-10");
    assert_eq!(tool_def["enable_image_understanding"], true);
    assert!(tool_def.get("enable_video_understanding").is_none());

    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], true);
    assert_eq!(out["provider"], "xai");
    assert_eq!(out["tool"], "x_search");
    assert_eq!(out["model"], "grok-4.5");
    assert_eq!(out["query"], "Apa kata orang tentang xAI di X?");
    assert_eq!(out["answer"], "Orang di X membahas peluncuran terbaru xAI.");
    assert_eq!(
        out["citations"],
        serde_json::json!([{"url": "https://x.com/contoh/status/1", "title": "Contoh postingan"}])
    );
    assert_eq!(out["degraded"], false);
}

/// Anotasi url_citation inline diekstrak dari output[].message.content[].
#[tokio::test]
async fn x_search_extracts_inline_url_citations() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/v1/responses".into(),
        200,
        r#"{
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": "xAI memposting pembaruan di X.",
                    "annotations": [{
                        "type": "url_citation",
                        "url": "https://x.com/xai/status/123",
                        "title": "xAI update",
                        "start_index": 0,
                        "end_index": 3
                    }]
                }]
            }]
        }"#
        .into(),
    )])
    .await;
    set_env("XAI_API_KEY", "xai-test-key");
    set_env("XAI_BASE_URL", &format!("{}/v1", mock.url()));

    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(
            args(serde_json::json!({
                "query": "postingan terbaru xai",
                "excluded_x_handles": ["grok"]
            })),
            &ctx,
        )
        .await;
    remove_env("XAI_API_KEY");
    remove_env("XAI_BASE_URL");
    let res = res.expect("x_search gagal");
    assert!(!res.is_error, "content: {}", res.content);

    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], true);
    // Tanpa output_text: jawaban dirakit dari potongan message.
    assert_eq!(out["answer"], "xAI memposting pembaruan di X.");
    assert_eq!(
        out["inline_citations"],
        serde_json::json!([{
            "url": "https://x.com/xai/status/123",
            "title": "xAI update",
            "start_index": 0,
            "end_index": 3
        }])
    );
    // Kanal sitasi inline tidak kosong -> tidak degraded walau filter aktif.
    assert_eq!(out["degraded"], false);
}

/// Filter penyempit aktif tapi xAI tidak mengembalikan sitasi di kanal
/// mana pun -> degraded: true (paritas original).
#[tokio::test]
async fn x_search_degraded_when_filters_match_nothing() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/v1/responses".into(),
        200,
        r#"{"output_text": "Jawaban generik tanpa sitasi."}"#.into(),
    )])
    .await;
    set_env("XAI_API_KEY", "xai-test-key");
    set_env("XAI_BASE_URL", &format!("{}/v1", mock.url()));

    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(
            args(serde_json::json!({
                "query": "hal langka",
                "from_date": "2026-01-01",
                "to_date": "2026-01-02"
            })),
            &ctx,
        )
        .await;
    remove_env("XAI_API_KEY");
    remove_env("XAI_BASE_URL");
    let res = res.expect("x_search gagal");

    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], true);
    assert_eq!(out["degraded"], true);
    assert!(
        out["degraded_reason"]
            .as_str()
            .unwrap_or("")
            .contains("from_date"),
        "degraded_reason: {}",
        out["degraded_reason"]
    );
}

/// Tanpa filter penyempit, jawaban tanpa sitasi bukan "filter miss".
#[tokio::test]
async fn x_search_not_degraded_without_filters() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/v1/responses".into(),
        200,
        r#"{"output_text": "Jawaban luas.", "citations": []}"#.into(),
    )])
    .await;
    set_env("XAI_API_KEY", "xai-test-key");
    set_env("XAI_BASE_URL", &format!("{}/v1", mock.url()));

    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(args(serde_json::json!({"query": "hal umum"})), &ctx)
        .await;
    remove_env("XAI_API_KEY");
    remove_env("XAI_BASE_URL");
    let res = res.expect("x_search gagal");

    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], true);
    assert_eq!(out["degraded"], false);
    assert!(out["degraded_reason"].is_null());
}

#[tokio::test]
async fn x_search_rejects_conflicting_handle_filters() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_env("XAI_API_KEY", "xai-test-key");
    // Validasi gagal sebelum request HTTP apa pun.
    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(
            args(serde_json::json!({
                "query": "diskusi xai",
                "allowed_x_handles": ["xai"],
                "excluded_x_handles": ["grok"]
            })),
            &ctx,
        )
        .await;
    remove_env("XAI_API_KEY");
    let res = res.expect("x_search gagal");
    assert!(res.is_error, "content: {}", res.content);
    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], false);
    assert_eq!(out["error_type"], "validation");
    assert!(
        out["error"].as_str().unwrap_or("").contains("bersamaan"),
        "error: {}",
        out["error"]
    );
}

#[tokio::test]
async fn x_search_validates_dates_and_handles() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_env("XAI_API_KEY", "xai-test-key");
    let (ctx, _guard) = test_ctx();
    let cases: Vec<(&str, serde_json::Value, &str)> = vec![
        (
            "from_lewat_to",
            serde_json::json!({"query": "x", "from_date": "2026-04-10", "to_date": "2026-04-01"}),
            "from_date",
        ),
        (
            "from_masa_depan",
            serde_json::json!({"query": "x", "from_date": "2999-01-01"}),
            "masa depan",
        ),
        (
            "format_salah",
            serde_json::json!({"query": "x", "from_date": "01-04-2026"}),
            "YYYY-MM-DD",
        ),
        (
            "tanggal_tak_valid",
            serde_json::json!({"query": "x", "to_date": "2026-02-30"}),
            "YYYY-MM-DD",
        ),
        (
            "handle_lebih_dari_10",
            serde_json::json!({"query": "x", "allowed_x_handles": ["a","b","c","d","e","f","g","h","i","j","k"]}),
            "maksimal 10",
        ),
        ("query_kosong", serde_json::json!({"query": "   "}), "query"),
    ];
    for (name, a, expect) in cases {
        let res = XSearchTool
            .execute(args(a), &ctx)
            .await
            .expect("x_search gagal");
        assert!(res.is_error, "{name}: {}", res.content);
        let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
        assert_eq!(out["success"], false, "{name}");
        assert_eq!(out["error_type"], "validation", "{name}");
        assert!(
            out["error"].as_str().unwrap_or("").contains(expect),
            "{name}: {}",
            out["error"]
        );
    }
    remove_env("XAI_API_KEY");
}

#[tokio::test]
async fn x_search_missing_key_is_json_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    remove_env("XAI_API_KEY");
    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(args(serde_json::json!({"query": "x"})), &ctx)
        .await
        .expect("x_search gagal");
    assert!(res.is_error, "content: {}", res.content);
    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], false);
    assert_eq!(out["error_type"], "credentials");
    assert!(
        out["error"].as_str().unwrap_or("").contains("XAI_API_KEY"),
        "error: {}",
        out["error"]
    );
}

/// Error HTTP dipetakan ala original: body {"code","error"} -> "code: error".
#[tokio::test]
async fn x_search_http_error_mapped() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/v1/responses".into(),
        403,
        r#"{"code":"forbidden","error":"x_search is not enabled for this model"}"#.into(),
    )])
    .await;
    set_env("XAI_API_KEY", "SALAH");
    set_env("XAI_BASE_URL", &format!("{}/v1", mock.url()));
    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(args(serde_json::json!({"query": "diskusi xai"})), &ctx)
        .await;
    remove_env("XAI_API_KEY");
    remove_env("XAI_BASE_URL");
    let res = res.expect("x_search gagal");
    assert!(res.is_error, "content: {}", res.content);
    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], false);
    assert_eq!(out["provider"], "xai");
    assert_eq!(out["tool"], "x_search");
    assert_eq!(out["error_type"], "http");
    assert_eq!(
        out["error"],
        "forbidden: x_search is not enabled for this model"
    );
}

/// Skema mengikuti tool xAI: filter-filter original, tanpa max_results
/// (xAI x_search tidak punya field count).
#[tokio::test]
async fn x_search_schema_matches_responses_api() {
    let schema = XSearchTool.parameters_schema();
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert_eq!(required, vec!["query"]);
    let props = &schema["properties"];
    for p in [
        "query",
        "allowed_x_handles",
        "excluded_x_handles",
        "from_date",
        "to_date",
        "enable_image_understanding",
        "enable_video_understanding",
    ] {
        assert!(props.get(p).is_some(), "properti {p} hilang");
    }
    assert!(
        props.get("max_results").is_none(),
        "max_results harus dihapus"
    );
    assert!(
        XSearchTool.description().contains("XAI_API_KEY"),
        "deskripsi: {}",
        XSearchTool.description()
    );
}

/// Mock berurutan untuk uji retry: tiap koneksi mendapat respons berikutnya
/// dari daftar. Mengembalikan (base_url, penghitung request).
async fn start_sequential_mock(
    responses: Vec<(u16, String)>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count2 = count.clone();
    tokio::spawn(async move {
        for (status, body) in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buf = vec![0u8; 65536];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let header_end = req.find("\r\n\r\n").map(|i| i + 4).unwrap_or(req.len());
            let content_length: usize = req
                .lines()
                .skip(1)
                .find_map(|l| {
                    let (name, val) = l.split_once(':')?;
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        val.trim().parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let mut body_len = n.saturating_sub(header_end);
            while body_len < content_length {
                let mut tmp = vec![0u8; 8192];
                let m = socket.read(&mut tmp).await.unwrap_or(0);
                if m == 0 {
                    break;
                }
                body_len += m;
            }
            count2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let reason = match status {
                200 => "OK",
                500 => "Internal Server Error",
                _ => "Error",
            };
            let resp = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
        }
    });
    (format!("http://{addr}"), count)
}

/// 5xx di-retry (2x, backoff 1.5s/3.0s ala original) lalu sukses.
#[tokio::test]
async fn x_search_retries_500_then_succeeds() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (url, count) = start_sequential_mock(vec![
        (500, r#"{"error":"boom"}"#.to_string()),
        (500, r#"{"error":"boom"}"#.to_string()),
        (200, r#"{"output_text":"pulih setelah retry"}"#.to_string()),
    ])
    .await;
    set_env("XAI_API_KEY", "xai-test-key");
    set_env("XAI_BASE_URL", &url);
    let (ctx, _guard) = test_ctx();
    let res = XSearchTool
        .execute(args(serde_json::json!({"query": "x"})), &ctx)
        .await;
    remove_env("XAI_API_KEY");
    remove_env("XAI_BASE_URL");
    let res = res.expect("x_search gagal");
    assert!(!res.is_error, "content: {}", res.content);
    let out: serde_json::Value = serde_json::from_str(&res.content).expect("hasil harus JSON");
    assert_eq!(out["success"], true);
    assert_eq!(out["answer"], "pulih setelah retry");
    assert_eq!(
        count.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "harus 1 request + 2 retry"
    );
}

// ---------------------------------------------------------------------------
// react_to_message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn react_telegram_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botTOK123/setMessageReaction".into(),
        200,
        r#"{"ok":true,"result":true}"#.into(),
    )])
    .await;
    set_env("TELEGRAM_BOT_TOKEN", "TOK123");
    set_env("TELEGRAM_API_BASE", &mock.url());
    let (ctx, _guard) = test_ctx();
    let res = ReactToMessageTool
        .execute(
            args(serde_json::json!({
                "platform": "telegram", "target": "555",
                "message_id": "77", "emoji": "👍"
            })),
            &ctx,
        )
        .await;
    remove_env("TELEGRAM_BOT_TOKEN");
    remove_env("TELEGRAM_API_BASE");
    let res = res.expect("react telegram gagal");
    assert!(res.content.contains("👍"), "content: {}", res.content);
    let rec = mock
        .find("POST", "/botTOK123/setMessageReaction")
        .expect("tanpa request");
    assert!(
        rec.body.contains("\"chat_id\":\"555\"") || rec.body.contains("\"chat_id\":555"),
        "body: {}",
        rec.body
    );
    assert!(rec.body.contains("👍"), "body: {}", rec.body);
}

#[tokio::test]
async fn react_telegram_rejected_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/botBAD/setMessageReaction".into(),
        200,
        r#"{"ok":false,"description":"message not found"}"#.into(),
    )])
    .await;
    set_env("TELEGRAM_BOT_TOKEN", "BAD");
    set_env("TELEGRAM_API_BASE", &mock.url());
    let (ctx, _guard) = test_ctx();
    let res = ReactToMessageTool
        .execute(
            args(serde_json::json!({
                "platform": "telegram", "target": "555",
                "message_id": "77", "emoji": "👍"
            })),
            &ctx,
        )
        .await;
    remove_env("TELEGRAM_BOT_TOKEN");
    remove_env("TELEGRAM_API_BASE");
    let err = res.expect_err("seharusnya error saat ok=false");
    assert!(err.to_string().contains("menolak"), "err: {err}");
}

#[tokio::test]
async fn react_discord_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "PUT".into(),
        "/channels/C1/messages/M1/reactions/".into(),
        200,
        r#"{}"#.into(),
    )])
    .await;
    set_env("DISCORD_BOT_TOKEN", "DISC-MOCK");
    set_env("DISCORD_API_BASE", &mock.url());
    let (ctx, _guard) = test_ctx();
    let res = ReactToMessageTool
        .execute(
            args(serde_json::json!({
                "platform": "discord", "target": "C1",
                "message_id": "M1", "emoji": "👍"
            })),
            &ctx,
        )
        .await;
    remove_env("DISCORD_BOT_TOKEN");
    remove_env("DISCORD_API_BASE");
    res.expect("react discord gagal");
    let rec = mock
        .find("PUT", "/channels/C1/messages/M1/reactions/")
        .expect("tanpa request");
    assert_eq!(
        rec.auth.as_deref(),
        Some("Bot DISC-MOCK"),
        "auth: {:?}",
        rec.auth
    );
    // 👍 = F0 9F 91 8D ter-encode.
    assert!(rec.path.contains("%F0%9F%91%8D"), "path: {}", rec.path);
}

#[tokio::test]
async fn react_unsupported_platform_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (ctx, _guard) = test_ctx();
    let res = ReactToMessageTool
        .execute(
            args(serde_json::json!({
                "platform": "slack", "target": "C1",
                "message_id": "M1", "emoji": "👍"
            })),
            &ctx,
        )
        .await;
    let err = res.expect_err("seharusnya error untuk platform tak didukung");
    assert!(err.to_string().contains("tidak didukung"), "err: {err}");
}

// ---------------------------------------------------------------------------
// video_generate (fal.ai)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn video_generate_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut mock = MockServer::bind().await;
    let base = mock.url();
    // status_url/response_url menunjuk ke mock ini.
    mock.serve(vec![
        (
            "POST".into(),
            "/fal-ai/veo3".into(),
            200,
            format!(
                r#"{{"status_url":"{base}/req/r1/status","response_url":"{base}/req/r1","request_id":"r1"}}"#
            ),
        ),
        ("GET".into(), "/req/r1/status".into(), 200, r#"{"status":"COMPLETED"}"#.into()),
        ("GET".into(), "/req/r1".into(), 200, r#"{"video":{"url":"http://cdn.mock/v.mp4"}}"#.into()),
    ]);
    set_env("FAL_API_KEY", "FAL-MOCK");
    set_env("FAL_API_BASE", &mock.url());

    let (ctx, _guard) = test_ctx();
    let res = VideoGenerateTool
        .execute(args(serde_json::json!({"prompt": "kucing astronot"})), &ctx)
        .await;
    remove_env("FAL_API_KEY");
    remove_env("FAL_API_BASE");
    let res = res.expect("video_generate gagal");
    assert!(
        res.content.contains("http://cdn.mock/v.mp4"),
        "content: {}",
        res.content
    );

    let submit = mock.find("POST", "/fal-ai/veo3").expect("tanpa submit");
    assert_eq!(
        submit.auth.as_deref(),
        Some("Key FAL-MOCK"),
        "auth: {:?}",
        submit.auth
    );
    assert!(
        submit.body.contains("kucing astronot"),
        "body: {}",
        submit.body
    );
    assert!(
        mock.find("GET", "/req/r1/status").is_some(),
        "tanpa polling status"
    );
}

#[tokio::test]
async fn video_generate_failed_job_is_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut mock = MockServer::bind().await;
    let base = mock.url();
    mock.serve(vec![
        (
            "POST".into(),
            "/fal-ai/veo3".into(),
            200,
            format!(
                r#"{{"status_url":"{base}/req/r2/status","response_url":"{base}/req/r2","request_id":"r2"}}"#
            ),
        ),
        ("GET".into(), "/req/r2/status".into(), 200, r#"{"status":"FAILED"}"#.into()),
    ]);
    set_env("FAL_API_KEY", "FAL-MOCK");
    set_env("FAL_API_BASE", &mock.url());
    let (ctx, _guard) = test_ctx();
    let res = VideoGenerateTool
        .execute(args(serde_json::json!({"prompt": "x"})), &ctx)
        .await;
    remove_env("FAL_API_KEY");
    remove_env("FAL_API_BASE");
    let err = res.expect_err("seharusnya error saat job FAILED");
    assert!(err.to_string().contains("gagal"), "err: {err}");
}

// ---------------------------------------------------------------------------
// video_analyze (Gemini)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn video_analyze_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mock = MockServer::start(vec![(
        "POST".into(),
        "/v1beta/models/gemini-2.0-flash:generateContent".into(),
        200,
        r#"{"candidates":[{"content":{"parts":[{"text":"video tentang kucing"}]}}]}"#.into(),
    )])
    .await;
    set_env("GEMINI_API_KEY", "GEM-MOCK");
    set_env("GEMINI_API_BASE", &mock.url());

    let (ctx, _guard) = test_ctx();
    let vid = ctx.working_dir.join("klip.mp4");
    std::fs::write(&vid, b"FAKEVIDEOBYTES").unwrap();
    let res = VideoAnalyzeTool
        .execute(args(serde_json::json!({"source": "klip.mp4"})), &ctx)
        .await;
    remove_env("GEMINI_API_KEY");
    remove_env("GEMINI_API_BASE");
    let res = res.expect("video_analyze gagal");
    assert!(
        res.content.contains("video tentang kucing"),
        "content: {}",
        res.content
    );

    let rec = mock.find("POST", "/v1beta/models/").expect("tanpa request");
    assert!(rec.path.contains("key=GEM-MOCK"), "path: {}", rec.path);
    assert!(rec.body.contains("inlineData"), "body tidak ada inlineData");
    // base64 dari "FAKEVIDEOBYTES" = RkFLRVZJREVPQllURVM=
    assert!(
        rec.body.contains("RkFLRVZJREVPQllURVM="),
        "body: {}",
        &rec.body[..rec.body.len().min(400)]
    );
    assert!(
        rec.body.contains("video/mp4"),
        "body: {}",
        &rec.body[..rec.body.len().min(400)]
    );
}

// ---------------------------------------------------------------------------
// Modal (mock CLI via PATH)
// ---------------------------------------------------------------------------

/// Tulis skrip mock `modal` ke bindir; kembalikan bindir.
fn mock_modal_bin(log: &std::path::Path) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hermes-mockbin-{}-{}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let script = format!(
        "#!/bin/bash\necho \"ARGS:$*\" >> {}\necho '{{\"code\":0,\"stdout\":\"hello modal\",\"stderr\":\"\"}}'\n",
        log.display()
    );
    let path = dir.join("modal");
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

#[tokio::test]
async fn modal_exec_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (ctx, _guard) = test_ctx();
    let log = ctx.working_dir.join("modal.log");
    let bindir = mock_modal_bin(&log);
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{old_path}", bindir.display()));

    let res = ExecTool
        .execute(
            args(serde_json::json!({"command": "echo hi", "backend": "modal", "timeout": 30})),
            &ctx,
        )
        .await;

    std::env::set_var("PATH", &old_path);
    let _ = std::fs::remove_dir_all(&bindir);
    let res = res.expect("exec modal gagal");
    assert!(!res.is_error, "content: {}", res.content);
    assert!(
        res.content.contains("[exit: 0]"),
        "content: {}",
        res.content
    );
    assert!(
        res.content.contains("hello modal"),
        "content: {}",
        res.content
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(logged.contains("run"), "log: {logged}");
    assert!(logged.contains("--cmd"), "log: {logged}");
    assert!(logged.contains("echo hi"), "log: {logged}");
}

// ---------------------------------------------------------------------------
// Vercel Sandbox (mock @vercel/sandbox via node_modules lokal)
// ---------------------------------------------------------------------------

fn mock_vercel_sandbox_pkg() {
    let pkg_dir = std::env::temp_dir().join("hermes-vercel/node_modules/@vercel/sandbox");
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("package.json"),
        r#"{"name":"@vercel/sandbox","version":"0.0.0-test","type":"module","main":"index.js","exports":{".":"./index.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        pkg_dir.join("index.js"),
        r#"export class Sandbox {
  static async create() { return new Sandbox(); }
  async runCommand({ cmd }) {
    return {
      exitCode: 0,
      stdout: async () => `vercel stdout: ${cmd.join(" ")}`,
      stderr: async () => "",
    };
  }
  async stop() {}
}
"#,
    )
    .unwrap();
}

#[tokio::test]
async fn vercel_exec_ok() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    mock_vercel_sandbox_pkg();
    set_env("VERCEL_TOKEN", "VERCEL-MOCK");
    let (ctx, _guard) = test_ctx();
    let res = ExecTool
        .execute(
            args(serde_json::json!({"command": "echo hi", "backend": "vercel", "timeout": 60})),
            &ctx,
        )
        .await;
    remove_env("VERCEL_TOKEN");
    // Bersihkan mock agar tidak mengotori run berikutnya.
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join("hermes-vercel"));
    let res = res.expect("exec vercel gagal");
    assert!(!res.is_error, "content: {}", res.content);
    assert!(
        res.content.contains("[exit: 0]"),
        "content: {}",
        res.content
    );
    assert!(
        res.content.contains("vercel stdout"),
        "content: {}",
        res.content
    );
}
