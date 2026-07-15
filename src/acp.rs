//! ACP (agent-client-protocol) support: serve as a stdio agent and delegate
//! subtasks to configured child agents.

pub use crate::config::AcpAgentConfig;
pub use agent::{AcpAgentManager, AgentTask, AgentTaskResult};
pub use server::{AcpPrompt, AcpServer};

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

mod server {
    use std::collections::HashMap;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::sync::RwLock;

    use agent_client_protocol::schema::{
        AgentCapabilities, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk,
        Implementation, InitializeRequest, InitializeResponse, NewSessionRequest,
        NewSessionResponse, PromptCapabilities, PromptRequest, PromptResponse, SessionId,
        SessionNotification, SessionUpdate, StopReason, TextContent,
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
                return responder
                    .respond_with_internal_error(format!("unknown session: {session_id}"));
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
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(answer)),
                            )),
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
}

// ---------------------------------------------------------------------------
// Agent manager (delegate to child agents)
// ---------------------------------------------------------------------------

mod agent {
    use crate::config::{AcpAgentConfig, Config};
    #[cfg(feature = "acp")]
    use crate::paths::{normalize_path, path_is_inside};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    #[cfg(feature = "acp")]
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tokio::task::JoinSet;
    use tokio::time::{Duration, timeout};

    #[cfg(feature = "acp")]
    use agent_client_protocol::schema::{
        InitializeRequest, PermissionOption, PermissionOptionKind, ProtocolVersion,
        RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
        SelectedPermissionOutcome,
    };
    #[cfg(feature = "acp")]
    use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo};

    #[cfg(feature = "acp")]
    const NANO_ACP_ALLOWED_ROOT_ENV: &str = "NANO_ACP_ALLOWED_ROOT";
    #[cfg(feature = "acp")]
    const NANO_ACP_TOOLS_ENV: &str = "NANO_ACP_TOOLS";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AgentTask {
        pub task_id: String,
        #[serde(default)]
        pub agent: Option<String>,
        #[serde(default)]
        pub description: String,
        pub prompt: String,
        #[serde(default)]
        pub working_directory: Option<String>,
    }

    impl AgentTask {
        pub fn new(task_id: impl Into<String>, prompt: impl Into<String>) -> Self {
            Self {
                task_id: task_id.into(),
                agent: None,
                description: String::new(),
                prompt: prompt.into(),
                working_directory: None,
            }
        }

        pub fn with_agent(mut self, agent: impl Into<String>) -> Self {
            self.agent = Some(agent.into());
            self
        }

        pub fn with_working_directory(mut self, working_directory: impl Into<String>) -> Self {
            self.working_directory = Some(working_directory.into());
            self
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AgentTaskResult {
        pub task_id: String,
        pub agent: String,
        pub output: String,
    }

    /// Manager for spawning and managing multiple ACP agents.
    pub struct AcpAgentManager {
        agent_configs: Arc<RwLock<HashMap<String, AcpAgentConfig>>>,
    }

    impl AcpAgentManager {
        pub fn new() -> Self {
            Self {
                agent_configs: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        pub fn from_config(config: &Config) -> Self {
            Self {
                agent_configs: Arc::new(RwLock::new(config.acp_agents.clone())),
            }
        }

        pub async fn register_agent(
            &self,
            name: impl Into<String>,
            config: AcpAgentConfig,
        ) -> Result<(), String> {
            let mut agents = self.agent_configs.write().await;
            agents.insert(name.into(), config);
            Ok(())
        }

        pub async fn has_agents(&self) -> bool {
            self.agent_configs
                .read()
                .await
                .values()
                .any(|agent| agent.enabled)
        }

        pub async fn list_agents(&self) -> Vec<String> {
            let mut names = self
                .agent_configs
                .read()
                .await
                .iter()
                .filter(|(_, agent)| agent.enabled)
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            names.sort();
            names
        }

        pub async fn spawn_agent_for_task(
            &self,
            task: AgentTask,
        ) -> Result<AgentTaskResult, String> {
            let (agent_name, agent_config) = self.agent_for_task(&task).await?;
            run_agent_task(agent_name, agent_config, task).await
        }

        pub async fn spawn_agents_for_tasks(
            &self,
            tasks: Vec<AgentTask>,
        ) -> Result<Vec<AgentTaskResult>, String> {
            let mut join_set = JoinSet::new();

            for task in tasks {
                let (agent_name, agent_config) = self.agent_for_task(&task).await?;
                join_set.spawn(run_agent_task(agent_name, agent_config, task));
            }

            let mut results = Vec::new();
            while let Some(joined) = join_set.join_next().await {
                match joined {
                    Ok(Ok(result)) => results.push(result),
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(format!("ACP task join failed: {error}")),
                }
            }

            results.sort_by(|left, right| left.task_id.cmp(&right.task_id));
            Ok(results)
        }

        async fn agent_for_task(
            &self,
            task: &AgentTask,
        ) -> Result<(String, AcpAgentConfig), String> {
            let agents = self.agent_configs.read().await;

            if let Some(agent_name) = task.agent.as_deref() {
                let config = agents
                    .get(agent_name)
                    .cloned()
                    .ok_or_else(|| format!("unknown ACP agent: {agent_name}"))?;
                if !config.enabled {
                    return Err(format!("ACP agent '{agent_name}' is disabled"));
                }
                return Ok((agent_name.to_string(), config));
            }

            let mut names = agents
                .iter()
                .filter(|(_, agent)| agent.enabled)
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            names.sort();
            let name = names
                .first()
                .cloned()
                .ok_or_else(|| "no enabled ACP agents configured".to_string())?;
            let config = agents
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("unknown ACP agent: {name}"))?;
            Ok((name, config))
        }
    }

    impl Default for AcpAgentManager {
        fn default() -> Self {
            Self::new()
        }
    }

    #[cfg(test)]
    mod manager_tests {
        use super::*;

        #[tokio::test]
        async fn disabled_agents_are_hidden_and_rejected() {
            let config: Config = serde_json::from_str(
                r#"{
                    "acp_agents": {
                        "active": { "command": "nano-agent" },
                        "paused": { "enabled": false, "command": "nano-agent" }
                    }
                }"#,
            )
            .unwrap();
            let manager = AcpAgentManager::from_config(&config);

            assert!(manager.has_agents().await);
            assert_eq!(manager.list_agents().await, ["active"]);
            assert_eq!(
                manager
                    .agent_for_task(&AgentTask::new("task", "prompt"))
                    .await
                    .unwrap()
                    .0,
                "active"
            );
            let error = manager
                .agent_for_task(&AgentTask::new("task", "prompt").with_agent("paused"))
                .await
                .unwrap_err();
            assert_eq!(error, "ACP agent 'paused' is disabled");
        }

        #[tokio::test]
        async fn all_disabled_agents_behave_as_unconfigured() {
            let manager = AcpAgentManager::new();
            manager
                .register_agent(
                    "paused",
                    AcpAgentConfig {
                        enabled: false,
                        command: "nano-agent".to_string(),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();

            assert!(!manager.has_agents().await);
            assert!(manager.list_agents().await.is_empty());
            let error = manager
                .agent_for_task(&AgentTask::new("task", "prompt"))
                .await
                .unwrap_err();
            assert_eq!(error, "no enabled ACP agents configured");
        }
    }

    #[cfg(feature = "acp")]
    async fn run_agent_task(
        agent_name: String,
        agent_config: AcpAgentConfig,
        task: AgentTask,
    ) -> Result<AgentTaskResult, String> {
        if agent_config.command.trim().is_empty() {
            return Err(format!("ACP agent '{agent_name}' has no command"));
        }

        let tool_policy = AcpToolPolicy::from_config(&agent_name, &agent_config)?;
        let agent = build_acp_agent(&agent_name, &agent_config, &tool_policy)?;
        let cwd = task_cwd(&agent_config, &task, &tool_policy)?;
        let prompt = if task.description.trim().is_empty() {
            task.prompt.clone()
        } else {
            format!("{}\n\n{}", task.description.trim(), task.prompt)
        };
        let task_id = task.task_id.clone();
        let timeout_secs = agent_config.timeout_secs.max(1);
        let permission_policy = tool_policy.clone();
        let permission_cwd = cwd.clone();

        let run = Client
            .builder()
            .name(format!("nano-child-{agent_name}"))
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _connection| {
                    responder.respond(permission_response(
                        &permission_policy,
                        &permission_cwd,
                        &request,
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                connection
                    .build_session(cwd)
                    .block_task()
                    .run_until(async |mut session| {
                        session.send_prompt(prompt)?;
                        session.read_to_string().await
                    })
                    .await
            });

        let output = timeout(Duration::from_secs(timeout_secs), run)
            .await
            .map_err(|_| format!("ACP agent '{agent_name}' timed out after {timeout_secs}s"))?
            .map_err(|error| format!("ACP agent '{agent_name}' failed: {error}"))?;

        Ok(AgentTaskResult {
            task_id,
            agent: agent_name,
            output,
        })
    }

    #[cfg(not(feature = "acp"))]
    async fn run_agent_task(
        agent_name: String,
        _agent_config: AcpAgentConfig,
        task: AgentTask,
    ) -> Result<AgentTaskResult, String> {
        let _ = task;
        Err(format!(
            "ACP feature not enabled; cannot spawn ACP agent '{agent_name}'"
        ))
    }

    #[cfg(feature = "acp")]
    fn build_acp_agent(
        agent_name: &str,
        agent_config: &AcpAgentConfig,
        tool_policy: &AcpToolPolicy,
    ) -> Result<AcpAgent, String> {
        let mut env = agent_config.env.clone();
        match &tool_policy.allowed_root {
            Some(root) => {
                env.insert(NANO_ACP_TOOLS_ENV.to_string(), "1".to_string());
                env.insert(
                    NANO_ACP_ALLOWED_ROOT_ENV.to_string(),
                    root.display().to_string(),
                );
            }
            None => {
                env.insert(NANO_ACP_TOOLS_ENV.to_string(), "0".to_string());
                env.remove(NANO_ACP_ALLOWED_ROOT_ENV);
            }
        }

        let mut args = env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>();
        args.sort();
        args.push(agent_config.command.clone());
        args.extend(agent_config.args.clone());

        AcpAgent::from_args(args)
            .map_err(|error| format!("invalid ACP agent command for '{agent_name}': {error}"))
    }

    #[cfg(feature = "acp")]
    #[derive(Clone, Debug)]
    struct AcpToolPolicy {
        allowed_root: Option<PathBuf>,
    }

    #[cfg(feature = "acp")]
    impl AcpToolPolicy {
        fn from_config(agent_name: &str, agent_config: &AcpAgentConfig) -> Result<Self, String> {
            let Some(root) = agent_config
                .working_directory
                .as_deref()
                .map(str::trim)
                .filter(|root| !root.is_empty())
            else {
                return Ok(Self { allowed_root: None });
            };

            let allowed_root = resolve_existing_dir(root, None).map_err(|error| {
                format!("invalid working_directory for ACP agent '{agent_name}': {error}")
            })?;
            Ok(Self {
                allowed_root: Some(allowed_root),
            })
        }

        fn tools_allowed(&self) -> bool {
            self.allowed_root.is_some()
        }
    }

    #[cfg(feature = "acp")]
    fn task_cwd(
        _agent_config: &AcpAgentConfig,
        task: &AgentTask,
        tool_policy: &AcpToolPolicy,
    ) -> Result<PathBuf, String> {
        let Some(root) = &tool_policy.allowed_root else {
            return task
                .working_directory
                .as_deref()
                .map(str::trim)
                .filter(|cwd| !cwd.is_empty())
                .map(|cwd| resolve_existing_dir(cwd, None))
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map_err(|error| format!("failed to resolve ACP task cwd: {error}"))
                });
        };

        let cwd = task
            .working_directory
            .as_deref()
            .map(str::trim)
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| resolve_existing_dir(cwd, Some(root)))
            .unwrap_or_else(|| Ok(root.clone()))?;

        if path_is_inside(root, &cwd) {
            Ok(cwd)
        } else {
            Err(format!(
                "ACP task cwd '{}' is outside configured working_directory '{}'",
                cwd.display(),
                root.display()
            ))
        }
    }

    #[cfg(feature = "acp")]
    fn permission_response(
        tool_policy: &AcpToolPolicy,
        session_cwd: &Path,
        request: &RequestPermissionRequest,
    ) -> RequestPermissionResponse {
        let allowed = tool_policy
            .allowed_root
            .as_deref()
            .is_some_and(|root| permission_request_inside_root(root, session_cwd, request));

        let allow = allowed
            .then(|| allow_option(&request.options))
            .flatten()
            .filter(|_| tool_policy.tools_allowed());

        match allow {
            Some(option) => RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
                SelectedPermissionOutcome::new(option.option_id.clone()),
            )),
            None => RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled),
        }
    }

    #[cfg(feature = "acp")]
    fn allow_option(options: &[PermissionOption]) -> Option<&PermissionOption> {
        options
            .iter()
            .find(|option| matches!(option.kind, PermissionOptionKind::AllowOnce))
            .or_else(|| {
                options
                    .iter()
                    .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
            })
    }

    #[cfg(feature = "acp")]
    fn permission_request_inside_root(
        root: &Path,
        session_cwd: &Path,
        request: &RequestPermissionRequest,
    ) -> bool {
        if !path_is_inside(root, session_cwd) {
            return false;
        }

        request
            .tool_call
            .fields
            .locations
            .as_ref()
            .map(|locations| {
                locations.iter().all(|location| {
                    let path = absolutize_path(&location.path, session_cwd);
                    path_is_inside(root, &path)
                })
            })
            .unwrap_or(true)
    }

    #[cfg(feature = "acp")]
    fn resolve_existing_dir(path: &str, relative_base: Option<&Path>) -> Result<PathBuf, String> {
        let path = PathBuf::from(path);
        let absolute = if path.is_absolute() {
            path
        } else if let Some(base) = relative_base {
            base.join(path)
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };

        let canonical = std::fs::canonicalize(&absolute)
            .map_err(|error| format!("'{}': {error}", absolute.display()))?;
        if !canonical.is_dir() {
            return Err(format!("'{}' is not a directory", canonical.display()));
        }
        Ok(canonical)
    }

    #[cfg(feature = "acp")]
    fn absolutize_path(path: &Path, base: &Path) -> PathBuf {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        };

        std::fs::canonicalize(&absolute).unwrap_or_else(|_| normalize_path(&absolute))
    }

    #[cfg(all(test, feature = "acp"))]
    mod tests {
        use super::*;

        fn temp_dir(name: &str) -> PathBuf {
            let path =
                std::env::temp_dir().join(format!("nano-agent-acp-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            std::fs::canonicalize(path).unwrap()
        }

        #[test]
        fn configured_working_directory_sets_task_root() {
            let root = temp_dir("task-root");
            let config = AcpAgentConfig {
                command: "nano-agent".to_string(),
                working_directory: Some(root.display().to_string()),
                ..Default::default()
            };
            let policy = AcpToolPolicy::from_config("coder", &config).unwrap();
            let cwd = task_cwd(&config, &AgentTask::new("task", "prompt"), &policy).unwrap();

            assert_eq!(cwd, root);
        }

        #[test]
        fn task_working_directory_must_stay_inside_configured_root() {
            let root = temp_dir("inside-root");
            let outside = temp_dir("outside-root");
            let config = AcpAgentConfig {
                command: "nano-agent".to_string(),
                working_directory: Some(root.display().to_string()),
                ..Default::default()
            };
            let policy = AcpToolPolicy::from_config("coder", &config).unwrap();
            let task = AgentTask::new("task", "prompt")
                .with_working_directory(outside.display().to_string());

            let error = task_cwd(&config, &task, &policy).unwrap_err();
            assert!(error.contains("outside configured working_directory"));
        }

        #[test]
        fn missing_working_directory_disables_child_tool_approval() {
            let config = AcpAgentConfig {
                command: "nano-agent".to_string(),
                ..Default::default()
            };
            let policy = AcpToolPolicy::from_config("coder", &config).unwrap();

            assert!(!policy.tools_allowed());
        }
    }
}
