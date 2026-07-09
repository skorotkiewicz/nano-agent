//! Turn orchestration: run one prompt through the configured API format and
//! persist the resulting session state.

use crate::chat::{run_chat, run_responses};
use crate::provider::ApiFormat;
#[cfg(feature = "acp")]
use crate::provider::get_api_target;
use crate::session::{SessionState, append_session_messages, save_session};
use crate::state::color;
use reqwest::Client;

pub async fn run_state_turn(
    client: &Client,
    prompt: &str,
    state: &mut SessionState,
    label: &mut Option<String>,
    label_prompt: &str,
) -> String {
    let result = match state {
        SessionState::Responses { previous, .. } => {
            run_responses(client, prompt, previous.as_deref()).await
        }
        SessionState::Chat { messages } => run_chat(client, prompt, messages.clone()).await,
    };

    let (answer, prev_id, new_messages) = match result {
        Ok(values) => values,
        Err(cancelled) => {
            if cancelled.should_report() {
                eprintln!("{}", color("90", "cancelled"));
            }
            return String::new();
        }
    };
    let session_label = label.as_deref().unwrap_or(label_prompt);

    match state {
        SessionState::Responses { previous, messages } => {
            if let Some(ref id) = prev_id {
                let merged_messages = append_session_messages(messages, new_messages);
                save_session(id, session_label, ApiFormat::Responses, merged_messages);
            }
            *previous = prev_id;
        }
        SessionState::Chat { messages } => {
            if let Some(msgs) = new_messages {
                save_session(
                    "chat-session",
                    session_label,
                    ApiFormat::ChatCompletions,
                    Some(msgs.clone()),
                );
                *messages = msgs;
            }
        }
    }

    if label.is_none() {
        *label = Some(label_prompt.to_string());
    }

    answer
}

/// One stateless turn: no session persistence, no conversation carry-over.
#[cfg(feature = "acp")]
pub async fn run_single_turn(client: &Client, prompt: &str) -> String {
    let result = match get_api_target().format {
        ApiFormat::Responses => run_responses(client, prompt, None).await,
        ApiFormat::ChatCompletions => run_chat(client, prompt, vec![]).await,
    };
    match result {
        Ok((answer, _, _)) => answer,
        Err(cancelled) => {
            if cancelled.should_report() {
                eprintln!("{}", color("90", "cancelled"));
            }
            String::new()
        }
    }
}
