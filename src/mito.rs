//! Mito: a local planning agent with its own context that prepares a
//! detailed handoff prompt for the primary LLM.

use crate::chat::run_chat_with_system;
use crate::prompt::{doc_names, find_files};
use crate::provider::get_mito_target;
use crate::session::SessionState;
use crate::state::context_cwd;
use crate::turn::run_state_turn;
use reqwest::Client;

pub fn strip_mito_prefix(prompt: &str) -> Option<&str> {
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
    let docs = find_files(vec![cwd.clone()], doc_names(), 40);

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

pub async fn run_mito_turn(
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
