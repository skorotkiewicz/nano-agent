//! Tool schemas, the user approval prompt, shell execution, and dispatch of
//! model tool calls (shell, ACP delegation, MCP).

use crate::input::CancelListen;
use crate::input::{LineKey, RawTerminal, read_line_key};
use crate::policy::{expose_mcp_tools, prepare_shell_execution};
use crate::state::{APPROVE_ALL, APPROVE_SAFE, acp_mode, color, get_mcp_client, sandbox_mode};
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
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, timeout};

const NANO_ACP_ALLOW_DANGER_ENV: &str = "NANO_ACP_ALLOW_DANGER";
const MAX_SHELL_OUTPUT_BYTES: usize = 12_000;

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
    DenyAcpDanger,
    Cancel,
}

/// Enter (empty line) accepts this: safe → [s], write → [y], danger → refuse (must type y).
fn default_approval(risk: &str) -> Approval {
    match risk {
        "safe" => Approval::ApproveSafe,
        "write" => Approval::Approve,
        _ => Approval::Deny,
    }
}

fn approval_from_line(choice: &str, risk: &str) -> Approval {
    match choice.trim().to_ascii_lowercase().as_str() {
        "" => default_approval(risk), // bare Enter / empty line
        "a" | "all" if risk != "danger" => Approval::ApproveAll,
        "s" | "safe" if risk != "danger" => Approval::ApproveSafe,
        "y" | "yes" => Approval::Approve,
        "esc" | "escape" | "cancel" => Approval::Cancel,
        _ => Approval::Deny,
    }
}

fn approval_from_key(key: LineKey, risk: &str) -> Option<Approval> {
    match key {
        LineKey::Enter => Some(default_approval(risk)),
        LineKey::Char('a') | LineKey::Char('A') if risk != "danger" => Some(Approval::ApproveAll),
        LineKey::Char('a') | LineKey::Char('A') => Some(Approval::Deny),
        LineKey::Char('s') | LineKey::Char('S') if risk != "danger" => Some(Approval::ApproveSafe),
        LineKey::Char('s') | LineKey::Char('S') => Some(Approval::Deny),
        LineKey::Char('y') | LineKey::Char('Y') => Some(Approval::Approve),
        LineKey::Char('n') | LineKey::Char('N') => Some(Approval::Deny),
        LineKey::Escape => Some(Approval::Cancel),
        _ => None,
    }
}

fn approve_all_applies(risk: &str) -> bool {
    risk != "danger"
}

fn approve_safe_applies(risk: &str) -> bool {
    risk == "safe"
}

fn acp_allows_risk(risk: &str) -> bool {
    let value = env::var(NANO_ACP_ALLOW_DANGER_ENV).ok();
    acp_allows_risk_with_value(risk, value.as_deref())
}

fn acp_allows_risk_with_value(risk: &str, value: Option<&str>) -> bool {
    risk != "danger"
        || value.is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
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
    let base = command_basename(first);
    matches!(
        base,
        "ls" | "pwd"
            | "cat"
            | "head"
            | "tail"
            | "wc"
            | "rg"
            | "grep"
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
            | "printenv"
            | "tree"
            | "bat"
            | "fd"
            | "jq"
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
        // cargo: source-writing subcommands need explicit approval unless in check mode.
        "cargo" => cargo_safe_subcommand(&tokens),
        "rustfmt" => formatter_safe_args(&tokens),
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

fn cargo_safe_subcommand(tokens: &[&str]) -> bool {
    tokens.get(1).is_some_and(|sub| match *sub {
        "fmt" => tokens.iter().skip(2).any(|token| *token == "--check"),
        "clippy" => !tokens.iter().skip(2).any(|token| *token == "--fix"),
        "test" | "check" | "build" | "tree" | "metadata" | "search" | "info" | "version"
        | "--version" | "help" | "--help" | "doc" | "locate-project" | "pkgid"
        | "verify-project" => true,
        _ => false,
    })
}

fn formatter_safe_args(tokens: &[&str]) -> bool {
    let args = &tokens[1..];
    !args.is_empty()
        && (args.contains(&"--check")
            || args
                .iter()
                .all(|token| matches!(*token, "-h" | "--help" | "-V" | "--version")))
}

fn read_tty_approval(risk: &str) -> io::Result<Approval> {
    let _raw = RawTerminal::enter()?;
    let mut stdin = io::stdin().lock();
    loop {
        if let Some(approval) = approval_from_key(read_line_key(&mut stdin)?, risk) {
            eprintln!();
            return Ok(approval);
        }
    }
}

fn command_risk_label(command: &str) -> (&'static str, &'static str) {
    // (ansi color, short label)
    let lower = command.to_ascii_lowercase();
    if looks_like_delete_command(command) {
        return ("31", "danger");
    }
    if looks_like_file_destroy_command(command) {
        return ("31", "danger");
    }
    if looks_like_destructive_git_command(command) {
        return ("31", "danger");
    }
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

fn looks_like_delete_command(command: &str) -> bool {
    command
        .split([';', '|', '\n', '\r'])
        .flat_map(|part| part.split("&&"))
        .flat_map(|part| part.split("||"))
        .any(segment_looks_like_delete_command)
}

fn segment_looks_like_delete_command(segment: &str) -> bool {
    let tokens = segment.split_whitespace().collect::<Vec<_>>();
    let tokens = sudo_command_tokens(&tokens);
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return false;
    };

    if matches!(first, "rm" | "unlink" | "rmdir") {
        return true;
    }
    if git_command_tokens(tokens).is_some_and(|git_tokens| {
        git_tokens
            .first()
            .is_some_and(|subcommand| subcommand.eq_ignore_ascii_case("rm"))
    }) {
        return true;
    }
    if first == "find" {
        return tokens.contains(&"-delete")
            || tokens
                .windows(2)
                .any(|pair| matches!(pair[0], "-exec" | "-execdir") && is_delete_program(pair[1]));
    }
    if first == "xargs" {
        return tokens.iter().skip(1).any(|token| is_delete_program(token));
    }
    false
}

fn is_delete_program(token: &str) -> bool {
    matches!(command_basename(token), "rm" | "unlink" | "rmdir")
}

fn looks_like_file_destroy_command(command: &str) -> bool {
    command
        .split([';', '|', '\n', '\r'])
        .flat_map(|part| part.split("&&"))
        .flat_map(|part| part.split("||"))
        .any(segment_looks_like_file_destroy_command)
}

fn segment_looks_like_file_destroy_command(segment: &str) -> bool {
    let tokens = segment.split_whitespace().collect::<Vec<_>>();
    let tokens = sudo_command_tokens(&tokens);
    let Some(first) = tokens.first().map(|token| command_basename(token)) else {
        return false;
    };

    match first {
        "dd" => tokens.iter().skip(1).any(|token| token.starts_with("of=")),
        "rsync" => tokens
            .iter()
            .skip(1)
            .any(|token| token.starts_with("--delete")),
        "shred" | "wipe" => true,
        _ => false,
    }
}

fn looks_like_destructive_git_command(command: &str) -> bool {
    command
        .split([';', '|', '\n', '\r'])
        .flat_map(|part| part.split("&&"))
        .flat_map(|part| part.split("||"))
        .any(segment_looks_like_destructive_git_command)
}

fn segment_looks_like_destructive_git_command(segment: &str) -> bool {
    let tokens = segment.split_whitespace().collect::<Vec<_>>();
    let Some(git_tokens) = git_command_tokens(&tokens) else {
        return false;
    };
    let Some(subcommand) = git_tokens.first().copied() else {
        return false;
    };
    let args = &git_tokens[1..];

    match subcommand {
        "restore" | "clean" => true,
        "reset" => args.contains(&"--hard"),
        "checkout" => args.iter().any(|token| matches!(*token, "--" | ".")),
        "stash" => args
            .first()
            .is_some_and(|action| matches!(*action, "drop" | "clear")),
        "branch" | "tag" => args
            .iter()
            .any(|token| matches!(*token, "-d" | "-D" | "--delete")),
        "worktree" => args.first().is_some_and(|action| *action == "remove"),
        "reflog" => args.first().is_some_and(|action| *action == "expire"),
        _ => false,
    }
}

fn git_command_tokens<'a>(tokens: &'a [&'a str]) -> Option<&'a [&'a str]> {
    let tokens = sudo_command_tokens(tokens);
    let mut index = 0;
    if tokens.get(index).map(|token| command_basename(token)) != Some("git") {
        return None;
    }
    index += 1;
    while let Some(token) = tokens.get(index).copied() {
        match token {
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace" => {
                index = (index + 2).min(tokens.len())
            }
            _ if token.starts_with("--git-dir=")
                || token.starts_with("--work-tree=")
                || token.starts_with("--namespace=") =>
            {
                index += 1
            }
            _ => break,
        }
    }
    Some(&tokens[index..])
}

fn sudo_command_tokens<'a>(tokens: &'a [&'a str]) -> &'a [&'a str] {
    if tokens.first().map(|token| command_basename(token)) != Some("sudo") {
        return tokens;
    }

    let mut index = 1;
    while let Some(token) = tokens.get(index).copied() {
        if token == "--" {
            index += 1;
            break;
        }
        if shell_assignment_token(token) {
            index += 1;
            continue;
        }
        if !token.starts_with('-') {
            break;
        }
        index += 1;
        if matches!(
            token,
            "-u" | "--user"
                | "-g"
                | "--group"
                | "-h"
                | "--host"
                | "-p"
                | "--prompt"
                | "-C"
                | "--close-from"
                | "-T"
                | "--command-timeout"
                | "-D"
                | "--chdir"
        ) {
            index = (index + 1).min(tokens.len());
        }
    }
    &tokens[index.min(tokens.len())..]
}

fn shell_assignment_token(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn command_basename(token: &str) -> &str {
    let token = token.trim_matches(|ch| matches!(ch, '\'' | '"' | '`'));
    std::path::Path::new(token)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(token)
}

fn approve_sync(args: &serde_json::Value) -> Approval {
    let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    let description = args
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let (_risk_color, risk_label) = command_risk_label(command);

    eprintln!();
    if !description.is_empty() {
        eprintln!("{} {}", color("32", "#"), color("90", description),);
    }
    eprintln!(
        "{} {}",
        color("31", "$"),
        color("90", command) // :KEEP AS IS: color(risk_color, &format!("[{risk_label}]"))
    );

    for line in approval_detail_lines(args) {
        eprintln!("{}", color("90", &line));
    }

    if APPROVE_ALL.load(Ordering::SeqCst) && approve_all_applies(risk_label) {
        return Approval::Approve;
    }
    if acp_mode() {
        return if acp_allows_risk(risk_label) {
            Approval::Approve
        } else {
            Approval::DenyAcpDanger
        };
    }
    if APPROVE_SAFE.load(Ordering::SeqCst) && approve_safe_applies(risk_label) {
        eprintln!("{}", color("90", "· auto-approved (safe)"));
        return Approval::Approve;
    }

    // Highlight Enter-default: safe → [s], write → [y], danger → no auto (safe refuse)
    let enter_hint = match risk_label {
        "safe" => color("36", "↵=[s]"),
        "write" => color("32", "↵=[y]"),
        _ => color("90", "↵ no (type y)"),
    };
    if risk_label == "danger" {
        eprint!(
            "  {}  {}  {}  {}",
            color("32", "[y]"),
            color("31", "[n]"),
            color("90", "[esc]"),
            enter_hint
        );
    } else {
        eprint!(
            "  {}  {}  {}  {}  {}  {}",
            color("32", "[y]"),
            color("33", "[a]all"),
            color("36", "[s]safe"),
            color("31", "[n]"),
            color("90", "[esc]"),
            enter_hint
        );
    }
    let _ = io::stderr().flush();

    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        return read_tty_approval(risk_label).unwrap_or(Approval::Deny);
    }

    let mut choice = String::new();
    if io::stdin().read_line(&mut choice).is_err() {
        return Approval::Deny;
    }
    approval_from_line(&choice, risk_label)
}

fn approval_detail_lines(args: &serde_json::Value) -> Vec<String> {
    ["cwd", "timeout", "env"]
        .into_iter()
        .filter_map(|key| {
            let value = args.get(key)?;
            if approval_detail_is_empty(key, value) {
                None
            } else {
                Some(format!("{key}: {}", approval_detail_value(key, value)))
            }
        })
        .collect()
}

fn approval_detail_value(key: &str, value: &serde_json::Value) -> serde_json::Value {
    if key != "env" {
        return value.clone();
    }
    let Some(env) = value.as_object() else {
        return value.clone();
    };
    serde_json::Value::Object(
        env.iter()
            .map(|(k, v)| {
                let value = if sensitive_env_key(k) {
                    serde_json::Value::String("[redacted]".to_string())
                } else {
                    v.clone()
                };
                (k.clone(), value)
            })
            .collect(),
    )
}

fn approval_detail_is_empty(key: &str, value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(s) => s.is_empty() || (key == "cwd" && s == "."),
        serde_json::Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn sensitive_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "API_KEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTH",
        "COOKIE",
        "CREDENTIAL",
    ]
    .iter()
    .any(|marker| upper.contains(marker))
}

fn merge_shell_env(
    overrides: Option<&serde_json::Map<String, serde_json::Value>>,
    mode: SandboxMode,
) -> Vec<(String, String)> {
    let mut env_map: std::collections::HashMap<String, String> = env::vars()
        .filter(|(key, _)| mode != SandboxMode::NetOnly || net_only_inherits_env_key(key))
        .collect();
    if mode == SandboxMode::NetOnly {
        env_map.entry("PATH".to_string()).or_insert_with(|| {
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()
        });
    }
    if let Some(overrides) = overrides {
        for (k, v) in overrides {
            if let Some(val) = v.as_str() {
                env_map.insert(k.clone(), val.to_string());
            }
        }
    }
    env_map.into_iter().collect()
}

fn net_only_inherits_env_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.starts_with("LC_")
        || matches!(
            key.as_str(),
            "PATH"
                | "LANG"
                | "LANGUAGE"
                | "TERM"
                | "COLORTERM"
                | "TZ"
                | "HTTP_PROXY"
                | "HTTPS_PROXY"
                | "ALL_PROXY"
                | "NO_PROXY"
                | "SSL_CERT_FILE"
                | "SSL_CERT_DIR"
        )
}

fn shell_timeout_secs(args: &serde_json::Value) -> u64 {
    // ponytail: floor at 1 — Duration::from_secs(0) races the future immediately
    args.get("timeout")
        .and_then(|t| t.as_u64())
        .unwrap_or(60)
        .max(1)
}

fn sandbox_network_hint(mode: SandboxMode, output: &str) -> Option<&'static str> {
    if mode != SandboxMode::Fs {
        return None;
    }
    let lower = output.to_ascii_lowercase();
    [
        "temporary failure in name resolution",
        "could not resolve host",
        "could not resolve hostname",
        "name or service not known",
        "network is unreachable",
        "failed to lookup address information",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    .then_some("hint: default sandbox blocks network; retry with NANO_SANDBOX=fs+net if this command needs network")
}

struct ShellOutput {
    status: std::process::ExitStatus,
    bytes: Vec<u8>,
    truncated: bool,
}

fn push_output_tail(output: &mut Vec<u8>, chunk: &[u8], max: usize) -> bool {
    if max == 0 {
        let truncated = !output.is_empty() || !chunk.is_empty();
        output.clear();
        return truncated;
    }
    if chunk.len() >= max {
        let truncated = !output.is_empty() || chunk.len() > max;
        output.clear();
        output.extend_from_slice(&chunk[chunk.len() - max..]);
        return truncated;
    }

    let overflow = output.len().saturating_add(chunk.len()).saturating_sub(max);
    if overflow > 0 {
        output.drain(..overflow);
    }
    output.extend_from_slice(chunk);
    overflow > 0
}

async fn collect_shell_output(mut child: tokio::process::Child) -> io::Result<ShellOutput> {
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("shell stdout was not piped"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("shell stderr was not piped"))?;
    let read_output = async move {
        let mut output = Vec::new();
        let mut stdout_chunk = [0; 8192];
        let mut stderr_chunk = [0; 8192];
        let mut stdout_done = false;
        let mut stderr_done = false;
        let mut truncated = false;
        while !stdout_done || !stderr_done {
            tokio::select! {
                read = stdout.read(&mut stdout_chunk), if !stdout_done => {
                    let read = read?;
                    stdout_done = read == 0;
                    if read > 0 {
                        truncated |= push_output_tail(
                            &mut output,
                            &stdout_chunk[..read],
                            MAX_SHELL_OUTPUT_BYTES,
                        );
                    }
                }
                read = stderr.read(&mut stderr_chunk), if !stderr_done => {
                    let read = read?;
                    stderr_done = read == 0;
                    if read > 0 {
                        truncated |= push_output_tail(
                            &mut output,
                            &stderr_chunk[..read],
                            MAX_SHELL_OUTPUT_BYTES,
                        );
                    }
                }
            }
        }
        Ok::<_, io::Error>((output, truncated))
    };

    let (status, (bytes, truncated)) = tokio::try_join!(child.wait(), read_output)?;
    Ok(ShellOutput {
        status,
        bytes,
        truncated,
    })
}

fn execution_error(error: &io::Error, sandbox_enabled: bool) -> String {
    let mut message = format!("ExecutionError: {error}");
    if error.kind() == io::ErrorKind::NotFound && sandbox_enabled {
        message.push_str("\nhint: bwrap not found (install bubblewrap) or set NANO_SANDBOX=off");
    }
    message
}

async fn execute_shell(
    args: &serde_json::Value,
    prepared: (PathBuf, PathBuf, bool),
) -> Result<String, ToolCancelled> {
    let command = args.get("command").and_then(|c| c.as_str()).unwrap_or("");
    if command.trim().is_empty() {
        return Ok("bad arguments: command is required".to_string());
    }
    let timeout_secs = shell_timeout_secs(args);
    let env_vars = args.get("env").and_then(|e| e.as_object());

    let (run_cwd, writable_root, force_sandbox) = prepared;

    // NANO_SANDBOX overrides the remembered project mode when explicitly set.
    // Restricted ACP children always keep fs isolation; force_sandbox never drops to Off.
    let mode = if force_sandbox {
        match sandbox_mode() {
            SandboxMode::Off => SandboxMode::Fs,
            other => other,
        }
    } else {
        sandbox_mode()
    };
    let sandbox = Sandbox::with_mode(mode)
        .with_shell("sh")
        .with_cwd(writable_root)
        .restrict_to_cwd(force_sandbox);

    let mut cmd = sandbox.wrap_command(command);
    cmd.current_dir(&run_cwd);
    if mode == SandboxMode::NetOnly {
        cmd.env_clear();
    }
    cmd.envs(merge_shell_env(env_vars, mode));

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());
    // ponytail: kill_on_drop so timeout/cancel reaps the child
    cmd.kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => return Ok(execution_error(&error, sandbox.mode().enabled())),
    };
    let run = collect_shell_output(child);

    let cancel = if acp_mode() {
        None
    } else {
        CancelListen::start()
    };
    let output_result = match cancel {
        Some(cancel) => {
            let timed = timeout(Duration::from_secs(timeout_secs), run);
            tokio::select! {
                res = timed => {
                    drop(cancel);
                    res
                }
                _ = cancel.wait() => {
                    drop(cancel);
                    return Err(ToolCancelled::User);
                }
            }
        }
        None => timeout(Duration::from_secs(timeout_secs), run).await,
    };

    match output_result {
        Ok(Ok(output)) => {
            let mut res = format!(
                "$ {}\nexit {}\n",
                command,
                output.status.code().unwrap_or(-1)
            );
            if output.truncated {
                res.push_str("[…output truncated…]\n");
            }
            let stdout = String::from_utf8_lossy(&output.bytes);
            res.push_str(&stdout);
            if !output.status.success()
                && let Some(hint) = sandbox_network_hint(mode, &res)
            {
                if !res.ends_with('\n') {
                    res.push('\n');
                }
                res.push_str(hint);
                res.push('\n');
            }
            Ok(res)
        }
        Ok(Err(error)) => Ok(execution_error(&error, sandbox.mode().enabled())),
        Err(_) => Ok(format!("$ {}\ntimeout after {}s\n", command, timeout_secs)),
    }
}

fn parse_tool_args(args_str: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(args_str).map_err(|e| format!("bad arguments: {}", e))
}

/// User-typed shell (`!` / `!!`): run through the same sandbox/path as tools, no approval prompt.
pub async fn run_user_shell(command: &str) -> String {
    let command = command.trim();
    if command.is_empty() {
        return "bad arguments: empty command".to_string();
    }
    let args = serde_json::json!({
        "command": command,
        "description": "user shell",
    });
    match prepare_shell_execution(&args) {
        Ok(prepared) => execute_shell(&args, prepared)
            .await
            .unwrap_or_else(|_| format!("$ {command}\ncancelled by user (esc)\n")),
        Err(error) => error,
    }
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
        Approval::Approve => execute_shell(args, prepared).await,
        Approval::ApproveAll => {
            APPROVE_ALL.store(true, Ordering::SeqCst);
            execute_shell(args, prepared).await
        }
        Approval::ApproveSafe => {
            // Approve this command once; subsequent safe-pattern commands auto-pass this turn.
            APPROVE_SAFE.store(true, Ordering::SeqCst);
            execute_shell(args, prepared).await
        }
        Approval::Deny => Ok(color("31", "denied by user")),
        Approval::DenyAcpDanger => Ok(format!(
            "denied: ACP mode will not run [danger] shell commands unless {NANO_ACP_ALLOW_DANGER_ENV}=1"
        )),
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
        Approval, LineKey, acp_allows_risk_with_value, approval_detail_lines, approval_from_key,
        approval_from_line, approve_all_applies, approve_safe_applies, command_risk_label,
        is_safe_command, merge_shell_env, net_only_inherits_env_key, push_output_tail,
        sandbox_network_hint, sensitive_env_key, shell_timeout_secs,
    };
    use nano_agent::sandbox::SandboxMode;

    #[test]
    fn approval_choices_include_cancel() {
        assert_eq!(approval_from_line("yes", "write"), Approval::Approve);
        assert_eq!(approval_from_line("all", "write"), Approval::ApproveAll);
        assert_eq!(approval_from_line("safe", "write"), Approval::ApproveSafe);
        assert_eq!(approval_from_line("cancel", "write"), Approval::Cancel);
        assert_eq!(approval_from_line("nope", "write"), Approval::Deny);
        assert_eq!(approval_from_line("all", "danger"), Approval::Deny);
        assert_eq!(approval_from_line("safe", "danger"), Approval::Deny);
        assert_eq!(approval_from_line("yes", "danger"), Approval::Approve);

        assert_eq!(
            approval_from_key(LineKey::Escape, "safe"),
            Some(Approval::Cancel)
        );
        assert_eq!(
            approval_from_key(LineKey::Char('Y'), "safe"),
            Some(Approval::Approve)
        );
        assert_eq!(
            approval_from_key(LineKey::Char('s'), "write"),
            Some(Approval::ApproveSafe)
        );
        assert_eq!(
            approval_from_key(LineKey::Char('a'), "danger"),
            Some(Approval::Deny)
        );
        assert_eq!(
            approval_from_key(LineKey::Char('s'), "danger"),
            Some(Approval::Deny)
        );
    }

    #[test]
    fn approve_all_never_applies_to_danger() {
        assert!(approve_all_applies("safe"));
        assert!(approve_all_applies("write"));
        assert!(!approve_all_applies("danger"));
    }

    #[test]
    fn approve_safe_only_applies_to_safe() {
        assert!(approve_safe_applies("safe"));
        assert!(!approve_safe_applies("write"));
        assert!(!approve_safe_applies("danger"));
    }

    #[test]
    fn acp_danger_requires_explicit_env_override() {
        assert!(acp_allows_risk_with_value("safe", None));
        assert!(acp_allows_risk_with_value("write", None));
        assert!(!acp_allows_risk_with_value("danger", None));
        assert!(!acp_allows_risk_with_value("danger", Some("")));
        assert!(!acp_allows_risk_with_value("danger", Some("0")));
        assert!(!acp_allows_risk_with_value("danger", Some("false")));
        assert!(!acp_allows_risk_with_value("danger", Some("maybe")));
        assert!(acp_allows_risk_with_value("danger", Some("1")));
        assert!(acp_allows_risk_with_value("danger", Some("yes")));
    }

    #[test]
    fn enter_accepts_risk_suggestion() {
        assert_eq!(
            approval_from_key(LineKey::Enter, "safe"),
            Some(Approval::ApproveSafe)
        );
        assert_eq!(
            approval_from_key(LineKey::Enter, "write"),
            Some(Approval::Approve)
        );
        assert_eq!(
            approval_from_key(LineKey::Enter, "danger"),
            Some(Approval::Deny)
        );
        assert_eq!(approval_from_line("", "safe"), Approval::ApproveSafe);
        assert_eq!(approval_from_line("", "write"), Approval::Approve);
        assert_eq!(approval_from_line("", "danger"), Approval::Deny);
    }

    #[test]
    fn shell_timeout_floors_at_one_second() {
        assert_eq!(shell_timeout_secs(&serde_json::json!({})), 60);
        assert_eq!(shell_timeout_secs(&serde_json::json!({"timeout": 0})), 1);
        assert_eq!(shell_timeout_secs(&serde_json::json!({"timeout": 5})), 5);
    }

    #[test]
    fn shell_output_keeps_a_bounded_tail() {
        let mut output = b"1234".to_vec();
        assert!(push_output_tail(&mut output, b"56789", 6));
        assert_eq!(output, b"456789");

        assert!(push_output_tail(&mut output, b"abcdefgh", 4));
        assert_eq!(output, b"efgh");
    }

    #[test]
    fn merge_shell_env_overrides_existing_keys() {
        let overrides = serde_json::json!({"PATH": "/nano-override-path"});
        let map = overrides.as_object().unwrap();
        let merged = merge_shell_env(Some(map), SandboxMode::Fs);
        let path = merged
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_str());
        assert_eq!(path, Some("/nano-override-path"));
    }

    #[test]
    fn net_only_environment_excludes_credentials() {
        assert!(net_only_inherits_env_key("PATH"));
        assert!(net_only_inherits_env_key("https_proxy"));
        assert!(!net_only_inherits_env_key("OPENAI_API_KEY"));
        assert!(!net_only_inherits_env_key("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn safe_commands_are_narrow() {
        assert!(is_safe_command("ls -la"));
        assert!(is_safe_command("git status"));
        assert!(is_safe_command("cargo test"));
        assert!(is_safe_command("cargo fmt --check"));
        assert!(is_safe_command("rustfmt --check src/lib.rs"));
        assert!(is_safe_command("rg TODO src"));
        assert!(!is_safe_command("env touch PWNED"));
        assert!(!is_safe_command("awk 'BEGIN {system(\"touch PWNED\")}'"));
        assert!(!is_safe_command("git branch new-branch"));
        assert!(!is_safe_command("git remote remove origin"));
        assert!(!is_safe_command("cargo fmt"));
        assert!(!is_safe_command("cargo clippy --fix"));
        assert!(!is_safe_command("rustfmt src/lib.rs"));
        assert!(!is_safe_command("sed -i s/a/b/ file.txt"));
        assert!(!is_safe_command("sed -i.bak s/a/b/ file.txt"));
        assert!(!is_safe_command("awk -i inplace '{print}' file.txt"));
        assert!(!is_safe_command("find . -delete"));
        assert!(!is_safe_command("find . -exec rm {} +"));
        assert!(!is_safe_command("find . -execdir unlink {} +"));
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
        assert_eq!(command_risk_label("cargo fmt").1, "write");
        assert_eq!(command_risk_label("rustfmt src/lib.rs").1, "write");
        assert_eq!(command_risk_label("sed -i s/a/b/ file.txt").1, "write");
        assert_eq!(command_risk_label("find . -delete").1, "danger");
        assert_eq!(command_risk_label("find . -exec rm {} +").1, "danger");
        assert_eq!(
            command_risk_label("find . -execdir unlink {} +").1,
            "danger"
        );
        assert_eq!(command_risk_label("xargs -0 rm").1, "danger");
        // redirect is non-safe; not always DANGER unless matched - '>' is in safe BAD so write
        assert_eq!(command_risk_label("rm file.txt").1, "danger");
        assert_eq!(command_risk_label("/bin/rm file.txt").1, "danger");
        assert_eq!(command_risk_label("unlink file.txt").1, "danger");
        assert_eq!(command_risk_label("rmdir old-dir").1, "danger");
        assert_eq!(command_risk_label("sudo rm file.txt").1, "danger");
        assert_eq!(command_risk_label("sudo -u root rm file.txt").1, "danger");
        assert_eq!(
            command_risk_label("sudo --user root unlink file.txt").1,
            "danger"
        );
        assert_eq!(command_risk_label("sudo FOO=bar rmdir old-dir").1, "danger");
        assert_eq!(command_risk_label("git rm src/lib.rs").1, "danger");
        assert_eq!(command_risk_label("git -C repo rm src/lib.rs").1, "danger");
        assert_eq!(command_risk_label("cd /tmp && rm file.txt").1, "danger");
        assert_eq!(command_risk_label("rm -rf /tmp/x").1, "danger");
        assert_eq!(command_risk_label("dd of=/dev/sda if=image").1, "danger");
        assert_eq!(command_risk_label("rsync --delete src/ dst/").1, "danger");
        assert_eq!(command_risk_label("shred secrets.txt").1, "danger");
        assert_eq!(command_risk_label("sudo shred secrets.txt").1, "danger");
        assert_eq!(command_risk_label("sudo -u").1, "write");
        assert_eq!(command_risk_label("git -C").1, "write");
        assert_eq!(command_risk_label("git reset --hard HEAD").1, "danger");
        assert_eq!(command_risk_label("git restore src/lib.rs").1, "danger");
        assert_eq!(
            command_risk_label("git -C repo restore src/lib.rs").1,
            "danger"
        );
        assert_eq!(
            command_risk_label("git -c core.quotePath=false clean -fd").1,
            "danger"
        );
        assert_eq!(command_risk_label("sudo git clean -fd").1, "danger");
        assert_eq!(command_risk_label("sudo -n git clean -fd").1, "danger");
        assert_eq!(command_risk_label("sudo -u root git clean -fd").1, "danger");
        assert_eq!(
            command_risk_label("sudo --user root git restore src/lib.rs").1,
            "danger"
        );
        assert_eq!(command_risk_label("git checkout -- src/lib.rs").1, "danger");
        assert_eq!(command_risk_label("git checkout .").1, "danger");
        assert_eq!(command_risk_label("git clean -fd").1, "danger");
        assert_eq!(command_risk_label("git stash drop").1, "danger");
        assert_eq!(command_risk_label("git stash clear").1, "danger");
        assert_eq!(command_risk_label("git branch -D old").1, "danger");
        assert_eq!(command_risk_label("git branch --delete old").1, "danger");
        assert_eq!(command_risk_label("git tag -d old").1, "danger");
        assert_eq!(command_risk_label("git tag --delete old").1, "danger");
        assert_eq!(command_risk_label("git worktree remove ../wt").1, "danger");
        assert_eq!(
            command_risk_label("git reflog expire --expire=now --all").1,
            "danger"
        );
    }

    #[test]
    fn approval_details_include_meaningful_cwd() {
        assert!(approval_detail_lines(&serde_json::json!({"cwd": "."})).is_empty());
        assert_eq!(
            approval_detail_lines(&serde_json::json!({
                "cwd": "/tmp/project",
                "timeout": 5,
                "env": {"RUST_LOG": "debug"}
            })),
            vec![
                r#"cwd: "/tmp/project""#.to_string(),
                "timeout: 5".to_string(),
                r#"env: {"RUST_LOG":"debug"}"#.to_string(),
            ]
        );
    }

    #[test]
    fn approval_details_redact_sensitive_env_values() {
        let lines = approval_detail_lines(&serde_json::json!({
            "env": {
                "OPENAI_API_KEY": "sk-secret",
                "GITHUB_TOKEN": "ghp-secret",
                "RUST_LOG": "debug"
            }
        }));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(r#""OPENAI_API_KEY":"[redacted]""#));
        assert!(lines[0].contains(r#""GITHUB_TOKEN":"[redacted]""#));
        assert!(lines[0].contains(r#""RUST_LOG":"debug""#));
        assert!(!lines[0].contains("sk-secret"));
        assert!(!lines[0].contains("ghp-secret"));
        assert!(sensitive_env_key("AWS_SECRET_ACCESS_KEY"));
        assert!(sensitive_env_key("AUTHORIZATION"));
        assert!(!sensitive_env_key("RUST_LOG"));
    }

    #[test]
    fn fs_sandbox_hints_on_network_failures_only() {
        let output = "$ cargo test\nexit 101\nCould not resolve host: index.crates.io\n";
        assert!(sandbox_network_hint(SandboxMode::Fs, output).is_some());
        assert!(sandbox_network_hint(SandboxMode::FsNet, output).is_none());
        assert!(sandbox_network_hint(SandboxMode::NetOnly, output).is_none());
        assert!(sandbox_network_hint(SandboxMode::Off, output).is_none());
        assert!(sandbox_network_hint(SandboxMode::Fs, "exit 1\nsyntax error").is_none());
    }
}
