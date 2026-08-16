//! Token usage parsing and cost estimation.
//!
//! Provides utilities for:
//! - Parsing provider usage data from normalized runtime events
//! - Estimating costs based on model pricing
//! - Extracting usage from kickoff report JSON

use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

use crate::agents::{AgentProvider, RuntimeUsage};

/// Raw token usage data as reported by the Claude API.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct RawTokenUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    #[serde(default)]
    pub cache_read_input_tokens: Option<i64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<i64>,
}

/// A parsed token usage record ready for database insertion.
#[derive(Debug, Clone)]
pub struct ParsedUsage {
    pub agent_id: String,
    pub session_id: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub provider: AgentProvider,
    pub cached_input_tokens: Option<i64>,
    pub reasoning_output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub model: String,
    pub cost_estimate: Option<f64>,
    pub provider_metadata_json: Option<String>,
}

/// Model pricing per million tokens (input, output, cache read, cache creation).
/// Based on publicly available Anthropic pricing as of 2025.
///
/// `Default` yields all-zero pricing; `Deserialize` tolerates missing cache
/// fields (they default to `0.0`).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ModelPricing {
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_creation: f64,
}

/// Provider-agnostic pricing configuration.
///
/// Resolution for a given model name proceeds in this order:
/// 1. exact match in `models`,
/// 2. longest prefix match in `providers`,
/// 3. the built-in Anthropic substring heuristics,
/// 4. `default`,
/// 5. `None`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PricingConfig {
    /// Exact model-name to pricing overrides.
    #[serde(default)]
    pub models: HashMap<String, ModelPricing>,
    /// Provider prefix (e.g. `"gpt-"`) to pricing.
    #[serde(default)]
    pub providers: HashMap<String, ModelPricing>,
    /// Fallback pricing applied when nothing else matches.
    #[serde(default)]
    pub default: Option<ModelPricing>,
}

/// Load the `pricing` section of `hook-config.json` from the given crosslink
/// directory. Returns a default (empty) config if the file is missing, invalid,
/// or lacks a `pricing` key.
#[must_use]
pub fn load_pricing_config(crosslink_dir: &Path) -> PricingConfig {
    let config_path = crosslink_dir.join("hook-config.json");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return PricingConfig::default();
    };
    let value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return PricingConfig::default(),
    };
    value
        .get("pricing")
        .map_or_else(PricingConfig::default, |pricing| {
            serde_json::from_value(pricing.clone()).unwrap_or_default()
        })
}

fn get_pricing(model: &str, cfg: &PricingConfig) -> Option<ModelPricing> {
    // 1) exact match in models
    if let Some(p) = cfg.models.get(model) {
        return Some(p.clone());
    }
    // 2) longest prefix match in providers
    let mut best_prefix_len: usize = 0;
    let mut best: Option<ModelPricing> = None;
    for (prefix, pricing) in &cfg.providers {
        if model.starts_with(prefix.as_str()) && prefix.len() > best_prefix_len {
            best_prefix_len = prefix.len();
            best = Some(pricing.clone());
        }
    }
    if let Some(p) = best {
        return Some(p);
    }
    // 3) existing Anthropic substring match (kept exactly as before)
    // Normalize model name for matching
    let m = model.to_lowercase();
    if m.contains("opus") {
        Some(ModelPricing {
            input: 15.0,
            output: 75.0,
            cache_read: 1.5,
            cache_creation: 18.75,
        })
    } else if m.contains("sonnet") {
        Some(ModelPricing {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_creation: 3.75,
        })
    } else if m.contains("haiku") {
        Some(ModelPricing {
            input: 0.80,
            output: 4.0,
            cache_read: 0.08,
            cache_creation: 1.0,
        })
    } else {
        // 4) default, 5) None
        cfg.default.clone()
    }
}

/// Estimate cost in USD for a token usage record using the given pricing config.
#[must_use]
pub fn estimate_cost_cfg(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    cfg: &PricingConfig,
) -> Option<f64> {
    let pricing = get_pricing(model, cfg)?;
    #[allow(clippy::cast_precision_loss)] // token counts are well within f64 mantissa range
    let input_cost = (input_tokens as f64 / 1_000_000.0) * pricing.input;
    #[allow(clippy::cast_precision_loss)]
    let output_cost = (output_tokens as f64 / 1_000_000.0) * pricing.output;
    #[allow(clippy::cast_precision_loss)]
    let cache_read_cost =
        (cache_read_tokens.unwrap_or(0) as f64 / 1_000_000.0) * pricing.cache_read;
    #[allow(clippy::cast_precision_loss)]
    let cache_creation_cost =
        (cache_creation_tokens.unwrap_or(0) as f64 / 1_000_000.0) * pricing.cache_creation;
    Some(input_cost + output_cost + cache_read_cost + cache_creation_cost)
}

/// Estimate cost in USD for a token usage record using the default pricing config.
#[must_use]
#[allow(dead_code)]
pub fn estimate_cost(
    model: &str,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
) -> Option<f64> {
    estimate_cost_cfg(
        model,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_creation_tokens,
        &PricingConfig::default(),
    )
}

/// Parse a raw Claude API usage block into a `ParsedUsage` using the given pricing config.
#[must_use]
#[allow(dead_code)]
pub fn parse_api_usage_cfg(
    raw: &RawTokenUsage,
    agent_id: &str,
    session_id: Option<i64>,
    model: &str,
    cfg: &PricingConfig,
) -> ParsedUsage {
    let cost = estimate_cost_cfg(
        model,
        raw.input_tokens,
        raw.output_tokens,
        raw.cache_read_input_tokens,
        raw.cache_creation_input_tokens,
        cfg,
    );
    ParsedUsage {
        agent_id: agent_id.to_string(),
        session_id,
        input_tokens: raw.input_tokens,
        output_tokens: raw.output_tokens,
        provider: AgentProvider::Claude,
        cached_input_tokens: None,
        reasoning_output_tokens: None,
        cache_read_tokens: raw.cache_read_input_tokens,
        cache_creation_tokens: raw.cache_creation_input_tokens,
        model: model.to_string(),
        cost_estimate: cost,
        provider_metadata_json: None,
    }
}

/// Convert normalized provider usage into a database-ready record. Codex
/// account sessions remain unpriced unless project pricing explicitly matches
/// the selected model.
#[must_use]
pub fn parse_runtime_usage(
    raw: &RuntimeUsage,
    provider: AgentProvider,
    agent_id: &str,
    session_id: Option<i64>,
    model: &str,
    cfg: &PricingConfig,
    provider_metadata: Option<&serde_json::Value>,
) -> ParsedUsage {
    let cost = estimate_cost_cfg(
        model,
        raw.input_tokens,
        raw.output_tokens,
        raw.cached_input_tokens,
        None,
        cfg,
    );
    ParsedUsage {
        agent_id: agent_id.to_string(),
        session_id,
        input_tokens: raw.input_tokens,
        output_tokens: raw.output_tokens,
        provider,
        cached_input_tokens: raw.cached_input_tokens,
        reasoning_output_tokens: raw.reasoning_output_tokens,
        cache_read_tokens: raw.cached_input_tokens,
        cache_creation_tokens: None,
        model: model.to_string(),
        cost_estimate: cost,
        provider_metadata_json: provider_metadata.map(serde_json::Value::to_string),
    }
}

/// Parse a raw Claude API usage block into a `ParsedUsage` using the default pricing config.
#[must_use]
#[allow(dead_code)]
pub fn parse_api_usage(
    raw: &RawTokenUsage,
    agent_id: &str,
    session_id: Option<i64>,
    model: &str,
) -> ParsedUsage {
    parse_api_usage_cfg(raw, agent_id, session_id, model, &PricingConfig::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_cost_sonnet() {
        let cost = estimate_cost("claude-sonnet-4-20250514", 1_000_000, 1_000_000, None, None);
        assert!(cost.is_some());
        let c = cost.unwrap();
        // 3.0 + 15.0 = 18.0
        assert!((c - 18.0).abs() < 0.001);
    }

    #[test]
    fn test_estimate_cost_opus() {
        let cost = estimate_cost("claude-opus-4-20250514", 1_000_000, 1_000_000, None, None);
        assert!(cost.is_some());
        let c = cost.unwrap();
        // 15.0 + 75.0 = 90.0
        assert!((c - 90.0).abs() < 0.001);
    }

    #[test]
    fn test_estimate_cost_haiku() {
        let cost = estimate_cost(
            "claude-haiku-4-5-20251001",
            1_000_000,
            1_000_000,
            None,
            None,
        );
        assert!(cost.is_some());
        let c = cost.unwrap();
        // 0.80 + 4.0 = 4.80
        assert!((c - 4.80).abs() < 0.001);
    }

    #[test]
    fn test_estimate_cost_with_cache() {
        let cost = estimate_cost(
            "claude-sonnet-4-20250514",
            500_000,
            200_000,
            Some(1_000_000),
            Some(300_000),
        );
        assert!(cost.is_some());
        let c = cost.unwrap();
        // input: 0.5 * 3.0 = 1.5
        // output: 0.2 * 15.0 = 3.0
        // cache_read: 1.0 * 0.3 = 0.3
        // cache_creation: 0.3 * 3.75 = 1.125
        let expected = 1.5 + 3.0 + 0.3 + 1.125;
        assert!((c - expected).abs() < 0.001);
    }

    #[test]
    fn test_estimate_cost_unknown_model() {
        let cost = estimate_cost("gpt-4o", 1000, 500, None, None);
        assert!(cost.is_none());
    }

    #[test]
    fn test_parse_api_usage() {
        let raw = RawTokenUsage {
            input_tokens: 5000,
            output_tokens: 1000,
            cache_read_input_tokens: Some(10000),
            cache_creation_input_tokens: None,
        };
        let parsed = parse_api_usage(&raw, "agent-1", Some(42), "claude-sonnet-4-20250514");
        assert_eq!(parsed.agent_id, "agent-1");
        assert_eq!(parsed.session_id, Some(42));
        assert_eq!(parsed.input_tokens, 5000);
        assert_eq!(parsed.output_tokens, 1000);
        assert_eq!(parsed.cache_read_tokens, Some(10000));
        assert!(parsed.cost_estimate.is_some());
        assert_eq!(parsed.model, "claude-sonnet-4-20250514");
    }

    #[test]
    fn test_raw_token_usage_deserialize() {
        let json = r#"{"input_tokens": 100, "output_tokens": 50}"#;
        let raw: RawTokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(raw.input_tokens, 100);
        assert_eq!(raw.output_tokens, 50);
        assert!(raw.cache_read_input_tokens.is_none());
    }

    #[test]
    fn test_raw_token_usage_deserialize_with_cache() {
        let json = r#"{
            "input_tokens": 100,
            "output_tokens": 50,
            "cache_read_input_tokens": 2000,
            "cache_creation_input_tokens": 500
        }"#;
        let raw: RawTokenUsage = serde_json::from_str(json).unwrap();
        assert_eq!(raw.cache_read_input_tokens, Some(2000));
        assert_eq!(raw.cache_creation_input_tokens, Some(500));
    }

    #[test]
    fn test_pricing_config_exact_model_match() {
        let mut cfg = PricingConfig::default();
        cfg.models.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input: 5.0,
                output: 15.0,
                ..Default::default()
            },
        );
        let cost = estimate_cost_cfg("gpt-4o", 1_000_000, 1_000_000, None, None, &cfg);
        assert!(cost.is_some());
        // 5.0 + 15.0 = 20.0
        assert!((cost.unwrap() - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_pricing_config_provider_prefix_match() {
        let mut cfg = PricingConfig::default();
        cfg.providers.insert(
            "gpt-".to_string(),
            ModelPricing {
                input: 2.5,
                output: 10.0,
                ..Default::default()
            },
        );
        let cost = estimate_cost_cfg("gpt-4o-mini", 1_000_000, 1_000_000, None, None, &cfg);
        assert!(cost.is_some());
        // 2.5 + 10.0 = 12.5
        assert!((cost.unwrap() - 12.5).abs() < 0.001);
    }

    #[test]
    fn test_pricing_config_longest_prefix_wins() {
        let mut cfg = PricingConfig::default();
        cfg.providers.insert(
            "gpt-".to_string(),
            ModelPricing {
                input: 2.5,
                output: 10.0,
                ..Default::default()
            },
        );
        cfg.providers.insert(
            "gpt-4o".to_string(),
            ModelPricing {
                input: 5.0,
                output: 15.0,
                ..Default::default()
            },
        );
        // "gpt-4o-mini" matches both "gpt-" and "gpt-4o"; longest prefix wins.
        let cost = estimate_cost_cfg("gpt-4o-mini", 1_000_000, 1_000_000, None, None, &cfg);
        assert!(cost.is_some());
        // 5.0 + 15.0 = 20.0 (from the longer "gpt-4o" prefix)
        assert!((cost.unwrap() - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_pricing_config_default_fallback() {
        let mut cfg = PricingConfig::default();
        cfg.default = Some(ModelPricing {
            input: 1.0,
            output: 2.0,
            ..Default::default()
        });
        // Unknown model with no provider match falls back to default.
        let cost = estimate_cost_cfg("some-unknown-model", 1_000_000, 1_000_000, None, None, &cfg);
        assert!(cost.is_some());
        // 1.0 + 2.0 = 3.0
        assert!((cost.unwrap() - 3.0).abs() < 0.001);
    }

    #[test]
    fn test_pricing_config_none_when_no_match() {
        let cfg = PricingConfig::default();
        // No models, no providers, no default -> None.
        let cost = estimate_cost_cfg("gpt-4o", 1_000_000, 1_000_000, None, None, &cfg);
        assert!(cost.is_none());
    }

    #[test]
    fn test_pricing_config_anthropic_heuristic_still_works() {
        let cfg = PricingConfig::default();
        // With an empty config, Anthropic substring heuristics still apply.
        let cost = estimate_cost_cfg(
            "claude-sonnet-4-20250514",
            1_000_000,
            1_000_000,
            None,
            None,
            &cfg,
        );
        assert!(cost.is_some());
        assert!((cost.unwrap() - 18.0).abs() < 0.001);
    }

    #[test]
    fn test_load_pricing_config_missing_file() {
        let dir = std::env::temp_dir().join("crosslink-nonexistent-dir-xyz");
        let cfg = load_pricing_config(&dir);
        assert!(cfg.models.is_empty());
        assert!(cfg.providers.is_empty());
        assert!(cfg.default.is_none());
    }

    #[test]
    fn test_load_pricing_config_from_json() {
        let tmp = std::env::temp_dir().join("crosslink-pricing-test");
        std::fs::create_dir_all(&tmp).unwrap();
        let json = r#"{
            "pricing": {
                "models": {
                    "custom-model": { "input": 7.0, "output": 21.0 }
                },
                "providers": {
                    "gemini-": { "input": 1.0, "output": 2.0 }
                },
                "default": { "input": 0.5, "output": 1.0 }
            }
        }"#;
        std::fs::write(tmp.join("hook-config.json"), json).unwrap();
        let cfg = load_pricing_config(&tmp);
        assert!(cfg.models.contains_key("custom-model"));
        assert!(cfg.providers.contains_key("gemini-"));
        assert!(cfg.default.is_some());
        let cost = estimate_cost_cfg("custom-model", 1_000_000, 1_000_000, None, None, &cfg);
        assert!((cost.unwrap() - 28.0).abs() < 0.001);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
