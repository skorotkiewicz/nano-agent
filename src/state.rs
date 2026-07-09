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
/// After the user picks [s] Safe, auto-approve read-only command patterns this turn.
pub static APPROVE_SAFE: AtomicBool = AtomicBool::new(false);
/// Set when the user hits Esc/Ctrl+C mid-turn (API wait or long shell).
pub static CANCEL_TURN: AtomicBool = AtomicBool::new(false);
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

pub fn clear_cancel() {
    CANCEL_TURN.store(false, Ordering::SeqCst);
}

pub fn request_cancel() {
    CANCEL_TURN.store(true, Ordering::SeqCst);
}

pub fn get_model() -> &'static str {
    MODEL.get_or_init(|| {
        env::var("OPENAI_MODEL")
            .ok()
            .or_else(|| get_config().get_model().map(String::from))
            .unwrap_or_else(|| "gpt-5.5".to_string())
    })
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

/// Keep the tail of `text` (respecting UTF-8 boundaries) when over `max` bytes.
pub fn truncate_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
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

#[cfg(test)]
mod tests {
    use super::truncate_tail;

    #[test]
    fn truncate_tail_respects_utf8_boundaries() {
        assert_eq!(truncate_tail("prefix-ébc", 4), "ébc");
        assert_eq!(truncate_tail("short", 100), "short");
    }
}
