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
    DaemonClient, FactSetRequest, FactUnsetRequest, LinkScanRequest, ModuleMapRequest,
    ModuleSymbolsRequest,
};
use crate::error::{Error, Result};
use crate::ids::ProjectId;

const INDEX_HTML: &str = include_str!("../assets/web/index.html");

#[derive(Clone)]
struct WebState {
    socket_path: PathBuf,
    request_timeout: Duration,
    vault_root: PathBuf,
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
    /// `Some(true)` when the workspace dossier has changed since the guide
    /// was generated (facts, links, or code structure drifted).
    stale: Option<bool>,
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
        vault_root: config.vault.root.clone(),
    };
    Router::new()
        .route("/", get(index))
        .route("/api/workspace", get(workspace))
        .route("/api/architecture", get(architecture))
        .route("/api/activity", get(activity))
        .route("/api/modulemap", post(module_map))
        .route("/api/modulesymbols", post(module_symbols))
        .route("/api/reindex", post(reindex))
        .route("/api/facts/set", post(fact_set))
        .route("/api/facts/unset", post(fact_unset))
        .route("/api/linkscan", post(link_scan))
        .with_state(state)
}

/// Serve the workspace browser on `127.0.0.1:port` until the task is
/// cancelled (Ctrl-C in the CLI).
///
/// # Errors
///
/// Returns bind or serve errors.
pub async fn serve(config: Config, port: u16) -> Result<()> {
    let app = router(&config);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| Error::Config(format!("web bind 127.0.0.1:{port}: {error}")))?;
    axum::serve(listener, app)
        .await
        .map_err(|error| Error::Config(format!("web serve: {error}")))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
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
struct WebReindex {
    project_id: ProjectId,
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
    // Index scans can outlive the default deadline on large repos.
    let patient = DaemonClient::new(&state.socket_path, Duration::from_secs(300));
    let response = patient.refresh_index(std::path::Path::new(&path)).await?;
    Ok(Json(serde_json::json!({
        "nodes_written": response.index.nodes_written,
        "head": response.resolved.git.and_then(|git| git.head),
    })))
}

async fn activity(
    State(state): State<WebState>,
) -> std::result::Result<Json<Vec<crate::daemon::ActivityEvent>>, WebError> {
    Ok(Json(state.client().workspace_activity().await?))
}

async fn architecture(State(state): State<WebState>) -> Json<ArchitectureDoc> {
    let content = std::fs::read_to_string(state.vault_root.join("ARCHITECTURE.md")).ok();
    let drifted = match &content {
        Some(content) => {
            let recorded = content
                .split_once("dossier-sha256: ")
                .and_then(|(_, rest)| rest.get(..64));
            match recorded {
                Some(recorded) => state
                    .client()
                    .architect_dossier()
                    .await
                    .ok()
                    .map(|dossier| crate::architect::dossier_digest(&dossier) != recorded),
                None => None,
            }
        }
        None => None,
    };
    Json(ArchitectureDoc {
        content,
        stale: drifted,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(text.contains("<title>LEB Workspace</title>"));
        assert!(text.contains("Skip to Content"));
        assert!(
            !text.contains("http://") && !text.contains("https://cdn"),
            "page must be self-contained"
        );
    }
}
