use crate::config::{Config, McpServerConfig};
use crate::mcp::McpTool;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::server::McpServerHandle;

const MCP_CACHE_VERSION: u8 = 1;
const RESERVED_TOOL_NAMES: &[&str] = &["execute_shell", "delegate_task"];

#[derive(Debug, Serialize, Deserialize)]
struct McpToolCache {
    version: u8,
    fingerprint: String,
    tools: Vec<McpTool>,
}

pub struct McpClient {
    servers: Arc<Mutex<HashMap<String, McpServerHandle>>>,
    tools: Arc<Mutex<Vec<McpTool>>>,
    cache_path: PathBuf,
    total_servers: Arc<Mutex<usize>>,
    server_configs: Arc<Mutex<HashMap<String, McpServerConfig>>>,
}

impl McpClient {
    pub fn new() -> Self {
        let cache_path = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nano")
            .join("mcp_cache.json");

        McpClient {
            servers: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
            cache_path,
            total_servers: Arc::new(Mutex::new(0)),
            server_configs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn load_servers(&self, config: &Config) {
        *self.total_servers.lock().await = config.mcp_servers.len();
        let total = *self.total_servers.lock().await;
        let fingerprint = mcp_cache_fingerprint(config);

        {
            let mut configs = self.server_configs.lock().await;
            configs.clear();
            for (name, server_config) in &config.mcp_servers {
                configs.insert(name.clone(), server_config.clone());
            }
        }
        self.tools.lock().await.clear();
        self.servers.lock().await.clear();

        if let Some(cached_tools) = Self::load_from_cache(&self.cache_path, &fingerprint).await {
            self.tools.lock().await.extend(cached_tools);
            return;
        }

        let mut connected = 0;
        let mut loaded_tools = Vec::new();
        for (name, server_config) in &config.mcp_servers {
            eprintln!("\rconnecting... {}/{}", connected + 1, total);
            match McpServerHandle::connect(CompactString::new(name.clone()), server_config).await {
                Ok(mut server) => match server.list_tools().await {
                    Ok(tools) => {
                        loaded_tools.extend(tools);
                        self.servers.lock().await.insert(name.clone(), server);
                        connected += 1;
                    }
                    Err(e) => {
                        eprintln!("\n[WARN] MCP server '{}' failed to initialize: {}", name, e);
                    }
                },
                Err(e) => {
                    eprintln!("\n[WARN] MCP server '{}' failed to start: {}", name, e);
                }
            }
        }

        disambiguate_tools(&mut loaded_tools);
        *self.tools.lock().await = loaded_tools;
        self.save_to_cache(&fingerprint).await;
    }

    pub fn status(&self) -> String {
        let connected = self.servers.try_lock().map(|s| s.len()).unwrap_or(0);
        let total = self.total_servers.try_lock().map(|t| *t).unwrap_or(0);
        format!("(mcp: {}/{}) servers", connected, total)
    }

    async fn load_from_cache(path: &PathBuf, fingerprint: &str) -> Option<Vec<McpTool>> {
        if !path.exists() {
            return None;
        }
        let data = std::fs::read_to_string(path).ok()?;
        let cache: McpToolCache = serde_json::from_str(&data).ok()?;
        if cache.version == MCP_CACHE_VERSION && cache.fingerprint == fingerprint {
            Some(cache.tools)
        } else {
            None
        }
    }

    async fn save_to_cache(&self, fingerprint: &str) {
        let tools = self.tools.lock().await.clone();
        let cache = McpToolCache {
            version: MCP_CACHE_VERSION,
            fingerprint: fingerprint.to_string(),
            tools,
        };
        if let Ok(data) = serde_json::to_string(&cache) {
            if let Some(parent) = self.cache_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.cache_path, data);
        }
    }

    pub async fn get_tools_schema(&self) -> Vec<Value> {
        self.tools
            .lock()
            .await
            .iter()
            .map(|t| t.clone().to_tool_schema())
            .collect()
    }

    pub async fn has_tool(&self, name: &str) -> bool {
        self.tools.lock().await.iter().any(|t| t.name == name)
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<String, String> {
        let (server_name, config, call_name) = {
            let tools = self.tools.lock().await;
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| "tool not found".to_string())?;
            let server_name = tool.server_name.clone();
            let call_name = tool.call_name().to_string();
            drop(tools);

            let configs = self.server_configs.lock().await;
            let config = configs
                .get(&server_name)
                .cloned()
                .ok_or_else(|| "server config not found".to_string())?;
            (server_name, config, call_name)
        };

        // Check if already connected, if not connect lazily
        {
            let mut servers = self.servers.lock().await;
            if !servers.contains_key(&server_name) {
                eprintln!("connecting...");
                let mut server =
                    McpServerHandle::connect(CompactString::new(server_name.clone()), &config)
                        .await?;
                server.list_tools().await?; // Initialize tools
                servers.insert(server_name.clone(), server);
            }
        }

        let mut servers = self.servers.lock().await;
        let server = servers
            .get_mut(&server_name)
            .ok_or_else(|| "server not found".to_string())?;

        server.call_tool(&call_name, args).await
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

fn sorted_pairs(map: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    pairs
}

fn mcp_cache_fingerprint(config: &Config) -> String {
    let mut names = config.mcp_servers.keys().cloned().collect::<Vec<_>>();
    names.sort();

    let servers = names
        .into_iter()
        .filter_map(|name| {
            let server = config.mcp_servers.get(&name)?;
            Some(json!({
                "name": name,
                "command": server.command,
                "args": server.args,
                "env": sorted_pairs(&server.env),
                "url": server.url,
                "headers": sorted_pairs(&server.headers),
                "show_logs": server.show_logs,
            }))
        })
        .collect::<Vec<_>>();

    serde_json::to_string(&servers).unwrap_or_default()
}

fn sanitize_tool_name_part(input: &str) -> String {
    let sanitized = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "mcp".to_string()
    } else {
        sanitized
    }
}

fn disambiguate_tools(tools: &mut [McpTool]) {
    let mut counts = HashMap::new();
    for tool in tools.iter() {
        *counts.entry(tool.name.clone()).or_insert(0usize) += 1;
    }

    let mut used = RESERVED_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect::<HashSet<_>>();
    used.extend(
        tools
            .iter()
            .filter(|tool| {
                counts.get(&tool.name).copied().unwrap_or(0) == 1
                    && !RESERVED_TOOL_NAMES.contains(&tool.name.as_str())
            })
            .map(|tool| tool.name.clone()),
    );

    for tool in tools.iter_mut() {
        let should_disambiguate = counts.get(&tool.name).copied().unwrap_or(0) > 1
            || RESERVED_TOOL_NAMES.contains(&tool.name.as_str());
        if !should_disambiguate {
            continue;
        }

        let original_name = tool.call_name().to_string();
        let base = format!(
            "{}__{}",
            sanitize_tool_name_part(&tool.server_name),
            sanitize_tool_name_part(&original_name)
        );
        let mut exposed_name = base.clone();
        let mut suffix = 2;
        while used.contains(&exposed_name) {
            exposed_name = format!("{}__{}", base, suffix);
            suffix += 1;
        }

        tool.name = exposed_name;
        tool.original_name = Some(original_name);
        used.insert(tool.name.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::{disambiguate_tools, mcp_cache_fingerprint};
    use crate::config::Config;
    use crate::mcp::McpTool;

    #[test]
    fn cache_fingerprint_changes_when_mcp_config_changes() {
        let first: Config = serde_json::from_str(
            r#"{
                "mcp_servers": {
                    "docs": {
                        "command": "uvx",
                        "args": ["docs-server"]
                    }
                }
            }"#,
        )
        .unwrap();
        let second: Config = serde_json::from_str(
            r#"{
                "mcp_servers": {
                    "docs": {
                        "command": "uvx",
                        "args": ["different-server"]
                    }
                }
            }"#,
        )
        .unwrap();

        assert_ne!(
            mcp_cache_fingerprint(&first),
            mcp_cache_fingerprint(&second)
        );
    }

    #[test]
    fn duplicate_tool_names_are_disambiguated_but_call_original_name() {
        let mut tools = vec![
            McpTool::new(
                "search".to_string(),
                "Search docs".to_string(),
                serde_json::json!({"type": "object"}),
                "docs".to_string(),
            ),
            McpTool::new(
                "search".to_string(),
                "Search web".to_string(),
                serde_json::json!({"type": "object"}),
                "web".to_string(),
            ),
            McpTool::new(
                "fetch".to_string(),
                "Fetch docs".to_string(),
                serde_json::json!({"type": "object"}),
                "docs".to_string(),
            ),
        ];

        disambiguate_tools(&mut tools);

        assert_eq!(tools[0].name, "docs__search");
        assert_eq!(tools[0].call_name(), "search");
        assert_eq!(tools[1].name, "web__search");
        assert_eq!(tools[1].call_name(), "search");
        assert_eq!(tools[2].name, "fetch");
        assert_eq!(tools[2].call_name(), "fetch");
    }

    #[test]
    fn reserved_tool_names_are_disambiguated() {
        let mut tools = vec![McpTool::new(
            "execute_shell".to_string(),
            "Remote shell".to_string(),
            serde_json::json!({"type": "object"}),
            "remote".to_string(),
        )];

        disambiguate_tools(&mut tools);

        assert_eq!(tools[0].name, "remote__execute_shell");
        assert_eq!(tools[0].call_name(), "execute_shell");
    }
}
