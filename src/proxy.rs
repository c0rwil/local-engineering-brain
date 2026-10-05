//! OpenAI/Anthropic-compatible local runtime proxy.

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use moka::sync::Cache;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::config::{Config, ConfigOverrides, ProxyFailureMode};
use crate::context::{ContextCompileRequest, ContextMode, InjectionFraming};
use crate::daemon::DaemonClient;
use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::metrics::ProxyMetrics;
use crate::projects::GitState;

const SESSION_INJECTION_CACHE_BYTES: u64 = 8 * 1_024 * 1_024;

#[derive(Clone)]
struct ProxyState {
    config: Arc<Config>,
    daemon: DaemonClient,
    runtime: reqwest::Client,
    runtime_base: String,
    /// Byte-bounded process-local snapshot of the first compiled Engram block in
    /// each Claude Code session. Canonical memory remains daemon-owned.
    session_injections: Cache<SessionInjectionKey, CachedInjection>,
    /// Process-lifetime outcome counters; the proxy runs in its own process,
    /// so it serves them itself from `GET /engram/metrics` on this listener.
    metrics: Arc<ProxyMetrics>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionInjectionKey {
    session_id: String,
    project_id: String,
    token_budget: u32,
    framing: Option<InjectionFraming>,
}

#[derive(Clone, Debug)]
struct CachedInjection {
    content: Arc<str>,
    sha256: String,
    bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionCacheStatus {
    Disabled,
    Hit,
    Miss,
}

impl SessionCacheStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Hit => "hit",
            Self::Miss => "miss",
        }
    }

    const fn hit(self) -> Option<bool> {
        match self {
            Self::Disabled => None,
            Self::Hit => Some(true),
            Self::Miss => Some(false),
        }
    }
}

#[derive(Debug)]
struct InjectionTelemetry {
    sha256: String,
    bytes: u64,
    session_cache: SessionCacheStatus,
}

#[derive(Debug)]
enum Augmentation {
    Forwarded,
    Injected(InjectionTelemetry),
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
                    "type": "engram_proxy_error",
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
    let session_injections = Cache::builder()
        .max_capacity(SESSION_INJECTION_CACHE_BYTES)
        .weigher(|_key: &SessionInjectionKey, value: &CachedInjection| {
            u32::try_from(value.content.len()).unwrap_or(u32::MAX)
        })
        .build();
    let state = ProxyState {
        daemon: DaemonClient::new(
            &config.daemon.socket_path,
            Duration::from_millis(config.daemon.request_timeout_ms),
        ),
        runtime,
        runtime_base: config.proxy.runtime_url.trim_end_matches('/').to_owned(),
        session_injections,
        config: Arc::new(config),
        metrics: Arc::new(ProxyMetrics::default()),
    };
    let max_body = usize::try_from(state.config.daemon.max_request_bytes)
        .map_err(|_| Error::Config("proxy request limit exceeds this platform".to_owned()))?;
    let router = Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/engram/metrics", get(proxy_metrics))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/messages", post(anthropic_messages))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state);
    // The proxy binds loopback only (config validation). Answer only requests
    // addressed to this listener, so a web page cannot reach it by DNS
    // rebinding. SDK clients send no Origin and are unaffected.
    let port = listener
        .local_addr()
        .map_err(|source| Error::Daemon(format!("proxy listener address: {source}")))?
        .port();
    let router = crate::security::guard_loopback(router, port);
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
    // `Some(augmentation)` when augmentation was available; `None` when it
    // degraded open.
    let augmentation = match augment_request(state, headers, body, flavor).await {
        Ok(augmentation) => Some(augmentation),
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
    match augmentation {
        None => {
            state.metrics.count_degraded();
            response.headers_mut().insert(
                "x-engram-degraded",
                axum::http::HeaderValue::from_static("context_unavailable"),
            );
        }
        Some(Augmentation::Injected(telemetry)) => {
            state
                .metrics
                .count_injected(telemetry.bytes, telemetry.session_cache.hit());
            insert_injection_telemetry(&mut response, &telemetry)?;
        }
        Some(Augmentation::Forwarded) => state.metrics.count_forwarded(),
    }
    Ok(response)
}

fn insert_injection_telemetry(
    response: &mut Response,
    telemetry: &InjectionTelemetry,
) -> std::result::Result<(), ProxyError> {
    let bytes = telemetry.bytes.to_string();
    for (name, value) in [
        ("x-engram-injection-sha256", telemetry.sha256.as_str()),
        ("x-engram-injection-bytes", bytes.as_str()),
        ("x-engram-session-cache", telemetry.session_cache.as_str()),
    ] {
        let value = axum::http::HeaderValue::from_str(value).map_err(|error| {
            ProxyError::upstream(format!("invalid injection telemetry header: {error}"))
        })?;
        response.headers_mut().insert(name, value);
    }
    Ok(())
}

/// Resolve the effective injection budget: an explicit header wins, otherwise
/// the project-scoped configured budget capped by the proxy's own ceiling.
fn resolve_token_budget(
    state: &ProxyState,
    headers: &HeaderMap,
    root: Option<&Path>,
) -> std::result::Result<u32, ProxyError> {
    let default_token_budget = root
        .map(|root| {
            Config::load_for_home(
                state.config.config_home.as_deref(),
                Some(root),
                &ConfigOverrides {
                    workspace: Some(state.config.workspace.clone()),
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
    header_text(headers, "x-engram-token-budget")?
        .map(str::parse::<u32>)
        .transpose()
        .map_err(|_| ProxyError::bad_request("x-engram-token-budget must be an integer"))
        .map(|budget| budget.unwrap_or(default_token_budget))
}

/// Compile and inject bounded context.
async fn augment_request(
    state: &ProxyState,
    headers: &HeaderMap,
    body: &mut Value,
    flavor: ApiFlavor,
) -> std::result::Result<Augmentation, ProxyError> {
    let mode = match header_text(headers, "x-engram-mode")? {
        None | Some("brain" | "on") => ContextMode::Brain,
        Some("off") => ContextMode::Off,
        Some(other) => {
            return Err(ProxyError::bad_request(format!(
                "unsupported x-engram-mode `{other}`"
            )));
        }
    };
    if mode == ContextMode::Off && !has_project_scope(headers) {
        return Ok(Augmentation::Forwarded);
    }
    let current_paths = header_text(headers, "x-engram-current-paths")?
        .map(|paths| {
            paths
                .split(',')
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let (project_id, root, head, current_paths) =
        resolve_scope(state, headers, mode, &current_paths).await?;
    let Some(task_text) = extract_task(body, flavor)? else {
        // No user turn carries text (a degenerate but protocol-valid
        // conversation): there is no task to compile context for, so forward
        // the request unchanged in either mode.
        return Ok(Augmentation::Forwarded);
    };
    let token_budget = resolve_token_budget(state, headers, root.as_deref())?;
    let framing = match header_text(headers, "x-engram-injection-framing")? {
        None => None,
        Some(value) => Some(InjectionFraming::parse(value).ok_or_else(|| {
            ProxyError::bad_request("x-engram-injection-framing must be `reference` or `directive`")
        })?),
    };
    let session_key = session_injection_key(headers, mode, &project_id, token_budget, framing)?;
    if let Some(key) = &session_key
        && let Some(cached) = state.session_injections.get(key)
    {
        inject_for_flavor(body, flavor, &cached.content)?;
        return Ok(Augmentation::Injected(InjectionTelemetry {
            sha256: cached.sha256,
            bytes: cached.bytes,
            session_cache: SessionCacheStatus::Hit,
        }));
    }
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
            framing,
        })
        .await
        .map_err(|error| ProxyError::upstream(format!("context compilation failed: {error}")))?;
    if mode == ContextMode::Off {
        return Ok(Augmentation::Forwarded);
    }
    let injected = format!("{}{}", context.stable_prefix, context.dynamic_context);
    if injected.is_empty() {
        return Ok(Augmentation::Forwarded);
    }
    inject_for_flavor(body, flavor, &injected)?;
    let bytes = u64::try_from(injected.len()).unwrap_or(u64::MAX);
    let sha256 = sha256(injected.as_bytes());
    let session_cache = if let Some(key) = session_key {
        state.session_injections.insert(
            key,
            CachedInjection {
                content: Arc::from(injected),
                sha256: sha256.clone(),
                bytes,
            },
        );
        SessionCacheStatus::Miss
    } else {
        SessionCacheStatus::Disabled
    };
    Ok(Augmentation::Injected(InjectionTelemetry {
        sha256,
        bytes,
        session_cache,
    }))
}

fn inject_for_flavor(
    body: &mut Value,
    flavor: ApiFlavor,
    context: &str,
) -> std::result::Result<(), ProxyError> {
    match flavor {
        ApiFlavor::OpenAi => inject_openai(body, context),
        ApiFlavor::Anthropic => inject_anthropic(body, context),
    }
}

async fn resolve_scope(
    state: &ProxyState,
    headers: &HeaderMap,
    mode: ContextMode,
    current_paths: &[String],
) -> std::result::Result<(ProjectId, Option<PathBuf>, Option<String>, Vec<String>), ProxyError> {
    if let Some(path) = header_text(headers, "x-engram-project-path")? {
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
        // No client sends x-engram-current-paths in practice; the repo's own
        // dirty files ARE the files being worked on, and they let the
        // structural-overlap ranking actually engage. Bounded, and only a
        // fallback: an explicit header always wins.
        let effective_paths = if current_paths.is_empty() {
            git.as_ref().map_or_else(Vec::new, |git| {
                git.changed_paths
                    .iter()
                    .take(32)
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect()
            })
        } else {
            current_paths.to_vec()
        };
        let head = git.and_then(|git| git.head);
        return Ok((project_id, Some(root), head, effective_paths));
    }
    if let Some(id) = header_text(headers, "x-engram-project-id")? {
        if mode == ContextMode::Brain {
            return Err(ProxyError::bad_request(
                "brain mode requires x-engram-project-path for source freshness verification",
            ));
        }
        return ProjectId::parse(id)
            .map(|id| (id, None, None, current_paths.to_vec()))
            .map_err(|error| ProxyError::bad_request(error.to_string()));
    }
    Err(ProxyError::bad_request(
        "brain mode requires x-engram-project-path or x-engram-project-id",
    ))
}

fn index_freshness_mismatch(
    index: Option<&crate::world::StructuralIndexStatus>,
    git: Option<&GitState>,
) -> bool {
    let Some(index) = index else {
        return true;
    };
    index.needs_reindex
        || git.is_some_and(|git| {
            index.head != git.head || index.diff_fingerprint != git.diff_fingerprint
        })
}

/// Extract the task text from the MOST RECENT user message that carries any.
/// Agentic loops routinely end on a tool-result-only user message; the human
/// task statement lives in an earlier turn, so extraction walks backward
/// instead of failing the request. Returns `None` when no user message in the
/// conversation carries text at all — callers forward such requests unchanged
/// rather than rejecting a protocol-valid conversation.
fn extract_task(
    body: &Value,
    flavor: ApiFlavor,
) -> std::result::Result<Option<String>, ProxyError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProxyError::bad_request("request.messages must be an array"))?;
    for message in messages
        .iter()
        .rev()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    {
        let Some(content) = message.get("content") else {
            continue;
        };
        if let Some(text) = content.as_str() {
            if let Some(task) = task_intent(text) {
                return Ok(Some(task));
            }
            continue;
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
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>();
            if let Some(task) = task_intent(&texts.join("\n")) {
                return Ok(Some(task));
            }
        }
    }
    Ok(None)
}

/// A user message's text as the task: none for a shell-mode turn, notice or
/// subagent report, and without command output otherwise.
fn task_intent(text: &str) -> Option<String> {
    if text.trim().is_empty() || crate::agenthooks::is_host_record(text) {
        return None;
    }
    Some(crate::agenthooks::prompt_intent(text)).filter(|task| !task.trim().is_empty())
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
    let context_block = || json!({"type": "text", "text": context});
    match object.remove("system") {
        None => {
            object.insert("system".to_owned(), Value::Array(vec![context_block()]));
        }
        Some(Value::String(existing)) => {
            object.insert(
                "system".to_owned(),
                Value::Array(vec![
                    json!({"type": "text", "text": existing}),
                    context_block(),
                ]),
            );
        }
        Some(Value::Array(mut blocks)) => {
            // Claude gateways assign protocol and cache semantics to the
            // caller's leading blocks. Preserve their content and ordering,
            // then append Engram as an independent block.
            blocks.push(context_block());
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
    headers.contains_key("x-engram-project-path") || headers.contains_key("x-engram-project-id")
}

fn session_injection_key(
    headers: &HeaderMap,
    mode: ContextMode,
    project_id: &ProjectId,
    token_budget: u32,
    framing: Option<InjectionFraming>,
) -> std::result::Result<Option<SessionInjectionKey>, ProxyError> {
    if mode == ContextMode::Off {
        return Ok(None);
    }
    let Some(session_id) = header_text(headers, "x-claude-code-session-id")? else {
        return Ok(None);
    };
    if session_id.is_empty()
        || session_id.len() > 256
        || !session_id.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(ProxyError::bad_request(
            "x-claude-code-session-id must be 1..=256 visible ASCII bytes",
        ));
    }
    Ok(Some(SessionInjectionKey {
        session_id: session_id.to_owned(),
        project_id: project_id.to_string(),
        token_budget,
        framing,
    }))
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
    name.starts_with("x-engram-")
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

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
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

    #[test]
    fn extractor_upgrade_requires_refresh_even_without_git_changes() {
        let mut status = crate::world::StructuralIndexStatus {
            scan_id: "scan".to_owned(),
            provider: "tree_sitter".to_owned(),
            head: None,
            diff_fingerprint: "no-git".to_owned(),
            completed_at_ms: 0,
            changed_paths: Some(Vec::new()),
            needs_reindex: true,
        };
        assert!(index_freshness_mismatch(Some(&status), None));
        status.needs_reindex = false;
        assert!(!index_freshness_mismatch(Some(&status), None));
    }

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
    fn anthropic_injection_preserves_system_prefix_order_and_cache_control() {
        let mut body = json!({
            "system": [
                {
                    "type": "text",
                    "text": "Claude Code attribution",
                    "cache_control": {"type": "ephemeral"}
                },
                {"type": "text", "text": "existing instructions"}
            ],
            "messages": [{"role": "user", "content": "fix it"}]
        });
        let original = body["system"].clone();
        inject_anthropic(&mut body, "engram").expect("inject");
        let system = body["system"].as_array().expect("system blocks");
        assert_eq!(
            &system[..original.as_array().expect("original").len()],
            original.as_array().expect("original")
        );
        assert_eq!(system[0]["text"], "Claude Code attribution");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(system[1]["text"], "existing instructions");
        assert_eq!(system[2], json!({"type": "text", "text": "engram"}));
    }

    #[test]
    fn anthropic_injection_appends_after_string_system() {
        let mut body = json!({
            "system": "existing instructions",
            "messages": [{"role": "user", "content": "fix it"}]
        });
        inject_anthropic(&mut body, "engram").expect("inject");
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "existing instructions"},
                {"type": "text", "text": "engram"}
            ])
        );
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
            Some("first\nsecond".to_owned())
        );
    }

    #[test]
    fn task_extraction_walks_past_host_records() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "fix the scheduler"},
                {"role": "assistant", "content": "On it."},
                {"role": "user", "content": "<bash-input>cat .env</bash-input><bash-stdout>TOKEN=abc</bash-stdout>"},
                {"role": "user", "content": [{"type": "text", "text": "<task-notification>\n<status>completed</status>\n</task-notification>"}]}
            ]
        });
        assert_eq!(
            extract_task(&body, ApiFlavor::Anthropic).expect("task"),
            Some("fix the scheduler".to_owned())
        );
    }

    #[test]
    fn task_extraction_walks_past_tool_result_only_turns() {
        // Every agentic follow-up turn ends on a tool-result-only user
        // message; the task statement lives in the first user turn.
        let body = json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "fix the scheduler"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"cmd": "ls"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": ""}
                ]}
            ]
        });
        assert_eq!(
            extract_task(&body, ApiFlavor::Anthropic).expect("task"),
            Some("fix the scheduler".to_owned())
        );

        let no_text = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "tu_1", "content": ""}]
            }]
        });
        assert_eq!(
            extract_task(&no_text, ApiFlavor::Anthropic).expect("none"),
            None
        );
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn anthropic_off_mode_preserves_auth_tools_and_streaming_content() {
        let temp = TempDir::new().expect("temp");
        let captured = Arc::new(Mutex::new(None::<(HeaderMap, Value)>));
        let runtime_capture = Arc::clone(&captured);
        let stream = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"name\":\"read_file\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}\n\n",
        );
        let runtime_router = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let capture = Arc::clone(&runtime_capture);
                async move {
                    *capture.lock().expect("capture lock") = Some((headers, body));
                    (
                        StatusCode::OK,
                        [("content-type", "text/event-stream")],
                        stream,
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

        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("engramd.sock");
        config.proxy.runtime_url = format!("http://{runtime_address}");
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy bind");
        let proxy_address = proxy_listener.local_addr().expect("proxy address");
        let (proxy_stop_tx, proxy_stop_rx) = oneshot::channel::<()>();
        let proxy_server = tokio::spawn(serve_on_listener(config, proxy_listener, async move {
            let _ = proxy_stop_rx.await;
        }));

        let request = json!({
            "model": "frontier-test",
            "max_tokens": 128,
            "stream": true,
            "system": [{"type": "text", "text": "keep this system block exact"}],
            "messages": [
                {"role": "user", "content": "inspect the file"},
                {"role": "assistant", "content": [{
                    "type": "tool_use",
                    "id": "tool_1",
                    "name": "read_file",
                    "input": {"path": "src/lib.rs"}
                }]},
                {"role": "user", "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool_1",
                    "content": "pub fn answer() -> u8 { 42 }"
                }]}
            ],
            "tools": [{
                "name": "read_file",
                "description": "Read one file",
                "input_schema": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }]
        });
        let response = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/messages"))
            .header("authorization", "Bearer frontier-test")
            .header("x-api-key", "key-test")
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "tools-test")
            .header("x-engram-mode", "off")
            .header("x-engram-private-control", "must-not-leak")
            .header("connection", "close")
            .json(&request)
            .send()
            .await
            .expect("proxy request");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .expect("content type"),
            "text/event-stream"
        );
        assert_eq!(response.text().await.expect("stream body"), stream);

        {
            let captured = captured.lock().expect("capture lock");
            let (headers, body) = captured.as_ref().expect("runtime capture");
            assert_eq!(body, &request, "off mode must preserve request semantics");
            assert_eq!(headers["authorization"], "Bearer frontier-test");
            assert_eq!(headers["x-api-key"], "key-test");
            assert_eq!(headers["anthropic-version"], "2023-06-01");
            assert_eq!(headers["anthropic-beta"], "tools-test");
            assert!(!headers.contains_key("x-engram-mode"));
            assert!(!headers.contains_key("x-engram-private-control"));
            assert!(!headers.contains_key("connection"));
        }

        proxy_stop_tx.send(()).expect("proxy stop");
        runtime_stop_tx.send(()).expect("runtime stop");
        proxy_server.await.expect("proxy task").expect("proxy");
        runtime_server.await.expect("runtime task");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn loopback_guard_refuses_rebinding_and_cross_origin_writes_but_not_sdk_clients() {
        let temp = TempDir::new().expect("temp");
        let calls = Arc::new(Mutex::new(Vec::<Value>::new()));
        let runtime_calls = Arc::clone(&calls);
        let runtime_router = Router::new().route(
            "/v1/messages",
            post(move |Json(body): Json<Value>| {
                let calls = Arc::clone(&runtime_calls);
                async move {
                    calls.lock().expect("calls lock").push(body);
                    Json(json!({"type": "message", "content": []}))
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
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("engramd.sock");
        config.proxy.runtime_url = format!("http://{runtime_address}");
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("proxy bind");
        let proxy_address = proxy_listener.local_addr().expect("proxy address");
        let port = proxy_address.port();
        let (proxy_stop_tx, proxy_stop_rx) = oneshot::channel::<()>();
        let proxy_server = tokio::spawn(serve_on_listener(config, proxy_listener, async move {
            let _ = proxy_stop_rx.await;
        }));

        let client = reqwest::Client::new();
        let request = json!({
            "model": "frontier-test",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "say ok"}]
        });
        let send = |host: Option<String>, origin: Option<String>| {
            let mut builder = client
                .post(format!("http://{proxy_address}/v1/messages"))
                .header("x-engram-mode", "off")
                .json(&request);
            if let Some(host) = host {
                builder = builder.header("host", host);
            }
            if let Some(origin) = origin {
                builder = builder.header("origin", origin);
            }
            builder.send()
        };
        let health = client
            .get(format!("http://{proxy_address}/health"))
            .send()
            .await
            .expect("health");
        assert_eq!(health.status(), StatusCode::OK);
        let rebound = client
            .get(format!("http://{proxy_address}/health"))
            .header("host", format!("evil.example:{port}"))
            .send()
            .await
            .expect("rebound health");
        assert_eq!(rebound.status(), StatusCode::MISDIRECTED_REQUEST);
        // Refused requests never reach the runtime.
        for (host, origin, status) in [
            (
                Some(format!("evil.example:{port}")),
                None,
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                Some(format!("localhost.evil.example:{port}")),
                None,
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                None,
                Some("http://evil.example".to_owned()),
                StatusCode::FORBIDDEN,
            ),
            (None, Some("null".to_owned()), StatusCode::FORBIDDEN),
        ] {
            let response = send(host.clone(), origin.clone())
                .await
                .expect("refused request");
            assert_eq!(response.status(), status, "{host:?} {origin:?}");
        }
        assert!(calls.lock().expect("calls lock").is_empty());
        // SDK clients send no Origin; same-origin and localhost requests pass,
        // and the brain-off request reaches the runtime unchanged.
        for (host, origin) in [
            (None, None),
            (None, Some(format!("http://127.0.0.1:{port}"))),
            (Some(format!("localhost:{port}")), None),
        ] {
            let response = send(host.clone(), origin.clone())
                .await
                .expect("allowed request");
            assert_eq!(response.status(), StatusCode::OK, "{host:?} {origin:?}");
        }
        {
            let calls = calls.lock().expect("calls lock");
            assert_eq!(calls.len(), 3);
            assert!(calls.iter().all(|body| body == &request));
        }

        proxy_stop_tx.send(()).expect("proxy stop");
        runtime_stop_tx.send(()).expect("runtime stop");
        proxy_server.await.expect("proxy task").expect("proxy");
        runtime_server.await.expect("runtime task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn proxy_preserves_tools_stream_content_and_off_control() {
        let temp = TempDir::new().expect("temp");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("engramd.sock");
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
            .header("x-engram-project-path", project.to_string_lossy().as_ref())
            .header("x-engram-mode", "brain")
            .header("x-claude-code-session-id", "session-stable-1")
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
        assert_eq!(response.headers()["x-engram-session-cache"], "miss");
        let injection_hash = response.headers()["x-engram-injection-sha256"].clone();
        let injection_bytes = response.headers()["x-engram-injection-bytes"].clone();
        assert_eq!(
            response.text().await.expect("stream body"),
            "data: {\"delta\":\"ok\"}\n\ndata: [DONE]\n\n"
        );

        let followup_request = json!({
            "model": "local",
            "stream": true,
            "messages": [{"role": "user", "content": "now inspect a different file"}],
            "tools": [{"type": "function", "function": {"name": "list_files"}}]
        });
        let followup = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-engram-project-path", project.to_string_lossy().as_ref())
            .header("x-engram-mode", "brain")
            .header("x-claude-code-session-id", "session-stable-1")
            .json(&followup_request)
            .send()
            .await
            .expect("follow-up request");
        assert_eq!(followup.status(), StatusCode::OK);
        assert_eq!(followup.headers()["x-engram-session-cache"], "hit");
        assert_eq!(
            followup.headers()["x-engram-injection-sha256"],
            injection_hash
        );
        assert_eq!(
            followup.headers()["x-engram-injection-bytes"],
            injection_bytes
        );
        let _ = followup.text().await.expect("follow-up body");

        let off = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-engram-mode", "off")
            .header("x-claude-code-session-id", "session-stable-1")
            .json(&request)
            .send()
            .await
            .expect("off request");
        assert_eq!(off.status(), StatusCode::OK);
        assert!(!off.headers().contains_key("x-engram-session-cache"));
        let _ = off.text().await.expect("off response body");
        {
            let captured = captured.lock().expect("capture lock");
            assert_eq!(captured.len(), 3);
            assert_eq!(captured[0].1["tools"], request["tools"]);
            assert_eq!(captured[0].1["messages"][0]["role"], "system");
            assert_eq!(
                captured[0].1["messages"][0], captured[1].1["messages"][0],
                "session turns must reuse byte-identical injected context"
            );
            assert_eq!(captured[1].1["tools"], followup_request["tools"]);
            assert_eq!(
                captured[1].1["messages"][1],
                followup_request["messages"][0]
            );
            assert_eq!(captured[2].1, request);
            assert!(!captured[0].0.contains_key("x-engram-project-path"));
        }

        daemon_stop_tx.send(()).expect("daemon stop");
        daemon_server.await.expect("daemon task").expect("daemon");
        let degraded = reqwest::Client::new()
            .post(format!("http://{proxy_address}/v1/chat/completions"))
            .header("x-engram-project-path", project.to_string_lossy().as_ref())
            .header("x-engram-mode", "brain")
            .json(&request)
            .send()
            .await
            .expect("fail-open request");
        assert_eq!(degraded.status(), StatusCode::OK);
        assert_eq!(
            degraded
                .headers()
                .get("x-engram-degraded")
                .expect("degraded"),
            "context_unavailable"
        );
        let _ = degraded.text().await.expect("degraded response body");
        assert_eq!(
            captured.lock().expect("capture lock")[3].1,
            request,
            "fail-open must forward the original semantic request"
        );

        let outcomes: crate::metrics::ProxyMetricsSnapshot = reqwest::Client::new()
            .get(format!("http://{proxy_address}/engram/metrics"))
            .send()
            .await
            .expect("proxy metrics request")
            .json()
            .await
            .expect("proxy metrics body");
        assert_eq!(outcomes.injected, 2, "{outcomes:?}");
        assert_eq!(outcomes.session_cache_misses, 1, "{outcomes:?}");
        assert_eq!(outcomes.session_cache_hits, 1, "{outcomes:?}");
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
        git(
            &project,
            &["config", "user.email", "engram@example.invalid"],
        );
        git(&project, &["config", "user.name", "Engram Tests"]);
        git(&project, &["add", "src/lib.rs"]);
        git(&project, &["commit", "-qm", "initial"]);

        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("engramd.sock");
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
            .header("x-engram-project-path", project.to_string_lossy().as_ref())
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn dirty_working_files_bias_injection_toward_touched_code() {
        // A/B of the dirty-files fallback: two equally lexical memories, one
        // anchored to src/auth.rs. With a CLEAN tree the decoy's importance
        // edge (0.05 weight) wins the single injection slot; once auth.rs is
        // dirty, the structural-overlap term (0.18 weight) must flip the
        // slot to the memory anchored to the code actually being touched.
        let temp = TempDir::new().expect("temp");
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join("src")).expect("project");
        std::fs::write(
            project.join("src/auth.rs"),
            "pub fn check_token() -> bool { true }\n",
        )
        .expect("auth");
        std::fs::write(
            project.join("src/billing.rs"),
            "pub fn charge() -> bool { true }\n",
        )
        .expect("billing");
        git(&project, &["init", "-q"]);
        git(
            &project,
            &["config", "user.email", "engram@example.invalid"],
        );
        git(&project, &["config", "user.name", "Engram Tests"]);
        git(&project, &["add", "."]);
        git(&project, &["commit", "-qm", "initial"]);
        // The single-slot cap must live in the SCOPED (repo) config: the
        // compile handler prefers it over the daemon's global config.
        std::fs::create_dir_all(project.join(".engram")).expect("scoped dir");
        std::fs::write(
            project.join(".engram/config.toml"),
            "[context]\nmax_injected_memories = 1\n",
        )
        .expect("scoped config");

        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("engramd.sock");
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
        let project_id = indexed.resolved.project.id.clone();
        let nodes = daemon
            .search_world(&crate::daemon::WorldSearchRequest {
                project_id: project_id.clone(),
                query: "check_token".to_owned(),
                limit: 10,
            })
            .await
            .expect("node search");
        let auth_node = nodes
            .iter()
            .find(|node| node.label == "check_token")
            .expect("auth node");

        let seed = |claim: &str, importance: f64, node_ids: Vec<crate::ids::NodeId>| {
            let daemon = daemon.clone();
            let project_id = project_id.clone();
            let claim = claim.to_owned();
            async move {
                let memory = daemon
                    .create_memory(&MemoryCreateRequest {
                        project_id: project_id.clone(),
                        memory: MemoryDraft {
                            scope_type: "project".to_owned(),
                            scope_id: project_id.to_string(),
                            kind: "lesson".to_owned(),
                            claim,
                            rationale: "fixture".to_owned(),
                            confidence: 0.5,
                            importance,
                            tags: vec!["session".to_owned()],
                            valid_from_commit: None,
                            valid_to_commit: None,
                            trust_source: "human_instruction".to_owned(),
                            evidence_ids: Vec::new(),
                            source_task_ids: Vec::new(),
                            node_ids,
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
            }
        };
        seed(
            "Session flow helpers stay beside their token checks",
            0.6,
            vec![auth_node.id.clone()],
        )
        .await;
        seed(
            "Session flow constants stay beside their price tables",
            0.9,
            Vec::new(),
        )
        .await;

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

        let send_task = |task: &str| {
            let task = task.to_owned();
            let project = project.clone();
            async move {
                let response = reqwest::Client::new()
                    .post(format!("http://{proxy_address}/v1/chat/completions"))
                    .header("x-engram-project-path", project.to_string_lossy().as_ref())
                    .json(&json!({
                        "model": "local",
                        "messages": [{"role": "user", "content": task}]
                    }))
                    .send()
                    .await
                    .expect("proxy request");
                assert!(response.status().is_success());
            }
        };

        // Arm A: clean tree. The importance edge picks the decoy.
        send_task("tidy the session flow").await;
        // Arm B: auth.rs becomes dirty WITHOUT changing the anchored symbol
        // (append-only edit, so drift quarantine must not fire).
        std::fs::write(
            project.join("src/auth.rs"),
            "pub fn check_token() -> bool { true }\npub fn refresh_token() -> bool { true }\n",
        )
        .expect("append edit");
        send_task("tidy the session flow again").await;

        let (clean, dirty) = {
            let forwarded = captured.lock().expect("capture");
            assert_eq!(forwarded.len(), 2);
            (forwarded[0].to_string(), forwarded[1].to_string())
        };
        assert!(
            clean.contains("price tables") && !clean.contains("token checks"),
            "clean tree: importance edge wins the single slot: {clean}"
        );
        assert!(
            dirty.contains("token checks") && !dirty.contains("price tables"),
            "dirty auth.rs: structural overlap must flip the slot: {dirty}"
        );

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
