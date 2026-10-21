use crate::credential_pool::SharedPool;
use async_trait::async_trait;
use hermes_core::{
    ChatRequest, ContentBlock, HermesError, LlmProvider, Result, Role, StreamEvent, Usage,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

use tokio::sync::mpsc;

/// Env tuple kredensial Anthropic, sesuai urutan `resolve_anthropic_token()` di
/// `agent/anthropic_credentials.py`: `ANTHROPIC_TOKEN` / `CLAUDE_CODE_OAUTH_TOKEN`
/// dulu (yang pertama ter-set menang), lalu `ANTHROPIC_API_KEY`. Hanya env
/// pertama yang ter-set (non-kosong) yang dibaca — nilai dari env lain di tuple
/// diabaikan agar tidak ada kredensial asing yang ikut terkirim.
pub const ANTHROPIC_CREDENTIAL_ENV_VARS: [&str; 3] = [
    "ANTHROPIC_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
];

/// Beta yang aman untuk request native biasa (paritas `_COMMON_BETAS`).
const COMMON_BETAS: &str = "interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
/// Beta wajib untuk auth OAuth/subscription (paritas `_OAUTH_ONLY_BETAS`).
const OAUTH_ONLY_BETAS: &str = "claude-code-20250219,oauth-2025-04-20";
/// Versi Claude Code fallback (paritas `_CLAUDE_CODE_VERSION_FALLBACK`).
const CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.74";

/// Gaya auth per request (paritas `_auth_style`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthStyle {
    /// API key Console: `x-api-key`.
    ApiKey,
    /// Token OAuth: `Authorization: Bearer` + header identity Claude Code.
    OAuth,
}

/// Bentuk token OAuth (paritas `_is_oauth_token` di `agent/anthropic_credentials.py`):
/// `sk-ant-oat*`, JWT `eyJ*`, `cc-*`. Keluarga API key Console (`sk-ant-api*`,
/// `sk-ant-usr*`, `sk-ant-svc*`, `sk-ant-admin*`) membayar per token dan diautentikasi
/// sebagai `x-api-key`, jadi identity Claude Code tidak boleh ikut. Prefix `sk-ant-*`
/// yang belum dikenal tetap diklasifikasikan sebagai OAuth agar prefix OAuth baru
/// dari Anthropic tetap berfungsi.
fn is_oauth_token(key: &str) -> bool {
    if key.is_empty()
        || key.starts_with("sk-ant-api")
        || key.starts_with("sk-ant-usr")
        || key.starts_with("sk-ant-svc")
        || key.starts_with("sk-ant-admin")
    {
        return false;
    }
    key.starts_with("sk-ant-") || key.starts_with("eyJ") || key.starts_with("cc-")
}

/// Paritas `is_native_anthropic_host`: host persis `api.anthropic.com`
/// (base kosong = default native).
fn is_native_anthropic_host(base_url: &str) -> bool {
    let text = base_url.trim();
    if text.is_empty() {
        return true;
    }
    let after_scheme = text.split("://").nth(1).unwrap_or(text);
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    host.eq_ignore_ascii_case("api.anthropic.com")
}

/// Paritas `_auth_style` + `anthropic_route_is_oauth`: identity OAuth (Bearer +
/// header Claude Code) hanya untuk host native dengan token berbentuk OAuth;
/// endpoint pihak ketiga tidak pernah mendapat header identity Claude Code
/// (di sana header tersebut 401/403).
fn auth_style(base_url: &str, key: &str) -> AuthStyle {
    if is_native_anthropic_host(base_url) && is_oauth_token(key) {
        AuthStyle::OAuth
    } else {
        AuthStyle::ApiKey
    }
}

/// Bentuk header `user-agent` identity Claude Code (paritas `build_anthropic_client`).
fn claude_code_user_agent(version: &str) -> String {
    format!("claude-code/{version} (external, cli)")
}

/// Coba `claude --version` (paritas `_get_claude_code_version`); `None` bila CLI
/// tidak ada di PATH atau gagal.
fn detect_claude_code_version() -> Option<String> {
    let mut child = std::process::Command::new("claude")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    // Tunggu maksimal ~2 detik agar request tidak macet bila CLI menggantung.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                // Output seperti "2.1.74 (Claude Code)"; ambil token pertama.
                let token = String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()?
                    .trim_start_matches('v')
                    .to_string();
                return token
                    .chars()
                    .next()
                    .filter(|c| c.is_ascii_digit())
                    .map(|_| token);
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return None,
        }
    }
}

/// Versi Claude Code untuk header identity, di-cache sekali per proses.
fn claude_code_version() -> String {
    static CACHE: OnceLock<String> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            detect_claude_code_version().unwrap_or_else(|| CLAUDE_CODE_VERSION_FALLBACK.to_string())
        })
        .clone()
}

/// Susun header untuk satu request (paritas `build_anthropic_client` +
/// `_new_sdk_client`): selalu tepat SATU kredensial — OAuth mengirim
/// `Authorization: Bearer` (tanpa `x-api-key`), API key mengirim `x-api-key`
/// (tanpa `Authorization`). Beta header selalu dikirim; path OAuth mendapat
/// beta OAuth + header identity Claude Code.
fn request_headers(base_url: &str, key: &str, claude_version: &str) -> Vec<(String, String)> {
    match auth_style(base_url, key) {
        AuthStyle::OAuth => vec![
            ("authorization".to_string(), format!("Bearer {key}")),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            (
                "anthropic-beta".to_string(),
                format!("{COMMON_BETAS},{OAUTH_ONLY_BETAS}"),
            ),
            (
                "user-agent".to_string(),
                claude_code_user_agent(claude_version),
            ),
            ("x-app".to_string(), "cli".to_string()),
        ],
        AuthStyle::ApiKey => vec![
            ("x-api-key".to_string(), key.to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ("anthropic-beta".to_string(), COMMON_BETAS.to_string()),
        ],
    }
}

/// Klien Anthropic Messages API dengan streaming SSE.
pub struct AnthropicProvider {
    base_url: String,
    default_model: String,
    pool: Arc<SharedPool>,
    client: Client,
}

/// Blok image Anthropic dari data URL atau URL http(s).
fn image_block(data_url: &str) -> Value {
    if let Some(rest) = data_url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(";base64,") {
            let media_type = meta.split(';').next().unwrap_or("image/png");
            return json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data}
            });
        }
    }
    json!({"type": "image", "source": {"type": "url", "url": data_url}})
}

impl AnthropicProvider {
    /// Normalisasi base URL. Paritas original
    /// (`_base_client_kwargs` di `agent/anthropic_adapter.py`): trailing
    /// `/v1` dihapus karena path `/v1/messages` ditambahkan sendiri saat
    /// membangun URL request; tanpa ini base `.../v1` menghasilkan
    /// `/v1/v1/messages`.
    fn normalize_base_url(base_url: &str) -> String {
        let trimmed = base_url.trim_end_matches('/');
        trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
    }

    pub fn new(
        base_url: impl Into<String>,
        default_model: impl Into<String>,
        api_keys: Vec<String>,
    ) -> Self {
        Self {
            base_url: Self::normalize_base_url(&base_url.into()),
            default_model: default_model.into(),
            pool: Arc::new(SharedPool::new(crate::CredentialPool::new(api_keys))),
            client: Client::new(),
        }
    }

    /// Kumpulkan kredensial dari tuple env [`ANTHROPIC_CREDENTIAL_ENV_VARS`]
    /// (yang pertama ter-set menang) plus key tambahan dari konfigurasi.
    /// Beberapa key dalam satu env dipisah koma, seperti `CredentialPool::from_env`.
    pub fn collect_keys(extra: &[String]) -> Vec<String> {
        let mut keys: Vec<String> = extra.to_vec();
        let env_keys = ANTHROPIC_CREDENTIAL_ENV_VARS
            .iter()
            .filter_map(|v| std::env::var(v).ok())
            .map(|v| v.trim().to_string())
            .find(|v| !v.is_empty());
        if let Some(val) = env_keys {
            keys.extend(
                val.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
            );
        }
        let mut seen = std::collections::HashSet::new();
        keys.retain(|k| seen.insert(k.clone()));
        keys
    }

    /// True bila ada satu pun env di [`ANTHROPIC_CREDENTIAL_ENV_VARS`] yang ter-set.
    pub fn has_env_credential() -> bool {
        ANTHROPIC_CREDENTIAL_ENV_VARS.iter().any(|v| {
            std::env::var(v)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
        })
    }

    /// Konversi pesan core -> format Anthropic (system dipisah, tool_use/tool_result).
    fn convert(req: &ChatRequest) -> (Option<String>, Vec<Value>) {
        let mut system_parts = Vec::new();
        let mut messages = Vec::new();
        for m in &req.messages {
            match m.role {
                Role::System => system_parts.push(m.text()),
                Role::User => {
                    messages.push(json!({"role": "user", "content": Self::blocks(&m.content)}))
                }
                Role::Assistant => {
                    messages.push(json!({"role": "assistant", "content": Self::blocks(&m.content)}))
                }
                Role::Tool => {
                    // Anthropic memakai role "user" berisi tool_result blocks.
                    messages.push(json!({"role": "user", "content": Self::blocks(&m.content)}));
                }
            }
        }
        // Gabungkan pesan user berurutan (Anthropic menolak dua user message berurutan).
        let mut merged: Vec<Value> = Vec::new();
        for msg in messages {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if let Some(last) = merged.last_mut() {
                let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
                if last_role == role {
                    let mut lc = last
                        .get("content")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut nc = msg
                        .get("content")
                        .and_then(|c| c.as_array())
                        .cloned()
                        .unwrap_or_default();
                    lc.append(&mut nc);
                    last["content"] = Value::Array(lc);
                    continue;
                }
            }
            merged.push(msg);
        }
        let system = if system_parts.is_empty() {
            None
        } else {
            // Paritas `_convert_system_content` (anthropic_message_convert.py):
            // bagian system digabung dengan SATU newline.
            Some(system_parts.join("\n"))
        };
        (system, merged)
    }

    fn blocks(content: &[ContentBlock]) -> Vec<Value> {
        let mut out = Vec::new();
        for b in content {
            match b {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        out.push(json!({"type": "text", "text": text}));
                    }
                }
                ContentBlock::ToolCall { call } => {
                    out.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                ContentBlock::ToolResult { result } => {
                    out.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": result.content,
                        "is_error": result.is_error,
                    }));
                }
                ContentBlock::Image { data_url } => {
                    out.push(image_block(data_url));
                }
            }
        }
        if out.is_empty() {
            out.push(json!({"type": "text", "text": ""}));
        }
        out
    }

    fn convert_tools(req: &ChatRequest) -> Vec<Value> {
        // Paritas `convert_tools_to_anthropic` (anthropic_message_convert.py):
        // nama tool duplikat dibuang dengan peringatan — Anthropic 400 bila
        // ada nama duplikat. Nama kosong tidak ikut didedup.
        let mut seen = std::collections::HashSet::new();
        req.tools
            .iter()
            .filter(|t| {
                if t.name.is_empty() {
                    return true;
                }
                if !seen.insert(t.name.clone()) {
                    tracing::warn!(
                        "convert_tools_to_anthropic: nama tool duplikat '{}' dibuang",
                        t.name
                    );
                    return false;
                }
                true
            })
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect()
    }

    /// Bangun body JSON Messages API. Paritas `build_anthropic_kwargs`
    /// (agent/anthropic_adapter.py): `tools` dihilangkan bila kosong,
    /// `temperature` dihilangkan bila `None`, `max_tokens` selalu dikirim
    /// (wajib di Messages API).
    fn build_body(req: &ChatRequest) -> Value {
        let (system, messages) = Self::convert(req);
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "max_tokens": req.max_tokens.unwrap_or(8192),
        });
        let tools = Self::convert_tools(req);
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(s) = system {
            body["system"] = Value::String(s);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        // B3: effort reasoning → wire Anthropic `thinking` (butuh budget
        // numerik; dipetakan dari tangga kanonis). `none`/tak dikenal =
        // tidak dikirim (default provider).
        if let Some(level) = req.reasoning_effort.as_deref() {
            if let Some(budget) = hermes_core::thinking_budget_for_effort(level) {
                body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
            }
        }
        body
    }
}

/// Gabungkan satu objek `usage` SSE Anthropic ke akumulator.
///
/// `message_start` membawa `input_tokens` (plus field cache bila ada),
/// `message_delta` membawa `output_tokens`. Field yang absen di payload
/// tidak menimpa nilai yang sudah terakumulasi — paritas
/// `accumulate_event()` SDK Anthropic yang dipakai original
/// (`agent/anthropic_adapter.py`): usage akhir = usage `message_start` +
/// `output_tokens` dari `message_delta`.
fn merge_usage(acc: &mut Usage, u: &Value) {
    if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
        acc.input_tokens = n;
    }
    if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
        acc.output_tokens = n;
    }
    if let Some(n) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
        acc.cache_read_input_tokens = Some(n);
    }
    if let Some(n) = u.get("cache_creation_input_tokens").and_then(Value::as_u64) {
        acc.cache_creation_input_tokens = Some(n);
    }
}

/// Hasil pemrosesan satu baris SSE oleh [`AnthropicStreamParser`].
#[derive(Debug)]
enum SseAction {
    /// Teruskan event ini ke penerima stream.
    Emit(StreamEvent),
    /// `message_stop` diterima: stream selesai.
    Done,
    /// Baris bukan event yang relevan; abaikan.
    Ignore,
    /// Chunk malformed: gugurkan stream dengan error (paritas SDK yang
    /// melempar dari `sse.json()` pada payload non-JSON).
    Error(String),
}

/// Parser SSE streaming Anthropic yang stateful dan murni (tanpa I/O),
/// sehingga urutan event kaleng bisa diuji tanpa HTTP.
struct AnthropicStreamParser {
    usage: Usage,
    block_index: usize,
    tool_id: String,
    tool_name: String,
    /// Paritas `_stream_final_message` (anthropic_adapter.py): stream harus
    /// diakhiri `message_stop`; EOF tanpanya adalah error, bukan Done.
    saw_message_stop: bool,
}

impl AnthropicStreamParser {
    fn new() -> Self {
        Self {
            usage: Usage::default(),
            block_index: 0,
            tool_id: String::new(),
            tool_name: String::new(),
            saw_message_stop: false,
        }
    }

    fn usage(&self) -> &Usage {
        &self.usage
    }

    fn saw_message_stop(&self) -> bool {
        self.saw_message_stop
    }

    /// Proses satu baris SSE mentah (`event: ...` / `data: ...`).
    fn feed_line(&mut self, line: &str) -> SseAction {
        if let Some(data) = line.strip_prefix("event:") {
            return match data.trim() {
                "message_stop" => {
                    self.saw_message_stop = true;
                    SseAction::Done
                }
                // `content_block_start` sebagai baris event tidak membawa
                // detail; detailnya ada di payload `data:` berikutnya.
                _ => SseAction::Ignore,
            };
        }
        let Some(data) = line.strip_prefix("data:") else {
            return SseAction::Ignore;
        };
        let data = data.trim();
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => {
                let sample: String = data.chars().take(120).collect();
                return SseAction::Error(format!(
                    "chunk SSE Anthropic non-JSON, stream digugurkan: {sample}"
                ));
            }
        };
        let ev_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ev_type {
            "message_start" => {
                // `usage` ada di dalam `message`, bukan di top-level.
                if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
                    merge_usage(&mut self.usage, u);
                }
                SseAction::Ignore
            }
            "content_block_start" => {
                self.block_index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
                if let Some(cb) = v.get("content_block") {
                    self.tool_id = cb
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.tool_name = cb
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                }
                SseAction::Ignore
            }
            "content_block_delta" => {
                let Some(delta) = v.get("delta") else {
                    return SseAction::Ignore;
                };
                let d_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match d_type {
                    "text_delta" => match delta.get("text").and_then(|t| t.as_str()) {
                        Some(t) => SseAction::Emit(StreamEvent::TextDelta(t.to_string())),
                        None => SseAction::Ignore,
                    },
                    "input_json_delta" => {
                        match delta.get("partial_json").and_then(|p| p.as_str()) {
                            Some(pj) => SseAction::Emit(StreamEvent::ToolCallDelta {
                                index: self.block_index,
                                id: Some(self.tool_id.clone()),
                                name: Some(self.tool_name.clone()),
                                arguments_delta: pj.to_string(),
                            }),
                            None => SseAction::Ignore,
                        }
                    }
                    _ => SseAction::Ignore,
                }
            }
            "message_delta" => {
                if let Some(u) = v.get("usage") {
                    merge_usage(&mut self.usage, u);
                }
                SseAction::Ignore
            }
            _ => SseAction::Ignore,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = Self::build_body(&req);

        let api_key = self.pool.next_key().ok_or_else(|| {
            HermesError::Provider(
                "tidak ada kredensial Anthropic (set ANTHROPIC_TOKEN, CLAUDE_CODE_OAUTH_TOKEN, atau ANTHROPIC_API_KEY)"
                    .to_string(),
            )
        })?;
        // Gaya auth per request mengikuti bentuk kredensial (paritas `_auth_style`):
        // deteksi versi Claude Code hanya dijalankan untuk path OAuth.
        let headers = if auth_style(&self.base_url, &api_key) == AuthStyle::OAuth {
            request_headers(&self.base_url, &api_key, &claude_code_version())
        } else {
            request_headers(&self.base_url, &api_key, "")
        };
        let mut req_builder = self.client.post(&url);
        for (name, value) in headers {
            req_builder = req_builder.header(name, value);
        }
        let resp = req_builder.json(&body).send().await.map_err(|e| {
            let err = HermesError::transport(e.is_timeout(), format!("request gagal: {e}"));
            self.pool.report_failure(&api_key, &err);
            err
        })?;
        if !resp.status().is_success() {
            let err = crate::provider_http_error(resp).await;
            self.pool.report_failure(&api_key, &err);
            return Err(err);
        }
        self.pool.report_success(&api_key);

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut parser = AnthropicStreamParser::new();
            while let Some(chunk) = stream.next().await {
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx
                            .send(StreamEvent::Error(format!("stream error: {e}")))
                            .await;
                        return;
                    }
                };
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim().to_string();
                    buf = buf[pos + 1..].to_string();
                    if line.is_empty() {
                        continue;
                    }
                    match parser.feed_line(&line) {
                        SseAction::Emit(ev) => {
                            if tx.send(ev).await.is_err() {
                                return;
                            }
                        }
                        SseAction::Done => {
                            let _ = tx.send(StreamEvent::Done(parser.usage().clone())).await;
                            return;
                        }
                        SseAction::Ignore => {}
                        SseAction::Error(e) => {
                            let _ = tx.send(StreamEvent::Error(e)).await;
                            return;
                        }
                    }
                }
            }
            // Frame terakhir tanpa newline tetap diproses.
            if !buf.trim().is_empty() {
                match parser.feed_line(buf.trim()) {
                    SseAction::Emit(ev) => {
                        let _ = tx.send(ev).await;
                    }
                    SseAction::Done => {
                        let _ = tx.send(StreamEvent::Done(parser.usage().clone())).await;
                        return;
                    }
                    SseAction::Ignore => {}
                    SseAction::Error(e) => {
                        let _ = tx.send(StreamEvent::Error(e)).await;
                        return;
                    }
                }
            }
            // Paritas `_stream_final_message`: stream yang berakhir sebelum
            // `message_stop` adalah error (EmptyStreamError di original),
            // bukan Done — retry di level agen yang memutuskan.
            if parser.saw_message_stop() {
                let _ = tx.send(StreamEvent::Done(parser.usage().clone())).await;
            } else {
                let _ = tx
                    .send(StreamEvent::Error(
                        "Anthropic stream berakhir sebelum message_stop \
                         (kemungkinan stream terputus di tengah)"
                            .to_string(),
                    ))
                    .await;
            }
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env var bersifat global per proses; serialkan tes yang menyentuhnya.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Set env tuple ke *vars* (kosongkan dulu semuanya), kembalikan guard lock.
    fn set_credential_env(vars: &[(&str, &str)]) -> std::sync::MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().unwrap();
        for v in ANTHROPIC_CREDENTIAL_ENV_VARS {
            std::env::remove_var(v);
        }
        for (k, val) in vars {
            std::env::set_var(k, val);
        }
        guard
    }

    fn header_map(headers: &[(String, String)]) -> std::collections::HashMap<&str, &str> {
        headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn normalize_base_url_strips_trailing_v1_variants() {
        // (input, base yang diharapkan)
        let cases = [
            ("https://api.anthropic.com/v1", "https://api.anthropic.com"),
            ("https://api.anthropic.com/v1/", "https://api.anthropic.com"),
            ("https://api.anthropic.com", "https://api.anthropic.com"),
            ("https://api.anthropic.com/", "https://api.anthropic.com"),
            // Bukan sufiks "/v1" persis -> jangan disentuh.
            (
                "https://proxy.example.com/v12",
                "https://proxy.example.com/v12",
            ),
            (
                "https://proxy.example.com/apiv1",
                "https://proxy.example.com/apiv1",
            ),
            (
                "https://proxy.example.com/foo/v1",
                "https://proxy.example.com/foo",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(
                AnthropicProvider::normalize_base_url(input),
                expected,
                "input {input}"
            );
        }
    }

    #[test]
    fn provider_new_never_produces_double_v1_path() {
        let p = AnthropicProvider::new("https://api.anthropic.com/v1/", "m", vec![]);
        assert!(!p.base_url.ends_with("/v1"));
        assert_eq!(
            format!("{}/v1/messages", p.base_url),
            "https://api.anthropic.com/v1/messages"
        );
    }

    #[test]
    fn oauth_token_shape_classification_mirrors_original() {
        // Nilai fake; hanya bentuknya yang diuji.
        let oauth = [
            "sk-ant-oat01-FAKE",
            "sk-ant-FAKE-prefix-belum-dikenal",
            "eyJhbGciOiJIUzI1NiJ9.FAKE",
            "cc-FAKE",
        ];
        for k in oauth {
            assert!(is_oauth_token(k), "{k} seharusnya OAuth");
        }
        // Keluarga API key Console -> x-api-key, bukan OAuth.
        let api_keys = [
            "sk-ant-api03-FAKE",
            "sk-ant-usr01-FAKE",
            "sk-ant-svc01-FAKE",
            "sk-ant-admin01-FAKE",
            "",
            "kunci-acak-tanpa-awalan",
        ];
        for k in api_keys {
            assert!(!is_oauth_token(k), "{k:?} seharusnya API key");
        }
    }

    #[test]
    fn auth_style_gates_oauth_identity_on_native_host() {
        // Host native + token OAuth -> OAuth.
        assert_eq!(
            auth_style("https://api.anthropic.com", "sk-ant-oat01-FAKE"),
            AuthStyle::OAuth
        );
        assert_eq!(
            auth_style("https://api.anthropic.com/", "eyJhbGciOiJIUzI1NiJ9.FAKE"),
            AuthStyle::OAuth
        );
        // Host native + API key Console -> x-api-key.
        assert_eq!(
            auth_style("https://api.anthropic.com", "sk-ant-api03-FAKE"),
            AuthStyle::ApiKey
        );
        // Endpoint pihak ketiga tidak pernah mendapat identity OAuth
        // (paritas `anthropic_route_is_oauth`), walau token berbentuk OAuth.
        assert_eq!(
            auth_style("https://proxy.example.com/anthropic", "sk-ant-oat01-FAKE"),
            AuthStyle::ApiKey
        );
        assert_eq!(
            auth_style("https://proxy.example.com:8443", "cc-FAKE"),
            AuthStyle::ApiKey
        );
        // Host lain dengan API key -> x-api-key.
        assert_eq!(
            auth_style("https://proxy.example.com", "sk-ant-api03-FAKE"),
            AuthStyle::ApiKey
        );
    }

    #[test]
    fn native_host_detection() {
        assert!(is_native_anthropic_host(""));
        assert!(is_native_anthropic_host("https://api.anthropic.com"));
        assert!(is_native_anthropic_host("https://api.anthropic.com/v1"));
        assert!(is_native_anthropic_host("https://API.ANTHROPIC.COM"));
        assert!(!is_native_anthropic_host("https://proxy.example.com"));
        assert!(!is_native_anthropic_host(
            "https://api.anthropic.com.evil.example"
        ));
    }

    #[test]
    fn claude_code_user_agent_exact_format() {
        assert_eq!(
            claude_code_user_agent("2.1.74"),
            "claude-code/2.1.74 (external, cli)"
        );
    }

    #[test]
    fn api_key_path_sends_x_api_key_and_common_betas_only() {
        let headers = request_headers(
            "https://api.anthropic.com",
            "sk-ant-api03-FAKEKEY",
            "2.1.74",
        );
        let map = header_map(&headers);
        assert_eq!(map.get("x-api-key"), Some(&"sk-ant-api03-FAKEKEY"));
        assert!(
            !map.contains_key("authorization"),
            "path API key tidak boleh mengirim Authorization"
        );
        assert_eq!(
            map.get("anthropic-beta"),
            Some(&"interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14")
        );
        assert_eq!(map.get("anthropic-version"), Some(&"2023-06-01"));
        assert!(
            !map.contains_key("x-app") && !map.contains_key("user-agent"),
            "path API key tidak boleh mengirim identity Claude Code"
        );
    }

    #[test]
    fn oauth_path_sends_bearer_identity_and_oauth_betas() {
        let headers = request_headers(
            "https://api.anthropic.com",
            "sk-ant-oat01-FAKEKEY",
            "2.1.74",
        );
        let map = header_map(&headers);
        assert_eq!(
            map.get("authorization"),
            Some(&"Bearer sk-ant-oat01-FAKEKEY")
        );
        assert!(
            !map.contains_key("x-api-key"),
            "path OAuth tidak boleh mengirim x-api-key (tepat satu kredensial)"
        );
        assert_eq!(
            map.get("user-agent"),
            Some(&"claude-code/2.1.74 (external, cli)")
        );
        assert_eq!(map.get("x-app"), Some(&"cli"));
        assert_eq!(
            map.get("anthropic-beta"),
            Some(
                &"interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14,claude-code-20250219,oauth-2025-04-20"
            )
        );
        assert_eq!(map.get("anthropic-version"), Some(&"2023-06-01"));
    }

    #[test]
    fn env_tuple_first_set_wins() {
        // Semua ter-set -> ANTHROPIC_TOKEN menang.
        {
            let _g = set_credential_env(&[
                ("ANTHROPIC_TOKEN", "FAKE-TOKEN"),
                ("CLAUDE_CODE_OAUTH_TOKEN", "FAKE-OAUTH"),
                ("ANTHROPIC_API_KEY", "FAKE-KEY"),
            ]);
            assert_eq!(AnthropicProvider::collect_keys(&[]), vec!["FAKE-TOKEN"]);
            assert!(AnthropicProvider::has_env_credential());
        }
        // ANTHROPIC_TOKEN kosong -> CLAUDE_CODE_OAUTH_TOKEN berikutnya.
        {
            let _g = set_credential_env(&[
                ("ANTHROPIC_TOKEN", "   "),
                ("CLAUDE_CODE_OAUTH_TOKEN", "FAKE-OAUTH"),
                ("ANTHROPIC_API_KEY", "FAKE-KEY"),
            ]);
            assert_eq!(AnthropicProvider::collect_keys(&[]), vec!["FAKE-OAUTH"]);
        }
        // Hanya ANTHROPIC_API_KEY.
        {
            let _g = set_credential_env(&[("ANTHROPIC_API_KEY", "sk-ant-api03-FAKE")]);
            assert_eq!(
                AnthropicProvider::collect_keys(&[]),
                vec!["sk-ant-api03-FAKE"]
            );
        }
        // Tidak ada yang ter-set.
        {
            let _g = set_credential_env(&[]);
            assert!(AnthropicProvider::collect_keys(&[]).is_empty());
            assert!(!AnthropicProvider::has_env_credential());
        }
    }

    #[test]
    fn collect_keys_supports_comma_separated_and_config_extra() {
        let _g = set_credential_env(&[("ANTHROPIC_API_KEY", "sk-ant-api03-A, sk-ant-api03-B")]);
        assert_eq!(
            AnthropicProvider::collect_keys(&["sk-ant-api03-CONFIG".to_string()]),
            vec!["sk-ant-api03-CONFIG", "sk-ant-api03-A", "sk-ant-api03-B"]
        );
    }

    /// Jalankan urutan baris SSE mentah lewat parser murni; kembalikan
    /// semua event yang di-emit plus usage akhir dari `Done` (bila ada).
    /// `Error` dari parser diteruskan sebagai `StreamEvent::Error` agar
    /// terlihat di hasil tes.
    fn run_sse(lines: &[&str]) -> (Vec<StreamEvent>, Option<Usage>) {
        let mut parser = AnthropicStreamParser::new();
        let mut events = Vec::new();
        let mut done = None;
        for line in lines {
            match parser.feed_line(line) {
                SseAction::Emit(ev) => events.push(ev),
                SseAction::Done => {
                    let u = parser.usage().clone();
                    events.push(StreamEvent::Done(u.clone()));
                    done = Some(u);
                    break;
                }
                SseAction::Ignore => {}
                SseAction::Error(e) => {
                    events.push(StreamEvent::Error(e));
                    break;
                }
            }
        }
        (events, done)
    }

    #[test]
    fn sse_usage_merges_message_start_and_message_delta() {
        // Urutan SSE kaleng: message_start (input_tokens + cache) ->
        // delta konten -> message_delta (output_tokens) -> message_stop.
        let lines = [
            "event: message_start",
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-x","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":2}}}"#,
            "event: content_block_start",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Halo"}}"#,
            "event: message_delta",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":5}}"#,
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
        ];
        let (events, done) = run_sse(&lines);
        let usage = done.expect("harus ada StreamEvent::Done");
        // Keduanya harus terisi: input dari message_start, output dari delta.
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
        // Field cache dari message_start tidak boleh hilang saat delta
        // (yang tidak membawa field cache) di-merge.
        assert_eq!(usage.cache_read_input_tokens, Some(2));
        assert_eq!(usage.cache_creation_input_tokens, Some(4));
        // Delta teks tetap mengalir seperti biasa.
        assert!(events
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta(t) if t == "Halo")));
    }

    #[test]
    fn sse_usage_without_message_start_keeps_delta_only() {
        // Tanpa message_start: usage hanya dari message_delta (regresi).
        let lines = [
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            "event: message_delta",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":7}}"#,
            "event: message_stop",
        ];
        let (_, done) = run_sse(&lines);
        let usage = done.expect("harus ada StreamEvent::Done");
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 7);
        assert_eq!(usage.cache_read_input_tokens, None);
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    #[test]
    fn sse_merge_usage_does_not_clobber_absent_fields() {
        // Field yang absen di payload tidak boleh menimpa akumulator.
        let mut acc = Usage {
            input_tokens: 10,
            cache_read_input_tokens: Some(2),
            ..Default::default()
        };
        let u: Value = serde_json::from_str(r#"{"output_tokens":5}"#).expect("json valid");
        merge_usage(&mut acc, &u);
        assert_eq!(acc.input_tokens, 10);
        assert_eq!(acc.output_tokens, 5);
        assert_eq!(acc.cache_read_input_tokens, Some(2));
        assert_eq!(acc.cache_creation_input_tokens, None);
    }

    #[test]
    fn body_omits_empty_tools_and_none_temperature() {
        // Paritas `build_anthropic_kwargs`: `tools` dihilangkan bila kosong,
        // `temperature` dihilangkan bila None; `max_tokens` selalu ada.
        use hermes_core::{ChatRequest, Message, Role};
        let req = ChatRequest {
            model: "claude-x".to_string(),
            messages: vec![Message {
                role: Role::User,
                content: vec![hermes_core::ContentBlock::Text { text: "hi".into() }],
                name: None,
            }],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
        };
        let body = AnthropicProvider::build_body(&req);
        let obj = body.as_object().expect("body harus objek JSON");
        assert!(!obj.contains_key("tools"), "tools kosong harus dihilangkan");
        assert!(
            !obj.contains_key("temperature"),
            "temperature None harus dihilangkan"
        );
        assert!(obj.contains_key("max_tokens"), "max_tokens wajib ada");
        assert!(
            !obj.contains_key("system"),
            "system kosong harus dihilangkan"
        );
    }

    #[test]
    fn sse_eof_before_message_stop_is_an_error() {
        // Paritas `_stream_final_message`: EOF tanpa `message_stop` adalah
        // error (EmptyStreamError di original), bukan Done.
        let lines = [
            "event: message_start",
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":3}}}"#,
            "event: content_block_delta",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"setengah"}}"#,
        ];
        let mut parser = AnthropicStreamParser::new();
        for line in lines {
            assert!(!matches!(parser.feed_line(line), SseAction::Done));
        }
        assert!(
            !parser.saw_message_stop(),
            "tanpa message_stop, flag harus false"
        );
        // Dengan message_stop, flag menyala.
        let mut parser2 = AnthropicStreamParser::new();
        assert!(matches!(
            parser2.feed_line("event: message_stop"),
            SseAction::Done
        ));
        assert!(parser2.saw_message_stop());
    }

    #[test]
    fn sse_malformed_data_chunk_is_an_error() {
        // Paritas SDK: payload `data:` non-JSON menggugurkan stream.
        let mut parser = AnthropicStreamParser::new();
        match parser.feed_line("data: bukan json {{{") {
            SseAction::Error(_) => {}
            other => panic!("harusnya Error, dapat {other:?}"),
        }
    }

    #[test]
    fn convert_joins_system_with_single_newline() {
        // Paritas `_convert_system_content`: "\n", bukan "\n\n".
        use hermes_core::{ContentBlock, Message, Role};
        let req = ChatRequest {
            model: "claude-x".to_string(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: vec![ContentBlock::Text {
                        text: "satu".into(),
                    }],
                    name: None,
                },
                Message {
                    role: Role::System,
                    content: vec![ContentBlock::Text { text: "dua".into() }],
                    name: None,
                },
            ],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
        };
        let (system, _) = AnthropicProvider::convert(&req);
        assert_eq!(system.as_deref(), Some("satu\ndua"));
    }

    #[test]
    fn convert_tools_dedups_duplicate_names() {
        // Paritas `convert_tools_to_anthropic`: duplikat dibuang.
        let mk = |name: &str| hermes_core::ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        };
        let req = ChatRequest {
            model: "claude-x".to_string(),
            messages: vec![],
            tools: vec![mk("a"), mk("b"), mk("a")],
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
        };
        let tools = AnthropicProvider::convert_tools(&req);
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }
}
