//! API provider selection: which endpoint, wire format, key, and model to use.

use crate::state::{get_config, get_model};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiFormat {
    Responses,
    ChatCompletions,
}

#[derive(Clone, Debug)]
pub struct ApiTarget {
    pub url: String,
    pub format: ApiFormat,
    pub api_key: String,
    pub model: String,
}

fn custom_provider_endpoint(provider_type: &str, base: &str) -> (ApiFormat, String) {
    match provider_type.trim().to_ascii_lowercase().as_str() {
        "responses" | "openai-responses" => (ApiFormat::Responses, format!("{}/responses", base)),
        _ => (
            ApiFormat::ChatCompletions,
            format!("{}/chat/completions", base),
        ),
    }
}

pub fn apply_generation_controls(
    body: &mut Value,
    format: ApiFormat,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
) {
    if let Some(n) = max_tokens {
        let key = match format {
            ApiFormat::Responses => "max_output_tokens",
            ApiFormat::ChatCompletions => "max_tokens",
        };
        body[key] = n.into();
    }
    if let Some(t) = temperature {
        body["temperature"] = t.into();
    }
}

pub fn check_api_key() {
    let target = get_api_target();
    let env_key = env::var("OPENAI_API_KEY").unwrap_or_default();
    if target.api_key.is_empty() && env_key.is_empty() {
        eprintln!("set OPENAI_API_KEY or configure a provider in config.json");
        std::process::exit(1);
    }
    if target.model == "gpt-5.5"
        && (get_config().get_provider().is_some() || env::var("OPENAI_BASE_URL").is_ok())
    {
        eprintln!(
            "warning: no model configured; defaulting to '{}'. Set OPENAI_MODEL or a provider model.",
            target.model
        );
    }
}

fn resolve_api_key(configured: Option<String>, fallback: String) -> String {
    configured.unwrap_or(fallback)
}

fn custom_provider_target(provider_name: &str, model: Option<String>) -> Option<ApiTarget> {
    let custom = get_config().get_custom_provider(provider_name)?;
    let base = custom.base_url.trim_end_matches('/');
    let (format, url) = custom_provider_endpoint(&custom.provider_type, base);
    Some(ApiTarget {
        url,
        format,
        api_key: resolve_api_key(
            custom.api_key.clone(),
            env::var("OPENAI_API_KEY").unwrap_or_default(),
        ),
        model: model.unwrap_or_else(|| {
            custom
                .model
                .clone()
                .unwrap_or_else(|| get_model().to_string())
        }),
    })
}

pub fn get_api_target() -> ApiTarget {
    if let Some(provider_name) = get_config().get_provider()
        && let Some(target) = custom_provider_target(provider_name, None)
    {
        return target;
    }

    if let Ok(base) = env::var("OPENAI_BASE_URL") {
        let base = base.trim_end_matches('/');
        ApiTarget {
            url: format!("{}/chat/completions", base),
            format: ApiFormat::ChatCompletions,
            api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            model: get_model().to_string(),
        }
    } else {
        ApiTarget {
            url: "https://api.openai.com/v1/responses".to_string(),
            format: ApiFormat::Responses,
            api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            model: get_model().to_string(),
        }
    }
}

pub fn get_mito_target() -> Result<ApiTarget, String> {
    let mito = get_config().get_mito_mode();
    if !mito.enabled {
        return Err("mito mode is not enabled in config".to_string());
    }

    let provider = mito
        .provider
        .as_deref()
        .ok_or_else(|| "mito-mode.provider is not configured".to_string())?;
    let model = mito.model.clone();
    custom_provider_target(provider, model)
        .ok_or_else(|| format!("mito-mode.provider '{provider}' is not in custom_providers"))
}

#[cfg(test)]
mod tests {
    use super::{ApiFormat, apply_generation_controls, custom_provider_endpoint, resolve_api_key};

    #[test]
    fn responses_use_max_output_tokens() {
        let mut body = serde_json::json!({});
        apply_generation_controls(&mut body, ApiFormat::Responses, Some(123), Some(0.2));
        assert_eq!(body["max_output_tokens"], 123);
        assert!(body.get("max_tokens").is_none());
        assert!((body["temperature"].as_f64().unwrap_or_default() - 0.2).abs() < 0.000_001);
    }

    #[test]
    fn chat_uses_max_tokens() {
        let mut body = serde_json::json!({});
        apply_generation_controls(&mut body, ApiFormat::ChatCompletions, Some(456), None);
        assert_eq!(body["max_tokens"], 456);
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn custom_provider_api_key_falls_back_to_env_only_when_unset() {
        assert_eq!(
            resolve_api_key(None, "env-key".to_string()),
            "env-key".to_string()
        );
        assert_eq!(
            resolve_api_key(Some(String::new()), "env-key".to_string()),
            ""
        );
    }

    #[test]
    fn provider_type_can_select_responses_endpoint() {
        let (format, url) = custom_provider_endpoint("responses", "http://localhost:1234/v1");
        assert_eq!(format, ApiFormat::Responses);
        assert_eq!(url, "http://localhost:1234/v1/responses");
    }
}
