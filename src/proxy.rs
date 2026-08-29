//! OpenAI/Anthropic-compatible local runtime proxy.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::config::{Config, ConfigOverrides, ProxyFailureMode};
use crate::context::{ContextCompileRequest, ContextMode};
use crate::daemon::DaemonClient;
use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::metrics::ProxyMetrics;
use crate::projects::GitState;

#[derive(Clone)]
struct ProxyState {
    config: Arc<Config>,
    daemon: DaemonClient,
    runtime: reqwest::Client,
    runtime_base: String,
    /// Process-lifetime outcome counters; the proxy runs in its own process,
    /// so it serves them itself from `GET /leb/metrics` on this listener.
    metrics: Arc<ProxyMetrics>,
}

#[derive(Debug)]
struct ProxyError {
    status: StatusCode,
    message: String,
    degradable: bool,
}

impl ProxyError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
            degradable: false,
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            message: message.into(),
            degradable: true,
        }
    }
}

impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "error": {
                    "type": "leb_proxy_error",
                    "message": self.message
                }
            })),
        )
            .into_response()
    }
}

/// Serve the compatibility proxy until `shutdown` resolves.
///
/// # Errors
///
/// Returns an error for invalid network configuration, listener failures, or
/// abnormal server termination.
pub async fn serve_until<F>(config: Config, shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    config.validate()?;
    let bind = config
        .proxy
        .bind
        .parse::<SocketAddr>()
        .map_err(|_| Error::Config("invalid proxy bind address".to_owned()))?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|source| Error::Daemon(format!("could not bind proxy at {bind}: {source}")))?;
    serve_on_listener(config, listener, shutdown).await
}

async fn serve_on_listener<F>(
    config: Config,
    listener: tokio::net::TcpListener,
    shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let runtime = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| Error::Daemon(format!("could not build runtime client: {error}")))?;
    let state = ProxyState {
        daemon: DaemonClient::new(
            &config.daemon.socket_path,
            Duration::from_millis(config.daemon.request_timeout_ms),
        ),
        runtime,
        runtime_base: config.proxy.runtime_url.trim_end_matches('/').to_owned(),
        config: Arc::new(config),
        metrics: Arc::new(ProxyMetrics::default()),
    };
    let max_body = usize::try_from(state.config.daemon.max_request_bytes)
        .map_err(|_| Error::Config("proxy request limit exceeds this platform".to_owned()))?;
    let router = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/leb/metrics", get(proxy_metrics))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/messages", post(anthropic_messages))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|source| Error::Daemon(format!("proxy server failed: {source}")))?;
    Ok(())
}

async fn openai_chat(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> std::result::Result<Response, ProxyError> {
    proxy_request(
        &state,
        &headers,
        &mut body,
        ApiFlavor::OpenAi,
        "/v1/chat/completions",
    )
    .await
}

async fn anthropic_messages(
    State(state): State<ProxyState>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> std::result::Result<Response, ProxyError> {
    proxy_request(
        &state,
        &headers,
        &mut body,
        ApiFlavor::Anthropic,
        "/v1/messages",
    )
    .await
}

#[derive(Clone, Copy)]
enum ApiFlavor {
    OpenAi,
    Anthropic,
}

async fn proxy_metrics(
    State(state): State<ProxyState>,
) -> Json<crate::metrics::ProxyMetricsSnapshot> {
    Json(state.metrics.snapshot())
}

async fn proxy_request(
    state: &ProxyState,
    headers: &HeaderMap,
    body: &mut Value,
    flavor: ApiFlavor,
    runtime_path: &str,
) -> std::result::Result<Response, ProxyError> {
    // `Some(injected)` when augmentation ran; `None` when it degraded open.
    let injected = match augment_request(state, headers, body, flavor).await {
        Ok(injected) => Some(injected),
        Err(error)
            if error.degradable
                && state.config.proxy.failure_mode == ProxyFailureMode::FailOpen =>
        {
            None
        }
        Err(error) => {
            state.metrics.count_refused();
            return Err(error);
        }
    };
    let mut response = match forward(state, headers, runtime_path, body.take()).await {
        Ok(response) => response,
        Err(error) => {
            state.metrics.count_refused();
            return Err(error);
        }
    };
    match injected {
        None => {
            state.metrics.count_degraded();
            response.headers_mut().insert(
                "x-leb-degraded",
                axum::http::HeaderValue::from_static("context_unavailable"),
            );
        }
        Some(true) => state.metrics.count_injected(),
        Some(false) => state.metrics.count_forwarded(),
    }
    Ok(response)
}

/// Compile and inject bounded context; `Ok(true)` means content was injected.
async fn augment_request(
    state: &ProxyState,
    headers: &HeaderMap,
    body: &mut Value,
    flavor: ApiFlavor,
) -> std::result::Result<bool, ProxyError> {
    let mode = match header_text(headers, "x-leb-mode")? {
        None | Some("brain" | "on") => ContextMode::Brain,
        Some("off") => ContextMode::Off,
        Some(other) => {
            return Err(ProxyError::bad_request(format!(
                "unsupported x-leb-mode `{other}`"
            )));
        }
    };
    if mode == ContextMode::Off && !has_project_scope(headers) {
        return Ok(false);
    }
    let current_paths = header_text(headers, "x-leb-current-paths")?
        .map(|paths| {
            paths
                .split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let (project_id, root, head) = resolve_scope(state, headers, mode, &current_paths).await?;
    let task_text = extract_task(body, flavor)?;
    let default_token_budget = root
        .as_deref()
        .map(|root| {
            Config::load_for(
                Some(root),
                &ConfigOverrides {
                    db_path: Some(state.config.storage.db_path.clone()),
                    local_only: Some(state.config.network.local_only),
                },
            )
        })
        .transpose()
        .map_err(|error| ProxyError::upstream(format!("project configuration failed: {error}")))?
        .map_or(state.config.context.max_injected_tokens, |config| {
            config
                .context
                .max_injected_tokens
                .min(state.config.context.max_injected_tokens)
        });
    let token_budget = header_text(headers, "x-leb-token-budget")?
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|_| ProxyError::bad_request("x-leb-token-budget must be an integer"))?
        .unwrap_or(default_token_budget);
    let request_id = header_text(headers, "x-request-id")?
        .map_or_else(|| format!("proxy_{}", uuid::Uuid::new_v4()), str::to_owned);
    let context = state
        .daemon
        .compile_context(&ContextCompileRequest {
            request_id,
            project_id,
            task_text,
            current_paths,
            token_budget,
            mode,
            repository_root: root,
            head,
            task_id: None,
        })
        .await
        .map_err(|error| ProxyError::upstream(format!("context compilation failed: {error}")))?;
    if mode == ContextMode::Off {
        return Ok(false);
    }
    let injected = format!("{}{}", context.stable_prefix, context.dynamic_context);
    if injected.is_empty() {
        return Ok(false);
    }
    match flavor {
        ApiFlavor::OpenAi => inject_openai(body, &injected)?,
        ApiFlavor::Anthropic => inject_anthropic(body, &injected)?,
    }
    Ok(true)
}

async fn resolve_scope(
    state: &ProxyState,
    headers: &HeaderMap,
    mode: ContextMode,
    current_paths: &[String],
) -> std::result::Result<(ProjectId, Option<PathBuf>, Option<String>), ProxyError> {
    if let Some(path) = header_text(headers, "x-leb-project-path")? {
        let path = PathBuf::from(path);
        let status =
            state.daemon.resolve_project(&path).await.map_err(|error| {
                ProxyError::upstream(format!("project resolution failed: {error}"))
            })?;
        let root = status.resolved.root;
        let project_id = status.resolved.project.id;
        let git = status.resolved.git;
        let mut refresh_paths = current_paths
            .iter()
            .map(PathBuf::from)
            .collect::<BTreeSet<_>>();
        if let Some(git) = &git {
            refresh_paths.extend(git.changed_paths.iter().cloned());
        }
        let freshness_mismatch =
            index_freshness_mismatch(status.structural_index.as_ref(), git.as_ref());
        let non_git_explicit_refresh = git.is_none() && !refresh_paths.is_empty();
        if mode == ContextMode::Brain && (freshness_mismatch || non_git_explicit_refresh) {
            state
                .daemon
                .ensure_project_fresh(&root, refresh_paths.into_iter().collect())
                .await
                .map_err(|error| {
                    ProxyError::upstream(format!("source freshness refresh failed: {error}"))
                })?;
        }
        let head = git.and_then(|git| git.head);
        return Ok((project_id, Some(root), head));
    }
    if let Some(id) = header_text(headers, "x-leb-project-id")? {
        if mode == ContextMode::Brain {
            return Err(ProxyError::bad_request(
                "brain mode requires x-leb-project-path for source freshness verification",
            ));
        }
        return ProjectId::parse(id)
            .map(|id| (id, None, None))
            .map_err(|error| ProxyError::bad_request(error.to_string()));
    }
    Err(ProxyError::bad_request(
        "brain mode requires x-leb-project-path or x-leb-project-id",
    ))
}

fn index_freshness_mismatch(
    index: Option<&crate::world::StructuralIndexStatus>,
    git: Option<&GitState>,
) -> bool {
    let Some(index) = index else {
        return true;
    };
    git.is_some_and(|git| index.head != git.head || index.diff_fingerprint != git.diff_fingerprint)
}

fn extract_task(body: &Value, flavor: ApiFlavor) -> std::result::Result<String, ProxyError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProxyError::bad_request("request.messages must be an array"))?;
    let message = messages
        .iter()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        .ok_or_else(|| ProxyError::bad_request("request has no user message"))?;
    let content = message
        .get("content")
        .ok_or_else(|| ProxyError::bad_request("user message has no content"))?;
    if let Some(text) = content.as_str() {
        return Ok(text.to_owned());
    }
    if let Some(parts) = content.as_array() {
        let texts = parts
            .iter()
            .filter(|part| {
                part.get("type").and_then(Value::as_str).is_none_or(|kind| {
                    matches!(
                        (flavor, kind),
                        (ApiFlavor::OpenAi | ApiFlavor::Anthropic, "text")
                    )
                })
            })
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>();
        if !texts.is_empty() {
            return Ok(texts.join("\n"));
        }
    }
    Err(ProxyError::bad_request(
        "user message content contains no text",
    ))
}

fn inject_openai(body: &mut Value, context: &str) -> std::result::Result<(), ProxyError> {
    let messages = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ProxyError::bad_request("request.messages must be an array"))?;
    messages.insert(0, json!({"role": "system", "content": context}));
    Ok(())
}

fn inject_anthropic(body: &mut Value, context: &str) -> std::result::Result<(), ProxyError> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| ProxyError::bad_request("request body must be an object"))?;
    match object.remove("system") {
        None => {
            object.insert("system".to_owned(), Value::String(context.to_owned()));
        }
        Some(Value::String(existing)) => {
            object.insert(
                "system".to_owned(),
                Value::String(format!("{context}\n{existing}")),
            );
        }
        Some(Value::Array(mut blocks)) => {
            blocks.insert(0, json!({"type": "text", "text": context}));
            object.insert("system".to_owned(), Value::Array(blocks));
        }
        Some(_) => {
            return Err(ProxyError::bad_request(
                "Anthropic system must be a string or content block array",
            ));
        }
    }
    Ok(())
}

async fn forward(
    state: &ProxyState,
    headers: &HeaderMap,
    path: &str,
    body: Value,
) -> std::result::Result<Response, ProxyError> {
    let mut request = state
        .runtime
        .post(format!("{}{path}", state.runtime_base))
        .json(&body);
    for (name, value) in headers {
        let name_text = name.as_str();
        if !is_hop_or_leb_header(name_text) {
            request = request.header(name, value);
        }
    }
    let upstream = request
        .send()
        .await
        .map_err(|error| ProxyError::upstream(format!("runtime request failed: {error}")))?;
    let status = upstream.status();
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if !is_hop_response_header(name.as_str()) {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|error| ProxyError::upstream(format!("invalid runtime response: {error}")))
}

fn is_hop_response_header(name: &str) -> bool {
    matches!(
        name,
        "content-length"
            | "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn has_project_scope(headers: &HeaderMap) -> bool {
    headers.contains_key("x-leb-project-path") || headers.contains_key("x-leb-project-id")
}

fn header_text<'a>(
    headers: &'a HeaderMap,
    name: &str,
) -> std::result::Result<Option<&'a str>, ProxyError> {
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ProxyError::bad_request(format!("{name} is not valid UTF-8")))
        })
        .transpose()
}

fn is_hop_or_leb_header(name: &str) -> bool {
    name.starts_with("x-leb-")
        || matches!(
            name,
            "host"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
        )
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::Mutex;

    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use super::*;
    use crate::daemon::{MemoryCreateRequest, MemoryPromoteRequest};
    use crate::memory::{EpistemicState, MemoryDraft};

    fn git(path: &std::path::Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .output()
            .expect("git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn openai_injection_preserves_messages_tools_and_stable_first_position() {
        let mut body = json!({
            "model": "local",
            "messages": [{"role": "user", "content": "fix it"}],
            "tools": [{"type": "function", "function": {"name": "read"}}]
        });
        let tools = body["tools"].clone();
        inject_openai(&mut body, "stable\ndynamic").expect("inject");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "stable\ndynamic");
        assert_eq!(body["tools"], tools);
    }

    #[test]
    fn anthropic_injection_preserves_existing_system_blocks() {
        let mut body = json!({
            "system": [{"type": "text", "text": "existing"}],
            "messages": [{"role": "user", "content": "fix it"}]
        });
        inject_anthropic(&mut body, "leb").expect("inject");
        assert_eq!(body["system"][0]["text"], "leb");
        assert_eq!(body["system"][1]["text"], "existing");
    }

    #[test]
    fn task_extraction_handles_text_block_content() {
        let body = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "first"}, {"type": "image", "data": "x"}, {"type": "text", "text": "second"}]
            }]
        });
        assert_eq!(
            extract_task(&body, ApiFlavor::Anthropic).expect("task"),
            "first\nsecond"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn proxy_preserves_tools_stream_content_and_off_control() {
        let temp = TempDir::new().expect("temp");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("lebd.sock");
        config.daemon.request_timeout_ms = 2_000;

        let captured = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
        let runtime_capture = Arc::clone(&captured);
        let runtime_router = Router::new().route(
            "/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let capture = Arc::clone(&runtime_capture);
                async move {
                    capture.lock().expect("capture lock").push((headers, body));
                    (
                        StatusCode::OK,
                        [
                            ("content-type", "text/event-stream"),
                            ("cache-control", "no-cache"),
                            ("x-runtime-request-id", "runtime-123"),
                        ],
                        "data: {\"delta\":\"ok\"}\n\ndata: [DONE]\n\n",
                    )
                }
            }),
        );
        let runtime_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("runtime bind");
        let runtime_address = runtime_listener.local_addr().expect("runtime address");
        let (runtime_stop_tx, runtime_stop_rx) = oneshot::channel::<()>();
        let runtime_server = tokio::spawn(async move {
            axum::serve(runtime_listener, runtime_router)
                .with_graceful_shutdown(async move {
                    let _ = runtime_stop_rx.await;
                })
                .await
                .expect("runtime server");
        });
        config.proxy.runtime_url = format!("http://{runtime_address}");

        let (daemon_stop_tx, daemon_stop_rx) = oneshot::channel::<()>();
        let daemon_config = config.clone();
        let daemon_server = tokio::spawn(crate::daemon::serve_until(daemon_config, async move {
            let _ = daemon_stop_rx.await;
        }));
        let daemon_client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(2));
        for _ in 0..100 {
            if daemon_client.health().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = daemon_client
            .resolve_project(&project)
            .await
            .expect("resolve project");
        let memory = daemon_client
            .create_memory(&MemoryCreateRequest {
                project_id: status.resolved.project.id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: status.resolved.project.id.to_string(),
                    kind: "decision".to_owned(),
                    claim: "Always preserve the cleanup tool schema".to_owned(),
                    rationale: "Explicit test decision".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["cleanup".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            })
            .await
            .expect("memory");
        daemon_client
            .promote_memory(
                &memory.id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await
            .expect("promote");

        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy bind");
        let proxy_address = proxy_listener.local_addr().expect("proxy address");
        let (proxy_stop_tx, proxy_stop_rx) = oneshot::channel::<()>();
        let proxy_server = tokio::spawn(serve_on_listener(config, proxy_listener, async move {
            let _ = proxy_stop_rx.await;
        }));
        let request = json!({
            "model": "local",
            "stream": true,
            "messages": [{"role": "user", "content": "fix cleanup"}],
            "tools": [{"type": "function", "function": {"name": "read_file"}}]
        });
        let response = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-leb-project-path", project.to_string_lossy().as_ref())
            .header("x-leb-mode", "brain")
            .header("authorization", "Bearer local-test")
            .json(&request)
            .send()
            .await
            .expect("brain request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("cache-control").expect("cache"),
            "no-cache"
        );
        assert_eq!(
            response
                .headers()
                .get("x-runtime-request-id")
                .expect("request id"),
            "runtime-123"
        );
        assert_eq!(
            response.text().await.expect("stream body"),
            "data: {\"delta\":\"ok\"}\n\ndata: [DONE]\n\n"
        );

        reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-leb-mode", "off")
            .json(&request)
            .send()
            .await
            .expect("off request");
        {
            let captured = captured.lock().expect("capture lock");
            assert_eq!(captured.len(), 2);
            assert_eq!(captured[0].1["tools"], request["tools"]);
            assert_eq!(captured[0].1["messages"][0]["role"], "system");
            assert_eq!(captured[1].1, request);
            assert!(!captured[0].0.contains_key("x-leb-project-path"));
        }

        daemon_stop_tx.send(()).expect("daemon stop");
        daemon_server.await.expect("daemon task").expect("daemon");
        let degraded = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-leb-project-path", project.to_string_lossy().as_ref())
            .header("x-leb-mode", "brain")
            .json(&request)
            .send()
            .await
            .expect("fail-open request");
        assert_eq!(degraded.status(), StatusCode::OK);
        assert_eq!(
            degraded.headers().get("x-leb-degraded").expect("degraded"),
            "context_unavailable"
        );
        let _ = degraded.text().await.expect("degraded response body");
        assert_eq!(
            captured.lock().expect("capture lock")[2].1,
            request,
            "fail-open must forward the original semantic request"
        );

        let outcomes: crate::metrics::ProxyMetricsSnapshot = reqwest::Client::new()
            .get(format!("http://{proxy_address}/leb/metrics"))
            .send()
            .await
            .expect("proxy metrics request")
            .json()
            .await
            .expect("proxy metrics body");
        assert_eq!(outcomes.injected, 1, "{outcomes:?}");
        assert_eq!(outcomes.forwarded, 1, "{outcomes:?}");
        assert_eq!(outcomes.degraded, 1, "{outcomes:?}");
        assert_eq!(outcomes.refused, 0, "{outcomes:?}");

        proxy_stop_tx.send(()).expect("proxy stop");
        runtime_stop_tx.send(()).expect("runtime stop");
        proxy_server.await.expect("proxy task").expect("proxy");
        runtime_server.await.expect("runtime task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn brain_request_refreshes_dirty_symbols_before_memory_injection() {
        let temp = TempDir::new().expect("temp");
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("project");
        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn guard() -> bool { true }\n",
        )
        .expect("source");
        git(&project, &["init", "-q"]);
        git(&project, &["config", "user.email", "leb@example.invalid"]);
        git(&project, &["config", "user.name", "LEB Tests"]);
        git(&project, &["add", "src/lib.rs"]);
        git(&project, &["commit", "-qm", "initial"]);

        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("lebd.sock");
        config.daemon.request_timeout_ms = 5_000;
        let (daemon_stop_tx, daemon_stop_rx) = oneshot::channel::<()>();
        let daemon_config = config.clone();
        let daemon_server = tokio::spawn(crate::daemon::serve_until(daemon_config, async move {
            let _ = daemon_stop_rx.await;
        }));
        let daemon = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(5));
        for _ in 0..100 {
            if daemon.health().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let indexed = daemon
            .index_project_with_provider(&project, None, Some("tree_sitter".to_owned()), None)
            .await
            .expect("initial index");
        let nodes = daemon
            .search_world(&crate::daemon::WorldSearchRequest {
                project_id: indexed.resolved.project.id.clone(),
                query: "guard".to_owned(),
                limit: 10,
            })
            .await
            .expect("node search");
        let guard = nodes
            .iter()
            .find(|node| node.label == "guard")
            .expect("guard node");
        let memory = daemon
            .create_memory(&MemoryCreateRequest {
                project_id: indexed.resolved.project.id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: indexed.resolved.project.id.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "The guard function is always named guard".to_owned(),
                    rationale: "Explicit fixture".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["guard".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: vec![guard.id.clone()],
                },
            })
            .await
            .expect("memory");
        daemon
            .promote_memory(
                &memory.id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await
            .expect("promote");

        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let runtime_capture = Arc::clone(&captured);
        let runtime_router = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let capture = Arc::clone(&runtime_capture);
                async move {
                    capture.lock().expect("capture").push(body);
                    (StatusCode::OK, Json(json!({"choices": []})))
                }
            }),
        );
        let runtime_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("runtime");
        let runtime_address = runtime_listener.local_addr().expect("runtime address");
        let (runtime_stop_tx, runtime_stop_rx) = oneshot::channel::<()>();
        let runtime_server = tokio::spawn(async move {
            axum::serve(runtime_listener, runtime_router)
                .with_graceful_shutdown(async move {
                    let _ = runtime_stop_rx.await;
                })
                .await
                .expect("runtime server");
        });
        config.proxy.runtime_url = format!("http://{runtime_address}");
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy");
        let proxy_address = proxy_listener.local_addr().expect("proxy address");
        let (proxy_stop_tx, proxy_stop_rx) = oneshot::channel::<()>();
        let proxy_server = tokio::spawn(serve_on_listener(config, proxy_listener, async move {
            let _ = proxy_stop_rx.await;
        }));

        std::fs::write(
            project.join("src/lib.rs"),
            "pub fn renamed_guard() -> bool { false }\n",
        )
        .expect("edit source");
        let response = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-leb-project-path", project.to_string_lossy().as_ref())
            .json(&json!({
                "model": "local",
                "messages": [{"role": "user", "content": "change the guard"}]
            }))
            .send()
            .await
            .expect("proxy request");
        assert!(response.status().is_success());
        let stale_text_was_forwarded = {
            let forwarded = captured.lock().expect("capture");
            assert_eq!(forwarded.len(), 1);
            forwarded[0].to_string().contains("always named guard")
        };
        assert!(!stale_text_was_forwarded);
        let memory = daemon
            .get_memory(&memory.id)
            .await
            .expect("memory lookup")
            .expect("memory remains auditable");
        assert_eq!(memory.status, crate::memory::MemoryStatus::Suspect);

        let _ = proxy_stop_tx.send(());
        let _ = runtime_stop_tx.send(());
        let _ = daemon_stop_tx.send(());
        proxy_server
            .await
            .expect("proxy join")
            .expect("proxy result");
        runtime_server.await.expect("runtime join");
        daemon_server
            .await
            .expect("daemon join")
            .expect("daemon result");
    }
}
