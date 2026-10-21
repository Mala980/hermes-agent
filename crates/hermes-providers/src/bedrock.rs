//! Provider AWS Bedrock via Converse API + SigV4.
//!
//! Region: `AWS_REGION` → `AWS_DEFAULT_REGION` → `us-east-1`.
//!
//! Chain kredensial IAM (paritas `resolve_aws_auth_env_var` di
//! `agent/bedrock_adapter.py`, minus boto3):
//! 1. env eksplisit: `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`
//!    (opsional `AWS_SESSION_TOKEN`);
//! 2. file `~/.aws/credentials` profil `[default]` (parse INI ringan, tanpa
//!    dependensi baru);
//! 3. kredensial instance-metadata IMDSv2 (`http://169.254.169.254`): handshake
//!    token via PUT `X-aws-ec2-metadata-token-ttl-seconds`, lalu baca nama role
//!    dan dokumen kredensial JSON. Timeout koneksi 1 dtk + timeout I/O 1 dtk
//!    per permintaan, fail-fast: pada mesin non-EC2 mengembalikan "tidak ada
//!    kredensial" tanpa menggantung.
//!
//! Di luar cakupan (didukung original via boto3, sengaja tidak di-port):
//! profil bernama/SSO (`AWS_PROFILE`), assume-role, `AWS_BEARER_TOKEN_BEDROCK`,
//! kredensial container ECS (`AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`), dan
//! web identity / EKS IRSA (`AWS_WEB_IDENTITY_TOKEN_FILE`).
//!
//! Streaming memakai endpoint `converse-stream` (framing event-stream biner AWS).
//! Bila endpoint stream gagal saat connect (4xx/5xx — mis. model/region tak
//! mendukung streaming), otomatis fallback ke `converse` non-stream yang
//! di-replay sebagai event (perilaku lama).
use async_trait::async_trait;
use hermes_core::{
    ChatRequest, ContentBlock, HermesError, LlmProvider, Message, Result, Role, StreamEvent,
    ToolDefinition, Usage,
};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;

type HmacSha256 = Hmac<Sha256>;

pub struct BedrockProvider {
    region: String,
    default_model: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    client: Client,
}

impl BedrockProvider {
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> {
        let env: HashMap<String, String> = std::env::vars().collect();
        let region = resolve_bedrock_region(&env);
        let creds = credentials_from_env_vars(&env)
            .or_else(|| credentials_from_aws_credentials_file(home_dir().as_deref()))
            .or_else(|| credentials_from_imds(IMDS_BASE))
            .ok_or_else(|| {
                HermesError::Config(
                    "tidak ada kredensial AWS: set AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY, \
                     isi profil [default] di ~/.aws/credentials, atau jalankan di EC2 (IMDS)"
                        .into(),
                )
            })?;
        Ok(Self {
            region,
            default_model: default_model.into(),
            access_key: creds.access_key,
            secret_key: creds.secret_key,
            session_token: creds.session_token,
            client: Client::new(),
        })
    }

    /// Placeholder Converse untuk teks kosong — paritas original
    /// (`_EMPTY_TEXT_PLACEHOLDER = "(empty)"` di `agent/bedrock_adapter.py`;
    /// Converse menolak blok teks kosong ATAU whitespace-only).
    const EMPTY_TEXT_PLACEHOLDER: &'static str = "(empty)";

    /// `text` bila punya konten non-whitespace, else placeholder.
    fn safe_text(text: &str) -> &str {
        if text.trim().is_empty() {
            Self::EMPTY_TEXT_PLACEHOLDER
        } else {
            text
        }
    }

    /// Paritas `convert_messages_to_converse` (agent/bedrock_adapter.py):
    /// Converse butuh alternasi user/assistant yang ketat — tetangga se-role
    /// digabung, dan ujung-ujung di-pad dengan user turn placeholder bila
    /// diawali/diakhiri assistant. toolResult: `{"toolUseId", "content":
    /// [{"text"}]}` (tanpa `status`, content berupa list blok).
    fn convert(messages: &[Message]) -> (Vec<Value>, Vec<Value>) {
        let mut system = Vec::new();
        let mut out: Vec<Value> = Vec::new();
        // `append_turn`: gabung ke turn terakhir bila se-role.
        let mut append_turn = |role: &str, blocks: Vec<Value>| {
            if blocks.is_empty() {
                return;
            }
            let same_role = out
                .last()
                .and_then(|t| t.get("role"))
                .and_then(|r| r.as_str())
                == Some(role);
            if same_role {
                if let Some(content) = out
                    .last_mut()
                    .and_then(|t| t.get_mut("content"))
                    .and_then(|c| c.as_array_mut())
                {
                    content.extend(blocks);
                }
                return;
            }
            out.push(json!({"role": role, "content": blocks}));
        };
        for m in messages {
            match m.role {
                Role::System => {
                    // Paritas original (`_system_blocks`): bagian system yang
                    // kosong/whitespace-only dibuang, bukan diisi placeholder.
                    for b in &m.content {
                        if let ContentBlock::Text { text } = b {
                            if !text.trim().is_empty() {
                                system.push(json!({"text": text}));
                            }
                        }
                    }
                }
                Role::User => {
                    append_turn("user", Self::content_blocks(&m.content, false));
                }
                Role::Assistant => {
                    append_turn("assistant", Self::content_blocks(&m.content, true));
                }
                Role::Tool => {
                    let mut blocks = Vec::new();
                    for b in &m.content {
                        if let ContentBlock::ToolResult { result } = b {
                            blocks.push(json!({
                                "toolResult": {
                                    "toolUseId": result.tool_call_id,
                                    "content": [{"text": Self::safe_text(&result.content)}],
                                }
                            }));
                        }
                    }
                    append_turn("user", blocks);
                }
            }
        }
        // Pad ujung dengan user turn placeholder agar user-first/last.
        if !out.is_empty() {
            let first_is_user = out
                .first()
                .and_then(|t| t.get("role"))
                .and_then(|r| r.as_str())
                == Some("user");
            if !first_is_user {
                out.insert(
                    0,
                    json!({"role": "user", "content": [{"text": Self::EMPTY_TEXT_PLACEHOLDER}]}),
                );
            }
            let last_is_user = out
                .last()
                .and_then(|t| t.get("role"))
                .and_then(|r| r.as_str())
                == Some("user");
            if !last_is_user {
                out.push(
                    json!({"role": "user", "content": [{"text": Self::EMPTY_TEXT_PLACEHOLDER}]}),
                );
            }
        }
        (system, out)
    }

    fn content_blocks(content: &[ContentBlock], from_model: bool) -> Vec<Value> {
        let mut blocks = Vec::new();
        for b in content {
            match b {
                ContentBlock::Text { text } => {
                    // Paritas original (`_safe_text` di agent/bedrock_adapter.py):
                    // Converse API menolak blok teks yang kosong ATAU hanya
                    // whitespace (Ref: issue #9486) — ganti dengan placeholder.
                    blocks.push(json!({"text": Self::safe_text(text)}));
                }
                ContentBlock::Image { data_url } => {
                    if let Some(block) = image_block(data_url) {
                        blocks.push(block);
                    }
                }
                ContentBlock::ToolCall { call } => {
                    if from_model {
                        blocks.push(json!({
                            "toolUse": {
                                "toolUseId": call.id,
                                "name": call.name,
                                "input": call.arguments,
                            }
                        }));
                    }
                }
                ContentBlock::ToolResult { .. } => {}
            }
        }
        if blocks.is_empty() {
            blocks.push(json!({"text": Self::EMPTY_TEXT_PLACEHOLDER}));
        }
        blocks
    }

    fn convert_tools(tools: &[ToolDefinition]) -> Option<Value> {
        if tools.is_empty() {
            return None;
        }
        // Nama duplikat dibuang (yang pertama menang), paritas
        // `_dedupe_tool_names` — Converse 400 pada nama tool duplikat.
        let mut seen = std::collections::HashSet::new();
        let specs: Vec<Value> = tools
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
                    "toolSpec": {
                        "name": t.name,
                        "description": t.description,
                        "inputSchema": {"json": t.parameters},
                    }
                })
            })
            .collect();
        if specs.is_empty() {
            return None;
        }
        Some(json!({"tools": specs}))
    }

    /// Bangun body Converse API. Paritas `build_converse_kwargs`
    /// (agent/bedrock_adapter.py): `inferenceConfig` hanya memuat kunci yang
    /// terisi (tidak pernah null) dan dihilangkan seluruhnya bila kosong;
    /// `system`/`toolConfig` hanya bila terisi.
    fn build_body(req: &ChatRequest) -> Value {
        let (system, messages) = Self::convert(&req.messages);
        let mut inference_config = serde_json::Map::new();
        if let Some(mt) = req.max_tokens {
            inference_config.insert("maxTokens".to_string(), json!(mt));
        }
        if let Some(t) = req.temperature {
            inference_config.insert("temperature".to_string(), json!(t));
        }
        let mut body = json!({ "messages": messages });
        if !inference_config.is_empty() {
            body["inferenceConfig"] = Value::Object(inference_config);
        }
        if !system.is_empty() {
            body["system"] = Value::Array(system);
        }
        if let Some(tc) = Self::convert_tools(&req.tools) {
            body["toolConfig"] = tc;
        }
        body
    }

    /// Menandatangani request POST ke bedrock-runtime.
    ///
    /// Mengembalikan `(url, x-amz-date, authorization)`. Dipakai bersama oleh
    /// `converse` dan `converse-stream`. Fungsi-fungsi signer SigV4 di bawah
    /// (`sig_key`/`hex_hmac`/`hex_sha256`) TIDAK diubah.
    fn sign_post(&self, path: &str, body: &str) -> (String, String, String) {
        let host = format!("bedrock-runtime.{}.amazonaws.com", self.region);
        let url = format!("https://{host}{path}");
        let datetime = utc_now();
        let date = datetime[..8].to_string();

        let payload_hash = hex_sha256(body.as_bytes());
        let mut canonical_headers =
            format!("content-type:application/json\nhost:{host}\nx-amz-date:{datetime}\n");
        let mut signed_headers = "content-type;host;x-amz-date".to_string();
        if let Some(tok) = &self.session_token {
            canonical_headers.push_str(&format!("x-amz-security-token:{tok}\n"));
            signed_headers.push_str(";x-amz-security-token");
        }
        let canonical_request =
            format!("POST\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
        let scope = format!("{date}/{}/bedrock/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        );
        let signing_key = sig_key(&self.secret_key, &date, &self.region);
        let signature = hex_hmac(&signing_key, string_to_sign.as_bytes());
        let auth = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );
        (url, datetime, auth)
    }

    /// Panggilan Converse non-stream; mengembalikan (teks, tool_uses, usage).
    ///
    /// Juga dipakai sebagai fallback bila endpoint `converse-stream` gagal
    /// saat connect (4xx/5xx).
    async fn converse(
        &self,
        model: &str,
        body: &Value,
    ) -> Result<(String, Vec<(String, String, Value)>, Usage)> {
        let body_str = body.to_string();
        let path = format!("/model/{model}/converse");
        let (url, datetime, auth) = self.sign_post(&path, &body_str);

        let mut req = self
            .client
            .post(&url)
            .header("x-amz-date", &datetime)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .body(body_str);
        if let Some(tok) = &self.session_token {
            req = req.header("x-amz-security-token", tok);
        }
        let resp = req.send().await.map_err(|e| {
            HermesError::transport(e.is_timeout(), format!("bedrock request gagal: {e}"))
        })?;
        if !resp.status().is_success() {
            return Err(crate::provider_http_error(resp).await);
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|e| HermesError::Provider(format!("bedrock parse gagal: {e}")))?;
        let mut text = String::new();
        let mut tool_uses = Vec::new();
        for block in v
            .pointer("/output/message/content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default()
        {
            if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
            if let Some(tu) = block.get("toolUse") {
                tool_uses.push((
                    tu.get("toolUseId")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tu.get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                    tu.get("input").cloned().unwrap_or(Value::Null),
                ));
            }
        }
        let usage = Usage {
            input_tokens: v
                .pointer("/usage/inputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            output_tokens: v
                .pointer("/usage/outputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            ..Default::default()
        };
        Ok((text, tool_uses, usage))
    }

    /// Bangun request ke endpoint `converse-stream`.
    ///
    /// Respons (termasuk status non-sukses) dikembalikan ke pemanggil: status
    /// 4xx/5xx saat connect berarti endpoint tak didukung -> fallback `converse`.
    async fn stream_request(&self, model: &str, body: &Value) -> Result<reqwest::Response> {
        let body_str = body.to_string();
        let path = format!("/model/{model}/converse-stream");
        let (url, datetime, auth) = self.sign_post(&path, &body_str);
        let mut req = self
            .client
            .post(&url)
            .header("x-amz-date", &datetime)
            .header("authorization", auth)
            .header("content-type", "application/json")
            .header("accept", "application/vnd.amazon.eventstream")
            .body(body_str);
        if let Some(tok) = &self.session_token {
            req = req.header("x-amz-security-token", tok);
        }
        req.send().await.map_err(|e| {
            HermesError::transport(e.is_timeout(), format!("bedrock stream gagal: {e}"))
        })
    }
}

// ---------------------------------------------------------------------------
// Streaming via converse-stream (event-stream biner AWS).
// ---------------------------------------------------------------------------

/// Satu pesan event-stream: nilai header `:message-type`/`:event-type` + payload mentah.
struct EventFrame {
    message_type: String,
    event_type: String,
    payload: Vec<u8>,
}

fn frame_error() -> HermesError {
    HermesError::Provider("bedrock event-stream: framing rusak".into())
}

fn read_u16_at(buf: &[u8], pos: usize, limit: usize) -> Result<u16> {
    if pos + 2 > limit || pos + 2 > buf.len() {
        return Err(frame_error());
    }
    Ok(u16::from_be_bytes([buf[pos], buf[pos + 1]]))
}

/// Parse satu frame dari awal `buf`.
///
/// Framing AWS: u32 panjang total + u32 panjang header + CRC prelude (4B) +
/// header + payload + CRC pesan (4B). Kedua CRC dilewati tanpa verifikasi
/// (transport sudah TLS; fokus parser adalah framing).
///
/// `Ok(None)` = data belum lengkap, tunggu chunk berikutnya.
/// `Err` = framing rusak (panjang tidak konsisten / header terpotong).
fn parse_event_frame(buf: &[u8]) -> Result<Option<(EventFrame, usize)>> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let total = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let headers_len = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    if total < 16 || headers_len > total - 16 {
        return Err(frame_error());
    }
    if buf.len() < total {
        return Ok(None);
    }
    let headers_end = 12 + headers_len;
    let mut message_type = String::new();
    let mut event_type = String::new();
    let mut pos = 12usize;
    while pos < headers_end {
        let name_len = *buf.get(pos).ok_or_else(frame_error)? as usize;
        pos += 1;
        let name_end = pos.checked_add(name_len).ok_or_else(frame_error)?;
        if name_end > headers_end {
            return Err(frame_error());
        }
        let name = &buf[pos..name_end];
        pos = name_end;
        let value_type = *buf.get(pos).ok_or_else(frame_error)?;
        pos += 1;
        match value_type {
            0 | 1 => {}        // boolean true/false: tanpa muatan
            2 => pos += 1,     // byte
            3 => pos += 2,     // short
            4 => pos += 4,     // integer
            5 | 8 => pos += 8, // long / timestamp
            9 => pos += 16,    // uuid
            6 | 7 => {
                // bytes / string: u16 panjang + data
                let len = read_u16_at(buf, pos, headers_end)? as usize;
                pos += 2;
                if pos.checked_add(len).ok_or_else(frame_error)? > headers_end {
                    return Err(frame_error());
                }
                if value_type == 7 {
                    let s = String::from_utf8_lossy(&buf[pos..pos + len]).into_owned();
                    if name == b":message-type" {
                        message_type = s;
                    } else if name == b":event-type" {
                        event_type = s;
                    }
                }
                pos += len;
            }
            _ => return Err(frame_error()),
        }
        if pos > headers_end {
            return Err(frame_error());
        }
    }
    let payload_end = total - 4;
    let payload = buf[12 + headers_len..payload_end].to_vec();
    Ok(Some((
        EventFrame {
            message_type,
            event_type,
            payload,
        },
        total,
    )))
}

/// Aksi hasil pemetaan satu frame event-stream ke event publik.
enum FrameAction {
    /// Potongan teks asisten (`contentBlockDelta` / `delta.text`).
    Text(String),
    /// Awal blok toolUse: id + nama terisi, argumen kosong.
    ToolStart {
        index: usize,
        id: String,
        name: String,
    },
    /// Potongan JSON argumen toolUse (string parsial).
    ToolInput { index: usize, delta: String },
    /// `messageStop` diterima. `stopReason` dibaca dari payload saat mapping
    /// (lihat `map_frame`), tetapi tidak dibawa lebih jauh: `StreamEvent`
    /// publik tidak punya slot untuknya, jadi varian ini unit.
    StopReason,
    /// Pemakaian token dari event `metadata` (inputTokens/outputTokens).
    Usage(Usage),
    /// Frame exception (`:message-type` == "exception"), mis. `modelStreamErrorException`.
    StreamError(String),
    /// Frame yang tidak dipetakan (contentBlockStart teks, contentBlockStop, dsb).
    Ignore,
}

/// Petakan satu frame ke aksi. Payload event converse-stream berupa JSON.
fn map_frame(frame: &EventFrame) -> FrameAction {
    if frame.message_type == "exception" {
        let msg = serde_json::from_slice::<Value>(&frame.payload)
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| String::from_utf8_lossy(&frame.payload).into_owned());
        return FrameAction::StreamError(format!("{}: {msg}", frame.event_type));
    }
    let v: Value = match serde_json::from_slice(&frame.payload) {
        Ok(v) => v,
        Err(_) => return FrameAction::Ignore,
    };
    let index = v
        .get("contentBlockIndex")
        .and_then(|i| i.as_u64())
        .unwrap_or(0) as usize;
    match frame.event_type.as_str() {
        "contentBlockStart" => {
            if let Some(tu) = v.pointer("/start/toolUse") {
                FrameAction::ToolStart {
                    index,
                    id: tu
                        .get("toolUseId")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string(),
                    name: tu
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string(),
                }
            } else {
                FrameAction::Ignore
            }
        }
        "contentBlockDelta" => {
            if let Some(t) = v.pointer("/delta/text").and_then(|t| t.as_str()) {
                FrameAction::Text(t.to_string())
            } else if let Some(inp) = v.pointer("/delta/toolUse/input").and_then(|i| i.as_str()) {
                FrameAction::ToolInput {
                    index,
                    delta: inp.to_string(),
                }
            } else {
                FrameAction::Ignore
            }
        }
        "messageStop" => {
            // stopReason diekstrak di sini (validasi bentuk payload); nilainya
            // tidak dibawa ke StreamEvent karena API publik tak punya slotnya.
            let _ = v.get("stopReason").and_then(|s| s.as_str()).unwrap_or("");
            FrameAction::StopReason
        }
        "metadata" => FrameAction::Usage(Usage {
            input_tokens: v
                .pointer("/usage/inputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            output_tokens: v
                .pointer("/usage/outputTokens")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            ..Default::default()
        }),
        _ => FrameAction::Ignore,
    }
}

/// Status akumulasi saat menguras event-stream.
#[derive(Default)]
struct StreamState {
    /// Sisa byte yang belum membentuk satu frame penuh.
    buf: Vec<u8>,
    /// Pemakaian token dari event metadata terakhir.
    usage: Usage,
    /// Blok toolUse yang sedang berjalan: (contentBlockIndex, id, nama).
    pending_tool: Option<(usize, String, String)>,
}

/// Umpankan satu chunk byte mentah: parse frame yang lengkap, kirim event,
/// akumulasi state. Dipisah dari loop I/O agar bisa diuji unit dengan byte kalengan.
async fn pump_stream_chunk(
    state: &mut StreamState,
    chunk: &[u8],
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<()> {
    state.buf.extend_from_slice(chunk);
    loop {
        let (frame, used) = match parse_event_frame(&state.buf)? {
            None => break,
            Some(f) => f,
        };
        state.buf.drain(..used);
        match map_frame(&frame) {
            FrameAction::Text(t) => {
                let _ = tx.send(StreamEvent::TextDelta(t)).await;
            }
            FrameAction::ToolStart { index, id, name } => {
                state.pending_tool = Some((index, id.clone(), name.clone()));
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index,
                        id: Some(id),
                        name: Some(name),
                        arguments_delta: String::new(),
                    })
                    .await;
            }
            FrameAction::ToolInput { index, delta } => {
                let (idx, id, name) = match &state.pending_tool {
                    Some((i, id, name)) if *i == index => {
                        (index, Some(id.clone()), Some(name.clone()))
                    }
                    _ => (index, None, None),
                };
                let _ = tx
                    .send(StreamEvent::ToolCallDelta {
                        index: idx,
                        id,
                        name,
                        arguments_delta: delta,
                    })
                    .await;
            }
            FrameAction::StopReason => {
                // stopReason sudah diekstrak di map_frame; StreamEvent tak punya slotnya.
            }
            FrameAction::Usage(u) => {
                state.usage = u;
            }
            FrameAction::StreamError(e) => {
                return Err(HermesError::Provider(format!("bedrock stream error: {e}")));
            }
            FrameAction::Ignore => {}
        }
        if frame.event_type == "contentBlockStop" {
            state.pending_tool = None;
        }
    }
    Ok(())
}

/// Kuras body event-stream dari respons yang sudah sukses; kembalikan usage akhir.
async fn drain_event_stream(
    resp: reqwest::Response,
    tx: &mpsc::Sender<StreamEvent>,
) -> Result<Usage> {
    use futures::StreamExt;
    let mut stream = resp.bytes_stream();
    let mut state = StreamState::default();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            HermesError::transport(e.is_timeout(), format!("bedrock stream putus: {e}"))
        })?;
        pump_stream_chunk(&mut state, &chunk, tx).await?;
    }
    Ok(state.usage)
}

fn image_block(data_url: &str) -> Option<Value> {
    // Bedrock Converse image: format + bytes base64.
    let rest = data_url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(";base64,")?;
    let mime = meta.split(';').next().unwrap_or("image/png");
    let format = match mime {
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "jpeg",
    };
    Some(json!({"image": {"format": format, "source": {"bytes": data}}}))
}

fn utc_now() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn hex_sha256(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex_encode(&h.finalize())
}

fn hex_hmac(key: &[u8], data: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(data);
    hex_encode(&mac.finalize().into_bytes())
}

fn sig_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(format!("AWS4{secret}").as_bytes()).expect("hmac");
    mac.update(date.as_bytes());
    let k_date = mac.finalize().into_bytes().to_vec();
    let mut mac = HmacSha256::new_from_slice(&k_date).expect("hmac");
    mac.update(region.as_bytes());
    let k_region = mac.finalize().into_bytes().to_vec();
    let mut mac = HmacSha256::new_from_slice(&k_region).expect("hmac");
    mac.update(b"bedrock");
    let k_service = mac.finalize().into_bytes().to_vec();
    let mut mac = HmacSha256::new_from_slice(&k_service).expect("hmac");
    mac.update(b"aws4_request");
    mac.finalize().into_bytes().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// Resolusi region & kredensial AWS (audit W3-P4).
// ---------------------------------------------------------------------------

/// Kredensial IAM hasil resolusi chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BedrockCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

/// Endpoint IMDS default (link-local EC2).
pub(crate) const IMDS_BASE: &str = "http://169.254.169.254";

/// Timeout koneksi TCP ke IMDS. Pada mesin non-EC2 koneksi ke 169.254.169.254
/// bisa menggantung sampai TCP SYN timeout (~2 menit) bila rutenya ada tapi
/// blackhole — batas ini membuatnya fail-fast.
const IMDS_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Timeout baca/tulis per permintaan IMDS.
const IMDS_IO_TIMEOUT: Duration = Duration::from_secs(1);

/// Chain region: `AWS_REGION` → `AWS_DEFAULT_REGION` → `us-east-1`.
/// Nilai kosong/whitespace dianggap tidak di-set (paritas `.strip()` original).
pub(crate) fn resolve_bedrock_region(env: &HashMap<String, String>) -> String {
    for key in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
        if let Some(v) = env.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            return v.to_string();
        }
    }
    "us-east-1".to_string()
}

/// Level 1 chain: pasangan env eksplisit (`AWS_ACCESS_KEY_ID` +
/// `AWS_SECRET_ACCESS_KEY`, opsional `AWS_SESSION_TOKEN`).
pub(crate) fn credentials_from_env_vars(
    env: &HashMap<String, String>,
) -> Option<BedrockCredentials> {
    let access_key = nonempty_var(env, "AWS_ACCESS_KEY_ID")?;
    let secret_key = nonempty_var(env, "AWS_SECRET_ACCESS_KEY")?;
    let session_token = nonempty_var(env, "AWS_SESSION_TOKEN").map(str::to_string);
    Some(BedrockCredentials {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        session_token,
    })
}

fn nonempty_var<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Level 2 chain: profil `[default]` dari `~/.aws/credentials`.
pub(crate) fn credentials_from_aws_credentials_file(
    home: Option<&Path>,
) -> Option<BedrockCredentials> {
    let path = home?.join(".aws").join("credentials");
    let text = std::fs::read_to_string(&path).ok()?;
    parse_aws_credentials_ini(&text)
}

/// Parse INI ringan untuk `~/.aws/credentials`: header `[seksi]`, pasangan
/// `kunci = nilai`, komentar `#`/`;` dan baris kosong dilewati, kunci
/// case-insensitive. Hanya seksi `[default]` yang dibaca; profil bernama/SSO
/// di luar cakupan.
pub(crate) fn parse_aws_credentials_ini(text: &str) -> Option<BedrockCredentials> {
    let mut in_default = false;
    let mut access_key: Option<String> = None;
    let mut secret_key: Option<String> = None;
    let mut session_token: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_default = section.trim().eq_ignore_ascii_case("default");
            continue;
        }
        if !in_default {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "aws_access_key_id" => access_key = Some(value.to_string()),
            "aws_secret_access_key" => secret_key = Some(value.to_string()),
            "aws_session_token" => session_token = Some(value.to_string()),
            _ => {}
        }
    }
    Some(BedrockCredentials {
        access_key: access_key?,
        secret_key: secret_key?,
        session_token,
    })
}

/// Direktori home untuk `~/.aws/credentials` (`HOME`, fallback `USERPROFILE`).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Level 3 chain: kredensial IMDSv2 instance metadata.
///
/// Handshake wajib: `PUT /latest/api/token` (header
/// `X-aws-ec2-metadata-token-ttl-seconds`), lalu
/// `GET /latest/meta-data/iam/security-credentials/` untuk nama role, lalu
/// `GET .../security-credentials/{role}` untuk dokumen JSON
/// (`AccessKeyId`, `SecretAccessKey`, `Token`).
///
/// Sengaja sinkron di atas `TcpStream` dengan timeout koneksi 1 dtk dan
/// timeout I/O 1 dtk per permintaan (bukan reqwest async): `from_env`
/// dipanggil dari konteks sinkron (`factory::build_provider`) dan langkah ini
/// harus fail-fast pada mesin non-EC2 tanpa bergantung pada runtime async.
pub(crate) fn credentials_from_imds(imds_base: &str) -> Option<BedrockCredentials> {
    let token = imds_request(imds_base, "PUT", "/latest/api/token", None)?;
    if token.trim().is_empty() {
        return None;
    }
    let role = imds_request(
        imds_base,
        "GET",
        "/latest/meta-data/iam/security-credentials/",
        Some(&token),
    )?;
    let role = role.trim();
    if role.is_empty() {
        return None;
    }
    let doc = imds_request(
        imds_base,
        "GET",
        &format!("/latest/meta-data/iam/security-credentials/{role}"),
        Some(&token),
    )?;
    let doc: Value = serde_json::from_str(&doc).ok()?;
    let access_key = doc
        .get("AccessKeyId")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let secret_key = doc
        .get("SecretAccessKey")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let session_token = doc
        .get("Token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(BedrockCredentials {
        access_key: access_key.to_string(),
        secret_key: secret_key.to_string(),
        session_token,
    })
}

/// Satu permintaan HTTP/1.0 minimal ke IMDS; mengembalikan body bila status 2xx.
fn imds_request(imds_base: &str, method: &str, path: &str, token: Option<&str>) -> Option<String> {
    let (host, port) = imds_host_port(imds_base)?;
    // `to_socket_addrs` pada literal IP tidak menyentuh DNS — cepat.
    let addr = (host.as_str(), port).to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, IMDS_CONNECT_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(IMDS_IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IMDS_IO_TIMEOUT)).ok()?;
    let mut request = format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n");
    if method == "PUT" {
        request.push_str("X-aws-ec2-metadata-token-ttl-seconds: 21600\r\nContent-Length: 0\r\n");
    }
    if let Some(token) = token {
        request.push_str(&format!("X-aws-ec2-metadata-token: {token}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let (head, body) = response.split_once("\r\n\r\n")?;
    let status: u16 = head
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    Some(body.to_string())
}

/// Parse `http://host[:port]` → (host, port). Hanya skema `http` (IMDS).
fn imds_host_port(imds_base: &str) -> Option<(String, u16)> {
    let hostport = imds_base.strip_prefix("http://")?.split('/').next()?;
    if hostport.is_empty() {
        return None;
    }
    if let Some((host, port)) = hostport.rsplit_once(':') {
        Some((host.to_string(), port.parse().ok()?))
    } else {
        Some((hostport.to_string(), 80))
    }
}

#[async_trait]
impl LlmProvider for BedrockProvider {
    fn name(&self) -> &str {
        "bedrock"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let body = Self::build_body(&req);
        let model = req.model.clone();
        let (tx, rx) = mpsc::channel(256);
        // Pindahkan data milik provider ke task.
        let owned = Owned {
            region: self.region.clone(),
            access_key: self.access_key.clone(),
            secret_key: self.secret_key.clone(),
            session_token: self.session_token.clone(),
            default_model: self.default_model.clone(),
        };
        tokio::spawn(async move {
            let prov = BedrockProvider {
                region: owned.region,
                default_model: owned.default_model,
                access_key: owned.access_key,
                secret_key: owned.secret_key,
                session_token: owned.session_token,
                client: Client::new(),
            };
            // 1. Coba streaming nyata via converse-stream.
            let streamed = match prov.stream_request(&model, &body).await {
                Ok(resp) if resp.status().is_success() => Some(resp),
                // Status non-sukses (4xx/5xx) atau transport gagal -> fallback di bawah.
                _ => None,
            };
            if let Some(resp) = streamed {
                match drain_event_stream(resp, &tx).await {
                    Ok(usage) => {
                        let _ = tx.send(StreamEvent::Done(usage)).await;
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    }
                }
                return;
            }
            // 2. Fallback: endpoint converse-stream gagal saat connect (mis. model
            //    atau region tak mendukung streaming). Pakai converse non-stream
            //    lalu replay hasilnya sebagai event, seperti perilaku lama.
            match prov.converse(&model, &body).await {
                Ok((text, tool_uses, usage)) => {
                    if !text.is_empty() {
                        let _ = tx.send(StreamEvent::TextDelta(text)).await;
                    }
                    for (i, (id, name, input)) in tool_uses.into_iter().enumerate() {
                        let _ = tx
                            .send(StreamEvent::ToolCallDelta {
                                index: i,
                                id: Some(id),
                                name: Some(name),
                                arguments_delta: input.to_string(),
                            })
                            .await;
                    }
                    let _ = tx.send(StreamEvent::Done(usage)).await;
                }
                Err(e) => {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                }
            }
        });
        Ok(rx)
    }
}

struct Owned {
    region: String,
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    default_model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hmac_sha256_rfc4231() {
        // RFC 4231 Test Case 1.
        let key = vec![0x0bu8; 20];
        assert_eq!(
            hex_hmac(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn sigv4_key_derivation_shape() {
        // Kunci turunan harus 32 byte dan deterministik.
        let k1 = sig_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
        );
        let k2 = sig_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
        );
        assert_eq!(k1.len(), 32);
        assert_eq!(k1, k2);
    }

    /// Bangun satu frame event-stream kalengan. CRC diisi nol karena parser
    /// melewati verifikasi CRC (fokus pada framing).
    fn canned_frame(message_type: &str, event_type: &str, payload: &[u8]) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [
            (":message-type", message_type),
            (":event-type", event_type),
            (":content-type", "application/json"),
        ] {
            headers.push(name.len() as u8);
            headers.extend_from_slice(name.as_bytes());
            headers.push(7u8); // tipe nilai: string
            headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let total = (12 + headers.len() + payload.len() + 4) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        out.extend_from_slice(&[0u8; 4]); // prelude CRC (diabaikan)
        out.extend_from_slice(&headers);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0u8; 4]); // message CRC (diabaikan)
        out
    }

    #[test]
    fn eventstream_frame_parse_partial_waits() {
        let full = canned_frame("event", "messageStop", b"{\"stopReason\":\"end_turn\"}");
        // Belum satu frame penuh -> None (tunggu chunk berikutnya).
        assert!(parse_event_frame(&full[..7]).unwrap().is_none());
        assert!(parse_event_frame(&full[..full.len() - 1])
            .unwrap()
            .is_none());
        let (frame, used) = parse_event_frame(&full).unwrap().unwrap();
        assert_eq!(used, full.len());
        assert_eq!(frame.message_type, "event");
        assert_eq!(frame.event_type, "messageStop");
        assert_eq!(frame.payload, b"{\"stopReason\":\"end_turn\"}".to_vec());
    }

    #[test]
    fn eventstream_frame_bad_length_errors() {
        let mut bad = canned_frame("event", "messageStop", b"{}");
        bad[0] = 0; // total_len tak masuk akal (< 16)
        bad[3] = 5;
        assert!(parse_event_frame(&bad).is_err());
    }

    #[tokio::test]
    async fn eventstream_canned_sequence_maps_to_events() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StreamState::default();
        let seq = [
            canned_frame(
                "event",
                "contentBlockDelta",
                b"{\"contentBlockIndex\":0,\"delta\":{\"text\":\"Halo\"}}",
            ),
            canned_frame(
                "event",
                "contentBlockDelta",
                b"{\"contentBlockIndex\":0,\"delta\":{\"text\":\" dunia\"}}",
            ),
            canned_frame("event", "messageStop", b"{\"stopReason\":\"end_turn\"}"),
            canned_frame(
                "event",
                "metadata",
                b"{\"usage\":{\"inputTokens\":10,\"outputTokens\":5}}",
            ),
        ]
        .concat();
        // Umpan dalam potongan ganjil untuk menguji buffering antar-chunk.
        for chunk in seq.chunks(7) {
            pump_stream_chunk(&mut state, chunk, &tx).await.unwrap();
        }
        drop(tx);
        let mut texts = Vec::new();
        let mut others = 0u32;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(t) => texts.push(t),
                _ => others += 1,
            }
        }
        assert_eq!(texts.concat(), "Halo dunia");
        assert_eq!(others, 0);
        assert_eq!(state.usage.input_tokens, 10);
        assert_eq!(state.usage.output_tokens, 5);
    }

    #[tokio::test]
    async fn eventstream_tool_use_deltas() {
        let (tx, mut rx) = mpsc::channel(32);
        let mut state = StreamState::default();
        // Payload delta dibangun via json! agar tak ada escaping manual berlapis.
        let delta_payload = serde_json::json!({
            "contentBlockIndex": 1,
            "delta": {"toolUse": {"input": "{\"cmd\":\"ls\"}"}},
        })
        .to_string();
        let seq = [
            canned_frame(
                "event",
                "contentBlockStart",
                b"{\"contentBlockIndex\":1,\"start\":{\"toolUse\":{\"toolUseId\":\"tu-1\",\"name\":\"exec\"}}}",
            ),
            canned_frame("event", "contentBlockDelta", delta_payload.as_bytes()),
            canned_frame("event", "contentBlockStop", b"{\"contentBlockIndex\":1}"),
        ]
        .concat();
        pump_stream_chunk(&mut state, &seq, &tx).await.unwrap();
        drop(tx);
        let mut got: Vec<(usize, Option<String>, Option<String>, String)> = Vec::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::ToolCallDelta {
                    index,
                    id,
                    name,
                    arguments_delta,
                } => got.push((index, id, name, arguments_delta)),
                _ => panic!("event tak terduga"),
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, 1);
        assert_eq!(got[0].1.as_deref(), Some("tu-1"));
        assert_eq!(got[0].2.as_deref(), Some("exec"));
        assert_eq!(got[0].3, "");
        assert_eq!(got[1].3, r#"{"cmd":"ls"}"#);
    }

    #[tokio::test]
    async fn eventstream_exception_becomes_stream_error() {
        let (tx, _rx) = mpsc::channel::<StreamEvent>(32);
        let mut state = StreamState::default();
        let frame = canned_frame(
            "exception",
            "modelStreamErrorException",
            b"{\"message\":\"model bermasalah\"}",
        );
        let err = pump_stream_chunk(&mut state, &frame, &tx)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("modelStreamErrorException"), "{msg}");
        assert!(msg.contains("model bermasalah"), "{msg}");
    }
}

#[cfg(test)]
mod placeholder_tests {
    use super::*;
    use hermes_core::ContentBlock;

    fn texts_of(blocks: &[Value]) -> Vec<&str> {
        blocks
            .iter()
            .map(|b| {
                b.get("text")
                    .and_then(|t| t.as_str())
                    .unwrap_or("<non-text>")
            })
            .collect()
    }

    #[test]
    fn empty_and_whitespace_text_become_empty_placeholder() {
        // Converse menolak blok {"text": ""} maupun whitespace-only;
        // keduanya diganti "(empty)" (paritas `_safe_text`).
        let content = vec![
            ContentBlock::Text {
                text: String::new(),
            },
            ContentBlock::Text {
                text: "   \n\t ".to_string(),
            },
            ContentBlock::Text {
                text: "halo".to_string(),
            },
        ];
        let blocks = BedrockProvider::content_blocks(&content, false);
        assert_eq!(texts_of(&blocks), vec!["(empty)", "(empty)", "halo"]);
    }

    #[test]
    fn all_blank_content_yields_single_placeholder_block() {
        // Semua blok kosong -> bukan array kosong (Converse butuh isi),
        // melainkan satu blok placeholder.
        let content = vec![ContentBlock::Text {
            text: " ".to_string(),
        }];
        let blocks = BedrockProvider::content_blocks(&content, true);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"].as_str(), Some("(empty)"));
    }

    #[test]
    fn empty_tool_result_text_becomes_placeholder() {
        // ToolResult dengan konten kosong juga lewat `_safe_text` di original.
        let msgs = vec![hermes_core::Message {
            role: hermes_core::Role::Tool,
            content: vec![ContentBlock::ToolResult {
                result: hermes_core::ToolResult {
                    tool_call_id: "c1".to_string(),
                    name: "t".to_string(),
                    content: String::new(),
                    is_error: false,
                },
            }],
            name: None,
        }];
        let (_system, out) = BedrockProvider::convert(&msgs);
        // Paritas `convert_messages_to_converse`: `content` berupa LIST blok,
        // tanpa field `status`.
        let tool_result = &out[0]["content"][0]["toolResult"];
        assert!(tool_result.get("status").is_none());
        let text = &tool_result["content"][0]["text"];
        assert_eq!(text.as_str(), Some("(empty)"));
    }

    #[test]
    fn convert_merges_same_role_and_pads_ends() {
        // Paritas `convert_messages_to_converse`: tetangga se-role digabung;
        // stream yang diawali/diakhiri assistant di-pad user placeholder.
        use hermes_core::{Message, Role};
        let txt = |t: &str| ContentBlock::Text { text: t.into() };
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![txt("a1")],
                name: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![txt("a2")],
                name: None,
            },
            Message {
                role: Role::User,
                content: vec![txt("u1")],
                name: None,
            },
        ];
        let (_system, out) = BedrockProvider::convert(&msgs);
        // [pad user, assistant(a1+a2), user(u1)] — ujung akhir sudah user,
        // jadi tidak di-pad.
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"][0]["text"], "(empty)");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"].as_array().unwrap().len(), 2);
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["text"], "u1");
    }

    #[test]
    fn convert_tool_results_merge_into_user_turn() {
        // Dua ToolResult berurutan menjadi satu user turn (bukan dua).
        use hermes_core::{Message, Role};
        let tool_msg = |id: &str| Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                result: hermes_core::ToolResult {
                    tool_call_id: id.to_string(),
                    name: "t".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                },
            }],
            name: None,
        };
        let msgs = vec![tool_msg("c1"), tool_msg("c2")];
        let (_system, out) = BedrockProvider::convert(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn build_body_omits_empty_inference_config() {
        // Paritas `build_converse_kwargs`: inferenceConfig dihilangkan bila
        // kosong; tidak pernah berisi null.
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            reasoning_effort: None,
        };
        let body = BedrockProvider::build_body(&req);
        let obj = body.as_object().expect("body harus objek JSON");
        assert!(
            !obj.contains_key("inferenceConfig"),
            "inferenceConfig kosong harus dihilangkan"
        );
        assert!(!obj.contains_key("toolConfig"));
        assert!(!obj.contains_key("system"));
    }

    #[test]
    fn build_body_includes_set_inference_fields() {
        let req = ChatRequest {
            model: "m".to_string(),
            messages: vec![],
            tools: vec![],
            max_tokens: Some(50),
            temperature: Some(0.5),
            reasoning_effort: None,
        };
        let body = BedrockProvider::build_body(&req);
        assert_eq!(body["inferenceConfig"]["maxTokens"], json!(50));
        assert_eq!(body["inferenceConfig"]["temperature"], json!(0.5));
    }

    // ------------------------------------------------------------------
    // Chain region & kredensial (audit W3-P4).
    // ------------------------------------------------------------------

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn region_chain_order() {
        // AWS_REGION menang atas AWS_DEFAULT_REGION.
        let env = env_of(&[
            ("AWS_REGION", "eu-west-1"),
            ("AWS_DEFAULT_REGION", "ap-south-1"),
        ]);
        assert_eq!(resolve_bedrock_region(&env), "eu-west-1");
        // Hanya AWS_DEFAULT_REGION.
        let env = env_of(&[("AWS_DEFAULT_REGION", "ap-south-1")]);
        assert_eq!(resolve_bedrock_region(&env), "ap-south-1");
        // Tidak ada → us-east-1.
        let env: HashMap<String, String> = HashMap::new();
        assert_eq!(resolve_bedrock_region(&env), "us-east-1");
    }

    #[test]
    fn region_blank_values_count_as_unset() {
        // String kosong / whitespace dianggap tidak di-set (paritas .strip()).
        let env = env_of(&[("AWS_REGION", ""), ("AWS_DEFAULT_REGION", "   ")]);
        assert_eq!(resolve_bedrock_region(&env), "us-east-1");
        let env = env_of(&[("AWS_REGION", "  "), ("AWS_DEFAULT_REGION", "eu-central-1")]);
        assert_eq!(resolve_bedrock_region(&env), "eu-central-1");
        // Whitespace di sekitar nilai valid dipangkas.
        let env = env_of(&[("AWS_REGION", "  us-west-2  ")]);
        assert_eq!(resolve_bedrock_region(&env), "us-west-2");
    }

    #[test]
    fn credentials_from_explicit_env() {
        let env = env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
            ("AWS_SESSION_TOKEN", "token"),
        ]);
        let creds = credentials_from_env_vars(&env).expect("harus resolve");
        assert_eq!(creds.access_key, "AKIAEXAMPLE");
        assert_eq!(creds.secret_key, "secret");
        assert_eq!(creds.session_token.as_deref(), Some("token"));

        // Session token opsional.
        let env = env_of(&[
            ("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE"),
            ("AWS_SECRET_ACCESS_KEY", "secret"),
        ]);
        let creds = credentials_from_env_vars(&env).expect("harus resolve");
        assert!(creds.session_token.is_none());

        // Secret hilang → level ini dilewati (None), bukan error.
        let env = env_of(&[("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")]);
        assert!(credentials_from_env_vars(&env).is_none());
    }

    #[test]
    fn ini_parse_reads_default_profile_only() {
        let ini = "\
# komentar
; komentar gaya ini juga
[other]
aws_access_key_id = OTHERKEY
aws_secret_access_key = othersecret

[default]
aws_access_key_id = AKIADEFAULT
aws_secret_access_key = defaultsecret
aws_session_token = defaulttoken
";
        let creds = parse_aws_credentials_ini(ini).expect("harus resolve");
        assert_eq!(creds.access_key, "AKIADEFAULT");
        assert_eq!(creds.secret_key, "defaultsecret");
        assert_eq!(creds.session_token.as_deref(), Some("defaulttoken"));
    }

    #[test]
    fn ini_parse_rejects_incomplete_or_missing_default() {
        // Tanpa [default].
        let ini = "[other]\naws_access_key_id = X\naws_secret_access_key = Y\n";
        assert!(parse_aws_credentials_ini(ini).is_none());
        // Secret hilang di [default].
        let ini = "[default]\naws_access_key_id = X\n";
        assert!(parse_aws_credentials_ini(ini).is_none());
        // Header case-insensitive, kunci case-insensitive, spasi longgar.
        let ini = "[DEFAULT]\n AWS_Access_Key_Id=Z \nAws_Secret_Access_Key = S\n";
        let creds = parse_aws_credentials_ini(ini).expect("harus resolve");
        assert_eq!(creds.access_key, "Z");
        assert_eq!(creds.secret_key, "S");
    }

    /// HOME sementara untuk tes file kredensial — TIDAK PERNAH menyentuh HOME asli.
    fn temp_home_with_credentials(contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-bedrock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let aws_dir = dir.join(".aws");
        std::fs::create_dir_all(&aws_dir).expect("buat dir temp");
        std::fs::write(aws_dir.join("credentials"), contents).expect("tulis credentials");
        dir
    }

    #[test]
    fn credentials_file_from_temp_home() {
        let home = temp_home_with_credentials(
            "[default]\naws_access_key_id = AKIAFILE\naws_secret_access_key = filesecret\n",
        );
        let creds =
            credentials_from_aws_credentials_file(Some(&home)).expect("harus resolve dari file");
        assert_eq!(creds.access_key, "AKIAFILE");
        assert_eq!(creds.secret_key, "filesecret");
        assert!(creds.session_token.is_none());
        std::fs::remove_dir_all(&home).ok();

        // File tidak ada → None (bukan panic).
        let missing =
            std::env::temp_dir().join(format!("hermes-bedrock-missing-{}", std::process::id()));
        assert!(credentials_from_aws_credentials_file(Some(&missing)).is_none());
        assert!(credentials_from_aws_credentials_file(None).is_none());
    }

    #[test]
    fn imds_unreachable_fails_fast() {
        // Port loopback tertutup (connection refused) + alamat TEST-NET-1 yang
        // tidak ter-rute (connect timeout). Keduanya harus gagal cepat —
        // TIDAK BOLEH menggantung, tanpa dependensi jaringan nyata.
        let start = std::time::Instant::now();
        assert!(credentials_from_imds("http://127.0.0.1:9").is_none());
        assert!(credentials_from_imds("http://192.0.2.1").is_none());
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "IMDS fallback terlalu lambat: {elapsed:?}"
        );
    }

    /// Server IMDS tiruan minimal di atas TcpListener std: melayani tepat 3
    /// permintaan (PUT token → GET role → GET dokumen kredensial).
    fn spawn_mock_imds() -> (String, std::thread::JoinHandle<()>) {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock IMDS");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().expect("accept");
                stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut head = Vec::new();
                let mut chunk = [0u8; 1024];
                loop {
                    match stream.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            head.extend_from_slice(&chunk[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > 8192 {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let request_line = String::from_utf8_lossy(&head)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string();
                // Verifikasi handshake IMDSv2: token PUT membawa TTL, GET
                // berikutnya membawa token.
                let head_text = String::from_utf8_lossy(&head).to_lowercase();
                let body = if request_line.starts_with("PUT /latest/api/token") {
                    assert!(
                        head_text.contains("x-aws-ec2-metadata-token-ttl-seconds"),
                        "PUT token harus membawa header TTL"
                    );
                    "mock-imds-token"
                } else if request_line == "GET /latest/meta-data/iam/security-credentials/ HTTP/1.0"
                {
                    assert!(
                        head_text.contains("x-aws-ec2-metadata-token: mock-imds-token"),
                        "GET role harus membawa token"
                    );
                    "mock-role"
                } else {
                    assert!(
                        request_line.starts_with(
                            "GET /latest/meta-data/iam/security-credentials/mock-role"
                        ),
                        "request tak terduga: {request_line}"
                    );
                    assert!(
                        head_text.contains("x-aws-ec2-metadata-token: mock-imds-token"),
                        "GET dokumen harus membawa token"
                    );
                    r#"{"AccessKeyId":"AKIAIMDS","SecretAccessKey":"imdssecret","Token":"imdstoken"}"#
                };
                let response = format!(
                    "HTTP/1.0 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (base, handle)
    }

    #[test]
    fn imds_mock_happy_path() {
        let (base, handle) = spawn_mock_imds();
        let creds = credentials_from_imds(&base).expect("IMDS mock harus resolve");
        assert_eq!(creds.access_key, "AKIAIMDS");
        assert_eq!(creds.secret_key, "imdssecret");
        assert_eq!(creds.session_token.as_deref(), Some("imdstoken"));
        handle.join().expect("mock IMDS selesai");
    }

    #[test]
    fn blank_system_text_is_dropped_not_placeholder() {
        // `_system_blocks`: bagian system kosong dibuang, bukan placeholder.
        let msgs = vec![hermes_core::Message {
            role: hermes_core::Role::System,
            content: vec![ContentBlock::Text {
                text: "  ".to_string(),
            }],
            name: None,
        }];
        let (system, _out) = BedrockProvider::convert(&msgs);
        assert!(system.is_empty());
    }
}
