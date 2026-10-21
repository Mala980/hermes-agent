use crate::credential_pool::SharedPool;
use async_trait::async_trait;
use hermes_core::{
    ChatRequest, ContentBlock, HermesError, LlmProvider, Message, Result, Role, StreamEvent,
    ToolDefinition, Usage,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Klien OpenAI-compatible dengan streaming SSE.
/// Dipakai untuk OpenAI, OpenRouter, Nous Portal, Ollama, vLLM, dan endpoint
/// OpenAI-compatible lainnya — sesuai sifat model-agnostic Hermes.
pub struct OpenAiCompatProvider {
    name: String,
    base_url: String,
    default_model: String,
    pool: Arc<SharedPool>,
    client: Client,
    /// Header tambahan (mis. HTTP-Referer untuk OpenRouter).
    extra_headers: Vec<(String, String)>,
    /// Bila true: auth via header `api-key` (Azure), dan base_url sudah URL penuh.
    azure_api_key_header: bool,
    /// Path chat bila base_url belum lengkap (default "/chat/completions").
    chat_path: String,
}

impl OpenAiCompatProvider {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        default_model: impl Into<String>,
        api_keys: Vec<String>,
    ) -> Self {
        let name = name.into();
        let mut extra_headers = Vec::new();
        if name == "openrouter" {
            extra_headers.push((
                "HTTP-Referer".to_string(),
                "https://hermes-rs.local".to_string(),
            ));
            extra_headers.push(("X-Title".to_string(), "Hermes Agent (Rust)".to_string()));
        }
        Self {
            name,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            default_model: default_model.into(),
            pool: Arc::new(SharedPool::new(crate::CredentialPool::new(api_keys))),
            client: Client::new(),
            extra_headers,
            azure_api_key_header: false,
            chat_path: "/chat/completions".to_string(),
        }
    }

    /// Azure OpenAI: URL deployment + header `api-key` (bukan Bearer).
    ///
    /// `resource`: nama resource Azure (xxx.openai.azure.com), `deployment`: nama deployment,
    /// `api_version`: mis. "2024-10-21". Key dari env `AZURE_OPENAI_API_KEY`.
    pub fn new_azure(
        resource: impl Into<String>,
        deployment: impl Into<String>,
        api_version: impl Into<String>,
        api_keys: Vec<String>,
    ) -> Self {
        let url = format!(
            "https://{}.openai.azure.com/openai/deployments/{}/chat/completions?api-version={}",
            resource.into(),
            deployment.into(),
            api_version.into(),
        );
        Self {
            name: "azure".to_string(),
            base_url: url,
            default_model: String::new(),
            pool: Arc::new(SharedPool::new(crate::CredentialPool::new(api_keys))),
            client: Client::new(),
            extra_headers: Vec::new(),
            azure_api_key_header: true,
            chat_path: String::new(),
        }
    }

    /// Azure Foundry: base URL penuh milik user dipakai verbatim — tidak ditambahi
    /// path deployment maupun query `api-version` (paritas profil `azure-foundry`
    /// di original: `AZURE_FOUNDRY_BASE_URL` + `AZURE_FOUNDRY_API_KEY`).
    ///
    /// Auth tetap via header `api-key` seperti mode legacy Azure.
    ///
    /// BUKAN dalam cakupan port ini: Entra ID (aliran OAuth/MSAL untuk memperoleh
    /// bearer token) — tidak ada implementasi MSAL di port ini. Endpoint Foundry
    /// yang memakai api-key tetap berfungsi; endpoint yang mewajibkan Entra ID
    /// token akan ditolak server (401) seperti mestinya.
    pub fn new_azure_foundry(base_url: impl Into<String>, api_keys: Vec<String>) -> Self {
        let mut p = Self::new("azure-foundry", base_url, String::new(), api_keys);
        p.azure_api_key_header = true;
        p.chat_path.clear();
        p
    }

    /// Kumpulkan API key dari env var + config menjadi satu vec untuk pool.
    pub fn collect_keys(env_var: Option<&str>, extra: &[String]) -> Vec<String> {
        let mut keys: Vec<String> = extra.to_vec();
        if let Some(v) = env_var {
            if let Ok(val) = std::env::var(v) {
                keys.extend(
                    val.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
        }
        let mut seen = std::collections::HashSet::new();
        keys.retain(|k| seen.insert(k.clone()));
        keys
    }

    fn convert_messages(messages: &[Message]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "tool",
                };
                let mut obj = serde_json::Map::new();
                obj.insert("role".to_string(), Value::String(role.to_string()));
                let mut texts = Vec::new();
                let mut tool_calls = Vec::new();
                let mut tool_call_id = None;
                let mut images = Vec::new();
                for b in &m.content {
                    match b {
                        ContentBlock::Text { text } => texts.push(text.clone()),
                        ContentBlock::Image { data_url } => images.push(data_url.clone()),
                        ContentBlock::ToolCall { call } => {
                            tool_calls.push(json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": args_to_string(&call.arguments),
                                }
                            }));
                        }
                        ContentBlock::ToolResult { result } => {
                            tool_call_id = Some(result.tool_call_id.clone());
                            texts.push(result.content.clone());
                        }
                    }
                }
                let text = texts.join("");
                if m.role == Role::Tool {
                    obj.insert(
                        "tool_call_id".to_string(),
                        Value::String(tool_call_id.unwrap_or_default()),
                    );
                    obj.insert("content".to_string(), Value::String(text));
                } else {
                    let has_tool_calls = !tool_calls.is_empty();
                    if has_tool_calls {
                        obj.insert("tool_calls".to_string(), Value::Array(tool_calls));
                    }
                    // Beberapa provider menolak content kosong saat ada tool_calls.
                    // Bila ada gambar, kirim content sebagai array part (teks + image_url).
                    if !images.is_empty() {
                        let mut parts = Vec::new();
                        if !text.is_empty() {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        for url in &images {
                            parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                        obj.insert("content".to_string(), Value::Array(parts));
                    } else {
                        let content = if text.is_empty() && has_tool_calls {
                            Value::Null
                        } else {
                            Value::String(text)
                        };
                        obj.insert("content".to_string(), content);
                    }
                }
                Value::Object(obj)
            })
            .collect()
    }

    /// URL chat-completions efektif: `base_url` dipakai apa adanya bila
    /// `chat_path` kosong (mode Azure legacy / Foundry), kalau tidak ditempel
    /// `chat_path`.
    fn chat_url(&self) -> String {
        if self.chat_path.is_empty() {
            self.base_url.clone()
        } else {
            format!("{}{}", self.base_url, self.chat_path)
        }
    }

    /// Bangun body JSON chat-completions. Satu-satunya tempat request chat
    /// dirakit — dipakai `chat_stream` (dan lewatnya `chat_simple`), sehingga
    /// pemilihan nama parameter token-limit berlaku untuk chat maupun streaming.
    ///
    /// Paritas wire `agent/transports/chat_completions.py` (`_base_kwargs`,
    /// `_apply_max_tokens`): `tools` dihilangkan bila kosong, `temperature`
    /// dan kunci batas token dihilangkan bila `None` (tidak pernah `null`),
    /// dan nama tool duplikat dibuang.
    fn build_chat_body(req: &ChatRequest) -> Value {
        let mut body = json!({
            "model": req.model,
            "messages": Self::convert_messages(&req.messages),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        let tools = Self::convert_tools(&req.tools);
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(t) = req.temperature {
            body["temperature"] = json!(t);
        }
        // P11: keluarga model OpenAI yang menolak `max_tokens` (HTTP 400
        // `unsupported_parameter`) dikirimi `max_completion_tokens` — aturan
        // sama persis dengan `utils.model_forces_max_completion_tokens` di original.
        // Bila `None`, kunci dihilangkan (original tidak pernah mengirim null).
        let key = if model_forces_max_completion_tokens(&req.model) {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        if let Some(mt) = req.max_tokens {
            body[key] = json!(mt);
        }
        // B3: effort reasoning diteruskan sebagai parameter wire asli
        // OpenAI `reasoning_effort` (didukung juga oleh banyak endpoint
        // OpenAI-compatible). `none` = tidak dikirim (default provider).
        if let Some(level) = req.reasoning_effort.as_deref().filter(|l| *l != "none") {
            body["reasoning_effort"] = json!(level);
        }
        body
    }

    fn convert_tools(tools: &[ToolDefinition]) -> Vec<Value> {
        // Paritas `_dedupe_tool_names` (agent/auxiliary_client.py): nama tool
        // duplikat dibuang dengan peringatan (yang pertama menang); nama
        // kosong tidak ikut didedup, seperti aslinya.
        let mut seen = std::collections::HashSet::new();
        tools
            .iter()
            .filter(|t| {
                if t.name.is_empty() {
                    return true;
                }
                if !seen.insert(t.name.clone()) {
                    tracing::warn!("nama tool duplikat '{}' dibuang", t.name);
                    return false;
                }
                true
            })
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect()
    }
}

/// Hasil pemrosesan satu baris SSE oleh [`handle_sse_line`].
#[derive(Debug)]
enum SseLineAction {
    /// Teruskan event-event ini ke penerima stream (sesuai urutan).
    Emit(Vec<StreamEvent>),
    /// `data: [DONE]`: stream selesai normal.
    Done,
    /// Baris bukan event yang relevan; abaikan.
    Ignore,
    /// Chunk malformed / berbentuk error: gugurkan stream dengan error.
    Error(String),
}

/// Bentuk error dalam payload SSE, paritas `_payload_has_error_shape`
/// (agent/chat_completion_helpers.py) + `_choiceless_chunk` (DeepInfra
/// mengirim error validasi sebagai chunk in-stream tanpa `choices`).
/// Error seperti ini tidak boleh hilang diam-diam.
fn stream_error_shape(v: &Value) -> Option<String> {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .or_else(|| err.as_str())
            .unwrap_or("provider stream error");
        let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("");
        return Some(if code.is_empty() {
            msg.to_string()
        } else {
            format!("{code}: {msg}")
        });
    }
    // DeepInfra: choices=null + error_type/error_message.
    let err_type = v.get("error_type").and_then(|s| s.as_str());
    let err_msg = v.get("error_message").and_then(|s| s.as_str());
    if err_type.is_some() || err_msg.is_some() {
        return Some(format!(
            "{}: {}",
            err_type.unwrap_or("provider_in_stream_error"),
            err_msg.unwrap_or("")
        ));
    }
    // Bentuk generik: `message` + `code`/`error_code`.
    if let Some(msg) = v.get("message").and_then(|m| m.as_str()) {
        if v.get("code").is_some() || v.get("error_code").is_some() {
            return Some(msg.to_string());
        }
    }
    None
}

/// Proses satu baris SSE mentah menjadi aksi. Murni (tanpa I/O) agar urutan
/// chunk kaleng bisa diuji tanpa HTTP.
///
/// Paritas `_iter_provider_stream_chunks` (agent/chat_completion_helpers.py):
/// chunk `data:` yang bukan JSON valid menggugurkan stream sebagai
/// `ProviderStreamError` (tidak dilewati diam-diam); frame `data:` kosong
/// (keepalive tanpa payload) juga menggugurkan stream, bukan diterima sebagai
/// jawaban kosong.
fn handle_sse_line(line: &str, usage: &mut Usage) -> SseLineAction {
    let line = line.trim();
    if line.is_empty() || !line.starts_with("data:") {
        return SseLineAction::Ignore;
    }
    let data = line["data:".len()..].trim();
    if data == "[DONE]" {
        return SseLineAction::Done;
    }
    if data.is_empty() {
        return SseLineAction::Error(
            "provider stream mengembalikan frame SSE data kosong (keepalive tanpa payload)"
                .to_string(),
        );
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => {
            let sample: String = data.chars().take(120).collect();
            return SseLineAction::Error(format!(
                "chunk SSE non-JSON dari provider, stream digugurkan: {sample}"
            ));
        }
    };
    if let Some(err) = stream_error_shape(&v) {
        return SseLineAction::Error(err);
    }
    // usage (openrouter / beberapa provider mengirim di akhir)
    if let Some(u) = v.get("usage") {
        usage.input_tokens = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        usage.output_tokens = u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
    }
    // Paritas `_accept_chat_chunk`: hanya `choices[0]` yang dibaca.
    let choices = match v.get("choices").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return SseLineAction::Ignore,
    };
    let Some(choice) = choices.first() else {
        return SseLineAction::Ignore;
    };
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    let mut events = Vec::new();
    if let Some(t) = delta.get("content").and_then(|c| c.as_str()) {
        if !t.is_empty() {
            events.push(StreamEvent::TextDelta(t.to_string()));
        }
    }
    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
            let id = tc.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
            let func = tc.get("function").unwrap_or(&Value::Null);
            let name = func
                .get("name")
                .and_then(|n| n.as_str())
                .map(|s| s.to_string());
            let args = func
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            events.push(StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta: args,
            });
        }
    }
    if events.is_empty() {
        SseLineAction::Ignore
    } else {
        SseLineAction::Emit(events)
    }
}

/// Kirim satu [`SseLineAction`] ke penerima stream.
/// Mengembalikan `true` bila stream harus diakhiri.
async fn dispatch_sse_action(
    action: SseLineAction,
    tx: &mpsc::Sender<StreamEvent>,
    usage: &Usage,
) -> bool {
    match action {
        SseLineAction::Emit(events) => {
            for ev in events {
                if tx.send(ev).await.is_err() {
                    return true;
                }
            }
            false
        }
        SseLineAction::Done => {
            let _ = tx.send(StreamEvent::Done(usage.clone())).await;
            true
        }
        SseLineAction::Ignore => false,
        SseLineAction::Error(e) => {
            let _ = tx.send(StreamEvent::Error(e)).await;
            true
        }
    }
}
fn args_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string(v).unwrap_or_default(),
    }
}

/// Cerminan `utils.model_forces_max_completion_tokens` di original (utils.py:762):
/// `true` untuk keluarga model OpenAI yang menolak `max_tokens` (HTTP 400
/// `unsupported_parameter`) dan membutuhkan `max_completion_tokens`.
///
/// Aturan yang disalin verbatim: nama model di-trim, di-lowercase, awalan
/// provider sebelum `/` terakhir dibuang (mis. `openai/gpt-4o`), lalu dicek
/// prefix `gpt-4o` / `gpt-4.1` / `gpt-5` / `o1` / `o3` / `o4`.
pub fn model_forces_max_completion_tokens(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    let name = m.rsplit('/').next().unwrap_or(&m);
    ["gpt-4o", "gpt-4.1", "gpt-5", "o1", "o3", "o4"]
        .iter()
        .any(|p| name.starts_with(p))
}

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let url = self.chat_url();
        let body = Self::build_chat_body(&req);

        let api_key = self.pool.next_key();
        let mut rb = self.client.post(&url).json(&body);
        if let Some(k) = api_key.clone() {
            if self.azure_api_key_header {
                rb = rb.header("api-key", k);
            } else {
                rb = rb.bearer_auth(k);
            }
        }
        for (k, v) in &self.extra_headers {
            rb = rb.header(k.as_str(), v.as_str());
        }

        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                let err = HermesError::transport(e.is_timeout(), format!("request gagal: {e}"));
                if let Some(k) = &api_key {
                    self.pool.report_failure(k, &err);
                }
                return Err(err);
            }
        };
        if !resp.status().is_success() {
            let err = crate::provider_http_error(resp).await;
            if let Some(k) = &api_key {
                self.pool.report_failure(k, &err);
            }
            return Err(err);
        }
        if let Some(k) = &api_key {
            self.pool.report_success(k);
        }

        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            let mut usage = Usage::default();
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
                // Proses baris SSE yang lengkap.
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].to_string();
                    buf.drain(..=pos);
                    if dispatch_sse_action(handle_sse_line(&line, &mut usage), &tx, &usage).await {
                        return;
                    }
                }
            }
            // Paritas `_iter_sse_events` (gemini_native_adapter.py): frame
            // terakhir yang tidak diakhiri newline tetap diproses, bukan
            // dibuang diam-diam.
            if !buf.trim().is_empty()
                && dispatch_sse_action(handle_sse_line(&buf, &mut usage), &tx, &usage).await
            {
                return;
            }
            let _ = tx.send(StreamEvent::Done(usage)).await;
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::{ContentBlock, Message, Role};

    fn chat_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "halo".to_string(),
                }],
                name: None,
            }],
            tools: vec![],
            max_tokens: Some(100),
            temperature: None,
            reasoning_effort: None,
        }
    }

    #[test]
    fn forced_models_match_original_rule() {
        // Cerminan utils.py:762-765: prefix gpt-4o / gpt-4.1 / gpt-5 / o1 / o3 / o4.
        for m in [
            "o1",
            "o1-mini",
            "o1-preview",
            "o3",
            "o3-mini",
            "o4-mini",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4o-2024-08-06",
            "gpt-4.1",
            "gpt-4.1-mini",
            "gpt-4.1-nano",
            "gpt-5",
            "gpt-5-mini",
            "gpt-5-nano",
            "gpt-5.1",
            "openai/gpt-5", // awalan provider sebelum "/" dibuang seperti original
            "  GPT-4O  ",   // trim + case-insensitive seperti original
        ] {
            assert!(
                model_forces_max_completion_tokens(m),
                "harusnya forced: {m}"
            );
        }
    }

    #[test]
    fn unforced_models_keep_max_tokens() {
        for m in [
            "",
            "gpt-3.5-turbo",
            "gpt-4",
            "gpt-4-turbo",
            "gpt-4.5-preview",
            "llama3.1",
            "claude-3-5-sonnet",
            "oxygen",
        ] {
            assert!(
                !model_forces_max_completion_tokens(m),
                "harusnya tidak forced: {m}"
            );
        }
    }

    #[test]
    fn body_contains_exactly_one_token_limit_key() {
        for (model, expected_key) in [
            ("o1-mini", "max_completion_tokens"),
            ("o3", "max_completion_tokens"),
            ("gpt-4o", "max_completion_tokens"),
            ("gpt-4.1", "max_completion_tokens"),
            ("gpt-5", "max_completion_tokens"),
            ("gpt-3.5-turbo", "max_tokens"),
            ("gpt-4-turbo", "max_tokens"),
            ("llama3.1", "max_tokens"),
        ] {
            let body = OpenAiCompatProvider::build_chat_body(&chat_req(model));
            let obj = body.as_object().expect("body harus objek JSON");
            let other = if expected_key == "max_tokens" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            assert!(
                obj.contains_key(expected_key),
                "model {model}: body harus memuat {expected_key}"
            );
            assert!(
                !obj.contains_key(other),
                "model {model}: body tidak boleh memuat {other}"
            );
            assert_eq!(obj[expected_key], json!(100));
        }
    }

    #[test]
    fn body_with_no_limit_omits_token_key() {
        // Paritas `_apply_max_tokens`: `max_tokens: None` -> kunci dihilangkan,
        // bukan `null` (original tidak pernah mengirim null).
        let mut req = chat_req("o3-mini");
        req.max_tokens = None;
        let body = OpenAiCompatProvider::build_chat_body(&req);
        let obj = body.as_object().expect("body harus objek JSON");
        assert!(!obj.contains_key("max_completion_tokens"));
        assert!(!obj.contains_key("max_tokens"));
    }

    #[test]
    fn body_omits_tools_and_temperature_when_unset() {
        // Paritas `_base_kwargs`: `tools` dihilangkan bila kosong,
        // `temperature` dihilangkan bila None (bukan null).
        let req = chat_req("gpt-4o");
        let body = OpenAiCompatProvider::build_chat_body(&req);
        let obj = body.as_object().expect("body harus objek JSON");
        assert!(!obj.contains_key("tools"), "tools kosong harus dihilangkan");
        assert!(
            !obj.contains_key("temperature"),
            "temperature None harus dihilangkan, bukan null"
        );
    }

    #[test]
    fn body_includes_tools_and_temperature_when_set() {
        let mut req = chat_req("gpt-4o");
        req.temperature = Some(0.7);
        req.tools = vec![hermes_core::ToolDefinition {
            name: "exec".to_string(),
            description: "run".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let body = OpenAiCompatProvider::build_chat_body(&req);
        let obj = body.as_object().expect("body harus objek JSON");
        let temp = obj["temperature"]
            .as_f64()
            .expect("temperature harus angka");
        assert!(
            (temp - 0.7).abs() < 1e-6,
            "temperature harus 0.7, dapat {temp}"
        );
        assert_eq!(obj["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn convert_tools_dedups_duplicate_names() {
        // Paritas `_dedupe_tool_names`: duplikat dibuang, yang pertama menang.
        let mk = |name: &str| hermes_core::ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        };
        let tools = OpenAiCompatProvider::convert_tools(&[mk("a"), mk("b"), mk("a")]);
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn azure_foundry_uses_base_url_verbatim() {
        let base = "https://myres.services.ai.azure.com/openai/v1/chat/completions";
        let p = OpenAiCompatProvider::new_azure_foundry(format!("{base}/"), vec![]);
        // Isi URL dipakai apa adanya (hanya trailing slash dirapikan oleh new()).
        assert_eq!(p.base_url, base);
        assert!(p.chat_path.is_empty());
        assert!(p.azure_api_key_header);
        assert_eq!(p.chat_url(), base);
        assert_eq!(p.name(), "azure-foundry");
    }

    #[test]
    fn azure_legacy_keeps_deployment_url_building() {
        let p = OpenAiCompatProvider::new_azure("myres", "mydep", "2024-10-21", vec![]);
        assert_eq!(
            p.chat_url(),
            "https://myres.openai.azure.com/openai/deployments/mydep/chat/completions?api-version=2024-10-21"
        );
        assert!(p.azure_api_key_header);
        assert_eq!(p.name(), "azure");
    }

    #[test]
    fn regular_provider_appends_chat_path() {
        let p = OpenAiCompatProvider::new("openai", "https://api.openai.com/v1", "gpt-4o", vec![]);
        assert_eq!(p.chat_url(), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn sse_text_and_tool_call_deltas() {
        let mut usage = Usage::default();
        let action = handle_sse_line(
            r#"data: {"choices":[{"delta":{"content":"halo","tool_calls":[{"index":0,"id":"c1","function":{"name":"exec","arguments":"{}"}}]}}]}"#,
            &mut usage,
        );
        match action {
            SseLineAction::Emit(events) => {
                assert_eq!(events.len(), 2);
                assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "halo"));
                assert!(
                    matches!(&events[1], StreamEvent::ToolCallDelta { id: Some(id), .. } if id == "c1")
                );
            }
            other => panic!("harusnya Emit, dapat {other:?}"),
        }
    }

    #[test]
    fn sse_done_sentinel() {
        let mut usage = Usage::default();
        assert!(matches!(
            handle_sse_line("data: [DONE]", &mut usage),
            SseLineAction::Done
        ));
    }

    #[test]
    fn sse_reads_only_first_choice() {
        // Paritas `_accept_chat_chunk`: hanya `choices[0]` dibaca.
        let mut usage = Usage::default();
        let action = handle_sse_line(
            r#"data: {"choices":[{"delta":{"content":"satu"}},{"delta":{"content":"dua"}}]}"#,
            &mut usage,
        );
        match action {
            SseLineAction::Emit(events) => {
                assert_eq!(events.len(), 1);
                assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "satu"));
            }
            other => panic!("harusnya Emit, dapat {other:?}"),
        }
    }

    #[test]
    fn sse_error_shape_aborts_stream() {
        // Paritas `_payload_has_error_shape`: error in-stream tidak hilang diam-diam.
        let mut usage = Usage::default();
        for line in [
            r#"data: {"error":{"message":"boom","code":"bad"}}"#,
            r#"data: {"error":"plain boom"}"#,
            r#"data: {"choices":null,"error_type":"bad_request","error_message":"nope"}"#,
            r#"data: {"message":"kaput","code":"x"}"#,
        ] {
            assert!(
                matches!(handle_sse_line(line, &mut usage), SseLineAction::Error(_)),
                "baris error harus menggugurkan stream: {line}"
            );
        }
    }

    #[test]
    fn sse_malformed_and_empty_frames_abort_stream() {
        // Paritas `_provider_stream_error_from_json_decode_error`: chunk
        // non-JSON dan frame data kosong menggugurkan stream sebagai error.
        let mut usage = Usage::default();
        assert!(matches!(
            handle_sse_line("data: ini bukan json", &mut usage),
            SseLineAction::Error(_)
        ));
        assert!(matches!(
            handle_sse_line("data:", &mut usage),
            SseLineAction::Error(_)
        ));
    }

    #[test]
    fn sse_usage_chunk_and_ignores() {
        let mut usage = Usage::default();
        // Chunk usage tanpa choices: usage terbaca, tidak ada event.
        let action = handle_sse_line(
            r#"data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":7}}"#,
            &mut usage,
        );
        assert!(matches!(action, SseLineAction::Ignore));
        assert_eq!(usage.input_tokens, 3);
        assert_eq!(usage.output_tokens, 7);
        // Baris non-data (komentar SSE) diabaikan.
        assert!(matches!(
            handle_sse_line(": ping", &mut usage),
            SseLineAction::Ignore
        ));
        assert!(matches!(
            handle_sse_line("event: message", &mut usage),
            SseLineAction::Ignore
        ));
    }
}
