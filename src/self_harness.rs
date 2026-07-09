//! Self-Harness: propose one prompt-overlay edit, keep it only after validation.

use crate::prompt;
use crate::provider::{ApiFormat, apply_generation_controls, get_api_target};
use crate::session::sessions_in_cwd;
use crate::state::{get_config, truncate_tail};
use reqwest::Client;
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tokio::time::timeout;

const HARNESS_DIR: &str = ".nano";
const ACTIVE_HARNESS: &str = "harness.md";
const SELF_HARNESS_DIR: &str = "self-harness";
const CANDIDATE_HARNESS: &str = "candidate.md";
const LOG: &str = "log.jsonl";
const MAX_EVIDENCE_BYTES: usize = 10_000;
const MAX_HARNESS_BYTES: usize = 4_000;
const MAX_REJECTION_HISTORY: usize = 3;
const VALIDATION_TIMEOUT_SECS: u64 = 600;

#[derive(Debug)]
struct Candidate {
    rationale: String,
    harness: String,
}

#[derive(Debug)]
struct ValidationResult {
    success: bool,
    code: Option<i32>,
    output: String,
}

pub fn strip_self_harness_prefix(prompt: &str) -> Option<&str> {
    let trimmed = prompt.trim_start();
    let rest = trimmed.strip_prefix("/self-harness")?;
    rest.chars()
        .next()
        .is_some_and(char::is_whitespace)
        .then(|| rest.trim())
        .filter(|command| !command.is_empty())
}

pub fn active_harness_path(cwd: &Path) -> PathBuf {
    cwd.join(HARNESS_DIR).join(ACTIVE_HARNESS)
}

pub fn load_active_harness(cwd: &Path) -> Option<String> {
    let text = fs::read_to_string(active_harness_path(cwd)).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

pub async fn run_self_harness(client: &Client, validation_command: &str) -> String {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let evidence = evidence_bundle();
    if evidence.trim().is_empty() {
        return "self-harness: no recent session evidence in this directory".to_string();
    }

    let current_harness = load_active_harness(&cwd).unwrap_or_else(|| "none".to_string());
    let rejected_history = recent_rejections(&cwd);
    let prompt_text = proposal_prompt(
        &current_harness,
        &evidence,
        &rejected_history,
        validation_command,
    );
    let system = format!(
        "{}\n\nSelf-Harness proposer mode: mine recurring failures from the evidence, propose one minimal prompt-overlay edit, and do not call tools.",
        prompt::get_system()
    );

    let response = match ask_model(client, &system, &prompt_text).await {
        Ok(response) => response,
        Err(error) => return format!("self-harness proposal failed: {error}"),
    };
    let candidate = match parse_candidate_response(&response) {
        Ok(candidate) => candidate,
        Err(error) => return format!("self-harness rejected proposal: {error}"),
    };

    match validate_and_promote(&cwd, validation_command, &candidate).await {
        Ok(result) if result.success => {
            log_decision(&cwd, true, validation_command, &candidate, &result);
            format!(
                "self-harness accepted: {}\nvalidator exit: {}",
                candidate.rationale,
                result.code.unwrap_or(0)
            )
        }
        Ok(result) => {
            log_decision(&cwd, false, validation_command, &candidate, &result);
            format!(
                "self-harness rejected: validator exit {}\n{}",
                result.code.unwrap_or(-1),
                result.output
            )
        }
        Err(error) => format!("self-harness failed: {error}"),
    }
}

fn proposal_prompt(
    current_harness: &str,
    evidence: &str,
    rejected_history: &str,
    validation_command: &str,
) -> String {
    let rejected_section = if rejected_history.trim().is_empty() {
        String::new()
    } else {
        format!("\nRecent rejected harness attempts:\n{rejected_history}\n")
    };

    format!(
        "Use the Self-Harness loop from arXiv:2606.09498.\n\
         Weakness Mining: cluster recurring failures in the evidence.\n\
         Harness Proposal: produce one minimal prompt-overlay edit tied to those failures.\n\
         Only act on weaknesses that recur at least twice in the evidence.\n\
         Ignore one-off failures and style-only rewrites.\n\
         Prefer behavior-changing process checks over wording cleanups.\n\
         Proposal Validation: this program will temporarily install your overlay and run: {validation_command}\n\n\
         Current prompt overlay:\n{current_harness}\n\n\
         Evidence bundle:\n{evidence}{rejected_section}\n\
         Return exactly:\n\
         WHY: one sentence naming the recurring weakness and why this edit targets it\n\
         HARNESS:\n\
         <plain markdown instructions for Nano, max {MAX_HARNESS_BYTES} bytes>\n\
         END"
    )
}

async fn ask_model(client: &Client, system: &str, prompt_text: &str) -> Result<String, String> {
    let target = get_api_target();
    let mut body = match target.format {
        ApiFormat::Responses => serde_json::json!({
            "model": target.model,
            "instructions": system,
            "input": [{"type": "message", "role": "user", "content": prompt_text}]
        }),
        ApiFormat::ChatCompletions => serde_json::json!({
            "model": target.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": prompt_text}
            ]
        }),
    };
    apply_generation_controls(
        &mut body,
        target.format,
        get_config().get_max_tokens(),
        get_config().get_temperature(),
    );

    let mut req = client
        .post(&target.url)
        .header("Content-Type", "application/json");
    if !target.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", target.api_key));
    }

    let http = req.json(&body).send().await.map_err(|e| e.to_string())?;
    let status = http.status();
    let response: Value = http.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("API Error: {status} {response}"));
    }

    let text = match target.format {
        ApiFormat::Responses => response_text(&response),
        ApiFormat::ChatCompletions => chat_text(&response),
    };
    if text.trim().is_empty() {
        Err(format!("empty proposal response: {response}"))
    } else {
        Ok(text)
    }
}

fn response_text(response: &Value) -> String {
    response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("")
}

fn chat_text(response: &Value) -> String {
    response
        .get("choices")
        .and_then(|choices| choices.get(0))
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn parse_candidate_response(response: &str) -> Result<Candidate, String> {
    let (_, after_why) = response
        .split_once("WHY:")
        .ok_or_else(|| "missing WHY marker".to_string())?;
    let (rationale, after_harness) = after_why
        .split_once("HARNESS:")
        .ok_or_else(|| "missing HARNESS marker".to_string())?;
    let harness = strip_fence(
        after_harness
            .split_once("\nEND")
            .map(|(harness, _)| harness)
            .unwrap_or(after_harness),
    )
    .trim();

    if harness.is_empty() {
        return Err("empty harness edit".to_string());
    }
    if harness.len() > MAX_HARNESS_BYTES {
        return Err(format!(
            "harness edit is {} bytes; max is {MAX_HARNESS_BYTES}",
            harness.len()
        ));
    }

    Ok(Candidate {
        rationale: rationale.trim().to_string(),
        harness: harness.to_string(),
    })
}

fn strip_fence(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(without_opening) = trimmed.strip_prefix("```") else {
        return trimmed;
    };
    let without_lang = without_opening
        .split_once('\n')
        .map(|(_, rest)| rest)
        .unwrap_or(without_opening);
    without_lang
        .split_once("\nEND")
        .map(|(harness, _)| harness)
        .unwrap_or(without_lang)
        .trim()
        .trim_end_matches("```")
        .trim()
}

fn evidence_bundle() -> String {
    let mut out = String::new();
    let mut sessions = sessions_in_cwd();
    sessions.reverse();

    for session in sessions.into_iter().take(8) {
        let mut session_out = String::new();
        match session.messages {
            Some(messages) => summarize_messages(&mut session_out, &messages),
            None => continue,
        }
        if session_out.trim().is_empty() {
            continue;
        }
        push_capped(
            &mut out,
            &format!(
                "\nSESSION label={} ts={}\n",
                session.label.replace('\n', " "),
                session.ts
            ),
        );
        push_capped(&mut out, &session_out);
        if out.len() >= MAX_EVIDENCE_BYTES {
            break;
        }
    }

    out
}

fn summarize_messages(out: &mut String, messages: &[Value]) {
    for message in messages.iter().rev().take(20).rev() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let role_label = if role == "tool" {
            message
                .get("name")
                .and_then(Value::as_str)
                .map(|name| format!("tool({name})"))
                .unwrap_or_else(|| role.to_string())
        } else {
            role.to_string()
        };
        let mut line = format!("{role_label}: ");
        if let Some(content) = message.get("content").and_then(Value::as_str) {
            line.push_str(content);
        } else if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            let names = calls
                .iter()
                .filter_map(|call| call.get("function"))
                .filter_map(|function| function.get("name"))
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            line.push_str(&format!("tool calls: {names}"));
        } else {
            line.push_str(&message.to_string());
        }
        if !is_failure_signal(&line) {
            continue;
        }
        line.push('\n');
        push_capped(out, &truncate_tail(&line, 800));
        if out.len() >= MAX_EVIDENCE_BYTES {
            break;
        }
    }
}

fn is_failure_signal(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "api error:",
        "executionerror:",
        "timeout after",
        "denied by user",
        "denied:",
        "stopped: too many tool calls",
        "unknown tool",
        "bad arguments:",
        "failed:",
        "mito error:",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || has_nonzero_exit(text)
}

fn has_nonzero_exit(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim()
            .strip_prefix("exit ")
            .and_then(|code| code.parse::<i32>().ok())
            .is_some_and(|code| code != 0)
    })
}

fn recent_rejections(cwd: &Path) -> String {
    let path = cwd.join(HARNESS_DIR).join(SELF_HARNESS_DIR).join(LOG);
    let text = fs::read_to_string(path).unwrap_or_default();
    recent_rejections_from_log(&text)
}

fn recent_rejections_from_log(text: &str) -> String {
    let mut entries = Vec::new();
    for line in text.lines().rev() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if value.get("accepted").and_then(Value::as_bool) != Some(false) {
            continue;
        }

        let rationale = value
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let tried = value
            .get("harness")
            .and_then(Value::as_str)
            .and_then(|harness| harness.lines().find(|line| !line.trim().is_empty()))
            .unwrap_or("")
            .trim();
        let code = value
            .get("validation_code")
            .and_then(Value::as_i64)
            .map(|code| code.to_string())
            .unwrap_or_else(|| "?".to_string());

        if rationale.is_empty() && tried.is_empty() {
            continue;
        }

        let summary = if tried.is_empty() {
            format!("- exit {code}: {rationale}")
        } else if rationale.is_empty() {
            format!("- exit {code}: tried `{tried}`")
        } else {
            format!("- exit {code}: {rationale}; tried `{tried}`")
        };
        entries.push(summary);
        if entries.len() == MAX_REJECTION_HISTORY {
            break;
        }
    }

    entries.reverse();
    entries.join("\n")
}

fn push_capped(out: &mut String, text: &str) {
    if out.len() >= MAX_EVIDENCE_BYTES {
        return;
    }
    let remaining = MAX_EVIDENCE_BYTES - out.len();
    out.push_str(&truncate_tail(text, remaining));
}

async fn validate_and_promote(
    cwd: &Path,
    validation_command: &str,
    candidate: &Candidate,
) -> Result<ValidationResult, String> {
    let active_path = active_harness_path(cwd);
    let previous = fs::read_to_string(&active_path).ok();
    let candidate_dir = cwd.join(HARNESS_DIR).join(SELF_HARNESS_DIR);
    fs::create_dir_all(&candidate_dir).map_err(|e| e.to_string())?;
    fs::write(candidate_dir.join(CANDIDATE_HARNESS), &candidate.harness)
        .map_err(|e| e.to_string())?;
    fs::write(&active_path, &candidate.harness).map_err(|e| e.to_string())?;
    prompt::clear_system_cache();

    let result = run_validation(cwd, validation_command).await;
    if !result.success {
        match previous {
            Some(previous) => fs::write(&active_path, previous).map_err(|e| e.to_string())?,
            None => {
                let _ = fs::remove_file(&active_path);
            }
        }
        prompt::clear_system_cache();
    }
    Ok(result)
}

async fn run_validation(cwd: &Path, validation_command: &str) -> ValidationResult {
    let output = timeout(
        Duration::from_secs(VALIDATION_TIMEOUT_SECS),
        Command::new("sh")
            .arg("-c")
            .arg(validation_command)
            .current_dir(cwd)
            .output(),
    )
    .await;

    match output {
        Ok(Ok(output)) => {
            let mut combined = String::new();
            combined.push_str(&String::from_utf8_lossy(&output.stdout));
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            ValidationResult {
                success: output.status.success(),
                code: output.status.code(),
                output: truncate_tail(&combined, 8_000),
            }
        }
        Ok(Err(error)) => ValidationResult {
            success: false,
            code: None,
            output: error.to_string(),
        },
        Err(_) => ValidationResult {
            success: false,
            code: None,
            output: format!("timeout after {VALIDATION_TIMEOUT_SECS}s"),
        },
    }
}

fn log_decision(
    cwd: &Path,
    accepted: bool,
    validation_command: &str,
    candidate: &Candidate,
    result: &ValidationResult,
) {
    let log_dir = cwd.join(HARNESS_DIR).join(SELF_HARNESS_DIR);
    if fs::create_dir_all(&log_dir).is_err() {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let record = serde_json::json!({
        "ts": ts,
        "accepted": accepted,
        "validation_command": validation_command,
        "validation_code": result.code,
        "rationale": candidate.rationale,
        "harness": candidate.harness,
    });
    let line = match serde_json::to_string(&record) {
        Ok(line) => line,
        Err(_) => return,
    };
    let path = log_dir.join(LOG);
    let mut old = fs::read_to_string(&path).unwrap_or_default();
    old.push_str(&line);
    old.push('\n');
    let _ = fs::write(path, old);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_self_harness_prefix() {
        assert_eq!(
            strip_self_harness_prefix("  /self-harness cargo test"),
            Some("cargo test")
        );
        assert_eq!(strip_self_harness_prefix("/self-harness"), None);
        assert_eq!(strip_self_harness_prefix("hello"), None);
    }

    #[test]
    fn parses_candidate_markers() {
        let parsed = parse_candidate_response(
            "WHY: repeated missing artifact failures\nHARNESS:\nBefore final answer, verify required artifacts exist.\nEND",
        )
        .unwrap();
        assert_eq!(parsed.rationale, "repeated missing artifact failures");
        assert_eq!(
            parsed.harness,
            "Before final answer, verify required artifacts exist."
        );
    }

    #[test]
    fn strips_candidate_markdown_fence() {
        let parsed = parse_candidate_response(
            "WHY: too many retries\nHARNESS:\n```markdown\nRetry once.\n```\nEND",
        )
        .unwrap();
        assert_eq!(parsed.harness, "Retry once.");
    }

    #[test]
    fn rejects_empty_candidate() {
        let err = parse_candidate_response("WHY: no evidence\nHARNESS:\nEND").unwrap_err();
        assert_eq!(err, "empty harness edit");
    }

    #[test]
    fn truncates_on_utf8_boundary() {
        assert_eq!(truncate_tail("aébc", 4), "ébc");
    }

    #[test]
    fn summarize_messages_includes_tool_name() {
        let mut out = String::new();
        summarize_messages(
            &mut out,
            &[serde_json::json!({
                "role": "tool",
                "name": "execute_shell",
                "content": "ExecutionError: ls failed"
            })],
        );

        assert!(out.contains("tool(execute_shell): ExecutionError: ls failed"));
    }

    #[test]
    fn summarize_messages_skips_non_failure_noise() {
        let mut out = String::new();
        summarize_messages(
            &mut out,
            &[
                serde_json::json!({"role": "user", "content": "hello"}),
                serde_json::json!({"role": "assistant", "content": "all good"}),
                serde_json::json!({"role": "assistant", "content": "API Error: boom"}),
            ],
        );

        assert!(!out.contains("hello"));
        assert!(!out.contains("all good"));
        assert!(out.contains("API Error: boom"));
    }

    #[test]
    fn proposal_prompt_mentions_recurrence_and_rejections() {
        let prompt = proposal_prompt(
            "none",
            "assistant: API Error: boom",
            "- exit 1: repeated failure; tried `Verify artifacts exist.`",
            "cargo test",
        );

        assert!(prompt.contains("recur at least twice"));
        assert!(prompt.contains("Ignore one-off failures"));
        assert!(prompt.contains("Recent rejected harness attempts"));
    }

    #[test]
    fn recent_rejections_ignores_accepted_entries() {
        let history = recent_rejections_from_log(
            r#"{"accepted":true,"validation_code":0,"rationale":"ok","harness":"Keep going"}
{"accepted":false,"validation_code":1,"rationale":"repeated failure","harness":"Verify artifacts exist.\nSecond line"}
{"accepted":false,"validation_code":2,"rationale":"","harness":"Retry once"}"#,
        );

        assert!(history.contains("repeated failure"));
        assert!(history.contains("tried `Retry once`"));
        assert!(!history.contains("Keep going"));
    }
}
