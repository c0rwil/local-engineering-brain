//! Local workspace browser: a loopback-only HTTP server that serves a
//! self-contained page over the daemon's data.
//!
//! The web process owns NO data: every read and write goes through the
//! daemon's Unix-socket API, so single-writer discipline holds and the UI
//! can never see torn state. The page is embedded in the binary (no CDN, no
//! network assets) and the listener binds 127.0.0.1 only — this is an
//! onboarding surface for the local engineer, not a service.

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::daemon::{
    DaemonClient, FactSetRequest, FactUnsetRequest, LinkScanRequest, MemoryCreateRequest,
    MemoryPromoteRequest, MemoryProvenanceRequest, ModuleMapRequest, ModuleSymbolsRequest,
};
use crate::error::{Error, Result};
use crate::ids::ProjectId;
use crate::memory::{EpistemicState, MemoryDraft};

const INDEX_HTML: &str = include_str!("../assets/web/index.html");

#[derive(Clone)]
struct WebState {
    socket_path: PathBuf,
    request_timeout: Duration,
    architect_auto: String,
    token: Option<String>,
}

impl WebState {
    fn client(&self) -> DaemonClient {
        DaemonClient::new(&self.socket_path, self.request_timeout)
    }
}

#[derive(Deserialize)]
struct WebFactSet {
    project_id: ProjectId,
    key: String,
    value: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct WebFactUnset {
    project_id: ProjectId,
    key: String,
}

#[derive(Deserialize)]
struct WebLinkScan {
    project_id: ProjectId,
    #[serde(default)]
    apply: bool,
}

#[derive(Serialize)]
struct ArchitectureDoc {
    content: Option<String>,
    /// `Some(true)` when the workspace's stable projection (services,
    /// facts, links, module names, routes) has drifted past the prose.
    stale: Option<bool>,
    /// The configured auto-regeneration mode ("off" | "local" | "claude").
    auto: String,
}

struct WebError(Error);

impl IntoResponse for WebError {
    fn into_response(self) -> axum::response::Response {
        (axum::http::StatusCode::BAD_GATEWAY, self.0.to_string()).into_response()
    }
}

impl From<Error> for WebError {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

/// Build the router. Exposed for tests; production entry is [`serve`].
pub fn router(config: &Config) -> Router {
    let state = WebState {
        socket_path: config.daemon.socket_path.clone(),
        request_timeout: Duration::from_millis(config.daemon.request_timeout_ms),
        architect_auto: config.architect.auto.clone(),
        token: config.web.token.clone(),
    };
    Router::new()
        .route("/", get(index))
        .route(
            "/product.js",
            get(|| async {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/javascript; charset=utf-8",
                    )],
                    include_str!("../assets/web/product.js"),
                )
            }),
        )
        .route(
            "/product.css",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../assets/web/product.css"),
                )
            }),
        )
        .route("/api/product/lessons", post(lesson_mutate))
        .route("/api/product/query", post(product_query))
        .route("/api/product/mutate", post(product_mutate))
        .route("/api/workspace", get(workspace))
        .route("/api/architecture", get(architecture))
        .route("/api/activity", get(activity))
        .route("/api/modulemap", post(module_map))
        .route("/api/modulesymbols", post(module_symbols))
        .route("/api/provenance", post(provenance))
        .route("/api/symbolsearch", post(symbol_search))
        .route("/api/memory/promote", post(memory_promote))
        .route("/api/memory/forget", post(memory_forget))
        .route("/api/memory/add", post(memory_add))
        .route("/api/reindex", post(reindex))
        .route("/api/facts/set", post(fact_set))
        .route("/api/facts/unset", post(fact_unset))
        .route("/api/linkscan", post(link_scan))
        .route("/webhook/repo-updated", post(webhook_repo_updated))
        .route("/api/sync/knowledge", get(sync_knowledge))
        .route("/api/sync/offer", post(sync_offer))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ))
        .with_state(state)
}

/// Bearer-token gate for data routes. The page itself (`/`) is static HTML
/// and stays open; every /api and /webhook request needs the token when one
/// is configured.
async fn require_token(
    State(state): State<WebState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let path = request.uri().path();
    let guarded = path.starts_with("/api") || path.starts_with("/webhook");
    if guarded && let Some(expected) = &state.token {
        let presented = request
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim);
        if presented != Some(expected.as_str()) {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                "missing or invalid bearer token",
            )
                .into_response();
        }
    }
    next.run(request).await
}

async fn sync_knowledge(
    State(state): State<WebState>,
) -> std::result::Result<Json<crate::sync::SyncKnowledge>, WebError> {
    Ok(Json(state.client().sync_export().await?))
}

async fn sync_offer(
    State(state): State<WebState>,
    Json(offers): Json<crate::sync::SyncKnowledge>,
) -> std::result::Result<Json<crate::sync::OfferReport>, WebError> {
    Ok(Json(state.client().sync_accept(&offers).await?))
}

/// CI-driven freshness: after a merge to main, the pipeline POSTs the repo
/// name here and the shared-truth view refreshes immediately instead of
/// waiting for the origin poll.
#[derive(Deserialize)]
struct WebhookRepoUpdated {
    repo: String,
}

async fn webhook_repo_updated(
    State(state): State<WebState>,
    Json(request): Json<WebhookRepoUpdated>,
) -> std::result::Result<Json<serde_json::Value>, WebError> {
    let client = state.client();
    let snapshot = client.workspace().await?;
    let project = snapshot
        .projects
        .iter()
        .find(|project| project.name == request.repo)
        .ok_or_else(|| {
            WebError(Error::Config(format!(
                "unknown repository `{}`",
                request.repo
            )))
        })?;
    let patient = DaemonClient::new(&state.socket_path, Duration::from_secs(600));
    let response = patient
        .origin_refresh(&crate::daemon::OriginRefreshRequest {
            project_id: project.id.clone(),
        })
        .await?;
    Ok(Json(serde_json::json!({
        "repo": request.repo,
        "refreshed": response.reference,
    })))
}

/// Serve the workspace browser until the task is cancelled.
///
/// # Errors
///
/// Returns bind or serve errors, or a config error when a non-loopback bind
/// has no token.
pub async fn serve(config: Config, port: u16) -> Result<()> {
    let bind = config.web.bind.clone();
    let loopback = bind == "127.0.0.1" || bind == "localhost" || bind == "::1";
    if !loopback && config.web.token.is_none() {
        return Err(Error::Config(
            "binding the browser beyond loopback requires [web] token".to_owned(),
        ));
    }
    let app = router(&config);
    // A restarting daemon races the previous instance releasing the port
    // (`daemon stop` returns before the process exits). Retry the bind for a
    // bounded window instead of silently running without the browser for the
    // daemon's whole lifetime — observed twice after upgrades.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let listener = loop {
        match tokio::net::TcpListener::bind((bind.as_str(), port)).await {
            Ok(listener) => break listener,
            Err(error) if std::time::Instant::now() < deadline => {
                let _ = error;
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            Err(error) => {
                return Err(Error::Config(format!(
                    "web bind {bind}:{port}: {error} (gave up after 30 s)"
                )));
            }
        }
    };
    axum::serve(listener, app)
        .await
        .map_err(|error| Error::Config(format!("web serve: {error}")))
}

async fn index() -> impl IntoResponse {
    // no-cache (NOT no-store): the browser revalidates on every load, so a
    // redeployed daemon never serves a user a stale page from memory cache.
    (
        [(
            axum::http::header::CACHE_CONTROL,
            "no-cache, must-revalidate",
        )],
        Html(INDEX_HTML),
    )
}

async fn workspace(
    State(state): State<WebState>,
) -> std::result::Result<Json<crate::daemon::WorkspaceSnapshot>, WebError> {
    Ok(Json(state.client().workspace().await?))
}

#[derive(Deserialize)]
struct WebModuleMap {
    project_id: ProjectId,
}

async fn module_map(
    State(state): State<WebState>,
    Json(request): Json<WebModuleMap>,
) -> std::result::Result<Json<crate::vault::ModuleMap>, WebError> {
    // Match the daemon's long deadline: the edge-resolution join is slow.
    let patient = DaemonClient::new(&state.socket_path, Duration::from_secs(180));
    Ok(Json(
        patient
            .module_map(&ModuleMapRequest {
                project_id: request.project_id,
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct WebModuleSymbols {
    project_id: ProjectId,
    group: String,
}

async fn module_symbols(
    State(state): State<WebState>,
    Json(request): Json<WebModuleSymbols>,
) -> std::result::Result<Json<crate::daemon::ModuleSymbols>, WebError> {
    Ok(Json(
        state
            .client()
            .module_symbols(&ModuleSymbolsRequest {
                project_id: request.project_id,
                group: request.group,
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct WebProvenance {
    memory_id: String,
}

async fn provenance(
    State(state): State<WebState>,
    Json(request): Json<WebProvenance>,
) -> std::result::Result<Json<crate::daemon::MemoryProvenance>, WebError> {
    Ok(Json(
        state
            .client()
            .memory_provenance(&MemoryProvenanceRequest {
                memory_id: request.memory_id,
            })
            .await?,
    ))
}

#[derive(Deserialize)]
struct WebSymbolSearch {
    q: String,
}

#[derive(Serialize)]
struct SymbolHit {
    project: String,
    kind: String,
    label: String,
    file_path: Option<String>,
}

async fn symbol_search(
    State(state): State<WebState>,
    Json(request): Json<WebSymbolSearch>,
) -> std::result::Result<Json<Vec<SymbolHit>>, WebError> {
    let query = request.q.trim().to_owned();
    if query.len() < 2 || query.len() > 120 {
        return Ok(Json(Vec::new()));
    }
    let client = state.client();
    let snapshot = client.workspace().await?;
    let mut hits = Vec::new();
    for project in &snapshot.projects {
        let Ok(nodes) = client
            .search_world(&crate::daemon::WorldSearchRequest {
                project_id: project.id.clone(),
                query: query.clone(),
                limit: 6,
            })
            .await
        else {
            continue;
        };
        for node in nodes {
            if node.kind == "ExternalReference" {
                continue;
            }
            hits.push(SymbolHit {
                project: project.name.clone(),
                kind: node.kind,
                label: node.label,
                file_path: node.file_path,
            });
            if hits.len() >= 36 {
                return Ok(Json(hits));
            }
        }
    }
    Ok(Json(hits))
}

#[derive(Deserialize)]
struct WebMemoryId {
    memory_id: String,
}

async fn memory_promote(
    State(state): State<WebState>,
    Json(request): Json<WebMemoryId>,
) -> std::result::Result<Json<crate::memory::MemoryRecord>, WebError> {
    let id = crate::ids::MemoryId::parse(&request.memory_id)?;
    // A curator clicking Promote IS the human override; the CLI keeps the
    // stepwise evidence-gated path for automation.
    Ok(Json(
        state
            .client()
            .promote_memory(
                &id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await?,
    ))
}

async fn memory_forget(
    State(state): State<WebState>,
    Json(request): Json<WebMemoryId>,
) -> std::result::Result<Json<crate::memory::MemoryRecord>, WebError> {
    let id = crate::ids::MemoryId::parse(&request.memory_id)?;
    Ok(Json(state.client().forget_memory(&id).await?))
}

#[derive(Deserialize)]
struct WebMemoryAdd {
    project_id: ProjectId,
    kind: String,
    claim: String,
    #[serde(default)]
    rationale: Option<String>,
}

async fn memory_add(
    State(state): State<WebState>,
    Json(request): Json<WebMemoryAdd>,
) -> std::result::Result<Json<crate::memory::MemoryRecord>, WebError> {
    if !matches!(
        request.kind.as_str(),
        "lesson" | "preference" | "constraint"
    ) {
        return Err(WebError(Error::Config(
            "kind must be lesson, preference, or constraint".to_owned(),
        )));
    }
    let record = state
        .client()
        .create_memory(&MemoryCreateRequest {
            project_id: request.project_id.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: request.project_id.to_string(),
                kind: request.kind,
                claim: request.claim,
                rationale: request
                    .rationale
                    .unwrap_or_else(|| "Recorded by a human in the workspace browser".to_owned()),
                confidence: 0.5,
                importance: 0.8,
                tags: vec!["human".to_owned()],
                valid_from_commit: None,
                valid_to_commit: None,
                trust_source: "human_instruction".to_owned(),
                evidence_ids: Vec::new(),
                source_task_ids: Vec::new(),
                node_ids: Vec::new(),
            },
        })
        .await?;
    Ok(Json(record))
}

#[derive(Deserialize)]
struct WebReindex {
    project_id: ProjectId,
    /// `"local"` opts into the developer's checkout; anything else (the
    /// default) indexes shared truth — fetched `origin/<default>` in a
    /// throwaway worktree that never touches the checkout.
    #[serde(default)]
    source: Option<String>,
}

async fn reindex(
    State(state): State<WebState>,
    Json(request): Json<WebReindex>,
) -> std::result::Result<Json<serde_json::Value>, WebError> {
    let snapshot = state.client().workspace().await?;
    let path = snapshot
        .projects
        .iter()
        .find(|project| project.id == request.project_id)
        .and_then(|project| project.primary_path.clone())
        .ok_or_else(|| {
            WebError(Error::Config(
                "this project has no known local checkout to refresh".to_owned(),
            ))
        })?;
    let use_local = request.source.as_deref() == Some("local");
    // Index scans can outlive the default deadline on large repos.
    let patient = DaemonClient::new(&state.socket_path, Duration::from_secs(600));
    if use_local {
        let response = patient.refresh_index(std::path::Path::new(&path)).await?;
        return Ok(Json(serde_json::json!({
            "nodes_written": response.index.nodes_written,
            "source": "local checkout",
        })));
    }
    let response = patient
        .origin_refresh(&crate::daemon::OriginRefreshRequest {
            project_id: request.project_id,
        })
        .await?;
    Ok(Json(serde_json::json!({
        "nodes_written": 0,
        "source": response.reference,
    })))
}

async fn activity(
    State(state): State<WebState>,
) -> std::result::Result<Json<Vec<crate::daemon::ActivityEvent>>, WebError> {
    Ok(Json(state.client().workspace_activity().await?))
}

async fn architecture(State(state): State<WebState>) -> Json<ArchitectureDoc> {
    let content = state
        .client()
        .product_query(&crate::product::Query::Guide)
        .await
        .ok()
        .and_then(|value| {
            serde_json::from_value::<Option<crate::product::Record>>(value)
                .ok()
                .flatten()
        })
        .filter(|record| !record.archived)
        .and_then(|record| match record.item {
            crate::product::Item::Guide(guide) => Some(guide.content),
            _ => None,
        });
    let drifted = match &content {
        Some(content) => match crate::architect::stamped_digest(content) {
            Some(recorded) => state
                .client()
                .architect_dossier()
                .await
                .ok()
                .map(|dossier| crate::architect::stable_digest(&dossier) != recorded),
            None => None,
        },
        None => None,
    };
    Json(ArchitectureDoc {
        content,
        stale: drifted,
        auto: state.architect_auto.clone(),
    })
}

async fn fact_set(
    State(state): State<WebState>,
    Json(request): Json<WebFactSet>,
) -> std::result::Result<Json<crate::projects::ProjectFact>, WebError> {
    let fact = state
        .client()
        .set_fact(&FactSetRequest {
            project_id: request.project_id,
            key: request.key,
            value: request.value,
            note: request.note,
        })
        .await?;
    Ok(Json(fact))
}

async fn fact_unset(
    State(state): State<WebState>,
    Json(request): Json<WebFactUnset>,
) -> std::result::Result<Json<crate::daemon::FactUnsetResponse>, WebError> {
    let response = state
        .client()
        .unset_fact(&FactUnsetRequest {
            project_id: request.project_id,
            key: request.key,
        })
        .await?;
    Ok(Json(response))
}

async fn link_scan(
    State(state): State<WebState>,
    Json(request): Json<WebLinkScan>,
) -> std::result::Result<Json<crate::linkscan::LinkScanReport>, WebError> {
    // Resolve the repo's checkout through the daemon's own record of it, so
    // the browser never supplies filesystem paths.
    let snapshot = state.client().workspace().await?;
    let path = snapshot
        .projects
        .iter()
        .find(|project| project.id == request.project_id)
        .and_then(|project| project.primary_path.clone())
        .ok_or_else(|| {
            WebError(Error::Config(
                "this project has no known local checkout to scan".to_owned(),
            ))
        })?;
    let report = state
        .client()
        .scan_links(&LinkScanRequest {
            path: PathBuf::from(path),
            apply: request.apply,
        })
        .await?;
    Ok(Json(report))
}

async fn product_query(
    State(state): State<WebState>,
    Json(request): Json<crate::product::Query>,
) -> std::result::Result<Json<serde_json::Value>, WebError> {
    Ok(Json(state.client().product_query(&request).await?))
}
async fn product_mutate(
    State(state): State<WebState>,
    Json(request): Json<crate::product::Mutation>,
) -> std::result::Result<Json<crate::product::Record>, WebError> {
    Ok(Json(state.client().product_mutate(&request).await?))
}

async fn lesson_mutate(
    State(state): State<WebState>,
    Json(request): Json<crate::product::lessons::LessonMutation>,
) -> std::result::Result<Json<serde_json::Value>, WebError> {
    Ok(Json(state.client().lesson_mutate(&request).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn product_data_routes_obey_the_existing_token_gate() {
        let mut config = Config::default();
        config.web.token = Some("test-token".to_owned());
        for path in [
            "/api/product/query",
            "/api/product/mutate",
            "/api/product/lessons",
        ] {
            let response = tower::ServiceExt::oneshot(
                router(&config),
                axum::http::Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn index_serves_the_embedded_page() {
        let config = Config::default();
        let app = router(&config);
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/")
                .body(axum::body::Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1_048_576)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("<title>Engram Workspace</title>"));
        assert!(text.contains("Skip to Content"));
        assert!(
            !text.contains("http://") && !text.contains("https://cdn"),
            "page must be self-contained"
        );
    }
}
