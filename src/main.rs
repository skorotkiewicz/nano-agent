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
           /mito ...          local planner handoff\n\
           /self-harness <cmd> propose/keep harness after validator passes\n\
           line ending with \\ continues multiline input\n\n\
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

async fn route_prompt(
    client: &Client,
    prompt: &str,
    state: &mut SessionState,
    label: &mut Option<String>,
    mito_messages: &mut Vec<Value>,
) -> String {
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
    eprintln!(
        "{} repl {} mcp: {}",
        color("1", "nano"),
        color(
            "90",
            "(:q quit, :reset reset, /mito plan, /self-harness <validator>, end with \\ for multiline)"
        ),
        color("90", &get_mcp_client().status())
    );
    eprintln!(
        "{}",
        color(
            "90",
            &format!(
                "sandbox: {}  model: {}",
                nano_agent::sandbox::SandboxMode::from_env_value(
                    std::env::var("NANO_SANDBOX").ok().as_deref()
                )
                .label(),
                get_api_target().model
            )
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
        eprintln!("no sessions in this directory — starting a fresh one");
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
