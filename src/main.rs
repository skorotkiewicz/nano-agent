use dirs::home_dir;
use nano_agent::{config::Config, mcp::McpClient, sandbox::Sandbox};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
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

static IS_TTY: OnceLock<bool> = OnceLock::new();
static APPROVE_ALL: AtomicBool = AtomicBool::new(false);
static CONFIG: OnceLock<Config> = OnceLock::new();
static MODEL: OnceLock<String> = OnceLock::new();
static MAX_STEPS: OnceLock<usize> = OnceLock::new();
static SESSIONS_PATH: OnceLock<PathBuf> = OnceLock::new();
static SYSTEM: OnceLock<String> = OnceLock::new();
static MCP_CLIENT: OnceLock<McpClient> = OnceLock::new();

fn get_config() -> &'static Config {
    CONFIG.get_or_init(Config::load)
}

fn get_mcp_client() -> &'static McpClient {
    MCP_CLIENT.get_or_init(McpClient::new)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ApiFormat {
    Responses,
    ChatCompletions,
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
    // Check custom providers first
    if let Some(provider_name) = get_config().get_provider()
        && let Some(custom) = get_config().get_custom_provider(provider_name)
    {
        let base = custom.base_url.trim_end_matches('/');
        return (
            format!("{}/chat/completions", base),
            ApiFormat::ChatCompletions,
            custom.api_key.clone().unwrap_or_default(),
        );
    }

    if let Ok(base) = env::var("OPENAI_BASE_URL") {
        let base = base.trim_end_matches('/');
        (
            format!("{}/chat/completions", base),
            ApiFormat::ChatCompletions,
            env::var("OPENAI_API_KEY").unwrap_or_default(),
        )
    } else {
        (
            "https://api.openai.com/v1/responses".to_string(),
            ApiFormat::Responses,
            env::var("OPENAI_API_KEY").unwrap_or_default(),
        )
    }
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

// --- System & Tool Setup ---
fn get_system() -> &'static str {
    SYSTEM.get_or_init(|| {
        let cwd = env::current_dir()
            .unwrap_or_default()
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

        // "You are Nano, a shell agent. Use the execute_shell tool for ALL shell commands.\n\
        //  When user asks for shell commands, ALWAYS make a tool_call to execute_shell - never describe the command in text.\n\
        //  description must be exactly 5-10 words explaining why this command is useful.\n\
        //  Be concise. No markdown. cwd: {}\n\

        format!(
            "You are Nano, a general-purpose shell agent with one tool: execute_shell.\n\
             When user asks for shell commands, ALWAYS make a tool_call to execute_shell\n\
             Use it to inspect, edit, install, test, search, automate, and answer.\n\
             Be concise, tenacious, and relentlessly useful. Keep taking shell steps until done or blocked.\n\
             Output short plain-text snippets optimized for terminal reading; no markdown rendering or syntax highlighting.\n\
             Never run destructive commands unless explicitly requested.\n\
             cwd: {}\n\
             platform: {}\n\
             shell: {}\n\
             Important docs (read as needed): {}\n\
             Important skill files (read as needed): {}",
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
    let cwd = args.get("cwd").and_then(|c| c.as_str()).unwrap_or(".");
    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(60);
    let env_vars = args.get("env").and_then(|e| e.as_object());

    let run_cwd = if cwd == "." || cwd.is_empty() {
        env::current_dir().unwrap_or_default()
    } else {
        PathBuf::from(cwd)
    };

    let merged_command = format!("{} 2>&1", command);

    let sandbox_enabled = env::var("NANO_SANDBOX")
        .map(|v| v == "0" || v.to_lowercase() == "false")
        .unwrap_or(true);
    let sandbox = Sandbox::new(sandbox_enabled)
        .with_shell("sh")
        .with_cwd(run_cwd.clone());

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

// --- API Interaction ---
async fn respond_api(
    client: &Client,
    body: serde_json::Value,
    api_key: &str,
) -> Result<serde_json::Value, reqwest::Error> {
    let (url, _, _) = get_api_config();

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

    let mut req = client.post(&url).header("Content-Type", "application/json");

    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
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
    let mut tools: Vec<serde_json::Value> = vec![get_tool_responses().clone()];
    tools.extend(get_mcp_client().get_tools_schema().await);

    let mut body = serde_json::json!({
        "model": get_model(),
        "instructions": get_system(),
        "tools": tools,
        "input": payload
    });
    if let Some(prev) = previous {
        body["previous_response_id"] = serde_json::Value::String(prev.to_string());
    }
    let (_, _, api_key) = get_api_config();
    respond_api(client, body, &api_key).await
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

    let result = if get_mcp_client().has_tool(name).await {
        let args_str = call
            .get("arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("{}");
        let args: serde_json::Value = serde_json::from_str(args_str)
            .unwrap_or_else(|e| serde_json::json!({"error": format!("bad arguments: {}", e)}));

        if args.get("error").is_some() {
            args.get("error")
                .unwrap()
                .as_str()
                .unwrap_or("bad arguments")
                .to_string()
        } else {
            get_mcp_client()
                .call_tool(name, args)
                .await
                .unwrap_or_else(|e| e)
        }
    } else if name == "execute_shell" {
        let args_str = call
            .get("arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("{}");
        let args: serde_json::Value = serde_json::from_str(args_str)
            .unwrap_or_else(|e| serde_json::json!({"error": format!("bad arguments: {}", e)}));

        if args.get("error").is_some() {
            args.get("error")
                .unwrap()
                .as_str()
                .unwrap_or("bad arguments")
                .to_string()
        } else {
            let desc = args
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let words = desc.split_whitespace().count();
            if !(5..=10).contains(&words) {
                "bad arguments: description must be 5-10 words".to_string()
            } else {
                let args_clone = args.clone();
                let approved = tokio::task::spawn_blocking(move || approve_sync(&args_clone))
                    .await
                    .unwrap_or(false);
                if approved {
                    execute_shell(&args).await
                } else {
                    color("31", "denied by user")
                }
            }
        }
    } else {
        "unknown tool".to_string()
    };

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
async fn respond_chat(
    client: &Client,
    messages: &[serde_json::Value],
) -> Result<serde_json::Value, reqwest::Error> {
    let mut tools = vec![serde_json::json!({"type": "function", "function": get_tool_chat()})];
    for tool in get_mcp_client().get_tools_schema().await {
        tools.push(serde_json::json!({"type": "function", "function": tool}));
    }
    let body = serde_json::json!({
        "model": get_model(),
        "messages": messages,
        "tools": tools
    });
    let (_, _, api_key) = get_api_config();
    respond_api(client, body, &api_key).await
}

async fn tool_output_chat(name: &str, args_str: &str, call_id: &str) -> serde_json::Value {
    let result = if get_mcp_client().has_tool(name).await {
        let args: serde_json::Value = serde_json::from_str(args_str)
            .unwrap_or_else(|e| serde_json::json!({"error": format!("bad arguments: {}", e)}));

        if args.get("error").is_some() {
            args.get("error")
                .unwrap()
                .as_str()
                .unwrap_or("bad arguments")
                .to_string()
        } else {
            get_mcp_client()
                .call_tool(name, args)
                .await
                .unwrap_or_else(|e| e)
        }
    } else if name == "execute_shell" {
        let args: serde_json::Value = serde_json::from_str(args_str)
            .unwrap_or_else(|e| serde_json::json!({"error": format!("bad arguments: {}", e)}));

        if args.get("error").is_some() {
            args.get("error")
                .unwrap()
                .as_str()
                .unwrap_or("bad arguments")
                .to_string()
        } else {
            let desc = args
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let words = desc.split_whitespace().count();
            if !(5..=10).contains(&words) {
                "bad arguments: description must be 5-10 words".to_string()
            } else {
                let args_clone = args.clone();
                let approved = tokio::task::spawn_blocking(move || approve_sync(&args_clone))
                    .await
                    .unwrap_or(false);
                if approved {
                    execute_shell(&args).await
                } else {
                    color("31", "denied by user")
                }
            }
        }
    } else {
        "unknown tool".to_string()
    };

    serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": result
    })
}

async fn run_chat(
    client: &Client,
    prompt: &str,
    mut messages: Vec<serde_json::Value>,
) -> (String, Option<String>, Option<Vec<serde_json::Value>>) {
    if messages.is_empty() {
        messages.push(serde_json::json!({"role": "system", "content": get_system()}));
    }
    messages.push(serde_json::json!({"role": "user", "content": prompt}));

    let mut response = match respond_chat(client, &messages).await {
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

        response = match respond_chat(client, &messages).await {
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

async fn repl(client: &Client, mut state: SessionState, mut label: Option<String>) {
    eprintln!(
        "{} repl {}",
        color("1", "nano"),
        color("90", "(:q quit, :reset reset)")
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();

    loop {
        eprint!("{} ", color("36", "nano >"));
        let _ = io::stderr().flush();

        let prompt = match lines.next_line().await {
            Ok(Some(line)) => line,
            _ => {
                eprintln!();
                return;
            }
        };

        let prompt = prompt.trim().to_string();
        if prompt.is_empty() {
            continue;
        }
        let lower = prompt.to_lowercase();
        if lower == ":q" || lower == "quit" || lower == "exit" {
            return;
        }
        if lower == ":reset" || lower == "reset" {
            state = match get_api_config().1 {
                ApiFormat::Responses => SessionState::Responses { previous: None },
                ApiFormat::ChatCompletions => SessionState::Chat { messages: vec![] },
            };
            label = None;
            eprintln!("{}", color("90", "reset"));
            continue;
        }

        let result = match &state {
            SessionState::Responses { previous } => {
                run_responses(client, &prompt, previous.as_deref()).await
            }
            SessionState::Chat { messages } => run_chat(client, &prompt, messages.clone()).await,
        };

        let (answer, prev_id, new_messages) = result;

        match &mut state {
            SessionState::Responses { previous } => {
                if let Some(ref id) = prev_id {
                    save_session(id, label.as_deref().unwrap_or(""), None);
                }
                *previous = prev_id;
            }
            SessionState::Chat { messages } => {
                if let Some(msgs) = new_messages {
                    save_session(
                        "chat-session",
                        label.as_deref().unwrap_or(""),
                        Some(msgs.clone()),
                    );
                    *messages = msgs;
                }
            }
        }

        if label.is_none() {
            label = Some(prompt.clone());
        }
        println!("{}", answer);
    }
}

// --- Main ---
#[tokio::main]
async fn main() {
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

    let (_, format, _) = get_api_config();

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
        let result = match &state {
            SessionState::Responses { previous } => {
                run_responses(&client, &prompt, previous.as_deref()).await
            }
            SessionState::Chat { messages } => run_chat(&client, &prompt, messages.clone()).await,
        };

        let (answer, prev_id, new_messages) = result;
        if let Some(ref id) = prev_id {
            save_session(id, label.as_deref().unwrap_or(&prompt), new_messages);
        }
        println!("{}", answer);
    } else {
        repl(&client, state, label).await;
    }
}
