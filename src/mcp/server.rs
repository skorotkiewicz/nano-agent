use crate::config::McpServerConfig;
use crate::mcp::McpTool;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

pub struct McpServer {
    name: String,
    #[allow(dead_code)]
    child: tokio::process::Child,
    reader: Mutex<BufReader<tokio::process::ChildStdout>>,
    writer: Mutex<tokio::process::ChildStdin>,
    request_id: AtomicU64,
    tools: Vec<McpTool>,
}

impl McpServer {
    pub async fn start(config: &McpServerConfig, name: &str) -> Result<Self, String> {
        if let Some(url) = &config.url {
            return Err(format!("HTTP MCP servers not yet supported: {}", url));
        }

        let command = config
            .command
            .as_ref()
            .ok_or("command required for stdio MCP server")?;

        let mut cmd = Command::new(command);
        cmd.args(&config.args);
        for (key, val) in &config.env {
            cmd.env(key, val);
        }
        cmd.stdout(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::inherit());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn MCP server: {}", e))?;

        let stdout = child.stdout.take().ok_or("failed to get stdout")?;

        let stdin = child.stdin.take().ok_or("failed to get stdin")?;

        Ok(McpServer {
            name: name.to_string(),
            child,
            reader: Mutex::new(BufReader::new(stdout)),
            writer: Mutex::new(stdin),
            request_id: AtomicU64::new(1),
            tools: Vec::new(),
        })
    }

    pub async fn initialize(&mut self) -> Result<Vec<McpTool>, String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "nano-agent", "version": "0.1.0"}
            }
        });
        self.send_request(&request).await?;

        self.list_tools().await
    }

    fn next_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::SeqCst)
    }

    async fn send_request(&mut self, request: &Value) -> Result<Value, String> {
        let mut writer = self.writer.lock().await;
        let message = serde_json::to_string(request).map_err(|e| format!("JSON error: {}", e))?;

        writer
            .write_all(format!("Content-Length: {}\r\n\r\n{}", message.len(), message).as_bytes())
            .await
            .map_err(|e| format!("Write error: {}", e))?;

        writer
            .flush()
            .await
            .map_err(|e| format!("Flush error: {}", e))?;

        drop(writer);

        self.read_response().await
    }

    async fn read_response(&mut self) -> Result<Value, String> {
        let mut reader = self.reader.lock().await;

        let mut headers = String::new();
        let mut line = String::new();
        while reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?
            > 0
        {
            headers.push_str(&line);
            if line == "\r\n" || line == "\n" {
                break;
            }
            line.clear();
        }

        let content_length = headers
            .lines()
            .find_map(|h| {
                if h.to_lowercase().starts_with("content-length:") {
                    h.split(':').nth(1)?.trim().parse().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0);

        let mut content = vec![0u8; content_length];
        use tokio::io::AsyncReadExt;
        reader
            .read_exact(&mut content)
            .await
            .map_err(|e| format!("Read error: {}", e))?;

        serde_json::from_slice(&content).map_err(|e| format!("Parse error: {}", e))
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/list",
            "params": {}
        });

        let response = self.send_request(&request).await?;

        let tools: Vec<McpTool> = response
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        Some(McpTool {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t.get("description")?.as_str()?.to_string(),
                            parameters: t.get("inputSchema")?.clone(),
                            server_name: self.name.clone(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        self.tools = tools.clone();
        Ok(tools)
    }

    pub async fn call_tool(&mut self, tool_name: &str, args: Value) -> Result<String, String> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id(),
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": args
            }
        });

        let response = self.send_request(&request).await?;

        response
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "No text in tool response".to_string())
    }
}
