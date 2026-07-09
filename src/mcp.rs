//! MCP client: connect to stdio / HTTP servers, list and call tools, with a
//! config-fingerprinted tool cache and name disambiguation.

use crate::config::{Config, McpServerConfig};
use compact_str::CompactString;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient, RunningService, serve_client};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::process::Command;
use tokio::sync::Mutex;

const MCP_CACHE_VERSION: u8 = 1;
const RESERVED_TOOL_NAMES: &[&str] = &["execute_shell", "delegate_task", "delegate_tasks"];

// ---------------------------------------------------------------------------
// Tool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub server_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
}

impl McpTool {
    pub fn new(name: String, description: String, parameters: Value, server_name: String) -> Self {
        McpTool {
            name,
            description,
            parameters,
            server_name,
            original_name: None,
        }
    }

    /// The name to actually call on the server (the disambiguated exposure name
    /// maps back to the server's original tool name).
    pub fn call_name(&self) -> &str {
        self.original_name.as_deref().unwrap_or(&self.name)
    }

    pub fn to_tool_schema(&self) -> Value {
        serde_json::json!({
            "type": "function",
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters
        })
    }
}

// ---------------------------------------------------------------------------
// Server handle
// ---------------------------------------------------------------------------

pub struct McpServerHandle {
    pub server_name: CompactString,
    pub running_service: RunningService<RoleClient, ()>,
    tools: Vec<McpTool>,
}

impl McpServerHandle {
    pub async fn connect(
        server_name: CompactString,
        config: &McpServerConfig,
    ) -> Result<Self, String> {
        let running_service = if let Some(url) = &config.url {
            let mut cfg = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if !config.headers.is_empty() {
                let headers = parse_headers(&config.headers)?;
                cfg = cfg.custom_headers(headers);
            }
            let transport =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransport::from_config(
                    cfg,
                );
            serve_client((), transport)
                .await
                .map_err(|e| format!("MCP HTTP connection failed for '{}': {}", server_name, e))?
        } else {
            let command = config
                .command
                .as_ref()
                .ok_or("command or url required for MCP server")?;

            let mut cmd = Command::new(command);
            cmd.args(&config.args);
            for (key, val) in &config.env {
                cmd.env(key, val);
            }
            if !config.show_logs {
                cmd.stderr(Stdio::null());
            }

            let transport = rmcp::transport::TokioChildProcess::new(cmd)
                .map_err(|e| format!("Failed to create transport: {}", e))?;

            serve_client((), transport)
                .await
                .map_err(|e| format!("MCP connection failed for '{}': {}", server_name, e))?
        };

        Ok(Self {
            server_name,
            running_service,
            tools: Vec::new(),
        })
    }

    pub fn peer(&self) -> Peer<RoleClient> {
        self.running_service.peer().clone()
    }

    pub async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let tools = self
            .running_service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("List tools failed: {}", e))?;

        let result: Vec<McpTool> = tools
            .into_iter()
            .map(|t| McpTool {
                name: t.name.to_string(),
                original_name: None,
                description: t
                    .description
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                parameters: serde_json::to_value(&t.input_schema).unwrap_or_default(),
                server_name: self.server_name.to_string(),
            })
            .collect();

        self.tools = result.clone();
        Ok(result)
    }

    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<String, String> {
        let arguments: Option<rmcp::model::JsonObject> = serde_json::from_value(args).ok();

        let params = arguments
            .map(|a| CallToolRequestParams::new(tool_name.to_string()).with_arguments(a))
            .unwrap_or_else(|| CallToolRequestParams::new(tool_name.to_string()));

        let result = self
            .running_service
            .peer()
            .call_tool(params)
            .await
            .map_err(|e| format!("Tool call failed: {}", e))?;

        let mut content = String::new();
        for item in result.content {
            if let rmcp::model::RawContent::Text(t) = item.raw {
                content.push_str(&t.text);
            }
        }

        Ok(content)
    }
}

fn parse_headers(
    headers: &HashMap<String, String>,
) -> Result<HashMap<http::HeaderName, http::HeaderValue>, String> {
    let mut result = HashMap::new();
    for (name, value) in headers {
        let h_name: http::HeaderName = name
            .parse()
            .map_err(|e| format!("Invalid header name '{}': {}", name, e))?;
        let h_value: http::HeaderValue = value
            .parse()
            .map_err(|e| format!("Invalid header value for '{}': {}", name, e))?;
        result.insert(h_name, h_value);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Client (with cache + lazy refresh)
// ---------------------------------------------------------------------------

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
    refresh_needed: Arc<AtomicBool>,
}

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            servers: Arc::clone(&self.servers),
            tools: Arc::clone(&self.tools),
            cache_path: self.cache_path.clone(),
            total_servers: Arc::clone(&self.total_servers),
            server_configs: Arc::clone(&self.server_configs),
            refresh_needed: Arc::clone(&self.refresh_needed),
        }
    }
}

impl McpClient {
    pub fn new() -> Self {
        use crate::paths::{ensure_nano_dirs, nano_mcp_cache_path};
        ensure_nano_dirs();
        let cache_path = nano_mcp_cache_path();
        // Migrate from XDG cache if present and target empty.
        if !cache_path.exists() {
            let legacy = dirs::cache_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("nano")
                .join("mcp_cache.json");
            if legacy.exists() {
                let _ = std::fs::copy(&legacy, &cache_path);
            }
        }

        McpClient {
            servers: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
            cache_path,
            total_servers: Arc::new(Mutex::new(0)),
            server_configs: Arc::new(Mutex::new(HashMap::new())),
            refresh_needed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn load_servers(&self, config: &Config) {
        *self.total_servers.lock().await = config.mcp_servers.len();
        let fingerprint = mcp_cache_fingerprint(config);

        {
            let mut configs = self.server_configs.lock().await;
            configs.clear();
            for (name, server_config) in &config.mcp_servers {
                configs.insert(name.clone(), server_config.clone());
            }
        }
        self.servers.lock().await.clear();

        if let Some(cached_tools) = Self::load_from_cache(&self.cache_path, &fingerprint).await {
            let mut tools = self.tools.lock().await;
            tools.clear();
            tools.extend(cached_tools);
            self.refresh_needed.store(true, Ordering::SeqCst);
            return;
        }

        self.tools.lock().await.clear();
        self.refresh_needed.store(false, Ordering::SeqCst);
        self.refresh_servers(config.clone(), false).await;
    }

    async fn refresh_servers(&self, config: Config, keep_cached_on_total_failure: bool) {
        let total = config.mcp_servers.len();
        let mut connected = 0;
        let mut loaded_tools = Vec::new();
        let mut live_servers = HashMap::new();
        for (name, server_config) in &config.mcp_servers {
            eprintln!("\rconnecting... {}/{}", connected + 1, total);
            match McpServerHandle::connect(CompactString::new(name.clone()), server_config).await {
                Ok(mut server) => match server.list_tools().await {
                    Ok(tools) => {
                        loaded_tools.extend(tools);
                        live_servers.insert(name.clone(), server);
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

        if connected == 0 && keep_cached_on_total_failure {
            return;
        }

        disambiguate_tools(&mut loaded_tools);
        *self.servers.lock().await = live_servers;
        *self.tools.lock().await = loaded_tools;
        self.refresh_needed.store(false, Ordering::SeqCst);
        self.save_to_cache(&mcp_cache_fingerprint(&config)).await;
    }

    async fn config_snapshot(&self) -> Config {
        Config {
            mcp_servers: self.server_configs.lock().await.clone(),
            ..Default::default()
        }
    }

    pub fn status(&self) -> String {
        let connected = self.servers.try_lock().map(|s| s.len()).unwrap_or(0);
        let total = self.total_servers.try_lock().map(|t| *t).unwrap_or(0);
        if connected == 0 && total > 0 {
            let cached = self.tools.try_lock().map(|t| t.len()).unwrap_or(0);
            if cached > 0 {
                return format!("(mcp: cached {} tools)", cached);
            }
        }
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
        let trigger_refresh = self.refresh_needed.swap(false, Ordering::SeqCst);
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

        {
            let mut servers = self.servers.lock().await;
            if !servers.contains_key(&server_name) {
                eprintln!("connecting...");
                let mut server =
                    McpServerHandle::connect(CompactString::new(server_name.clone()), &config)
                        .await?;
                server.list_tools().await?;
                servers.insert(server_name.clone(), server);
            }
        }

        let mut servers = self.servers.lock().await;
        let server = servers
            .get_mut(&server_name)
            .ok_or_else(|| "server not found".to_string())?;

        let result = server.call_tool(&call_name, args).await;
        drop(servers);

        if trigger_refresh {
            let client = self.clone();
            let config = self.config_snapshot().await;
            tokio::spawn(async move {
                client.refresh_servers(config, true).await;
            });
        }

        result
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

/// Rename duplicates and reserved collisions to `server__tool`, keeping the
/// original name for the actual server call.
fn disambiguate_tools(tools: &mut [McpTool]) {
    let mut counts = HashMap::new();
    for tool in tools.iter() {
        *counts.entry(tool.name.clone()).or_insert(0usize) += 1;
    }

    let mut used = RESERVED_TOOL_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect::<std::collections::HashSet<_>>();
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
    use std::path::PathBuf;

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
        let mut tools = vec![
            McpTool::new(
                "execute_shell".to_string(),
                "Remote shell".to_string(),
                serde_json::json!({"type": "object"}),
                "remote".to_string(),
            ),
            McpTool::new(
                "delegate_tasks".to_string(),
                "Remote bulk delegate".to_string(),
                serde_json::json!({"type": "object"}),
                "remote".to_string(),
            ),
        ];

        disambiguate_tools(&mut tools);

        assert_eq!(tools[0].name, "remote__execute_shell");
        assert_eq!(tools[0].call_name(), "execute_shell");
        assert_eq!(tools[1].name, "remote__delegate_tasks");
        assert_eq!(tools[1].call_name(), "delegate_tasks");
    }

    #[tokio::test]
    async fn cache_hit_loads_tools_without_connecting_and_arms_lazy_refresh() {
        let mut client = super::McpClient::new();
        client.cache_path = std::env::temp_dir().join(format!(
            "nano-agent-mcp-cache-test-{}.json",
            std::process::id()
        ));

        let config: Config = serde_json::from_str(
            r#"{
                "mcp_servers": {
                    "docs": {
                        "command": "definitely-not-a-real-command"
                    }
                }
            }"#,
        )
        .unwrap();
        let fingerprint = mcp_cache_fingerprint(&config);
        let cache = serde_json::json!({
            "version": 1,
            "fingerprint": fingerprint,
            "tools": [{
                "name": "search",
                "description": "cached",
                "parameters": {"type": "object"},
                "server_name": "docs"
            }]
        });
        std::fs::write(&client.cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();

        client.load_servers(&config).await;

        let tool_names = client
            .tools
            .lock()
            .await
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        assert_eq!(tool_names, vec!["search".to_string()]);
        assert!(client.servers.lock().await.is_empty());
        assert!(
            client
                .refresh_needed
                .load(std::sync::atomic::Ordering::SeqCst)
        );

        let _ = std::fs::remove_file(PathBuf::from(&client.cache_path));
    }
}
