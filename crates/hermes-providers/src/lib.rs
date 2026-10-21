//! hermes-providers: provider LLM untuk Hermes.
//!
//! - `openai`: klien OpenAI-compatible (OpenAI, OpenRouter, Nous Portal, Ollama, vLLM, ...)
//! - `anthropic`: klien Anthropic Messages API
//! - `gemini`: klien Gemini native (Google AI Studio)
//! - `bedrock`: klien AWS Bedrock (Converse API + SigV4)
//! - Azure OpenAI: via `OpenAiCompatProvider::new_azure`
//! - `credential_pool`: rotasi API key + cooldown otomatis

pub mod anthropic;
pub mod bedrock;
pub mod credential_pool;
pub mod factory;
pub mod gemini;
pub mod openai;

pub use anthropic::AnthropicProvider;
pub use bedrock::BedrockProvider;
pub use credential_pool::CredentialPool;
pub use factory::build_provider;
pub use gemini::GeminiProvider;
pub use openai::OpenAiCompatProvider;

use hermes_core::HermesError;

/// Bangun error HTTP terstruktur dari respons reqwest yang gagal, lengkap
/// dengan parsing header `Retry-After` (dipakai retry loop + klasifikasi).
pub async fn provider_http_error(resp: reqwest::Response) -> HermesError {
    let status = resp.status();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(hermes_core::parse_retry_after);
    let text = resp.text().await.unwrap_or_default();
    let short: String = text.chars().take(500).collect();
    HermesError::http(status.as_u16(), retry_after, short)
}
