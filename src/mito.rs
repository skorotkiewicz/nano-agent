//! Mito: a local planning agent with its own context that prepares a
//! detailed handoff prompt for the primary LLM.

use crate::chat::run_chat_with_system;
use crate::prompt::{doc_names, find_files};
use crate::provider::get_mito_target;
use crate::session::SessionState;
use crate::state::{/*color,*/ context_cwd};
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

/// Extract the handoff prompt the mito planner emits, if any.
///
/// The planner is asked to output exactly one handoff beginning with `MITO_SEND:`.
/// We accept the prefix in two positions so the model may think out loud first:
///   1. At the very start of the response (after leading whitespace), or
///   2. Immediately after a blank line — i.e. `MITO_SEND:` is the first non-blank
///      line of a paragraph that follows an empty line. This lets the model emit
///      reasoning/explanation above the handoff without breaking parsing.
fn extract_mito_handoff(answer: &str) -> Option<String> {
    const PREFIX: &str = "MITO_SEND:";
    fn parse_handoff(text: &str) -> Option<String> {
        let handoff = text.trim();
        (!handoff.is_empty()).then(|| handoff.to_string())
    }

    let trimmed = answer.trim_start();
    if let Some(handoff) = trimmed.strip_prefix(PREFIX) {
        return parse_handoff(handoff);
    }

    let mut offset = 0;
    let mut previous_blank = false;
    for line in answer.split('\n') {
        if previous_blank {
            let leading = line.len() - line.trim_start().len();
            if line.trim_start().starts_with(PREFIX) {
                return parse_handoff(&answer[offset + leading + PREFIX.len()..]);
            }
        }
        previous_blank = line.trim().is_empty();
        offset += line.len() + 1;
    }

    None
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
         When you are ready for the primary LLM to do the work, output exactly one handoff and no other text.\n\
         Start the handoff with MITO_SEND: then use this short schema (skip empty sections):\n\
         Objective: one sentence done-when\n\
         Context: paths, facts, prior decisions the primary LLM needs\n\
         Constraints: hard limits (no, barely, formats, platforms)\n\
         Steps: ordered work the primary LLM should do\n\
         Done when: concrete acceptance checks\n\
         cwd: {}\n\
         Important docs (read as needed): {}",
        cwd, docs
    )
}

fn format_mito_handoff_result(handoff: &str, main_answer: &str) -> String {
    if main_answer.is_empty() {
        String::new()
    } else {
        format!("mito > {}\n{}", handoff, main_answer)
    }
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
        match run_chat_with_system(client, prompt, mito_messages.clone(), &system, &target).await {
            Ok(result) => result,
            Err(_cancelled) => {
                // if cancelled.should_report() {
                //     eprintln!("{}", color("90", "cancelled"));
                // }
                return String::new();
            }
        };

    if let Some(messages) = new_messages {
        *mito_messages = messages;
    }

    let Some(handoff) = extract_mito_handoff(&answer) else {
        return format!("mito > {}", answer);
    };

    let main_answer = run_state_turn(client, &handoff, main_state, main_label, prompt).await;
    format_mito_handoff_result(&handoff, &main_answer)
}

#[cfg(test)]
mod tests {
    use super::{extract_mito_handoff, format_mito_handoff_result, strip_mito_prefix};

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
        assert_eq!(
            extract_mito_handoff("\n  MITO_SEND: implement the feature").as_deref(),
            Some("implement the feature")
        );
        assert_eq!(extract_mito_handoff("MITO_SEND:   "), None);
        assert_eq!(
            extract_mito_handoff("first ask a question\n\nMITO_SEND: implement the feature")
                .as_deref(),
            Some("implement the feature")
        );
        assert_eq!(
            extract_mito_handoff(
                "first ask a question\n\nMITO_SEND: implement the feature\nwith more detail"
            )
            .as_deref(),
            Some("implement the feature\nwith more detail")
        );
        assert_eq!(
            extract_mito_handoff("first ask a question\nMITO_SEND: implement the feature"),
            None
        );
        assert_eq!(extract_mito_handoff("ask a question first"), None);
    }

    #[test]
    fn handoff_result_stays_silent_when_main_turn_is_empty() {
        assert_eq!(format_mito_handoff_result("implement the feature", ""), "");
        assert_eq!(
            format_mito_handoff_result("implement the feature", "done"),
            "mito > implement the feature\ndone"
        );
    }
}
