//! Session persistence (~/.nano/sessions/<cwd-hash>.jsonl) and conversation state.

use crate::provider::ApiFormat;
use crate::state::color;
use dirs::home_dir;
use nano_agent::paths::{ensure_nano_dirs, session_file_for_cwd};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;

const MAX_SESSIONS: usize = 50;
// ponytail: hard caps so resume stays usable; raise if long-context session files matter
const MAX_SESSION_MESSAGES: usize = 200;
const MAX_MESSAGE_CHARS: usize = 4_000;
static MIGRATE_ONCE: Once = Once::new();

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
        /// Side-channel from `! cmd` to prepend onto the next user turn (Responses has no client messages array).
        pending_context: Option<String>,
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
                pending_context: None,
            },
            ApiFormat::ChatCompletions => SessionState::Chat { messages: vec![] },
        }
    }

    pub fn resume(format: ApiFormat, session: Session) -> Self {
        match format {
            ApiFormat::Responses => SessionState::Responses {
                previous: Some(session.id),
                messages: session.messages.unwrap_or_default(),
                pending_context: None,
            },
            ApiFormat::ChatCompletions => SessionState::Chat {
                messages: session.messages.unwrap_or_default(),
            },
        }
    }

    /// Record a user-bang shell result for the next model turn (`!` only).
    /// Plain text only — never ANSI (model / session JSON must not get escapes).
    pub fn note_user_shell(&mut self, command: &str, output: &str) {
        let note = format!("[user ran shell]\n$ {command}\n{output}");

        match self {
            SessionState::Chat { messages } => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": note,
                }));
            }
            SessionState::Responses {
                messages,
                pending_context,
                ..
            } => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": note.clone(),
                }));
                // Stack notes if user runs several `!` before chatting.
                *pending_context = Some(match pending_context.take() {
                    Some(prev) => format!("{prev}\n\n{note}"),
                    None => note,
                });
            }
        }
    }

    /// Take Responses-only context to inject into the next user prompt.
    pub fn take_pending_context(&mut self) -> Option<String> {
        match self {
            SessionState::Responses {
                pending_context, ..
            } => pending_context.take(),
            SessionState::Chat { .. } => None,
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

fn current_cwd_string() -> String {
    env::current_dir()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("")
        .to_string()
}

fn sessions_file() -> PathBuf {
    ensure_nano_dirs();
    migrate_legacy_sessions_once();
    session_file_for_cwd(&current_cwd_string())
}

/// One-shot: split ~/.nano_sessions.json into per-cwd JSONL under ~/.nano/sessions/.
fn migrate_legacy_sessions_once() {
    MIGRATE_ONCE.call_once(|| {
        let legacy = home_dir().unwrap_or_default().join(".nano_sessions.json");
        if !legacy.exists() {
            return;
        }
        let data = match std::fs::read_to_string(&legacy) {
            Ok(data) => data,
            Err(_) => return,
        };
        let sessions: Vec<Session> = match serde_json::from_str(&data) {
            Ok(sessions) => sessions,
            Err(_) => return,
        };

        let mut by_cwd: std::collections::HashMap<String, Vec<Session>> =
            std::collections::HashMap::new();
        for session in sessions {
            by_cwd.entry(session.cwd.clone()).or_default().push(session);
        }

        for (cwd, mut list) in by_cwd {
            list.sort_by_key(|s| s.ts);
            let path = session_file_for_cwd(&cwd);
            // Don't clobber newer JSONL if user already wrote under ~/.nano.
            if path.exists() {
                continue;
            }
            let _ = write_sessions_jsonl(&path, &list);
        }

        // Leave a trail; don't delete legacy automatically (user may want a backup).
        let bak = legacy.with_extension("json.bak");
        let _ = std::fs::rename(&legacy, &bak);
    });
}

fn load_sessions_from(path: &Path) -> Vec<Session> {
    if !path.exists() {
        return vec![];
    }
    let data = match std::fs::read_to_string(path) {
        Ok(data) => data,
        Err(_) => return vec![],
    };

    // JSONL first; fall back to a single JSON array (pre-split leftover).
    let mut sessions = Vec::new();
    let mut all_jsonl = !data.trim().is_empty();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Session>(line) {
            Ok(session) => sessions.push(session),
            Err(_) => {
                all_jsonl = false;
                break;
            }
        }
    }
    if all_jsonl && !sessions.is_empty() {
        return sessions;
    }
    serde_json::from_str(&data).unwrap_or_default()
}

fn write_sessions_jsonl(path: &Path, sessions: &[Session]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = String::new();
    for session in sessions {
        let line = serde_json::to_string(session).map_err(io::Error::other)?;
        body.push_str(&line);
        body.push('\n');
    }
    atomic_write(path, body.as_bytes())
}

/// Sessions recorded in the current working directory, oldest first.
pub fn sessions_in_cwd() -> Vec<Session> {
    load_sessions_from(&sessions_file())
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
    let path = sessions_file();
    let mut sessions = load_sessions_from(&path);
    let cwd = current_cwd_string();

    sessions.retain(|s| s.label != label);

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

    for session in &mut sessions {
        if let Some(messages) = session.messages.as_mut() {
            compact_messages(messages);
        }
    }

    let _ = write_sessions_jsonl(&path, &sessions);
}

fn compact_messages(messages: &mut Vec<Value>) {
    if messages.len() > MAX_SESSION_MESSAGES {
        let drop = messages.len() - MAX_SESSION_MESSAGES;
        messages.drain(0..drop);
        // Prefer keeping a leading system message if we just dropped one into the hole.
        // (Cheap: if first remaining isn't system but one exists later near head, leave it.)
    }
    for message in messages.iter_mut() {
        trim_message_fields(message);
    }
}

fn trim_message_fields(message: &mut Value) {
    if let Some(content) = message.get_mut("content")
        && let Some(text) = content.as_str()
        && text.len() > MAX_MESSAGE_CHARS
    {
        *content = Value::String(truncate_keep_tail(text, MAX_MESSAGE_CHARS));
    }
    if let Some(output) = message.get_mut("output")
        && let Some(text) = output.as_str()
        && text.len() > MAX_MESSAGE_CHARS
    {
        *output = Value::String(truncate_keep_tail(text, MAX_MESSAGE_CHARS));
    }
}

fn truncate_keep_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[…truncated]\n{}", &text[start..])
}

/// Write via temp+rename so a crash mid-write can't leave a half JSONL file.
fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let _ = std::fs::create_dir_all(parent);
    let tmp = parent.join(format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("sessions"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

pub fn pick_session() -> Option<Session> {
    let sessions = sessions_in_cwd();
    if sessions.is_empty() {
        eprintln!("no sessions in this directory — start fresh with `nano-agent` (no -s)");
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
        use super::{MAX_SESSION_MESSAGES, compact_messages};
        let mut messages = (0..(MAX_SESSION_MESSAGES + 25))
            .map(|i| serde_json::json!({"role": "user", "content": i.to_string()}))
            .collect::<Vec<_>>();
        compact_messages(&mut messages);
        assert_eq!(messages.len(), MAX_SESSION_MESSAGES);
        assert_eq!(messages[0]["content"], "25");
    }

    #[test]
    fn trims_oversized_message_content() {
        use super::{MAX_MESSAGE_CHARS, compact_messages};
        let big = "x".repeat(MAX_MESSAGE_CHARS + 100);
        let mut messages = vec![serde_json::json!({"role": "tool", "content": big})];
        compact_messages(&mut messages);
        let content = messages[0]["content"].as_str().unwrap();
        assert!(content.len() <= MAX_MESSAGE_CHARS + 20);
        assert!(content.starts_with("[…truncated]"));
    }

    #[test]
    fn note_user_shell_marks_chat_and_responses() {
        use super::SessionState;
        use crate::provider::ApiFormat;

        let mut chat = SessionState::new(ApiFormat::ChatCompletions);
        chat.note_user_shell("cat a", "hello");
        match &chat {
            SessionState::Chat { messages } => {
                assert_eq!(messages.len(), 1);
                assert!(messages[0]["content"].as_str().unwrap().contains("cat a"));
            }
            _ => panic!("expected chat"),
        }

        let mut resp = SessionState::new(ApiFormat::Responses);
        resp.note_user_shell("pwd", "/tmp");
        let pending = resp.take_pending_context().unwrap();
        assert!(pending.contains("pwd"));
        assert!(resp.take_pending_context().is_none());
    }

    #[test]
    fn jsonl_roundtrip() {
        use super::{Session, load_sessions_from, write_sessions_jsonl};
        use std::time::{SystemTime, UNIX_EPOCH};

        let dir = std::env::temp_dir().join(format!(
            "nano-session-test-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.jsonl");
        let sessions = vec![Session {
            id: "a".into(),
            label: "hello".into(),
            cwd: "/tmp".into(),
            ts: 1,
            format: Some(ApiFormat::ChatCompletions),
            messages: Some(vec![serde_json::json!({"role": "user", "content": "hi"})]),
        }];
        write_sessions_jsonl(&path, &sessions).unwrap();
        let loaded = load_sessions_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].label, "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
