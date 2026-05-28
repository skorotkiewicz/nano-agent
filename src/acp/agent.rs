use crate::config::{AcpAgentConfig, Config};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};

#[cfg(feature = "acp")]
use agent_client_protocol::schema::{
    InitializeRequest, PermissionOptionKind, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
};
#[cfg(feature = "acp")]
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo};

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
        !self.agent_configs.read().await.is_empty()
    }

    pub async fn list_agents(&self) -> Vec<String> {
        let mut names = self
            .agent_configs
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub async fn spawn_agent_for_task(&self, task: AgentTask) -> Result<AgentTaskResult, String> {
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

    async fn agent_for_task(&self, task: &AgentTask) -> Result<(String, AcpAgentConfig), String> {
        let agents = self.agent_configs.read().await;
        if agents.is_empty() {
            return Err("no ACP agents configured".to_string());
        }

        if let Some(agent_name) = task.agent.as_deref() {
            let config = agents
                .get(agent_name)
                .cloned()
                .ok_or_else(|| format!("unknown ACP agent: {agent_name}"))?;
            return Ok((agent_name.to_string(), config));
        }

        let mut names = agents.keys().cloned().collect::<Vec<_>>();
        names.sort();
        let name = names
            .first()
            .cloned()
            .ok_or_else(|| "no ACP agents configured".to_string())?;
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

#[cfg(feature = "acp")]
async fn run_agent_task(
    agent_name: String,
    agent_config: AcpAgentConfig,
    task: AgentTask,
) -> Result<AgentTaskResult, String> {
    if agent_config.command.trim().is_empty() {
        return Err(format!("ACP agent '{agent_name}' has no command"));
    }

    let agent = build_acp_agent(&agent_name, &agent_config)?;
    let cwd = task_cwd(&agent_config, &task)?;
    let prompt = if task.description.trim().is_empty() {
        task.prompt.clone()
    } else {
        format!("{}\n\n{}", task.description.trim(), task.prompt)
    };
    let task_id = task.task_id.clone();
    let timeout_secs = agent_config.timeout_secs.max(1);

    let run = Client
        .builder()
        .name(format!("nano-child-{agent_name}"))
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                let allow = request
                    .options
                    .iter()
                    .find(|option| matches!(option.kind, PermissionOptionKind::AllowOnce))
                    .or_else(|| {
                        request
                            .options
                            .iter()
                            .find(|option| matches!(option.kind, PermissionOptionKind::AllowAlways))
                    })
                    .or_else(|| request.options.first());

                match allow {
                    Some(option) => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option.option_id.clone(),
                        )),
                    )),
                    None => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    )),
                }
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
fn build_acp_agent(agent_name: &str, agent_config: &AcpAgentConfig) -> Result<AcpAgent, String> {
    let mut args = agent_config
        .env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    args.sort();
    args.push(agent_config.command.clone());
    args.extend(agent_config.args.clone());

    AcpAgent::from_args(args)
        .map_err(|error| format!("invalid ACP agent command for '{agent_name}': {error}"))
}

fn task_cwd(agent_config: &AcpAgentConfig, task: &AgentTask) -> Result<PathBuf, String> {
    let cwd = task
        .working_directory
        .as_deref()
        .or(agent_config.working_directory.as_deref())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if cwd.is_absolute() {
        Ok(cwd)
    } else {
        std::env::current_dir()
            .map(|base| base.join(cwd))
            .map_err(|error| format!("failed to resolve ACP task cwd: {error}"))
    }
}
