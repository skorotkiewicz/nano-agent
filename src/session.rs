//! Session persistence (~/.nano_sessions.json) and per-conversation state.

use crate::provider::ApiFormat;
use crate::state::color;
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static SESSIONS_PATH: OnceLock<PathBuf> = OnceLock::new();
const MAX_SESSIONS: usize = 50;
// ponytail: hard cap messages body so resume stays usable; upgrade = per-message tool trim
const MAX_SESSION_MESSAGES: usize = 200;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: String,
    pub label: String,
    pub cwd: String,
    pub ts: i64,
    #[serde(default)]
    pub format: Option<ApiFormat>,
    #[serde(default)]
    pub messages: Option<Vec<Value>>,
}

pub enum SessionState {
    Responses {
        previous: Option<String>,
        messages: Vec<Value>,
    },
    Chat {
        messages: Vec<Value>,
    },
}

impl SessionState {
    pub fn new(format: ApiFormat) -> Self {
        match format {
            ApiFormat::Responses => SessionState::Responses {
                previous: None,
                messages: vec![],
            },
            ApiFormat::ChatCompletions => SessionState::Chat { messages: vec![] },
        }
    }

    pub fn resume(format: ApiFormat, session: Session) -> Self {
        match format {
            ApiFormat::Responses => SessionState::Responses {
                previous: Some(session.id),
                messages: session.messages.unwrap_or_default(),
            },
            ApiFormat::ChatCompletions => SessionState::Chat {
                messages: session.messages.unwrap_or_default(),
            },
        }
    }
}

impl Session {
    /// A legacy `chat-session` id with no stored format is a chat-completions session.
    pub fn resolved_format(&self) -> ApiFormat {
        self.format.unwrap_or_else(|| {
            if self.id == "chat-session" {
                ApiFormat::ChatCompletions
            } else {
                ApiFormat::Responses
            }
        })
    }
}

fn sessions_path() -> &'static PathBuf {
    SESSIONS_PATH.get_or_init(|| home_dir().unwrap_or_default().join(".nano_sessions.json"))
}

fn current_cwd_string() -> String {
    env::current_dir()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("")
        .to_string()
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

/// Sessions recorded in the current working directory, oldest first.
pub fn sessions_in_cwd() -> Vec<Session> {
    let cwd = current_cwd_string();
    load_sessions()
        .into_iter()
        .filter(|s| s.cwd == cwd)
        .collect()
}

pub fn append_session_messages(
    existing: &mut Vec<Value>,
    new_messages: Option<Vec<Value>>,
) -> Option<Vec<Value>> {
    if let Some(new_messages) = new_messages {
        existing.extend(new_messages);
    }
    (!existing.is_empty()).then(|| existing.clone())
}

pub fn save_session(
    response_id: &str,
    label: &str,
    format: ApiFormat,
    messages: Option<Vec<Value>>,
) {
    let mut sessions = load_sessions();
    let cwd = current_cwd_string();

    sessions.retain(|s| !(s.label == label && s.cwd == cwd));

    sessions.push(Session {
        id: response_id.to_string(),
        label: label.chars().take(80).collect(),
        cwd,
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        format: Some(format),
        messages,
    });

    if sessions.len() > MAX_SESSIONS {
        sessions = sessions[sessions.len() - MAX_SESSIONS..].to_vec();
    }

    // Cap message trails so one chat path can't balloon ~/.nano_sessions.json.
    for session in &mut sessions {
        if let Some(messages) = session.messages.as_mut()
            && messages.len() > MAX_SESSION_MESSAGES
        {
            let drop = messages.len() - MAX_SESSION_MESSAGES;
            messages.drain(0..drop);
        }
    }

    if let Ok(data) = serde_json::to_string_pretty(&sessions) {
        let _ = atomic_write(sessions_path(), data.as_bytes());
    }
}

/// Write via temp+rename so a crash mid-write can't leave a half JSON array.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("nano_sessions"),
        std::process::id()
    ));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn pick_session() -> Option<Session> {
    let sessions = sessions_in_cwd();
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

#[cfg(test)]
mod tests {
    use super::{Session, append_session_messages};
    use crate::provider::ApiFormat;

    #[test]
    fn append_session_messages_keeps_prior_turns() {
        let mut existing = vec![serde_json::json!({"role": "user", "content": "first"})];
        let merged = append_session_messages(
            &mut existing,
            Some(vec![
                serde_json::json!({"role": "assistant", "content": "second"}),
            ]),
        )
        .unwrap();

        assert_eq!(existing.len(), 2);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["content"], "first");
        assert_eq!(merged[1]["content"], "second");
    }

    #[test]
    fn legacy_chat_session_id_resolves_to_chat_completions() {
        let session = Session {
            id: "chat-session".to_string(),
            label: "test".to_string(),
            cwd: ".".to_string(),
            ts: 0,
            format: None,
            messages: None,
        };
        assert_eq!(session.resolved_format(), ApiFormat::ChatCompletions);
    }

    #[test]
    fn caps_session_messages_to_max() {
        use super::MAX_SESSION_MESSAGES;
        let mut messages = (0..(MAX_SESSION_MESSAGES + 25))
            .map(|i| serde_json::json!({"role": "user", "content": i.to_string()}))
            .collect::<Vec<_>>();
        if messages.len() > MAX_SESSION_MESSAGES {
            let drop = messages.len() - MAX_SESSION_MESSAGES;
            messages.drain(0..drop);
        }
        assert_eq!(messages.len(), MAX_SESSION_MESSAGES);
        assert_eq!(messages[0]["content"], "25");
    }
}
