use crate::acp::AcpAgent;
use crate::config::Config;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AcpClient {
    agents: Arc<HashMap<String, AcpAgent>>,
}

impl AcpClient {
    pub fn new(config: &Config) -> Self {
        let agents = config
            .acp
            .agents
            .iter()
            .map(|(name, agent_config)| (name.clone(), AcpAgent::new(name.clone(), agent_config)))
            .collect();

        Self {
            agents: Arc::new(agents),
        }
    }

    pub fn has_agent(&self, name: &str) -> bool {
        self.agents.contains_key(name)
    }

    pub fn has_agents(&self) -> bool {
        !self.agents.is_empty()
    }

    pub fn list_agents(&self) -> Vec<String> {
        let mut names = self.agents.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    pub async fn delegate_to(&self, agent_name: &str, task: &str) -> Result<String, String> {
        let agent = self
            .agents
            .get(agent_name)
            .ok_or_else(|| format!("Unknown ACP agent: {}", agent_name))?;

        agent.delegate(task).await
    }
}

impl Default for AcpClient {
    fn default() -> Self {
        Self::new(&Config::default())
    }
}
