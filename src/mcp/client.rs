use crate::mcp::McpTool;
use compact_str::CompactString;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

// use crate::mcp::server::McpServerHandle;
use super::server::McpServerHandle;

pub struct McpClient {
    servers: Arc<Mutex<HashMap<String, McpServerHandle>>>,
    tools: Arc<Mutex<Vec<McpTool>>>,
    cache_path: PathBuf,
    total_servers: Arc<Mutex<usize>>,
    server_configs: Arc<Mutex<HashMap<String, crate::config::McpServerConfig>>>,
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

    pub async fn load_servers(&self, config: &crate::config::Config) {
        *self.total_servers.lock().await = config.mcp_servers.len();
        let total = *self.total_servers.lock().await;

        // Store server configs for lazy connection
        {
            let mut configs = self.server_configs.lock().await;
            for (name, server_config) in &config.mcp_servers {
                configs.insert(name.clone(), server_config.clone());
            }
        }

        // Try loading from cache first
        if let Some(cached_tools) = Self::load_from_cache(&self.cache_path).await
            && self.is_cache_valid(config).await
        {
            self.tools.lock().await.extend(cached_tools);
            return;
        }

        let mut connected = 0;
        for (name, server_config) in &config.mcp_servers {
            eprintln!("\rconnecting... {}/{}", connected + 1, total);
            match McpServerHandle::connect(CompactString::new(name.clone()), server_config).await {
                Ok(mut server) => match server.list_tools().await {
                    Ok(tools) => {
                        self.tools.lock().await.extend(tools.clone());
                        self.servers.lock().await.insert(name.clone(), server);
                        connected += 1;
                        // eprintln!("\r(mcp: {}/{}) servers", connected, total);
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

        self.save_to_cache().await;
    }

    pub fn status(&self) -> String {
        let connected = self.servers.try_lock().map(|s| s.len()).unwrap_or(0);
        let total = self.total_servers.try_lock().map(|t| *t).unwrap_or(0);
        format!("(mcp: {}/{}) servers", connected, total)
    }

    async fn load_from_cache(path: &PathBuf) -> Option<Vec<McpTool>> {
        if !path.exists() {
            return None;
        }
        let data = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&data).ok()
    }

    async fn save_to_cache(&self) {
        let tools = self.tools.lock().await.clone();
        if let Ok(data) = serde_json::to_string(&tools) {
            if let Some(parent) = self.cache_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&self.cache_path, data);
        }
    }

    async fn is_cache_valid(&self, _config: &crate::config::Config) -> bool {
        if !self.cache_path.exists() {
            return false;
        }

        let cache_modified = self.cache_path.metadata().and_then(|m| m.modified()).ok();

        if let Some(cache_time) = cache_modified {
            let config_path = std::env::current_dir()
                .unwrap_or_default()
                .join("nano_config.json");
            if config_path.exists()
                && let Ok(config_modified) = config_path.metadata().and_then(|m| m.modified())
                && config_modified > cache_time
            {
                return false;
            }
            true
        } else {
            false
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
        let (server_name, config) = {
            let tools = self.tools.lock().await;
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| "tool not found".to_string())?;
            let name = tool.server_name.clone();
            drop(tools);

            // Get config for this server
            let configs = self.server_configs.lock().await;
            let config = configs
                .get(&name)
                .cloned()
                .ok_or_else(|| "server config not found".to_string())?;
            (name, config)
        };

        // Check if already connected, if not connect lazily
        {
            let mut servers = self.servers.lock().await;
            if !servers.contains_key(&server_name) {
                let mut server =
                    McpServerHandle::connect(CompactString::new(server_name.clone()), &config)
                        .await?;
                eprintln!("connecting...");
                server.list_tools().await?; // Initialize tools
                servers.insert(server_name.clone(), server);
            }
        }

        let mut servers = self.servers.lock().await;
        let server = servers
            .get_mut(&server_name)
            .ok_or_else(|| "server not found".to_string())?;

        server.call_tool(name, args).await
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}
