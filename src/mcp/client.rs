use crate::config::Config;
use crate::mcp::{McpServer, McpTool};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct McpClient {
    servers: Arc<Mutex<HashMap<String, McpServer>>>,
    tools: Arc<Mutex<Vec<McpTool>>>,
}

impl McpClient {
    pub fn new() -> Self {
        McpClient {
            servers: Arc::new(Mutex::new(HashMap::new())),
            tools: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub async fn load_servers(&self, config: &Config) {
        for (name, server_config) in &config.mcp_servers {
            if let Ok(mut server) = McpServer::start(server_config, name).await
                && let Ok(tools) = server.initialize().await
            {
                self.tools.lock().await.extend(tools.clone());
                self.servers.lock().await.insert(name.clone(), server);
            }
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
