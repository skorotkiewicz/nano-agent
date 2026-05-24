use crate::acp::{Run, RunCreateRequest, RunMode, RunStatus};
use crate::config::AcpAgentConfig;
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct AcpAgent {
    pub name: String,
    pub agent_name: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub timeout: u64,
    pub headers: HashMap<String, String>,
    client: Client,
}

impl AcpAgent {
    pub fn new(name: String, config: &AcpAgentConfig) -> Self {
        let agent_name = config.agent_name.clone().unwrap_or_else(|| name.clone());
        Self {
            name,
            agent_name,
            endpoint: config.endpoint.clone(),
            api_key: config.api_key.clone(),
            timeout: config.timeout,
            headers: config.headers.clone(),
            client: Client::new(),
        }
    }

    pub async fn delegate(&self, task: &str) -> Result<String, String> {
        let mut request = RunCreateRequest::new_text(self.agent_name.clone(), task.to_string());
        request.mode = RunMode::Sync;

        let mut req = self
            .client
            .post(self.runs_url())
            .json(&request)
            .timeout(Duration::from_secs(self.timeout));

        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        for (name, value) in &self.headers {
            req = req.header(name, value);
        }

        let response = req
            .send()
            .await
            .map_err(|e| format!("ACP request failed for '{}': {}", self.name, e))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| format!("ACP response read failed for '{}': {}", self.name, e))?;

        if !status.is_success() {
            return Err(parse_error_body(&body)
                .unwrap_or_else(|| format!("ACP agent '{}' returned HTTP {}", self.name, status)));
        }

        let run: Run = serde_json::from_str(&body)
            .map_err(|e| format!("ACP response from '{}' was not a Run: {}", self.name, e))?;

        if run.status == RunStatus::Failed {
            let message = run
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "run failed".to_string());
            return Err(message);
        }

        let output = run.output_text();
        if output.is_empty() {
            Ok(format!(
                "run {} ended with status {:?}",
                run.run_id, run.status
            ))
        } else {
            Ok(output)
        }
    }

    fn runs_url(&self) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if base.ends_with("/runs") {
            base.to_string()
        } else {
            format!("{}/runs", base)
        }
    }
}

fn parse_error_body(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| value.get("error").and_then(|e| e.as_str()))
        .map(ToString::to_string)
}
