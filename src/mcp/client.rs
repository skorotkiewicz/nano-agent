use crate::config::Config;
use crate::mcp::{McpServer, McpTool};
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct McpClient {
    servers: Arc<Mutex<HashMap<String, McpServer>>>,
    tools: Arc<Mutex<Vec<McpTool>>>,
    cache_path: PathBuf,
    total_servers: Arc<Mutex<usize>>,
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
        }
    }

    pub async fn load_servers(&self, config: &Config) {
        *self.total_servers.lock().await = config.mcp_servers.len();
        let total = *self.total_servers.lock().await;

        // Try loading from cache first
        if let Some(cached_tools) = Self::load_from_cache(&self.cache_path).await {
            // Check if config changed since cache was created
            if self.is_cache_valid(config).await {
                self.tools.lock().await.extend(cached_tools);
                eprintln!("(mcp: 0/{}) servers (from cache)", total);
                return;
            }
        }

        eprintln!("(mcp: 0/{}) servers", total);

        // No valid cache, connect to servers
        let mut connected = 0;
        for (name, server_config) in &config.mcp_servers {
            if let Ok(mut server) = McpServer::start(server_config, name).await
                && let Ok(tools) = server.initialize().await
            {
                self.tools.lock().await.extend(tools.clone());
                self.servers.lock().await.insert(name.clone(), server);
                connected += 1;
                eprintln!("\r(mcp: {}/{}) servers", connected, total);
            }
        }

        // Save to cache
        self.save_to_cache().await;
    }

    pub async fn connect_to_server(&self, server_name: &str) -> Result<(), String> {
        // This would be called when an MCP tool is actually invoked
        // For now, servers are connected at startup via load_servers
        Err(format!(
            "Dynamic server connection not yet implemented: {}",
            server_name
        ))
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

    async fn is_cache_valid(&self, _config: &Config) -> bool {
        // Simple check: cache is valid if no MCP servers are configured
        // or if cache file is newer than config file
        if !self.cache_path.exists() {
            return false;
        }

        let cache_modified = self.cache_path.metadata().and_then(|m| m.modified()).ok();

        // If config file is newer than cache, invalidate
        if let Some(cache_time) = cache_modified {
            // Check nano_config.json
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
        let server_name = {
            let tools = self.tools.lock().await;
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| "tool not found".to_string())?;
            tool.server_name.clone()
        };

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
