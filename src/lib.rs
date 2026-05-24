pub mod config;
pub mod mcp;
pub mod sandbox;

pub use config::Config;
pub use mcp::{McpClient, McpServerHandle, McpTool};
pub use sandbox::Sandbox;

#[cfg(test)]
mod mcp_tests {
    use super::*;

    #[test]
    fn test_mcp_tool_creation() {
        let tool = McpTool::new(
            "test_tool".to_string(),
            "A test tool".to_string(),
            serde_json::json!({"type": "object"}),
            "test_server".to_string(),
        );
        assert_eq!(tool.name, "test_tool");
        assert_eq!(tool.server_name, "test_server");
    }

    #[test]
    fn test_mcp_tool_schema() {
        let tool = McpTool::new(
            "search".to_string(),
            "Search for something".to_string(),
            serde_json::json!({"type": "object"}),
            "semble".to_string(),
        );
        let schema = tool.to_tool_schema();
        assert_eq!(schema["name"], "search");
        assert_eq!(schema["type"], "function");
    }

    #[test]
    fn test_mcp_client_default() {
        let client = McpClient::new();
        // Should not panic
        let _ = client;
    }
}
