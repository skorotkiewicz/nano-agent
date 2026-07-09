//! API provider selection: which endpoint, wire format, key, and model to use.

use crate::state::{get_config, get_model};
use std::env;

#[derive(Clone, Copy, Debug, PartialEq)]
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

fn custom_provider_target(provider_name: &str, model: Option<String>) -> Option<ApiTarget> {
    let custom = get_config().get_custom_provider(provider_name)?;
    let base = custom.base_url.trim_end_matches('/');
    Some(ApiTarget {
        url: format!("{}/chat/completions", base),
        format: ApiFormat::ChatCompletions,
        api_key: custom.api_key.clone().unwrap_or_default(),
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
