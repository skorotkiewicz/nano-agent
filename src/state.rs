//! Process-wide state: lazily initialized globals and small shared helpers.

#[cfg(feature = "acp")]
use nano_agent::acp::AcpAgentManager;
use nano_agent::{config::Config, mcp::McpClient};
use std::env;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

pub static APPROVE_ALL: AtomicBool = AtomicBool::new(false);
pub static ACP_MODE: AtomicBool = AtomicBool::new(false);

static IS_TTY: OnceLock<bool> = OnceLock::new();
static CONFIG: OnceLock<Config> = OnceLock::new();
static MODEL: OnceLock<String> = OnceLock::new();
static MAX_STEPS: OnceLock<usize> = OnceLock::new();
static MCP_CLIENT: OnceLock<McpClient> = OnceLock::new();
#[cfg(feature = "acp")]
static ACP_MANAGER: OnceLock<AcpAgentManager> = OnceLock::new();

tokio::task_local! {
    pub static ACP_SESSION_CWD: PathBuf;
}

pub fn get_config() -> &'static Config {
    CONFIG.get_or_init(Config::load)
}

pub fn get_mcp_client() -> &'static McpClient {
    MCP_CLIENT.get_or_init(McpClient::new)
}

#[cfg(feature = "acp")]
pub fn get_acp_manager() -> &'static AcpAgentManager {
    ACP_MANAGER.get_or_init(|| AcpAgentManager::from_config(get_config()))
}

pub fn is_tty() -> bool {
    *IS_TTY.get_or_init(|| io::stderr().is_terminal())
}

pub fn acp_mode() -> bool {
    ACP_MODE.load(Ordering::SeqCst)
}

pub fn get_model() -> &'static str {
    MODEL.get_or_init(|| env::var("OPENAI_MODEL").unwrap_or_else(|_| "gpt-5.5".to_string()))
}

pub fn get_max_steps() -> usize {
    *MAX_STEPS.get_or_init(|| {
        env::var("NANO_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200)
    })
}

pub fn color(code: &str, text: &str) -> String {
    if is_tty() {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

pub fn env_flag_is_false(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

/// Working directory for the current context: the ACP session cwd when
/// handling an ACP prompt, otherwise the process cwd.
pub fn context_cwd() -> PathBuf {
    if acp_mode()
        && let Ok(cwd) = ACP_SESSION_CWD.try_with(Clone::clone)
    {
        return cwd;
    }

    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
