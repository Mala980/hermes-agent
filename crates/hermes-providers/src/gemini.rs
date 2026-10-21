//! Provider Gemini native (Google AI Studio / Vertex AI Express Mode).
//!
//! REST `streamGenerateContent` dengan SSE. Auth via header `x-goog-api-key`
//! (env `GEMINI_API_KEY` atau `GOOGLE_API_KEY`).
use crate::credential_pool::SharedPool;
use crate::CredentialPool;
use async_trait::async_trait;
use hermes_core::{
    ChatRequest, ContentBlock, HermesError, LlmProvider, Message, Result, Role, StreamEvent,
    ToolDefinition, Usage,
};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct GeminiProvider {
    default_model: String,
    pool: Arc<SharedPool>,
    client: Client,
    base_url: String,
}

/// Paritas `DEFAULT_GEMINI_BASE_URL` (agent/gemini_native_adapter.py).
pub const DEFAULT_GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
/// Paritas `GEMINI_DEFAULT_MAX_OUTPUT_TOKENS`: ceiling output yang dipakai
/// bila `max_tokens` tidak diisi (default internal API memotong output).
pub const GEMINI_DEFAULT_MAX_OUTPUT_TOKENS: u32 = 65535;
/// Paritas `_SKIP_SIGNATURE`: sentinel agar Gemini 3 thinking tidak menolak
/// replay functionCall tanpa thoughtSignature (400 INVALID_ARGUMENT).
const SKIP_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

/// True bila base menunjuk permukaan Vertex AI express-mode (paritas
/// `is_vertex_express_base_url`).
fn is_vertex_express_base_url(base_url: &str) -> bool {
    let lower = base_url.to_lowercase();
    lower.contains("aiplatform.googleapis.com") && !lower.contains("/endpoints/")
}

/// True bila segmen path cocok `v\d+(alpha|beta)?\d*` (case-insensitive),
/// paritas `_API_VERSION_SEGMENT`.
fn is_api_version_segment(seg: &str) -> bool {
    let rest = match seg.strip_prefix('v').or_else(|| seg.strip_prefix('V')) {
        Some(r) if !r.is_empty() => r,
        _ => return false,
    };
    let mut chars = rest.chars().peekable();
    let mut saw_digit = false;
    while matches!(chars.peek(), Some(c) if c.is_ascii_digit()) {
        chars.next();
        saw_digit = true;
    }
    if !saw_digit {
        return false;
    }
    let tail: String = chars.collect();
    if tail.is_empty() {
        return true;
    }
    let lower = tail.to_lowercase();
    let tail = if let Some(t) = lower.strip_prefix("alpha") {
        t
    } else if let Some(t) = lower.strip_prefix("beta") {
        t
    } else {
        return false;
    };
    tail.chars().all(|c| c.is_ascii_digit())
}

fn last_path_segment(url: &str) -> &str {
    url.rsplit('/').next().unwrap_or("")
}

/// Paritas `normalize_gemini_base_url` (agent/gemini_native_adapter.py):
/// base diperlakukan sebagai host root dan segmen versi API dijamin ada;
/// sufiks `/openai` dibuang; segmen versi yang sudah ada dipertahankan;
/// input kosong -> default. Base Vertex express dilengkapi ke bentuk
/// `.../publishers/google`.
pub fn normalize_gemini_base_url(base_url: Option<&str>) -> String {
    let mut trimmed = base_url.unwrap_or("").trim().to_string();
    while trimmed.ends_with('/') {
        trimmed.pop();
    }
    if trimmed.len() >= 7 && trimmed[trimmed.len() - 7..].eq_ignore_ascii_case("/openai") {
        trimmed.truncate(trimmed.len() - 7);
        while trimmed.ends_with('/') {
            trimmed.pop();
        }
    }
    if trimmed.is_empty() {
        trimmed = DEFAULT_GEMINI_BASE_URL.to_string();
    }
    if is_vertex_express_base_url(&trimmed) {
        let lower = trimmed.to_lowercase();
        if lower.ends_with("/publishers/google") {
            return trimmed;
        }
        if !lower.contains("/projects/") && !is_api_version_segment(last_path_segment(&trimmed)) {
            trimmed.push_str("/v1beta1");
        }
        trimmed.push_str("/publishers/google");
        return trimmed;
    }
    if is_api_version_segment(last_path_segment(&trimmed)) {
        return trimmed;
    }
    trimmed.push_str("/v1beta");
    trimmed
}

/// Paritas `bare_gemini_model_id`: buang prefix agregator `google/`/`gemini/`.
fn bare_gemini_model_id(model: &str) -> String {
    let name = model.trim();
    for prefix in ["google/", "gemini/"] {
        if name.len() > prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix) {
            let rest = name[prefix.len()..].trim();
            if !rest.is_empty() {
                return rest.to_string();
            }
        }
    }
    name.to_string()
}

/// Paritas `_gemini_version`: `(major, minor)` dari id model, mis.
/// `gemini-4-argon` -> (4, 0), `google/gemini-3.8-flash` -> (3, 8);
/// (0, 0) untuk id tanpa versi.
fn gemini_version(model: &str) -> (u32, u32) {
    let bare = bare_gemini_model_id(model).to_lowercase();
    let rest = match bare.strip_prefix("gemini-") {
        Some(r) => r,
        None => return (0, 0),
    };
    let mut chars = rest.chars().peekable();
    let mut major = String::new();
    while let Some(c) = chars.peek() {
        if c.is_ascii_digit() {
            major.push(*c);
            chars.next();
        } else {
            break;
        }
    }
    if major.is_empty() {
        return (0, 0);
    }
    let major: u32 = major.parse().unwrap_or(0);
    let tail: String = chars.collect();
    let minor = tail
        .strip_prefix('.')
        .map(|m| {
            m.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .unwrap_or(0);
    (major, minor)
}

/// Paritas `gemini_requires_tool_call_ids`: Gemini 3+ butuh `id` eksplisit
/// pada functionCall/functionResponse agar tool call paralel berpasangan
/// dengan responsnya; 2.x menolak field tersebut.
fn gemini_requires_tool_call_ids(model: &str) -> bool {
    gemini_version(model).0 >= 3
}

/// Paritas `_looks_like_json_schema`: hasil tool yang berupa JSON Schema
/// (`$ref: "#/..."`) harus diteruskan sebagai teks opaque — Gemini 3
/// me-resolve pointer `$ref` di dalam functionResponse.response dan menolak
/// yang tak dikenal dengan 400.
fn looks_like_json_schema(node: &Value) -> bool {
    match node {
        Value::Object(map) => map.iter().any(|(k, v)| {
            (k == "$ref" && v.as_str().map(|s| s.starts_with("#/")).unwrap_or(false))
                || looks_like_json_schema(v)
        }),
        Value::Array(arr) => arr.iter().any(looks_like_json_schema),
        _ => false,
    }
}

impl GeminiProvider {
    pub fn new(default_model: impl Into<String>, api_keys: Vec<String>) -> Self {
        Self::with_base_url(default_model, api_keys, None)
    }

    /// Konstruktor dengan base URL eksplisit (dari konfigurasi).
    ///
    /// Presedensi paritas original: `GEMINI_BASE_URL` (env) > base_url
    /// konfigurasi eksplisit > default. Base hasil akhir dinormalisasi via
    /// [`normalize_gemini_base_url`].
    pub fn with_base_url(
        default_model: impl Into<String>,
        api_keys: Vec<String>,
        base_url: Option<String>,
    ) -> Self {
        let env_base = std::env::var("GEMINI_BASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let cfg_base = base_url
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() && v != DEFAULT_GEMINI_BASE_URL);
        let base_url = normalize_gemini_base_url(env_base.or(cfg_base).as_deref());
        Self {
            default_model: default_model.into(),
            pool: Arc::new(SharedPool::new(CredentialPool::new(api_keys))),
            client: Client::new(),
            base_url,
        }
    }

    /// Paritas `gemini_accepts_parameters_json_schema`: field
    /// `FunctionDeclaration.parametersJsonSchema` (JSON Schema penuh) hanya
    /// ada di permukaan `v1beta` generativelanguage; versi lain dan proxy
    /// tak dikenal memakai subset OpenAPI `parameters`.
    fn accepts_parameters_json_schema(&self) -> bool {
        self.base_url
            .trim_end_matches('/')
            .to_lowercase()
            .ends_with("/v1beta")
    }

    pub fn collect_keys() -> Vec<String> {
        let mut keys = Vec::new();
        for env in ["GEMINI_API_KEY", "GOOGLE_API_KEY"] {
            if let Ok(k) = std::env::var(env) {
                if !k.trim().is_empty() {
                    keys.push(k);
                }
            }
        }
        keys
    }

    /// Konversi pesan core -> format Gemini native.
    ///
    /// Paritas `_build_gemini_contents` (agent/gemini_native_adapter.py):
    /// - `systemInstruction` = `{"role": "system", "parts": [{"text": ...}]}`,
    ///   teks system digabung dengan `"\n"`;
    /// - Gemini 3+ (`gemini_requires_tool_call_ids`): `functionCall` dan
    ///   `functionResponse` membawa `id` eksplisit agar replay paralel
    ///   berpasangan; 2.x tidak dikirimi field `id`.
    fn convert(messages: &[Message], model: &str) -> (Option<Value>, Vec<Value>) {
        let include_ids = gemini_requires_tool_call_ids(model);
        let mut system_parts = Vec::new();
        let mut contents = Vec::new();
        for m in messages {
            match m.role {
                Role::System => {
                    for b in &m.content {
                        if let ContentBlock::Text { text } = b {
                            if !text.is_empty() {
                                system_parts.push(text.clone());
                            }
                        }
                    }
                }
                Role::User => {
                    contents.push(
                        json!({"role": "user", "parts": Self::parts(&m.content, false, include_ids)}),
                    );
                }
                Role::Assistant => {
                    contents.push(
                        json!({"role": "model", "parts": Self::parts(&m.content, true, include_ids)}),
                    );
                }
                Role::Tool => {
                    // ToolResult → functionResponse dalam pesan user.
                    let mut parts = Vec::new();
                    for b in &m.content {
                        if let ContentBlock::ToolResult { result } = b {
                            parts.push(Self::function_response_part(result, include_ids));
                        }
                    }
                    if !parts.is_empty() {
                        contents.push(json!({"role": "user", "parts": parts}));
                    }
                }
            }
        }
        let joined = system_parts.join("\n").trim().to_string();
        let system = if joined.is_empty() {
            None
        } else {
            // Paritas `_build_gemini_contents`: systemInstruction memakai
            // kunci `role: "system"`.
            Some(json!({"role": "system", "parts": [{"text": joined}]}))
        };
        (system, Self::merge_alternating(contents))
    }

    /// Satu part `functionResponse`, paritas `_translate_tool_result_to_gemini`:
    /// objek JSON dikirim apa adanya (kecuali JSON Schema — diteruskan sebagai
    /// teks opaque), selain itu dibungkus `{"output": teks}`.
    fn function_response_part(result: &hermes_core::ToolResult, include_ids: bool) -> Value {
        let mut fr = serde_json::Map::new();
        fr.insert("name".to_string(), json!(result.name));
        fr.insert(
            "response".to_string(),
            Self::function_response_payload(&result.content),
        );
        if include_ids && !result.tool_call_id.is_empty() {
            fr.insert("id".to_string(), json!(result.tool_call_id));
        }
        json!({ "functionResponse": Value::Object(fr) })
    }

    /// `functionResponse.response` mengikuti aslinya: bila output tool berupa
    /// objek JSON (dan bukan JSON Schema — lihat [`looks_like_json_schema`]),
    /// kirim apa adanya; selain itu bungkus sebagai `{"output": <teks>}`.
    fn function_response_payload(text: &str) -> Value {
        let parsed = if text.trim_start().starts_with('{') || text.trim_start().starts_with('[') {
            serde_json::from_str::<Value>(text).ok()
        } else {
            None
        };
        match parsed {
            Some(obj @ Value::Object(_)) if !looks_like_json_schema(&obj) => obj,
            _ => json!({"output": text}),
        }
    }

    fn has_function_response(content: &Value) -> bool {
        content
            .get("parts")
            .and_then(|p| p.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("functionResponse").is_some())
            })
            .unwrap_or(false)
    }

    /// Gabungkan content berurutan dengan role sama (setara dengan
    /// `_merge_alternating` aslinya): Gemini menolak multiturn request yang
    /// tidak bergantian role (HTTP 400).
    ///
    /// Pengecualian seperti aslinya: jangan pernah melebur user text turn
    /// ke dalam user content sebelumnya yang hanya membawa functionResponse
    /// (atau sebaliknya) — model membaca teks trailing sebagai lanjutan
    /// dari hasil tool dan mengembalikan candidate kosong. Pasangan yang
    /// dipisah tetap valid via placeholder model turn di antaranya.
    fn merge_alternating(contents: Vec<Value>) -> Vec<Value> {
        let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
        for content in contents {
            if Self::same_role(merged.last(), &content)
                && content.get("role").and_then(Value::as_str) == Some("user")
                && Self::has_function_response(merged.last().unwrap())
                    != Self::has_function_response(&content)
            {
                // Jangan melebur; selipkan placeholder model turn.
                merged.push(json!({
                    "role": "model",
                    "parts": [{"text": INTERRUPTED_RESPONSE_PLACEHOLDER}],
                }));
            }
            if Self::same_role(merged.last(), &content) {
                let incoming: Vec<Value> = content
                    .get("parts")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                merged.last_mut().unwrap()["parts"]
                    .as_array_mut()
                    .unwrap()
                    .extend(incoming);
            } else {
                merged.push(content);
            }
        }
        merged
    }

    fn same_role(a: Option<&Value>, b: &Value) -> bool {
        a.and_then(|p| p.get("role"))
            .zip(b.get("role"))
            .map(|(r1, r2)| r1 == r2)
            .unwrap_or(false)
    }

    fn parts(content: &[ContentBlock], from_model: bool, include_ids: bool) -> Vec<Value> {
        let mut parts = Vec::new();
        for b in content {
            match b {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        parts.push(json!({"text": text}));
                    }
                }
                ContentBlock::Image { data_url } => {
                    parts.push(image_part(data_url));
                }
                ContentBlock::ToolCall { call } => {
                    if from_model {
                        // Paritas `_translate_tool_call_to_gemini`:
                        // `thoughtSignature` selalu dikirim (sentinel bila
                        // tidak ada signature replay), dan Gemini 3+
                        // mendapat `id` eksplisit pada functionCall.
                        let mut fc = serde_json::Map::new();
                        fc.insert("name".to_string(), json!(call.name));
                        fc.insert("args".to_string(), call.arguments.clone());
                        if include_ids && !call.id.is_empty() {
                            fc.insert("id".to_string(), json!(call.id));
                        }
                        parts.push(json!({
                            "functionCall": Value::Object(fc),
                            "thoughtSignature": SKIP_THOUGHT_SIGNATURE,
                        }));
                    }
                }
                ContentBlock::ToolResult { .. } => {}
            }
        }
        if parts.is_empty() {
            parts.push(json!({"text": ""}));
        }
        parts
    }

    /// Paritas `_translate_tools_to_gemini` + `_function_declaration`:
    /// deklarasi tanpa nama dilewati, nama duplikat dibuang, `description`
    /// hanya bila non-kosong, `parameters` hanya bila berupa objek —
    /// disanitasi ke subset OpenAPI (`parameters`) atau JSON Schema penuh
    /// (`parametersJsonSchema`, permukaan v1beta).
    fn convert_tools(tools: &[ToolDefinition], json_schema: bool) -> Vec<Value> {
        let mut seen = std::collections::HashSet::new();
        let mut decls = Vec::new();
        for t in tools {
            if t.name.is_empty() {
                continue;
            }
            if !seen.insert(t.name.clone()) {
                tracing::warn!("nama tool duplikat '{}' dibuang", t.name);
                continue;
            }
            let mut decl = serde_json::Map::new();
            decl.insert("name".to_string(), json!(t.name));
            if !t.description.is_empty() {
                decl.insert("description".to_string(), json!(t.description));
            }
            if t.parameters.is_object() {
                if json_schema {
                    decl.insert(
                        "parametersJsonSchema".to_string(),
                        prepare_gemini_tool_parameters(&t.parameters),
                    );
                } else {
                    decl.insert(
                        "parameters".to_string(),
                        sanitize_gemini_tool_parameters(&t.parameters),
                    );
                }
            }
            decls.push(Value::Object(decl));
        }
        if decls.is_empty() {
            vec![]
        } else {
            vec![json!({"functionDeclarations": decls})]
        }
    }
}

// Placeholder model turn seperti aslinya (`_INTERRUPTED_RESPONSE_PLACEHOLDER`):
// diselipkan antara functionResponse content dan human text content agar
// pasangan yang tidak digabung tetap bergantian role.
const INTERRUPTED_RESPONSE_PLACEHOLDER: &str =
    "[The previous response was interrupted before it completed.]";

fn image_part(data_url: &str) -> Value {
    if let Some(rest) = data_url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(";base64,") {
            let mime = meta.split(';').next().unwrap_or("image/png");
            return json!({"inlineData": {"mimeType": mime, "data": data}});
        }
    }
    json!({"fileData": {"mimeType": "image/jpeg", "fileUri": data_url}})
}

// ── Sanitasi skema tool, paritas agent/gemini_schema.py ─────────────────────

/// Kunci yang diterima `FunctionDeclaration.parameters` Gemini (subset
/// OpenAPI `Schema`); sisanya — mis. `$schema`/`additionalProperties` —
/// dibuang karena membuat request 400.
const GEMINI_SCHEMA_ALLOWED_KEYS: &[&str] = &[
    "type",
    "format",
    "title",
    "description",
    "nullable",
    "enum",
    "maxItems",
    "minItems",
    "properties",
    "required",
    "minProperties",
    "maxProperties",
    "minLength",
    "maxLength",
    "pattern",
    "example",
    "anyOf",
    "propertyOrdering",
    "default",
    "items",
    "minimum",
    "maximum",
];

/// Kunci struktural per tipe, paritas `_GEMINI_STRUCTURAL_KEYS`.
const GEMINI_STRUCTURAL_KEYS_ARRAY: &[&str] = &["items", "minItems", "maxItems"];
const GEMINI_STRUCTURAL_KEYS_OBJECT: &[&str] = &[
    "properties",
    "required",
    "minProperties",
    "maxProperties",
    "propertyOrdering",
];

/// Normalisasi `type: [...]` menjadi `type` tunggal / `anyOf`, paritas
/// `_normalize_type_array` (tools/schema_sanitizer.py).
fn normalize_type_array(value: &[Value], out: &mut serde_json::Map<String, Value>) {
    let has_null = value.iter().any(|t| t.as_str() == Some("null"));
    let non_null: Vec<&str> = value
        .iter()
        .filter_map(|t| t.as_str())
        .filter(|t| *t != "null")
        .collect();
    if non_null.is_empty() {
        out.insert(
            "type".to_string(),
            Value::String(if has_null { "null" } else { "object" }.to_string()),
        );
        return;
    }
    if non_null.len() == 1 {
        out.insert("type".to_string(), Value::String(non_null[0].to_string()));
    } else {
        out.insert(
            "anyOf".to_string(),
            Value::Array(non_null.iter().map(|t| json!({ "type": t })).collect()),
        );
    }
    if has_null {
        out.entry("nullable".to_string())
            .or_insert(Value::Bool(true));
    }
}

/// Nilai enum aman-Gemini (harus string), atau `None` untuk dibuang.
/// Paritas `_stringify_enum_value`.
fn stringify_enum_value(item: &Value) -> Option<String> {
    match item {
        Value::Bool(b) => Some(if *b { "true" } else { "false" }.to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// Salinan skema parameter tool yang kompatibel Gemini: hanya subset kunci
/// yang didokumentasikan yang dipertahankan, rekursif ke
/// `properties`/`items`/`anyOf`. Paritas `sanitize_gemini_schema`.
fn sanitize_gemini_schema(schema: &Value) -> serde_json::Map<String, Value> {
    let mut cleaned = serde_json::Map::new();
    let obj = match schema.as_object() {
        Some(o) => o,
        None => return cleaned,
    };
    for (key, value) in obj {
        if !GEMINI_SCHEMA_ALLOWED_KEYS.contains(&key.as_str()) {
            continue;
        }
        match key.as_str() {
            "properties" => {
                if let Some(props) = value.as_object() {
                    let mut map = serde_json::Map::new();
                    for (name, sub) in props {
                        map.insert(name.clone(), Value::Object(sanitize_gemini_schema(sub)));
                    }
                    cleaned.insert("properties".to_string(), Value::Object(map));
                }
            }
            "items" => {
                cleaned.insert(
                    "items".to_string(),
                    Value::Object(sanitize_gemini_schema(value)),
                );
            }
            "anyOf" => {
                if let Some(arr) = value.as_array() {
                    cleaned.insert(
                        "anyOf".to_string(),
                        Value::Array(
                            arr.iter()
                                .filter(|i| i.is_object())
                                .map(|i| Value::Object(sanitize_gemini_schema(i)))
                                .collect(),
                        ),
                    );
                }
            }
            _ => {
                cleaned.insert(key.clone(), value.clone());
            }
        }
    }

    // Normalisasi `type` array; `type_was_array` dipakai untuk aturan enum.
    let type_was_array = cleaned.get("type").map(|t| t.is_array()).unwrap_or(false);
    if let Some(type_val) = cleaned.remove("type") {
        if let Some(arr) = type_val.as_array() {
            let mut derived = serde_json::Map::new();
            normalize_type_array(arr, &mut derived);
            if let Some(any_of) = derived.remove("anyOf") {
                // Cabang union menyimpan constraint strukturalnya sendiri:
                // kunci struktural dipindah dari induk ke cabang sesuai tipe.
                let constraints = cleaned.remove("anyOf").map(|v| {
                    let mut m = serde_json::Map::new();
                    m.insert("anyOf".to_string(), v);
                    m
                });
                let mut structural = serde_json::Map::new();
                for key in GEMINI_STRUCTURAL_KEYS_ARRAY
                    .iter()
                    .chain(GEMINI_STRUCTURAL_KEYS_OBJECT.iter())
                {
                    if let Some(v) = cleaned.remove(*key) {
                        structural.insert((*key).to_string(), v);
                    }
                }
                let branches: Vec<Value> = any_of
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|branch| {
                        let mut merged = branch.as_object().cloned().unwrap_or_default();
                        if let Some(c) = &constraints {
                            for (k, v) in c {
                                merged.insert(k.clone(), v.clone());
                            }
                        }
                        let branch_type = merged.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let allowed: &[&str] = if branch_type == "array" {
                            GEMINI_STRUCTURAL_KEYS_ARRAY
                        } else if branch_type == "object" {
                            GEMINI_STRUCTURAL_KEYS_OBJECT
                        } else {
                            &[]
                        };
                        for (k, v) in &structural {
                            if allowed.contains(&k.as_str()) {
                                merged.insert(k.clone(), v.clone());
                            }
                        }
                        Value::Object(sanitize_gemini_schema(&Value::Object(merged)))
                    })
                    .collect();
                cleaned.insert("anyOf".to_string(), Value::Array(branches));
            } else if let Some(t) = derived.remove("type") {
                cleaned.insert("type".to_string(), t);
            }
            if derived.get("nullable").and_then(|v| v.as_bool()) == Some(true) {
                cleaned.insert("nullable".to_string(), Value::Bool(true));
            }
        } else {
            cleaned.insert("type".to_string(), type_val);
        }
    }

    // Gemini mewajibkan tiap entri `enum` berupa string.
    if let Some(enum_val) = cleaned.get("enum").and_then(|v| v.as_array()).cloned() {
        let normalized_type = cleaned.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if type_was_array || ["integer", "number", "boolean"].contains(&normalized_type) {
            let mut seen = std::collections::HashSet::new();
            let stringified: Vec<Value> = enum_val
                .iter()
                .filter_map(stringify_enum_value)
                .filter(|s| seen.insert(s.clone()))
                .map(Value::String)
                .collect();
            if stringified.is_empty() {
                cleaned.remove("enum");
            } else {
                cleaned.insert("enum".to_string(), Value::Array(stringified));
            }
        }
    }

    // `required` hanya boleh memuat nama yang ada di `properties` (400 bila
    // tidak — satu skema buruk menggagalkan SELURUH request).
    if let Some(required_val) = cleaned.get("required").and_then(|v| v.as_array()).cloned() {
        let prop_names: std::collections::HashSet<&str> = cleaned
            .get("properties")
            .and_then(|p| p.as_object())
            .map(|m| m.keys().map(|k| k.as_str()).collect())
            .unwrap_or_default();
        let valid: Vec<&str> = required_val
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|n| prop_names.contains(n))
            .collect();
        if valid.is_empty() {
            cleaned.remove("required");
        } else if valid.len() != required_val.len() {
            cleaned.insert(
                "required".to_string(),
                Value::Array(valid.into_iter().map(|s| json!(s)).collect()),
            );
        }
    }

    cleaned
}

/// Normalisasi parameter tool menjadi skema objek Gemini yang valid.
/// Paritas `sanitize_gemini_tool_parameters`.
fn sanitize_gemini_tool_parameters(parameters: &Value) -> Value {
    let cleaned = sanitize_gemini_schema(parameters);
    if cleaned.is_empty() {
        json!({ "type": "object", "properties": {} })
    } else {
        Value::Object(cleaned)
    }
}

/// Resolve JSON pointer dokumen-lokal (`#/...`), paritas `_resolve_local_ref`.
fn resolve_local_ref<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    if !reference.starts_with("#/") {
        return None;
    }
    let mut node = root;
    for raw_part in reference[2..].split('/') {
        let part = raw_part.replace("~1", "/").replace("~0", "~");
        node = node.as_object()?.get(&part)?;
    }
    if node.is_object() {
        Some(node)
    } else {
        None
    }
}

/// Inline `$ref` dokumen-lokal secara rekursif; `Err(())` bila sirkular,
/// tak ter-resolve, atau budget 256 habis — pemanggil lalu memakai skema
/// apa adanya. Paritas `_inline_refs`.
fn inline_refs(
    node: &Value,
    root: &Value,
    budget: &mut i64,
    stack: &mut Vec<String>,
) -> std::result::Result<Value, ()> {
    match node {
        Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(inline_refs(item, root, budget, stack)?);
            }
            Ok(Value::Array(out))
        }
        Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(|r| r.as_str()) {
                if stack.iter().any(|s| s == reference) {
                    return Err(());
                }
                *budget -= 1;
                if *budget < 0 {
                    return Err(());
                }
                let target = resolve_local_ref(root, reference).ok_or(())?;
                stack.push(reference.to_string());
                let inlined = inline_refs(target, root, budget, stack)?;
                stack.pop();
                let mut inlined_map = inlined.as_object().cloned().unwrap_or_default();
                // Sibling `$ref` (description, default, ...) berlaku
                // berdampingan dengan target dan menang atasnya.
                for (k, v) in map {
                    if k == "$ref" {
                        continue;
                    }
                    inlined_map.insert(k.clone(), inline_refs(v, root, budget, stack)?);
                }
                return Ok(Value::Object(inlined_map));
            }
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), inline_refs(v, root, budget, stack)?);
            }
            Ok(Value::Object(out))
        }
        _ => Ok(node.clone()),
    }
}

/// JSON Schema penuh untuk `parametersJsonSchema`: root `$schema` dibuang,
/// `$ref` dokumen-lokal di-inline, `$defs`/`definitions` dibuang, root objek
/// dijamin. Paritas `prepare_gemini_tool_parameters`.
fn prepare_gemini_tool_parameters(parameters: &Value) -> Value {
    let empty_schema = || json!({ "type": "object", "properties": {} });
    let obj = match parameters.as_object() {
        Some(o) if !o.is_empty() => o,
        _ => return empty_schema(),
    };
    let mut schema: serde_json::Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| k.as_str() != "$schema")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let root = Value::Object(schema.clone());
    let mut budget: i64 = 256;
    let mut stack = Vec::new();
    match inline_refs(&root, &root, &mut budget, &mut stack) {
        Ok(Value::Object(m)) => schema = m,
        // `$ref` tak bisa di-inline semua: kirim apa adanya agar provider
        // menyebut masalah sebenarnya (paritas original).
        Err(()) => return Value::Object(schema),
        _ => {}
    }
    schema.remove("$defs");
    schema.remove("definitions");
    if schema.is_empty() {
        return empty_schema();
    }
    if schema.get("type").and_then(|t| t.as_str()) == Some("object")
        && !schema.contains_key("properties")
    {
        schema.insert("properties".to_string(), json!({}));
    }
    Value::Object(schema)
}

/// Paritas `_effective_gemini_max_output_tokens`: `max_tokens` yang tidak
/// diisi menjadi ceiling 65535 (default internal API memotong output);
/// thinking yang aktif menaikkan cap ke ceiling agar token thought tidak
/// memakan habis jawaban.
fn effective_max_output_tokens(max_tokens: Option<u32>, thinking: bool) -> u32 {
    match max_tokens {
        Some(n) if n > 0 && !thinking => n,
        Some(n) => n.max(GEMINI_DEFAULT_MAX_OUTPUT_TOKENS),
        None => GEMINI_DEFAULT_MAX_OUTPUT_TOKENS,
    }
}

/// Satu baris SSE Gemini -> event-event stream. Murni agar bisa diuji.
///
/// Paritas `_parse_sse_line` (agent/gemini_native_adapter.py): hanya baris
/// `data: ` (dengan spasi) yang diproses; `[DONE]` mengakhiri; baris non-JSON
/// dilewati (bukan error); payload non-dict dilewati. `Err(String)` = error
/// dari API (`"error"` di payload) yang menggugurkan stream.
fn handle_gemini_sse_line(
    line: &str,
    usage: &mut Usage,
    tool_index: &mut usize,
) -> std::result::Result<Vec<StreamEvent>, String> {
    let line = line.strip_suffix('\r').unwrap_or(line);
    let Some(data) = line.strip_prefix("data: ") else {
        return Ok(vec![]);
    };
    if data == "[DONE]" {
        return Ok(vec![]);
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Ok(vec![]),
    };
    if !v.is_object() {
        return Ok(vec![]);
    }
    if let Some(err) = v.get("error") {
        return Err(format!("gemini error: {err}"));
    }
    let mut events = Vec::new();
    if let Some(u) = v.get("usageMetadata") {
        usage.input_tokens = u
            .get("promptTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        usage.output_tokens = u
            .get("candidatesTokenCount")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
    }
    let parts = v
        .pointer("/candidates/0/content/parts")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    for part in parts {
        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
            if !t.is_empty() {
                events.push(StreamEvent::TextDelta(t.to_string()));
            }
        }
        if let Some(fc) = part.get("functionCall") {
            let name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = fc
                .get("args")
                .map(|a| a.to_string())
                .unwrap_or_else(|| "{}".to_string());
            events.push(StreamEvent::ToolCallDelta {
                index: *tool_index,
                id: Some(format!("gemini-call-{tool_index}")),
                name: Some(name.to_string()),
                arguments_delta: args,
            });
            *tool_index += 1;
        }
    }
    Ok(events)
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn name(&self) -> &str {
        "gemini"
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    async fn chat_stream(&self, req: ChatRequest) -> Result<mpsc::Receiver<StreamEvent>> {
        let (system_instruction, contents) = Self::convert(&req.messages, &req.model);
        // Urutan kunci generationConfig paritas `build_gemini_request`:
        // temperature, maxOutputTokens, thinkingConfig — yang `None`
        // dihilangkan (tidak pernah null).
        let mut thinking_budget = None;
        if let Some(level) = req.reasoning_effort.as_deref() {
            thinking_budget = hermes_core::thinking_budget_for_effort(level);
        }
        let mut generation = serde_json::Map::new();
        if let Some(t) = req.temperature {
            generation.insert("temperature".to_string(), json!(t));
        }
        generation.insert(
            "maxOutputTokens".to_string(),
            json!(effective_max_output_tokens(
                req.max_tokens,
                thinking_budget.is_some()
            )),
        );
        if let Some(budget) = thinking_budget {
            generation.insert(
                "thinkingConfig".to_string(),
                json!({"thinkingBudget": budget}),
            );
        }
        let mut body = json!({
            "contents": contents,
            "generationConfig": Value::Object(generation),
        });
        // Paritas `build_gemini_request`: `systemInstruction`, `tools`,
        // `toolConfig` hanya dikirim bila terisi.
        if let Some(s) = system_instruction {
            body["systemInstruction"] = s;
        }
        let tools = Self::convert_tools(&req.tools, self.accepts_parameters_json_schema());
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, req.model
        );
        let api_key = self.pool.next_key();
        let mut rb = self.client.post(&url).json(&body);
        if let Some(k) = api_key.clone() {
            rb = rb.header("x-goog-api-key", k);
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
            let mut tool_index: usize = 0;
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
                    let line = buf[..pos].to_string();
                    buf.drain(..=pos);
                    match handle_gemini_sse_line(&line, &mut usage, &mut tool_index) {
                        Ok(events) => {
                            let mut closed = false;
                            for ev in events {
                                if tx.send(ev).await.is_err() {
                                    closed = true;
                                    break;
                                }
                            }
                            if closed {
                                return;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(StreamEvent::Error(e)).await;
                            return;
                        }
                    }
                }
            }
            // Frame terakhir tanpa newline tetap diproses (paritas
            // `_iter_sse_events`: flush residual buffer saat EOF).
            if !buf.trim().is_empty() {
                match handle_gemini_sse_line(&buf, &mut usage, &mut tool_index) {
                    Ok(events) => {
                        for ev in events {
                            if tx.send(ev).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(e)).await;
                        return;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done(usage)).await;
        });
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
            name: None,
        }
    }

    fn assistant_tool_call_msg(name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                call: hermes_core::ToolCall {
                    id: "c1".into(),
                    name: name.into(),
                    arguments: serde_json::json!({}),
                },
            }],
            name: None,
        }
    }

    fn tool_result_msg(call_id: &str, name: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                result: hermes_core::ToolResult {
                    tool_call_id: call_id.into(),
                    name: name.into(),
                    content: content.into(),
                    is_error: false,
                },
            }],
            name: None,
        }
    }

    #[test]
    fn merges_consecutive_user_messages() {
        let msgs = vec![user_msg("a"), user_msg("b")];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "a");
        assert_eq!(contents[0]["parts"][1]["text"], "b");
    }

    #[test]
    fn merges_consecutive_function_responses() {
        let msgs = vec![
            user_msg("run both"),
            assistant_tool_call_msg("exec"),
            tool_result_msg("c1", "exec", "out1"),
            tool_result_msg("c2", "exec", "out2"),
        ];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[2]["role"], "user");
        let parts = contents[2]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts[0].get("functionResponse").is_some());
        assert!(parts[1].get("functionResponse").is_some());
        assert_eq!(parts[0]["functionResponse"]["response"]["output"], "out1");
        assert_eq!(parts[1]["functionResponse"]["response"]["output"], "out2");
    }

    #[test]
    fn keeps_function_response_and_human_text_apart() {
        // functionResponse content + human text content tidak boleh digabung;
        // disisipi placeholder model turn agar tetap bergantian role.
        let msgs = vec![
            assistant_tool_call_msg("exec"),
            tool_result_msg("c1", "exec", "out1"),
            user_msg("lanjut"),
        ];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(contents.len(), 4);
        assert_eq!(contents[0]["role"], "model");
        assert_eq!(contents[1]["role"], "user");
        assert!(contents[1]["parts"][0].get("functionResponse").is_some());
        assert_eq!(contents[2]["role"], "model");
        assert_eq!(
            contents[2]["parts"][0]["text"],
            INTERRUPTED_RESPONSE_PLACEHOLDER
        );
        assert_eq!(contents[3]["role"], "user");
        assert_eq!(contents[3]["parts"][0]["text"], "lanjut");
    }

    #[test]
    fn alternating_turns_are_left_intact() {
        let msgs = vec![
            user_msg("a"),
            assistant_tool_call_msg("exec"),
            user_msg("b"),
        ];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[2]["role"], "user");
    }

    #[test]
    fn function_response_sends_json_object_as_is() {
        let msgs = vec![tool_result_msg(
            "c1",
            "exec",
            r#"{"ok":true,"files":["a"]}"#,
        )];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let response = &contents[0]["parts"][0]["functionResponse"]["response"];
        assert_eq!(response, &serde_json::json!({"ok": true, "files": ["a"]}));
        assert!(response.get("output").is_none());
    }

    #[test]
    fn function_response_wraps_plain_text_as_output() {
        let msgs = vec![tool_result_msg("c1", "exec", "plain output")];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert_eq!(
            contents[0]["parts"][0]["functionResponse"]["response"],
            serde_json::json!({"output": "plain output"})
        );
    }

    #[test]
    fn converts_roles_correctly() {
        let msgs = vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text { text: "sys".into() }],
                name: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
                name: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: hermes_core::ToolCall {
                        id: "c1".into(),
                        name: "exec".into(),
                        arguments: serde_json::json!({"command": "ls"}),
                    },
                }],
                name: None,
            },
        ];
        let (system, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        // Paritas `_build_gemini_contents`: systemInstruction memakai kunci
        // `role: "system"` dengan teks system digabung "\n".
        let system = system.expect("system harus ada");
        assert_eq!(system["role"], "system");
        assert_eq!(system["parts"][0]["text"], "sys");
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[1]["role"], "model");
        assert!(contents[1]["parts"][0].get("functionCall").is_some());
    }

    #[test]
    fn system_instruction_joins_multiple_with_newline() {
        let msgs = vec![
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
        ];
        let (system, _) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let system = system.expect("system harus ada");
        assert_eq!(system["role"], "system");
        assert_eq!(system["parts"][0]["text"], "satu\ndua");
    }

    #[test]
    fn gemini3_function_call_and_response_carry_ids() {
        // Paritas `gemini_requires_tool_call_ids`: Gemini 3+ mendapat `id`
        // eksplisit; 2.x tidak.
        let msgs = vec![
            assistant_tool_call_msg("exec"),
            tool_result_msg("c1", "exec", "out"),
        ];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-3.7-flash");
        let fc = &contents[0]["parts"][0]["functionCall"];
        assert_eq!(fc["id"], "c1");
        assert_eq!(
            contents[0]["parts"][0]["thoughtSignature"],
            SKIP_THOUGHT_SIGNATURE
        );
        let fr = &contents[1]["parts"][0]["functionResponse"];
        assert_eq!(fr["id"], "c1");

        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        assert!(contents[0]["parts"][0]["functionCall"].get("id").is_none());
        assert!(contents[1]["parts"][0]["functionResponse"]
            .get("id")
            .is_none());
    }

    #[test]
    fn gemini_version_parsing_matches_original() {
        // (model, (major, minor)) — paritas `_gemini_version`.
        for (model, expected) in [
            ("gemini-3.7-flash", (3, 7)),
            ("gemini-4-argon", (4, 0)),
            ("google/gemini-3.8-flash", (3, 8)),
            ("gemini/gemini-2.0-flash", (2, 0)),
            ("gemini-2.0-flash", (2, 0)),
            ("gemini-flash-latest", (0, 0)),
            ("gemini-exp-1206", (0, 0)),
            ("llama3.1", (0, 0)),
        ] {
            assert_eq!(gemini_version(model), expected, "model {model}");
            assert_eq!(
                gemini_requires_tool_call_ids(model),
                expected.0 >= 3,
                "ids untuk {model}"
            );
        }
    }

    #[test]
    fn function_response_json_schema_result_goes_as_opaque_text() {
        // Paritas `_looks_like_json_schema`: hasil tool berupa JSON Schema
        // tidak dikirim sebagai response terstruktur.
        let msgs = vec![tool_result_msg(
            "c1",
            "exec",
            r##"{"$ref":"#/$defs/Foo","$defs":{"Foo":{"type":"object"}}}"##,
        )];
        let (_, contents) = GeminiProvider::convert(&msgs, "gemini-2.0-flash");
        let response = &contents[0]["parts"][0]["functionResponse"]["response"];
        assert!(response.get("output").is_some(), "harus dibungkus output");
    }

    #[test]
    fn convert_tools_sanitizes_parameters() {
        // Paritas `sanitize_gemini_tool_parameters`: kunci tak dikenal
        // dibuang; `required` yang tak ada di `properties` dipangkas.
        let tools = vec![hermes_core::ToolDefinition {
            name: "exec".to_string(),
            description: "run".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "$schema": "http://json-schema.org/draft-07/schema#",
                "additionalProperties": false,
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "perintah",
                        "examples": ["ls"],
                    }
                },
                "required": ["cmd", "tak_ada"],
            }),
        }];
        let converted = GeminiProvider::convert_tools(&tools, false);
        let decl = &converted[0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "exec");
        assert_eq!(decl["description"], "run");
        let params = &decl["parameters"];
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert!(params["properties"]["cmd"].get("examples").is_none());
        assert_eq!(params["required"], serde_json::json!(["cmd"]));
    }

    #[test]
    fn convert_tools_uses_json_schema_on_v1beta() {
        // Paritas `gemini_accepts_parameters_json_schema`: permukaan v1beta
        // memakai `parametersJsonSchema` (JSON Schema penuh).
        let tools = vec![hermes_core::ToolDefinition {
            name: "exec".to_string(),
            description: String::new(),
            parameters: serde_json::json!({
                "$schema": "x",
                "type": "object",
                "$defs": {"A": {"type": "string"}},
                "properties": {"p": {"$ref": "#/$defs/A"}},
            }),
        }];
        let converted = GeminiProvider::convert_tools(&tools, true);
        let decl = &converted[0]["functionDeclarations"][0];
        // description kosong tidak dikirim; $ref di-inline; $defs dibuang.
        assert!(decl.get("description").is_none());
        let pjs = &decl["parametersJsonSchema"];
        assert!(pjs.get("$schema").is_none());
        assert!(pjs.get("$defs").is_none());
        assert_eq!(
            pjs["properties"]["p"],
            serde_json::json!({"type": "string"})
        );
    }

    #[test]
    fn convert_tools_omits_and_dedups() {
        assert!(GeminiProvider::convert_tools(&[], false).is_empty());
        let mk = |name: &str| hermes_core::ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            parameters: serde_json::json!({}),
        };
        let converted = GeminiProvider::convert_tools(&[mk("a"), mk(""), mk("a")], false);
        let names: Vec<&str> = converted[0]["functionDeclarations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn normalize_gemini_base_url_matches_original() {
        // (input, ekspektasi) — paritas `normalize_gemini_base_url`.
        for (input, expected) in [
            (None, DEFAULT_GEMINI_BASE_URL),
            (Some(""), DEFAULT_GEMINI_BASE_URL),
            (
                Some("https://generativelanguage.googleapis.com/v1beta"),
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                Some("https://generativelanguage.googleapis.com"),
                "https://generativelanguage.googleapis.com/v1beta",
            ),
            (
                Some("http://localhost:4000/gemini"),
                "http://localhost:4000/gemini/v1beta",
            ),
            (
                Some("http://localhost:4000/gemini/"),
                "http://localhost:4000/gemini/v1beta",
            ),
            (
                Some("https://proxy.example.com/openai"),
                "https://proxy.example.com/v1beta",
            ),
            (
                Some("https://aiplatform.googleapis.com/v1beta1"),
                "https://aiplatform.googleapis.com/v1beta1/publishers/google",
            ),
        ] {
            assert_eq!(
                normalize_gemini_base_url(input),
                expected,
                "input {input:?}"
            );
        }
    }

    #[test]
    fn effective_max_output_tokens_matches_original() {
        // Paritas `_effective_gemini_max_output_tokens`.
        assert_eq!(
            effective_max_output_tokens(None, false),
            GEMINI_DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(
            effective_max_output_tokens(None, true),
            GEMINI_DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(effective_max_output_tokens(Some(100), false), 100);
        assert_eq!(
            effective_max_output_tokens(Some(100), true),
            GEMINI_DEFAULT_MAX_OUTPUT_TOKENS
        );
    }

    #[test]
    fn sse_line_parsing_matches_original() {
        // Paritas `_parse_sse_line`: butuh prefix `data: ` (spasi);
        // non-JSON dilewati; error payload menggugurkan.
        let mut usage = Usage::default();
        let mut idx = 0;
        // Tanpa spasi setelah `data:` -> diabaikan seperti aslinya.
        assert!(handle_gemini_sse_line("data:{}", &mut usage, &mut idx)
            .unwrap()
            .is_empty());
        assert!(
            handle_gemini_sse_line("data: bukan json", &mut usage, &mut idx)
                .unwrap()
                .is_empty()
        );
        let events = handle_gemini_sse_line(
            r#"data: {"candidates":[{"content":{"parts":[{"text":"hi"}]}}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3}}"#,
            &mut usage,
            &mut idx,
        )
        .unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], StreamEvent::TextDelta(t) if t == "hi"));
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 3);
        assert!(
            handle_gemini_sse_line(r#"data: {"error":{"code":400}}"#, &mut usage, &mut idx)
                .is_err()
        );
    }
}
