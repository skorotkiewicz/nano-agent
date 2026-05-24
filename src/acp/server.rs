use crate::acp::{
    AcpError, AcpEvent, AgentManifest, AgentsListResponse, Run, RunCreateRequest,
    RunEventsListResponse, RunMode, RunResumeRequest, RunStatus,
};
use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{AUTHORIZATION, CONTENT_TYPE};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{RwLock, mpsc};
use tokio_stream::wrappers::ReceiverStream;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;

type BoxTaskFuture = Pin<Box<dyn Future<Output = Result<String, String>> + Send>>;
type TaskHandler = Arc<dyn Fn(String) -> BoxTaskFuture + Send + Sync>;
type ResponseBody = BoxBody<Bytes, Infallible>;

static RUN_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct ServerState {
    manifests: Arc<HashMap<String, AgentManifest>>,
    runs: Arc<RwLock<HashMap<String, StoredRun>>>,
    task_handler: TaskHandler,
    api_key: Option<String>,
}

#[derive(Debug, Clone)]
struct StoredRun {
    run: Run,
    events: Vec<AcpEvent>,
}

pub struct AcpServer {
    host: String,
    port: u16,
    manifests: Vec<AgentManifest>,
    api_key: Option<String>,
    task_handler: TaskHandler,
}

impl AcpServer {
    pub fn new<H, F>(
        host: impl Into<String>,
        port: u16,
        manifest: AgentManifest,
        api_key: Option<String>,
        task_handler: H,
    ) -> Self
    where
        H: Fn(String) -> F + Send + Sync + 'static,
        F: Future<Output = Result<String, String>> + Send + 'static,
    {
        Self {
            host: host.into(),
            port,
            manifests: vec![manifest],
            api_key,
            task_handler: Arc::new(move |task| Box::pin(task_handler(task))),
        }
    }

    pub async fn start(self) -> Result<(), String> {
        let addr = format!("{}:{}", self.host, self.port);
        let listener = TcpListener::bind((self.host.as_str(), self.port))
            .await
            .map_err(|e| format!("failed to bind ACP server to {}: {}", addr, e))?;

        let manifests = self
            .manifests
            .into_iter()
            .map(|manifest| (manifest.name.clone(), manifest))
            .collect::<HashMap<_, _>>();
        let state = ServerState {
            manifests: Arc::new(manifests),
            runs: Arc::new(RwLock::new(HashMap::new())),
            task_handler: self.task_handler,
            api_key: self.api_key,
        };

        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("ACP accept failed: {}", e))?;
            let io = TokioIo::new(stream);
            let state = state.clone();

            tokio::spawn(async move {
                let service = service_fn(move |req| {
                    let state = state.clone();
                    async move { Ok::<_, Infallible>(handle_request(req, state).await) }
                });

                if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                    eprintln!("ACP connection error: {}", e);
                }
            });
        }
    }
}

async fn handle_request(req: Request<Incoming>, state: ServerState) -> Response<ResponseBody> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.as_slice() == ["ping"] && method == Method::GET {
        return json_response(StatusCode::OK, &serde_json::json!({"status": "ok"}));
    }

    if let Some(response) = authorize(&req, &state) {
        return response;
    }

    match (method, segments.as_slice()) {
        (Method::GET, ["agents"]) => list_agents(&req, &state).await,
        (Method::GET, ["agents", name]) => get_agent(name, &state).await,
        (Method::POST, ["runs"]) => create_run(req, state).await,
        (Method::GET, ["runs", run_id]) => get_run(run_id, &state).await,
        (Method::POST, ["runs", run_id]) => resume_run(req, run_id, state).await,
        (Method::POST, ["runs", run_id, "cancel"]) => cancel_run(run_id, &state).await,
        (Method::GET, ["runs", run_id, "events"]) => list_run_events(run_id, &state).await,
        _ => error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP endpoint: {}", path)),
        ),
    }
}

fn authorize(req: &Request<Incoming>, state: &ServerState) -> Option<Response<ResponseBody>> {
    let Some(expected) = &state.api_key else {
        return None;
    };

    let authorized = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(|value| value == format!("Bearer {}", expected))
        .unwrap_or(false);

    if authorized {
        None
    } else {
        Some(error_response(
            StatusCode::UNAUTHORIZED,
            AcpError::invalid_input("missing or invalid ACP authorization"),
        ))
    }
}

async fn list_agents(req: &Request<Incoming>, state: &ServerState) -> Response<ResponseBody> {
    let limit = query_usize(req, "limit", 10, 1, 1000);
    let offset = query_usize(req, "offset", 0, 0, usize::MAX);
    let mut agents = state.manifests.values().cloned().collect::<Vec<_>>();
    agents.sort_by(|left, right| left.name.cmp(&right.name));
    let agents = agents.into_iter().skip(offset).take(limit).collect();

    json_response(StatusCode::OK, &AgentsListResponse { agents })
}

async fn get_agent(name: &str, state: &ServerState) -> Response<ResponseBody> {
    match state.manifests.get(name) {
        Some(manifest) => json_response(StatusCode::OK, manifest),
        None => error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP agent: {}", name)),
        ),
    }
}

async fn create_run(req: Request<Incoming>, state: ServerState) -> Response<ResponseBody> {
    let request = match read_json::<RunCreateRequest>(req).await {
        Ok(request) => request,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    if !state.manifests.contains_key(&request.agent_name) {
        return error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP agent: {}", request.agent_name)),
        );
    }

    let prompt = request.prompt_text();
    if prompt.trim().is_empty() {
        return error_response(
            StatusCode::BAD_REQUEST,
            AcpError::invalid_input("run input must contain text content"),
        );
    }

    let mut run = Run::new(request.agent_name.clone(), new_run_id());
    run.session_id = request
        .session_id
        .clone()
        .or_else(|| request.session.as_ref().map(|session| session.id.clone()));

    match request.mode {
        RunMode::Sync => {
            insert_run(&state, run.clone()).await;
            let run = execute_stored_run(state, run.run_id.clone(), prompt).await;
            json_response(StatusCode::OK, &run)
        }
        RunMode::Async => {
            insert_run(&state, run.clone()).await;
            let run_id = run.run_id.clone();
            let async_state = state.clone();
            tokio::spawn(async move {
                execute_stored_run(async_state, run_id, prompt).await;
            });
            json_response(StatusCode::ACCEPTED, &run)
        }
        RunMode::Stream => {
            let body = stream_run(state, run, prompt);
            Response::builder()
                .status(StatusCode::OK)
                .header(CONTENT_TYPE, "text/event-stream")
                .header("Cache-Control", "no-cache")
                .body(body)
                .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

async fn get_run(run_id: &str, state: &ServerState) -> Response<ResponseBody> {
    let runs = state.runs.read().await;
    match runs.get(run_id) {
        Some(stored) => json_response(StatusCode::OK, &stored.run),
        None => error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP run: {}", run_id)),
        ),
    }
}

async fn resume_run(
    req: Request<Incoming>,
    run_id: &str,
    state: ServerState,
) -> Response<ResponseBody> {
    let request = match read_json::<RunResumeRequest>(req).await {
        Ok(request) => request,
        Err(error) => return error_response(StatusCode::BAD_REQUEST, error),
    };

    if request.run_id != run_id {
        return error_response(
            StatusCode::BAD_REQUEST,
            AcpError::invalid_input("path run_id and body run_id must match"),
        );
    }

    let runs = state.runs.read().await;
    match runs.get(run_id) {
        Some(stored) if stored.run.status != RunStatus::Awaiting => error_response(
            StatusCode::BAD_REQUEST,
            AcpError::invalid_input("run is not awaiting input"),
        ),
        Some(stored) => json_response(StatusCode::OK, &stored.run),
        None => error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP run: {}", run_id)),
        ),
    }
}

async fn cancel_run(run_id: &str, state: &ServerState) -> Response<ResponseBody> {
    let mut runs = state.runs.write().await;
    let Some(stored) = runs.get_mut(run_id) else {
        return error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP run: {}", run_id)),
        );
    };

    if !stored.run.status.is_terminal() {
        stored.run.status = RunStatus::Cancelled;
        stored.run.finished_at = Some(crate::acp::now_rfc3339());
        let event = AcpEvent::run("run.cancelled", stored.run.clone());
        stored.events.push(event);
    }

    json_response(StatusCode::ACCEPTED, &stored.run)
}

async fn list_run_events(run_id: &str, state: &ServerState) -> Response<ResponseBody> {
    let runs = state.runs.read().await;
    match runs.get(run_id) {
        Some(stored) => json_response(
            StatusCode::OK,
            &RunEventsListResponse {
                events: stored.events.clone(),
            },
        ),
        None => error_response(
            StatusCode::NOT_FOUND,
            AcpError::not_found(format!("unknown ACP run: {}", run_id)),
        ),
    }
}

async fn execute_stored_run(state: ServerState, run_id: String, prompt: String) -> Run {
    let in_progress = transition_run(&state, &run_id, RunStatus::InProgress).await;
    if in_progress
        .as_ref()
        .map(|run| run.status.is_terminal())
        .unwrap_or(false)
    {
        return in_progress.unwrap();
    }

    let handler = state.task_handler.clone();
    let result = handler(prompt).await;
    finish_run(&state, &run_id, result)
        .await
        .unwrap_or_else(|| {
            let mut run = Run::new("unknown", run_id);
            run.fail("run disappeared before completion");
            run
        })
}

async fn insert_run(state: &ServerState, run: Run) {
    let event = AcpEvent::run("run.created", run.clone());
    let mut runs = state.runs.write().await;
    runs.insert(
        run.run_id.clone(),
        StoredRun {
            run,
            events: vec![event],
        },
    );
}

async fn transition_run(state: &ServerState, run_id: &str, status: RunStatus) -> Option<Run> {
    let mut runs = state.runs.write().await;
    let stored = runs.get_mut(run_id)?;
    if stored.run.status.is_terminal() {
        return Some(stored.run.clone());
    }
    stored.run.status = status;
    let event = AcpEvent::run(event_type_for_status(status), stored.run.clone());
    stored.events.push(event);
    Some(stored.run.clone())
}

async fn finish_run(
    state: &ServerState,
    run_id: &str,
    result: Result<String, String>,
) -> Option<Run> {
    let mut runs = state.runs.write().await;
    let stored = runs.get_mut(run_id)?;
    if stored.run.status.is_terminal() {
        return Some(stored.run.clone());
    }

    match result {
        Ok(output) => {
            stored.run.complete(output);
            if let Some(message) = stored.run.output.first().cloned() {
                stored
                    .events
                    .push(AcpEvent::message("message.completed", message));
            }
            stored
                .events
                .push(AcpEvent::run("run.completed", stored.run.clone()));
        }
        Err(error) => {
            stored.run.fail(error);
            stored
                .events
                .push(AcpEvent::run("run.failed", stored.run.clone()));
        }
    }

    Some(stored.run.clone())
}

fn stream_run(state: ServerState, run: Run, prompt: String) -> ResponseBody {
    let (tx, rx) = mpsc::channel::<Bytes>(8);
    tokio::spawn(async move {
        let run_id = run.run_id.clone();
        insert_run(&state, run.clone()).await;
        let created = AcpEvent::run("run.created", run);
        if !send_sse(&tx, &created).await {
            return;
        }

        if let Some(in_progress) = transition_run(&state, &run_id, RunStatus::InProgress).await {
            let event = AcpEvent::run("run.in-progress", in_progress);
            if !send_sse(&tx, &event).await {
                return;
            }
        }

        let handler = state.task_handler.clone();
        let result = handler(prompt).await;
        let Some(final_run) = finish_run(&state, &run_id, result).await else {
            let _ = send_sse(
                &tx,
                &AcpEvent::error(AcpError::server_error("run disappeared before completion")),
            )
            .await;
            return;
        };

        if let Some(message) = final_run.output.first().cloned() {
            let _ = send_sse(&tx, &AcpEvent::message("message.completed", message)).await;
        }

        let event_type = event_type_for_status(final_run.status);
        let _ = send_sse(&tx, &AcpEvent::run(event_type, final_run)).await;
    });

    let stream = ReceiverStream::new(rx).map(|bytes| Ok::<_, Infallible>(Frame::data(bytes)));
    BodyExt::boxed(StreamBody::new(stream))
}

async fn send_sse(tx: &mpsc::Sender<Bytes>, event: &AcpEvent) -> bool {
    let Ok(data) = serde_json::to_string(event) else {
        return false;
    };
    let body = format!("event: {}\ndata: {}\n\n", event.event_type, data);
    tx.send(Bytes::from(body)).await.is_ok()
}

async fn read_json<T>(req: Request<Incoming>) -> Result<T, AcpError>
where
    T: DeserializeOwned,
{
    let bytes = req
        .into_body()
        .collect()
        .await
        .map_err(|e| AcpError::invalid_input(format!("failed to read request body: {}", e)))?
        .to_bytes();

    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(AcpError::invalid_input("request body is too large"));
    }

    serde_json::from_slice(&bytes)
        .map_err(|e| AcpError::invalid_input(format!("invalid JSON body: {}", e)))
}

fn json_response<T>(status: StatusCode, value: &T) -> Response<ResponseBody>
where
    T: Serialize,
{
    match serde_json::to_vec(value) {
        Ok(body) => Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json")
            .body(full_body(body))
            .unwrap_or_else(|_| empty_response(StatusCode::INTERNAL_SERVER_ERROR)),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            AcpError::server_error(format!("failed to serialize response: {}", e)),
        ),
    }
}

fn error_response(status: StatusCode, error: AcpError) -> Response<ResponseBody> {
    json_response(status, &error)
}

fn empty_response(status: StatusCode) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(full_body(Vec::new()))
        .unwrap()
}

fn full_body(body: impl Into<Bytes>) -> ResponseBody {
    Full::new(body.into()).boxed()
}

fn query_usize(
    req: &Request<Incoming>,
    name: &str,
    default: usize,
    min: usize,
    max: usize,
) -> usize {
    req.uri()
        .query()
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                if key == name {
                    value.parse::<usize>().ok()
                } else {
                    None
                }
            })
        })
        .map(|value| value.clamp(min, max))
        .unwrap_or(default)
}

fn event_type_for_status(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Created => "run.created",
        RunStatus::InProgress => "run.in-progress",
        RunStatus::Awaiting => "run.awaiting",
        RunStatus::Cancelling => "run.cancelling",
        RunStatus::Cancelled => "run.cancelled",
        RunStatus::Completed => "run.completed",
        RunStatus::Failed => "run.failed",
    }
}

fn new_run_id() -> String {
    let count = RUN_COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id() as u128;
    let mut bytes = (nanos ^ (count << 48) ^ (pid << 16)).to_be_bytes();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::{RunCreateRequest, RunMode};

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn wait_for_server(client: &reqwest::Client, port: u16) {
        let url = format!("http://127.0.0.1:{}/ping", port);
        for _ in 0..50 {
            if client.get(&url).send().await.is_ok() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("ACP test server did not start");
    }

    #[tokio::test]
    #[ignore = "binds a loopback socket"]
    async fn test_acp_server_sync_and_stream_runs() {
        let port = free_port();
        let server = AcpServer::new(
            "127.0.0.1",
            port,
            AgentManifest::nano("nano", "test agent"),
            None,
            |task| async move { Ok(format!("echo: {}", task)) },
        );
        let handle = tokio::spawn(async move {
            let _ = server.start().await;
        });

        let client = reqwest::Client::new();
        wait_for_server(&client, port).await;

        let agents: AgentsListResponse = client
            .get(format!("http://127.0.0.1:{}/agents", port))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(agents.agents[0].name, "nano");

        let run: Run = client
            .post(format!("http://127.0.0.1:{}/runs", port))
            .json(&RunCreateRequest::new_text("nano", "hello"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.output_text(), "echo: hello");

        let mut stream_request = RunCreateRequest::new_text("nano", "stream");
        stream_request.mode = RunMode::Stream;
        let stream_body = client
            .post(format!("http://127.0.0.1:{}/runs", port))
            .json(&stream_request)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(stream_body.contains("event: run.created"));
        assert!(stream_body.contains("event: run.completed"));

        handle.abort();
    }
}
