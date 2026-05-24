use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_content_type() -> String {
    "text/plain".to_string()
}

fn default_content_encoding() -> String {
    "plain".to_string()
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessagePart {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default = "default_content_type")]
    pub content_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default = "default_content_encoding")]
    pub content_encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl MessagePart {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            name: None,
            content_type: default_content_type(),
            content: Some(content.into()),
            content_encoding: default_content_encoding(),
            content_url: None,
            metadata: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: String,
    pub parts: Vec<MessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

impl Message {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            parts: vec![MessagePart::text(content)],
            created_at: Some(now_rfc3339()),
            completed_at: None,
        }
    }

    pub fn text_content(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| part.content.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    #[default]
    Sync,
    #[serde(rename = "async")]
    Async,
    Stream,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RunStatus {
    #[serde(rename = "created")]
    Created,
    #[serde(rename = "in-progress")]
    InProgress,
    #[serde(rename = "awaiting")]
    Awaiting,
    #[serde(rename = "cancelling")]
    Cancelling,
    #[serde(rename = "cancelled")]
    Cancelled,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl AcpError {
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_input".to_string(),
            message: message.into(),
            data: None,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            code: "not_found".to_string(),
            message: message.into(),
            data: None,
        }
    }

    pub fn server_error(message: impl Into<String>) -> Self {
        Self {
            code: "server_error".to_string(),
            message: message.into(),
            data: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpSession {
    pub id: String,
    #[serde(default)]
    pub history: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunCreateRequest {
    pub agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<AcpSession>,
    pub input: Vec<Message>,
    #[serde(default)]
    pub mode: RunMode,
}

impl RunCreateRequest {
    pub fn new_text(agent_name: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            agent_name: agent_name.into(),
            session_id: None,
            session: None,
            input: vec![Message::text("user", input)],
            mode: RunMode::Sync,
        }
    }

    pub fn prompt_text(&self) -> String {
        self.input
            .iter()
            .map(Message::text_content)
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunResumeRequest {
    pub run_id: String,
    pub await_resume: Value,
    #[serde(default)]
    pub mode: RunMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Run {
    pub agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub run_id: String,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub await_request: Option<Value>,
    #[serde(default)]
    pub output: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AcpError>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

impl Run {
    pub fn new(agent_name: impl Into<String>, run_id: impl Into<String>) -> Self {
        Self {
            agent_name: agent_name.into(),
            session_id: None,
            run_id: run_id.into(),
            status: RunStatus::Created,
            await_request: None,
            output: Vec::new(),
            error: None,
            created_at: now_rfc3339(),
            finished_at: None,
        }
    }

    pub fn complete(&mut self, output: impl Into<String>) {
        self.status = RunStatus::Completed;
        self.output = vec![Message::text(
            format!("agent/{}", self.agent_name),
            output.into(),
        )];
        self.finished_at = Some(now_rfc3339());
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        self.status = RunStatus::Failed;
        self.error = Some(AcpError::server_error(error));
        self.finished_at = Some(now_rfc3339());
    }

    pub fn output_text(&self) -> String {
        self.output
            .iter()
            .map(Message::text_content)
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentManifest {
    pub name: String,
    pub description: String,
    pub input_content_types: Vec<String>,
    pub output_content_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Value>,
}

impl AgentManifest {
    pub fn nano(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_content_types: vec!["text/plain".to_string()],
            output_content_types: vec!["text/plain".to_string()],
            metadata: Some(serde_json::json!({
                "framework": "nano-agent",
                "programming_language": "Rust",
                "capabilities": [{
                    "name": "Shell agent",
                    "description": "Can inspect and modify a local workspace through approved shell commands."
                }]
            })),
            status: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentsListResponse {
    pub agents: Vec<AgentManifest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunEventsListResponse {
    pub events: Vec<AcpEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcpEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part: Option<MessagePart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generic: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AcpError>,
}

impl AcpEvent {
    pub fn run(event_type: impl Into<String>, run: Run) -> Self {
        Self {
            event_type: event_type.into(),
            run: Some(run),
            message: None,
            part: None,
            generic: None,
            error: None,
        }
    }

    pub fn message(event_type: impl Into<String>, message: Message) -> Self {
        Self {
            event_type: event_type.into(),
            run: None,
            message: Some(message),
            part: None,
            generic: None,
            error: None,
        }
    }

    pub fn error(error: AcpError) -> Self {
        Self {
            event_type: "error".to_string(),
            run: None,
            message: None,
            part: None,
            generic: None,
            error: Some(error),
        }
    }
}
