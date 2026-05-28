use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::RwLock;

use agent_client_protocol::schema::{
    AgentCapabilities, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk,
    Implementation, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PromptCapabilities, PromptRequest, PromptResponse, SessionId, SessionNotification,
    SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Stdio};

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(1);

type BoxPromptFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
type PromptHandler = Arc<dyn Fn(AcpPrompt) -> BoxPromptFuture + Send + Sync>;

#[derive(Clone, Debug)]
pub struct AcpPrompt {
    pub session_id: String,
    pub cwd: PathBuf,
    pub prompt: String,
}

#[derive(Clone, Debug)]
struct SessionState {
    cwd: PathBuf,
}

#[derive(Clone)]
pub struct AcpServer {
    agent_name: String,
    description: String,
    sessions: Arc<RwLock<HashMap<String, SessionState>>>,
    prompt_handler: PromptHandler,
}

impl AcpServer {
    pub fn new<H, F>(
        agent_name: impl Into<String>,
        description: impl Into<String>,
        prompt_handler: H,
    ) -> Self
    where
        H: Fn(AcpPrompt) -> F + Send + Sync + 'static,
        F: Future<Output = Result<String, String>> + Send + 'static,
    {
        Self {
            agent_name: agent_name.into(),
            description: description.into(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            prompt_handler: Arc::new(move |prompt| Box::pin(prompt_handler(prompt))),
        }
    }

    pub async fn serve_stdio(self) -> Result<(), String> {
        let initialize_server = self.clone();
        let new_session_server = self.clone();
        let prompt_server = self.clone();
        let close_session_server = self.clone();

        Agent
            .builder()
            .name(self.agent_name.clone())
            .on_receive_request(
                async move |initialize: InitializeRequest, responder, _connection| {
                    responder.respond(initialize_server.initialize_response(initialize))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: NewSessionRequest, responder, _connection| {
                    let response = new_session_server.new_session(request).await;
                    responder.respond(response)
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: CloseSessionRequest, responder, _connection| {
                    close_session_server.close_session(request).await;
                    responder.respond(CloseSessionResponse::new())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, connection| {
                    let server = prompt_server.clone();
                    let prompt_connection = connection.clone();
                    connection.spawn(async move {
                        server
                            .handle_prompt(request, responder, prompt_connection)
                            .await
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_dispatch(
                async move |message: Dispatch, cx: ConnectionTo<Client>| {
                    message.respond_with_error(
                        agent_client_protocol::util::internal_error("unhandled ACP message"),
                        cx,
                    )
                },
                agent_client_protocol::on_receive_dispatch!(),
            )
            .connect_to(Stdio::new())
            .await
            .map_err(|error| format!("ACP server failed: {error}"))
    }

    fn initialize_response(&self, initialize: InitializeRequest) -> InitializeResponse {
        InitializeResponse::new(initialize.protocol_version)
            .agent_capabilities(
                AgentCapabilities::new()
                    .prompt_capabilities(PromptCapabilities::new().embedded_context(true)),
            )
            .agent_info(
                Implementation::new(self.agent_name.clone(), env!("CARGO_PKG_VERSION"))
                    .title(self.description.clone()),
            )
    }

    async fn new_session(&self, request: NewSessionRequest) -> NewSessionResponse {
        let session_id = new_session_id();
        self.sessions.write().await.insert(
            session_id.to_string(),
            SessionState {
                cwd: absolute_or_current(request.cwd),
            },
        );

        NewSessionResponse::new(session_id)
    }

    async fn close_session(&self, request: CloseSessionRequest) {
        self.sessions
            .write()
            .await
            .remove(&request.session_id.to_string());
    }

    async fn handle_prompt(
        &self,
        request: PromptRequest,
        responder: agent_client_protocol::Responder<PromptResponse>,
        connection: ConnectionTo<Client>,
    ) -> Result<(), agent_client_protocol::Error> {
        let session_id = request.session_id.to_string();
        let Some(session) = self.sessions.read().await.get(&session_id).cloned() else {
            return responder.respond_with_internal_error(format!("unknown session: {session_id}"));
        };

        let prompt = AcpPrompt {
            session_id: session_id.clone(),
            cwd: session.cwd,
            prompt: prompt_to_text(&request.prompt),
        };

        match (self.prompt_handler)(prompt).await {
            Ok(answer) => {
                if !answer.is_empty() {
                    connection.send_notification(SessionNotification::new(
                        request.session_id,
                        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                            TextContent::new(answer),
                        ))),
                    ))?;
                }
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            }
            Err(error) => responder.respond_with_internal_error(error),
        }
    }
}

fn prompt_to_text(prompt: &[ContentBlock]) -> String {
    prompt
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        })
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn absolute_or_current(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

fn new_session_id() -> SessionId {
    let count = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    SessionId::new(format!("nano-{nanos:x}-{count:x}"))
}
