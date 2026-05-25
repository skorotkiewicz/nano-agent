use serde::{Deserialize, Serialize};
use serde_json::Value;

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
