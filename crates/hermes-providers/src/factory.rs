use crate::{AnthropicProvider, BedrockProvider, GeminiProvider, OpenAiCompatProvider};
use hermes_core::{HermesError, LlmProvider, ProviderEntry, Result};
use std::sync::Arc;

/// Nama env var `<NAME>_BASE_URL` dari nama provider — paritas pola
/// `base_url_env_var` di original (mis. `OPENROUTER_BASE_URL`,
/// `OPENAI_BASE_URL`, `ANTHROPIC_BASE_URL`, `GEMINI_BASE_URL`).
fn base_url_env_var_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.push_str("_BASE_URL");
    out
}

/// Base URL bawaan yang dikapalkan untuk (protocol, name): dipakai untuk
/// membedakan base_url eksplisit di konfigurasi dari nilai default.
fn shipped_default_base(protocol: &str, name: &str) -> Option<&'static str> {
    match (protocol, name) {
        ("openai", "openrouter") => Some("https://openrouter.ai/api/v1"),
        ("openai", "nous") => Some("https://inference-api.nousresearch.com/v1"),
        ("openai", "openai") => Some("https://api.openai.com/v1"),
        ("openai", "ollama") => Some("http://localhost:11434/v1"),
        ("anthropic", "anthropic") => Some("https://api.anthropic.com"),
        _ => None,
    }
}

/// Paritas `<NAME>_BASE_URL` per provider di original: env menimpa base_url
/// bawaan profil; base_url eksplisit di konfigurasi menang atas env.
/// Protokol `gemini` ditangani di `GeminiProvider::with_base_url` (butuh
/// normalisasi segmen versi), `azure` sudah punya jalurnya sendiri
/// (`AZURE_FOUNDRY_BASE_URL`), `bedrock` berbasis region.
fn resolve_base_url(entry: &ProviderEntry) -> String {
    let var: Option<String> = match entry.protocol.as_str() {
        "anthropic" => Some("ANTHROPIC_BASE_URL".to_string()),
        "openai" => Some(base_url_env_var_name(&entry.name)),
        _ => None,
    };
    if let Some(var) = var {
        if let Ok(v) = std::env::var(&var) {
            let v = v.trim().trim_end_matches('/').to_string();
            if !v.is_empty() {
                let current = entry.base_url.trim().trim_end_matches('/');
                let is_default = current.is_empty()
                    || shipped_default_base(&entry.protocol, &entry.name)
                        .map(|d| d == current)
                        .unwrap_or(false);
                if is_default {
                    return v;
                }
            }
        }
    }
    entry.base_url.clone()
}

/// Bangun provider dari entri konfigurasi, meniru `agent_init.py` Hermes asli.
pub fn build_provider(entry: &ProviderEntry) -> Result<Arc<dyn LlmProvider>> {
    let keys = OpenAiCompatProvider::collect_keys(entry.api_key_env.as_deref(), &entry.api_keys);
    let default_model = entry
        .default_model
        .clone()
        .unwrap_or_else(|| "nousresearch/hermes-4-70b".to_string());
    match entry.protocol.as_str() {
        "anthropic" => Ok(Arc::new(AnthropicProvider::new(
            resolve_base_url(entry),
            default_model,
            AnthropicProvider::collect_keys(&entry.api_keys),
        ))),
        "openai" => Ok(Arc::new(OpenAiCompatProvider::new(
            entry.name.clone(),
            resolve_base_url(entry),
            default_model,
            keys,
        ))),
        "gemini" => Ok(Arc::new(GeminiProvider::with_base_url(
            entry
                .default_model
                .clone()
                // Paritas default model native client original
                // (`gemini-3.7-flash` di agent/gemini_native_adapter.py).
                .unwrap_or_else(|| "gemini-3.7-flash".to_string()),
            GeminiProvider::collect_keys(),
            Some(entry.base_url.clone()),
        ))),
        "azure" => {
            // P17: bila base_url adalah URL penuh (diawali "http") → mode Azure
            // Foundry: dipakai verbatim, tanpa penambahan path/api-version.
            // Kalau tidak → mode legacy, bangun URL deployment dari
            // "<resource>|<deployment>|<api-version>".
            let foundry_base = if entry.base_url.trim_start().starts_with("http") {
                entry.base_url.trim().to_string()
            } else {
                std::env::var("AZURE_FOUNDRY_BASE_URL").unwrap_or_default()
            };
            if foundry_base.trim_start().starts_with("http") {
                let azure_keys = OpenAiCompatProvider::collect_keys(
                    Some("AZURE_FOUNDRY_API_KEY"),
                    &entry.api_keys,
                );
                Ok(Arc::new(OpenAiCompatProvider::new_azure_foundry(
                    foundry_base,
                    azure_keys,
                )))
            } else {
                let (resource, deployment, api_version) = parse_azure_base(&entry.base_url)?;
                let azure_keys = OpenAiCompatProvider::collect_keys(
                    Some("AZURE_OPENAI_API_KEY"),
                    &entry.api_keys,
                );
                Ok(Arc::new(OpenAiCompatProvider::new_azure(
                    resource,
                    deployment,
                    api_version,
                    azure_keys,
                )))
            }
        }
        "bedrock" => Ok(Arc::new(BedrockProvider::from_env(
            entry
                .default_model
                .clone()
                .unwrap_or_else(|| "anthropic.claude-3-5-sonnet-20241022-v2:0".to_string()),
        )?)),
        other => Err(HermesError::Config(format!(
            "protokol provider tidak dikenal: {other}"
        ))),
    }
}

/// Parse base_url Azure: "resource|deployment|api-version".
fn parse_azure_base(base: &str) -> Result<(String, String, String)> {
    let parts: Vec<&str> = base.split('|').collect();
    if parts.len() == 3 {
        return Ok((
            parts[0].to_string(),
            parts[1].to_string(),
            parts[2].to_string(),
        ));
    }
    // Atau dari env: AZURE_OPENAI_RESOURCE, AZURE_OPENAI_DEPLOYMENT, AZURE_OPENAI_API_VERSION.
    let resource = std::env::var("AZURE_OPENAI_RESOURCE").map_err(|_| {
        HermesError::Config(
            "azure butuh base_url 'resource|deployment|api-version' atau env AZURE_OPENAI_RESOURCE"
                .into(),
        )
    })?;
    let deployment = std::env::var("AZURE_OPENAI_DEPLOYMENT").unwrap_or_else(|_| "gpt-4o".into());
    let api_version =
        std::env::var("AZURE_OPENAI_API_VERSION").unwrap_or_else(|_| "2024-10-21".into());
    Ok((resource, deployment, api_version))
}

/// Provider default: OpenRouter bila ada key, fallback ke Ollama lokal.
pub fn default_provider() -> Arc<dyn LlmProvider> {
    for entry in hermes_core::HermesConfig::default_providers() {
        if let Ok(p) = build_provider(&entry) {
            // Pilih yang punya key (atau ollama tanpa key).
            if entry.name == "ollama"
                || (entry.protocol == "anthropic" && AnthropicProvider::has_env_credential())
                || std::env::var(entry.api_key_env.unwrap_or_default()).is_ok()
            {
                return p;
            }
        }
    }
    // Fallback terakhir: ollama tanpa key.
    Arc::new(OpenAiCompatProvider::new(
        "ollama",
        "http://localhost:11434/v1",
        "llama3.1",
        vec![],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::ProviderEntry;

    fn azure_entry(base_url: &str) -> ProviderEntry {
        ProviderEntry {
            name: "azure".to_string(),
            protocol: "azure".to_string(),
            base_url: base_url.to_string(),
            api_key_env: None,
            api_keys: vec![],
            default_model: None,
        }
    }

    #[test]
    fn azure_full_base_url_routes_to_foundry() {
        let p = build_provider(&azure_entry(
            "https://myres.services.ai.azure.com/openai/v1/chat/completions",
        ))
        .expect("factory harus menerima URL penuh");
        assert_eq!(p.name(), "azure-foundry");
    }

    #[test]
    fn azure_resource_fragment_routes_to_legacy() {
        let p = build_provider(&azure_entry("myres|mydep|2024-10-21"))
            .expect("factory harus menerima format resource|deployment|api-version");
        assert_eq!(p.name(), "azure");
    }

    /// Env bersifat global per proses; serialkan tes yang menyentuhnya.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn openai_entry(name: &str, base_url: &str) -> ProviderEntry {
        ProviderEntry {
            name: name.to_string(),
            protocol: "openai".to_string(),
            base_url: base_url.to_string(),
            api_key_env: None,
            api_keys: vec![],
            default_model: None,
        }
    }

    #[test]
    fn base_url_env_var_name_matches_original_pattern() {
        assert_eq!(base_url_env_var_name("openrouter"), "OPENROUTER_BASE_URL");
        assert_eq!(base_url_env_var_name("openai"), "OPENAI_BASE_URL");
        assert_eq!(base_url_env_var_name("my-proxy"), "MY_PROXY_BASE_URL");
    }

    #[test]
    fn name_base_url_env_overrides_shipped_default() {
        // Paritas `<NAME>_BASE_URL`: env menimpa default bawaan.
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENROUTER_BASE_URL", "https://proxy.lokal/v1/");
        let entry = openai_entry("openrouter", "https://openrouter.ai/api/v1");
        assert_eq!(
            resolve_base_url(&entry),
            "https://proxy.lokal/v1",
            "env harus menimpa default bawaan"
        );
        std::env::remove_var("OPENROUTER_BASE_URL");
    }

    #[test]
    fn explicit_config_base_url_beats_env() {
        // Base URL eksplisit di konfigurasi menang atas env (paritas original).
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("OPENROUTER_BASE_URL", "https://proxy.lokal/v1");
        let entry = openai_entry("openrouter", "https://khusus.example.com/v1");
        assert_eq!(
            resolve_base_url(&entry),
            "https://khusus.example.com/v1",
            "konfigurasi eksplisit harus menang atas env"
        );
        std::env::remove_var("OPENROUTER_BASE_URL");
    }

    #[test]
    fn anthropic_base_url_env_override() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("ANTHROPIC_BASE_URL", "https://proxy.lokal/anthropic");
        let entry = ProviderEntry {
            name: "anthropic".to_string(),
            protocol: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key_env: None,
            api_keys: vec![],
            default_model: None,
        };
        assert_eq!(resolve_base_url(&entry), "https://proxy.lokal/anthropic");
        std::env::remove_var("ANTHROPIC_BASE_URL");
    }
}
