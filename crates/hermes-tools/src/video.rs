//! Tools video: video_generate (fal.ai) dan video_analyze (Gemini).
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};

// ---------------------------------------------------------------------------
// video_generate — text-to-video via fal.ai queue API
// ---------------------------------------------------------------------------

pub struct VideoGenerateTool;

/// Base URL fal.ai queue API; bisa di-override via FAL_API_BASE untuk pengujian.
fn fal_api_base() -> String {
    std::env::var("FAL_API_BASE").unwrap_or_else(|_| "https://queue.fal.run".to_string())
}

/// Base URL Gemini API; bisa di-override via GEMINI_API_BASE untuk pengujian.
fn gemini_api_base() -> String {
    std::env::var("GEMINI_API_BASE")
        .unwrap_or_else(|_| "https://generativelanguage.googleapis.com".to_string())
}

#[async_trait]
impl Tool for VideoGenerateTool {
    fn name(&self) -> &str {
        "video_generate"
    }

    fn description(&self) -> &str {
        "Generate video dari deskripsi teks via fal.ai. Butuh env FAL_API_KEY. \
         Mengembalikan URL video hasil (dan menyimpannya bila diminta)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("prompt", string_prop("Deskripsi video yang diinginkan.")),
                (
                    "duration",
                    integer_prop(
                        "Durasi video yang diinginkan dalam detik. Provider memotong \
                         ke rentang yang didukung. Kosongkan untuk default provider.",
                    ),
                ),
                (
                    "aspect_ratio",
                    serde_json::json!({
                        "type": "string",
                        "enum": ["16:9", "9:16", "1:1", "4:3", "3:4", "3:2", "2:3", "21:9"],
                        "description": "Rasio aspek output (default: 16:9).",
                        "default": "16:9"
                    }),
                ),
                (
                    "resolution",
                    serde_json::json!({
                        "type": "string",
                        "enum": ["480p", "540p", "720p", "768p", "1080p"],
                        "description": "Resolusi output (default: 720p).",
                        "default": "720p"
                    }),
                ),
                ("model", string_prop("Model fal.ai (default: fal-ai/veo3).")),
                (
                    "output",
                    string_prop("Path lokal untuk menyimpan video (opsional)."),
                ),
                (
                    "timeout_secs",
                    integer_prop("Timeout polling detik (default 600)."),
                ),
            ],
            vec!["prompt"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let prompt = get_str(&args, "prompt")?;
        let model = opt_str(&args, "model").unwrap_or("fal-ai/veo3");
        let timeout = opt_u64(&args, "timeout_secs")
            .unwrap_or(600)
            .clamp(60, 1800);
        let key = std::env::var("FAL_API_KEY")
            .map_err(|_| HermesError::tool("video_generate", "set env FAL_API_KEY"))?;
        let client = reqwest::Client::new();

        // Batch 8: duration/aspect_ratio/resolution seperti skema aslinya;
        // dikirim ke fal.ai (provider memotong/mengabaikan yang tak didukung).
        let mut submit = serde_json::json!({
            "prompt": prompt,
            "aspect_ratio": opt_str(&args, "aspect_ratio").unwrap_or("16:9"),
            "resolution": opt_str(&args, "resolution").unwrap_or("720p"),
        });
        if let Some(d) = opt_u64(&args, "duration") {
            submit["duration"] = serde_json::json!(d);
        }

        // 1. Submit ke queue.
        let resp = client
            .post(format!("{}/{model}", fal_api_base()))
            .header("Authorization", format!("Key {key}"))
            .json(&submit)
            .send()
            .await
            .map_err(|e| HermesError::tool("video_generate", format!("submit gagal: {e}")))?;
        if !resp.status().is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(HermesError::tool(
                "video_generate",
                format!("fal.ai menolak: {b}"),
            ));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| HermesError::tool("video_generate", format!("parse gagal: {e}")))?;
        let status_url = v.get("status_url").and_then(|s| s.as_str()).unwrap_or("");
        let response_url = v.get("response_url").and_then(|s| s.as_str()).unwrap_or("");
        if status_url.is_empty() {
            return Err(HermesError::tool(
                "video_generate",
                "fal.ai tidak mengembalikan status_url",
            ));
        }

        // 2. Poll hingga selesai.
        let started = std::time::Instant::now();
        loop {
            if started.elapsed().as_secs() > timeout {
                return Err(HermesError::tool(
                    "video_generate",
                    "timeout menunggu hasil",
                ));
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let st: serde_json::Value = client
                .get(status_url)
                .header("Authorization", format!("Key {key}"))
                .send()
                .await
                .map_err(|e| HermesError::tool("video_generate", format!("poll gagal: {e}")))?
                .json()
                .await
                .map_err(|e| {
                    HermesError::tool("video_generate", format!("parse status gagal: {e}"))
                })?;
            match st.get("status").and_then(|s| s.as_str()) {
                Some("COMPLETED") => break,
                Some("FAILED") | Some("CANCELED") => {
                    return Err(HermesError::tool(
                        "video_generate",
                        format!("job gagal: {st}"),
                    ))
                }
                _ => continue,
            }
        }

        // 3. Ambil hasil.
        let out: serde_json::Value = client
            .get(response_url)
            .header("Authorization", format!("Key {key}"))
            .send()
            .await
            .map_err(|e| HermesError::tool("video_generate", format!("ambil hasil gagal: {e}")))?
            .json()
            .await
            .map_err(|e| HermesError::tool("video_generate", format!("parse hasil gagal: {e}")))?;
        let video_url = out
            .pointer("/video/url")
            .or_else(|| out.pointer("/video_url"))
            .and_then(|u| u.as_str())
            .ok_or_else(|| HermesError::tool("video_generate", format!("hasil tanpa URL: {out}")))?
            .to_string();

        let mut content = format!("Video berhasil dibuat: {video_url}");
        if let Some(dest) = opt_str(&args, "output") {
            let bytes = client
                .get(&video_url)
                .send()
                .await
                .map_err(|e| HermesError::tool("video_generate", format!("unduh gagal: {e}")))?
                .bytes()
                .await
                .map_err(|e| {
                    HermesError::tool("video_generate", format!("baca body gagal: {e}"))
                })?;
            let path = ctx.resolve_path(dest)?;
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    HermesError::tool("video_generate", format!("mkdir gagal: {e}"))
                })?;
            }
            std::fs::write(&path, &bytes)
                .map_err(|e| HermesError::tool("video_generate", format!("tulis gagal: {e}")))?;
            content.push_str(&format!("\nDisimpan di: {}", path.display()));
        }
        Ok(ok_result(self, content))
    }
}

// ---------------------------------------------------------------------------
// video_analyze — analisis video via Gemini (inlineData, maks ~18MB)
// ---------------------------------------------------------------------------

pub struct VideoAnalyzeTool;

#[async_trait]
impl Tool for VideoAnalyzeTool {
    fn name(&self) -> &str {
        "video_analyze"
    }

    fn description(&self) -> &str {
        "Analisis isi video (file lokal atau URL) dengan Gemini vision. \
         Butuh env GEMINI_API_KEY. Batas ukuran ~18MB (inline)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "source",
                    string_prop("Path file video lokal atau URL http(s)."),
                ),
                (
                    "prompt",
                    string_prop("Pertanyaan/instruksi analisis (default: deskripsikan isi video)."),
                ),
                (
                    "model",
                    string_prop("Model Gemini (default: gemini-2.0-flash)."),
                ),
            ],
            vec!["source"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let source = get_str(&args, "source")?;
        let prompt =
            opt_str(&args, "prompt").unwrap_or("Deskripsikan isi video ini secara detail.");
        let model = opt_str(&args, "model").unwrap_or("gemini-2.0-flash");
        let key = std::env::var("GEMINI_API_KEY")
            .map_err(|_| HermesError::tool("video_analyze", "set env GEMINI_API_KEY"))?;

        // Ambil bytes video (maks 18MB).
        const MAX: usize = 18 * 1024 * 1024;
        let bytes: Vec<u8> = if source.starts_with("http://") || source.starts_with("https://") {
            let client = reqwest::Client::new();
            let resp = client
                .get(source)
                .send()
                .await
                .map_err(|e| HermesError::tool("video_analyze", format!("unduh gagal: {e}")))?;
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| HermesError::tool("video_analyze", format!("baca body gagal: {e}")))?;
            bytes.to_vec()
        } else {
            let path = ctx.resolve_path(source)?;
            std::fs::read(&path)
                .map_err(|e| HermesError::tool("video_analyze", format!("baca file gagal: {e}")))?
        };
        if bytes.len() > MAX {
            return Err(HermesError::tool(
                "video_analyze",
                format!(
                    "video {}MB melebihi batas inline 18MB",
                    bytes.len() / 1024 / 1024
                ),
            ));
        }
        let mime = mime_for(source);
        let b64 = base64_encode(&bytes);

        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "{}/v1beta/models/{model}:generateContent",
                gemini_api_base()
            ))
            .query(&[("key", key.as_str())])
            .json(&serde_json::json!({
                "contents": [{
                    "parts": [
                        {"text": prompt},
                        {"inlineData": {"mimeType": mime, "data": b64}},
                    ]
                }]
            }))
            .send()
            .await
            .map_err(|e| HermesError::tool("video_analyze", format!("request gagal: {e}")))?;
        if !resp.status().is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(HermesError::tool(
                "video_analyze",
                format!("Gemini menolak: {b}"),
            ));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| HermesError::tool("video_analyze", format!("parse gagal: {e}")))?;
        let text = v
            .pointer("/candidates/0/content/parts")
            .and_then(|p| p.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        Ok(ok_result(self, format!("Analisis video:\n{text}")))
    }
}

fn mime_for(source: &str) -> &'static str {
    let s = source.to_lowercase();
    if s.ends_with(".webm") {
        "video/webm"
    } else if s.ends_with(".mov") {
        "video/quicktime"
    } else if s.ends_with(".avi") {
        "video/x-msvideo"
    } else {
        "video/mp4"
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len() * 4 / 3 + 4);
    for chunk in bytes.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        out.push(ALPH[((n >> 18) & 63) as usize] as char);
        out.push(ALPH[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPH[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPH[(n & 63) as usize] as char
        } else {
            '='
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Batch 8: video_generate menyatakan duration/aspect_ratio/resolution
    /// seperti skema aslinya.
    #[test]
    fn video_generate_schema_params() {
        let s = VideoGenerateTool.parameters_schema().to_string();
        for p in ["duration", "aspect_ratio", "resolution"] {
            assert!(s.contains(p), "param hilang: {p} — skema: {s}");
        }
        // Enum + default sesuai video_gen_provider.py aslinya.
        assert!(s.contains("\"16:9\""), "enum aspect_ratio: {s}");
        assert!(s.contains("\"720p\""), "enum resolusi: {s}");
        assert!(s.contains("16:9"), "default aspect_ratio: {s}");
        assert!(s.contains("720p"), "default resolusi: {s}");
        assert!(s.contains("detik"), "deskripsi duration: {s}");
    }
}
