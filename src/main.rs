mod chat;
mod input;
mod mito;
mod policy;
mod prompt;
mod provider;
mod self_harness;
mod session;
mod state;
mod tools;
mod turn;

use input::read_repl_input;
use mito::{run_mito_turn, strip_mito_prefix};
#[cfg(feature = "acp")]
use nano_agent::acp::{AcpPrompt, AcpServer};
use provider::{check_api_key, get_api_target, print_effective_config};
use reqwest::Client;
use self_harness::{run_self_harness, strip_self_harness_prefix};
use serde_json::Value;
use session::{Session, SessionState, pick_session, sessions_in_cwd};
use state::{color, get_config, get_mcp_client};
use std::env;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

use turn::run_state_turn;

fn print_usage() {
    eprintln!(
        "nano-agent — tiny shell agent for OpenAI-compatible APIs\n\n\
         Usage:\n\
           nano-agent [flags] [prompt...]\n\n\
         Flags:\n\
           -c                 continue last session in this directory\n\
           -s                 pick a recent session in this directory\n\
           --show-config      print effective provider/model/sandbox and exit\n\
           --help, -h         show this help\n\
           --acp              run as ACP stdio agent (needs --features acp)\n\n\
         REPL:\n\
           :q / quit / exit   quit\n\
           :reset             clear history and mito context\n\
           :config            print effective config\n\
           :help              this help\n\
           /mito ...          local planner handoff\n\
           /self-harness <cmd> propose/keep harness after validator passes\n\
           ! <cmd>            run shell; result shown to model next turn\n\
           !! <cmd>           run shell; result NOT sent to model\n\
           line ending with \\ continues multiline input\n\
           Esc / Ctrl+C        cancel in-flight think or long shell\n\n\
         Env:\n\
           OPENAI_API_KEY     required unless provider sets a key\n\
           OPENAI_BASE_URL    OpenAI-compatible base (implies chat-completions)\n\
           OPENAI_MODEL       model id (default gpt-5.5)\n\
           NANO_MAX_STEPS     tool-loop cap (default 200)\n\
           NANO_SANDBOX       off | fs (default) | fs+net\n"
    );
}

fn http_client() -> Client {
    // ponytail: finite timeouts beat hanging forever on a dead endpoint
    Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// `! cmd` (visible to model) or `!! cmd` (stdout only, hidden from model).
fn strip_shell_bang(prompt: &str) -> Option<(bool, &str)> {
    let trimmed = prompt.trim_start();
    if let Some(rest) = trimmed.strip_prefix("!!") {
        // accept `!!cmd` or `!! cmd`
        let cmd = rest.strip_prefix(' ').unwrap_or(rest).trim();
        return (!cmd.is_empty()).then_some((false, cmd));
    }
    if let Some(rest) = trimmed.strip_prefix('!') {
        // don't treat `!=` etc.; require space or start of shell-ish path
        let cmd = rest.strip_prefix(' ').unwrap_or(rest).trim();
        // `!/path` and `!cmd` ok; refuse pure `!`
        return (!cmd.is_empty()).then_some((true, cmd));
    }
    None
}

/// Color only the leading `$` for the terminal. Session/model notes stay plain.
fn display_shell_output(output: &str) -> String {
    let mut lines = output.lines();
    let Some(first) = lines.next() else {
        return String::new();
    };
    let mut out = if let Some(rest) = first.strip_prefix("$ ") {
        format!("{} {}", color("31", "$"), rest)
    } else if first == "$" {
        color("31", "$")
    } else {
        first.to_string()
    };
    for line in lines {
        out.push('\n');
        out.push_str(line);
    }
    if output.ends_with('\n') {
        out.push('\n');
    }
    out
}

async fn run_bang_shell(state: &mut SessionState, visible: bool, command: &str) -> String {
    let output = tools::run_user_shell(command).await;
    // Always print for the human. Color `$` here only — not in model notes.
    if visible {
        state.note_user_shell(command, &output);
        display_shell_output(&output).to_string()
        // format!(
        //     "{}\n{}",
        //     display_shell_output(&output),
        //     color("90", "→ noted for next model turn")
        // )
    } else {
        display_shell_output(&output).to_string()
        // format!(
        //     "{}\n{}",
        //     display_shell_output(&output),
        //     color("90", "→ hidden from model")
        // )
    }
}

async fn route_prompt(
    client: &Client,
    prompt: &str,
    state: &mut SessionState,
    label: &mut Option<String>,
    mito_messages: &mut Vec<Value>,
) -> String {
    if let Some((visible, command)) = strip_shell_bang(prompt) {
        return run_bang_shell(state, visible, command).await;
    }
    if let Some(validation_command) = strip_self_harness_prefix(prompt) {
        run_self_harness(client, validation_command).await
    } else if let Some(mito_prompt) = strip_mito_prefix(prompt) {
        run_mito_turn(client, mito_prompt, mito_messages, state, label).await
    } else {
        run_state_turn(client, prompt, state, label, prompt).await
    }
}

#[cfg(feature = "acp")]
async fn run_acp_server() -> Result<(), String> {
    use std::sync::atomic::Ordering;

    state::ACP_MODE.store(true, Ordering::SeqCst);
    check_api_key();
    if policy::expose_mcp_tools() {
        get_mcp_client().load_servers(get_config()).await;
    }

    let client = http_client();
    let server = AcpServer::new(
        "nano",
        "Nano local shell agent",
        move |acp_prompt: AcpPrompt| {
            let client = client.clone();
            async move {
                let prompt = format!(
                    "ACP session: {}\ncwd: {}\n\n{}",
                    acp_prompt.session_id,
                    acp_prompt.cwd.display(),
                    acp_prompt.prompt
                );
                let answer = state::ACP_SESSION_CWD
                    .scope(
                        acp_prompt.cwd.clone(),
                        turn::run_single_turn(&client, &prompt),
                    )
                    .await;
                if answer.starts_with("API Error:") {
                    Err(answer)
                } else {
                    Ok(answer)
                }
            }
        },
    );

    server.serve_stdio().await
}

async fn repl(client: &Client, mut state: SessionState, mut label: Option<String>) {
    let target = get_api_target();
    let sandbox = nano_agent::sandbox::SandboxMode::from_env_value(
        std::env::var("NANO_SANDBOX").ok().as_deref(),
    )
    .label();
    // One quiet banner line — dense, not chatty.
    eprintln!(
        "{}  {}  sandbox:{}  {}",
        color("1", "nano"),
        color("90", &target.model),
        color("90", sandbox),
        color("90", &get_mcp_client().status())
    );
    eprintln!(
        "{}",
        color(
            "90",
            ":q · :reset · !shell · !!quiet · \\ multiline · /mito · esc · :help"
        )
    );
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = stdin.lines();
    let mut mito_messages = Vec::new();

    loop {
        let prompt = match read_repl_input(&mut lines).await {
            Some(prompt) => prompt,
            None => return,
        };

        if prompt.is_empty() {
            continue;
        }
        let lower = prompt.to_lowercase();
        if lower == ":q" || lower == "quit" || lower == "exit" {
            return;
        }
        if lower == ":reset" || lower == "reset" {
            state = SessionState::new(get_api_target().format);
            label = None;
            mito_messages.clear();
            eprintln!("{}", color("90", "reset"));
            continue;
        }
        if lower == ":config" || lower == "config" {
            print_effective_config();
            continue;
        }
        if lower == ":help" || lower == "help" {
            print_usage();
            continue;
        }

        let answer =
            route_prompt(client, &prompt, &mut state, &mut label, &mut mito_messages).await;
        if !answer.is_empty() {
            println!("{}", answer);
        }
    }
}

fn resume_last_session() -> Session {
    let sessions = sessions_in_cwd();
    let Some(last) = sessions.into_iter().next_back() else {
        eprintln!("no sessions in this directory — start fresh without -c");
        std::process::exit(1);
    };
    last
}

fn ensure_session_format(current: provider::ApiFormat, session: &Session) {
    let saved = session.resolved_format();
    if saved == current {
        return;
    }

    let current_name = match current {
        provider::ApiFormat::Responses => "Responses API",
        provider::ApiFormat::ChatCompletions => "Chat Completions API",
    };
    let saved_name = match saved {
        provider::ApiFormat::Responses => "Responses API",
        provider::ApiFormat::ChatCompletions => "Chat Completions API",
    };

    eprintln!(
        "cannot resume session '{}' created with {}; current configuration uses {}\n\
         start fresh: nano-agent (without -c/-s), or match the original API format",
        session.label, saved_name, current_name
    );
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return;
    }
    if args.iter().any(|arg| arg == "--show-config") {
        print_effective_config();
        return;
    }

    if args.iter().any(|arg| arg == "--acp") {
        #[cfg(feature = "acp")]
        {
            if let Err(e) = run_acp_server().await {
                eprintln!("ACP error: {}", e);
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(feature = "acp"))]
        {
            eprintln!("ACP feature not enabled - rebuild with --features acp");
            std::process::exit(1);
        }
    }

    check_api_key();

    get_mcp_client().load_servers(get_config()).await;

    let client = http_client();

    let mut flag = None;
    if !args.is_empty() && (args[0] == "-c" || args[0] == "-s") {
        flag = Some(args.remove(0));
    }
    let prompt = args.join(" ");

    let format = get_api_target().format;
    let mut state = SessionState::new(format);
    let mut label = None;

    match flag.as_deref() {
        Some("-s") => {
            if let Some(session) = pick_session() {
                ensure_session_format(format, &session);
                eprintln!("{}", color("90", &format!("resuming: {}", session.label)));
                label = Some(session.label.clone());
                state = SessionState::resume(format, session);
            }
        }
        Some("-c") => {
            let last = resume_last_session();
            ensure_session_format(format, &last);
            eprintln!("{}", color("90", &format!("continuing: {}", last.label)));
            label = Some(last.label.clone());
            state = SessionState::resume(format, last);
        }
        _ => {}
    }

    if !prompt.is_empty() {
        let mut mito_messages = Vec::new();
        let answer =
            route_prompt(&client, &prompt, &mut state, &mut label, &mut mito_messages).await;
        if !answer.is_empty() {
            println!("{}", answer);
        }
    } else {
        repl(&client, state, label).await;
    }
}

#[cfg(test)]
mod bang_tests {
    use super::strip_shell_bang;

    #[test]
    fn bang_parses_visible_and_hidden() {
        assert_eq!(
            strip_shell_bang("! cat text.md"),
            Some((true, "cat text.md"))
        );
        assert_eq!(
            strip_shell_bang("!! cat text.md"),
            Some((false, "cat text.md"))
        );
        assert_eq!(strip_shell_bang("!!ls"), Some((false, "ls")));
        assert_eq!(strip_shell_bang("!ls -la"), Some((true, "ls -la")));
        assert_eq!(strip_shell_bang("!"), None);
        assert_eq!(strip_shell_bang("!!"), None);
        assert_eq!(strip_shell_bang("hello"), None);
        assert_eq!(strip_shell_bang("/mito x"), None);
    }

    #[test]
    fn display_shell_output_colors_dollar_only() {
        use super::display_shell_output;
        let rendered = display_shell_output("$ ls\nexit 0\nok\n");
        assert!(rendered.contains("ls"));
        assert!(rendered.contains("exit 0"));
        assert!(rendered.contains('$'));
        // no color on rest of first command line body
        assert!(rendered.contains(" ls") || rendered.contains("ls"));
    }
}
