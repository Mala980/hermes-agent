//! Tool `vision_analyze`: analisis gambar dengan model vision via provider aktif.
use crate::schema::*;
use async_trait::async_trait;
use hermes_core::{
    with_retries, ChatRequest, ContentBlock, HermesError, Message, Result, RiskLevel, Role, Tool,
    ToolContext, ToolEffect, ToolResult,
};

pub struct VisionAnalyzeTool;

#[async_trait]
impl Tool for VisionAnalyzeTool {
    fn name(&self) -> &str {
        "vision_analyze"
    }

    fn description(&self) -> &str {
        "Analisis gambar (file lokal atau URL) menggunakan model vision dari provider aktif. \
         Model harus mendukung input gambar."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        object_schema(
            vec![
                (
                    "source",
                    string_prop("Path file gambar lokal atau URL http(s)."),
                ),
                (
                    "prompt",
                    string_prop("Pertanyaan/instruksi analisis (default: deskripsikan gambar)."),
                ),
                (
                    "model",
                    string_prop("Model vision (default: model aktif sesi)."),
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
        let prompt = opt_str(&args, "prompt").unwrap_or("Deskripsikan gambar ini secara detail.");
        let provider = ctx.provider.clone().ok_or_else(|| {
            HermesError::tool("vision_analyze", "tidak ada provider LLM di konteks")
        })?;
        let model = opt_str(&args, "model").unwrap_or(&ctx.model).to_string();

        let data_url = if source.starts_with("http://") || source.starts_with("https://") {
            source.to_string()
        } else {
            let path = ctx.resolve_path(source)?;
            let bytes = std::fs::read(&path).map_err(|e| {
                HermesError::tool("vision_analyze", format!("baca file gagal: {e}"))
            })?;
            if bytes.len() > 20 * 1024 * 1024 {
                return Err(HermesError::tool("vision_analyze", "gambar melebihi 20MB"));
            }
            format!("data:{};base64,{}", mime_for(&path), base64_encode(&bytes))
        };

        let req = ChatRequest {
            model,
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text {
                        text: prompt.to_string(),
                    },
                    ContentBlock::Image { data_url },
                ],
                name: None,
            }],
            tools: vec![],
            max_tokens: Some(1024),
            temperature: None,
            reasoning_effort: None,
        };
        // Retry dengan semantik yang sama seperti turn path agen
        // (retry hanya untuk 429/5xx/gangguan jaringan).
        let (text, _usage) = with_retries(|| provider.chat_simple(req.clone()))
            .await
            .map_err(|e| HermesError::tool("vision_analyze", format!("provider gagal: {e}")))?;
        Ok(ToolResult {
            tool_call_id: String::new(),
            name: self.name().to_string(),
            content: format!("Analisis gambar:\n{text}"),
            is_error: false,
        })
    }
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
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
