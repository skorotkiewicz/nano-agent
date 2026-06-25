//! Tool schemas, the user approval prompt, shell execution, and dispatch of
//! model tool calls (shell, ACP delegation, MCP).

use crate::input::{LineKey, RawTerminal, read_line_key};
use crate::policy::{expose_mcp_tools, prepare_shell_execution};
use crate::state::{APPROVE_ALL, acp_mode, color, env_flag_is_false, get_mcp_client};
#[cfg(feature = "acp")]
use crate::{
    policy::expose_acp_delegate_tools,
    state::{get_acp_manager, is_tty},
};
#[cfg(feature = "acp")]
use nano_agent::acp::AgentTask;
use nano_agent::sandbox::Sandbox;
use std::env;
use std::io::{self, IsTerminal, Write};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, timeout};

#[derive(Debug, Clone, Copy)]
pub struct ToolCancelled;

pub fn get_tool_responses() -> &'static serde_json::Value {
    static TOOL: OnceLock<serde_json::Value> = OnceLock::new();
    TOOL.get_or_init(|| {
        serde_json::json!({
            "type": "function",
            "name": "execute_shell",
            "description": "Run a shell command with inherited environment.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "description": {"type": "string", "description": "Why this command is useful right now, in 5-10 words."},
                    "cwd": {"type": ["string", "null"]},
                    "timeout": {"type": "integer"},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}}
                },
                "required": ["command", "description"],
                "additionalProperties": false
            }
        })
    })
}

pub fn get_tool_chat() -> &'static serde_json::Value {
    static TOOL_CHAT: OnceLock<serde_json::Value> = OnceLock::new();
    TOOL_CHAT.get_or_init(|| {
        serde_json::json!({
            "name": "execute_shell",
            "description": "Run a shell command with inherited environment.",
            "parameters": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to run."},
                    "description": {"type": "string", "description": "Why this command is useful right now, in 5-10 words."},
                    "cwd": {"type": "string", "description": "Working directory."},
                    "timeout": {"type": "integer", "description": "Timeout in seconds."},
                    "env": {"type": "object", "additionalProperties": {"type": "string"}, "description": "Environment variables."}
                },
                "required": ["command", "description"]
            }
        })
    })
}

#[cfg(feature = "acp")]
fn acp_agent_property(agents: &[String]) -> serde_json::Value {
    let mut property = serde_json::json!({
        "type": "string",
        "description": "Configured ACP child agent name."
    });
    if !agents.is_empty() {
        property["enum"] = serde_json::Value::Array(
            agents
                .iter()
                .map(|agent| serde_json::Value::String(agent.clone()))
                .collect(),
        );
    }
    property
}

#[cfg(feature = "acp")]
pub async fn get_acp_delegate_tools_chat() -> Vec<serde_json::Value> {
    let agents = get_acp_manager().list_agents().await;
    if agents.is_empty() {
        return vec![];
    }

    let agent_property = acp_agent_property(&agents);
    vec![
        serde_json::json!({
            "name": "delegate_task",
            "description": "Delegate one task to a configured ACP child agent.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent": agent_property.clone(),
                    "task": {
                        "type": "string",
                        "description": "The complete task to send to the ACP agent."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory for the delegated ACP session."
                    }
                },
                "required": ["agent", "task"],
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "name": "delegate_tasks",
            "description": "Spawn multiple configured ACP child agents concurrently for separate tasks.",
            "parameters": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "agent": agent_property,
                                "task": {
                                    "type": "string",
                                    "description": "The complete task to send to the ACP agent."
                                },
                                "description": {
                                    "type": "string",
                                    "description": "Short human-readable label for this task."
                                },
                                "cwd": {
                                    "type": "string",
                                    "description": "Optional working directory for this delegated ACP session."
                                }
                            },
                            "required": ["agent", "task"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["tasks"],
                "additionalProperties": false
            }
        }),
    ]
}

#[cfg(feature = "acp")]
pub async fn get_acp_delegate_tools_responses() -> Vec<serde_json::Value> {
    get_acp_delegate_tools_chat()
        .await
        .into_iter()
        .map(|mut tool| {
            if let Some(object) = tool.as_object_mut() {
                object.insert(
                    "type".to_string(),
                    serde_json::Value::String("function".to_string()),
                );
            }
            tool
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Approval {
    Approve,
    ApproveAll,
    Deny,
    Cancel,
}

fn approval_from_line(choice: &str) -> Approval {
    match choice.trim().to_ascii_lowercase().as_str() {
        "a" | "all" => Approval::ApproveAll,
        "y" | "yes" => Approval::Approve,
        "esc" | "escape" | "cancel" => Approval::Cancel,
        _ => Approval::Deny,
    }
}

fn approval_from_key(key: LineKey) -> Option<Approval> {
    match key {
        LineKey::Char('a') | LineKey::Char('A') => Some(Approval::ApproveAll),
        LineKey::Char('y') | LineKey::Char('Y') => Some(Approval::Approve),
        LineKey::Char('n') | LineKey::Char('N') | LineKey::Enter => Some(Approval::Deny),
        LineKey::Escape => Some(Approval::Cancel),
        _ => None,
    }
}

fn read_tty_approval() -> io::Result<Approval> {
    let _raw = RawTerminal::enter()?;
    let mut stdin = io::stdin().lock();
    loop {
        if let Some(approval) = approval_from_key(read_line_key(&mut stdin)?) {
            eprintln!();
            return Ok(approval);
        }
    }
}

fn approve_sync(args: &serde_json::Value) -> Approval {
    eprintln!(
        "\n{}",
        color(
            "90",
            &format!(
                "# {}",
                args.get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("No description")
            )
        )
    );
    eprintln!(
        "{}",
        color(
            "32",
            &format!(
                "$ {}",
                args.get("command").and_then(|c| c.as_str()).unwrap_or("")
            )
        )
    );

    for key in &["cwd", "timeout", "env"] {
        let val = args.get(*key);
        if let Some(v) = val
            && v != &serde_json::Value::Null
            && v != &serde_json::Value::String(String::new())
            && v != &serde_json::Value::Object(serde_json::Map::new())
        {
            eprintln!("{}", color("90", &format!("{}: {}", key, v)));
        }
    }

    if APPROVE_ALL.load(Ordering::SeqCst) {
        return Approval::Approve;
    }
    if acp_mode() {
        return Approval::Approve;
    }

    eprint!(
        "Approve? {}  {}  {}  {}: ",
        color("32", "[y] Approve"),
        color("33", "[a] Approve All"),
        color("31", "[n] Deny"),
        color("90", "[Esc] Cancel")
    );
    let _ = io::stderr().flush();

    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        return read_tty_approval().unwrap_or(Approval::Deny);
    }

    let mut choice = String::new();
    if io::stdin().read_line(&mut choice).is_err() {
        return Approval::Deny;
    }
    approval_from_line(&choice)
}

async fn execute_shell(args: &serde_json::Value) -> String {
    let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(60);
    let env_vars = args.get("env").and_then(|e| e.as_object());

    let (run_cwd, writable_root, force_sandbox) = match prepare_shell_execution(args) {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };

    let merged_command = format!("{} 2>&1", command);

    let sandbox_enabled = force_sandbox
        || env::var("NANO_SANDBOX")
            .map(|v| !env_flag_is_false(&v))
            .unwrap_or(true);
    let sandbox = Sandbox::new(sandbox_enabled)
        .with_shell("sh")
        .with_cwd(writable_root)
        .restrict_to_cwd(force_sandbox);

    let mut cmd = sandbox.wrap_command(&merged_command);
    cmd.current_dir(&run_cwd);

    let mut current_env: Vec<(String, String)> = env::vars().collect();
    if let Some(env_map) = env_vars {
        for (k, v) in env_map {
            if let Some(val) = v.as_str() {
                current_env.push((k.clone(), val.to_string()));
            }
        }
    }
    cmd.envs(current_env);

    cmd.stdout(std::process::Stdio::piped());

    let output_result = timeout(Duration::from_secs(timeout_secs), cmd.output()).await;

    match output_result {
        Ok(Ok(output)) => {
            let mut res = format!(
                "$ {}\nexit {}\n",
                command,
                output.status.code().unwrap_or(-1)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            res.push_str(&stdout);
            if res.len() > 12000 {
                res = res[res.len() - 12000..].to_string();
            }
            res
        }
        Ok(Err(e)) => format!("ExecutionError: {}", e),
        Err(_) => format!("$ {}\ntimeout after {}s\n", command, timeout_secs),
    }
}

fn parse_tool_args(args_str: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(args_str).map_err(|e| format!("bad arguments: {}", e))
}

async fn execute_shell_tool(args: &serde_json::Value) -> Result<String, ToolCancelled> {
    let desc = args
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    if desc.trim().is_empty() {
        return Ok("bad arguments: description is required".to_string());
    }
    if let Err(error) = prepare_shell_execution(args) {
        return Ok(error);
    }

    let args_clone = args.clone();
    let approval = tokio::task::spawn_blocking(move || approve_sync(&args_clone))
        .await
        .unwrap_or(Approval::Deny);
    match approval {
        Approval::Approve => Ok(execute_shell(args).await),
        Approval::ApproveAll => {
            APPROVE_ALL.store(true, Ordering::SeqCst);
            Ok(execute_shell(args).await)
        }
        Approval::Deny => Ok(color("31", "denied by user")),
        Approval::Cancel => Err(ToolCancelled),
    }
}

#[cfg(feature = "acp")]
fn acp_tool_task_from_value(index: usize, value: &serde_json::Value) -> Result<AgentTask, String> {
    let task = value
        .get("task")
        .and_then(|task| task.as_str())
        .ok_or_else(|| format!("tasks[{index}].task is required"))?;

    let mut agent_task = AgentTask::new(format!("task_{index}"), task.to_string());
    if let Some(agent) = value.get("agent").and_then(|agent| agent.as_str()) {
        agent_task = agent_task.with_agent(agent.to_string());
    }
    if let Some(description) = value
        .get("description")
        .and_then(|description| description.as_str())
    {
        agent_task.description = description.to_string();
    }
    if let Some(cwd) = value.get("cwd").and_then(|cwd| cwd.as_str()) {
        agent_task = agent_task.with_working_directory(cwd.to_string());
    }

    Ok(agent_task)
}

#[cfg(feature = "acp")]
async fn handle_acp_tool(name: &str, args: &serde_json::Value) -> Option<String> {
    match name {
        "delegate_task" => {
            if !expose_acp_delegate_tools() {
                return Some("denied: ACP delegation tools are disabled".to_string());
            }
            let task = match acp_tool_task_from_value(0, args) {
                Ok(task) => task,
                Err(error) => return Some(format!("bad arguments: {error}")),
            };

            // $$info
            if is_tty() {
                eprintln!(
                    "{}",
                    color("90", &format!("→ delegate_task: {}", task.prompt))
                );
            }
            // $$info /

            Some(
                get_acp_manager()
                    .spawn_agent_for_task(task)
                    .await
                    .map(|result| result.output)
                    .unwrap_or_else(|error| format!("ACP delegation failed: {error}")),
            )
        }
        "delegate_tasks" => {
            if !expose_acp_delegate_tools() {
                return Some("denied: ACP delegation tools are disabled".to_string());
            }
            let values = match args.get("tasks").and_then(|tasks| tasks.as_array()) {
                Some(values) if !values.is_empty() => values,
                _ => return Some("bad arguments: tasks must be a non-empty array".to_string()),
            };

            // $$info
            if is_tty() {
                eprintln!(
                    "{}",
                    color("90", &format!("→ delegate_tasks: {} tasks", values.len()))
                );
            }
            // $$info /

            let mut tasks = Vec::new();
            for (index, value) in values.iter().enumerate() {
                match acp_tool_task_from_value(index, value) {
                    Ok(task) => tasks.push(task),
                    Err(error) => return Some(format!("bad arguments: {error}")),
                }
            }

            let output = get_acp_manager()
                .spawn_agents_for_tasks(tasks)
                .await
                .and_then(|results| {
                    serde_json::to_string_pretty(&results)
                        .map_err(|error| format!("failed to serialize ACP results: {error}"))
                })
                .unwrap_or_else(|error| format!("ACP delegation failed: {error}"));

            Some(output)
        }
        _ => None,
    }
}

pub async fn dispatch_tool_call(name: &str, args_str: &str) -> Result<String, ToolCancelled> {
    let args = match parse_tool_args(args_str) {
        Ok(args) => args,
        Err(e) => return Ok(e),
    };

    #[cfg(feature = "acp")]
    if let Some(result) = handle_acp_tool(name, &args).await {
        return Ok(result);
    }

    if get_mcp_client().has_tool(name).await {
        if !expose_mcp_tools() {
            return Ok("denied: MCP tools are disabled in this restricted ACP child".to_string());
        }
        Ok(get_mcp_client()
            .call_tool(name, args)
            .await
            .unwrap_or_else(|e| e))
    } else if name == "execute_shell" {
        execute_shell_tool(&args).await
    } else {
        Ok("unknown tool".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::{Approval, LineKey, approval_from_key, approval_from_line};

    #[test]
    fn approval_choices_include_cancel() {
        assert_eq!(approval_from_line("yes"), Approval::Approve);
        assert_eq!(approval_from_line("all"), Approval::ApproveAll);
        assert_eq!(approval_from_line("cancel"), Approval::Cancel);
        assert_eq!(approval_from_line("nope"), Approval::Deny);

        assert_eq!(approval_from_key(LineKey::Escape), Some(Approval::Cancel));
        assert_eq!(
            approval_from_key(LineKey::Char('Y')),
            Some(Approval::Approve)
        );
    }
}
