//! Tool schemas, the user approval prompt, shell execution, and dispatch of
//! model tool calls (shell, ACP delegation, MCP).

use crate::input::{LineKey, RawTerminal, read_line_key};
use crate::policy::{expose_mcp_tools, prepare_shell_execution};
use crate::state::{APPROVE_ALL, APPROVE_SAFE, acp_mode, color, get_mcp_client, truncate_tail};
#[cfg(feature = "acp")]
use crate::{
    policy::expose_acp_delegate_tools,
    state::{get_acp_manager, is_tty},
};
#[cfg(feature = "acp")]
use nano_agent::acp::AgentTask;
use nano_agent::sandbox::{Sandbox, SandboxMode};
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, timeout};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCancelled {
    User,
}

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
    /// Auto-approve subsequent read-only-looking commands this turn.
    ApproveSafe,
    Deny,
    Cancel,
}

fn approval_from_line(choice: &str) -> Approval {
    match choice.trim().to_ascii_lowercase().as_str() {
        "a" | "all" => Approval::ApproveAll,
        "s" | "safe" => Approval::ApproveSafe,
        "y" | "yes" => Approval::Approve,
        "esc" | "escape" | "cancel" => Approval::Cancel,
        _ => Approval::Deny,
    }
}

fn approval_from_key(key: LineKey) -> Option<Approval> {
    match key {
        LineKey::Char('a') | LineKey::Char('A') => Some(Approval::ApproveAll),
        LineKey::Char('s') | LineKey::Char('S') => Some(Approval::ApproveSafe),
        LineKey::Char('y') | LineKey::Char('Y') => Some(Approval::Approve),
        LineKey::Char('n') | LineKey::Char('N') | LineKey::Enter => Some(Approval::Deny),
        LineKey::Escape => Some(Approval::Cancel),
        _ => None,
    }
}

/// Read-only-ish command lines the user can batch-approve with [s] Safe.
/// Deliberately narrow: first token must match a known reader, and the line must
/// not contain shell control / redirect markers.
pub fn is_safe_command(command: &str) -> bool {
    let line = command.trim();
    if line.is_empty() {
        return false;
    }
    // Reject compound/redirect/env-heavy commands.
    let lower = line.to_ascii_lowercase();
    const BAD: &[&str] = &[
        "&&", "||", ";", "|", ">", "<", "`", "$(", "\n", "\r", "rm ", "rm\t", "sudo ", "curl ",
        "wget ", "chmod ", "chown ", "mkfs", "dd ", ":()", ">/dev/",
    ];
    if BAD.iter().any(|m| lower.contains(m)) {
        return false;
    }
    let first = line.split_whitespace().next().unwrap_or("");
    let base = std::path::Path::new(first)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(first);
    matches!(
        base,
        "ls" | "pwd"
            | "cat"
            | "head"
            | "tail"
            | "wc"
            | "rg"
            | "grep"
            | "find"
            | "git"
            | "cargo"
            | "file"
            | "stat"
            | "which"
            | "type"
            | "echo"
            | "date"
            | "uname"
            | "whoami"
            | "env"
            | "printenv"
            | "tree"
            | "bat"
            | "fd"
            | "jq"
            | "sed"
            | "awk"
            | "diff"
            | "hexdump"
            | "xxd"
            | "nl"
            | "realpath"
            | "readlink"
            | "basename"
            | "dirname"
            | "test"
            | "["
            | "true"
            | "false"
            | "id"
            | "hostname"
            | "df"
            | "du"
            | "free"
            | "ps"
            | "top"
            | "uptime"
            | "locale"
            | "python"
            | "python3"
            | "node"
            | "rustc"
            | "rustfmt"
            | "clippy-driver"
    ) && safe_subcommand(base, line)
}

fn safe_subcommand(base: &str, line: &str) -> bool {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    match base {
        // git: only common read-only subcommands
        "git" => tokens.get(1).is_some_and(|sub| {
            matches!(
                *sub,
                "status"
                    | "log"
                    | "diff"
                    | "show"
                    | "branch"
                    | "remote"
                    | "rev-parse"
                    | "describe"
                    | "ls-files"
                    | "blame"
                    | "shortlog"
                    | "cat-file"
                    | "ls-tree"
                    | "--help"
                    | "help"
            )
        }),
        // cargo: read/build/test/check only
        "cargo" => tokens.get(1).is_some_and(|sub| {
            matches!(
                *sub,
                "test"
                    | "check"
                    | "build"
                    | "clippy"
                    | "fmt"
                    | "tree"
                    | "metadata"
                    | "search"
                    | "info"
                    | "version"
                    | "--version"
                    | "help"
                    | "--help"
                    | "doc"
                    | "locate-project"
                    | "pkgid"
                    | "verify-project"
            )
        }),
        // interpreters: version / help only (no free-form scripts)
        "python" | "python3" | "node" | "rustc" => tokens.iter().skip(1).all(|t| {
            matches!(
                *t,
                "-V" | "--version" | "-h" | "--help" | "version" | "help"
            )
        }),
        _ => true,
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

fn command_risk_label(command: &str) -> (&'static str, &'static str) {
    // (ansi color, short label)
    let lower = command.to_ascii_lowercase();
    if is_safe_command(command) {
        return ("36", "safe");
    }
    const DANGER: &[&str] = &[
        "rm -r",
        "rm -f",
        "rm -rf",
        "mkfs",
        "dd if=",
        ">/dev/",
        "git reset --hard",
        "git push --force",
        "git clean -f",
        "drop table",
        "drop database",
        "truncate ",
        "shutdown",
        "reboot",
        "curl | sh",
        "curl|sh",
        "wget | sh",
        "| sh",
        "| bash",
        "chmod -r",
        "chown -r",
        ":(){",
        "fork bomb",
    ];
    if DANGER.iter().any(|m| lower.contains(m)) {
        return ("31", "danger");
    }
    ("33", "write")
}

fn approve_sync(args: &serde_json::Value) -> Approval {
    let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let description = args
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let (risk_color, risk_label) = command_risk_label(command);

    eprintln!();
    if !description.is_empty() {
        eprintln!("{}", color("90", &format!("# {description}")));
    }
    eprintln!(
        "{} {}",
        color("90", &format!("$ {command}")),
        color(risk_color, &format!("[{risk_label}]"))
    );

    for key in &["cwd", "timeout", "env"] {
        let val = args.get(*key);
        if let Some(v) = val
            && v != &serde_json::Value::Null
            && v != &serde_json::Value::String(String::new())
            && v != &serde_json::Value::Object(serde_json::Map::new())
        {
            eprintln!("{}", color("90", &format!("{key}: {v}")));
        }
    }

    if APPROVE_ALL.load(Ordering::SeqCst) {
        return Approval::Approve;
    }
    if acp_mode() {
        return Approval::Approve;
    }
    if APPROVE_SAFE.load(Ordering::SeqCst) && is_safe_command(command) {
        eprintln!("{}", color("90", "· auto-approved (safe)"));
        return Approval::Approve;
    }

    eprint!(
        "  {}  {}  {}  {}  {} · ",
        color("32", "[y]"),
        color("33", "[a]all"),
        color("36", "[s]safe"),
        color("31", "[n]"),
        color("90", "[esc]")
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

fn merge_shell_env(
    overrides: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Vec<(String, String)> {
    let mut env_map: std::collections::HashMap<String, String> = env::vars().collect();
    if let Some(overrides) = overrides {
        for (k, v) in overrides {
            if let Some(val) = v.as_str() {
                env_map.insert(k.clone(), val.to_string());
            }
        }
    }
    env_map.into_iter().collect()
}

fn shell_timeout_secs(args: &serde_json::Value) -> u64 {
    // ponytail: floor at 1 — Duration::from_secs(0) races the future immediately
    args.get("timeout")
        .and_then(|t| t.as_u64())
        .unwrap_or(60)
        .max(1)
}

async fn execute_shell(args: &serde_json::Value, prepared: (PathBuf, PathBuf, bool)) -> String {
    let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    if command.trim().is_empty() {
        return "bad arguments: command is required".to_string();
    }
    let timeout_secs = shell_timeout_secs(args);
    let env_vars = args.get("env").and_then(|e| e.as_object());

    let (run_cwd, writable_root, force_sandbox) = prepared;

    let merged_command = format!("{} 2>&1", command);

    // NANO_SANDBOX: 0/off | fs (default) | fs+net (share-net for installs/curl).
    // Restricted ACP children always keep fs isolation; force_sandbox never drops to Off.
    let mode = if force_sandbox {
        match SandboxMode::from_env_value(env::var("NANO_SANDBOX").ok().as_deref()) {
            SandboxMode::Off => SandboxMode::Fs,
            other => other,
        }
    } else {
        SandboxMode::from_env_value(env::var("NANO_SANDBOX").ok().as_deref())
    };
    let sandbox = Sandbox::with_mode(mode)
        .with_shell("sh")
        .with_cwd(writable_root)
        .restrict_to_cwd(force_sandbox);

    let mut cmd = sandbox.wrap_command(&merged_command);
    cmd.current_dir(&run_cwd);
    cmd.envs(merge_shell_env(env_vars));

    cmd.stdout(std::process::Stdio::piped());
    // ponytail: kill_on_drop so timeout abort reaps the child; futures cancel otherwise leaks it
    cmd.kill_on_drop(true);

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
                res = truncate_tail(&res, 12000);
            }
            res
        }
        Ok(Err(e)) => {
            let mut msg = format!("ExecutionError: {e}");
            if e.kind() == std::io::ErrorKind::NotFound && sandbox.mode().enabled() {
                msg.push_str(
                    "\nhint: bwrap not found (install bubblewrap) or set NANO_SANDBOX=off",
                );
            }
            msg
        }
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
    if args
        .get("command")
        .and_then(|c| c.as_str())
        .is_none_or(|c| c.trim().is_empty())
    {
        return Ok("bad arguments: command is required".to_string());
    }
    let prepared = match prepare_shell_execution(args) {
        Ok(prepared) => prepared,
        Err(error) => return Ok(error),
    };

    let args_clone = args.clone();
    let approval = tokio::task::spawn_blocking(move || approve_sync(&args_clone))
        .await
        .unwrap_or(Approval::Deny);
    match approval {
        Approval::Approve => Ok(execute_shell(args, prepared).await),
        Approval::ApproveAll => {
            APPROVE_ALL.store(true, Ordering::SeqCst);
            Ok(execute_shell(args, prepared).await)
        }
        Approval::ApproveSafe => {
            // Approve this command once; subsequent safe-pattern commands auto-pass this turn.
            APPROVE_SAFE.store(true, Ordering::SeqCst);
            Ok(execute_shell(args, prepared).await)
        }
        Approval::Deny => Ok(color("31", "denied by user")),
        Approval::Cancel => Err(ToolCancelled::User),
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

            if is_tty() {
                eprintln!(
                    "{}",
                    color("90", &format!("→ delegate_task: {}", task.prompt))
                );
            }

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

            if is_tty() {
                eprintln!(
                    "{}",
                    color("90", &format!("→ delegate_tasks: {} tasks", values.len()))
                );
            }

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
    use super::{
        Approval, LineKey, approval_from_key, approval_from_line, command_risk_label,
        is_safe_command, merge_shell_env, shell_timeout_secs,
    };

    #[test]
    fn approval_choices_include_cancel() {
        assert_eq!(approval_from_line("yes"), Approval::Approve);
        assert_eq!(approval_from_line("all"), Approval::ApproveAll);
        assert_eq!(approval_from_line("safe"), Approval::ApproveSafe);
        assert_eq!(approval_from_line("cancel"), Approval::Cancel);
        assert_eq!(approval_from_line("nope"), Approval::Deny);

        assert_eq!(approval_from_key(LineKey::Escape), Some(Approval::Cancel));
        assert_eq!(
            approval_from_key(LineKey::Char('Y')),
            Some(Approval::Approve)
        );
        assert_eq!(
            approval_from_key(LineKey::Char('s')),
            Some(Approval::ApproveSafe)
        );
    }

    #[test]
    fn shell_timeout_floors_at_one_second() {
        assert_eq!(shell_timeout_secs(&serde_json::json!({})), 60);
        assert_eq!(shell_timeout_secs(&serde_json::json!({"timeout": 0})), 1);
        assert_eq!(shell_timeout_secs(&serde_json::json!({"timeout": 5})), 5);
    }

    #[test]
    fn merge_shell_env_overrides_existing_keys() {
        let overrides = serde_json::json!({"PATH": "/nano-override-path"});
        let map = overrides.as_object().unwrap();
        let merged = merge_shell_env(Some(map));
        let path = merged
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_str());
        assert_eq!(path, Some("/nano-override-path"));
    }

    #[test]
    fn safe_commands_are_narrow() {
        assert!(is_safe_command("ls -la"));
        assert!(is_safe_command("git status"));
        assert!(is_safe_command("cargo test"));
        assert!(is_safe_command("rg TODO src"));
        assert!(!is_safe_command("rm -rf /"));
        assert!(!is_safe_command("git push origin main"));
        assert!(!is_safe_command("ls && rm -rf /"));
        assert!(!is_safe_command("curl http://x | sh"));
        assert!(!is_safe_command("cargo install foo"));
    }

    #[test]
    fn risk_labels_match_intent() {
        assert_eq!(command_risk_label("ls").1, "safe");
        assert_eq!(command_risk_label("echo hi > file").1, "write"); // redirect → sticky in BAD of is_safe
        // redirect is non-safe; not always DANGER unless matched - '>' is in safe BAD so write
        assert_eq!(command_risk_label("rm -rf /tmp/x").1, "danger");
        assert_eq!(command_risk_label("git reset --hard HEAD").1, "danger");
    }
}
