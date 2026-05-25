use crate::config::McpServerConfig;
use crate::mcp::McpTool;
use compact_str::CompactString;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient, RunningService, serve_client};
use serde_json::Value;
use std::collections::HashMap;
// use std::process::Stdio;
use tokio::process::Command;

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
            // HTTP-based MCP server
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
            // Stdio-based MCP server
            let command = config
                .command
                .as_ref()
                .ok_or("command or url required for MCP server")?;

            let mut cmd = Command::new(command);
            cmd.args(&config.args);
            for (key, val) in &config.env {
                cmd.env(key, val);
            }
            // if !config.show_logs {
            //     cmd.stderr(Stdio::null());
            // }

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
