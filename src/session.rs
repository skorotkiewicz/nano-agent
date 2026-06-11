//! Session persistence (~/.nano_sessions.json) and per-conversation state.

use crate::provider::ApiFormat;
use crate::state::color;
use dirs::home_dir;
use serde::{Deserialize, Serialize};
use std::env;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

static SESSIONS_PATH: OnceLock<PathBuf> = OnceLock::new();

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: String,
    pub label: String,
    pub cwd: String,
    pub ts: i64,
    #[serde(default)]
    pub messages: Option<Vec<serde_json::Value>>,
}

pub enum SessionState {
    Responses { previous: Option<String> },
    Chat { messages: Vec<serde_json::Value> },
}

impl SessionState {
    pub fn new(format: ApiFormat) -> Self {
        match format {
            ApiFormat::Responses => SessionState::Responses { previous: None },
            ApiFormat::ChatCompletions => SessionState::Chat { messages: vec![] },
        }
    }

    pub fn resume(format: ApiFormat, session: Session) -> Self {
        match format {
            ApiFormat::Responses => SessionState::Responses {
                previous: Some(session.id),
            },
            ApiFormat::ChatCompletions => SessionState::Chat {
                messages: session.messages.unwrap_or_default(),
            },
        }
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

pub fn save_session(response_id: &str, label: &str, messages: Option<Vec<serde_json::Value>>) {
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
        messages,
    });

    if sessions.len() > 50 {
        sessions = sessions[sessions.len() - 50..].to_vec();
    }

    if let Ok(data) = serde_json::to_string_pretty(&sessions) {
        let _ = std::fs::write(sessions_path(), data);
    }
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
