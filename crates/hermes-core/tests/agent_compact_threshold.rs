//! Uji integrasi (B7 + W1-B7): threshold kompresi proporsional terhadap
//! context window model, bukan angka fixed 100K.
//!
//! Paritas dengan original:
//! - `compression.threshold = 0.50` (rasio; `hermes_cli/config_defaults.py`),
//! - lantai raise-only 0.75 untuk window < 512K (`_SMALL_CTX_*` di
//!   `agent/context_compressor.py`),
//! - lantai minimum 64K (`MINIMUM_CONTEXT_LENGTH`) + batas 85%
//!   (`_MIN_CTX_TRIGGER_RATIO`),
//! - tabel per-model `context_window_for_model` (paritas katalog keluarga
//!   model hardcoded `agent/model_metadata.py`): claude-*/gpt-*/gemini-*/
//!   deepseek-*/atmo/*, model tak dikenal -> default 128K,
//! - pemicu: `preflight_tokens >= threshold_tokens`
//!   (`agent/turn_context_compaction.py`).

use hermes_core::{HermesConfig, ProfileConfig};

fn profile() -> ProfileConfig {
    ProfileConfig {
        max_tokens: Some(0),
        ..ProfileConfig::default()
    }
}

fn profile_for_model(model: &str) -> ProfileConfig {
    ProfileConfig {
        model: model.to_string(),
        max_tokens: Some(0),
        ..ProfileConfig::default()
    }
}

#[test]
fn default_trigger_uses_model_table_with_128k_fallback() {
    // Model default ("nousresearch/hermes-4-70b") tak dikenal tabel ->
    // window 128K; 128K < 512K -> rasio naik ke 0.75;
    // (128000 - 8192) * 0.75 — BUKAN fixed 100_000 seperti sebelumnya.
    let trigger = ProfileConfig::default().compaction_trigger_tokens();
    assert_eq!(trigger, 89_856);
    assert_ne!(trigger, 100_000);
}

#[test]
fn context_window_table_covers_known_families() {
    // (model, window yang diharapkan)
    let cases = [
        ("anthropic/claude-sonnet-4-5", 200_000),
        ("claude-opus-4-1", 200_000),
        ("openai/gpt-4o", 128_000),
        ("gpt-5-mini", 128_000),
        ("gemini-2.5-flash", 1_000_000),
        ("google/gemini-2.0-flash-001", 1_000_000),
        ("deepseek/deepseek-chat", 128_000),
        // Namespace atmo/* -> 128K (model AtmoRouter).
        ("atmo/deepseek-v4.1-flash", 128_000),
        ("atmo/llama-3.3-70b", 128_000),
        // Tak dikenal -> default 128K.
        ("nousresearch/hermes-4-70b", 128_000),
        ("some-future-model-xyz", 128_000),
    ];
    for (model, window) in cases {
        assert_eq!(
            ProfileConfig::context_window_for_model(model),
            window,
            "window salah untuk model {model}"
        );
    }
}

#[test]
fn trigger_scales_with_model_family_window() {
    // claude-* -> 200K < 512K -> 0.75; gemini-* -> 1M -> 0.50.
    let t_claude = profile_for_model("anthropic/claude-sonnet-4-5").compaction_trigger_tokens();
    let t_gemini = profile_for_model("gemini-2.5-flash").compaction_trigger_tokens();
    assert_eq!(t_claude, 150_000);
    assert_eq!(t_gemini, 500_000);
    assert!(t_gemini > t_claude, "threshold harus berbeda antar window");
}

#[test]
fn atmo_and_unknown_models_use_128k_default_window() {
    for model in ["atmo/deepseek-v4.1-flash", "nousresearch/hermes-4-70b"] {
        // 128K < 512K -> lantai raise-only 0.75: 0.75 * 128000 = 96000.
        let t = profile_for_model(model).compaction_trigger_tokens();
        assert_eq!(t, 96_000, "model {model}");
    }
}

#[test]
fn explicit_context_window_overrides_model_table() {
    // Window eksplisit menang atas tabel per-model.
    let p = ProfileConfig {
        model: "anthropic/claude-sonnet-4-5".to_string(),
        context_window: Some(1_000_000),
        max_tokens: Some(0),
        ..ProfileConfig::default()
    };
    assert_eq!(p.compaction_trigger_tokens(), 500_000);
}

#[test]
fn trigger_scales_with_window() {
    let t256 = ProfileConfig {
        context_window: Some(256_000),
        ..profile()
    }
    .compaction_trigger_tokens();
    let t1m = ProfileConfig {
        context_window: Some(1_000_000),
        ..profile()
    }
    .compaction_trigger_tokens();
    // 256K < 512K -> lantai 0.75; 1M -> rasio 0.50
    assert_eq!(t256, 192_000);
    assert_eq!(t1m, 500_000);
    assert!(t1m > t256, "threshold harus berbeda antar window");
}

#[test]
fn small_window_gets_raise_only_floor() {
    let p = ProfileConfig {
        context_window: Some(200_000),
        ..profile()
    };
    // 200K < 512K -> 0.75 * 200000
    assert_eq!(p.compaction_trigger_tokens(), 150_000);
}

#[test]
fn tiny_window_uses_minimum_length_floor_with_cap() {
    let p = ProfileConfig {
        context_window: Some(64_000),
        ..profile()
    };
    // 0.75*64000=48000 < lantai 64K; lantai mengikat -> dibatasi 85% window.
    assert_eq!(p.compaction_trigger_tokens(), 54_400);
}

#[test]
fn ratio_is_configurable() {
    let p = ProfileConfig {
        context_window: Some(1_000_000),
        compact_threshold_ratio: 0.25,
        ..profile()
    };
    assert_eq!(p.compaction_trigger_tokens(), 250_000);
}

#[test]
fn max_tokens_is_subtracted_from_window() {
    let p = ProfileConfig {
        context_window: Some(1_000_000),
        max_tokens: Some(8_192),
        ..profile()
    };
    // (1000000 - 8192) * 0.50
    assert_eq!(p.compaction_trigger_tokens(), 495_904);
}

#[test]
fn explicit_absolute_threshold_wins() {
    // Config lama yang menyetel angka fixed eksplisit tetap dihormati apa adanya.
    let p = ProfileConfig {
        compact_threshold: 100_000,
        context_window: Some(1_000_000),
        ..profile()
    };
    assert_eq!(p.compaction_trigger_tokens(), 100_000);
}

// ---- Batch 4 / W3-P6: ceiling max_tokens per-model ----

#[test]
fn max_output_ceiling_table_covers_known_families() {
    // (model, ceiling yang diharapkan)
    let cases = [
        ("anthropic/claude-sonnet-4-5", 64_000),
        ("claude-opus-4-1", 64_000),
        ("openai/gpt-4o", 128_000),
        ("gpt-4.1-mini", 128_000),
        ("gemini-2.5-flash", 65_536),
        ("google/gemini-2.0-flash-001", 65_536),
        ("deepseek/deepseek-chat", 64_000),
        ("atmo/deepseek-v4.1-flash", 64_000),
        // Tak dikenal -> lantai 8192 (perilaku lama).
        ("nousresearch/hermes-4-70b", 8_192),
        ("some-future-model-xyz", 8_192),
    ];
    for (model, ceiling) in cases {
        assert_eq!(
            ProfileConfig::max_output_ceiling_for_model(model),
            ceiling,
            "ceiling salah untuk model {model}"
        );
    }
}

#[test]
fn unset_max_tokens_uses_model_ceiling() {
    // Config tidak menyetel max_tokens -> ceiling per-model dipakai,
    // bukan lagi 8192 untuk semua model.
    let p = ProfileConfig {
        model: "anthropic/claude-sonnet-4-5".to_string(),
        max_tokens: None,
        ..ProfileConfig::default()
    };
    assert_eq!(
        p.effective_max_tokens("anthropic/claude-sonnet-4-5"),
        64_000
    );

    let p = ProfileConfig {
        model: "gpt-4o".to_string(),
        max_tokens: None,
        ..ProfileConfig::default()
    };
    assert_eq!(p.effective_max_tokens("gpt-4o"), 128_000);
}

#[test]
fn explicit_max_tokens_wins_but_is_capped_by_ceiling() {
    // Eksplisit di bawah ceiling -> dipakai apa adanya.
    let p = ProfileConfig {
        model: "anthropic/claude-sonnet-4-5".to_string(),
        max_tokens: Some(4_000),
        ..ProfileConfig::default()
    };
    assert_eq!(p.effective_max_tokens("claude-sonnet-4-5"), 4_000);
    // Eksplisit di atas ceiling -> dibatasi ceiling (min(config, ceiling)).
    let p = ProfileConfig {
        model: "anthropic/claude-sonnet-4-5".to_string(),
        max_tokens: Some(200_000),
        ..ProfileConfig::default()
    };
    assert_eq!(p.effective_max_tokens("claude-sonnet-4-5"), 64_000);
}

#[test]
fn effective_max_tokens_uses_turn_model_not_profile_model() {
    // Turn model (fallback provider) yang dipakai untuk lookup ceiling.
    let p = ProfileConfig {
        model: String::new(),
        max_tokens: None,
        ..ProfileConfig::default()
    };
    assert_eq!(p.effective_max_tokens("gemini-2.5-flash"), 65_536);
    assert_eq!(p.effective_max_tokens("unknown-thing"), 8_192);
}

#[test]
fn nous_default_base_url_uses_inference_api_host() {
    // Paritas original: `_NOUS_DEFAULT_BASE_URL` =
    // "https://inference-api.nousresearch.com/v1".
    let providers = HermesConfig::default_providers();
    let nous = providers
        .iter()
        .find(|p| p.name == "nous")
        .expect("provider nous");
    assert_eq!(nous.base_url, "https://inference-api.nousresearch.com/v1");
}
