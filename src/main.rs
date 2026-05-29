mod input;

use dirs::home_dir;
use input::read_repl_input;
#[cfg(feature = "acp")]
use nano_agent::acp::{AcpAgentManager, AcpPrompt, AcpServer, AgentTask};
use nano_agent::{config::Config, mcp::McpClient, sandbox::Sandbox};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::time::{Duration, timeout};

// --- Constants & Globals ---
const SKIP_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "__pycache__",
    "node_modules",
    "venv",
    "target",
];
const NANO_ACP_ALLOWED_ROOT_ENV: &str = "NANO_ACP_ALLOWED_ROOT";
const NANO_ACP_TOOLS_ENV: &str = "NANO_ACP_TOOLS";

static IS_TTY: OnceLock<bool> = OnceLock::new();
static APPROVE_ALL: AtomicBool = AtomicBool::new(false);
static ACP_MODE: AtomicBool = AtomicBool::new(false);
static CONFIG: OnceLock<Config> = OnceLock::new();
static MODEL: OnceLock<String> = OnceLock::new();
static MAX_STEPS: OnceLock<usize> = OnceLock::new();
static SESSIONS_PATH: OnceLock<PathBuf> = OnceLock::new();
static SYSTEM: OnceLock<String> = OnceLock::new();
static MCP_CLIENT: OnceLock<McpClient> = OnceLock::new();
#[cfg(feature = "acp")]
static ACP_MANAGER: OnceLock<AcpAgentManager> = OnceLock::new();

tokio::task_local! {
    static ACP_SESSION_CWD: PathBuf;
}

fn get_config() -> &'static Config {
    CONFIG.get_or_init(Config::load)
}

fn get_mcp_client() -> &'static McpClient {
    MCP_CLIENT.get_or_init(McpClient::new)
}

#[cfg(feature = "acp")]
fn get_acp_manager() -> &'static AcpAgentManager {
    ACP_MANAGER.get_or_init(|| AcpAgentManager::from_config(get_config()))
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ApiFormat {
    Responses,
    ChatCompletions,
}

#[derive(Clone, Debug)]
struct ApiTarget {
    url: String,
    format: ApiFormat,
    api_key: String,
    model: String,
}

fn is_tty() -> bool {
    *IS_TTY.get_or_init(|| io::stderr().is_terminal())
}

fn get_model() -> &'static str {
    if let Some(model) = get_config().get_model() {
        return model;
    }
    MODEL.get_or_init(|| env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.5".to_string()))
}

fn get_max_steps() -> usize {
    *MAX_STEPS.get_or_init(|| {
        env::var("NANO_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200)
    })
}

fn sessions_path() -> &'static PathBuf {
    SESSIONS_PATH.get_or_init(|| home_dir().unwrap_or_default().join(".nano_sessions.json"))
}

fn check_api_key() {
    let (_, _, key) = get_api_config();
    let env_key = env::var("OPENAI_API_KEY").unwrap_or_default();
    if key.is_empty() && env_key.is_empty() {
        eprintln!("set OPENAI_API_KEY or configure a provider in config.json");
        std::process::exit(1);
    }
}

fn get_api_config() -> (String, ApiFormat, String) {
    let target = get_api_target();
    (target.url, target.format, target.api_key)
}

fn custom_provider_target(provider_name: &str, model: String) -> Option<ApiTarget> {
    let custom = get_config().get_custom_provider(provider_name)?;
    let base = custom.base_url.trim_end_matches('/');
    Some(ApiTarget {
        url: format!("{}/chat/completions", base),
        format: ApiFormat::ChatCompletions,
        api_key: custom.api_key.clone().unwrap_or_default(),
        model,
    })
}

fn get_api_target() -> ApiTarget {
    if let Some(provider_name) = get_config().get_provider()
        && let Some(target) = custom_provider_target(provider_name, get_model().to_string())
    {
        return target;
    }

    if let Ok(base) = env::var("OPENAI_BASE_URL") {
        let base = base.trim_end_matches('/');
        ApiTarget {
            url: format!("{}/chat/completions", base),
            format: ApiFormat::ChatCompletions,
            api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            model: get_model().to_string(),
        }
    } else {
        ApiTarget {
            url: "https://api.openai.com/v1/responses".to_string(),
            format: ApiFormat::Responses,
            api_key: env::var("OPENAI_API_KEY").unwrap_or_default(),
            model: get_model().to_string(),
        }
    }
}

fn get_mito_target() -> Result<ApiTarget, String> {
    let mito = get_config().get_mito_mode();
    if !mito.enabled {
        return Err("mito mode is not enabled in config".to_string());
    }

    let provider = mito
        .provider
        .as_deref()
        .ok_or_else(|| "mito-mode.provider is not configured".to_string())?;
    let model = mito.model.as_deref().unwrap_or(provider).to_string();
    custom_provider_target(provider, model)
        .ok_or_else(|| format!("mito-mode.provider '{provider}' is not in custom_providers"))
}

fn color(code: &str, text: &str) -> String {
    if is_tty() {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

// --- Session Management ---
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Session {
    id: String,
    label: String,
    cwd: String,
    ts: i64,
    #[serde(default)]
    messages: Option<Vec<serde_json::Value>>,
}

fn load_sessions() -> Vec<Session> {
    let path = sessions_path();
    if path.exists() {
        let data = std::fs::read_to_string(path).unwrap_or_default();
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        vec![]
    }
}

fn save_session(response_id: &str, label: &str, messages: Option<Vec<serde_json::Value>>) {
    let mut sessions = load_sessions();
    let cwd = env::current_dir()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("")
        .to_string();

    sessions.retain(|s| !(s.label == label && s.cwd == cwd));

    sessions.push(Session {
        id: response_id.to_string(),
        label: label.chars().take(80).collect(),
        cwd: cwd.clone(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        messages,
    });

    if sessions.len() > 50 {
        sessions = sessions[sessions.len() - 50..].to_vec();
    }

    if let Ok(data) = serde_json::to_string_pretty(&sessions) {
        let _ = std::fs::write(sessions_path(), data);
    }
}

fn pick_session() -> Option<Session> {
    let cwd = env::current_dir()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("")
        .to_string();
    let sessions: Vec<Session> = load_sessions()
        .into_iter()
        .filter(|s| s.cwd == cwd)
        .collect();

    if sessions.is_empty() {
        eprintln!("no sessions in this directory");
        std::process::exit(1);
    }

    let recent: Vec<&Session> = sessions.iter().rev().take(10).collect();
    for (i, s) in recent.iter().enumerate() {
        let age = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - s.ts;
        let label = if age < 3600 {
            format!("{}m", age / 60)
        } else if age < 86400 {
            format!("{}h", age / 3600)
        } else {
            format!("{}d", age / 86400)
        };
        eprintln!(
            "  {}  {}  {}",
            color("90", &i.to_string()),
            s.label,
            color("90", &format!("{} ago", label))
        );
    }

    eprint!("{}{} ", color("1", "nano"), color("90", "#"));
    let mut choice = String::new();
    if io::stdin().read_line(&mut choice).is_err() {
        std::process::exit(0);
    }

    match choice.trim().parse::<usize>() {
        Ok(i) if i < recent.len() => Some(recent[i].clone()),
        _ => {
            eprintln!("invalid session");
            std::process::exit(1);
        }
    }
}

// --- File Finder ---
fn find_files(roots: Vec<String>, names: Vec<&str>, limit: usize) -> String {
    let home = home_dir().unwrap_or_default();
    let mut found = Vec::new();

    for root in roots {
        let root_path = if root.starts_with('~') {
            home.join(&root[2..])
        } else {
            PathBuf::from(&root)
        };

        if !root_path.is_dir() {
            continue;
        }

        for entry in walkdir::WalkDir::new(&root_path)
            .into_iter()
            .filter_entry(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| !SKIP_DIRS.contains(&s))
                    .unwrap_or(true)
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let file_name = entry.file_name().to_str().unwrap_or("").to_lowercase();
            if names.iter().any(|n| file_name == *n) {
                let path = entry.path().to_path_buf();
                let path_str = if path.starts_with(&home) {
                    format!(
                        "~/{}",
                        path.strip_prefix(&home).unwrap().to_str().unwrap_or("")
                    )
                } else {
                    path.to_str().unwrap_or("").to_string()
                };
                found.push(path_str);
                if found.len() >= limit {
                    found.sort();
                    found.dedup();
                    return found.join(", ");
                }
            }
        }
    }

    found.sort();
    found.dedup();
    if found.is_empty() {
        "none".to_string()
    } else {
        found.join(", ")
    }
}

fn env_flag_is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn acp_spawn_policy_active() -> bool {
    ACP_MODE.load(Ordering::SeqCst)
        && (env::var_os(NANO_ACP_TOOLS_ENV).is_some()
            || env::var_os(NANO_ACP_ALLOWED_ROOT_ENV).is_some())
}

fn acp_tools_enabled() -> bool {
    if !ACP_MODE.load(Ordering::SeqCst) {
        return true;
    }

    env::var(NANO_ACP_TOOLS_ENV)
        .map(|value| !env_flag_is_false(&value))
        .unwrap_or(true)
}

fn expose_execute_shell_tools() -> bool {
    acp_tools_enabled()
}

#[cfg(feature = "acp")]
fn expose_acp_delegate_tools() -> bool {
    acp_tools_enabled()
}

fn expose_mcp_tools() -> bool {
    !acp_spawn_policy_active()
}

fn context_cwd() -> PathBuf {
    if ACP_MODE.load(Ordering::SeqCst)
        && let Ok(cwd) = ACP_SESSION_CWD.try_with(Clone::clone)
    {
        return cwd;
    }

    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn configured_acp_root() -> Result<Option<PathBuf>, String> {
    if !ACP_MODE.load(Ordering::SeqCst) {
        return Ok(None);
    }

    let Some(root) = env::var_os(NANO_ACP_ALLOWED_ROOT_ENV) else {
        return Ok(None);
    };
    let root = root.to_string_lossy();
    let root = root.trim();
    if root.is_empty() {
        return Err(format!("{NANO_ACP_ALLOWED_ROOT_ENV} is empty"));
    }

    let root = PathBuf::from(root);
    let root = if root.is_absolute() {
        root
    } else {
        env::current_dir()
            .map_err(|error| format!("failed to resolve ACP working directory: {error}"))?
            .join(root)
    };
    let root =
        std::fs::canonicalize(&root).map_err(|error| format!("'{}': {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("'{}' is not a directory", root.display()));
    }
    Ok(Some(root))
}

fn acp_allowed_root() -> Option<PathBuf> {
    configured_acp_root().ok().flatten()
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn path_is_inside(root: &Path, path: &Path) -> bool {
    let root = normalize_path(root);
    let path = normalize_path(path);
    path == root || path.starts_with(root)
}

fn shell_cwd_from_args(args: &serde_json::Value) -> Result<PathBuf, String> {
    let cwd = args.get("cwd").and_then(|c| c.as_str()).unwrap_or(".");
    let base = context_cwd();
    let cwd = if cwd == "." || cwd.is_empty() {
        base
    } else {
        let cwd = PathBuf::from(cwd);
        if cwd.is_absolute() {
            cwd
        } else {
            base.join(cwd)
        }
    };

    let cwd =
        std::fs::canonicalize(&cwd).map_err(|error| format!("cwd '{}': {error}", cwd.display()))?;
    if !cwd.is_dir() {
        return Err(format!("cwd '{}' is not a directory", cwd.display()));
    }
    Ok(cwd)
}

fn validate_acp_shell_access(run_cwd: &Path) -> Result<Option<PathBuf>, String> {
    if !ACP_MODE.load(Ordering::SeqCst) {
        return Ok(None);
    }
    if !acp_tools_enabled() {
        return Err(
            "ACP tools are disabled because acp_agents.working_directory is not configured"
                .to_string(),
        );
    }

    let Some(root) = configured_acp_root()? else {
        return Ok(None);
    };
    if path_is_inside(&root, run_cwd) {
        Ok(Some(root))
    } else {
        Err(format!(
            "cwd '{}' is outside ACP working_directory '{}'",
            run_cwd.display(),
            root.display()
        ))
    }
}

fn prepare_shell_execution(args: &serde_json::Value) -> Result<(PathBuf, PathBuf, bool), String> {
    let run_cwd = shell_cwd_from_args(args).map_err(|error| format!("bad arguments: {error}"))?;
    let restricted_root =
        validate_acp_shell_access(&run_cwd).map_err(|error| format!("denied: {error}"))?;
    let force_sandbox = restricted_root.is_some();
    let writable_root = restricted_root.unwrap_or_else(|| run_cwd.clone());
    Ok((run_cwd, writable_root, force_sandbox))
}

// --- System & Tool Setup ---
fn get_system() -> &'static str {
    SYSTEM.get_or_init(|| {
        let cwd = context_cwd()
            .to_str()
            .unwrap_or(".")
            .to_string();
        let home = home_dir()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("")
            .to_string();

        let docs = find_files(vec![cwd.clone()], vec!["claude.md", "agent.md", "agents.md", "AGENTS.md", "readme.md"], 40);
        let skills = find_files(
            vec![
                ".claude/skills".to_string(),
                // format!("{}/.claude/skills", home),
                // format!("{}/.codex/skills", home),
                // format!("{}/.codex/plugins", home),
                format!("{}/.pi/agent/_skills", home),
            ],
            vec!["skill.md", "skills.md"],
            40,
        );
        #[cfg(feature = "acp")]
        let delegation = if !get_config().acp_agents.is_empty() {
            " Use delegate_task or delegate_tasks to spawn configured ACP child agents for independent subtasks."
        } else {
            ""
        };
        #[cfg(not(feature = "acp"))]
        let delegation = "";
        let tool_guidance = if expose_execute_shell_tools() {
            "You are Nano, a general-purpose shell agent with a primary tool: execute_shell.\n\
             When user asks for shell commands, ALWAYS make a tool_call to execute_shell\n\
             Use it to inspect, edit, install, test, search, automate, and answer."
                .to_string()
        } else {
            "You are Nano, a general-purpose shell agent. Local shell and MCP tools are disabled in this restricted ACP session.\n\
             Answer from the prompt and provided context only."
                .to_string()
        };
        let acp_restriction = acp_allowed_root()
            .map(|root| {
                format!(
                    " Local shell commands must stay under {}.",
                    root.display()
                )
            })
            .unwrap_or_default();
        let persistence = if expose_execute_shell_tools() {
            "Keep taking shell steps until done or blocked."
        } else {
            "Complete the task without tool calls."
        };

        // "You are Nano, a shell agent. Use the execute_shell tool for ALL shell commands.\n\
        //  When user asks for shell commands, ALWAYS make a tool_call to execute_shell - never describe the command in text.\n\
        //  description must be exactly 5-10 words explaining why this command is useful.\n\
        //  Be concise. No markdown. cwd: {}\n\

        format!(
            "{}\n\
             {}{}\n\
             Be concise, tenacious, and relentlessly useful. {}\n\
             Output short plain-text snippets optimized for terminal reading; no markdown rendering or syntax highlighting.\n\
             Never run destructive commands unless explicitly requested.\n\
             cwd: {}\n\
             platform: {}\n\
             shell: {}\n\
             Important docs (read as needed): {}\n\
             Important skill files (read as needed): {}",
            tool_guidance,
            delegation,
            acp_restriction,
            persistence,
            cwd,
            env::consts::OS,
            env::var("SHELL").unwrap_or_default(),
            docs,
            skills
        )
    })
}

fn get_tool_responses() -> &'static serde_json::Value {
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

fn get_tool_chat() -> &'static serde_json::Value {
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
async fn get_acp_delegate_tools_chat() -> Vec<serde_json::Value> {
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
async fn get_acp_delegate_tools_responses() -> Vec<serde_json::Value> {
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

// --- Approvals & Execution ---
fn approve_sync(args: &serde_json::Value) -> bool {
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
        return true;
    }
    if ACP_MODE.load(Ordering::SeqCst) {
        return true;
    }

    eprint!(
        "Approve? {}  {}  {}: ",
        color("32", "[y] Approve"),
        color("33", "[a] Approve All"),
        color("31", "[n] Deny")
    );
    let _ = io::stderr().flush();

    let mut choice = String::new();
    if io::stdin().read_line(&mut choice).is_err() {
        return false;
    }
    let choice = choice.trim().to_lowercase();

    if choice == "a" || choice == "all" {
        APPROVE_ALL.store(true, Ordering::SeqCst);
        return true;
    }
    choice == "y" || choice == "yes"
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

async fn execute_shell_tool(args: &serde_json::Value) -> String {
    let desc = args
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("");
    let words = desc.split_whitespace().count();
    if !(5..=10).contains(&words) {
        return "bad arguments: description must be 5-10 words".to_string();
    }
    if let Err(error) = prepare_shell_execution(args) {
        return error;
    }

    let args_clone = args.clone();
    let approved = tokio::task::spawn_blocking(move || approve_sync(&args_clone))
        .await
        .unwrap_or(false);
    if approved {
        execute_shell(args).await
    } else {
        color("31", "denied by user")
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

async fn dispatch_tool_call(name: &str, args_str: &str) -> String {
    let args = match parse_tool_args(args_str) {
        Ok(args) => args,
        Err(e) => return e,
    };

    #[cfg(feature = "acp")]
    if let Some(result) = handle_acp_tool(name, &args).await {
        return result;
    }

    if get_mcp_client().has_tool(name).await {
        if !expose_mcp_tools() {
            return "denied: MCP tools are disabled in this restricted ACP child".to_string();
        }
        get_mcp_client()
            .call_tool(name, args)
            .await
            .unwrap_or_else(|e| e)
    } else if name == "execute_shell" {
        execute_shell_tool(&args).await
    } else {
        "unknown tool".to_string()
    }
}

// --- API Interaction ---
async fn respond_api(
    client: &Client,
    target: &ApiTarget,
    body: serde_json::Value,
) -> Result<serde_json::Value, reqwest::Error> {
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    let spinner_handle = if is_tty() {
        Some(tokio::spawn(async move {
            let frames = ['-', '\\', '|', '/'];
            let mut index = 0;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        eprint!("\r  {}\x1b[0K", color("90", &format!("{} thinking", frames[index % frames.len()])));
                        let _ = io::stderr().flush();
                        index += 1;
                    }
                    _ = rx.changed() => {
                        eprint!("\r\x1b[0K");
                        let _ = io::stderr().flush();
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };

    let mut req = client
        .post(&target.url)
        .header("Content-Type", "application/json");

    if !target.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", target.api_key));
    }

    let res = req.json(&body).send().await?.json().await;

    let _ = tx.send(true);
    if let Some(h) = spinner_handle {
        let _ = h.await;
    }

    res
}

// --- Responses API Mode ---
async fn respond_responses(
    client: &Client,
    payload: serde_json::Value,
    previous: Option<&str>,
) -> Result<serde_json::Value, reqwest::Error> {
    let target = get_api_target();
    let mut tools: Vec<serde_json::Value> = Vec::new();
    if expose_execute_shell_tools() {
        tools.push(get_tool_responses().clone());
    }
    #[cfg(feature = "acp")]
    if expose_acp_delegate_tools() {
        tools.extend(get_acp_delegate_tools_responses().await);
    }
    if expose_mcp_tools() {
        tools.extend(get_mcp_client().get_tools_schema().await);
    }

    let mut body = serde_json::json!({
        "model": target.model.as_str(),
        "instructions": get_system(),
        "tools": tools,
        "input": payload
    });
    if let Some(prev) = previous {
        body["previous_response_id"] = serde_json::Value::String(prev.to_string());
    }
    respond_api(client, &target, body).await
}

fn text(response: &serde_json::Value) -> String {
    response
        .get("output")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|item| item.get("type").and_then(|t| t.as_str()) == Some("message"))
                .filter_map(|item| item.get("content").and_then(|c| c.as_array()))
                .flatten()
                .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("output_text"))
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

async fn tool_output_responses(call: &serde_json::Value) -> serde_json::Value {
    let call_id = call
        .get("call_id")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args_str = call
        .get("arguments")
        .and_then(|a| a.as_str())
        .unwrap_or("{}");
    let result = dispatch_tool_call(name, args_str).await;

    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": result
    })
}

async fn run_responses(
    client: &Client,
    prompt: &str,
    previous: Option<&str>,
) -> (String, Option<String>, Option<Vec<serde_json::Value>>) {
    let payload = serde_json::json!([{"type": "message", "role": "user", "content": prompt}]);
    let mut response = match respond_responses(client, payload, previous).await {
        Ok(r) => r,
        Err(e) => return (format!("API Error: {}", e), None, None),
    };

    let mut prev_id = response
        .get("id")
        .and_then(|i| i.as_str())
        .map(String::from);

    for _ in 0..get_max_steps() {
        let calls: Vec<&serde_json::Value> = response
            .get("output")
            .and_then(|o| o.as_array())
            .map(|arr| {
                arr.iter()
                    .filter(|x| x.get("type").and_then(|t| t.as_str()) == Some("function_call"))
                    .collect()
            })
            .unwrap_or_default();

        if calls.is_empty() {
            return (text(&response), prev_id, None);
        }

        let mut outputs = Vec::new();
        for call in &calls {
            outputs.push(tool_output_responses(call).await);
        }

        response = match respond_responses(
            client,
            serde_json::Value::Array(outputs),
            prev_id.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return (format!("API Error: {}", e), prev_id, None),
        };
        prev_id = response
            .get("id")
            .and_then(|i| i.as_str())
            .map(String::from);
    }

    ("stopped: too many tool calls".to_string(), prev_id, None)
}

// --- Chat Completions API Mode ---
async fn respond_chat_with_target(
    client: &Client,
    messages: &[serde_json::Value],
    target: &ApiTarget,
) -> Result<serde_json::Value, reqwest::Error> {
    let mut tools = Vec::new();
    if expose_execute_shell_tools() {
        tools.push(serde_json::json!({"type": "function", "function": get_tool_chat()}));
    }
    #[cfg(feature = "acp")]
    if expose_acp_delegate_tools() {
        for tool in get_acp_delegate_tools_chat().await {
            tools.push(serde_json::json!({"type": "function", "function": tool}));
        }
    }
    if expose_mcp_tools() {
        for tool in get_mcp_client().get_tools_schema().await {
            tools.push(serde_json::json!({"type": "function", "function": tool}));
        }
    }
    let body = serde_json::json!({
        "model": target.model.as_str(),
        "messages": messages,
        "tools": tools
    });
    respond_api(client, target, body).await
}

async fn tool_output_chat(name: &str, args_str: &str, call_id: &str) -> serde_json::Value {
    let result = dispatch_tool_call(name, args_str).await;

    serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": result
    })
}

async fn run_chat(
    client: &Client,
    prompt: &str,
    messages: Vec<serde_json::Value>,
) -> (String, Option<String>, Option<Vec<serde_json::Value>>) {
    let target = get_api_target();
    run_chat_with_system(client, prompt, messages, get_system(), &target).await
}

async fn run_chat_with_system(
    client: &Client,
    prompt: &str,
    mut messages: Vec<serde_json::Value>,
    system: &str,
    target: &ApiTarget,
) -> (String, Option<String>, Option<Vec<serde_json::Value>>) {
    if messages.is_empty() {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.push(serde_json::json!({"role": "user", "content": prompt}));

    let mut response = match respond_chat_with_target(client, &messages, target).await {
        Ok(r) => r,
        Err(e) => return (format!("API Error: {}", e), None, Some(messages)),
    };

    for _ in 0..get_max_steps() {
        let choice = response
            .get("choices")
            .and_then(|c| c.get(0))
            .cloned()
            .unwrap_or_default();
        let msg = choice.get("message").cloned().unwrap_or_default();

        let text_content = msg
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let tool_calls = msg
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .cloned()
            .unwrap_or_default();

        // Fallback: try to parse tool call from text content if tool_calls is empty

        // let parsed_tool_call = if tool_calls.is_empty() {
        //     extract_tool_call_from_text(&text_content)
        // } else {
        //     None
        // };

        messages.push(msg);

        let tool_calls_to_process = if !tool_calls.is_empty() {
            tool_calls
        // } else if let Some(tc) = parsed_tool_call {
        //     vec![tc]
        } else {
            return (
                text_content,
                Some("chat-session".to_string()),
                Some(messages),
            );
        };

        for call in &tool_calls_to_process {
            let call_id = call.get("id").and_then(|c| c.as_str()).unwrap_or("call_1");
            let func = call.get("function").cloned().unwrap_or_default();
            let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args_str = func
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");

            let output = tool_output_chat(name, args_str, call_id).await;
            messages.push(output);
        }

        response = match respond_chat_with_target(client, &messages, target).await {
            Ok(r) => r,
            Err(e) => {
                return (
                    format!("API Error: {}", e),
                    Some("chat-session".to_string()),
                    Some(messages),
                );
            }
        };
    }

    (
        "stopped: too many tool calls".to_string(),
        Some("chat-session".to_string()),
        Some(messages),
    )
}

#[allow(dead_code)]
fn extract_tool_call_from_text(text: &str) -> Option<serde_json::Value> {
    // Look for JSON object with "name": "execute_shell" in the text
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('{')
            && trimmed.contains("\"name\"")
            && trimmed.contains("\"execute_shell\"")
        {
            // Try to parse as tool call format
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(trimmed) {
                return Some(serde_json::json!({
                    "id": "call_1",
                    "type": "function",
                    "function": parsed
                }));
            }
        }
    }
    None
}

// --- Repl & State ---
enum SessionState {
    Responses { previous: Option<String> },
    Chat { messages: Vec<serde_json::Value> },
}

fn strip_mito_prefix(prompt: &str) -> Option<&str> {
    let trimmed = prompt.trim_start();
    let rest = trimmed.strip_prefix("/mito")?;
    if rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

fn extract_mito_handoff(answer: &str) -> Option<String> {
    const PREFIX: &str = "MITO_SEND:";
    let index = answer.find(PREFIX)?;
    let handoff = answer[index + PREFIX.len()..].trim();
    if handoff.is_empty() {
        None
    } else {
        Some(handoff.to_string())
    }
}

fn get_mito_system() -> String {
    let cwd = context_cwd().to_str().unwrap_or(".").to_string();
    let docs = find_files(
        vec![cwd.clone()],
        vec![
            "claude.md",
            "agent.md",
            "agents.md",
            "AGENTS.md",
            "readme.md",
        ],
        40,
    );

    format!(
        "You are Mito, Nano's local planning agent.\n\
         The user talks to you through /mito messages; the /mito prefix has already been removed.\n\
         Keep a separate private context from the primary LLM.\n\
         Your job is to discuss the request with the user, inspect the current directory when useful, and prepare a detailed handoff for the primary LLM.\n\
         Ask concise clarifying questions when the task is underspecified.\n\
         Use execute_shell and MCP tools only when they help you understand the repo or produce a better handoff. For execute_shell, description must be 5-10 words. Never run destructive commands unless explicitly requested.\n\
         When you are ready for the primary LLM to do the work, output exactly one handoff and no other text, starting with MITO_SEND: followed by the complete prompt.\n\
         The handoff prompt must include the objective, relevant context, constraints, expected files or deliverables, and any preferences learned from the user.\n\
         cwd: {}\n\
         Important docs (read as needed): {}",
        cwd, docs
    )
}

#[cfg(feature = "acp")]
async fn run_single_turn(client: &Client, prompt: &str) -> String {
    let format = get_api_target().format;
    let (answer, _, _) = match format {
        ApiFormat::Responses => run_responses(client, prompt, None).await,
        ApiFormat::ChatCompletions => run_chat(client, prompt, vec![]).await,
    };
    answer
}

#[cfg(feature = "acp")]
async fn run_acp_server() -> Result<(), String> {
    ACP_MODE.store(true, Ordering::SeqCst);
    check_api_key();
    if expose_mcp_tools() {
        get_mcp_client().load_servers(get_config()).await;
    }

    let client = Client::new();
    let server = AcpServer::new(
        "nano",
        "Nano local shell agent",
        move |acp_prompt: AcpPrompt| {
            let client = client.clone();
            async move {
                let prompt = format!(
                    "ACP session: {}\ncwd: {}\n\n{}",
                    acp_prompt.session_id,
                    acp_prompt.cwd.display(),
                    acp_prompt.prompt
                );
                let answer = ACP_SESSION_CWD
                    .scope(acp_prompt.cwd.clone(), run_single_turn(&client, &prompt))
                    .await;
                if answer.starts_with("API Error:") {
                    Err(answer)
                } else {
                    Ok(answer)
                }
            }
        },
    );

    server.serve_stdio().await
}

async fn run_state_turn(
    client: &Client,
    prompt: &str,
    state: &mut SessionState,
    label: &mut Option<String>,
    label_prompt: &str,
) -> String {
    let result = match state {
        SessionState::Responses { previous } => {
            run_responses(client, prompt, previous.as_deref()).await
        }
        SessionState::Chat { messages } => run_chat(client, prompt, messages.clone()).await,
    };

    let (answer, prev_id, new_messages) = result;
    let session_label = label.as_deref().unwrap_or(label_prompt);

    match state {
        SessionState::Responses { previous } => {
            if let Some(ref id) = prev_id {
                save_session(id, session_label, None);
            }
            *previous = prev_id;
        }
        SessionState::Chat { messages } => {
            if let Some(msgs) = new_messages {
                save_session("chat-session", session_label, Some(msgs.clone()));
                *messages = msgs;
            }
        }
    }

    if label.is_none() {
        *label = Some(label_prompt.to_string());
    }

    answer
}

async fn run_mito_turn(
    client: &Client,
    prompt: &str,
    mito_messages: &mut Vec<serde_json::Value>,
    main_state: &mut SessionState,
    main_label: &mut Option<String>,
) -> String {
    let target = match get_mito_target() {
        Ok(target) => target,
        Err(error) => return format!("mito error: {error}"),
    };
    let system = get_mito_system();
    let (answer, _, new_messages) =
        run_chat_with_system(client, prompt, mito_messages.clone(), &system, &target).await;

    if let Some(messages) = new_messages {
        *mito_messages = messages;
    }

    let Some(handoff) = extract_mito_handoff(&answer) else {
        return format!("mito > {}", answer);
    };

    let main_answer = run_state_turn(client, &handoff, main_state, main_label, prompt).await;
    if main_answer.is_empty() {
        format!("mito > {}", handoff)
    } else {
        format!("mito > {}\n{}", handoff, main_answer)
    }
}

async fn repl(client: &Client, mut state: SessionState, mut label: Option<String>) {
    eprintln!(
        "{} repl {} mcp: {}",
        color("1", "nano"),
        color(
            "90",
            "(:q quit, :reset reset, /mito plan, end with \\ for multiline)"
        ),
        color("90", &get_mcp_client().status())
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut mito_messages = Vec::new();

    loop {
        let prompt = match read_repl_input(&mut lines).await {
            Some(prompt) => prompt,
            None => return,
        };

        if prompt.is_empty() {
            continue;
        }
        let lower = prompt.to_lowercase();
        if lower == ":q" || lower == "quit" || lower == "exit" {
            return;
        }
        if lower == ":reset" || lower == "reset" {
            state = match get_api_target().format {
                ApiFormat::Responses => SessionState::Responses { previous: None },
                ApiFormat::ChatCompletions => SessionState::Chat { messages: vec![] },
            };
            label = None;
            mito_messages.clear();
            eprintln!("{}", color("90", "reset"));
            continue;
        }

        let answer = if let Some(mito_prompt) = strip_mito_prefix(&prompt) {
            run_mito_turn(
                client,
                mito_prompt,
                &mut mito_messages,
                &mut state,
                &mut label,
            )
            .await
        } else {
            run_state_turn(client, &prompt, &mut state, &mut label, &prompt).await
        };
        println!("{}", answer);
    }
}

// --- Main ---
#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--acp") {
        #[cfg(feature = "acp")]
        {
            if let Err(e) = run_acp_server().await {
                eprintln!("ACP error: {}", e);
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(feature = "acp"))]
        {
            eprintln!("ACP feature not enabled - rebuild with --features acp");
            std::process::exit(1);
        }
    }

    check_api_key();

    // Load MCP servers
    get_mcp_client().load_servers(get_config()).await;

    let client = Client::new();
    let mut args: Vec<String> = env::args().skip(1).collect();

    let mut flag = None;
    if !args.is_empty() && (args[0] == "-c" || args[0] == "-s") {
        flag = Some(args.remove(0));
    }
    let prompt = args.join(" ");

    let format = get_api_target().format;

    let mut state = match format {
        ApiFormat::Responses => SessionState::Responses { previous: None },
        ApiFormat::ChatCompletions => SessionState::Chat { messages: vec![] },
    };
    let mut label = None;

    match flag.as_deref() {
        Some("-s") => {
            if let Some(session) = pick_session() {
                eprintln!("{}", color("90", &format!("resuming: {}", session.label)));
                state = match format {
                    ApiFormat::Responses => SessionState::Responses {
                        previous: Some(session.id),
                    },
                    ApiFormat::ChatCompletions => SessionState::Chat {
                        messages: session.messages.unwrap_or_default(),
                    },
                };
                label = Some(session.label);
            }
        }
        Some("-c") => {
            let cwd = env::current_dir()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("")
                .to_string();
            let sessions: Vec<Session> = load_sessions()
                .into_iter()
                .filter(|s| s.cwd == cwd)
                .collect();
            if sessions.is_empty() {
                eprintln!("no sessions in this directory");
                std::process::exit(1);
            }
            let last = sessions.last().unwrap();
            eprintln!("{}", color("90", &format!("continuing: {}", last.label)));
            state = match format {
                ApiFormat::Responses => SessionState::Responses {
                    previous: Some(last.id.clone()),
                },
                ApiFormat::ChatCompletions => SessionState::Chat {
                    messages: last.messages.clone().unwrap_or_default(),
                },
            };
            label = Some(last.label.clone());
        }
        _ => {} // Catch-all for None or any other flag
    }

    if !prompt.is_empty() {
        let mut mito_messages = Vec::new();
        let answer = if let Some(mito_prompt) = strip_mito_prefix(&prompt) {
            run_mito_turn(
                &client,
                mito_prompt,
                &mut mito_messages,
                &mut state,
                &mut label,
            )
            .await
        } else {
            run_state_turn(&client, &prompt, &mut state, &mut label, &prompt).await
        };
        println!("{}", answer);
    } else {
        repl(&client, state, label).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_mito_handoff, strip_mito_prefix};

    #[test]
    fn strip_mito_prefix_accepts_command_boundary() {
        assert_eq!(strip_mito_prefix("/mito build this"), Some("build this"));
        assert_eq!(strip_mito_prefix("  /mito\nbuild this"), Some("build this"));
        assert_eq!(strip_mito_prefix("/mito"), Some(""));
        assert_eq!(strip_mito_prefix("/mitochondria"), None);
    }

    #[test]
    fn extract_mito_handoff_reads_marker_body() {
        assert_eq!(
            extract_mito_handoff("MITO_SEND: implement the feature").as_deref(),
            Some("implement the feature")
        );
        assert_eq!(extract_mito_handoff("MITO_SEND:   "), None);
        assert_eq!(extract_mito_handoff("ask a question first"), None);
    }
}
