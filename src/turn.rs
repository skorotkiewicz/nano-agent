//! Turn orchestration: run one prompt through the configured API format and
//! persist the resulting session state.

use crate::chat::{run_chat, run_responses};
#[cfg(feature = "acp")]
use crate::provider::{ApiFormat, get_api_target};
use crate::session::{SessionState, save_session};
use reqwest::Client;

pub async fn run_state_turn(
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

/// One stateless turn: no session persistence, no conversation carry-over.
#[cfg(feature = "acp")]
pub async fn run_single_turn(client: &Client, prompt: &str) -> String {
    let (answer, _, _) = match get_api_target().format {
        ApiFormat::Responses => run_responses(client, prompt, None).await,
        ApiFormat::ChatCompletions => run_chat(client, prompt, vec![]).await,
    };
    answer
}
