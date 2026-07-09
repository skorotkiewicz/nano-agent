//! nano-agent: a tiny shell agent for OpenAI-compatible APIs.

#[cfg(feature = "acp")]
pub mod acp;
pub mod config;
pub mod mcp;
pub mod paths;
pub mod sandbox;

#[cfg(feature = "acp")]
pub use acp::{AcpAgentConfig, AcpAgentManager, AcpPrompt, AcpServer, AgentTask, AgentTaskResult};
pub use config::Config;
pub use mcp::{McpClient, McpServerHandle, McpTool};
pub use sandbox::Sandbox;
