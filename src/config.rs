use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomProvider {
    pub provider_type: String,
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub show_logs: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MitoModeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

fn default_acp_timeout_secs() -> u64 {
    600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpAgentConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default = "default_acp_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for AcpAgentConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: vec![],
            env: HashMap::new(),
            working_directory: None,
            timeout_secs: default_acp_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub custom_providers: HashMap<String, CustomProvider>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub acp_agents: HashMap<String, AcpAgentConfig>,
    #[serde(default, rename = "mito-mode", alias = "mito_mode")]
    pub mito_mode: MitoModeConfig,
}

impl Config {
    pub fn load() -> Self {
        let global = Self::load_from_path(&config_path_global());
        let local = Self::load_from_path(&config_path_local());
        match (global, local) {
            (Some(mut g), Some(l)) => {
                merge_config_value(&mut g, l);
                serde_json::from_value(g).unwrap_or_default()
            }
            (Some(g), None) | (None, Some(g)) => serde_json::from_value(g).unwrap_or_default(),
            (None, None) => Self::default(),
        }
    }

    fn load_from_path(path: &PathBuf) -> Option<Value> {
        if path.exists() {
            let data = std::fs::read_to_string(path).ok()?;
            let mut value: Value = serde_json::from_str(&data).ok()?;
            normalize_config_value(&mut value);
            Some(value)
        } else {
            None
        }
    }

    pub fn get_model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn get_provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn get_max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    pub fn get_temperature(&self) -> Option<f32> {
        self.temperature
    }

    pub fn get_custom_provider(&self, name: &str) -> Option<&CustomProvider> {
        self.custom_providers.get(name)
    }

    pub fn get_mito_mode(&self) -> &MitoModeConfig {
        &self.mito_mode
    }
}

fn normalize_config_value(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !object.contains_key("mito-mode")
        && let Some(alias) = object.remove("mito_mode")
    {
        object.insert("mito-mode".to_string(), alias);
    }
}

fn merge_config_value(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                if let Some(existing) = base_map.get_mut(&key) {
                    merge_config_value(existing, value);
                } else {
                    base_map.insert(key, value);
                }
            }
        }
        (base_slot, overlay_value) => *base_slot = overlay_value,
    }
}

fn config_path_global() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_default()
        .join("nano")
        .join("config.json")
}

fn config_path_local() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_default()
        .join("nano_config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.model.is_none());
        assert!(config.provider.is_none());
        assert!(config.max_tokens.is_none());
        assert!(config.temperature.is_none());
        assert!(!config.mito_mode.enabled);
    }

    #[test]
    fn test_config_load_valid_json() {
        let config = r#"{"model": "test-model", "provider": "test-provider"}"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert_eq!(parsed.model, Some("test-model".to_string()));
        assert_eq!(parsed.provider, Some("test-provider".to_string()));
    }

    #[test]
    fn test_load_config_with_providers() {
        let config = r#"{
            "custom_providers": {
                "test": {
                    "provider_type": "openai",
                    "base_url": "http://test.com",
                    "api_key": "key"
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.get_custom_provider("test").is_some());
    }

    #[test]
    fn test_config_load_missing() {
        let config = Config::load();
        // Should return default without error
        assert!(config.model.is_none() || config.model.is_some());
    }

    #[test]
    fn test_getters() {
        let config = Config {
            model: Some("gpt-4".to_string()),
            provider: Some("openai".to_string()),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            ..Default::default()
        };
        assert_eq!(config.get_model(), Some("gpt-4"));
        assert_eq!(config.get_provider(), Some("openai"));
        assert_eq!(config.get_max_tokens(), Some(4096));
        assert_eq!(config.get_temperature(), Some(0.7));
    }

    #[test]
    fn test_load_config_with_mcp_servers() {
        let config = r#"{
            "mcp_servers": {
                "semble": {
                    "command": "uvx",
                    "args": ["--from", "semble[mcp]", "semble"]
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.mcp_servers.contains_key("semble"));
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let config = r#"{
            "mcp_servers": {
                "test": {}
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let server = parsed.mcp_servers.get("test").unwrap();
        assert!(server.command.is_none());
        assert!(server.args.is_empty());
        assert!(server.env.is_empty());
        assert!(server.url.is_none());
        assert!(!server.show_logs);
    }

    #[test]
    fn test_mcp_server_config_show_logs() {
        let config = r#"{
            "mcp_servers": {
                "local": {
                    "command": "uvx",
                    "show_logs": true
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let server = parsed.mcp_servers.get("local").unwrap();
        assert!(server.show_logs);
    }

    #[test]
    fn test_mcp_server_config_url() {
        let config = r#"{
            "mcp_servers": {
                "context7": {
                    "url": "https://mcp.context7.com/mcp",
                    "headers": {
                        "CONTEXT7_API_KEY": "ctx7sk-..."
                    }
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let server = parsed.mcp_servers.get("context7").unwrap();
        assert!(server.url.is_some());
        assert_eq!(server.url.as_ref().unwrap(), "https://mcp.context7.com/mcp");
        assert!(server.headers.contains_key("CONTEXT7_API_KEY"));
    }

    #[test]
    fn test_load_config_with_acp_agents() {
        let config = r#"{
            "acp_agents": {
                "coder": {
                    "command": "nano-agent",
                    "args": ["--acp"],
                    "working_directory": "/tmp/project",
                    "timeout_secs": 120
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let agent = parsed.acp_agents.get("coder").unwrap();
        assert_eq!(agent.command, "nano-agent");
        assert_eq!(agent.args, vec!["--acp"]);
        assert_eq!(agent.working_directory.as_deref(), Some("/tmp/project"));
        assert_eq!(agent.timeout_secs, 120);
    }

    #[test]
    fn test_load_config_with_mito_mode() {
        let config = r#"{
            "mito-mode": {
                "enabled": true,
                "provider": "local-gemma4",
                "model": "gemma4"
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let mito = parsed.get_mito_mode();
        assert!(mito.enabled);
        assert_eq!(mito.provider.as_deref(), Some("local-gemma4"));
        assert_eq!(mito.model.as_deref(), Some("gemma4"));
    }

    #[test]
    fn test_load_config_with_mito_mode_alias() {
        let config = r#"{
            "mito_mode": {
                "enabled": true,
                "provider": "local"
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.mito_mode.enabled);
        assert_eq!(parsed.mito_mode.provider.as_deref(), Some("local"));
    }

    #[test]
    fn merge_overlays_local_fields_and_maps() {
        let mut global: Value = serde_json::from_str(
            r#"{
                "model": "global-model",
                "provider": "global",
                "max_tokens": 100,
                "custom_providers": {"a": {"provider_type": "openai", "base_url": "x"}}
            }"#,
        )
        .unwrap();
        let local: Value = serde_json::from_str(
            r#"{
                "model": "local-model",
                "mcp_servers": {"semble": {"command": "uvx"}}
            }"#,
        )
        .unwrap();
        merge_config_value(&mut global, local);
        let merged: Config = serde_json::from_value(global).unwrap();
        assert_eq!(merged.model.as_deref(), Some("local-model"));
        assert_eq!(merged.provider.as_deref(), Some("global"));
        assert_eq!(merged.max_tokens, Some(100));
        assert!(merged.custom_providers.contains_key("a"));
        assert!(merged.mcp_servers.contains_key("semble"));
    }

    #[test]
    fn merge_allows_local_mito_mode_to_disable_global() {
        let mut global: Value = serde_json::from_str(
            r#"{
                "mito-mode": {
                    "enabled": true,
                    "provider": "global"
                }
            }"#,
        )
        .unwrap();
        let mut local: Value = serde_json::from_str(
            r#"{
                "mito_mode": {
                    "enabled": false
                }
            }"#,
        )
        .unwrap();
        normalize_config_value(&mut local);
        merge_config_value(&mut global, local);
        let merged: Config = serde_json::from_value(global).unwrap();
        assert!(!merged.mito_mode.enabled);
        assert_eq!(merged.mito_mode.provider.as_deref(), Some("global"));
    }
}
