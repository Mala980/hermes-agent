//! Tools sosial: x_search (cari postingan X/Twitter via xAI Responses API)
//! dan react_to_message.
use crate::schema::*;
use async_trait::async_trait;
use chrono::NaiveDate;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};

// ---------------------------------------------------------------------------
// x_search — via xAI Responses API (BUKAN X API v2).
//
// Paritas dengan tools/x_search_tool.py di original (8f5693af):
// - Endpoint: POST {base_url}/responses, default https://api.x.ai/v1/responses.
// - Auth: header `Authorization: Bearer <XAI_API_KEY>` (+ Content-Type JSON,
//   User-Agent ala Hermes).
// - Body: {model, input:[{role:"user", content:query}], tools:[{type:"x_search",
//   ...filter}], store:false}.
// - Respons: output_text (atau potongan teks output[].message.content[]),
//   citations[], anotasi url_citation inline, dan flag `degraded` bila filter
//   penyempit aktif tapi xAI tidak mengembalikan sitasi.
//
// Keputusan port (didokumentasikan, bukan kelalaian):
// - X_BEARER_TOKEN DIHAPUS: itu kredensial X API v2 (api.x.com) — API yang
//   sama sekali berbeda (endpoint, skema auth, bentuk respons). "Fallback"
//   ke sana berarti memelihara dua implementasi, bukan satu; original pun
//   tidak mengenal X_BEARER_TOKEN.
// - Tanpa parameter jumlah hasil: tool xAI `x_search` tidak punya field
//   count (original juga tidak punya max_results); yang kembali adalah
//   jawaban model + sitasi, bukan daftar tweet mentah.
// - Tanpa reasoning.effort: original membacanya dari config hermes
//   (x_search.reasoning_effort); port tidak punya subsistem config itu.
// - Tanpa jalur xai-oauth: original mengutamakan XAI_API_KEY lalu OAuth;
//   port tidak punya credential pool OAuth, jadi XAI_API_KEY satu-satunya.
// ---------------------------------------------------------------------------

pub struct XSearchTool;

/// Model default xAI untuk x_search (sama seperti original: "grok-4.5").
const X_SEARCH_MODEL_DEFAULT: &str = "grok-4.5";
/// Timeout request dalam detik (sama seperti original: 180).
const X_SEARCH_TIMEOUT_SECS: u64 = 180;
/// Retry untuk 5xx/timeout/connection error (sama seperti original: 2x).
const X_SEARCH_MAX_RETRIES: u32 = 2;
/// Batas handle per filter (sama seperti original: MAX_HANDLES = 10).
const MAX_X_HANDLES: usize = 10;

/// Base URL xAI; bisa di-override via XAI_BASE_URL untuk pengujian.
/// (Original memakai HERMES_XAI_BASE_URL lalu XAI_BASE_URL; port hanya
/// XAI_BASE_URL karena tidak ada lapisan config hermes.)
fn xai_base_url() -> String {
    std::env::var("XAI_BASE_URL")
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://api.x.ai/v1".to_string())
}

/// User-Agent untuk panggilan xAI (original: "Hermes-Agent/<versi>").
pub fn xai_user_agent() -> String {
    format!("hermes-rs/{}", env!("CARGO_PKG_VERSION"))
}

/// Model xAI untuk x_search; bisa di-override via XAI_X_SEARCH_MODEL.
fn x_search_model() -> String {
    std::env::var("XAI_X_SEARCH_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| X_SEARCH_MODEL_DEFAULT.to_string())
}

fn string_array_prop(description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "items": {"type": "string"},
        "description": description,
    })
}

/// Normalisasi daftar handle: buang spasi dan '@' di depan, buang yang
/// kosong, tolak bila lebih dari MAX_X_HANDLES (seperti original).
fn normalize_handles(
    value: Option<&serde_json::Value>,
    field: &str,
) -> std::result::Result<Vec<String>, String> {
    let mut out = Vec::new();
    if let Some(v) = value {
        let arr = v
            .as_array()
            .ok_or_else(|| format!("{field} harus array of string"))?;
        for h in arr {
            let s = h
                .as_str()
                .ok_or_else(|| format!("{field} harus array of string"))?
                .trim()
                .trim_start_matches('@')
                .trim();
            if !s.is_empty() {
                out.push(s.to_string());
            }
        }
    }
    if out.len() > MAX_X_HANDLES {
        return Err(format!("{field} mendukung maksimal {MAX_X_HANDLES} handle"));
    }
    Ok(out)
}

/// Parse tanggal ketat YYYY-MM-DD (seperti original: strptime %Y-%m-%d).
/// None untuk string kosong (filter tidak dipakai).
fn parse_iso_date(raw: &str, field: &str) -> std::result::Result<Option<NaiveDate>, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let b = s.as_bytes();
    let strict = s.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && s[..4].chars().all(|c| c.is_ascii_digit())
        && s[5..7].chars().all(|c| c.is_ascii_digit())
        && s[8..10].chars().all(|c| c.is_ascii_digit());
    if !strict {
        return Err(format!("{field} harus YYYY-MM-DD (dapat {raw:?})"));
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| format!("{field} harus YYYY-MM-DD (dapat {raw:?})"))
}

/// from <= to; from tidak boleh setelah hari ini (UTC). `to` boleh di masa
/// depan (seperti original).
fn validate_date_range(
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> std::result::Result<(), String> {
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(format!(
                "from_date ({}) harus pada atau sebelum to_date ({})",
                f.format("%Y-%m-%d"),
                t.format("%Y-%m-%d")
            ));
        }
    }
    if let Some(f) = from {
        let today = chrono::Utc::now().date_naive();
        if f > today {
            return Err(format!(
                "from_date ({}) di masa depan; X Search hanya mengindeks postingan lampau",
                f.format("%Y-%m-%d")
            ));
        }
    }
    Ok(())
}

/// Bangun definisi tool xAI `{"type":"x_search", ...}` sekaligus daftar
/// filter aktif. Filter aktif dipakai untuk flag `degraded` (lihat original).
fn build_x_search_tool_def(
    allowed: Vec<String>,
    excluded: Vec<String>,
    from_date: &str,
    to_date: &str,
    image_understanding: bool,
    video_understanding: bool,
) -> std::result::Result<(serde_json::Value, Vec<String>), String> {
    if !allowed.is_empty() && !excluded.is_empty() {
        return Err(
            "allowed_x_handles dan excluded_x_handles tidak bisa dipakai bersamaan".to_string(),
        );
    }
    let from = parse_iso_date(from_date, "from_date")?;
    let to = parse_iso_date(to_date, "to_date")?;
    validate_date_range(from, to)?;

    let mut def = serde_json::Map::new();
    def.insert(
        "type".to_string(),
        serde_json::Value::String("x_search".to_string()),
    );
    let mut active = Vec::new();
    if !allowed.is_empty() {
        def.insert(
            "allowed_x_handles".to_string(),
            serde_json::Value::from(allowed),
        );
        active.push("allowed_x_handles".to_string());
    }
    if !excluded.is_empty() {
        def.insert(
            "excluded_x_handles".to_string(),
            serde_json::Value::from(excluded),
        );
        active.push("excluded_x_handles".to_string());
    }
    if let Some(f) = from {
        def.insert(
            "from_date".to_string(),
            serde_json::Value::String(f.format("%Y-%m-%d").to_string()),
        );
        active.push("from_date".to_string());
    }
    if let Some(t) = to {
        def.insert(
            "to_date".to_string(),
            serde_json::Value::String(t.format("%Y-%m-%d").to_string()),
        );
        active.push("to_date".to_string());
    }
    if image_understanding {
        def.insert(
            "enable_image_understanding".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    if video_understanding {
        def.insert(
            "enable_video_understanding".to_string(),
            serde_json::Value::Bool(true),
        );
    }
    Ok((serde_json::Value::Object(def), active))
}

/// Teks jawaban: field output_text dulu, lalu gabungan potongan
/// output_text/text dari item bertipe message (seperti original).
fn extract_answer(payload: &serde_json::Value) -> String {
    if let Some(t) = payload.get("output_text").and_then(|v| v.as_str()) {
        let t = t.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    let mut parts = Vec::new();
    let items = payload
        .get("output")
        .and_then(|o| o.as_array())
        .into_iter()
        .flatten();
    for item in items {
        if item.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let contents = item
            .get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten();
        for c in contents {
            let ctype = c.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ctype == "output_text" || ctype == "text" {
                let t = c.get("text").and_then(|t| t.as_str()).unwrap_or("").trim();
                if !t.is_empty() {
                    parts.push(t.to_string());
                }
            }
        }
    }
    parts.join("\n\n")
}

/// Sitasi inline: anotasi bertipe url_citation di konten message
/// (seperti original).
fn extract_inline_citations(payload: &serde_json::Value) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let items = payload
        .get("output")
        .and_then(|o| o.as_array())
        .into_iter()
        .flatten();
    for item in items {
        if item.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let contents = item
            .get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten();
        for c in contents {
            let annotations = c
                .get("annotations")
                .and_then(|a| a.as_array())
                .into_iter()
                .flatten();
            for a in annotations {
                if a.get("type").and_then(|t| t.as_str()) == Some("url_citation") {
                    out.push(serde_json::json!({
                        "url": a.get("url").and_then(|u| u.as_str()).unwrap_or(""),
                        "title": a.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                        "start_index": a.get("start_index").cloned().unwrap_or(serde_json::Value::Null),
                        "end_index": a.get("end_index").cloned().unwrap_or(serde_json::Value::Null),
                    }));
                }
            }
        }
    }
    out
}

/// Pesan error HTTP ala original: "code: error" dari body JSON bila ada.
fn http_error_message(status: u16, body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if v.is_object() {
            let code = v.get("code").and_then(|c| c.as_str()).unwrap_or("").trim();
            let message = v.get("error").and_then(|e| e.as_str()).unwrap_or("").trim();
            let message = if message.is_empty() {
                v.to_string()
            } else {
                message.to_string()
            };
            if !code.is_empty() && !message.contains(code) {
                return format!("{code}: {message}");
            }
            if !message.is_empty() {
                return message;
            }
        }
    }
    let t = body.trim();
    if t.is_empty() {
        format!("HTTP {status}")
    } else {
        t.chars().take(500).collect()
    }
}

/// Backoff antar retry: min(5.0, 1.5*(attempt+1)) detik (seperti original).
fn retry_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_secs_f64((1.5 * f64::from(attempt) + 1.5).min(5.0))
}

/// POST ke /responses dengan retry untuk 5xx/timeout/connection error
/// (seperti original). Ok hanya untuk 2xx; Err = (pesan, jenis error:
/// "http" | "timeout" | "transport").
async fn post_responses(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    payload: &serde_json::Value,
) -> std::result::Result<reqwest::Response, (String, &'static str)> {
    let mut attempt = 0u32;
    loop {
        let res = client
            .post(url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("User-Agent", xai_user_agent())
            .json(payload)
            .send()
            .await;
        match res {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                if status.is_server_error() && attempt < X_SEARCH_MAX_RETRIES {
                    tokio::time::sleep(retry_backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                return Err((http_error_message(status.as_u16(), &body), "http"));
            }
            Err(e) => {
                let kind = if e.is_timeout() {
                    "timeout"
                } else {
                    "transport"
                };
                if (e.is_timeout() || e.is_connect()) && attempt < X_SEARCH_MAX_RETRIES {
                    tokio::time::sleep(retry_backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                return Err((format!("request gagal: {e}"), kind));
            }
        }
    }
}

/// Hasil sukses → JSON seperti kontrak original (handler original SELALU
/// mengembalikan string JSON dengan field `success`).
fn ok_payload(
    model: &str,
    query: &str,
    answer: String,
    citations: Vec<serde_json::Value>,
    inline_citations: Vec<serde_json::Value>,
    active_filters: &[String],
) -> String {
    // xAI mengembalikan 200 dengan jawaban sintetis walau tidak ada postingan
    // yang cocok dengan filter penyempit; bila kedua kanal sitasi kosong,
    // jawaban berasal dari pengetahuan model, bukan indeks X.
    let degraded =
        !active_filters.is_empty() && citations.is_empty() && inline_citations.is_empty();
    serde_json::json!({
        "success": true,
        "provider": "xai",
        "credential_source": "xai",
        "tool": "x_search",
        "model": model,
        "query": query,
        "answer": answer,
        "citations": citations,
        "inline_citations": inline_citations,
        "degraded": degraded,
        "degraded_reason": if degraded {
            serde_json::Value::String(format!(
                "no citations returned despite filters: {}",
                active_filters.join(", ")
            ))
        } else {
            serde_json::Value::Null
        },
    })
    .to_string()
}

/// Hasil gagal → JSON {success:false, provider, tool, error, error_type}.
/// error_type: "validation" | "credentials" | "http" | "timeout" |
/// "transport" | "parse".
fn err_payload(error: String, error_type: &'static str) -> String {
    serde_json::json!({
        "success": false,
        "provider": "xai",
        "tool": "x_search",
        "error": error,
        "error_type": error_type,
    })
    .to_string()
}

#[async_trait]
impl Tool for XSearchTool {
    fn name(&self) -> &str {
        "x_search"
    }

    fn description(&self) -> &str {
        "Cari postingan, profil, dan thread X (Twitter) via tool bawaan x_search \
         pada xAI Responses API. Butuh env XAI_API_KEY. Hanya untuk penemuan \
         baca-saja — diskusi, reaksi, atau klaim terkini di X publik. Jangan \
         dipakai untuk memposting, membalas, like, DM, mengunggah media, \
         atau menghapus."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("query", string_prop("Apa yang dicari di X.")),
                (
                    "allowed_x_handles",
                    string_array_prop(
                        "Opsional: daftar handle X yang disertakan secara eksklusif (maks 10, tanpa @).",
                    ),
                ),
                (
                    "excluded_x_handles",
                    string_array_prop(
                        "Opsional: daftar handle X yang dikecualikan (maks 10, tanpa @).",
                    ),
                ),
                (
                    "from_date",
                    string_prop("Opsional: tanggal mulai, format YYYY-MM-DD."),
                ),
                (
                    "to_date",
                    string_prop("Opsional: tanggal akhir, format YYYY-MM-DD."),
                ),
                (
                    "enable_image_understanding",
                    boolean_prop("Analisis gambar pada postingan X yang cocok."),
                ),
                (
                    "enable_video_understanding",
                    boolean_prop("Analisis video pada postingan X yang cocok."),
                ),
            ],
            vec!["query"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let query = get_str(&args, "query")?.trim().to_string();
        // Kontrak hasil mengikuti original: content SELALU JSON dengan field
        // `success`; is_error mencerminkan !success. (Argumen wajib yang
        // hilang tetap Err ala konvensi port via `?` di atas.)
        let fail = |error: String, error_type: &'static str| ToolResult {
            tool_call_id: String::new(),
            name: self.name().to_string(),
            content: err_payload(error, error_type),
            is_error: true,
        };
        if query.is_empty() {
            return Ok(fail(
                "query wajib diisi untuk x_search".to_string(),
                "validation",
            ));
        }
        // Kredensial dulu seperti original: error ramah, bukan 401 dari xAI.
        let api_key = match std::env::var("XAI_API_KEY") {
            Ok(k) if !k.trim().is_empty() => k.trim().to_string(),
            _ => {
                return Ok(fail(
                    "Tidak ada kredensial xAI. Set env XAI_API_KEY.".to_string(),
                    "credentials",
                ))
            }
        };
        let allowed = match normalize_handles(args.get("allowed_x_handles"), "allowed_x_handles") {
            Ok(h) => h,
            Err(e) => return Ok(fail(e, "validation")),
        };
        let excluded = match normalize_handles(args.get("excluded_x_handles"), "excluded_x_handles")
        {
            Ok(h) => h,
            Err(e) => return Ok(fail(e, "validation")),
        };
        let from_date = args.get("from_date").and_then(|v| v.as_str()).unwrap_or("");
        let to_date = args.get("to_date").and_then(|v| v.as_str()).unwrap_or("");
        let image_u = args
            .get("enable_image_understanding")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let video_u = args
            .get("enable_video_understanding")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let (tool_def, active_filters) = match build_x_search_tool_def(
            allowed, excluded, from_date, to_date, image_u, video_u,
        ) {
            Ok(t) => t,
            Err(e) => return Ok(fail(e, "validation")),
        };
        let model = x_search_model();
        let payload = serde_json::json!({
            "model": model,
            "input": [{"role": "user", "content": query}],
            "tools": [tool_def],
            "store": false,
        });
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(X_SEARCH_TIMEOUT_SECS))
            .build()
            .map_err(|e| {
                HermesError::tool("x_search", format!("gagal membangun HTTP client: {e}"))
            })?;
        let url = format!("{}/responses", xai_base_url());
        let resp = match post_responses(&client, &url, &api_key, &payload).await {
            Ok(r) => r,
            Err((message, kind)) => return Ok(fail(message, kind)),
        };
        let data: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(fail(format!("parse gagal: {e}"), "parse")),
        };
        let answer = extract_answer(&data);
        let citations: Vec<serde_json::Value> = data
            .get("citations")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let inline_citations = extract_inline_citations(&data);
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: self.name().to_string(),
            content: ok_payload(
                &model,
                &query,
                answer,
                citations,
                inline_citations,
                &active_filters,
            ),
            is_error: false,
        })
    }
}

// ---------------------------------------------------------------------------
// react_to_message — beri reaksi emoji ke pesan (telegram/discord)
//
// CATATAN KEJUJURAN: di original, react_to_message adalah tool tapback UI
// desktop (menempelkan reaksi emoji ke pesan di aplikasi desktop, lewat
// desktop bridge). hermes-rs TIDAK punya desktop bridge, sehingga semantik
// desktop tersebut tidak dapat diimplementasikan. Implementasi ini
// MENGADAPTASI semantik aslinya ke platform chat yang didukung gateway
// (Telegram via setMessageReaction, Discord via create reaction): perilaku
// desktop-tapback original tidak direplikasi di sini.
// ---------------------------------------------------------------------------

pub struct ReactToMessageTool;

#[async_trait]
impl Tool for ReactToMessageTool {
    fn name(&self) -> &str {
        "react_to_message"
    }

    fn description(&self) -> &str {
        "Beri reaksi emoji ke sebuah pesan di Telegram atau Discord. \
         Untuk Telegram: target = chat_id, message_id = id pesan. \
         Untuk Discord: target = channel_id, message_id = id pesan."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("platform", string_prop("telegram | discord.")),
                (
                    "target",
                    string_prop("chat_id (telegram) / channel_id (discord)."),
                ),
                ("message_id", string_prop("ID pesan yang diberi reaksi.")),
                ("emoji", string_prop("Emoji reaksi, mis. 👍.")),
            ],
            vec!["platform", "target", "message_id", "emoji"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<ToolResult> {
        let platform = get_str(&args, "platform")?;
        let target = get_str(&args, "target")?;
        let message_id = get_str(&args, "message_id")?;
        let emoji = get_str(&args, "emoji")?;
        let client = reqwest::Client::new();
        match platform {
            "telegram" => {
                let token = std::env::var("TELEGRAM_BOT_TOKEN")
                    .map_err(|_| HermesError::tool("react_to_message", "set TELEGRAM_BOT_TOKEN"))?;
                // Base URL bisa di-override via TELEGRAM_API_BASE untuk pengujian
                // (env yang sama dipakai gateway telegram).
                let base = std::env::var("TELEGRAM_API_BASE")
                    .unwrap_or_else(|_| "https://api.telegram.org".to_string());
                let resp = client
                    .post(format!(
                        "{}/bot{token}/setMessageReaction",
                        base.trim_end_matches('/')
                    ))
                    .json(&serde_json::json!({
                        "chat_id": target,
                        "message_id": message_id.parse::<i64>().unwrap_or(0),
                        "reaction": [{"type": "emoji", "emoji": emoji}],
                    }))
                    .send()
                    .await
                    .map_err(|e| {
                        HermesError::tool("react_to_message", format!("request gagal: {e}"))
                    })?;
                let v: serde_json::Value = resp.json().await.map_err(|e| {
                    HermesError::tool("react_to_message", format!("parse gagal: {e}"))
                })?;
                if v.get("ok").and_then(|o| o.as_bool()) != Some(true) {
                    return Err(HermesError::tool(
                        "react_to_message",
                        format!("telegram menolak: {v}"),
                    ));
                }
            }
            "discord" => {
                let token = std::env::var("DISCORD_BOT_TOKEN")
                    .map_err(|_| HermesError::tool("react_to_message", "set DISCORD_BOT_TOKEN"))?;
                // Base URL bisa di-override via DISCORD_API_BASE untuk pengujian
                // (env yang sama dipakai gateway discord).
                let base = std::env::var("DISCORD_API_BASE")
                    .unwrap_or_else(|_| "https://discord.com/api/v10".to_string());
                // Discord butuh emoji ter-encode untuk reaksi custom; emoji unicode langsung bisa dipakai.
                let enc: String = urlencoding_lite(emoji);
                let resp = client
                    .put(format!(
                        "{}/channels/{target}/messages/{message_id}/reactions/{enc}/@me",
                        base.trim_end_matches('/')
                    ))
                    .header("Authorization", format!("Bot {token}"))
                    .send()
                    .await
                    .map_err(|e| {
                        HermesError::tool("react_to_message", format!("request gagal: {e}"))
                    })?;
                if !resp.status().is_success() {
                    let b = resp.text().await.unwrap_or_default();
                    return Err(HermesError::tool(
                        "react_to_message",
                        format!("discord menolak: {b}"),
                    ));
                }
            }
            other => {
                return Err(HermesError::tool(
                    "react_to_message",
                    format!("platform tidak didukung: {other}"),
                ))
            }
        }
        Ok(ok_result(self, format!("Reaksi {emoji} terkirim.")))
    }
}

/// Percent-encode minimal untuk path URL (tanpa dependensi tambahan).
fn urlencoding_lite(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn ok_result(tool: &dyn Tool, content: String) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        name: tool.name().to_string(),
        content,
        is_error: false,
    }
}
