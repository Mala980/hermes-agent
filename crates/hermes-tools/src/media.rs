//! Tools media: generate gambar, text-to-speech, dan transkripsi audio.
//!
//! Menggunakan API yang kompatibel OpenAI (endpoint `/images/generations`,
//! `/audio/speech`, `/audio/transcriptions`). Konfigurasi via env:
//! `MEDIA_API_URL` (default `https://api.openai.com/v1`),
//! `MEDIA_API_KEY` (atau `OPENAI_API_KEY`).
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{HermesError, Result, RiskLevel, Tool, ToolContext, ToolEffect, ToolResult};
use std::path::PathBuf;

fn api_config() -> Result<(String, String)> {
    let base =
        std::env::var("MEDIA_API_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let key = std::env::var("MEDIA_API_KEY")
        .or_else(|_| std::env::var("OPENAI_API_KEY"))
        .map_err(|_| HermesError::tool("media", "set MEDIA_API_KEY atau OPENAI_API_KEY"))?;
    Ok((base.trim_end_matches('/').to_string(), key))
}

fn output_path(ctx: &ToolContext, name: &str) -> Result<PathBuf> {
    let p = ctx.resolve_path(name)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| HermesError::tool("media", format!("gagal buat direktori: {e}")))?;
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// image_generate
// ---------------------------------------------------------------------------

pub struct ImageGenerateTool;

#[async_trait]
impl Tool for ImageGenerateTool {
    fn name(&self) -> &str {
        "image_generate"
    }

    fn description(&self) -> &str {
        "Generate gambar dari deskripsi teks (API kompatibel OpenAI). Mengembalikan path file gambar yang disimpan."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("prompt", string_prop("Deskripsi gambar yang diinginkan.")),
                (
                    "output",
                    string_prop(
                        "Nama file output relatif ke working dir (default: image-<timestamp>.png).",
                    ),
                ),
                ("model", string_prop("Model gambar (default: dall-e-3).")),
                (
                    "size",
                    string_prop("Ukuran gambar, mis. 1024x1024 (default)."),
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
        let model = opt_str(&args, "model").unwrap_or("dall-e-3");
        let size = opt_str(&args, "size").unwrap_or("1024x1024");
        let (base, key) = api_config()?;
        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/images/generations"))
            .bearer_auth(&key)
            .json(&serde_json::json!({ "model": model, "prompt": prompt, "size": size }))
            .send()
            .await
            .map_err(|e| HermesError::tool("image_generate", format!("request gagal: {e}")))?;
        if !resp.status().is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(HermesError::tool(
                "image_generate",
                format!("API menolak: {b}"),
            ));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| HermesError::tool("image_generate", format!("parse gagal: {e}")))?;
        let data = v
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| HermesError::tool("image_generate", "respons tanpa data gambar"))?;

        let saved = if let Some(b64) = data.get("b64_json").and_then(|b| b.as_str()) {
            let bytes = base64_decode(b64)?;
            let name = opt_str(&args, "output")
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("image-{}.png", chrono_now()));
            let path = output_path(ctx, &name)?;
            std::fs::write(&path, &bytes).map_err(|e| {
                HermesError::tool("image_generate", format!("tulis file gagal: {e}"))
            })?;
            path.display().to_string()
        } else if let Some(url) = data.get("url").and_then(|u| u.as_str()) {
            // Unduh URL gambar ke file lokal.
            let bytes = client
                .get(url)
                .send()
                .await
                .map_err(|e| HermesError::tool("image_generate", format!("unduh gagal: {e}")))?
                .bytes()
                .await
                .map_err(|e| {
                    HermesError::tool("image_generate", format!("baca body gagal: {e}"))
                })?;
            let name = opt_str(&args, "output")
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("image-{}.png", chrono_now()));
            let path = output_path(ctx, &name)?;
            std::fs::write(&path, &bytes).map_err(|e| {
                HermesError::tool("image_generate", format!("tulis file gagal: {e}"))
            })?;
            path.display().to_string()
        } else {
            return Err(HermesError::tool(
                "image_generate",
                "respons tanpa url/b64_json",
            ));
        };
        Ok(success(self, format!("Gambar disimpan di: {saved}")))
    }
}

// ---------------------------------------------------------------------------
// text_to_speech
// ---------------------------------------------------------------------------

pub struct TextToSpeechTool;

#[async_trait]
impl Tool for TextToSpeechTool {
    fn name(&self) -> &str {
        "text_to_speech"
    }

    fn description(&self) -> &str {
        "Ubah teks menjadi audio (TTS, API kompatibel OpenAI). Mengembalikan path file audio."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                ("text", string_prop("Teks yang akan dibacakan.")),
                (
                    "output",
                    string_prop("Nama file output (default: speech-<timestamp>.mp3)."),
                ),
                ("model", string_prop("Model TTS (default: tts-1).")),
                (
                    "voice",
                    string_prop("Suara: alloy, echo, fable, onyx, nova, shimmer (default: alloy)."),
                ),
                (
                    "format",
                    string_prop("Format audio: mp3, opus, wav (default: mp3)."),
                ),
            ],
            vec!["text"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Write
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let text = get_str(&args, "text")?;
        let model = opt_str(&args, "model").unwrap_or("tts-1");
        let voice = opt_str(&args, "voice").unwrap_or("alloy");
        let format = opt_str(&args, "format").unwrap_or("mp3");
        let bytes = tts_bytes(text, model, voice, format).await?;
        let name = opt_str(&args, "output")
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("speech-{}.{format}", chrono_now()));
        let path = output_path(ctx, &name)?;
        std::fs::write(&path, &bytes)
            .map_err(|e| HermesError::tool("text_to_speech", format!("tulis file gagal: {e}")))?;
        Ok(success(
            self,
            format!(
                "Audio disimpan di: {} ({} bytes)",
                path.display(),
                bytes.len()
            ),
        ))
    }
}

/// Panggil API TTS dan kembalikan bytes audio. Dipakai tool + voice mode + gateway.
pub async fn tts_bytes(text: &str, model: &str, voice: &str, format: &str) -> Result<Vec<u8>> {
    let (base, key) = api_config()?;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/audio/speech"))
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "model": model, "input": text, "voice": voice, "response_format": format,
        }))
        .send()
        .await
        .map_err(|e| HermesError::tool("text_to_speech", format!("request gagal: {e}")))?;
    if !resp.status().is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(HermesError::tool(
            "text_to_speech",
            format!("API menolak: {b}"),
        ));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| HermesError::tool("text_to_speech", format!("baca audio gagal: {e}")))
}

// ---------------------------------------------------------------------------
// transcribe_audio
// ---------------------------------------------------------------------------

pub struct TranscribeAudioTool;

#[async_trait]
impl Tool for TranscribeAudioTool {
    fn name(&self) -> &str {
        "transcribe_audio"
    }

    fn description(&self) -> &str {
        "Transkripsi file audio menjadi teks (Whisper, API kompatibel OpenAI)."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "path",
                    string_prop("Path file audio (relatif ke working dir atau absolut)."),
                ),
                (
                    "model",
                    string_prop("Model transkripsi (default: whisper-1)."),
                ),
                (
                    "language",
                    string_prop("Kode bahasa opsional, mis. id, en."),
                ),
            ],
            vec!["path"],
        )
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Low
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::Read
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<ToolResult> {
        let path = ctx.resolve_path(get_str(&args, "path")?)?;
        let model = opt_str(&args, "model").unwrap_or("whisper-1");
        let language = opt_str(&args, "language");
        let bytes = std::fs::read(&path)
            .map_err(|e| HermesError::tool("transcribe_audio", format!("baca file gagal: {e}")))?;
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("audio.bin")
            .to_string();
        let text = transcribe_bytes(&bytes, &filename, model, language).await?;
        Ok(success(self, format!("Transkripsi:\n{text}")))
    }
}

/// Transkripsi bytes audio menjadi teks (Whisper / kompatibel OpenAI).
pub async fn transcribe_bytes(
    bytes: &[u8],
    filename: &str,
    model: &str,
    language: Option<&str>,
) -> Result<String> {
    let (base, key) = api_config()?;
    let part = reqwest::multipart::Part::bytes(bytes.to_vec()).file_name(filename.to_string());
    let mut form = reqwest::multipart::Form::new()
        .text("model", model.to_string())
        .part("file", part);
    if let Some(lang) = language {
        form = form.text("language", lang.to_string());
    }
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{base}/audio/transcriptions"))
        .bearer_auth(&key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| HermesError::tool("transcribe_audio", format!("request gagal: {e}")))?;
    if !resp.status().is_success() {
        let b = resp.text().await.unwrap_or_default();
        return Err(HermesError::tool(
            "transcribe_audio",
            format!("API menolak: {b}"),
        ));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| HermesError::tool("transcribe_audio", format!("parse gagal: {e}")))?;
    Ok(v.get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn success(tool: &dyn Tool, content: String) -> ToolResult {
    ToolResult {
        tool_call_id: String::new(),
        name: tool.name().to_string(),
        content,
        is_error: false,
    }
}

fn chrono_now() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn base64_decode(s: &str) -> Result<Vec<u8>> {
    // Dekoder base64 minimal tanpa dependensi tambahan.
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = s.bytes().filter(|&b| b != b'=').collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n: u32 = 0;
        let mut count = 0;
        for &c in chunk {
            n = (n << 6)
                | val(c).ok_or_else(|| HermesError::tool("media", "base64 tidak valid"))? as u32;
            count += 1;
        }
        n <<= 6 * (4 - count);
        out.push((n >> 16) as u8);
        if count > 2 {
            out.push((n >> 8) as u8);
        }
        if count > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}
