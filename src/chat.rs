//! API interaction: request sending (with spinner) and the tool-call loops
//! for both the Responses API and the Chat Completions API.

use crate::policy::{expose_execute_shell_tools, expose_mcp_tools};
use crate::prompt::get_system;
use crate::provider::{ApiTarget, apply_generation_controls, get_api_target};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnCancelled {
    User,
}

impl TurnCancelled {
    pub fn should_report(self) -> bool {
        !matches!(self, Self::User)
    }
}

impl From<ToolCancelled> for TurnCancelled {
    fn from(value: ToolCancelled) -> Self {
        match value {
            ToolCancelled::User => Self::User,
        }
    }
}

/// Reject non-2xx / error-shaped bodies so a 401/429/proxy HTML page doesn't
/// look like an empty model reply.
fn parse_api_body(status: reqwest::StatusCode, raw: &str) -> Result<serde_json::Value, String> {
    let body: serde_json::Value = match serde_json::from_str(raw) {
        Ok(body) => body,
        Err(_) if status.is_success() => {
            return Err(format!("invalid JSON response: {}", truncate_for_error(raw)));
        }
        Err(_) => {
            return Err(format!("{status} {}", truncate_for_error(raw)));
        }
    };

    if !status.is_success() {
        return Err(format!("{status} {body}"));
    }
    // Some OpenAI-compat proxies return HTTP 200 with {"error": ...}.
    if body.get("error").is_some() && body.get("choices").is_none() && body.get("output").is_none()
    {
        return Err(format!("{body}"));
    }
    Ok(body)
}

fn truncate_for_error(text: &str) -> String {
    const MAX: usize = 500;
    if text.len() <= MAX {
        return text.to_string();
    }
    let mut end = MAX;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

async fn respond_api(
    client: &Client,
    target: &ApiTarget,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
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

    let res = async {
        let http = req.json(&body).send().await.map_err(|e| e.to_string())?;
        let status = http.status();
        let raw = http.text().await.map_err(|e| e.to_string())?;
        parse_api_body(status, &raw)
    }
    .await;

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
) -> Result<serde_json::Value, String> {
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
    apply_generation_controls(
        &mut body,
        target.format,
        get_config().get_max_tokens(),
        get_config().get_temperature(),
    );
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
) -> Result<(serde_json::Value, serde_json::Value), ToolCancelled> {
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

    Ok((
        serde_json::json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": result
        }),
        serde_json::json!({
            "role": "tool",
            "name": name,
            "content": result
        }),
    ))
}

pub async fn run_responses(client: &Client, prompt: &str, previous: Option<&str>) -> TurnOutcome {
    let payload = serde_json::json!([{"type": "message", "role": "user", "content": prompt}]);
    let mut messages = vec![serde_json::json!({"role": "user", "content": prompt})];

    // Keep the last known Responses id on failure so mid-turn API errors don't
    // wipe previous_response_id and break -c/-s resume.
    let mut prev_id = previous.map(String::from);

    let mut response = match respond_responses(client, payload, previous).await {
        Ok(r) => r,
        Err(e) => {
            let err = format!("API Error: {}", e);
            messages.push(serde_json::json!({"role": "assistant", "content": err}));
            return Ok((err, prev_id, Some(messages)));
        }
    };

    if let Some(id) = response.get("id").and_then(|i| i.as_str()) {
        prev_id = Some(id.to_string());
    }

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
        let mut tool_invocations = Vec::new();
        let mut tool_messages = Vec::new();
        for call in &calls {
            let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = call
                .get("arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            log_tool_call(name, args);
            tool_invocations.push((name, args));
            let (output, message) = tool_output_responses(call).await?;
            outputs.push(output);
            tool_messages.push(message);
        }
        messages.push(serde_json::json!({
            "role": "assistant",
            "tool_calls": tool_invocations
                .iter()
                .map(|(n, a)| serde_json::json!({"function": {"name": n, "arguments": a}}))
                .collect::<Vec<_>>()
        }));
        messages.extend(tool_messages);

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
        if let Some(id) = response.get("id").and_then(|i| i.as_str()) {
            prev_id = Some(id.to_string());
        }
    }

    let stopped = "stopped: too many tool calls".to_string();
    messages.push(serde_json::json!({"role": "assistant", "content": stopped}));
    Ok((stopped, prev_id, Some(messages)))
}

// --- Chat Completions API Mode ---

/// Keep the first system message current (harness overlay, diy sandbox policy, cwd).
fn ensure_system_message(messages: &mut Vec<serde_json::Value>, system: &str) {
    let system_msg = serde_json::json!({"role": "system", "content": system});
    if messages
        .first()
        .and_then(|m| m.get("role"))
        .and_then(|r| r.as_str())
        == Some("system")
    {
        messages[0] = system_msg;
    } else {
        messages.insert(0, system_msg);
    }
}

async fn respond_chat_with_target(
    client: &Client,
    messages: &[serde_json::Value],
    target: &ApiTarget,
) -> Result<serde_json::Value, String> {
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
        for mut tool in get_mcp_client().get_tools_schema().await {
            if let Some(obj) = tool.as_object_mut() {
                obj.remove("type");
            }
            tools.push(serde_json::json!({"type": "function", "function": tool}));
        }
    }
    let mut body = serde_json::json!({
        "model": target.model.as_str(),
        "messages": messages,
        "tools": tools
    });
    apply_generation_controls(
        &mut body,
        target.format,
        get_config().get_max_tokens(),
        get_config().get_temperature(),
    );
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
    ensure_system_message(&mut messages, system);
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

#[cfg(test)]
mod tests {
    use super::{TurnCancelled, ensure_system_message, parse_api_body};
    use crate::tools::ToolCancelled;

    #[test]
    fn user_tool_cancel_is_silent() {
        let cancelled: TurnCancelled = ToolCancelled::User.into();
        assert!(!cancelled.should_report());
    }

    #[test]
    fn api_error_status_is_rejected() {
        let err = parse_api_body(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error": {"message": "bad key"}}"#,
        )
        .unwrap_err();
        assert!(err.contains("401"));
        assert!(err.contains("bad key"));

        assert!(parse_api_body(reqwest::StatusCode::OK, r#"{"ok": true}"#).is_ok());

        let html = parse_api_body(reqwest::StatusCode::BAD_GATEWAY, "<html>bad gateway</html>")
            .unwrap_err();
        assert!(html.contains("502"));

        let soft = parse_api_body(
            reqwest::StatusCode::OK,
            r#"{"error": {"message": "quota"}}"#,
        )
        .unwrap_err();
        assert!(soft.contains("quota"));
    }

    #[test]
    fn ensure_system_message_refreshes_first_system() {
        let mut messages = vec![
            serde_json::json!({"role": "system", "content": "old"}),
            serde_json::json!({"role": "user", "content": "hi"}),
        ];
        ensure_system_message(&mut messages, "new harness");
        assert_eq!(messages[0]["content"], "new harness");
        assert_eq!(messages.len(), 2);

        let mut empty = vec![];
        ensure_system_message(&mut empty, "fresh");
        assert_eq!(empty[0]["role"], "system");
        assert_eq!(empty[0]["content"], "fresh");
    }
}
