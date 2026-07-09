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
use provider::{check_api_key, get_api_target};
use reqwest::Client;
use self_harness::{run_self_harness, strip_self_harness_prefix};
use serde_json::Value;
use session::{Session, SessionState, pick_session, sessions_in_cwd};
use state::{color, get_config, get_mcp_client};
use std::env;
use tokio::io::{AsyncBufReadExt, BufReader};

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
use turn::run_state_turn;

#[cfg(feature = "acp")]
async fn run_acp_server() -> Result<(), String> {
    use std::sync::atomic::Ordering;

    state::ACP_MODE.store(true, Ordering::SeqCst);
    check_api_key();
    if policy::expose_mcp_tools() {
        get_mcp_client().load_servers(get_config()).await;
    }

    let client = Client::new();
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
        eprintln!("no sessions in this directory");
        std::process::exit(1);
    };
    last
}

#[tokio::main]
async fn main() {
    let mut args: Vec<String> = env::args().skip(1).collect();

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

    // Load MCP servers
    get_mcp_client().load_servers(get_config()).await;

    let client = Client::new();

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
                eprintln!("{}", color("90", &format!("resuming: {}", session.label)));
                label = Some(session.label.clone());
                state = SessionState::resume(format, session);
            }
        }
        Some("-c") => {
            let last = resume_last_session();
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
