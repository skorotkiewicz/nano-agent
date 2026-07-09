//! API interaction: request sending (with spinner) and the tool-call loops
//! for both the Responses API and the Chat Completions API.

use crate::policy::{expose_execute_shell_tools, expose_mcp_tools};
use crate::prompt::get_system;
use crate::provider::{ApiTarget, get_api_target};
use crate::state::{color, get_config, get_max_steps, get_mcp_client, is_tty};
use crate::tools::{ToolCancelled, dispatch_tool_call, get_tool_chat, get_tool_responses};
#[cfg(feature = "acp")]
use crate::{
    policy::expose_acp_delegate_tools,
    tools::{get_acp_delegate_tools_chat, get_acp_delegate_tools_responses},
};
use reqwest::Client;
use std::io::{self, Write};
use tokio::time::Duration;

/// `(answer, session_id, chat_messages)` for one completed turn.
pub type TurnOutcome =
    Result<(String, Option<String>, Option<Vec<serde_json::Value>>), TurnCancelled>;

#[derive(Debug, Clone, Copy)]
pub struct TurnCancelled;

impl From<ToolCancelled> for TurnCancelled {
    fn from(_: ToolCancelled) -> Self {
        Self
    }
}

async fn respond_api(
    client: &Client,
    target: &ApiTarget,
    body: serde_json::Value,
) -> Result<serde_json::Value, reqwest::Error> {
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

    let mut req = client
        .post(&target.url)
        .header("Content-Type", "application/json");

    if !target.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", target.api_key));
    }

    let res = req.json(&body).send().await?.json().await;

    let _ = tx.send(true);
    if let Some(h) = spinner_handle {
        let _ = h.await;
    }

    res
}

fn log_tool_call(name: &str, args: &str) {
    if is_tty() {
        eprintln!(
            "{}",
            color("90", &format!("→ tool call: {} {}", name, args))
        );
    }
}

// --- Responses API Mode ---

async fn respond_responses(
    client: &Client,
    payload: serde_json::Value,
    previous: Option<&str>,
) -> Result<serde_json::Value, reqwest::Error> {
    let target = get_api_target();
    let mut tools: Vec<serde_json::Value> = Vec::new();
    if expose_execute_shell_tools() {
        tools.push(get_tool_responses().clone());
    }
    #[cfg(feature = "acp")]
    if expose_acp_delegate_tools() {
        tools.extend(get_acp_delegate_tools_responses().await);
    }
    if expose_mcp_tools() {
        tools.extend(get_mcp_client().get_tools_schema().await);
    }

    let mut body = serde_json::json!({
        "model": target.model.as_str(),
        "instructions": get_system(),
        "tools": tools,
        "input": payload
    });
    if let Some(prev) = previous {
        body["previous_response_id"] = serde_json::Value::String(prev.to_string());
    }
    if let Some(n) = get_config().get_max_tokens() {
        body["max_tokens"] = n.into();
    }
    if let Some(t) = get_config().get_temperature() {
        body["temperature"] = t.into();
    }
    respond_api(client, &target, body).await
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

async fn tool_output_responses(
    call: &serde_json::Value,
) -> Result<serde_json::Value, ToolCancelled> {
    let call_id = call
        .get("call_id")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args_str = call
        .get("arguments")
        .and_then(|a| a.as_str())
        .unwrap_or("{}");
    let result = dispatch_tool_call(name, args_str).await?;

    Ok(serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": result
    }))
}

pub async fn run_responses(client: &Client, prompt: &str, previous: Option<&str>) -> TurnOutcome {
    let payload = serde_json::json!([{"type": "message", "role": "user", "content": prompt}]);
    let mut messages = vec![serde_json::json!({"role": "user", "content": prompt})];

    let mut response = match respond_responses(client, payload, previous).await {
        Ok(r) => r,
        Err(e) => {
            let err = format!("API Error: {}", e);
            messages.push(serde_json::json!({"role": "assistant", "content": err}));
            return Ok((err, None, Some(messages)));
        }
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
            let answer = text(&response);
            messages.push(serde_json::json!({"role": "assistant", "content": answer}));
            return Ok((answer, prev_id, Some(messages)));
        }

        let mut outputs = Vec::new();
        let mut tool_names: Vec<&str> = Vec::new();
        for call in &calls {
            let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = call
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            log_tool_call(name, args);
            tool_names.push(name);
            outputs.push(tool_output_responses(call).await?);
        }
        messages.push(serde_json::json!({
            "role": "assistant",
            "tool_calls": tool_names
                .iter()
                .map(|n| serde_json::json!({"function": {"name": n}}))
                .collect::<Vec<_>>()
        }));

        response = match respond_responses(
            client,
            serde_json::Value::Array(outputs),
            prev_id.as_deref(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let err = format!("API Error: {}", e);
                messages.push(serde_json::json!({"role": "assistant", "content": err}));
                return Ok((err, prev_id, Some(messages)));
            }
        };
        prev_id = response
            .get("id")
            .and_then(|i| i.as_str())
            .map(String::from);
    }

    let stopped = "stopped: too many tool calls".to_string();
    messages.push(serde_json::json!({"role": "assistant", "content": stopped}));
    Ok((stopped, prev_id, Some(messages)))
}

// --- Chat Completions API Mode ---

async fn respond_chat_with_target(
    client: &Client,
    messages: &[serde_json::Value],
    target: &ApiTarget,
) -> Result<serde_json::Value, reqwest::Error> {
    let mut tools = Vec::new();
    if expose_execute_shell_tools() {
        tools.push(serde_json::json!({"type": "function", "function": get_tool_chat()}));
    }
    #[cfg(feature = "acp")]
    if expose_acp_delegate_tools() {
        for tool in get_acp_delegate_tools_chat().await {
            tools.push(serde_json::json!({"type": "function", "function": tool}));
        }
    }
    if expose_mcp_tools() {
        for tool in get_mcp_client().get_tools_schema().await {
            tools.push(serde_json::json!({"type": "function", "function": tool}));
        }
    }
    let mut body = serde_json::json!({
        "model": target.model.as_str(),
        "messages": messages,
        "tools": tools
    });
    if let Some(n) = get_config().get_max_tokens() {
        body["max_tokens"] = n.into();
    }
    if let Some(t) = get_config().get_temperature() {
        body["temperature"] = t.into();
    }
    respond_api(client, target, body).await
}

async fn tool_output_chat(
    name: &str,
    args_str: &str,
    call_id: &str,
) -> Result<serde_json::Value, ToolCancelled> {
    let result = dispatch_tool_call(name, args_str).await?;

    Ok(serde_json::json!({
        "role": "tool",
        "tool_call_id": call_id,
        "content": result
    }))
}

pub async fn run_chat(
    client: &Client,
    prompt: &str,
    messages: Vec<serde_json::Value>,
) -> TurnOutcome {
    let target = get_api_target();
    run_chat_with_system(client, prompt, messages, &get_system(), &target).await
}

pub async fn run_chat_with_system(
    client: &Client,
    prompt: &str,
    mut messages: Vec<serde_json::Value>,
    system: &str,
    target: &ApiTarget,
) -> TurnOutcome {
    if messages.is_empty() {
        messages.push(serde_json::json!({"role": "system", "content": system}));
    }
    messages.push(serde_json::json!({"role": "user", "content": prompt}));

    let mut response = match respond_chat_with_target(client, &messages, target).await {
        Ok(r) => r,
        Err(e) => return Ok((format!("API Error: {}", e), None, Some(messages))),
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

        messages.push(msg);

        if tool_calls.is_empty() {
            return Ok((
                text_content,
                Some("chat-session".to_string()),
                Some(messages),
            ));
        }

        for call in &tool_calls {
            let call_id = call.get("id").and_then(|c| c.as_str()).unwrap_or("call_1");
            let func = call.get("function").cloned().unwrap_or_default();
            let name = func.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args_str = func
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");

            log_tool_call(name, args_str);

            let output = tool_output_chat(name, args_str, call_id).await?;
            messages.push(output);
        }

        response = match respond_chat_with_target(client, &messages, target).await {
            Ok(r) => r,
            Err(e) => {
                return Ok((
                    format!("API Error: {}", e),
                    Some("chat-session".to_string()),
                    Some(messages),
                ));
            }
        };
    }

    Ok((
        "stopped: too many tool calls".to_string(),
        Some("chat-session".to_string()),
        Some(messages),
    ))
}
