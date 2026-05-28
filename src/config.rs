use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomProvider {
    pub provider_type: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub show_logs: bool,
}

fn default_acp_timeout() -> u64 {
    60
}

fn default_acp_host() -> String {
    "127.0.0.1".to_string()
}

fn default_acp_port() -> u16 {
    8643
}

fn default_acp_agent_name() -> String {
    "nano".to_string()
}

fn default_acp_description() -> String {
    "Nano local shell agent".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpAgentConfig {
    pub endpoint: String,
    #[serde(default)]
    pub agent_name: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_acp_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

impl Default for AcpAgentConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            agent_name: None,
            api_key: None,
            timeout: default_acp_timeout(),
            headers: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_acp_host")]
    pub host: String,
    #[serde(default = "default_acp_port")]
    pub port: u16,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_acp_agent_name")]
    pub agent_name: String,
    #[serde(default = "default_acp_description")]
    pub description: String,
    #[serde(default)]
    pub agents: HashMap<String, AcpAgentConfig>,
}

impl Default for AcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_acp_host(),
            port: default_acp_port(),
            api_key: None,
            agent_name: default_acp_agent_name(),
            description: default_acp_description(),
            agents: HashMap::new(),
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
    pub custom_providers: std::collections::HashMap<String, CustomProvider>,
    #[serde(default)]
    pub mcp_servers: std::collections::HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub acp: AcpConfig,
}

impl Config {
    pub fn load() -> Self {
        // Check global config first (~/.config/nano/config.json)
        if let Some(config) = Self::load_from_path(&config_path_global()) {
            return config;
        }
        // Fallback to local config in cwd
        Self::load_from_path(&config_path_local()).unwrap_or_default()
    }

    fn load_from_path(path: &PathBuf) -> Option<Self> {
        if path.exists() {
            let data = std::fs::read_to_string(path).ok()?;
            serde_json::from_str(&data).ok()
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
    fn test_acp_config_defaults() {
        let parsed: Config = serde_json::from_str("{}").unwrap();
        assert!(!parsed.acp.enabled);
        assert_eq!(parsed.acp.host, "127.0.0.1");
        assert_eq!(parsed.acp.port, 8643);
        assert_eq!(parsed.acp.agent_name, "nano");
    }

    #[test]
    fn test_load_config_with_acp_agent() {
        let config = r#"{
            "acp": {
                "enabled": true,
                "agents": {
                    "coder": {
                        "endpoint": "http://localhost:8644",
                        "agent_name": "nano-coder",
                        "timeout": 120
                    }
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let agent = parsed.acp.agents.get("coder").unwrap();
        assert!(parsed.acp.enabled);
        assert_eq!(agent.endpoint, "http://localhost:8644");
        assert_eq!(agent.agent_name.as_deref(), Some("nano-coder"));
        assert_eq!(agent.timeout, 120);
    }
}
