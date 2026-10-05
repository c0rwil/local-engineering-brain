//! Local single-owner daemon and Unix-socket API.
//!
//! `engramd` owns canonical database connections and the bounded mutation
//! scheduler. Concurrent local clients never need to coordinate `SQLite` writes
//! themselves, and overload is surfaced explicitly instead of growing memory.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::extract::Path as AxumPath;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::cache::{CacheStats, PartitionedCache};
use crate::config::{Config, ConfigOverrides};
use crate::context::{
    CompiledContext, ContextCompileRequest, ContextCompiler, ContextItem, ContextMode,
};
use crate::doctor::DoctorReport;
use crate::error::{Error, Result, io_error};
use crate::evaluation::{EvaluationRecord, EvaluationStore};
use crate::experience::{
    AttemptOutcome, AttemptRecord, DEFAULT_BLOB_THRESHOLD_BYTES, EvidenceRecord, EvidenceStrength,
    ExperienceStore, PastFailure, TaskOutcome, TaskRecord,
};
use crate::export::{ExportReport, export_project};
use crate::ids::{AttemptId, ContextRunId, EvidenceId, JobId, MemoryId, NodeId, ProjectId, TaskId};
use crate::jobs::{EnqueueJob, JobExecutor, JobQueue, JobRecord, JobRunner, JobStatus, JobType};
use crate::learning::{DistillationRequest, DistillationResult, LearningStore};
use crate::memory::{EpistemicState, MemoryDraft, MemoryRecord, MemoryStore};
use crate::metrics::{EndpointGroup, MetricsRegistry, MetricsSnapshot};
use crate::projects::{LinkedProject, ProjectRecord, ProjectRegistry, ResolvedProject};
use crate::scheduler::{MutationPriority, MutationScheduler, SchedulerStats};
use crate::security::Redactor;
use crate::semantic::{LocalEmbedderSettings, ModelRebuildReport, SemanticOptions};
use crate::storage::{Database, IntegrityReport, ReadPoolStats};
use crate::synthesis::{SynthesisOptions, SynthesisReport, SynthesisStore};
use crate::world::{
    GitHistory, GitIngestReport, GraphNode, GraphSlice, GraphifyProvider, IndexReport,
    StructuralIndexStatus, StructuralProvider, TreeSitterProvider, WorldModel,
};

const API_HEALTH: &str = "/api/v1/health";
const API_DOCTOR: &str = "/api/v1/admin/doctor";
const API_SHUTDOWN: &str = "/api/v1/admin/shutdown";
const API_RESOLVE_PROJECT: &str = "/api/v1/projects/resolve";
const API_LOOKUP_PROJECT: &str = "/api/v1/projects/lookup";
const API_FILE_SHAPE: &str = "/api/v1/world/file-shape";
const API_TASKS_BRIEF: &str = "/api/v1/tasks/brief";
const API_INDEX_PROJECT: &str = "/api/v1/projects/index";
const API_WORLD_SEARCH: &str = "/api/v1/world/search";
const API_CONTEXT_COMPILE: &str = "/api/v1/context/compile";
const API_CONTEXT_DELIVERED: &str = "/api/v1/context/delivered";
const API_MEMORY_CREATE: &str = "/api/v1/memories";
const API_TASKS: &str = "/api/v1/tasks";
const API_EVIDENCE: &str = "/api/v1/evidence";
const API_HOOK_EVIDENCE: &str = "/api/v1/hook-evidence";
const API_EVALUATION_RUNS: &str = "/api/v1/evaluation/runs";
const API_LEARNING_DISTILL: &str = "/api/v1/learning/distill";
const API_MEMORY_SEARCH: &str = "/api/v1/memories/search";
const API_WORLD_NEIGHBORS: &str = "/api/v1/world/neighbors";
const API_CONTEXT_EXPLAIN: &str = "/api/v1/context/explain";
const API_PAST_FAILURES: &str = "/api/v1/experience/failures";
const API_BACKUP_CREATE: &str = "/api/v1/admin/backups";
const API_BACKUP_VERIFY: &str = "/api/v1/admin/backups/verify";
const API_PROJECT_EXPORT: &str = "/api/v1/admin/export";
const API_SEMANTIC_REBUILD: &str = "/api/v1/semantic/rebuild";
const API_METRICS: &str = "/api/v1/metrics";
const API_MEMORY_STATS: &str = "/api/v1/metrics/memories";
const API_JOBS: &str = "/api/v1/jobs";
const API_JOBS_LIST: &str = "/api/v1/jobs/list";
const API_SYNTHESIS_RUN: &str = "/api/v1/synthesis/run";
const API_SKILL_PROMOTE: &str = "/api/v1/synthesis/promote";
const API_PROJECT_LINK: &str = "/api/v1/projects/link";
const API_PROJECT_UNLINK: &str = "/api/v1/projects/unlink";
const API_PROJECT_LINKS: &str = "/api/v1/projects/links";
const API_VAULT_BUILD: &str = "/api/v1/vault/build";
const API_LINK_SCAN: &str = "/api/v1/projects/link-scan";
const API_FACT_SET: &str = "/api/v1/projects/facts/set";
const API_FACT_UNSET: &str = "/api/v1/projects/facts/unset";
const API_FACT_LIST: &str = "/api/v1/projects/facts/list";
const API_ARCHITECT_DOSSIER: &str = "/api/v1/architect/dossier";
const API_WORKSPACE: &str = "/api/v1/workspace";
const API_SYNC_EXPORT: &str = "/api/v1/sync/export";
const API_SYNC_OFFERS: &str = "/api/v1/sync/offers";
const API_SYNC_APPLY: &str = "/api/v1/sync/apply";
const API_SYNC_ACCEPT: &str = "/api/v1/sync/accept";
const API_ACTIVITY: &str = "/api/v1/workspace/activity";
const API_MODULE_MAP: &str = "/api/v1/projects/module-map";
const API_MODULE_SYMBOLS: &str = "/api/v1/projects/module-symbols";
const API_ORIGIN_REFRESH: &str = "/api/v1/projects/origin-refresh";
const API_MEMORY_PROVENANCE: &str = "/api/v1/memories/provenance";
const API_EFFECTIVENESS: &str = "/api/v1/effectiveness";
const API_USAGE_RECORD: &str = "/api/v1/effectiveness/usage";
const API_TOOL_OBSERVATION: &str = "/api/v1/effectiveness/tool";
const API_VERIFICATION_OBSERVATION: &str = "/api/v1/effectiveness/verification";
const API_JUDGE_STATUS: &str = "/api/v1/judge";
const API_JUDGE_RUN: &str = "/api/v1/judge/run";
const API_REVERIFY_PLAN: &str = "/api/v1/reviews/reverify/plan";
const API_REVERIFY_ATTACH: &str = "/api/v1/reviews/reverify/attach";
const API_REVIEW_EXPIRE: &str = "/api/v1/reviews/expire";
const API_PROJECT_MERGE: &str = "/api/v1/projects/merge";
const API_PROJECT_MERGE_FINALIZE: &str = "/api/v1/projects/merge/finalize";

/// Language builtins and assertion macros say nothing about what a module
/// DOES; the module-detail calls list excludes them.
const CALL_NOISE: &[&str] = &[
    "Some",
    "None",
    "Ok",
    "Err",
    "assert",
    "assert_eq",
    "assert_ne",
    "format",
    "println",
    "print",
    "eprintln",
    "vec",
    "panic",
    "clone",
    "into",
    "from",
    "unwrap",
    "expect",
    "new",
    "len",
    "push",
    "insert",
    "get",
    "to_owned",
    "to_string",
    "as_ref",
    "as_str",
    "iter",
    "collect",
    "map",
    "and_then",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "ok_or_else",
    "matches",
    "write",
    "writeln",
    "default",
    "drop",
    "Box",
    "Arc",
    "String",
    "Vec",
    "require",
    "console",
    "log",
    "JSON",
    "String",
    "Number",
    "Boolean",
    "Array",
    "Object",
    "Promise",
    "await",
    "fetch",
    "len",
    "str",
    "int",
    "float",
    "list",
    "dict",
    "set",
    "print",
    "isinstance",
    "super",
    "range",
    "enumerate",
    "tuple",
    "bool",
    "zip",
    "sorted",
    "getattr",
    "hasattr",
    "logger.info",
    "logger.debug",
    "logger.warning",
    "logger.error",
    "logger.exception",
];

#[derive(Clone)]
struct DaemonState {
    config: Arc<Config>,
    database: Database,
    redactor: Redactor,
    scheduler: MutationScheduler,
    blocking_slots: Arc<Semaphore>,
    background_slots: Arc<Semaphore>,
    judge_slot: Arc<Semaphore>,
    judge_stopping: Arc<std::sync::atomic::AtomicBool>,
    world_cache: Option<Arc<WorldReadCache>>,
    metrics: Arc<MetricsRegistry>,
    shutdown: Arc<tokio::sync::Notify>,
    started: Instant,
    /// Set once shutdown starts: new requests get `draining` while the
    /// listener stays open, so clients do not start a second daemon.
    draining: Arc<std::sync::atomic::AtomicBool>,
    /// Requests admitted and not yet answered.
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// Survivor of every merged project id. Requests that still carry a
    /// merged id are redirected to its survivor.
    merged_projects: MergedProjects,
}

type MergedProjects = Arc<std::sync::RwLock<Arc<std::collections::BTreeMap<String, String>>>>;

/// Bounded derived cache over graph read endpoints. Values are never
/// canonical: keys carry a per-project generation that graph mutations bump
/// before their response returns, so a stale entry can never be served after
/// the write that superseded it completes.
struct WorldReadCache {
    search: PartitionedCache<(String, usize), Vec<GraphNode>>,
    neighbors: PartitionedCache<(NodeId, usize, usize), GraphSlice>,
    generations: std::sync::Mutex<std::collections::HashMap<ProjectId, u64>>,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorldCacheHealth {
    pub search: CacheStats,
    pub neighbors: CacheStats,
    pub hits: u64,
    pub misses: u64,
}

impl WorldReadCache {
    fn from_config(cache: &crate::config::CacheConfig) -> Option<Arc<Self>> {
        if !cache.enabled {
            return None;
        }
        // Each endpoint cache receives half of the configured global budget.
        let half_budget = cache.total_budget_mb.saturating_mul(1024 * 1024) / 2;
        let projects = usize::from(cache.max_active_projects.max(1));
        let max_item = cache.max_item_kb.saturating_mul(1024);
        Some(Arc::new(Self {
            search: PartitionedCache::new(half_budget.max(1), projects, max_item),
            neighbors: PartitionedCache::new(half_budget.max(1), projects, max_item),
            generations: std::sync::Mutex::new(std::collections::HashMap::new()),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }))
    }

    fn generation(&self, project_id: &ProjectId) -> u64 {
        self.generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(project_id)
            .copied()
            .unwrap_or(0)
    }

    /// Invalidate all cached reads for a project by advancing its generation.
    /// Called after a graph mutation commits and before its response returns.
    fn bump(&self, project_id: &ProjectId) {
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = generations.entry(project_id.clone()).or_insert(0);
        *entry = entry.wrapping_add(1);
        // Superseded generations are unreachable; drop their bytes eagerly so
        // the partition budget is available to the live generation.
        self.search.invalidate_project(project_id);
        self.neighbors.invalidate_project(project_id);
    }

    fn record_hit(&self) {
        self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn record_miss(&self) {
        self.misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn health(&self) -> WorldCacheHealth {
        WorldCacheHealth {
            search: self.search.stats(),
            neighbors: self.neighbors.stats(),
            hits: self.hits.load(std::sync::atomic::Ordering::Relaxed),
            misses: self.misses.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// Estimated resident weight of a cached value, from its serialized size.
/// Over the item bound (or unserializable) means the value bypasses the cache.
fn cache_weight<T: Serialize>(value: &T) -> u32 {
    serde_json::to_vec(value).map_or(u32::MAX, |bytes| {
        u32::try_from(bytes.len()).unwrap_or(u32::MAX)
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    Normal,
    Soft,
    Hard,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthResponse {
    pub status: String,
    #[serde(default)]
    pub workspace: String,
    pub version: String,
    pub build_id: String,
    pub database: PathBuf,
    pub uptime_ms: u64,
    pub local_only: bool,
    pub scheduler: SchedulerStats,
    pub read_pool: ReadPoolStats,
    pub rss_bytes: Option<u64>,
    pub rss_soft_limit_bytes: u64,
    pub rss_hard_limit_bytes: u64,
    pub memory_pressure: MemoryPressure,
    /// Absent when derived caching is disabled or the daemon predates it.
    #[serde(default)]
    pub cache: Option<WorldCacheHealth>,
    /// Daemon process id; absent from daemons that predate it.
    #[serde(default)]
    pub pid: Option<u32>,
    /// The process chain that auto-started this daemon, when known. On macOS
    /// the daemon inherits privacy access from it.
    #[serde(default)]
    pub launcher: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectResolveRequest {
    pub path: PathBuf,
}

/// Result of a read-only project lookup: `None` when no registered project
/// owns the path. Registration never happens on this path.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectLookupResponse {
    pub project: Option<ProjectRecord>,
}

/// Request for the graph shape of one file (repo-relative path).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileShapeRequest {
    pub project_id: ProjectId,
    pub file_path: String,
    /// Checkout holding the file. Symbol line spans are reported only when
    /// its copy of the file still has the indexed digest; without a root,
    /// none are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_root: Option<PathBuf>,
    /// The task, usually the session's prompt. Definitions it names come
    /// first, then those matching its terms, then the rest in file order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_text: Option<String>,
    /// The context run compiled for the task. Definitions its task source
    /// map selected come first, with those the task names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_run_id: Option<ContextRunId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileShapeSymbol {
    pub kind: String,
    pub label: String,
    /// 1-based inclusive lines, checked like task source map ranges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<crate::world::LineRange>,
    /// Listed ahead of file order because the task selects, names or
    /// matches it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub task_match: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileShapeCaller {
    pub label: String,
    pub file_path: String,
}

/// The graph's answer to "what is the file about to be touched": the
/// symbols it defines, who calls into it from elsewhere, and what it
/// imports. Powers pre-tool context injection.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FileShape {
    pub symbols: Vec<FileShapeSymbol>,
    pub callers: Vec<FileShapeCaller>,
    pub imports: Vec<String>,
    /// Size of the checkout's copy of the file, or the indexed size when the
    /// checkout cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

/// Request for a project's session brief of episodic state.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TasksBriefRequest {
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TaskBriefItem {
    pub id: String,
    pub prompt: String,
    pub status: String,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

/// Open episodes (handoffs to resurface) plus the most recently completed
/// one, for the session-start standing brief.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TasksBrief {
    pub open: Vec<TaskBriefItem>,
    pub last_completed: Option<TaskBriefItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectStatus {
    #[serde(flatten)]
    pub resolved: ResolvedProject,
    pub database: PathBuf,
    pub database_integrity: String,
    pub project_event_count: i64,
    pub local_only: bool,
    pub structural_index: Option<StructuralIndexStatus>,
    /// Whether the shared index represents this checkout's HEAD and dirty state.
    pub index_matches_checkout: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectIndexRequest {
    pub path: PathBuf,
    pub changed_paths: Option<Vec<PathBuf>>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub graphify_export: Option<PathBuf>,
    /// Reconcile the latest committed scan with current Git HEAD/diff state.
    /// The daemon derives a bounded delta and falls back to a full native scan.
    #[serde(default)]
    pub ensure_fresh: bool,
}

/// Merge one project into another (`engram project merge`), or plan it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectMergeRequest {
    /// The survivor: a project id, a project name, or an absolute checkout path.
    pub into: String,
    /// The project to retire: a project id, name, or absolute checkout path.
    pub from: String,
    /// Only plan: report what would move, collide and be rebuilt.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProjectMergeResponse {
    Planned {
        plan: Box<crate::merge::MergePlan>,
    },
    Merged {
        outcome: Box<crate::merge::MergeOutcome>,
    },
    AlreadyMerged {
        outcome: Box<crate::merge::MergeOutcome>,
    },
}

/// Finish merges into a survivor after its re-index.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MergeFinalizeRequest {
    pub project_id: ProjectId,
    /// Attach waiting memory anchors, or mark their memories for
    /// revalidation. Leave false while the survivor could not be re-indexed.
    #[serde(default)]
    pub resolve_anchors: bool,
    /// The checkout this follow-up re-indexed; recorded so a repeated merge
    /// can skip the re-index.
    #[serde(default)]
    pub reindexed: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectIndexResponse {
    pub resolved: ResolvedProject,
    pub index: IndexReport,
    pub git: Option<GitIngestReport>,
    #[serde(default)]
    pub invalidated_memories: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorldSearchRequest {
    pub project_id: crate::ids::ProjectId,
    pub query: String,
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorldNeighborsRequest {
    pub project_id: ProjectId,
    pub node_id: NodeId,
    pub max_depth: usize,
    pub max_nodes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryCreateRequest {
    pub project_id: crate::ids::ProjectId,
    pub memory: MemoryDraft,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectLinkRequest {
    pub project_id: ProjectId,
    pub linked_project_id: ProjectId,
    pub relation: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectUnlinkRequest {
    pub project_id: ProjectId,
    pub linked_project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectLinksRequest {
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectUnlinkResponse {
    pub removed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemorySummary {
    pub id: String,
    pub kind: String,
    pub claim: String,
    pub confidence: f64,
    pub tags: Vec<String>,
    /// candidate | observed | supported | validated
    #[serde(default)]
    pub epistemic_state: String,
    /// Distinct successful tasks served by a context that injected this
    /// memory (the same count that gates skill promotion).
    #[serde(default)]
    pub wins: i64,
    /// Total times this memory was injected into a compiled context.
    #[serde(default)]
    pub injections: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OriginViewInfo {
    pub project_id: ProjectId,
    pub indexed_head: Option<String>,
    pub indexed_at_ms: Option<i64>,
    pub nodes: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceProject {
    pub id: ProjectId,
    pub name: String,
    pub primary_path: Option<String>,
    pub facts: Vec<crate::projects::ProjectFact>,
    pub links: Vec<LinkedProject>,
    pub validated_memories: i64,
    pub memories: Vec<MemorySummary>,
    pub nodes: i64,
    pub indexed_head: Option<String>,
    pub indexed_at_ms: Option<i64>,
    /// The shared-truth (origin/main) snapshot, when one has been indexed.
    pub origin_view: Option<OriginViewInfo>,
    /// Detected technology stack (manifests + infra files, deterministic).
    #[serde(default)]
    pub technologies: Vec<crate::techstack::Technology>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActivityEvent {
    pub event_type: String,
    pub project_name: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkspaceSnapshot {
    pub projects: Vec<WorkspaceProject>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModuleMapRequest {
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryProvenanceRequest {
    pub memory_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProvenanceEvidence {
    pub kind: String,
    pub strength: String,
    pub command: Option<String>,
    pub exit_code: Option<i64>,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProvenanceEpisode {
    pub task_id: String,
    pub prompt: String,
    pub status: String,
    pub accepted_by_user: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryProvenance {
    pub epistemic_state: String,
    pub confidence: f64,
    pub trust_source: String,
    pub rationale: String,
    pub evidence: Vec<ProvenanceEvidence>,
    pub episodes: Vec<ProvenanceEpisode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OriginRefreshRequest {
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OriginRefreshResponse {
    pub reference: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModuleSymbolsRequest {
    pub project_id: ProjectId,
    /// A module-map group label (directory prefix like `src/auth`).
    pub group: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModuleSymbol {
    pub kind: String,
    pub label: String,
    pub file_path: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CalledFunction {
    pub label: String,
    pub calls: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModuleSymbols {
    pub symbols: Vec<ModuleSymbol>,
    /// Validated memories anchored (via their linked nodes) inside this module.
    pub memories: Vec<MemorySummary>,
    /// Deterministic composition summary ("14 files · 89 functions · 12 types").
    #[serde(default)]
    pub summary: String,
    /// What this module CALLS, ranked by reference count — usage, not
    /// definition.
    #[serde(default)]
    pub calls: Vec<CalledFunction>,
    /// Sibling modules this one depends on (resolved call targets).
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Sibling modules that depend on this one.
    #[serde(default)]
    pub used_by: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FactSetRequest {
    pub project_id: ProjectId,
    pub key: String,
    pub value: String,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FactUnsetRequest {
    pub project_id: ProjectId,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FactListRequest {
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FactUnsetResponse {
    pub removed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LinkScanRequest {
    pub path: PathBuf,
    /// Apply the definitive candidates as links (name matches never apply).
    #[serde(default)]
    pub apply: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct VaultBuildRequest {
    /// Build one project (plus the workspace overview), or every project
    /// when `None`.
    #[serde(default)]
    pub project_id: Option<ProjectId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryPromoteRequest {
    pub target: EpistemicState,
    #[serde(default)]
    pub human_override: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryRestoreDriftRequest {
    pub expected_event_sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryListRequest {
    pub project_id: ProjectId,
    pub kind: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ContextExplainRequest {
    pub run_id: ContextRunId,
    pub ordinal: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextDeliveryRequest {
    pub run_id: ContextRunId,
    pub task_id: TaskId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TaskStartRequest {
    #[serde(default)]
    pub context_request_id: Option<String>,
    pub project_id: ProjectId,
    pub prompt: String,
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AttemptCreateRequest {
    pub strategy_summary: String,
    pub outcome: AttemptOutcome,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvidenceCreateRequest {
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub attempt_id: Option<AttemptId>,
    pub kind: String,
    pub strength: EvidenceStrength,
    pub commit_hash: Option<String>,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub artifact_text: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// Evidence a native hook observed. Only this request can attach hook
/// provenance; the generic evidence endpoint strips it from client metadata.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HookEvidenceRequest {
    pub evidence: EvidenceCreateRequest,
    pub provenance: crate::agenthooks::HookProvenance,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TaskFinishRequest {
    pub outcome: TaskOutcome,
    pub accepted_by_user: Option<bool>,
    pub final_commit: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PastFailuresRequest {
    pub project_id: ProjectId,
    pub query: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackupRequest {
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackupVerifyRequest {
    pub path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectExportRequest {
    pub project_id: ProjectId,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SemanticRebuildRequest {
    pub project_id: ProjectId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SynthesisRunRequest {
    /// Project path; resolved and scoped like `projects/index`, so a project's
    /// own `.engram/config.toml` `[synthesis]` section governs its thresholds.
    pub path: PathBuf,
}

/// Promote a validated procedure memory to the `skill` kind after enough
/// recorded distinct successful uses (spec 16.4). Thresholds come from the
/// project-scoped `[synthesis]` configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SkillPromoteRequest {
    pub path: PathBuf,
    pub memory_id: MemoryId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SemanticRebuildResponse {
    pub project_id: ProjectId,
    /// Number of derived vectors rewritten from canonical memories.
    pub vectors: u64,
    /// Model vectors, when `semantic.provider = "local"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRebuildReport>,
}

/// Process-lifetime observability snapshot served by `GET /api/v1/metrics`.
/// Counters and histograms reset when the daemon restarts; durable history
/// lives in `context_runs` and the event log.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MetricsResponse {
    pub uptime_ms: u64,
    pub metrics: MetricsSnapshot,
    /// Bridged from the live world-read cache; absent when caching is off.
    #[serde(default)]
    pub cache: Option<WorldCacheHealth>,
}

/// Daemon-wide canonical memory counts served by `GET /api/v1/metrics/memories`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryStatsResponse {
    pub total: u64,
    pub by_status: std::collections::BTreeMap<String, u64>,
    pub by_epistemic_state: std::collections::BTreeMap<String, u64>,
}

/// Enqueue one allowlisted background job. Unknown types are rejected during
/// deserialization, so the allowlist is enforced at the API boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JobEnqueueRequest {
    pub job_type: JobType,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
    /// Enqueue is idempotent by `(job_type, dedupe_key)`; omitting the key
    /// makes every request a distinct job.
    #[serde(default)]
    pub dedupe_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JobListRequest {
    #[serde(default)]
    pub status: Option<JobStatus>,
    pub limit: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiErrorEnvelope {
    error: ApiErrorBody,
}

#[derive(Debug, Deserialize, Serialize)]
struct ApiErrorBody {
    code: String,
    message: String,
    retryable: bool,
}

struct ApiError {
    status: StatusCode,
    body: ApiErrorBody,
}

impl ApiError {
    fn from_core(error: &Error) -> Self {
        let (status, code, retryable) = match error {
            Error::Backpressure => (StatusCode::TOO_MANY_REQUESTS, "overloaded", true),
            Error::ResourcePressure { .. } => {
                (StatusCode::SERVICE_UNAVAILABLE, "memory_pressure", true)
            }
            Error::StorageReadTimeout => {
                (StatusCode::SERVICE_UNAVAILABLE, "read_pool_timeout", true)
            }
            Error::SchedulerStopped | Error::SchedulerResponseClosed => (
                StatusCode::SERVICE_UNAVAILABLE,
                "scheduler_unavailable",
                true,
            ),
            Error::AccessDenied { .. } => (
                StatusCode::FORBIDDEN,
                crate::error::ACCESS_DENIED_CODE,
                false,
            ),
            Error::MergeRefused(_) => (StatusCode::CONFLICT, "merge_refused", false),
            Error::InvalidProjectId(_)
            | Error::InvalidDomainId { .. }
            | Error::InvalidProjectMetadata { .. }
            | Error::Config(_)
            | Error::Git(_)
            | Error::Structural(_)
            | Error::Io { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", false),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, "internal", false),
        };
        Self {
            status,
            body: ApiErrorBody {
                code: code.to_owned(),
                message: error.to_string(),
                retryable,
            },
        }
    }

    fn timeout() -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            body: ApiErrorBody {
                code: "request_timeout".to_owned(),
                message: "daemon request exceeded its configured deadline".to_owned(),
                retryable: true,
            },
        }
    }

    fn draining() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: ApiErrorBody {
                code: crate::lifecycle::DRAINING_CODE.to_owned(),
                message: format!(
                    "engramd pid {} is finishing in-flight work before it exits",
                    std::process::id()
                ),
                retryable: true,
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(ApiErrorEnvelope { error: self.body })).into_response()
    }
}

/// A client for the versioned local daemon API.
#[derive(Clone, Debug)]
pub struct DaemonClient {
    socket_path: PathBuf,
    timeout: Duration,
}

impl DaemonClient {
    /// Read durable usage, delivery coverage and knowledge health.
    /// # Errors
    /// Returns transport, validation or storage errors.
    pub async fn effectiveness(
        &self,
        request: &crate::effectiveness::EffectivenessRequest,
    ) -> Result<crate::effectiveness::EffectivenessReport> {
        self.request(hyper::Method::POST, API_EFFECTIVENESS, Some(request))
            .await
    }

    /// Record one idempotent provider usage or hook delivery sample.
    /// # Errors
    /// Returns scope, duplicate-content or transport errors.
    pub async fn record_usage(
        &self,
        request: &crate::effectiveness::UsageRecordRequest,
    ) -> Result<crate::effectiveness::UsageRecordResult> {
        self.request(hyper::Method::POST, API_USAGE_RECORD, Some(request))
            .await
    }

    /// Queue one hook observation of an agent tool call for a merged write.
    /// Returns once the daemon has validated, timestamped and queued it.
    /// # Errors
    /// Returns validation, backpressure or transport errors.
    pub async fn record_tool_observation(
        &self,
        request: &crate::effectiveness::ToolObservationRequest,
    ) -> Result<crate::effectiveness::ToolObservationReceipt> {
        self.request(hyper::Method::POST, API_TOOL_OBSERVATION, Some(request))
            .await
    }

    /// Queue one hook-observed verification run for the effectiveness report.
    /// Returns once the daemon has validated, timestamped and queued it.
    /// # Errors
    /// Returns validation, backpressure or transport errors.
    pub async fn record_verification_observation(
        &self,
        request: &crate::effectiveness::VerificationObservationRequest,
    ) -> Result<crate::effectiveness::VerificationObservationReceipt> {
        self.request(
            hyper::Method::POST,
            API_VERIFICATION_OBSERVATION,
            Some(request),
        )
        .await
    }

    /// Inspect automatic review configuration and recent decisions.
    /// # Errors
    /// Returns transport or storage errors.
    pub async fn judge_status(&self) -> Result<serde_json::Value> {
        self.request::<(), _>(hyper::Method::GET, API_JUDGE_STATUS, None)
            .await
    }

    /// Queue a bounded review pass. Model calls happen outside the writer.
    /// # Errors
    /// Returns disabled-runner or transport errors.
    pub async fn judge_run(&self) -> Result<serde_json::Value> {
        self.request(
            hyper::Method::POST,
            API_JUDGE_RUN,
            Some(&serde_json::json!({})),
        )
        .await
    }

    /// Plan the re-verification of pending reviews. Reads only.
    /// # Errors
    /// Returns an invalid request, storage or transport errors.
    pub async fn reverify_plan(
        &self,
        request: &crate::reverify::PlanRequest,
    ) -> Result<crate::reverify::Plan> {
        self.request(hyper::Method::POST, API_REVERIFY_PLAN, Some(request))
            .await
    }

    /// Attach recorded re-verification evidence to pending reviews and queue
    /// a judge pass when the judge is enabled.
    /// # Errors
    /// Returns invalid attachments, storage or transport errors.
    pub async fn reverify_attach(
        &self,
        request: &crate::reverify::AttachRequest,
    ) -> Result<crate::reverify::AttachResult> {
        self.request(hyper::Method::POST, API_REVERIFY_ATTACH, Some(request))
            .await
    }

    /// Find, and with `apply` expire, pending reviews that only new
    /// verification work can unblock.
    /// # Errors
    /// Returns an invalid request, storage or transport errors.
    pub async fn review_expire(
        &self,
        request: &crate::expiry::ExpiryRequest,
    ) -> Result<crate::expiry::ExpiryReport> {
        self.request(hyper::Method::POST, API_REVIEW_EXPIRE, Some(request))
            .await
    }
    #[must_use]
    pub fn new(socket_path: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout,
        }
    }

    /// Query daemon liveness and bounded-resource utilization.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket is unavailable, the deadline expires,
    /// or the daemon returns an incompatible response.
    pub async fn health(&self) -> Result<HealthResponse> {
        self.request::<(), _>(hyper::Method::GET, API_HEALTH, None)
            .await
    }

    /// Fetch the daemon's process-lifetime metrics snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket is unavailable, the deadline expires,
    /// or the daemon returns an incompatible response.
    pub async fn metrics(&self) -> Result<MetricsResponse> {
        self.request::<(), _>(hyper::Method::GET, API_METRICS, None)
            .await
    }

    /// Fetch daemon-wide canonical memory counts by status and state.
    ///
    /// # Errors
    ///
    /// Returns an error for transport, timeout, or storage-read failures.
    pub async fn memory_stats(&self) -> Result<MemoryStatsResponse> {
        self.request::<(), _>(hyper::Method::GET, API_MEMORY_STATS, None)
            .await
    }

    /// Resolve or register a project through the canonical mutation scheduler.
    ///
    /// # Errors
    ///
    /// Returns transport, overload, validation, or canonical storage errors.
    pub async fn resolve_project(&self, path: &Path) -> Result<ProjectStatus> {
        self.request(
            hyper::Method::POST,
            API_RESOLVE_PROJECT,
            Some(&ProjectResolveRequest {
                path: path.to_path_buf(),
            }),
        )
        .await
    }

    /// Graph shape of one file: defined symbols, external callers, imports.
    ///
    /// # Errors
    ///
    /// Returns transport or storage-read errors.
    pub async fn file_shape(&self, request: &FileShapeRequest) -> Result<FileShape> {
        self.request(hyper::Method::POST, API_FILE_SHAPE, Some(request))
            .await
    }

    /// Open episodes + last completed one, for the session-start brief.
    ///
    /// # Errors
    ///
    /// Returns transport or storage-read errors.
    pub async fn tasks_brief(&self, project_id: &ProjectId) -> Result<TasksBrief> {
        self.request(
            hyper::Method::POST,
            API_TASKS_BRIEF,
            Some(&TasksBriefRequest {
                project_id: project_id.clone(),
            }),
        )
        .await
    }

    /// Read-only lookup: which registered project owns `path`, or `None`.
    /// Never registers — safe for universal (user-level) hooks.
    ///
    /// # Errors
    ///
    /// Returns transport or storage-read errors.
    pub async fn lookup_project(&self, path: &Path) -> Result<ProjectLookupResponse> {
        self.request(
            hyper::Method::POST,
            API_LOOKUP_PROJECT,
            Some(&ProjectResolveRequest {
                path: path.to_path_buf(),
            }),
        )
        .await
    }

    /// Run diagnostics inside the daemon without opening a second writer.
    ///
    /// # Errors
    ///
    /// Returns an error for transport, timeout, or response failures.
    pub async fn doctor(&self) -> Result<DoctorReport> {
        self.request::<(), _>(hyper::Method::GET, API_DOCTOR, None)
            .await
    }

    /// Request a graceful shutdown from the same local user.
    ///
    /// # Errors
    ///
    /// Returns an error for transport, timeout, or response failures.
    pub async fn shutdown(&self) -> Result<()> {
        self.request::<(), _>(hyper::Method::POST, API_SHUTDOWN, None)
            .await
    }

    /// Build or incrementally refresh a project's structural world model.
    ///
    /// # Errors
    ///
    /// Returns transport, scanning, overload, or canonical storage errors.
    pub async fn index_project(
        &self,
        path: &Path,
        changed_paths: Option<Vec<PathBuf>>,
    ) -> Result<ProjectIndexResponse> {
        self.index_project_with_provider(path, changed_paths, None, None)
            .await
    }

    /// Refresh structural and Git facts with an explicit provider override.
    ///
    /// # Errors
    ///
    /// Returns transport, provider validation, overload, or storage errors.
    pub async fn index_project_with_provider(
        &self,
        path: &Path,
        changed_paths: Option<Vec<PathBuf>>,
        provider: Option<String>,
        graphify_export: Option<PathBuf>,
    ) -> Result<ProjectIndexResponse> {
        self.request(
            hyper::Method::POST,
            API_INDEX_PROJECT,
            Some(&ProjectIndexRequest {
                path: path.to_path_buf(),
                changed_paths,
                provider,
                graphify_export,
                ensure_fresh: false,
            }),
        )
        .await
    }

    /// Reconcile structural state with the repository's current committed and
    /// working-tree state before correctness-critical context compilation.
    ///
    /// # Errors
    ///
    /// Returns transport, bounded Git, parsing, or storage errors.
    pub async fn ensure_project_fresh(
        &self,
        path: &Path,
        current_paths: Vec<PathBuf>,
    ) -> Result<ProjectIndexResponse> {
        self.request(
            hyper::Method::POST,
            API_INDEX_PROJECT,
            Some(&ProjectIndexRequest {
                path: path.to_path_buf(),
                changed_paths: Some(current_paths),
                provider: Some("tree_sitter".to_owned()),
                graphify_export: None,
                ensure_fresh: true,
            }),
        )
        .await
    }

    /// Search normalized active graph nodes.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, or storage errors.
    pub async fn search_world(&self, request: &WorldSearchRequest) -> Result<Vec<GraphNode>> {
        self.request(hyper::Method::POST, API_WORLD_SEARCH, Some(request))
            .await
    }

    /// Traverse a bounded canonical graph neighborhood.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, or storage errors.
    pub async fn world_neighbors(&self, request: &WorldNeighborsRequest) -> Result<GraphSlice> {
        self.request(hyper::Method::POST, API_WORLD_NEIGHBORS, Some(request))
            .await
    }

    /// Compile bounded context through the canonical daemon.
    ///
    /// # Errors
    ///
    /// Returns transport, retrieval, validity, overload, or storage errors.
    pub async fn compile_context(
        &self,
        request: &ContextCompileRequest,
    ) -> Result<CompiledContext> {
        self.request(hyper::Method::POST, API_CONTEXT_COMPILE, Some(request))
            .await
    }

    /// Acknowledge successful context delivery to a task, returning whether a
    /// new receipt was inserted. Repeated acknowledgements return false.
    ///
    /// # Errors
    /// Returns transport, scope, context-mode, overload, or storage errors.
    pub async fn context_delivered(&self, request: &ContextDeliveryRequest) -> Result<bool> {
        self.request(hyper::Method::POST, API_CONTEXT_DELIVERED, Some(request))
            .await
    }

    /// Explain one durable selected context item.
    ///
    /// # Errors
    ///
    /// Returns transport, typed-ID, or storage errors.
    pub async fn explain_context(
        &self,
        request: &ContextExplainRequest,
    ) -> Result<Option<ContextItem>> {
        self.request(hyper::Method::POST, API_CONTEXT_EXPLAIN, Some(request))
            .await
    }

    /// Create a candidate memory.
    ///
    /// # Errors
    ///
    /// Returns transport, evidence-scope, overload, or storage errors.
    pub async fn create_memory(&self, request: &MemoryCreateRequest) -> Result<MemoryRecord> {
        self.request(hyper::Method::POST, API_MEMORY_CREATE, Some(request))
            .await
    }

    /// List bounded project memories for explicit inspection.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, or storage errors.
    pub async fn list_memories(&self, request: &MemoryListRequest) -> Result<Vec<MemoryRecord>> {
        self.request(hyper::Method::POST, API_MEMORY_SEARCH, Some(request))
            .await
    }

    /// Fetch a memory by typed ID.
    ///
    /// # Errors
    ///
    /// Returns transport, ID, or storage errors.
    pub async fn get_memory(&self, id: &MemoryId) -> Result<Option<MemoryRecord>> {
        self.request::<(), _>(
            hyper::Method::GET,
            &format!("{API_MEMORY_CREATE}/{id}"),
            None,
        )
        .await
    }

    /// Promote a memory through its evidence-gated state machine.
    ///
    /// # Errors
    ///
    /// Returns transport, transition, evidence, overload, or storage errors.
    pub async fn promote_memory(
        &self,
        id: &MemoryId,
        request: &MemoryPromoteRequest,
    ) -> Result<MemoryRecord> {
        self.request(
            hyper::Method::POST,
            &format!("{API_MEMORY_CREATE}/{id}/promote"),
            Some(request),
        )
        .await
    }

    /// Restore only an identified drift quarantine whose source digests match again.
    /// # Errors
    /// Returns stale event, source mismatch, transport, or storage errors.
    pub async fn restore_memory_drift(
        &self,
        id: &MemoryId,
        request: &MemoryRestoreDriftRequest,
    ) -> Result<MemoryRecord> {
        self.request(
            hyper::Method::POST,
            &format!("{API_MEMORY_CREATE}/{id}/restore-drift"),
            Some(request),
        )
        .await
    }

    /// Logically forget a memory while preserving its auditable canonical record.
    ///
    /// # Errors
    ///
    /// Returns transport, ID, overload, or storage errors.
    pub async fn forget_memory(&self, id: &MemoryId) -> Result<MemoryRecord> {
        self.request::<(), _>(
            hyper::Method::POST,
            &format!("{API_MEMORY_CREATE}/{id}/forget"),
            None,
        )
        .await
    }

    /// Purge one memory and its derived retrieval material.
    ///
    /// # Errors
    ///
    /// Returns transport, overload, ID, or storage errors.
    pub async fn purge_memory(&self, id: &MemoryId) -> Result<()> {
        self.request::<(), _>(
            hyper::Method::DELETE,
            &format!("{API_MEMORY_CREATE}/{id}"),
            None,
        )
        .await
    }

    /// Start one task episode with an explicit project scope.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, overload, or storage errors.
    pub async fn purge_task_prompt(&self, task_id: &TaskId) -> Result<()> {
        self.request::<serde_json::Value, serde_json::Value>(
            hyper::Method::DELETE,
            &format!("{API_TASKS}/{task_id}/prompt"),
            None,
        )
        .await?;
        Ok(())
    }

    /// Start an episode after redacting its prompt.
    ///
    /// # Errors
    /// Returns transport or storage errors.
    pub async fn start_task(&self, request: &TaskStartRequest) -> Result<TaskRecord> {
        self.request(hyper::Method::POST, API_TASKS, Some(request))
            .await
    }

    /// Append an attempt to an open task episode.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, overload, or storage errors.
    pub async fn record_attempt(
        &self,
        task_id: &TaskId,
        request: &AttemptCreateRequest,
    ) -> Result<AttemptRecord> {
        self.request(
            hyper::Method::POST,
            &format!("{API_TASKS}/{task_id}/attempts"),
            Some(request),
        )
        .await
    }

    /// Record provenance-bearing evidence. Artifact text is redacted before it
    /// is persisted, and large artifacts move to the content-addressed blob
    /// store behind a digest reference.
    ///
    /// # Errors
    ///
    /// Returns transport, scope-validation, overload, or storage errors.
    pub async fn record_evidence(&self, request: &EvidenceCreateRequest) -> Result<EvidenceRecord> {
        self.request(hyper::Method::POST, API_EVIDENCE, Some(request))
            .await
    }

    /// Record evidence a native hook observed, with its hook provenance.
    ///
    /// # Errors
    ///
    /// Returns transport, scope, validation, or storage errors.
    pub async fn record_hook_evidence(
        &self,
        request: &HookEvidenceRequest,
    ) -> Result<EvidenceRecord> {
        self.request(hyper::Method::POST, API_HOOK_EVIDENCE, Some(request))
            .await
    }

    /// Fetch one evidence record with its artifact text fully resolved.
    ///
    /// # Errors
    ///
    /// Returns transport, ID, blob-resolution, or storage errors.
    pub async fn get_evidence(&self, id: &EvidenceId) -> Result<Option<EvidenceRecord>> {
        self.request::<(), _>(hyper::Method::GET, &format!("{API_EVIDENCE}/{id}"), None)
            .await
    }

    /// Close an open task with an explicit observed outcome.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, overload, or storage errors.
    pub async fn finish_task(
        &self,
        task_id: &TaskId,
        request: &TaskFinishRequest,
    ) -> Result<TaskRecord> {
        self.request(
            hyper::Method::POST,
            &format!("{API_TASKS}/{task_id}/finish"),
            Some(request),
        )
        .await
    }

    /// Query bounded demonstrated failed attempts.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, or storage errors.
    pub async fn past_failures(&self, request: &PastFailuresRequest) -> Result<Vec<PastFailure>> {
        self.request(hyper::Method::POST, API_PAST_FAILURES, Some(request))
            .await
    }

    /// Persist one compact paired-evaluation result.
    ///
    /// # Errors
    ///
    /// Returns transport, overload, validation, or storage errors.
    pub async fn record_evaluation(&self, record: &EvaluationRecord) -> Result<()> {
        self.request(hyper::Method::POST, API_EVALUATION_RUNS, Some(record))
            .await
    }

    /// Distill an evidence-linked successful episode into candidate memory.
    ///
    /// # Errors
    ///
    /// Returns transport, evidence, scope, overload, or storage errors.
    pub async fn distill(&self, request: &DistillationRequest) -> Result<DistillationResult> {
        self.request(hyper::Method::POST, API_LEARNING_DISTILL, Some(request))
            .await
    }

    /// Create a transactionally consistent canonical backup.
    ///
    /// # Errors
    ///
    /// Returns transport, destination, overload, or storage errors.
    pub async fn create_backup(&self, destination: &Path) -> Result<()> {
        self.request(
            hyper::Method::POST,
            API_BACKUP_CREATE,
            Some(&BackupRequest {
                destination: destination.to_path_buf(),
            }),
        )
        .await
    }

    /// Plan or execute a project merge. Execution takes a backup first and
    /// waits for the merge transaction, so there is no client deadline.
    ///
    /// # Errors
    ///
    /// Returns `merge_refused` errors with the reason, or transport errors.
    pub async fn merge_projects(
        &self,
        request: &ProjectMergeRequest,
    ) -> Result<ProjectMergeResponse> {
        self.request(hyper::Method::POST, API_PROJECT_MERGE, Some(request))
            .await
    }

    /// Finish merges into a survivor after its re-index.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn finalize_merge(
        &self,
        request: &MergeFinalizeRequest,
    ) -> Result<crate::merge::FinalizeOutcome> {
        self.request(
            hyper::Method::POST,
            API_PROJECT_MERGE_FINALIZE,
            Some(request),
        )
        .await
    }

    /// Verify a backup using normal schema and integrity checks.
    ///
    /// # Errors
    ///
    /// Returns transport, path, schema, or integrity-query errors.
    pub async fn verify_backup(&self, path: &Path) -> Result<IntegrityReport> {
        self.request(
            hyper::Method::POST,
            API_BACKUP_VERIFY,
            Some(&BackupVerifyRequest {
                path: path.to_path_buf(),
            }),
        )
        .await
    }

    /// Write a bounded-memory NDJSON project export.
    ///
    /// # Errors
    ///
    /// Returns transport, destination, typed-scope, or storage errors.
    pub async fn export_project(&self, request: &ProjectExportRequest) -> Result<ExportReport> {
        self.request(hyper::Method::POST, API_PROJECT_EXPORT, Some(request))
            .await
    }

    /// Run one bounded episode-to-procedure synthesis pass for a project.
    ///
    /// # Errors
    ///
    /// Returns transport, overload, validation, or canonical storage errors.
    pub async fn run_synthesis(&self, request: &SynthesisRunRequest) -> Result<SynthesisReport> {
        self.request(hyper::Method::POST, API_SYNTHESIS_RUN, Some(request))
            .await
    }

    /// Promote a validated, sufficiently-used procedure memory to a skill.
    ///
    /// # Errors
    ///
    /// Returns transport, gating, overload, or canonical storage errors.
    pub async fn promote_skill(&self, request: &SkillPromoteRequest) -> Result<MemoryRecord> {
        self.request(hyper::Method::POST, API_SKILL_PROMOTE, Some(request))
            .await
    }

    /// Create or update a directed cross-repository knowledge link.
    ///
    /// # Errors
    ///
    /// Returns an error for transport failures or invalid link requests.
    pub async fn link_project(&self, request: &ProjectLinkRequest) -> Result<LinkedProject> {
        self.request(hyper::Method::POST, API_PROJECT_LINK, Some(request))
            .await
    }

    /// Remove a directed cross-repository knowledge link.
    ///
    /// # Errors
    ///
    /// Returns an error for transport or storage failures.
    pub async fn unlink_project(
        &self,
        request: &ProjectUnlinkRequest,
    ) -> Result<ProjectUnlinkResponse> {
        self.request(hyper::Method::POST, API_PROJECT_UNLINK, Some(request))
            .await
    }

    /// Refresh a project's structural index against current Git state.
    ///
    /// # Errors
    ///
    /// Returns transport, scanning, overload, or canonical storage errors.
    pub async fn refresh_index(&self, path: &Path) -> Result<ProjectIndexResponse> {
        self.request(
            hyper::Method::POST,
            API_INDEX_PROJECT,
            Some(&ProjectIndexRequest {
                path: path.to_path_buf(),
                changed_paths: None,
                provider: None,
                graphify_export: None,
                ensure_fresh: true,
            }),
        )
        .await
    }

    /// Fetch a memory's evidence chain and source episodes.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn memory_provenance(
        &self,
        request: &MemoryProvenanceRequest,
    ) -> Result<MemoryProvenance> {
        self.request(hyper::Method::POST, API_MEMORY_PROVENANCE, Some(request))
            .await
    }

    /// Refresh a project's shared-truth (origin/main) view.
    ///
    /// # Errors
    ///
    /// Returns git, transport, or storage errors.
    pub async fn origin_refresh(
        &self,
        request: &OriginRefreshRequest,
    ) -> Result<OriginRefreshResponse> {
        self.request(hyper::Method::POST, API_ORIGIN_REFRESH, Some(request))
            .await
    }

    /// List a module group's defined symbols and the memories anchored there.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn module_symbols(&self, request: &ModuleSymbolsRequest) -> Result<ModuleSymbols> {
        self.request(hyper::Method::POST, API_MODULE_SYMBOLS, Some(request))
            .await
    }

    /// Compute a project's module map (expensive; seconds on large repos).
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn module_map(&self, request: &ModuleMapRequest) -> Result<crate::vault::ModuleMap> {
        self.request(hyper::Method::POST, API_MODULE_MAP, Some(request))
            .await
    }

    /// Fetch the newest workspace events for activity feeds.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn workspace_activity(&self) -> Result<Vec<ActivityEvent>> {
        self.request::<(), _>(hyper::Method::GET, API_ACTIVITY, None)
            .await
    }

    /// Export this instance's promoted knowledge for down-sync.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn sync_export(&self) -> Result<crate::sync::SyncKnowledge> {
        self.request::<(), _>(hyper::Method::GET, API_SYNC_EXPORT, None)
            .await
    }

    /// Collect local validated memories to offer upstream.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn sync_offers(&self) -> Result<crate::sync::SyncKnowledge> {
        self.request::<(), _>(hyper::Method::GET, API_SYNC_OFFERS, None)
            .await
    }

    /// Apply downloaded central knowledge to this instance.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn sync_apply(
        &self,
        knowledge: &crate::sync::SyncKnowledge,
    ) -> Result<crate::sync::SyncApplyReport> {
        self.request(hyper::Method::POST, API_SYNC_APPLY, Some(knowledge))
            .await
    }

    /// Accept up-sync offers (central side): unknown memories become
    /// candidates for curation.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn sync_accept(
        &self,
        offers: &crate::sync::SyncKnowledge,
    ) -> Result<crate::sync::OfferReport> {
        self.request(hyper::Method::POST, API_SYNC_ACCEPT, Some(offers))
            .await
    }

    /// Fetch the whole-workspace snapshot for UIs.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn workspace(&self) -> Result<WorkspaceSnapshot> {
        self.request::<(), _>(hyper::Method::GET, API_WORKSPACE, None)
            .await
    }

    /// Complete an episode or review a lesson with explicit evidence gates.
    /// # Errors
    /// Returns validation, storage or transport errors.
    pub async fn lesson_mutate(
        &self,
        request: &crate::product::lessons::LessonMutation,
    ) -> Result<serde_json::Value> {
        self.request(
            hyper::Method::POST,
            "/api/v1/product/lessons",
            Some(request),
        )
        .await
    }

    /// Query the product catalog, documentation, task context or change impact.
    /// # Errors
    /// Returns validation, storage or transport errors.
    pub async fn product_query(
        &self,
        request: &crate::product::Query,
    ) -> Result<serde_json::Value> {
        self.request(hyper::Method::POST, "/api/v1/product/query", Some(request))
            .await
    }

    /// Apply a versioned catalog mutation through the canonical scheduler.
    /// # Errors
    /// Returns conflict, validation or transport errors.
    pub async fn product_mutate(
        &self,
        request: &crate::product::Mutation,
    ) -> Result<crate::product::Record> {
        self.request(hyper::Method::POST, "/api/v1/product/mutate", Some(request))
            .await
    }

    /// Fetch the deterministic workspace architecture dossier.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn architect_dossier(&self) -> Result<String> {
        self.request::<(), _>(hyper::Method::GET, API_ARCHITECT_DOSSIER, None)
            .await
    }

    /// Set one curated architecture fact on a project.
    ///
    /// # Errors
    ///
    /// Returns transport or validation errors.
    pub async fn set_fact(&self, request: &FactSetRequest) -> Result<crate::projects::ProjectFact> {
        self.request(hyper::Method::POST, API_FACT_SET, Some(request))
            .await
    }

    /// Remove one curated architecture fact.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn unset_fact(&self, request: &FactUnsetRequest) -> Result<FactUnsetResponse> {
        self.request(hyper::Method::POST, API_FACT_UNSET, Some(request))
            .await
    }

    /// List a project's curated architecture facts.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn list_facts(
        &self,
        request: &FactListRequest,
    ) -> Result<Vec<crate::projects::ProjectFact>> {
        self.request(hyper::Method::POST, API_FACT_LIST, Some(request))
            .await
    }

    /// Scan a repository's manifests for links to other registered projects.
    ///
    /// # Errors
    ///
    /// Returns transport or storage errors.
    pub async fn scan_links(
        &self,
        request: &LinkScanRequest,
    ) -> Result<crate::linkscan::LinkScanReport> {
        self.request(hyper::Method::POST, API_LINK_SCAN, Some(request))
            .await
    }

    /// Export the derived Obsidian vault (requires `vault.enabled`).
    ///
    /// # Errors
    ///
    /// Returns transport errors or a config error when the vault is disabled.
    pub async fn build_vault(
        &self,
        request: &VaultBuildRequest,
    ) -> Result<crate::vault::VaultReport> {
        self.request(hyper::Method::POST, API_VAULT_BUILD, Some(request))
            .await
    }

    /// List a project's outgoing knowledge links.
    ///
    /// # Errors
    ///
    /// Returns an error for transport or storage failures.
    pub async fn project_links(&self, request: &ProjectLinksRequest) -> Result<Vec<LinkedProject>> {
        self.request(hyper::Method::POST, API_PROJECT_LINKS, Some(request))
            .await
    }

    /// Rebuild the derived semantic vector sidecar from canonical memories.
    ///
    /// # Errors
    ///
    /// Returns transport, overload, or canonical storage errors.
    pub async fn rebuild_semantic_index(
        &self,
        request: &SemanticRebuildRequest,
    ) -> Result<SemanticRebuildResponse> {
        self.request(hyper::Method::POST, API_SEMANTIC_REBUILD, Some(request))
            .await
    }

    /// Idempotently enqueue one allowlisted background job.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, overload, or storage errors.
    pub async fn enqueue_job(&self, request: &JobEnqueueRequest) -> Result<JobRecord> {
        self.request(hyper::Method::POST, API_JOBS, Some(request))
            .await
    }

    /// List newest background jobs, optionally filtered by status.
    ///
    /// # Errors
    ///
    /// Returns transport, validation, or storage errors.
    pub async fn list_jobs(&self, request: &JobListRequest) -> Result<Vec<JobRecord>> {
        self.request(hyper::Method::POST, API_JOBS_LIST, Some(request))
            .await
    }

    /// Fetch one background job by typed ID.
    ///
    /// # Errors
    ///
    /// Returns transport, ID, or storage errors.
    pub async fn get_job(&self, id: &JobId) -> Result<Option<JobRecord>> {
        self.request::<(), _>(hyper::Method::GET, &format!("{API_JOBS}/{id}"), None)
            .await
    }

    async fn request<B, R>(&self, method: hyper::Method, path: &str, body: Option<&B>) -> Result<R>
    where
        B: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let terminal_ack = requires_terminal_ack(&method, path);
        let socket_path = self.socket_path.clone();
        let request = async move {
            let stream = UnixStream::connect(&socket_path)
                .await
                .map_err(|source| io_error(&socket_path, source))?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .map_err(|error| Error::Daemon(format!("HTTP handshake failed: {error}")))?;
            tokio::spawn(async move {
                let _ = connection.await;
            });

            let encoded = body.map_or_else(
                || Ok(Vec::new()),
                |value| serde_json::to_vec(value).map_err(Error::from),
            )?;
            let request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost")
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(encoded)))
                .map_err(|error| Error::Daemon(format!("invalid HTTP request: {error}")))?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|error| Error::Daemon(format!("daemon request failed: {error}")))?;
            let status = response.status();
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|error| Error::Daemon(format!("daemon response failed: {error}")))?
                .to_bytes();
            if !status.is_success() {
                let envelope =
                    serde_json::from_slice::<ApiErrorEnvelope>(&bytes).map_err(|_| {
                        Error::Daemon(format!(
                            "daemon returned HTTP {status} without a valid error"
                        ))
                    })?;
                return Err(Error::Daemon(format!(
                    "{}: {}",
                    envelope.error.code, envelope.error.message
                )));
            }
            serde_json::from_slice(&bytes).map_err(Error::from)
        };

        if terminal_ack {
            request.await
        } else {
            tokio::time::timeout(self.timeout, request)
                .await
                .map_err(|_| Error::Daemon("daemon request timed out".to_owned()))?
        }
    }
}

fn requires_terminal_ack(method: &hyper::Method, path: &str) -> bool {
    if *method == hyper::Method::GET {
        return false;
    }
    path == API_RESOLVE_PROJECT
        || path == API_INDEX_PROJECT
        || path == API_PROJECT_LINK
        || path == API_PROJECT_UNLINK
        || path == API_FACT_SET
        || path == API_FACT_UNSET
        || path == API_MEMORY_CREATE
        || path.starts_with(&format!("{API_MEMORY_CREATE}/"))
        || path == API_TASKS
        || path.starts_with(&format!("{API_TASKS}/"))
        || path == API_EVIDENCE
        || path == API_EVALUATION_RUNS
        || path == API_LEARNING_DISTILL
        || path == API_BACKUP_CREATE
        || path == API_PROJECT_EXPORT
        || path == API_SEMANTIC_REBUILD
        || path == API_JOBS
        || path == API_SYNTHESIS_RUN
        || path == API_SKILL_PROMOTE
        || path == API_PROJECT_MERGE
        || path == API_PROJECT_MERGE_FINALIZE
        || path == API_REVERIFY_PLAN
        || path == API_REVERIFY_ATTACH
        || path == API_REVIEW_EXPIRE
}

/// Longest a graceful shutdown waits for in-flight requests, the background
/// job and a judge call before it closes the listener anyway.
const DRAIN_DEADLINE: Duration = Duration::from_secs(120);

/// Longest a graceful shutdown waits for already-queued writes (some
/// endpoints acknowledge a write once it is queued).
const WRITE_DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// Serve the local API until `shutdown` resolves, holding the database's
/// daemon lock (see [`crate::lifecycle`]) for the duration.
///
/// # Errors
///
/// Returns an error when another daemon holds the database, when canonical
/// storage or the Unix socket cannot be safely initialized, or when the HTTP
/// server exits abnormally.
pub async fn serve_until<F>(config: Config, external_shutdown: F) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let database = config.storage.db_path.clone();
    let lock = tokio::task::spawn_blocking(move || {
        crate::lifecycle::DaemonLock::acquire(&database, crate::lifecycle::start_wait())
    })
    .await
    .map_err(|error| Error::Daemon(format!("daemon lock task failed: {error}")))??;
    serve_with_lock(config, &lock, external_shutdown).await
}

/// Serve the local API until `shutdown` resolves. The caller holds `lock`
/// for this database, which `engramd` keeps until the process exits.
///
/// # Errors
///
/// Returns an error when `lock` guards another database, when a daemon from
/// a build without the lock still has the database open, when canonical
/// storage or the Unix socket cannot be safely initialized, or when the HTTP
/// server exits abnormally.
#[allow(clippy::too_many_lines)]
pub async fn serve_with_lock<F>(
    config: Config,
    lock: &crate::lifecycle::DaemonLock,
    external_shutdown: F,
) -> Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    if lock.database() != config.storage.db_path {
        return Err(Error::Daemon(format!(
            "daemon lock {} does not guard {}",
            lock.path().display(),
            config.storage.db_path.display()
        )));
    }
    // A daemon built before the lock closes its listener at once on shutdown
    // but keeps the database open while it drains, and can still hold it for
    // a moment after removing its socket file. Unless a daemon answers on the
    // socket (binding below refuses that case), wait for such a process to
    // let go of the database.
    if std::os::unix::net::UnixStream::connect(&config.daemon.socket_path).is_err() {
        let database = config.storage.db_path.clone();
        tokio::task::spawn_blocking(move || {
            crate::lifecycle::wait_for_unlocked_daemons(&database, crate::lifecycle::start_wait())
        })
        .await
        .map_err(|error| Error::Daemon(format!("daemon handover task failed: {error}")))??;
    }
    // Claim the database-scoped socket before opening canonical storage. The
    // daemon lock already excludes other lock-holding daemons; the socket
    // probe still refuses a serving daemon from a build without the lock.
    let (listener, _guard) = bind_socket(
        &config.daemon.socket_path,
        usize::from(config.daemon.max_connections),
    )?;
    let database = Database::open_with_options(
        &config.storage.db_path,
        usize::from(config.daemon.read_connections),
        Duration::from_millis(config.daemon.read_wait_timeout_ms),
    )?;
    let queue_capacity = usize::try_from(config.daemon.writer_queue_capacity)
        .map_err(|_| Error::Config("writer queue capacity exceeds this platform".to_owned()))?;
    let local_shutdown = Arc::new(tokio::sync::Notify::new());
    let blocking_concurrency = usize::from(config.daemon.read_connections)
        .checked_add(usize::from(config.daemon.background_concurrency))
        .ok_or_else(|| Error::Config("blocking concurrency exceeds this platform".to_owned()))?;
    let merged = database.with_reader(crate::merge::redirects)?;
    let state = DaemonState {
        merged_projects: Arc::new(std::sync::RwLock::new(Arc::new(merged))),
        judge_slot: Arc::new(Semaphore::new(1)),
        judge_stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        config: Arc::new(config.clone()),
        database,
        redactor: Redactor::new(&config.security.custom_redaction_patterns)?,
        scheduler: MutationScheduler::start(queue_capacity),
        blocking_slots: Arc::new(Semaphore::new(blocking_concurrency)),
        background_slots: Arc::new(Semaphore::new(usize::from(
            config.daemon.background_concurrency,
        ))),
        world_cache: WorldReadCache::from_config(&config.cache),
        metrics: Arc::new(MetricsRegistry::default()),
        shutdown: Arc::clone(&local_shutdown),
        started: Instant::now(),
        draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        in_flight: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    // Preserve a pre-catalog guide once. The vault is an optional export from
    // this point onward; it is never required to serve documentation.
    let product_store =
        crate::product::ProductStore::new(state.database.clone(), state.redactor.clone());
    if product_store
        .query(&crate::product::Query::Guide)?
        .is_null()
    {
        let legacy = config.vault.root.join("ARCHITECTURE.md");
        if std::fs::metadata(&legacy).is_ok_and(|m| m.len() <= 262_144)
            && let Ok(content) = std::fs::read_to_string(&legacy)
        {
            let stamp = crate::architect::stamped_digest(&content)
                .unwrap_or("legacy-unverified")
                .to_owned();
            let import = state
                .scheduler
                .submit(MutationPriority::Interactive, move || {
                    product_store.mutate(crate::product::Mutation::Save {
                        id: Some("workspace-guide".to_owned()),
                        expected_version: 0,
                        item: Box::new(crate::product::Item::Guide(crate::product::Guide {
                            content,
                            dossier_digest: stamp,
                            model: "legacy-import".to_owned(),
                        })),
                    })
                })?;
            match tokio::task::spawn_blocking(move || import.wait()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    eprintln!("legacy guide import skipped; original export preserved: {error}");
                }
                Err(error) => eprintln!("legacy guide import task failed: {error}"),
            }
        }
    }
    let router = Router::new()
        .route(API_EFFECTIVENESS, post(effectiveness_report))
        .route(API_USAGE_RECORD, post(record_usage))
        .route(API_TOOL_OBSERVATION, post(record_tool_observation))
        .route(
            API_VERIFICATION_OBSERVATION,
            post(record_verification_observation),
        )
        .route(API_JUDGE_STATUS, get(judge_status))
        .route(API_JUDGE_RUN, post(judge_run))
        .route(API_REVERIFY_PLAN, post(reverify_plan))
        .route(API_REVERIFY_ATTACH, post(reverify_attach))
        .route(API_REVIEW_EXPIRE, post(review_expire))
        .route("/api/v1/product/lessons", post(lesson_mutate))
        .route("/api/v1/product/query", post(product_query))
        .route("/api/v1/product/mutate", post(product_mutate))
        .route(API_HEALTH, get(health))
        .route(API_METRICS, get(metrics_snapshot))
        .route(API_MEMORY_STATS, get(memory_stats))
        .route(API_DOCTOR, get(doctor))
        .route(API_SHUTDOWN, post(shutdown))
        .route(API_BACKUP_CREATE, post(create_backup))
        .route(API_BACKUP_VERIFY, post(verify_backup))
        .route(API_PROJECT_EXPORT, post(export_project_handler))
        .route(API_RESOLVE_PROJECT, post(resolve_project))
        .route(API_LOOKUP_PROJECT, post(lookup_project))
        .route(API_FILE_SHAPE, post(file_shape))
        .route(API_TASKS_BRIEF, post(tasks_brief))
        .route(API_INDEX_PROJECT, post(index_project))
        .route(API_WORLD_SEARCH, post(search_world))
        .route(API_WORLD_NEIGHBORS, post(world_neighbors))
        .route(API_CONTEXT_COMPILE, post(compile_context))
        .route(API_CONTEXT_DELIVERED, post(context_delivered))
        .route(API_CONTEXT_EXPLAIN, post(explain_context))
        .route(API_MEMORY_CREATE, post(create_memory))
        .route(API_MEMORY_SEARCH, post(list_memories))
        .route(
            "/api/v1/memories/{id}",
            get(get_memory).delete(purge_memory),
        )
        .route("/api/v1/memories/{id}/promote", post(promote_memory))
        .route("/api/v1/memories/{id}/forget", post(forget_memory))
        .route(
            "/api/v1/memories/{id}/restore-drift",
            post(restore_memory_drift),
        )
        .route(API_TASKS, post(start_task))
        .route(
            "/api/v1/tasks/{id}/prompt",
            axum::routing::delete(purge_task_prompt),
        )
        .route("/api/v1/tasks/{id}/attempts", post(record_attempt))
        .route("/api/v1/tasks/{id}/finish", post(finish_task))
        .route(API_EVIDENCE, post(record_evidence))
        .route(API_HOOK_EVIDENCE, post(record_hook_evidence))
        .route("/api/v1/evidence/{id}", get(get_evidence))
        .route(API_PAST_FAILURES, post(past_failures))
        .route(API_EVALUATION_RUNS, post(record_evaluation))
        .route(API_LEARNING_DISTILL, post(distill))
        .route(API_SEMANTIC_REBUILD, post(rebuild_semantic_index))
        .route(API_JOBS, post(enqueue_job))
        .route(API_JOBS_LIST, post(list_jobs))
        .route("/api/v1/jobs/{id}", get(get_job))
        .route(API_SYNTHESIS_RUN, post(run_synthesis))
        .route(API_SKILL_PROMOTE, post(promote_skill))
        .route(API_PROJECT_LINK, post(link_project))
        .route(API_PROJECT_UNLINK, post(unlink_project))
        .route(API_PROJECT_LINKS, post(list_project_links))
        .route(API_VAULT_BUILD, post(build_vault))
        .route(API_LINK_SCAN, post(scan_links))
        .route(API_FACT_SET, post(set_fact))
        .route(API_FACT_UNSET, post(unset_fact))
        .route(API_FACT_LIST, post(list_facts))
        .route(API_ARCHITECT_DOSSIER, get(architect_dossier))
        .route(API_WORKSPACE, get(workspace_snapshot))
        .route(API_SYNC_EXPORT, get(sync_export_handler))
        .route(API_SYNC_OFFERS, get(sync_offers_handler))
        .route(API_SYNC_APPLY, post(sync_apply_handler))
        .route(API_SYNC_ACCEPT, post(sync_accept_handler))
        .route(API_ACTIVITY, get(workspace_activity))
        .route(API_MODULE_MAP, post(project_module_map))
        .route(API_MODULE_SYMBOLS, post(project_module_symbols))
        .route(API_ORIGIN_REFRESH, post(origin_refresh_handler))
        .route(API_MEMORY_PROVENANCE, post(memory_provenance_handler))
        .route(API_PROJECT_MERGE, post(merge_projects))
        .route(API_PROJECT_MERGE_FINALIZE, post(finalize_merge))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            redirect_merged_projects,
        ))
        .layer(DefaultBodyLimit::max(
            usize::try_from(config.daemon.max_request_bytes)
                .map_err(|_| Error::Config("request limit exceeds this platform".to_owned()))?,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            track_request_latency,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            drain_gate,
        ))
        .with_state(state.clone());

    let (jobs_stop_sender, jobs_handle) = start_job_system(&state, &config).await?;

    // The listener stays open until the drain below has finished, so clients
    // see `draining` instead of a refused connection and never start a second
    // daemon while this one still writes.
    let (close_sender, close_receiver) = tokio::sync::oneshot::channel::<()>();
    let mut server = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                let _ = close_receiver.await;
            })
            .into_future(),
    );
    let socket_path = config.daemon.socket_path.clone();
    let server_outcome = |joined: std::result::Result<io::Result<()>, tokio::task::JoinError>| {
        joined
            .map_err(|error| Error::Daemon(format!("daemon server task failed: {error}")))?
            .map_err(|source| io_error(&socket_path, source))
    };
    tokio::select! {
        () = external_shutdown => {}
        () = local_shutdown.notified() => {}
        joined = &mut server => return server_outcome(joined),
    }
    let drained = drain(&state, jobs_stop_sender, jobs_handle).await;
    let _ = close_sender.send(());
    if let Ok(joined) = tokio::time::timeout(Duration::from_secs(5), &mut server).await {
        server_outcome(joined)?;
    } else {
        server.abort();
        eprintln!("engramd: closing connections that outlived the drain");
    }
    drained
}

/// Stop admitting work, then finish what was admitted, each step bounded:
/// the background job, a judge call and in-flight requests within
/// [`DRAIN_DEADLINE`], then already-queued writes within
/// [`WRITE_DRAIN_DEADLINE`]. Work still unfinished at a bound is logged.
async fn drain(
    state: &DaemonState,
    jobs_stop: tokio::sync::watch::Sender<bool>,
    jobs: Option<tokio::task::JoinHandle<()>>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    state.draining.store(true, Ordering::SeqCst);
    state.judge_stopping.store(true, Ordering::SeqCst);
    let _ = jobs_stop.send(true);
    let deadline = tokio::time::Instant::now() + DRAIN_DEADLINE;
    let mut outcome = Ok(());
    if let Some(handle) = jobs {
        match tokio::time::timeout_at(deadline, handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                outcome = Err(Error::Daemon(format!(
                    "job runner exited abnormally: {error}"
                )));
            }
            Err(_) => eprintln!("engramd: the running background job outlived the drain deadline"),
        }
    }
    // Finish the one in-flight, timeout-bounded inference and persist its
    // audit. No further batch items start once stopping.
    let _judge_finished = match tokio::time::timeout_at(deadline, state.judge_slot.acquire()).await
    {
        Ok(Ok(permit)) => Some(permit),
        Ok(Err(error)) => {
            return Err(Error::Daemon(format!(
                "judge shutdown wait failed: {error}"
            )));
        }
        Err(_) => {
            eprintln!("engramd: the judge call outlived the drain deadline");
            None
        }
    };
    while state.in_flight.load(Ordering::SeqCst) > 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let unanswered = state.in_flight.load(Ordering::SeqCst);
    if unanswered > 0 {
        eprintln!("engramd: {unanswered} requests outlived the drain deadline and are cut off");
    }
    // Some endpoints acknowledge once a write is queued; let those land.
    let write_deadline = tokio::time::Instant::now() + WRITE_DRAIN_DEADLINE;
    while state.scheduler.pending() > 0 && tokio::time::Instant::now() < write_deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let dropped = state.scheduler.pending();
    if dropped > 0 {
        eprintln!(
            "engramd: {dropped} queued writes did not finish within {} s and are dropped",
            WRITE_DRAIN_DEADLINE.as_secs()
        );
    }
    outcome
}

/// Recover crashed jobs, then start the daemon-owned background job loop.
///
/// Crash recovery (spec section 27.5): the daemon is the only executor, so any
/// `running` job at startup means the previous process died mid-run. The reset
/// happens before the runner or any client can observe the queue, and runs
/// even when the runner is disabled so the queue never lies about liveness.
///
/// `Notify::notify_one` wakes only a single waiter, so daemon shutdown is
/// fanned out to the job loop through the returned watch sender instead of
/// adding a second competing waiter on the shutdown notify.
async fn start_job_system(
    state: &DaemonState,
    config: &Config,
) -> Result<(
    tokio::sync::watch::Sender<bool>,
    Option<tokio::task::JoinHandle<()>>,
)> {
    let recovery_queue = JobQueue::new(state.database.clone());
    let recovery = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            recovery_queue.recover_running(job_time_ms())
        })?;
    tokio::task::spawn_blocking(move || recovery.wait())
        .await
        .map_err(|error| Error::Daemon(format!("job recovery task failed: {error}")))??;

    if config.watch.enabled {
        let state_dir = config
            .storage
            .db_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("worktrees");
        let origin_poll = (config.watch.origin_poll_secs > 0)
            .then(|| std::time::Duration::from_secs(config.watch.origin_poll_secs));
        tokio::spawn(watch_loop(
            config.daemon.socket_path.clone(),
            state.database.clone(),
            state_dir,
            std::time::Duration::from_secs(config.watch.local_poll_secs.max(5)),
            origin_poll,
            SyncAuto {
                central_url: config.sync.central_url.clone(),
                token: config.sync.token.clone(),
                every: std::time::Duration::from_secs(config.sync.every_secs.max(60)),
                push: config.sync.push,
            },
            GuideAuto {
                mode: config.architect.auto.clone(),
                min_interval: std::time::Duration::from_secs(
                    config.architect.auto_min_secs.max(300),
                ),
                runtime_url: config.architect.runtime_url.clone(),
                model: config.architect.model.clone(),
                local_only: config.network.local_only,
                vault_root: config.vault.root.clone(),
                vault_enabled: config.vault.enabled,
            },
        ));
    }

    // The workspace browser rides inside the daemon: loopback-only, and a
    // busy port degrades to a warning instead of failing the daemon.
    if config.web.enabled {
        let web_config = config.clone();
        let port = config.web.port;
        tokio::spawn(async move {
            if let Err(error) = crate::web::serve(web_config, port).await {
                eprintln!("workspace browser not started on 127.0.0.1:{port}: {error}");
            }
        });
    }

    let (stop_sender, stop_receiver) = tokio::sync::watch::channel(false);
    if config.judge.enabled {
        let state = state.clone();
        let mut stopped = stop_receiver.clone();
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_secs(state.config.judge.interval_seconds));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _=interval.tick()=>{if admit_work(&state,MutationPriority::BackgroundLearning).is_ok(){spawn_judge_pass(state.clone());}},
                    _=stopped.changed()=>break,
                }
            }
        });
    }
    let handle = if config.jobs.enabled {
        let executor = JobExecutor::new(
            state.database.clone(),
            state.scheduler.clone(),
            state.redactor.clone(),
            config.jobs.clone(),
            Some(Arc::clone(&state.metrics)),
        )
        .with_vault_root(config.vault.enabled.then(|| config.vault.root.clone()))
        .with_semantic_provider(LocalEmbedderSettings::from_config(&config.semantic));
        let runner = JobRunner::new(
            executor,
            state.scheduler.clone(),
            Arc::clone(&state.background_slots),
            config.jobs.clone(),
            mebibytes_to_bytes(config.daemon.rss_hard_limit_mb),
            Arc::new(current_rss_bytes),
        );
        Some(runner.spawn(stop_receiver))
    } else {
        drop(stop_receiver);
        None
    };
    Ok((stop_sender, handle))
}

/// Wall-clock milliseconds for durable job rows.
fn job_time_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

async fn enqueue_job(
    State(state): State<DaemonState>,
    Json(request): Json<JobEnqueueRequest>,
) -> std::result::Result<Json<JobRecord>, ApiError> {
    admit_work(&state, MutationPriority::BackgroundLearning)?;
    let queue = JobQueue::new(state.database.clone());
    let enqueue = EnqueueJob {
        job_type: request.job_type,
        project_id: request.project_id,
        payload: request.payload.unwrap_or_else(|| serde_json::json!({})),
        dedupe_key: request
            .dedupe_key
            .unwrap_or_else(|| JobId::new().to_string()),
        max_attempts: state.config.jobs.max_attempts,
    };
    let handle = state
        .scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            queue.enqueue(&enqueue, job_time_ms())
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn list_jobs(
    State(state): State<DaemonState>,
    Json(request): Json<JobListRequest>,
) -> std::result::Result<Json<Vec<JobRecord>>, ApiError> {
    let queue = JobQueue::new(state.database.clone());
    let jobs = run_blocking(&state, move || queue.list(request.status, request.limit)).await?;
    Ok(Json(jobs))
}

async fn get_job(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<Option<JobRecord>>, ApiError> {
    let id = JobId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let queue = JobQueue::new(state.database.clone());
    let job = run_blocking(&state, move || queue.get(&id)).await?;
    Ok(Json(job))
}

async fn health(State(state): State<DaemonState>) -> Json<HealthResponse> {
    let rss_bytes = current_rss_bytes();
    let soft = mebibytes_to_bytes(state.config.daemon.rss_soft_limit_mb);
    let hard = mebibytes_to_bytes(state.config.daemon.rss_hard_limit_mb);
    Json(HealthResponse {
        status: "ok".to_owned(),
        workspace: state.config.workspace.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        build_id: crate::BUILD_ID.to_owned(),
        database: state.config.storage.db_path.clone(),
        uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        local_only: state.config.network.local_only,
        scheduler: state.scheduler.stats(),
        read_pool: state.database.read_pool_stats(),
        rss_bytes,
        rss_soft_limit_bytes: soft,
        rss_hard_limit_bytes: hard,
        memory_pressure: classify_memory_pressure(rss_bytes, soft, hard),
        cache: state.world_cache.as_ref().map(|cache| cache.health()),
        pid: Some(std::process::id()),
        launcher: std::env::var(crate::autostart::LAUNCHER_ENV).ok(),
    })
}

/// Counts one admitted request until its response is produced.
struct InFlight(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Admit requests while serving; answer `draining` once shutdown started.
/// The request is counted before the flag is read, so the drain cannot miss
/// a request that was admitted just before it began.
async fn drain_gate(
    State(state): State<DaemonState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    state
        .in_flight
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _counted = InFlight(Arc::clone(&state.in_flight));
    if state.draining.load(std::sync::atomic::Ordering::SeqCst)
        && request.uri().path() != API_SHUTDOWN
    {
        return ApiError::draining().into_response();
    }
    next.run(request).await
}

/// Redirect requests that still carry a merged project id (a stale pin, a
/// hook sidecar written before the merge, an MCP client that cached it) to
/// its survivor. Only typed project references are rewritten (see
/// [`crate::merge::redirect_project_references`]); prompts, commands,
/// artifacts and other text keep every byte. The merge endpoints themselves
/// see the ids they were given.
async fn redirect_merged_projects(
    State(state): State<DaemonState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let redirects = state
        .merged_projects
        .read()
        .map(|guard| Arc::clone(&guard))
        .unwrap_or_default();
    let path = request.uri().path();
    if redirects.is_empty()
        || request.method() == hyper::Method::GET
        || path == API_PROJECT_MERGE
        || path == API_PROJECT_MERGE_FINALIZE
    {
        return next.run(request).await;
    }
    let limit = usize::try_from(state.config.daemon.max_request_bytes).unwrap_or(usize::MAX);
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, limit).await else {
        return ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            body: ApiErrorBody {
                code: "payload_too_large".to_owned(),
                message: format!("request body exceeds {limit} bytes"),
                retryable: false,
            },
        }
        .into_response();
    };
    let bytes = crate::merge::redirect_request_body(&bytes, &redirects).map_or(bytes, Bytes::from);
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
    .await
}

enum PreparedMerge {
    AlreadyMerged(Box<crate::merge::MergeOutcome>),
    Ready(Box<crate::merge::MergePlan>),
}

/// Resolve both selectors, refuse invalid parties, probe every recorded
/// checkout for identity evidence and plan the merge. Read-only.
fn prepare_merge(database: &Database, request: &ProjectMergeRequest) -> Result<PreparedMerge> {
    let registry = ProjectRegistry::new(database.clone());
    let survivor = crate::merge::resolve_selector(&registry, &request.into)?;
    let retired = crate::merge::resolve_selector(&registry, &request.from)?;
    let from_path = Path::new(request.from.trim());
    let earlier = database.with_reader(|connection| {
        // A repeated merge whose --from names a checkout path that the merge
        // moved to the survivor.
        if survivor == retired && from_path.is_absolute() {
            let path = from_path
                .canonicalize()
                .unwrap_or_else(|_| from_path.to_path_buf());
            if let Some(original) =
                crate::merge::merge_that_moved(connection, &survivor, &path.to_string_lossy())?
            {
                return crate::merge::earlier_merge(connection, &survivor, &original);
            }
        }
        crate::merge::earlier_merge(connection, &survivor, &retired)
    })?;
    if let Some(outcome) = earlier {
        return Ok(PreparedMerge::AlreadyMerged(Box::new(outcome)));
    }
    let (survivor_party, retired_party) = database.with_reader(|connection| {
        crate::merge::check_parties(connection, &survivor, &retired)?;
        Ok((
            crate::merge::party(connection, &survivor)?,
            crate::merge::party(connection, &retired)?,
        ))
    })?;
    let evidence = crate::merge::identity_evidence(&survivor_party, &retired_party);
    let plan = database.with_reader(|connection| {
        let snapshot = connection.unchecked_transaction()?;
        let plan = crate::merge::plan(&snapshot, &survivor_party, &retired_party, evidence)?;
        snapshot.commit()?;
        Ok(plan)
    })?;
    Ok(PreparedMerge::Ready(Box::new(plan)))
}

/// Longest the identity probe of a merge may take (Git on every checkout).
const MERGE_PROBE_DEADLINE: Duration = Duration::from_secs(300);

async fn merge_projects(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectMergeRequest>,
) -> std::result::Result<Json<ProjectMergeResponse>, ApiError> {
    let database = state.database.clone();
    let dry_run = request.dry_run;
    let prepared = run_blocking_with_timeout(&state, MERGE_PROBE_DEADLINE, move || {
        prepare_merge(&database, &request)
    })
    .await?;
    let plan = match prepared {
        PreparedMerge::AlreadyMerged(outcome) => {
            reload_merged_projects(&state);
            return Ok(Json(ProjectMergeResponse::AlreadyMerged { outcome }));
        }
        PreparedMerge::Ready(plan) if dry_run => {
            return Ok(Json(ProjectMergeResponse::Planned { plan }));
        }
        PreparedMerge::Ready(plan) => plan,
    };
    if let Some(refusal) = &plan.identity.refusal {
        return Err(ApiError::from_core(&Error::MergeRefused(refusal.clone())));
    }
    let backup = state
        .config
        .storage
        .db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("backups")
        .join(format!(
            "pre-merge-{}-{}.sqlite",
            &plan.retired.id.as_str()[4..12],
            job_time_ms()
        ));
    let input = crate::merge::MergeExecution {
        survivor: plan.survivor.id.clone(),
        retired: plan.retired.id.clone(),
        identity: plan.identity.clone(),
        backup_path: Some(backup.display().to_string()),
    };
    let job_state = state.clone();
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            merge_job(&job_state, &input, &backup)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let (outcome, merged) = wait_for_mutation(&state, handle).await?;
    let outcome = Box::new(outcome);
    Ok(Json(if merged {
        ProjectMergeResponse::Merged { outcome }
    } else {
        ProjectMergeResponse::AlreadyMerged { outcome }
    }))
}

/// The scheduler job of an executed merge: the backup, the merge
/// transaction, and making the merge visible. Nothing writes between the
/// backup and the transaction, and the job finishes (redirects included)
/// even when the client that asked for it has disconnected.
fn merge_job(
    state: &DaemonState,
    input: &crate::merge::MergeExecution,
    backup: &Path,
) -> Result<(crate::merge::MergeOutcome, bool)> {
    let earlier = state.database.with_reader(|connection| {
        crate::merge::earlier_merge(connection, &input.survivor, &input.retired)
    })?;
    if let Some(outcome) = earlier {
        reload_merged_projects(state);
        return Ok((outcome, false));
    }
    state.database.backup(backup)?;
    let outcome = crate::merge::execute(&state.database, input)?;
    after_merge(state, &outcome);
    Ok((outcome, true))
}

/// Load the survivor of every merged project id into the request redirect.
fn reload_merged_projects(state: &DaemonState) {
    match state.database.with_reader(crate::merge::redirects) {
        Ok(redirects) => {
            if let Ok(mut guard) = state.merged_projects.write() {
                *guard = Arc::new(redirects);
            }
        }
        Err(error) => eprintln!("engramd: could not reload merged project ids: {error}"),
    }
}

/// Make a committed merge visible: redirect its retired id from now on,
/// invalidate cached graph reads, re-scope writes queued behind the merge
/// (recorded in an audit event and logged), and re-embed model vectors the
/// merge dropped.
fn after_merge(state: &DaemonState, outcome: &crate::merge::MergeOutcome) {
    reload_merged_projects(state);
    if let Some(cache) = &state.world_cache {
        cache.bump(&outcome.survivor);
        cache.bump(&outcome.retired);
    }
    let database = state.database.clone();
    let survivor = outcome.survivor.clone();
    let swept = state
        .scheduler
        .submit(MutationPriority::Analytics, move || {
            let swept = crate::merge::finalize(
                &database,
                &survivor,
                &crate::merge::FinalizeOptions {
                    trigger: "after_merge".to_owned(),
                    ..crate::merge::FinalizeOptions::default()
                },
            )?;
            if !swept.stragglers.is_empty() {
                eprintln!(
                    "engramd: moved rows written under a merged id to {survivor}: {:?}",
                    swept.stragglers
                );
            }
            Ok(())
        });
    if let Err(error) = swept {
        eprintln!(
            "engramd: could not queue the post-merge sweep ({error}); the merge command's \
             last step runs it"
        );
    }
    let dropped_vectors = outcome
        .audit
        .rebuilt
        .get("memory_vectors")
        .copied()
        .unwrap_or(0);
    if dropped_vectors > 0 && state.config.jobs.enabled {
        let queue = JobQueue::new(state.database.clone());
        let enqueue = EnqueueJob {
            job_type: JobType::SemanticRebuild,
            project_id: Some(outcome.survivor.clone()),
            payload: serde_json::json!({"reason": "project merge re-scoped memory vectors"}),
            dedupe_key: format!("merge-{}", outcome.merge_event_id),
            max_attempts: state.config.jobs.max_attempts,
        };
        let _ = state
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                queue.enqueue(&enqueue, job_time_ms())
            });
    }
}

async fn finalize_merge(
    State(state): State<DaemonState>,
    Json(request): Json<MergeFinalizeRequest>,
) -> std::result::Result<Json<crate::merge::FinalizeOutcome>, ApiError> {
    let database = state.database.clone();
    let survivor = request.project_id.clone();
    let options = crate::merge::FinalizeOptions {
        resolve_anchors: request.resolve_anchors,
        reindexed: request.reindexed.clone(),
        trigger: "client".to_owned(),
    };
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            crate::merge::finalize(&database, &survivor, &options)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let outcome = wait_for_mutation(&state, handle).await?;
    reload_merged_projects(&state);
    if let Some(cache) = &state.world_cache {
        cache.bump(&request.project_id);
    }
    Ok(Json(outcome))
}

/// Time every served request into its endpoint-group histogram. One relaxed
/// atomic add per request keeps the hot-path overhead trivial.
async fn track_request_latency(
    State(state): State<DaemonState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let group = EndpointGroup::classify(request.uri().path());
    let started = Instant::now();
    let response = next.run(request).await;
    state.metrics.observe_request(group, started.elapsed());
    response
}

async fn metrics_snapshot(State(state): State<DaemonState>) -> Json<MetricsResponse> {
    Json(MetricsResponse {
        uptime_ms: u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        metrics: state.metrics.snapshot(),
        cache: state.world_cache.as_ref().map(|cache| cache.health()),
    })
}

async fn memory_stats(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<MemoryStatsResponse>, ApiError> {
    let database = state.database.clone();
    let breakdown = run_blocking(&state, move || {
        database.with_reader(|connection| {
            let group_counts = |query: &str| -> Result<std::collections::BTreeMap<String, u64>> {
                let mut statement = connection.prepare(query)?;
                let rows = statement.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?;
                let mut counts = std::collections::BTreeMap::new();
                for row in rows {
                    let (key, count) = row?;
                    counts.insert(key, u64::try_from(count).unwrap_or(0));
                }
                Ok(counts)
            };
            let by_status = group_counts("SELECT status, COUNT(*) FROM memories GROUP BY status")?;
            let by_epistemic_state = group_counts(
                "SELECT epistemic_state, COUNT(*) FROM memories GROUP BY epistemic_state",
            )?;
            let total = by_status.values().sum();
            Ok(MemoryStatsResponse {
                total,
                by_status,
                by_epistemic_state,
            })
        })
    })
    .await?;
    Ok(Json(breakdown))
}

async fn doctor(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<DoctorReport>, ApiError> {
    let config = Arc::clone(&state.config);
    let database = state.database.clone();
    let report = run_blocking(&state, move || {
        Ok(DoctorReport::run_with_database(&config, &database))
    })
    .await?;
    Ok(Json(report))
}

async fn shutdown(State(state): State<DaemonState>) -> Json<()> {
    state.shutdown.notify_one();
    Json(())
}

async fn create_backup(
    State(state): State<DaemonState>,
    Json(request): Json<BackupRequest>,
) -> std::result::Result<Json<()>, ApiError> {
    let database = state.database.clone();
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            database.backup(&request.destination)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    wait_for_mutation(&state, handle).await?;
    Ok(Json(()))
}

async fn verify_backup(
    State(state): State<DaemonState>,
    Json(request): Json<BackupVerifyRequest>,
) -> std::result::Result<Json<IntegrityReport>, ApiError> {
    let report = run_blocking(&state, move || Database::verify_existing(&request.path)).await?;
    Ok(Json(report))
}

async fn export_project_handler(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectExportRequest>,
) -> std::result::Result<Json<ExportReport>, ApiError> {
    let database = state.database.clone();
    let report = run_blocking(&state, move || {
        export_project(&database, &request.project_id, &request.destination)
    })
    .await?;
    Ok(Json(report))
}

async fn file_shape(
    State(state): State<DaemonState>,
    Json(request): Json<FileShapeRequest>,
) -> std::result::Result<Json<FileShape>, ApiError> {
    if request.file_path.len() > 500 {
        return Err(ApiError::from_core(&Error::Config(
            "file path too long".to_owned(),
        )));
    }
    // Spans follow the same per-project switch as task source map ranges.
    let line_ranges = request.repository_root.as_deref().is_some_and(|root| {
        scoped_config(&state, root).map_or(state.config.context.source_line_ranges, |config| {
            config.context.source_line_ranges
        })
    });
    let database = state.database.clone();
    let shape = run_blocking(&state, move || {
        let (mut shape, ids, indexed_size) = file_shape_rows(&database, &request)?;
        let mut spans = crate::world::LiveSpans::new(
            &database,
            request.project_id.as_str(),
            request.repository_root.as_deref(),
        );
        if line_ranges {
            for (symbol, id) in shape.symbols.iter_mut().zip(&ids) {
                if let crate::world::SpanCheck::Current(lines) = spans.check(id)? {
                    symbol.lines = Some(lines);
                }
            }
        }
        shape.size_bytes = spans.checkout_size(&request.file_path).or(indexed_size);
        Ok(shape)
    })
    .await?;
    Ok(Json(shape))
}

/// The indexed part of a file shape: symbols with their node ids (in the
/// same order), callers, imports and the indexed file size.
fn file_shape_rows(
    database: &Database,
    request: &FileShapeRequest,
) -> Result<(FileShape, Vec<String>, Option<u64>)> {
    use rusqlite::OptionalExtension as _;
    database.with_reader(|connection| {
        let (ids, symbols) = ranked_file_symbols(connection, request)?;
        let callers = file_shape_callers(connection, request)?;
        let mut statement = connection.prepare(
            "SELECT DISTINCT target.label FROM edges e
                 JOIN nodes source ON e.source_id = source.id
                 JOIN nodes target ON e.target_id = target.id
                 WHERE e.project_id = ?1 AND e.relation = 'IMPORTS'
                   AND source.file_path = ?2 AND target.tombstoned_at_ms IS NULL
                 ORDER BY target.label LIMIT 20",
        )?;
        let imports = statement
            .query_map(
                rusqlite::params![request.project_id.as_str(), request.file_path],
                |row| row.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let indexed_size = connection
            .query_row(
                "SELECT size_bytes FROM indexed_files WHERE project_id = ?1 AND path = ?2",
                rusqlite::params![request.project_id.as_str(), request.file_path],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .and_then(|size| u64::try_from(size).ok());
        Ok((
            FileShape {
                symbols,
                callers,
                imports,
                size_bytes: None,
            },
            ids,
            indexed_size,
        ))
    })
}

/// Symbols one file shape returns, at most.
const FILE_SHAPE_SYMBOLS: usize = 40;
/// Definitions of one file considered for relevance ordering, in file order.
const FILE_SHAPE_CANDIDATES: i64 = 10_000;

/// The file's definitions with their node ids, best first: those the task's
/// source map selected or the task names, then those matching task terms
/// (strongest first), then file order. Without task signals the order is
/// file order.
fn ranked_file_symbols(
    connection: &rusqlite::Connection,
    request: &FileShapeRequest,
) -> Result<(Vec<String>, Vec<FileShapeSymbol>)> {
    struct Ranked {
        tier: u8,
        score: f64,
        position: usize,
        id: String,
        symbol: FileShapeSymbol,
    }
    let selected: BTreeSet<String> = match &request.context_run_id {
        Some(run) => connection
            .prepare(
                "SELECT ci.source_id FROM context_items ci
                 JOIN context_runs cr ON cr.id = ci.context_run_id
                 WHERE ci.context_run_id = ?1 AND cr.project_id = ?2",
            )?
            .query_map(
                rusqlite::params![run.as_str(), request.project_id.as_str()],
                |row| row.get(0),
            )?
            .collect::<std::result::Result<_, _>>()?,
        None => BTreeSet::new(),
    };
    let terms = request
        .task_text
        .as_deref()
        .map(crate::repository_context::TaskTerms::parse);
    let mut statement = connection.prepare(
        "SELECT id, kind, label FROM nodes
         WHERE project_id = ?1 AND file_path = ?2 AND tombstoned_at_ms IS NULL
           AND kind IN ('Function', 'Type', 'Interface', 'Method', 'Module', 'Section')
         ORDER BY start_byte LIMIT ?3",
    )?;
    let mut ranked = statement
        .query_map(
            rusqlite::params![
                request.project_id.as_str(),
                request.file_path,
                FILE_SHAPE_CANDIDATES
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .enumerate()
        .map(|(position, (id, kind, label))| {
            let named = selected.contains(&id) || terms.as_ref().is_some_and(|t| t.names(&label));
            let score = if named {
                0.0
            } else {
                terms.as_ref().map_or(0.0, |t| t.partial_score(&label))
            };
            let tier = if named {
                0
            } else if score > 0.0 {
                1
            } else {
                2
            };
            Ranked {
                tier,
                score,
                position,
                id,
                symbol: FileShapeSymbol {
                    kind,
                    label,
                    lines: None,
                    task_match: tier < 2,
                },
            }
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        a.tier
            .cmp(&b.tier)
            .then_with(|| b.score.total_cmp(&a.score))
            .then_with(|| a.position.cmp(&b.position))
    });
    ranked.truncate(FILE_SHAPE_SYMBOLS);
    Ok(ranked
        .into_iter()
        .map(|ranked| (ranked.id, ranked.symbol))
        .unzip())
}

/// Definitions in other files that call into `request.file_path`.
fn file_shape_callers(
    connection: &rusqlite::Connection,
    request: &FileShapeRequest,
) -> Result<Vec<FileShapeCaller>> {
    // Tree-sitter records a CALLS target as an ExternalReference in
    // the REFERENCING file; recover this file's callers by matching
    // reference labels to the definitions here (same resolution the
    // vault uses). A name that another file also defines is skipped:
    // the label alone cannot say which definition a call means, and
    // duplicate private helpers or per-script `main`s made most such
    // matches coincidental. Drive the join FROM this file's few definitions —
    // label-first — so every step is index-covered; joining from the
    // edge side scans the whole project's CALLS set and times out on
    // large graphs (observed on a 41k-node monorepo).
    // CROSS JOIN pins the join order (SQLite honors it strictly):
    // without pinning — and without ANALYZE stats, which a fresh
    // brain never has — the planner drives from the project's whole
    // CALLS edge set instead of this file's handful of labels, and
    // a 40k-node monorepo blows the request deadline (observed:
    // 113 s → sub-second pinned). The index pins matter as well:
    // otherwise `target` is read through `nodes_project_active`
    // (every reference of the kind, per definition), which reached
    // seconds per call once Python call sites were indexed.
    let mut statement = connection.prepare(&format!(
        "SELECT caller_label, caller_path FROM (
                 SELECT DISTINCT caller.label AS caller_label,
                        caller.file_path AS caller_path
                 FROM nodes defined INDEXED BY nodes_project_file
                 CROSS JOIN nodes target INDEXED BY nodes_project_label
                 CROSS JOIN edges e
                 CROSS JOIN nodes caller
                 WHERE defined.project_id = ?1 AND defined.file_path = ?2
                   AND defined.kind IN ('Function', 'Type', 'Interface', 'Method')
                   AND defined.tombstoned_at_ms IS NULL
                   AND {only}
                   AND target.project_id = defined.project_id
                   AND target.kind = 'ExternalReference'
                   AND target.label = defined.label
                   AND target.tombstoned_at_ms IS NULL
                   AND e.project_id = defined.project_id
                   AND e.target_id = target.id AND e.relation = 'CALLS'
                   AND e.tombstoned_at_ms IS NULL
                   AND caller.id = e.source_id
                   AND caller.tombstoned_at_ms IS NULL
                   AND caller.file_path IS NOT NULL AND caller.file_path != ?2
                 UNION
                 SELECT DISTINCT caller.label, caller.file_path
                 FROM nodes callee
                 CROSS JOIN edges e
                 CROSS JOIN nodes caller
                 WHERE callee.project_id = ?1 AND callee.file_path = ?2
                   AND callee.kind <> 'ExternalReference'
                   AND callee.tombstoned_at_ms IS NULL
                   AND e.project_id = callee.project_id
                   AND e.target_id = callee.id AND e.relation = 'CALLS'
                   AND e.tombstoned_at_ms IS NULL
                   AND caller.id = e.source_id
                   AND caller.tombstoned_at_ms IS NULL
                   AND caller.file_path IS NOT NULL AND caller.file_path != ?2
             ) ORDER BY caller_path, caller_label LIMIT 25",
        only = crate::vault::defined_in_one_file("defined"),
    ))?;
    let callers = statement
        .query_map(
            rusqlite::params![request.project_id.as_str(), request.file_path],
            |row| {
                Ok(FileShapeCaller {
                    label: row.get(0)?,
                    file_path: row.get(1)?,
                })
            },
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(callers)
}

async fn tasks_brief(
    State(state): State<DaemonState>,
    Json(request): Json<TasksBriefRequest>,
) -> std::result::Result<Json<TasksBrief>, ApiError> {
    let database = state.database.clone();
    let brief = run_blocking(&state, move || {
        database.with_reader(|connection| {
            fn row_to_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskBriefItem> {
                Ok(TaskBriefItem {
                    id: row.get(0)?,
                    prompt: row.get(1)?,
                    status: row.get(2)?,
                    started_at_ms: row.get(3)?,
                    completed_at_ms: row.get(4)?,
                })
            }
            let mut statement = connection.prepare(
                "SELECT id, prompt, status, started_at_ms, completed_at_ms FROM tasks
                 WHERE project_id = ?1 AND status = 'open'
                 ORDER BY started_at_ms DESC LIMIT 5",
            )?;
            let open = statement
                .query_map([request.project_id.as_str()], row_to_item)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut statement = connection.prepare(
                "SELECT id, prompt, status, started_at_ms, completed_at_ms FROM tasks
                 WHERE project_id = ?1 AND status IN ('success','partial','failure')
                 ORDER BY completed_at_ms DESC LIMIT 1",
            )?;
            let last_completed = statement
                .query_map([request.project_id.as_str()], row_to_item)?
                .next()
                .transpose()?;
            Ok(TasksBrief {
                open,
                last_completed,
            })
        })
    })
    .await?;
    Ok(Json(brief))
}

async fn lookup_project(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectResolveRequest>,
) -> std::result::Result<Json<ProjectLookupResponse>, ApiError> {
    let database = state.database.clone();
    let project = run_blocking(&state, move || {
        ProjectRegistry::new(database).lookup(&request.path)
    })
    .await?;
    Ok(Json(ProjectLookupResponse { project }))
}

async fn resolve_project(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectResolveRequest>,
) -> std::result::Result<Json<ProjectStatus>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let path = request.path;
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.resolve_or_register(&path)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let resolved = wait_for_mutation(&state, handle).await?;

    let project_id = resolved.project.id.clone();
    let database = state.database.clone();
    let database_path = state.config.storage.db_path.clone();
    let local_only = state.config.network.local_only;
    let (integrity, event_count, structural_index) = run_blocking(&state, move || {
        let integrity = "not_checked_on_hot_path".to_owned();
        let event_count = database.with_reader(|connection| {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM events WHERE project_id = ?1",
                    [project_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(Error::from)
        })?;
        let structural_index = WorldModel::new(database).latest_index_status(&project_id)?;
        Ok((integrity, event_count, structural_index))
    })
    .await?;

    let index_matches_checkout = structural_index.as_ref().is_some_and(|index| {
        resolved.git.as_ref().is_some_and(|git| {
            index.head == git.head && index.diff_fingerprint == git.diff_fingerprint
        })
    });
    Ok(Json(ProjectStatus {
        resolved,
        database: database_path,
        database_integrity: integrity,
        project_event_count: event_count,
        local_only,
        structural_index,
        index_matches_checkout,
    }))
}

#[allow(clippy::too_many_lines)]
async fn index_project(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectIndexRequest>,
) -> std::result::Result<Json<ProjectIndexResponse>, ApiError> {
    admit_work(&state, MutationPriority::ChangedFileRefresh)?;
    let registry = ProjectRegistry::new(state.database.clone());
    let path = request.path;
    let resolve = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.resolve_or_register(&path)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let resolved = wait_for_mutation(&state, resolve).await?;
    let scoped =
        scoped_config(&state, &resolved.root).map_err(|error| ApiError::from_core(&error))?;

    let world = WorldModel::new(state.database.clone());
    let scan_world = world.clone();
    let scan_project = resolved.clone();
    let requested_changed_paths = request.changed_paths;
    let ensure_fresh = request.ensure_fresh;
    let max_files = usize::try_from(
        scoped
            .structural
            .max_files
            .min(state.config.structural.max_files),
    )
    .map_err(|_| ApiError::from_core(&Error::Config("file limit is too large".to_owned())))?;
    let max_file_bytes = scoped
        .structural
        .max_file_bytes
        .min(state.config.structural.max_file_bytes);
    let provider_name = request
        .provider
        .unwrap_or_else(|| scoped.structural.provider.clone());
    let graphify_export = request
        .graphify_export
        .or_else(|| scoped.structural.graphify_export.clone());
    let max_graph_nodes = usize::try_from(
        scoped
            .structural
            .max_graph_nodes
            .min(state.config.structural.max_graph_nodes),
    )
    .map_err(|_| ApiError::from_core(&Error::Config("graph node limit is too large".to_owned())))?;
    let max_graph_edges = usize::try_from(
        scoped
            .structural
            .max_graph_edges
            .min(state.config.structural.max_graph_edges),
    )
    .map_err(|_| ApiError::from_core(&Error::Config("graph edge limit is too large".to_owned())))?;
    let max_export_bytes = scoped
        .structural
        .max_graphify_export_bytes
        .min(state.config.structural.max_graphify_export_bytes);
    let max_prepared_bytes = scoped
        .structural
        .max_prepared_bytes
        .min(state.config.structural.max_prepared_bytes);
    let git_limit = usize::try_from(
        scoped
            .structural
            .git_history_commits
            .min(state.config.structural.git_history_commits),
    )
    .map_err(|_| {
        ApiError::from_core(&Error::Config("Git history limit is too large".to_owned()))
    })?;
    let database = state.database.clone();
    let scan_started = Instant::now();
    let prepared = run_background(&state, move || {
        let provider: Box<dyn StructuralProvider> = match provider_name.as_str() {
            "tree_sitter" => Box::new(TreeSitterProvider::new(
                max_files,
                max_file_bytes,
                max_graph_nodes,
                max_graph_edges,
                max_prepared_bytes,
            )),
            "graphify" => Box::new(GraphifyProvider::new(
                graphify_export.ok_or_else(|| {
                    Error::Config("Graphify provider requires an export path".to_owned())
                })?,
                max_graph_nodes,
                max_graph_edges,
                max_export_bytes,
                max_prepared_bytes,
            )),
            other => {
                return Err(Error::Config(format!(
                    "unsupported structural provider `{other}`"
                )));
            }
        };
        let changed_paths = plan_changed_paths(
            &scan_world,
            &scan_project,
            requested_changed_paths,
            ensure_fresh,
        )?;
        let structural =
            scan_world.prepare_index(provider.as_ref(), &scan_project, changed_paths.as_deref())?;
        let git = if scan_project.git.is_some() {
            Some(GitHistory::new(database).prepare(&scan_project, git_limit)?)
        } else {
            None
        };
        Ok((structural, git))
    })
    .await?;
    state.metrics.observe_index_scan(scan_started.elapsed());

    let apply_project = resolved.clone();
    let redactor = state.redactor.clone();
    let apply = state
        .scheduler
        .submit(MutationPriority::ChangedFileRefresh, move || {
            let index = world.apply_prepared(&apply_project, prepared.0)?;
            let drift = MemoryStore::with_redactor(world.database().clone(), redactor.clone())
                .mark_symbol_drift_suspect(&apply_project.project.id)?;
            if let Err(error) =
                crate::product::ProductStore::new(world.database().clone(), redactor)
                    .refresh_changed_sections(&apply_project.project.id)
            {
                eprintln!("derived documentation refresh failed after indexing: {error}");
            }
            let git = prepared
                .1
                .map(|history| {
                    GitHistory::new(world.database().clone())
                        .apply_prepared(&apply_project, history)
                })
                .transpose()?;
            Ok((index, git, drift))
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let (index, git, drift) = wait_for_mutation(&state, apply).await?;
    state
        .metrics
        .count_drift_invalidation(drift.invalidated, drift.previously_injected);
    if let Some(cache) = &state.world_cache {
        cache.bump(&resolved.project.id);
    }
    let is_view_project = ProjectRegistry::new(state.database.clone())
        .is_view_project(&resolved.project.id)
        .unwrap_or(false);
    // Auto-link discovery: definitive manifest signals (path deps,
    // submodules) auto-apply with evidence in the note; name matches are
    // proposals only, surfaced by `engram link scan`. Shadow origin-views are
    // documentation snapshots, not real projects — they never link or
    // export a vault folder of their own.
    if state.config.links.auto_scan && !is_view_project {
        let database = state.database.clone();
        let project_id = resolved.project.id.clone();
        let root = resolved.root.clone();
        if let Ok(candidates) = run_blocking(&state, move || {
            crate::linkscan::scan(&database, &project_id, &root)
        })
        .await
            && candidates.iter().any(|candidate| candidate.definitive)
        {
            let database = state.database.clone();
            let project_id = resolved.project.id.clone();
            let _ = state
                .scheduler
                .submit(MutationPriority::BackgroundLearning, move || {
                    crate::linkscan::apply_definitive(&database, &project_id, &candidates)
                });
        }
    }

    // Derived vault refresh: a durable background job per project and refresh
    // window, so vault notes and diagrams track graph changes without blocking
    // the index response. Requires jobs + vault to both be enabled, and only
    // the projects the full vault build exports (not views, not hidden).
    if state.config.vault.enabled
        && state.config.vault.auto_refresh
        && state.config.jobs.enabled
        && crate::vault::exports_project(&state.database, &resolved.project.id).unwrap_or(false)
    {
        let queue = JobQueue::new(state.database.clone());
        let project_id = resolved.project.id.clone();
        let interval = state.config.vault.refresh_interval_secs;
        let max_attempts = state.config.jobs.max_attempts;
        let _ = state
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                queue.enqueue_vault_refresh(&project_id, interval, max_attempts, job_time_ms())
            });
    }
    Ok(Json(ProjectIndexResponse {
        resolved,
        index,
        git,
        invalidated_memories: drift.invalidated,
    }))
}

fn plan_changed_paths(
    world: &WorldModel,
    project: &ResolvedProject,
    requested: Option<Vec<PathBuf>>,
    ensure_fresh: bool,
) -> Result<Option<Vec<PathBuf>>> {
    if !ensure_fresh {
        return Ok(requested);
    }
    let Some(previous) = world.latest_index_status(&project.project.id)? else {
        return Ok(None);
    };
    if previous.needs_reindex {
        return Ok(None);
    }
    let mut changed = requested
        .unwrap_or_default()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let Some(git) = &project.git else {
        return Ok(Some(changed.into_iter().collect()));
    };

    if previous.head != git.head {
        let (Some(previous_head), Some(_)) = (previous.head.as_deref(), git.head.as_deref()) else {
            return Ok(None);
        };
        // Missing/pruned/unrelated revisions must never preserve a stale
        // graph. A bounded full scan is the correctness fallback.
        let Ok(committed_delta) = git.changed_paths_since(previous_head) else {
            return Ok(None);
        };
        changed.extend(committed_delta);
    }

    if previous.diff_fingerprint != git.diff_fingerprint {
        // Local edits changed since the prior scan. Files dirty at that scan
        // may have been reverted since, so they must be rescanned alongside
        // the currently dirty set. When the prior dirty set is unknown
        // (pre-migration scans), only a full rescan is safe.
        let Some(previous_dirty) = &previous.changed_paths else {
            return Ok(None);
        };
        changed.extend(previous_dirty.iter().cloned());
        changed.extend(git.changed_paths.iter().cloned());
    }
    Ok(Some(changed.into_iter().collect()))
}

async fn search_world(
    State(state): State<DaemonState>,
    Json(request): Json<WorldSearchRequest>,
) -> std::result::Result<Json<Vec<GraphNode>>, ApiError> {
    let cache_key = (request.query.clone(), request.limit);
    let generation = state.world_cache.as_ref().map(|cache| {
        (
            cache.generation(&request.project_id),
            request.project_id.clone(),
        )
    });
    if let (Some(cache), Some((generation, project_id))) = (&state.world_cache, &generation) {
        if let Some(nodes) = cache.search.get(project_id, *generation, &cache_key) {
            cache.record_hit();
            return Ok(Json((*nodes).clone()));
        }
        cache.record_miss();
    }
    let world = WorldModel::new(state.database.clone());
    let nodes = run_blocking(&state, move || {
        world.search_symbols(&request.project_id, &request.query, request.limit)
    })
    .await?;
    if let (Some(cache), Some((generation, project_id))) = (&state.world_cache, &generation) {
        let weight = cache_weight(&nodes);
        cache
            .search
            .insert(project_id, *generation, cache_key, nodes.clone(), weight);
    }
    Ok(Json(nodes))
}

async fn world_neighbors(
    State(state): State<DaemonState>,
    Json(request): Json<WorldNeighborsRequest>,
) -> std::result::Result<Json<GraphSlice>, ApiError> {
    let cache_key = (
        request.node_id.clone(),
        request.max_depth,
        request.max_nodes,
    );
    let generation = state.world_cache.as_ref().map(|cache| {
        (
            cache.generation(&request.project_id),
            request.project_id.clone(),
        )
    });
    if let (Some(cache), Some((generation, project_id))) = (&state.world_cache, &generation) {
        if let Some(slice) = cache.neighbors.get(project_id, *generation, &cache_key) {
            cache.record_hit();
            return Ok(Json((*slice).clone()));
        }
        cache.record_miss();
    }
    let world = WorldModel::new(state.database.clone());
    let slice = run_blocking(&state, move || {
        world.neighbors(
            &request.project_id,
            &request.node_id,
            request.max_depth,
            request.max_nodes,
        )
    })
    .await?;
    if let (Some(cache), Some((generation, project_id))) = (&state.world_cache, &generation) {
        let weight = cache_weight(&slice);
        cache
            .neighbors
            .insert(project_id, *generation, cache_key, slice.clone(), weight);
    }
    Ok(Json(slice))
}

async fn compile_context(
    State(state): State<DaemonState>,
    Json(request): Json<ContextCompileRequest>,
) -> std::result::Result<Json<CompiledContext>, ApiError> {
    admit_work(&state, MutationPriority::Interactive)?;
    let scoped = request
        .repository_root
        .as_deref()
        .map(|root| scoped_config(&state, root))
        .transpose()
        .map_err(|error| ApiError::from_core(&error))?;
    let max_tokens = scoped
        .as_ref()
        .map_or(state.config.context.max_injected_tokens, |config| {
            config
                .context
                .max_injected_tokens
                .min(state.config.context.max_injected_tokens)
        });
    let stable_tokens =
        scoped
            .as_ref()
            .map_or(state.config.context.stable_prefix_tokens, |config| {
                config
                    .context
                    .stable_prefix_tokens
                    .min(state.config.context.stable_prefix_tokens)
                    .min(max_tokens)
            });
    let semantic_enabled = scoped
        .as_ref()
        .map_or(state.config.context.semantic_retrieval, |config| {
            config.context.semantic_retrieval
        });
    let compact_injection = scoped
        .as_ref()
        .map_or(state.config.context.compact_injection, |config| {
            config.context.compact_injection
        });
    let source_context = scoped
        .as_ref()
        .map_or(&state.config.context, |c| &c.context);
    let compiler = (if semantic_enabled {
        let semantic = scoped
            .as_ref()
            .map_or(&state.config.semantic, |config| &config.semantic);
        ContextCompiler::with_semantic(
            state.database.clone(),
            max_tokens,
            stable_tokens,
            semantic_options(semantic, &state.config.semantic),
        )
    } else {
        ContextCompiler::new(state.database.clone(), max_tokens, stable_tokens)
    })
    .with_compact_injection(compact_injection)
    .with_review_standards(
        scoped
            .as_ref()
            .map_or(state.config.context.review_standards_section, |config| {
                config.context.review_standards_section
            }),
    )
    .with_memory_cap(
        scoped
            .as_ref()
            .map_or(state.config.context.max_injected_memories, |config| {
                config.context.max_injected_memories
            }),
    )
    .with_linked_projects(
        scoped
            .as_ref()
            .map_or(state.config.context.include_linked_projects, |config| {
                config.context.include_linked_projects
            }),
    )
    .with_source_line_ranges(source_context.source_line_ranges)
    .with_related_use_cues(source_context.related_use_cues);
    let prepare_compiler = compiler.clone();
    let request_mode = request.mode;
    let retrieval_started = Instant::now();
    let prepared = run_blocking(&state, move || prepare_compiler.prepare(&request)).await?;
    if request_mode == ContextMode::Brain {
        state.metrics.observe_retrieval(retrieval_started.elapsed());
    }
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            compiler.apply_prepared(prepared)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let context = wait_for_mutation(&state, handle).await?;
    state.metrics.observe_context_compile(
        context.mode == ContextMode::Brain,
        context.metrics.compile_ms,
        u64::try_from(context.items.len()).unwrap_or(u64::MAX),
        u64::from(context.metrics.selected_tokens),
        u64::from(context.metrics.candidate_count),
    );
    Ok(Json(context))
}

async fn context_delivered(
    State(state): State<DaemonState>,
    Json(request): Json<ContextDeliveryRequest>,
) -> std::result::Result<Json<bool>, ApiError> {
    admit_work(&state, MutationPriority::Interactive)?;
    let store = ExperienceStore::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.record_context_delivery(&request.task_id, &request.run_id)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

/// Bounds may be project-scoped; the embedding provider is always the
/// daemon's machine-level configuration.
fn semantic_options(
    bounds: &crate::config::SemanticConfig,
    machine: &crate::config::SemanticConfig,
) -> SemanticOptions {
    SemanticOptions::from_config(bounds, machine)
}

async fn rebuild_semantic_index(
    State(state): State<DaemonState>,
    Json(request): Json<SemanticRebuildRequest>,
) -> std::result::Result<Json<SemanticRebuildResponse>, ApiError> {
    admit_work(&state, MutationPriority::BackgroundLearning)?;
    let project_id = request.project_id.clone();
    let (database, scheduler) = (state.database.clone(), state.scheduler.clone());
    let local = LocalEmbedderSettings::from_config(&state.config.semantic);
    // A local model embeds outside the writer, page by page, and can outlive
    // the default deadline; it stops starting pages shortly before ours.
    let deadline =
        Duration::from_millis(state.config.daemon.request_timeout_ms).max(Duration::from_secs(600));
    let stop_at = Instant::now() + deadline.saturating_sub(Duration::from_secs(30));
    let report = run_blocking_with_timeout(&state, deadline, move || {
        crate::semantic::rebuild_project(
            &database,
            &scheduler,
            local.as_ref(),
            &request.project_id,
            Some(stop_at),
        )
    })
    .await?;
    Ok(Json(SemanticRebuildResponse {
        project_id,
        vectors: report.vectors,
        model: report.model,
    }))
}

fn scoped_config(state: &DaemonState, root: &Path) -> Result<Config> {
    Config::load_for(
        Some(root),
        &ConfigOverrides {
            workspace: Some(state.config.workspace.clone()),
            db_path: Some(state.config.storage.db_path.clone()),
            local_only: Some(state.config.network.local_only),
        },
    )
}

async fn explain_context(
    State(state): State<DaemonState>,
    Json(request): Json<ContextExplainRequest>,
) -> std::result::Result<Json<Option<ContextItem>>, ApiError> {
    let compiler = ContextCompiler::new(
        state.database.clone(),
        state.config.context.max_injected_tokens,
        state.config.context.stable_prefix_tokens,
    );
    let item = run_blocking(&state, move || {
        compiler.explain(&request.run_id, request.ordinal)
    })
    .await?;
    Ok(Json(item))
}

async fn link_project(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectLinkRequest>,
) -> std::result::Result<Json<LinkedProject>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.link(
                &request.project_id,
                &request.linked_project_id,
                &request.relation,
                request.note.as_deref(),
            )
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn unlink_project(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectUnlinkRequest>,
) -> std::result::Result<Json<ProjectUnlinkResponse>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.unlink(&request.project_id, &request.linked_project_id)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let removed = wait_for_mutation(&state, handle).await?;
    Ok(Json(ProjectUnlinkResponse { removed }))
}

async fn list_project_links(
    State(state): State<DaemonState>,
    Json(request): Json<ProjectLinksRequest>,
) -> std::result::Result<Json<Vec<LinkedProject>>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let links = run_blocking(&state, move || registry.links(&request.project_id)).await?;
    Ok(Json(links))
}

/// Tech detection walks the filesystem; on a central server the snapshot is
/// polled by every viewer, so results are cached for ten minutes.
fn cached_techstack(project_id: &ProjectId, path: &str) -> Vec<crate::techstack::Technology> {
    use std::sync::{Mutex, OnceLock};
    type TechCache = Mutex<
        std::collections::HashMap<String, (std::time::Instant, Vec<crate::techstack::Technology>)>,
    >;
    static CACHE: OnceLock<TechCache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    if let Ok(guard) = cache.lock()
        && let Some((at, cached)) = guard.get(project_id.as_str())
        && at.elapsed() < std::time::Duration::from_secs(600)
    {
        return cached.clone();
    }
    let detected = crate::techstack::detect(std::path::Path::new(path));
    if let Ok(mut guard) = cache.lock() {
        guard.insert(
            project_id.as_str().to_owned(),
            (std::time::Instant::now(), detected.clone()),
        );
    }
    detected
}

#[allow(clippy::too_many_lines)]
async fn workspace_snapshot(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<WorkspaceSnapshot>, ApiError> {
    let database = state.database.clone();
    let snapshot = run_blocking(&state, move || {
        let registry = ProjectRegistry::new(database.clone());
        let rows = database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT p.id, p.name,
                        (SELECT COUNT(*) FROM memories m
                          WHERE m.project_id = p.id AND m.status = 'active'
                            AND m.epistemic_state = 'validated'),
                        (SELECT COUNT(*) FROM nodes n
                          WHERE n.project_id = p.id AND n.tombstoned_at_ms IS NULL),
                        (SELECT s.head FROM structural_scans s
                          WHERE s.project_id = p.id
                          ORDER BY s.completed_at_ms DESC LIMIT 1),
                        (SELECT s.completed_at_ms FROM structural_scans s
                          WHERE s.project_id = p.id
                          ORDER BY s.completed_at_ms DESC LIMIT 1)
                 FROM projects p WHERE p.tombstoned_at_ms IS NULL
                   AND NOT EXISTS (SELECT 1 FROM project_facts pf WHERE pf.project_id = p.id
                       AND (pf.key = 'internal.view_of'
                            OR (pf.key = 'internal.hidden' AND pf.value = 'true')))
                 ORDER BY p.name, p.id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })?;
        let mut projects = Vec::new();
        for (id, name, validated_memories, nodes, indexed_head, indexed_at_ms) in rows {
            let project_id = crate::ids::ProjectId::parse(&id)?;
            let primary_path = registry
                .alias_paths(&project_id)?
                .into_iter()
                .find(|path| path.is_dir())
                .map(|path| path.display().to_string());
            let technologies = primary_path
                .as_deref()
                .map(|path| cached_techstack(&project_id, path))
                .unwrap_or_default();
            let memories = database.with_reader(|connection| {
                let mut statement = connection.prepare(
                    "SELECT m.id, m.kind, m.claim, m.confidence, m.tags_json,
                            m.epistemic_state,
                            (SELECT COUNT(DISTINCT served.source_id)
                             FROM usage_events injected
                             JOIN usage_events served
                               ON served.context_run_id = injected.context_run_id
                             WHERE injected.source_type = 'memory'
                               AND injected.source_id = m.id
                               AND injected.usage_kind = 'injected'
                               AND served.source_type = 'task'
                               AND served.usage_kind = 'context_served'
                               AND served.outcome = 'success'),
                            (SELECT COUNT(*) FROM usage_events u
                             WHERE u.source_type = 'memory' AND u.source_id = m.id
                               AND u.usage_kind = 'injected')
                     FROM memories m
                     WHERE m.project_id = ?1 AND m.status = 'active'
                     ORDER BY (m.epistemic_state = 'validated') DESC,
                              m.known_from_ms DESC, m.id LIMIT 40",
                )?;
                let rows = statement
                    .query_map([project_id.as_str()], |row| {
                        Ok(MemorySummary {
                            id: row.get(0)?,
                            kind: row.get(1)?,
                            claim: row.get(2)?,
                            confidence: row.get(3)?,
                            tags: serde_json::from_str(&row.get::<_, String>(4)?)
                                .unwrap_or_default(),
                            epistemic_state: row.get(5)?,
                            wins: row.get(6)?,
                            injections: row.get(7)?,
                        })
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })?;
            let facts = registry.facts(&project_id)?;
            let origin_view = facts
                .iter()
                .find(|fact| fact.key == "internal.origin_view")
                .and_then(|fact| crate::ids::ProjectId::parse(&fact.value).ok())
                .and_then(|shadow_id| {
                    database
                        .with_reader(|connection| {
                            use rusqlite::OptionalExtension as _;
                            let scan = connection
                                .query_row(
                                    "SELECT head, completed_at_ms FROM structural_scans
                                     WHERE project_id = ?1
                                     ORDER BY completed_at_ms DESC LIMIT 1",
                                    [shadow_id.as_str()],
                                    |row| {
                                        Ok((
                                            row.get::<_, Option<String>>(0)?,
                                            row.get::<_, Option<i64>>(1)?,
                                        ))
                                    },
                                )
                                .optional()?;
                            let nodes = connection.query_row(
                                "SELECT COUNT(*) FROM nodes
                                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL",
                                [shadow_id.as_str()],
                                |row| row.get::<_, i64>(0),
                            )?;
                            Ok(scan.map(|(indexed_head, indexed_at_ms)| OriginViewInfo {
                                project_id: shadow_id.clone(),
                                indexed_head,
                                indexed_at_ms,
                                nodes,
                            }))
                        })
                        .ok()
                        .flatten()
                });
            projects.push(WorkspaceProject {
                links: registry.links(&project_id)?,
                facts: facts
                    .into_iter()
                    .filter(|fact| !fact.key.starts_with("internal."))
                    .collect(),
                id: project_id,
                name,
                primary_path,
                validated_memories,
                memories,
                nodes,
                indexed_head,
                indexed_at_ms,
                origin_view,
                technologies,
            });
        }
        Ok(WorkspaceSnapshot { projects })
    })
    .await?;
    Ok(Json(snapshot))
}

async fn project_module_map(
    State(state): State<DaemonState>,
    Json(request): Json<ModuleMapRequest>,
) -> std::result::Result<Json<crate::vault::ModuleMap>, ApiError> {
    let database = state.database.clone();
    // Edge resolution outlives the default deadline on the biggest repos.
    let deadline =
        Duration::from_millis(state.config.daemon.request_timeout_ms).max(Duration::from_secs(180));
    let map = run_blocking_with_timeout(&state, deadline, move || {
        crate::vault::module_map(&database, &request.project_id)
    })
    .await?;
    Ok(Json(map))
}

async fn memory_provenance_handler(
    State(state): State<DaemonState>,
    Json(request): Json<MemoryProvenanceRequest>,
) -> std::result::Result<Json<MemoryProvenance>, ApiError> {
    let database = state.database.clone();
    let provenance = run_blocking(&state, move || {
        database.with_reader(|connection| {
            let (epistemic_state, confidence, trust_source, rationale) = connection
                .query_row(
                    "SELECT epistemic_state, confidence, trust_source, rationale
                     FROM memories WHERE id = ?1",
                    [request.memory_id.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, f64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .map_err(|_| Error::Config(format!("unknown memory `{}`", request.memory_id)))?;
            let mut statement = connection.prepare(
                "SELECT e.kind, e.strength, e.command, e.exit_code, e.observed_at_ms
                 FROM memory_evidence me
                 JOIN evidence e ON e.id = me.evidence_id
                 WHERE me.memory_id = ?1
                 ORDER BY e.observed_at_ms LIMIT 40",
            )?;
            let evidence = statement
                .query_map([request.memory_id.as_str()], |row| {
                    Ok(ProvenanceEvidence {
                        kind: row.get(0)?,
                        strength: row.get(1)?,
                        command: row.get(2)?,
                        exit_code: row.get(3)?,
                        observed_at_ms: row.get(4)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut episode_statement = connection.prepare(
                "SELECT t.id, t.prompt, t.status, t.accepted_by_user
                 FROM memory_sources ms
                 JOIN tasks t ON t.id = ms.task_id
                 WHERE ms.memory_id = ?1
                 ORDER BY t.started_at_ms LIMIT 20",
            )?;
            let episodes = episode_statement
                .query_map([request.memory_id.as_str()], |row| {
                    Ok(ProvenanceEpisode {
                        task_id: row.get(0)?,
                        prompt: row.get(1)?,
                        status: row.get(2)?,
                        accepted_by_user: row
                            .get::<_, Option<i64>>(3)?
                            .map(|accepted| accepted == 1),
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(MemoryProvenance {
                epistemic_state,
                confidence,
                trust_source,
                rationale,
                evidence,
                episodes,
            })
        })
    })
    .await?;
    Ok(Json(provenance))
}

async fn origin_refresh_handler(
    State(state): State<DaemonState>,
    Json(request): Json<OriginRefreshRequest>,
) -> std::result::Result<Json<OriginRefreshResponse>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let project = registry
        .find_by_id(&request.project_id)
        .map_err(|error| ApiError::from_core(&error))?
        .ok_or_else(|| {
            ApiError::from_core(&Error::Config(format!(
                "unknown project `{}`",
                request.project_id
            )))
        })?;
    let repo = registry
        .alias_paths(&request.project_id)
        .map_err(|error| ApiError::from_core(&error))?
        .into_iter()
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            ApiError::from_core(&Error::Config(
                "this project has no known local checkout".to_owned(),
            ))
        })?;
    let state_dir = state
        .config
        .storage
        .db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("worktrees");
    let reference = refresh_origin_view(
        &state.config.daemon.socket_path,
        &state.database,
        &state_dir,
        &request.project_id,
        &project.name,
        &repo,
    )
    .await
    .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(OriginRefreshResponse { reference }))
}

#[allow(clippy::too_many_lines)]
async fn project_module_symbols(
    State(state): State<DaemonState>,
    Json(request): Json<ModuleSymbolsRequest>,
) -> std::result::Result<Json<ModuleSymbols>, ApiError> {
    if request.group.len() > 200 || request.group.contains('%') || request.group.contains('_') {
        // The group feeds a LIKE prefix; module labels never contain
        // wildcards, so reject rather than escape.
        return Err(ApiError::from_core(&Error::Config(
            "invalid module group".to_owned(),
        )));
    }
    let database = state.database.clone();
    let result = run_blocking(&state, move || {
        let prefix = format!("{}/%", request.group);
        database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT kind, label, file_path FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                   AND kind IN ('Function', 'Type', 'Interface', 'Module')
                   AND file_path IS NOT NULL
                   AND (file_path LIKE ?2 OR file_path = ?3)
                 ORDER BY kind, label LIMIT 60",
            )?;
            let symbols = statement
                .query_map(
                    rusqlite::params![request.project_id.as_str(), prefix, request.group],
                    |row| {
                        Ok(ModuleSymbol {
                            kind: row.get(0)?,
                            label: row.get(1)?,
                            file_path: row.get(2)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut memory_statement = connection.prepare(
                "SELECT DISTINCT m.id, m.kind, m.claim, m.confidence, m.tags_json
                 FROM memories m
                 JOIN memory_nodes mn ON mn.memory_id = m.id
                 JOIN nodes n ON n.id = mn.node_id
                 WHERE m.project_id = ?1 AND m.status = 'active'
                   AND m.epistemic_state = 'validated'
                   AND (n.file_path LIKE ?2 OR n.file_path = ?3)
                 ORDER BY m.known_from_ms DESC LIMIT 20",
            )?;
            let memories = memory_statement
                .query_map(
                    rusqlite::params![request.project_id.as_str(), prefix, request.group],
                    |row| {
                        Ok(MemorySummary {
                            id: row.get(0)?,
                            kind: row.get(1)?,
                            claim: row.get(2)?,
                            confidence: row.get(3)?,
                            tags: serde_json::from_str(&row.get::<_, String>(4)?)
                                .unwrap_or_default(),
                            epistemic_state: "validated".to_owned(),
                            wins: 0,
                            injections: 0,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let (files, functions, types) = connection.query_row(
                "SELECT COUNT(DISTINCT file_path),
                        SUM(kind = 'Function'), SUM(kind IN ('Type', 'Interface'))
                 FROM nodes
                 WHERE project_id = ?1 AND tombstoned_at_ms IS NULL
                   AND (file_path LIKE ?2 OR file_path = ?3)",
                rusqlite::params![request.project_id.as_str(), prefix, request.group],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    ))
                },
            )?;
            let summary = format!(
                "{files} file{} · {functions} function{} · {types} type{}",
                if files == 1 { "" } else { "s" },
                if functions == 1 { "" } else { "s" },
                if types == 1 { "" } else { "s" },
            );
            let noise = CALL_NOISE
                .iter()
                .map(|word| format!("'{word}'"))
                .collect::<Vec<_>>()
                .join(",");
            let mut calls_statement = connection.prepare(&format!(
                "SELECT t.label, COUNT(*) AS n FROM edges e
                 JOIN nodes s ON s.id = e.source_id
                    AND (s.file_path LIKE ?2 OR s.file_path = ?3)
                 JOIN nodes t ON t.id = e.target_id AND t.kind = 'ExternalReference'
                 WHERE e.project_id = ?1 AND e.relation = 'CALLS'
                   AND e.tombstoned_at_ms IS NULL
                   AND t.label NOT IN ({noise})
                   AND length(t.label) > 2
                 GROUP BY t.label ORDER BY n DESC, t.label LIMIT 15"
            ))?;
            let calls = calls_statement
                .query_map(
                    rusqlite::params![request.project_id.as_str(), prefix, request.group],
                    |row| {
                        Ok(CalledFunction {
                            label: row.get(0)?,
                            calls: row.get(1)?,
                        })
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            // Resolved cross-module dependencies, both directions, scoped to
            // this one module so the join stays cheap.
            let module_of = |path: &str| -> String {
                let mut parts = path.split('/').filter(|part| !part.is_empty());
                match (parts.next(), parts.next()) {
                    (Some(first), Some(second)) if second.contains('.') => first.to_owned(),
                    (Some(first), Some(second)) => format!("{first}/{second}"),
                    (Some(first), None) => first.to_owned(),
                    _ => "(root)".to_owned(),
                }
            };
            let direction = |outbound: bool| -> std::result::Result<Vec<String>, Error> {
                // Both directions start from this module's own nodes and
                // resolve each distinct label once through
                // `nodes_project_label`, so the cost follows the module, not
                // the project's edge count. Driving from the project's edges
                // instead took 1.3-1.7 s per call with pinned indexes, and up
                // to 74 s unpinned, on a 50k-edge index with Python calls.
                // As in the file shape, only names defined in one file count.
                let sql = if outbound {
                    format!(
                        "WITH targets AS MATERIALIZED (
                             SELECT DISTINCT t.label FROM nodes s INDEXED BY nodes_project_file
                             CROSS JOIN edges e INDEXED BY edges_source_active
                                ON e.project_id = s.project_id AND e.source_id = s.id
                             CROSS JOIN nodes t ON t.id = e.target_id
                             WHERE s.project_id = ?1 AND (s.file_path LIKE ?2 OR s.file_path = ?3)
                               AND e.tombstoned_at_ms IS NULL AND t.kind = 'ExternalReference')
                         SELECT DISTINCT d.file_path FROM targets
                         CROSS JOIN nodes d INDEXED BY nodes_project_label
                            ON d.project_id = ?1 AND d.label = targets.label
                         WHERE d.kind IN ('Function', 'Type', 'Module', 'Interface')
                           AND d.tombstoned_at_ms IS NULL AND d.file_path IS NOT NULL
                           AND {only} LIMIT 400",
                        only = crate::vault::defined_in_one_file("d"),
                    )
                } else {
                    format!(
                        "WITH defined AS MATERIALIZED (
                             SELECT DISTINCT d.label FROM nodes d INDEXED BY nodes_project_file
                             WHERE d.project_id = ?1 AND (d.file_path LIKE ?2 OR d.file_path = ?3)
                               AND d.kind IN ('Function', 'Type', 'Module', 'Interface')
                               AND d.tombstoned_at_ms IS NULL AND {only})
                         SELECT DISTINCT s.file_path FROM defined
                         CROSS JOIN nodes t INDEXED BY nodes_project_label
                            ON t.project_id = ?1 AND t.label = defined.label
                         CROSS JOIN edges e INDEXED BY edges_target_active
                            ON e.project_id = ?1 AND e.target_id = t.id
                         CROSS JOIN nodes s ON s.id = e.source_id
                         WHERE t.kind = 'ExternalReference' AND e.tombstoned_at_ms IS NULL
                           AND s.file_path IS NOT NULL LIMIT 400",
                        only = crate::vault::defined_in_one_file("d"),
                    )
                };
                let mut statement = connection.prepare(&sql)?;
                let rows = statement
                    .query_map(
                        rusqlite::params![request.project_id.as_str(), prefix, request.group],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let mut groups = rows
                    .iter()
                    .map(|path| module_of(path))
                    .filter(|group| group != &request.group)
                    .collect::<Vec<_>>();
                groups.sort();
                groups.dedup();
                groups.truncate(10);
                Ok(groups)
            };
            let depends_on = direction(true)?;
            let used_by = direction(false)?;
            Ok(ModuleSymbols {
                symbols,
                memories,
                summary,
                calls,
                depends_on,
                used_by,
            })
        })
    })
    .await?;
    Ok(Json(result))
}

async fn sync_export_handler(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<crate::sync::SyncKnowledge>, ApiError> {
    let database = state.database.clone();
    let knowledge = run_blocking(&state, move || crate::sync::export_knowledge(&database)).await?;
    Ok(Json(knowledge))
}

async fn sync_offers_handler(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<crate::sync::SyncKnowledge>, ApiError> {
    let database = state.database.clone();
    let offers = run_blocking(&state, move || crate::sync::collect_offers(&database)).await?;
    Ok(Json(offers))
}

async fn sync_apply_handler(
    State(state): State<DaemonState>,
    Json(knowledge): Json<crate::sync::SyncKnowledge>,
) -> std::result::Result<Json<crate::sync::SyncApplyReport>, ApiError> {
    let database = state.database.clone();
    let redactor = state.redactor.clone();
    let handle = state
        .scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            crate::sync::apply_knowledge(&database, &redactor, &knowledge)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn sync_accept_handler(
    State(state): State<DaemonState>,
    Json(offers): Json<crate::sync::SyncKnowledge>,
) -> std::result::Result<Json<crate::sync::OfferReport>, ApiError> {
    let database = state.database.clone();
    let redactor = state.redactor.clone();
    let handle = state
        .scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            crate::sync::accept_offers(&database, &redactor, &offers)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn workspace_activity(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<Vec<ActivityEvent>>, ApiError> {
    let database = state.database.clone();
    let events = run_blocking(&state, move || {
        database.with_reader(|connection| {
            let mut statement = connection.prepare(
                "SELECT e.event_type, p.name, e.created_at_ms
                 FROM events e
                 LEFT JOIN projects p ON p.id = e.project_id
                 WHERE e.project_id IS NULL OR NOT EXISTS (
                     SELECT 1 FROM project_facts pf WHERE pf.project_id = e.project_id
                       AND (pf.key = 'internal.view_of'
                            OR (pf.key = 'internal.hidden' AND pf.value = 'true')))
                 ORDER BY e.sequence DESC LIMIT 60",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ActivityEvent {
                        event_type: row.get(0)?,
                        project_name: row.get(1)?,
                        created_at_ms: row.get(2)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    })
    .await?;
    Ok(Json(events))
}

async fn architect_dossier(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<String>, ApiError> {
    let database = state.database.clone();
    let dossier = run_blocking(&state, move || crate::architect::build_dossier(&database)).await?;
    Ok(Json(dossier))
}

async fn set_fact(
    State(state): State<DaemonState>,
    Json(request): Json<FactSetRequest>,
) -> std::result::Result<Json<crate::projects::ProjectFact>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.set_fact(
                &request.project_id,
                &request.key,
                &request.value,
                request.note.as_deref(),
            )
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn unset_fact(
    State(state): State<DaemonState>,
    Json(request): Json<FactUnsetRequest>,
) -> std::result::Result<Json<FactUnsetResponse>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.unset_fact(&request.project_id, &request.key)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let removed = wait_for_mutation(&state, handle).await?;
    Ok(Json(FactUnsetResponse { removed }))
}

async fn list_facts(
    State(state): State<DaemonState>,
    Json(request): Json<FactListRequest>,
) -> std::result::Result<Json<Vec<crate::projects::ProjectFact>>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let facts = run_blocking(&state, move || registry.facts(&request.project_id)).await?;
    Ok(Json(facts))
}

async fn scan_links(
    State(state): State<DaemonState>,
    Json(request): Json<LinkScanRequest>,
) -> std::result::Result<Json<crate::linkscan::LinkScanReport>, ApiError> {
    let registry = ProjectRegistry::new(state.database.clone());
    let path = request.path;
    let resolve = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.resolve_or_register(&path)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let resolved = wait_for_mutation(&state, resolve).await?;
    let database = state.database.clone();
    let project_id = resolved.project.id.clone();
    let root = resolved.root.clone();
    let candidates = run_blocking(&state, move || {
        crate::linkscan::scan(&database, &project_id, &root)
    })
    .await?;
    let applied = if request.apply {
        let database = state.database.clone();
        let project_id = resolved.project.id.clone();
        let to_apply = candidates.clone();
        let handle = state
            .scheduler
            .submit(MutationPriority::Interactive, move || {
                crate::linkscan::apply_definitive(&database, &project_id, &to_apply)
            })
            .map_err(|error| ApiError::from_core(&error))?;
        wait_for_mutation(&state, handle).await?
    } else {
        Vec::new()
    };
    Ok(Json(crate::linkscan::LinkScanReport {
        candidates,
        applied,
    }))
}

async fn build_vault(
    State(state): State<DaemonState>,
    Json(request): Json<VaultBuildRequest>,
) -> std::result::Result<Json<crate::vault::VaultReport>, ApiError> {
    if !state.config.vault.enabled {
        return Err(ApiError::from_core(&Error::Config(
            "vault export is disabled; set vault.enabled = true".to_owned(),
        )));
    }
    let builder =
        crate::vault::VaultBuilder::new(state.database.clone(), state.config.vault.root.clone());
    // Full-workspace exports run the cross-module edge-resolution query for
    // every project and legitimately outlive the default request deadline;
    // the CLI pairs this with an equally patient client.
    let deadline =
        Duration::from_millis(state.config.daemon.request_timeout_ms).max(Duration::from_secs(600));
    let report = run_blocking_with_timeout(&state, deadline, move || match &request.project_id {
        Some(project_id) => builder.build_project(project_id),
        None => builder.build_all(),
    })
    .await?;
    Ok(Json(report))
}

async fn create_memory(
    State(state): State<DaemonState>,
    Json(request): Json<MemoryCreateRequest>,
) -> std::result::Result<Json<MemoryRecord>, ApiError> {
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.create_candidate(&request.project_id, &request.memory)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn list_memories(
    State(state): State<DaemonState>,
    Json(request): Json<MemoryListRequest>,
) -> std::result::Result<Json<Vec<MemoryRecord>>, ApiError> {
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let memories = run_blocking(&state, move || {
        store.list(&request.project_id, request.kind.as_deref(), request.limit)
    })
    .await?;
    Ok(Json(memories))
}

async fn get_memory(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<Option<MemoryRecord>>, ApiError> {
    let id = MemoryId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let memory = run_blocking(&state, move || store.get(&id)).await?;
    Ok(Json(memory))
}

async fn promote_memory(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<MemoryPromoteRequest>,
) -> std::result::Result<Json<MemoryRecord>, ApiError> {
    let id = MemoryId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let target = request.target;
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.promote(&id, request.target, request.human_override)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let memory = wait_for_mutation(&state, handle).await?;
    if target == EpistemicState::Rejected {
        state.metrics.count_memory_rejection();
    } else {
        state.metrics.count_memory_promotion();
    }
    Ok(Json(memory))
}

async fn purge_memory(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<()>, ApiError> {
    let id = MemoryId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            store.purge(&id)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    wait_for_mutation(&state, handle).await?;
    state.metrics.count_memory_purge();
    Ok(Json(()))
}

async fn forget_memory(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<MemoryRecord>, ApiError> {
    let id = MemoryId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            store.mark_status(&id, crate::memory::MemoryStatus::Archived, "user_forget")
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn restore_memory_drift(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<MemoryRestoreDriftRequest>,
) -> std::result::Result<Json<MemoryRecord>, ApiError> {
    let id = MemoryId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = MemoryStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            store.restore_symbol_drift(&id, request.expected_event_sequence)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn purge_task_prompt(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let task_id = TaskId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = ExperienceStore::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::CorrectnessCritical, move || {
            store.purge_task_prompt(&task_id)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    wait_for_mutation(&state, handle).await?;
    Ok(Json(
        serde_json::json!({"purged": true, "scope": "task_prompt",
        "backup_limitation": "existing backups and exports are not modified"}),
    ))
}

async fn start_task(
    State(state): State<DaemonState>,
    Json(request): Json<TaskStartRequest>,
) -> std::result::Result<Json<TaskRecord>, ApiError> {
    let store = ExperienceStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.start_task_with_context(
                &request.project_id,
                &request.prompt,
                request.session_id.as_deref(),
                request.context_request_id.as_deref(),
            )
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn record_attempt(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<AttemptCreateRequest>,
) -> std::result::Result<Json<AttemptRecord>, ApiError> {
    let task_id = TaskId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = ExperienceStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.record_attempt(
                &task_id,
                &request.strategy_summary,
                request.outcome,
                &request.files,
            )
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn record_evidence(
    State(state): State<DaemonState>,
    Json(request): Json<EvidenceCreateRequest>,
) -> std::result::Result<Json<EvidenceRecord>, ApiError> {
    // Client metadata cannot claim native-hook provenance.
    let metadata = crate::agenthooks::evidence_metadata(request.metadata.clone(), None);
    store_evidence(state, request, metadata).await
}

async fn record_hook_evidence(
    State(state): State<DaemonState>,
    Json(request): Json<HookEvidenceRequest>,
) -> std::result::Result<Json<EvidenceRecord>, ApiError> {
    let metadata = crate::agenthooks::evidence_metadata(
        request.evidence.metadata.clone(),
        Some(&request.provenance),
    );
    store_evidence(state, request.evidence, metadata).await
}

async fn store_evidence(
    state: DaemonState,
    request: EvidenceCreateRequest,
    metadata: serde_json::Value,
) -> std::result::Result<Json<EvidenceRecord>, ApiError> {
    let store = evidence_store(&state);
    let handle = state
        .scheduler
        .submit(MutationPriority::EvidenceVerification, move || {
            store.record_evidence(
                &request.project_id,
                request.task_id.as_ref(),
                request.attempt_id.as_ref(),
                &request.kind,
                request.strength,
                request.commit_hash.as_deref(),
                request.command.as_deref(),
                request.exit_code,
                request.artifact_text.as_bytes(),
                &metadata,
            )
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let evidence = wait_for_mutation(&state, handle).await?;
    state.metrics.count_evidence_record();
    Ok(Json(evidence))
}

async fn get_evidence(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
) -> std::result::Result<Json<Option<EvidenceRecord>>, ApiError> {
    let id = EvidenceId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = evidence_store(&state);
    let evidence = run_blocking(&state, move || store.get_evidence(&id)).await?;
    Ok(Json(evidence))
}

/// Experience store configured with this daemon's redaction and blob routing.
fn evidence_store(state: &DaemonState) -> ExperienceStore {
    let threshold = usize::try_from(state.config.storage.blob_threshold_bytes)
        .unwrap_or(DEFAULT_BLOB_THRESHOLD_BYTES);
    ExperienceStore::with_redactor(state.database.clone(), state.redactor.clone())
        .with_blob_threshold(threshold)
}

async fn finish_task(
    State(state): State<DaemonState>,
    AxumPath(id): AxumPath<String>,
    Json(request): Json<TaskFinishRequest>,
) -> std::result::Result<Json<TaskRecord>, ApiError> {
    let task_id = TaskId::parse(&id).map_err(|error| ApiError::from_core(&error))?;
    let store = ExperienceStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.finish_task(
                &task_id,
                request.outcome,
                request.accepted_by_user,
                request.final_commit.as_deref(),
            )
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let finished = wait_for_mutation(&state, handle).await?;
    // Opt-in only (spec P9): a successful close may trigger one bounded
    // synthesis pass at background priority. The finish itself has committed,
    // so a failed or shed synthesis pass never fails this response.
    if state.config.synthesis.auto && request.outcome == TaskOutcome::Success {
        let options = SynthesisOptions::from(&state.config.synthesis);
        let store = SynthesisStore::with_redactor(state.database.clone(), state.redactor.clone());
        let project_id = finished.project_id.clone();
        if let Ok(handle) = state
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                store.synthesize(&project_id, &options)
            })
        {
            let _ = wait_for_mutation(&state, handle).await;
        }
    }
    Ok(Json(finished))
}

async fn past_failures(
    State(state): State<DaemonState>,
    Json(request): Json<PastFailuresRequest>,
) -> std::result::Result<Json<Vec<PastFailure>>, ApiError> {
    let store = ExperienceStore::with_redactor(state.database.clone(), state.redactor.clone());
    let failures = run_blocking(&state, move || {
        store.past_failures(&request.project_id, request.query.as_deref(), request.limit)
    })
    .await?;
    Ok(Json(failures))
}

async fn record_evaluation(
    State(state): State<DaemonState>,
    Json(record): Json<EvaluationRecord>,
) -> std::result::Result<Json<()>, ApiError> {
    admit_work(&state, MutationPriority::Analytics)?;
    let store = EvaluationStore::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Analytics, move || store.record(&record))
        .map_err(|error| ApiError::from_core(&error))?;
    wait_for_mutation(&state, handle).await?;
    Ok(Json(()))
}

async fn distill(
    State(state): State<DaemonState>,
    Json(request): Json<DistillationRequest>,
) -> std::result::Result<Json<DistillationResult>, ApiError> {
    admit_work(&state, MutationPriority::BackgroundLearning)?;
    let store = LearningStore::with_redactor(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            store.distill(&request)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn run_synthesis(
    State(state): State<DaemonState>,
    Json(request): Json<SynthesisRunRequest>,
) -> std::result::Result<Json<SynthesisReport>, ApiError> {
    admit_work(&state, MutationPriority::BackgroundLearning)?;
    let registry = ProjectRegistry::new(state.database.clone());
    let path = request.path;
    let resolve = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.resolve_or_register(&path)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let resolved = wait_for_mutation(&state, resolve).await?;
    let scoped =
        scoped_config(&state, &resolved.root).map_err(|error| ApiError::from_core(&error))?;
    let options = SynthesisOptions::from(&scoped.synthesis);
    let store = SynthesisStore::with_redactor(state.database.clone(), state.redactor.clone());
    let project_id = resolved.project.id;
    let handle = state
        .scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            store.synthesize(&project_id, &options)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn promote_skill(
    State(state): State<DaemonState>,
    Json(request): Json<SkillPromoteRequest>,
) -> std::result::Result<Json<crate::memory::MemoryRecord>, ApiError> {
    admit_work(&state, MutationPriority::BackgroundLearning)?;
    let registry = ProjectRegistry::new(state.database.clone());
    let path = request.path;
    let resolve = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            registry.resolve_or_register(&path)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let resolved = wait_for_mutation(&state, resolve).await?;
    let scoped =
        scoped_config(&state, &resolved.root).map_err(|error| ApiError::from_core(&error))?;
    let min_distinct_uses = scoped.synthesis.min_distinct_uses;
    let store = SynthesisStore::with_redactor(state.database.clone(), state.redactor.clone());
    let memory_id = request.memory_id;
    let handle = state
        .scheduler
        .submit(MutationPriority::BackgroundLearning, move || {
            store.promote_to_skill(&memory_id, min_distinct_uses)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn wait_for_mutation<T>(
    _state: &DaemonState,
    handle: crate::scheduler::MutationHandle<T>,
) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
{
    tokio::task::spawn_blocking(move || handle.wait())
        .await
        .map_err(|error| {
            ApiError::from_core(&Error::Daemon(format!("mutation waiter failed: {error}")))
        })?
        .map_err(|error| ApiError::from_core(&error))
}

async fn run_blocking<T>(
    state: &DaemonState,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
{
    let deadline = Duration::from_millis(state.config.daemon.request_timeout_ms);
    run_blocking_with_timeout(state, deadline, operation).await
}

async fn run_blocking_with_timeout<T>(
    state: &DaemonState,
    deadline: Duration,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
{
    let started = Instant::now();
    let permit = tokio::time::timeout(deadline, Arc::clone(&state.blocking_slots).acquire_owned())
        .await
        .map_err(|_| ApiError::timeout())?
        .map_err(|_| {
            ApiError::from_core(&Error::Daemon(
                "blocking-work semaphore closed unexpectedly".to_owned(),
            ))
        })?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        operation()
    });
    let remaining = deadline.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(ApiError::timeout());
    }
    let result = tokio::time::timeout(remaining, task)
        .await
        .map_err(|_| ApiError::timeout())?
        .map_err(|error| {
            ApiError::from_core(&Error::Daemon(format!("blocking task failed: {error}")))
        })?;
    result.map_err(|error| ApiError::from_core(&error))
}

async fn run_background<T>(
    state: &DaemonState,
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> std::result::Result<T, ApiError>
where
    T: Send + 'static,
{
    let deadline = Duration::from_millis(state.config.daemon.request_timeout_ms);
    let started = Instant::now();
    let permit = tokio::time::timeout(
        deadline,
        Arc::clone(&state.background_slots).acquire_owned(),
    )
    .await
    .map_err(|_| ApiError::timeout())?
    .map_err(|_| {
        ApiError::from_core(&Error::Daemon(
            "background-work semaphore closed unexpectedly".to_owned(),
        ))
    })?;
    let remaining = deadline.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(ApiError::timeout());
    }
    run_blocking_with_timeout(state, remaining, move || {
        let _permit = permit;
        operation()
    })
    .await
}

fn admit_work(
    state: &DaemonState,
    priority: MutationPriority,
) -> std::result::Result<(), ApiError> {
    let Some(rss_bytes) = current_rss_bytes() else {
        return Ok(());
    };
    let soft = mebibytes_to_bytes(state.config.daemon.rss_soft_limit_mb);
    let hard = mebibytes_to_bytes(state.config.daemon.rss_hard_limit_mb);
    let blocked_limit = if rss_bytes >= hard && priority < MutationPriority::CorrectnessCritical {
        Some(hard)
    } else if rss_bytes >= soft
        && priority <= MutationPriority::ChangedFileRefresh
        && priority != MutationPriority::EvidenceVerification
    {
        Some(soft)
    } else {
        None
    };
    if let Some(limit_bytes) = blocked_limit {
        return Err(ApiError::from_core(&Error::ResourcePressure {
            rss_bytes,
            limit_bytes,
        }));
    }
    Ok(())
}

const fn mebibytes_to_bytes(mebibytes: u32) -> u64 {
    mebibytes as u64 * 1_024 * 1_024
}

fn classify_memory_pressure(rss_bytes: Option<u64>, soft: u64, hard: u64) -> MemoryPressure {
    match rss_bytes {
        None => MemoryPressure::Unknown,
        Some(rss) if rss >= hard => MemoryPressure::Hard,
        Some(rss) if rss >= soft => MemoryPressure::Soft,
        Some(_) => MemoryPressure::Normal,
    }
}

fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        if let Some(kibibytes) = status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        }) {
            return kibibytes.checked_mul(1_024);
        }
    }

    let output = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    std::str::from_utf8(&output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        .checked_mul(1_024)
}

struct BoundedUnixListener {
    inner: UnixListener,
    permits: Arc<Semaphore>,
}

struct PermittedUnixStream {
    inner: UnixStream,
    _permit: OwnedSemaphorePermit,
}

impl axum::serve::Listener for BoundedUnixListener {
    type Io = PermittedUnixStream;
    type Addr = tokio::net::unix::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("daemon connection semaphore is never closed");
        loop {
            match self.inner.accept().await {
                Ok((inner, address)) => {
                    return (
                        PermittedUnixStream {
                            inner,
                            _permit: permit,
                        },
                        address,
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

impl AsyncRead for PermittedUnixStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for PermittedUnixStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(context, buffer)
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;

        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.dev() == self.device && metadata.ino() == self.inode {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn bind_socket(path: &Path, max_connections: usize) -> Result<(BoundedUnixListener, SocketGuard)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                return Err(Error::Daemon(format!(
                    "refusing to replace non-socket path {}",
                    path.display()
                )));
            }
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => {
                    return Err(Error::Daemon(format!(
                        "another daemon is already listening at {}",
                        path.display()
                    )));
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(path).map_err(|source| io_error(path, source))?;
                }
                Err(source) => return Err(io_error(path, source)),
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(path, source)),
    }

    let inner = UnixListener::bind(path).map_err(|source| io_error(path, source))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error(path, source))?;
    let metadata = std::fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    let guard = SocketGuard {
        path: path.to_path_buf(),
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    Ok((
        BoundedUnixListener {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
        },
        guard,
    ))
}

/// Refresh (or create) a project's shared-truth origin/main view: fetch,
/// materialize a throwaway worktree, pin a SHADOW project identity into it,
/// and index that. The developer's checkout and the project's own (local)
/// graph are untouched — the two views never overwrite each other.
///
/// # Errors
///
/// Returns git, transport, or storage errors.
pub async fn refresh_origin_view(
    socket_path: &Path,
    database: &Database,
    state_dir: &Path,
    project_id: &ProjectId,
    project_name: &str,
    repo_path: &Path,
) -> Result<String> {
    let registry = ProjectRegistry::new(database.clone());
    let existing = registry
        .facts(project_id)?
        .into_iter()
        .find(|fact| fact.key == "internal.origin_view")
        .and_then(|fact| ProjectId::parse(&fact.value).ok());
    let shadow_id = existing.clone().unwrap_or_else(ProjectId::new);

    let repo = repo_path.to_path_buf();
    let worktrees = state_dir.to_path_buf();
    let worktree = tokio::task::spawn_blocking(move || {
        crate::gitref::checkout_origin_default(&repo, &worktrees)
    })
    .await
    .map_err(|error| Error::Daemon(format!("worktree task: {error}")))??;
    let reference = worktree.reference.clone();
    let metadata_dir = worktree.path().join(".engram");
    std::fs::create_dir_all(&metadata_dir)
        .map_err(|error| Error::Daemon(format!("shadow metadata dir: {error}")))?;
    std::fs::write(
        metadata_dir.join("project.toml"),
        format!(
            "project_id = \"{shadow_id}\"\nname = \"{project_name}@origin\"\nderived_view = true\n"
        ),
    )
    .map_err(|error| Error::Daemon(format!("shadow metadata: {error}")))?;

    let client = DaemonClient::new(socket_path, std::time::Duration::from_secs(300));
    client.refresh_index(worktree.path()).await?;
    if existing.is_none() {
        client
            .set_fact(&FactSetRequest {
                project_id: project_id.clone(),
                key: "internal.origin_view".to_owned(),
                value: shadow_id.to_string(),
                note: Some("shared-truth view marker (managed by Engram)".to_owned()),
            })
            .await?;
        client
            .set_fact(&FactSetRequest {
                project_id: shadow_id.clone(),
                key: "internal.view_of".to_owned(),
                value: project_id.to_string(),
                note: Some("origin/main snapshot of the real project".to_owned()),
            })
            .await?;
    }
    drop(worktree);
    Ok(reference)
}

/// Automatic cross-instance sync settings for the watcher.
struct SyncAuto {
    central_url: String,
    token: Option<String>,
    every: std::time::Duration,
    push: bool,
}

/// Automatic architecture-guide regeneration settings for the watcher.
struct GuideAuto {
    mode: String,
    min_interval: std::time::Duration,
    runtime_url: String,
    model: String,
    local_only: bool,
    vault_root: PathBuf,
    vault_enabled: bool,
}

/// Regenerate the guide when the workspace's STABLE projection has drifted
/// past the stamped prose. Rate-limited, and every failure is a logged
/// retry-next-tick, never a crash.
async fn maybe_regenerate_guide(
    guide: &GuideAuto,
    database: &Database,
    client: &DaemonClient,
) -> bool {
    if guide.mode == "off" {
        return false;
    }
    let store = crate::product::ProductStore::new(database.clone(), Redactor::default());
    let Ok(value) = store.query(&crate::product::Query::Guide) else {
        return false;
    };
    let Ok(Some(record)) = serde_json::from_value::<Option<crate::product::Record>>(value) else {
        return false;
    };
    if record.archived {
        return false;
    }
    let crate::product::Item::Guide(existing) = record.item else {
        return false;
    };
    let stamped = existing.dossier_digest;
    let dossier = {
        let database = database.clone();
        match tokio::task::spawn_blocking(move || crate::architect::build_dossier(&database)).await
        {
            Ok(Ok(dossier)) => dossier,
            _ => return false,
        }
    };
    if crate::architect::stable_digest(&dossier) == stamped {
        return false;
    }
    let prompt = crate::architect::synthesis_prompt(&dossier);
    let (model, synthesized) = if guide.mode == "claude" {
        let model = if guide.model.is_empty() {
            "claude-opus-4-8".to_owned()
        } else {
            guide.model.clone()
        };
        let cli_model = model.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::architect::synthesize_with_claude_cli(&cli_model, &prompt)
        })
        .await;
        match result {
            Ok(Ok(text)) => (model, text),
            Ok(Err(error)) => {
                eprintln!("guide auto-regen (claude) failed: {error}");
                return false;
            }
            Err(error) => {
                eprintln!("guide auto-regen task failed: {error}");
                return false;
            }
        }
    } else {
        if crate::architect::enforce_local_only(&guide.runtime_url, guide.local_only).is_err()
            || guide.model.is_empty()
        {
            eprintln!("guide auto-regen (local) needs architect.model and a loopback runtime");
            return false;
        }
        match crate::architect::synthesize(&guide.runtime_url, &guide.model, &prompt).await {
            Ok(text) => (guide.model.clone(), text),
            Err(error) => {
                eprintln!("guide auto-regen (local) failed: {error}");
                return false;
            }
        }
    };
    let document = crate::architect::guide_document(&model, &dossier, &synthesized);
    let saved = client
        .product_mutate(&crate::product::Mutation::Save {
            id: Some(record.id),
            expected_version: record.version,
            item: Box::new(crate::product::Item::Guide(crate::product::Guide {
                content: document.clone(),
                dossier_digest: crate::architect::stable_digest(&dossier),
                model: model.clone(),
            })),
        })
        .await;
    if let Err(error) = saved {
        eprintln!("guide auto-regeneration save failed: {error}");
        return false;
    }
    if guide.vault_enabled {
        let path = guide.vault_root.join("ARCHITECTURE.md");
        if let Err(error) = std::fs::create_dir_all(&guide.vault_root)
            .and_then(|()| std::fs::write(&path, &document))
        {
            eprintln!("optional guide export failed: {error}");
        }
    }
    eprintln!(
        "guide auto-regenerated with `{model}` ({} bytes)",
        document.len()
    );
    true
}

/// Commit-driven auto refresh. Compares committed HEADs only — uncommitted
/// edits never trigger a scan — and, on a slower cadence, fetches origin to
/// keep the shared-truth views current.
#[allow(clippy::too_many_lines)]
async fn watch_loop(
    socket_path: PathBuf,
    database: Database,
    state_dir: PathBuf,
    local_poll: std::time::Duration,
    origin_poll: Option<std::time::Duration>,
    sync: SyncAuto,
    guide: GuideAuto,
) {
    let client = DaemonClient::new(&socket_path, std::time::Duration::from_secs(300));
    let mut last_origin = std::time::Instant::now();
    let mut last_sync = std::time::Instant::now();
    // First guide check fires shortly after startup rather than a full
    // interval later: daemon restarts must not defer catching real drift.
    let mut last_guide = std::time::Instant::now()
        .checked_sub(
            guide
                .min_interval
                .saturating_sub(std::time::Duration::from_secs(60)),
        )
        .unwrap_or_else(std::time::Instant::now);
    // Crash hygiene: a refresh interrupted mid-flight leaves a dangling
    // worktree directory plus stale worktree metadata in the source repos.
    // Clean both once at startup; every live refresh cleans up after itself.
    {
        let cleanup_dir = state_dir.clone();
        let cleanup_client = DaemonClient::new(&socket_path, std::time::Duration::from_secs(60));
        let _ = std::fs::remove_dir_all(&cleanup_dir);
        if let Ok(snapshot) = cleanup_client.workspace().await {
            for project in &snapshot.projects {
                if let Some(path) = project.primary_path.as_deref() {
                    let repo = PathBuf::from(path);
                    let _ = tokio::task::spawn_blocking(move || {
                        std::process::Command::new("git")
                            .arg("-C")
                            .arg(&repo)
                            .args(["worktree", "prune"])
                            .output()
                    })
                    .await;
                }
            }
        }
    }
    loop {
        tokio::time::sleep(local_poll).await;
        let Ok(snapshot) = client.workspace().await else {
            continue;
        };
        for project in &snapshot.projects {
            let Some(path) = project.primary_path.as_deref() else {
                continue;
            };
            let repo = PathBuf::from(path);
            let repo_probe = repo.clone();
            let Ok(Ok(head)) =
                tokio::task::spawn_blocking(move || crate::gitref::head_commit(&repo_probe)).await
            else {
                continue;
            };
            if project.indexed_head.as_deref() != Some(head.as_str()) {
                eprintln!(
                    "watch: {} committed HEAD moved to {} - refreshing index",
                    project.name,
                    &head[..head.len().min(10)]
                );
                let _ = client.refresh_index(&repo).await;
            }
        }
        if !sync.central_url.is_empty()
            && sync.every > std::time::Duration::ZERO
            && last_sync.elapsed() >= sync.every
        {
            last_sync = std::time::Instant::now();
            match crate::sync::run_sync_cycle(
                &client,
                &sync.central_url,
                sync.token.as_deref(),
                true,
                sync.push,
            )
            .await
            {
                Ok(report) => {
                    if let Some(pulled) = &report.pulled
                        && (pulled.memories_added > 0 || pulled.memories_updated > 0)
                    {
                        eprintln!(
                            "sync: pulled {} new / {} updated memories from central",
                            pulled.memories_added, pulled.memories_updated
                        );
                    }
                }
                Err(error) => eprintln!("sync: cycle failed: {error}"),
            }
        }
        if last_guide.elapsed() >= guide.min_interval {
            last_guide = std::time::Instant::now();
            let _ = maybe_regenerate_guide(&guide, &database, &client).await;
        }
        if let Some(origin_poll) = origin_poll {
            if last_origin.elapsed() >= origin_poll {
                last_origin = std::time::Instant::now();
                for project in &snapshot.projects {
                    let (Some(path), Some(view)) = (
                        project.primary_path.as_deref(),
                        project.origin_view.as_ref(),
                    ) else {
                        continue;
                    };
                    let repo = PathBuf::from(path);
                    let probe = repo.clone();
                    let Ok(Ok(remote)) = tokio::task::spawn_blocking(move || {
                        let _ = crate::gitref::fetch_origin(&probe);
                        crate::gitref::origin_commit(&probe)
                    })
                    .await
                    else {
                        continue;
                    };
                    if view.indexed_head.as_deref() != Some(remote.as_str()) {
                        eprintln!(
                            "watch: {} origin default moved to {} - refreshing shared-truth view",
                            project.name,
                            &remote[..remote.len().min(10)]
                        );
                        let _ = refresh_origin_view(
                            &socket_path,
                            &database,
                            &state_dir,
                            &project.id,
                            &project.name,
                            &repo,
                        )
                        .await;
                    }
                }
            }
        }
    }
}

async fn product_query(
    State(state): State<DaemonState>,
    Json(request): Json<crate::product::Query>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let store = crate::product::ProductStore::new(state.database.clone(), state.redactor.clone());
    Ok(Json(
        run_blocking(&state, move || store.query(&request)).await?,
    ))
}

async fn effectiveness_report(
    State(state): State<DaemonState>,
    Json(request): Json<crate::effectiveness::EffectivenessRequest>,
) -> std::result::Result<Json<crate::effectiveness::EffectivenessReport>, ApiError> {
    let store = crate::effectiveness::EffectivenessStore::new(state.database.clone());
    Ok(Json(
        run_blocking(&state, move || store.report(&request)).await?,
    ))
}

async fn judge_status(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let store =
        crate::judge::JudgeStore::with_redactor(state.database.clone(), state.redactor.clone());
    let audits = run_blocking(&state, move || store.audits(50)).await?;
    Ok(Json(
        serde_json::json!({"workspace":state.config.workspace,"enabled":state.config.judge.enabled,
        "model":state.config.judge.model,"endpoint":state.config.judge.endpoint,"provider":state.config.judge.provider,
        "running":state.judge_slot.available_permits()==0,"audits":audits}),
    ))
}

async fn judge_run(
    State(state): State<DaemonState>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    if !state.config.judge.enabled {
        return Err(ApiError::from_core(&Error::Config(
            "Automatic review is disabled. Configure [judge] enabled, endpoint and model first."
                .to_owned(),
        )));
    }
    admit_work(&state, MutationPriority::BackgroundLearning)?;
    let queued = spawn_judge_pass(state);
    Ok(Json(
        serde_json::json!({"queued":queued,"already_running":!queued}),
    ))
}

/// Longest a re-verification plan may take: each review's live source check
/// runs up to twice, with bounded reads each.
const REVERIFY_PLAN_DEADLINE: Duration = Duration::from_secs(600);

async fn reverify_plan(
    State(state): State<DaemonState>,
    Json(request): Json<crate::reverify::PlanRequest>,
) -> std::result::Result<Json<crate::reverify::Plan>, ApiError> {
    let database = state.database.clone();
    let redactor = state.redactor.clone();
    let judge = state.config.judge.clone();
    let deadline =
        Duration::from_millis(state.config.daemon.request_timeout_ms).max(REVERIFY_PLAN_DEADLINE);
    Ok(Json(
        run_blocking_with_timeout(&state, deadline, move || {
            crate::reverify::plan(&database, &redactor, &judge, &request)
        })
        .await?,
    ))
}

/// Plan an expiry like a re-verification plan (it reads each review's live
/// sources), then expire through the scheduler when the request applies.
async fn review_expire(
    State(state): State<DaemonState>,
    Json(request): Json<crate::expiry::ExpiryRequest>,
) -> std::result::Result<Json<crate::expiry::ExpiryReport>, ApiError> {
    let database = state.database.clone();
    let redactor = state.redactor.clone();
    let judge = state.config.judge.clone();
    let deadline =
        Duration::from_millis(state.config.daemon.request_timeout_ms).max(REVERIFY_PLAN_DEADLINE);
    let planned = request.clone();
    let (mut report, expiries) = run_blocking_with_timeout(&state, deadline, move || {
        crate::expiry::plan(&database, &redactor, &judge, &planned)
    })
    .await?;
    if request.apply {
        let store =
            crate::product::ProductStore::new(state.database.clone(), state.redactor.clone());
        let older_than_ms = request.older_than_ms;
        let handle = state
            .scheduler
            .submit(MutationPriority::Interactive, move || {
                store.expire_reviews(&expiries, older_than_ms)
            })
            .map_err(|error| ApiError::from_core(&error))?;
        report.expired = wait_for_mutation(&state, handle).await?.len();
        report.applied = true;
    }
    Ok(Json(report))
}

async fn reverify_attach(
    State(state): State<DaemonState>,
    Json(request): Json<crate::reverify::AttachRequest>,
) -> std::result::Result<Json<crate::reverify::AttachResult>, ApiError> {
    let store = crate::product::ProductStore::new(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            request
                .attachments
                .iter()
                .map(|attachment| {
                    store
                        .attach_reverified_evidence(&attachment.review_id, &attachment.evidence_ids)
                })
                .collect::<Result<Vec<_>>>()
        })
        .map_err(|error| ApiError::from_core(&error))?;
    let reviews = wait_for_mutation(&state, handle).await?;
    // The changed evidence makes each proposal's packet new, so the next pass
    // judges it again. Start one now when the judge runs.
    let judge_enabled = state.config.judge.enabled;
    let judge_queued = judge_enabled
        && admit_work(&state, MutationPriority::BackgroundLearning).is_ok()
        && spawn_judge_pass(state);
    Ok(Json(crate::reverify::AttachResult {
        reviews,
        judge_enabled,
        judge_queued,
    }))
}

fn spawn_judge_pass(state: DaemonState) -> bool {
    if state
        .judge_stopping
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return false;
    }
    let Ok(permit) = Arc::clone(&state.judge_slot).try_acquire_owned() else {
        return false;
    };
    if state
        .judge_stopping
        .load(std::sync::atomic::Ordering::SeqCst)
    {
        return false;
    }
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = run_judge_pass(&state).await {
            eprintln!("automatic lesson review: {error}");
        }
    });
    true
}

async fn run_judge_pass(state: &DaemonState) -> Result<()> {
    let _background = Arc::clone(&state.background_slots)
        .acquire_owned()
        .await
        .map_err(|e| Error::Daemon(e.to_string()))?;
    let store =
        crate::judge::JudgeStore::with_redactor(state.database.clone(), state.redactor.clone());
    let config = state.config.judge.clone();
    let packets = tokio::task::spawn_blocking(move || store.prepare_pending(&config))
        .await
        .map_err(|e| Error::Daemon(e.to_string()))??;
    for packet in packets {
        if state
            .judge_stopping
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            break;
        }
        let evaluation = crate::judge::evaluate(&state.config.judge, &packet).await;
        let database = state.database.clone();
        let store =
            crate::judge::JudgeStore::with_redactor(database.clone(), state.redactor.clone());
        let config = state.config.judge.clone();
        let handle = state
            .scheduler
            .submit(MutationPriority::BackgroundLearning, move || {
                let audit = store.apply(&config, &packet, &evaluation)?;
                if evaluation.usage.input_tokens.is_some()
                    || evaluation.usage.output_tokens.is_some()
                    || evaluation.usage.reported_cost_usd.is_some()
                {
                    crate::effectiveness::EffectivenessStore::new(database).record(
                        &crate::effectiveness::UsageRecordRequest {
                            sample_id: audit.id.clone(),
                            project_id: packet.review.project_id,
                            task_id: Some(packet.review.task_id),
                            context_run_id: None,
                            session_id: None,
                            source: "internal_judge".to_owned(),
                            provider: Some(
                                match config.provider {
                                    crate::judge::JudgeProvider::Local => "local",
                                    crate::judge::JudgeProvider::Typesafe => "typesafe",
                                }
                                .to_owned(),
                            ),
                            model: Some(config.model),
                            mode: None,
                            metrics: crate::effectiveness::UsageMetrics {
                                input_tokens: evaluation
                                    .usage
                                    .input_tokens
                                    .and_then(|n| i64::try_from(n).ok()),
                                output_tokens: evaluation
                                    .usage
                                    .output_tokens
                                    .and_then(|n| i64::try_from(n).ok()),
                                cost_usd: evaluation.usage.reported_cost_usd,
                                ..crate::effectiveness::UsageMetrics::default()
                            },
                            observed_at_ms: Some(audit.recorded_at_ms),
                        },
                    )?;
                }
                Ok(())
            })?;
        tokio::task::spawn_blocking(move || handle.wait())
            .await
            .map_err(|e| Error::Daemon(e.to_string()))??;
    }
    Ok(())
}

async fn record_usage(
    State(state): State<DaemonState>,
    Json(mut request): Json<crate::effectiveness::UsageRecordRequest>,
) -> std::result::Result<Json<crate::effectiveness::UsageRecordResult>, ApiError> {
    admit_work(&state, MutationPriority::Analytics)?;
    // Labels can be provided by external adapters; never persist credentials.
    request.source = state.redactor.redact_text(&request.source);
    request.model = request.model.map(|s| state.redactor.redact_text(&s));
    request.provider = request.provider.map(|s| state.redactor.redact_text(&s));
    let store = crate::effectiveness::EffectivenessStore::new(state.database.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Analytics, move || store.record(&request))
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn record_tool_observation(
    State(state): State<DaemonState>,
    Json(mut request): Json<crate::effectiveness::ToolObservationRequest>,
) -> std::result::Result<Json<crate::effectiveness::ToolObservationReceipt>, ApiError> {
    admit_work(&state, MutationPriority::Analytics)?;
    // Labels and paths come from external adapters; never persist credentials.
    request.source = state.redactor.redact_text(&request.source);
    request.tool_name = state.redactor.redact_text(&request.tool_name);
    for path in &mut request.paths {
        *path = state.redactor.redact_text(path);
    }
    let call_id = request
        .call_id()
        .map_err(|error| ApiError::from_core(&error))?;
    // Time the observation on receipt, not when the low-priority write runs:
    // a pre-tool touch must stay ordered before the context compiled after it.
    let observed_at_ms = job_time_ms();
    let store = crate::effectiveness::EffectivenessStore::new(state.database.clone());
    // Acknowledge once queued. The pre-tool hook calls this before compiling
    // context; waiting for a lowest-priority commit would hold the hook behind
    // every queued higher-priority write. Scope errors are logged, not returned.
    let _queued = state
        .scheduler
        .submit(MutationPriority::Analytics, move || {
            store
                .record_tool_observation_at(&request, observed_at_ms)
                .inspect_err(|error| eprintln!("tool observation not recorded: {error}"))
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(crate::effectiveness::ToolObservationReceipt {
        call_id,
        observed_at_ms,
        queued: true,
    }))
}

async fn record_verification_observation(
    State(state): State<DaemonState>,
    Json(mut request): Json<crate::effectiveness::VerificationObservationRequest>,
) -> std::result::Result<Json<crate::effectiveness::VerificationObservationReceipt>, ApiError> {
    admit_work(&state, MutationPriority::Analytics)?;
    // Labels come from external adapters; never persist credentials.
    request.source = state.redactor.redact_text(&request.source);
    let call_id = request
        .call_id()
        .map_err(|error| ApiError::from_core(&error))?;
    // Time the run on receipt: a note's run must stay ordered before the
    // runs that follow it, whenever the low-priority writes commit.
    let observed_at_ms = job_time_ms();
    let store = crate::effectiveness::EffectivenessStore::new(state.database.clone());
    // Acknowledge once queued, like tool observations: the post-tool hook
    // must not wait behind every higher-priority write. Scope errors are
    // logged, not returned.
    let _queued = state
        .scheduler
        .submit(MutationPriority::Analytics, move || {
            store
                .record_verification_observation_at(&request, observed_at_ms)
                .inspect_err(|error| eprintln!("verification observation not recorded: {error}"))
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(crate::effectiveness::VerificationObservationReceipt {
        call_id,
        observed_at_ms,
        queued: true,
    }))
}

async fn product_mutate(
    State(state): State<DaemonState>,
    Json(request): Json<crate::product::Mutation>,
) -> std::result::Result<Json<crate::product::Record>, ApiError> {
    let store = crate::product::ProductStore::new(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || store.mutate(request))
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

async fn lesson_mutate(
    State(state): State<DaemonState>,
    Json(request): Json<crate::product::lessons::LessonMutation>,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let store = crate::product::ProductStore::new(state.database.clone(), state.redactor.clone());
    let handle = state
        .scheduler
        .submit(MutationPriority::Interactive, move || {
            store.lesson_mutate(&request)
        })
        .map_err(|error| ApiError::from_core(&error))?;
    Ok(Json(wait_for_mutation(&state, handle).await?))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use super::*;

    fn test_config(temp: &TempDir) -> Config {
        let mut config = Config::default();
        config.storage.db_path = temp.path().join("brain.sqlite");
        config.daemon.socket_path = temp.path().join("engramd.sock");
        config.daemon.request_timeout_ms = 2_000;
        config
    }

    async fn wait_until_ready(client: &DaemonClient) {
        for _ in 0..100 {
            if client.health().await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("daemon did not become ready");
    }

    fn bare_state(config: Config) -> DaemonState {
        use std::sync::atomic::{AtomicBool, AtomicUsize};
        DaemonState {
            database: Database::open(&config.storage.db_path).expect("database"),
            redactor: Redactor::new(&config.security.custom_redaction_patterns).expect("redactor"),
            config: Arc::new(config),
            scheduler: MutationScheduler::start(64),
            blocking_slots: Arc::new(Semaphore::new(2)),
            background_slots: Arc::new(Semaphore::new(1)),
            judge_slot: Arc::new(Semaphore::new(1)),
            judge_stopping: Arc::new(AtomicBool::new(false)),
            world_cache: None,
            metrics: Arc::new(MetricsRegistry::default()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            started: Instant::now(),
            draining: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            merged_projects: MergedProjects::default(),
        }
    }

    /// The merge job itself makes the merge visible, so a client that
    /// disconnects while it runs (the handler is dropped then) cannot leave
    /// the retired id without its redirect. A repeated merge reloads it too.
    #[test]
    fn the_merge_job_installs_redirects_without_the_handler() {
        let temp = TempDir::new().expect("temp dir");
        let state = bare_state(test_config(&temp));
        let events = crate::events::EventStore::new(state.database.clone());
        let survivor = ProjectId::parse("prj_aaaaaaaa-0000-4000-8000-000000000001").expect("id");
        let retired = ProjectId::parse("prj_bbbbbbbb-0000-4000-8000-000000000002").expect("id");
        for (project, name) in [(&survivor, "app"), (&retired, "app-copy")] {
            events
                .append(&crate::events::DomainEvent::ProjectRegistered {
                    project_id: project.clone(),
                    name: name.to_owned(),
                    canonical_remote: None,
                    root_commit: None,
                    parent_project_id: None,
                })
                .expect("register");
        }
        let input = crate::merge::MergeExecution {
            survivor: survivor.clone(),
            retired: retired.clone(),
            identity: crate::merge::IdentityEvidence {
                same_repository: true,
                canonical_remote: Some("example.com/org/app".to_owned()),
                root_commit: Some("0123456789abcdef".to_owned()),
                checkouts: Vec::new(),
                refusal: None,
            },
            backup_path: None,
        };
        let backup = temp.path().join("backups/pre-merge.sqlite");
        let redirect = |state: &DaemonState| {
            state
                .merged_projects
                .read()
                .expect("redirects")
                .get(retired.as_str())
                .cloned()
        };
        let (_, merged) = merge_job(&state, &input, &backup).expect("merge job");
        assert!(merged);
        assert!(backup.is_file(), "the backup comes first");
        assert_eq!(redirect(&state).as_deref(), Some(survivor.as_str()));

        // A restarted daemon, or one that lost the map: the repeated merge
        // reports the earlier merge and installs the redirect again.
        *state.merged_projects.write().expect("redirects") = Arc::default();
        let (_, merged_again) = merge_job(&state, &input, &backup).expect("repeat");
        assert!(!merged_again);
        assert_eq!(redirect(&state).as_deref(), Some(survivor.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drain_lands_writes_acknowledged_once_queued() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let temp = TempDir::new().expect("temp dir");
        let config = test_config(&temp);
        let state = DaemonState {
            database: Database::open(&config.storage.db_path).expect("database"),
            redactor: Redactor::new(&config.security.custom_redaction_patterns).expect("redactor"),
            config: Arc::new(config),
            scheduler: MutationScheduler::start(64),
            blocking_slots: Arc::new(Semaphore::new(2)),
            background_slots: Arc::new(Semaphore::new(1)),
            judge_slot: Arc::new(Semaphore::new(1)),
            judge_stopping: Arc::new(AtomicBool::new(false)),
            world_cache: None,
            metrics: Arc::new(MetricsRegistry::default()),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            started: Instant::now(),
            draining: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            merged_projects: MergedProjects::default(),
        };
        // Hold the writer so writes acknowledged at enqueue pile up behind it,
        // as tool observations do behind higher-priority work.
        let (release, held) = std::sync::mpsc::channel::<()>();
        let _blocker = state
            .scheduler
            .submit(MutationPriority::Interactive, move || {
                let _ = held.recv();
                Ok(())
            })
            .expect("blocker");
        let landed = Arc::new(AtomicUsize::new(0));
        for _ in 0..20 {
            let landed = Arc::clone(&landed);
            let _acknowledged = state
                .scheduler
                .submit(MutationPriority::Analytics, move || {
                    landed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .expect("queued write");
        }
        assert!(state.scheduler.pending() >= 20);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            let _ = release.send(());
        });
        let (stop, stopped) = tokio::sync::watch::channel(false);
        drain(&state, stop, None).await.expect("drain");
        assert!(*stopped.borrow(), "the job loop is told to stop");
        assert!(state.draining.load(Ordering::SeqCst), "new work is refused");
        assert!(state.judge_stopping.load(Ordering::SeqCst));
        assert_eq!(landed.load(Ordering::SeqCst), 20, "queued writes landed");
        assert_eq!(state.scheduler.pending(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn automatic_vault_refresh_skips_hidden_projects() {
        let temp = TempDir::new().expect("temp dir");
        let mut config = test_config(&temp);
        config.vault.enabled = true;
        config.vault.root = temp.path().join("vault");
        config.watch.enabled = false;
        config.web.enabled = false;
        config.links.auto_scan = false;
        let [shown, hidden] = ["shown", "hidden"].map(|name| {
            let root = temp.path().join(name);
            std::fs::create_dir_all(&root).expect("project dir");
            std::fs::write(root.join("lib.rs"), "pub fn run() {}\n").expect("source");
            root
        });
        let client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(5));
        let (stop, receive) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = receive.await;
        }));
        wait_until_ready(&client).await;
        let hidden_id = client
            .resolve_project(&hidden)
            .await
            .expect("register")
            .resolved
            .project
            .id;
        client
            .set_fact(&FactSetRequest {
                project_id: hidden_id.clone(),
                key: "internal.hidden".to_owned(),
                value: "true".to_owned(),
                note: None,
            })
            .await
            .expect("hide");
        client.index_project(&hidden, None).await.expect("index");
        let shown_id = client
            .index_project(&shown, None)
            .await
            .expect("index")
            .resolved
            .project
            .id;
        // Enqueues are queued behind the index response, in index order.
        let mut projects = Vec::new();
        for _ in 0..300 {
            projects = client
                .list_jobs(&JobListRequest {
                    status: None,
                    limit: 100,
                })
                .await
                .expect("jobs")
                .into_iter()
                .filter(|job| job.job_type == JobType::VaultExport)
                .filter_map(|job| job.project_id)
                .collect::<Vec<_>>();
            if projects.contains(&shown_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(projects, [shown_id], "only the shown project is exported");
        stop.send(()).expect("stop");
        server.await.expect("join").expect("serve");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn imports_long_legacy_guide_without_requiring_vault_export() {
        let temp = TempDir::new().unwrap();
        let mut config = test_config(&temp);
        config.vault.enabled = false;
        config.vault.root = temp.path().join("vault");
        config.watch.enabled = false;
        config.web.enabled = false;
        std::fs::create_dir_all(&config.vault.root).unwrap();
        let content = "A substantial existing architecture guide.\n".repeat(300);
        std::fs::write(config.vault.root.join("ARCHITECTURE.md"), &content).unwrap();
        let client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(3));
        let (stop, receive) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = receive.await;
        }));
        wait_until_ready(&client).await;
        let value = client
            .product_query(&crate::product::Query::Guide)
            .await
            .unwrap();
        assert_eq!(value["item"]["value"]["content"], content);
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::too_many_lines)]
    async fn daemon_file_shape_lists_task_relevant_definitions_first() {
        use std::fmt::Write as _;
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let mut source = (0..30).fold(String::new(), |mut source, n| {
            let _ = writeln!(source, "pub fn step_{n:02}() -> u32 {{\n    {n}\n}}\n");
            source
        });
        source.push_str("pub fn reseed_checksum_table() {}\npub fn checksum_seed_mix() {}\n");
        std::fs::write(project.join("engine.rs"), &source).expect("engine");
        let config = test_config(&temp);
        let client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;
        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        client.index_project(&project, None).await.expect("index");
        let shape = |task_text: Option<&str>, context_run_id: Option<ContextRunId>| {
            let request = FileShapeRequest {
                project_id: project_id.clone(),
                file_path: "engine.rs".to_owned(),
                repository_root: Some(project.clone()),
                task_text: task_text.map(str::to_owned),
                context_run_id,
            };
            let client = &client;
            async move { client.file_shape(&request).await }
        };
        let order = |shape: &FileShape| {
            shape
                .symbols
                .iter()
                .take(4)
                .map(|symbol| (symbol.label.clone(), symbol.task_match))
                .collect::<Vec<_>>()
        };
        let plain = |labels: &[&str]| {
            labels
                .iter()
                .map(|label| ((*label).to_owned(), false))
                .collect::<Vec<_>>()
        };

        // A named definition near the end comes first, with its span; then
        // task-word matches, strongest first; then file order.
        let ranked = shape(
            Some("Change step_27 so the checksum seed is configurable"),
            None,
        )
        .await
        .expect("ranked");
        assert_eq!(
            order(&ranked),
            [
                ("step_27".to_owned(), true),
                ("checksum_seed_mix".to_owned(), true),
                ("reseed_checksum_table".to_owned(), true),
                ("step_00".to_owned(), false),
            ]
        );
        assert_eq!(
            ranked.symbols[0]
                .lines
                .map(|lines| (lines.start, lines.end)),
            Some((109, 111))
        );
        // Without task signals, or when nothing matches: file order.
        for task in [None, Some("Tidy up the documentation wording")] {
            let shape = shape(task, None).await.expect("file order");
            assert_eq!(
                order(&shape),
                plain(&["step_00", "step_01", "step_02", "step_03"])
            );
        }
        // A definition the task source map selected comes first.
        let context = client
            .compile_context(&ContextCompileRequest {
                request_id: "relevance-run".to_owned(),
                project_id: project_id.clone(),
                task_text: "Inspect step_28 before the release".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: crate::context::ContextMode::Brain,
                repository_root: Some(project.clone()),
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect("compile");
        let selected = shape(None, Some(context.id.clone()))
            .await
            .expect("selected");
        assert_eq!(selected.symbols[0].label, "step_28", "{selected:?}");
        assert!(selected.symbols[0].task_match);
        // A run of another project selects nothing here.
        let other = client
            .file_shape(&FileShapeRequest {
                project_id: ProjectId::new(),
                file_path: "engine.rs".to_owned(),
                repository_root: None,
                task_text: None,
                context_run_id: Some(context.id),
            })
            .await
            .expect("other project");
        assert!(other.symbols.is_empty());
        let _ = shutdown_sender.send(());
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_file_shape_checks_spans_against_the_checkout() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join("engine.rs"),
            "pub fn run() {}\npub struct Engine;\npub fn helper() {}\n",
        )
        .expect("engine");
        let config = test_config(&temp);
        let client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;
        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        client.index_project(&project, None).await.expect("index");
        let plain = client
            .file_shape(&FileShapeRequest {
                project_id: project_id.clone(),
                file_path: "engine.rs".to_owned(),
                repository_root: None,
                task_text: None,
                context_run_id: None,
            })
            .await
            .expect("plain shape");
        assert!(
            plain.symbols.iter().all(|s| s.lines.is_none()),
            "no checkout, no spans"
        );
        assert_eq!(plain.size_bytes, Some(54), "the indexed size");
        let unknown = client
            .file_shape(&FileShapeRequest {
                project_id: project_id.clone(),
                file_path: "missing.rs".to_owned(),
                repository_root: Some(project.clone()),
                task_text: None,
                context_run_id: None,
            })
            .await
            .expect("unknown file");
        assert_eq!(unknown.size_bytes, None);
        // With the checkout, spans are checked like task source map ranges.
        let spans = |shape: &FileShape| {
            shape
                .symbols
                .iter()
                .map(|symbol| (symbol.label.clone(), symbol.lines.map(|l| (l.start, l.end))))
                .collect::<Vec<_>>()
        };
        let checked = FileShapeRequest {
            project_id: project_id.clone(),
            file_path: "engine.rs".to_owned(),
            repository_root: Some(project.clone()),
            task_text: None,
            context_run_id: None,
        };
        let shape = client.file_shape(&checked).await.expect("checked shape");
        assert_eq!(
            spans(&shape),
            [
                ("run".to_owned(), Some((1, 1))),
                ("Engine".to_owned(), Some((2, 2))),
                ("helper".to_owned(), Some((3, 3))),
            ]
        );
        assert_eq!(shape.size_bytes, Some(54));
        // A file edited since indexing gets no spans, and its current size.
        std::fs::write(
            project.join("engine.rs"),
            "// moved\npub fn run() {}\npub struct Engine;\npub fn helper() {}\n",
        )
        .expect("edit");
        let stale = client.file_shape(&checked).await.expect("stale shape");
        assert!(stale.symbols.iter().all(|s| s.lines.is_none()), "{stale:?}");
        assert_eq!(stale.size_bytes, Some(63));
        let _ = shutdown_sender.send(());
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn daemon_serves_file_shape_and_tasks_brief() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join("engine.rs"),
            "pub fn run() {}\npub struct Engine;\npub fn helper() {}\n",
        )
        .expect("engine");
        std::fs::write(project.join("caller.rs"), "fn boot() { run(); }\n").expect("caller");
        // `helper` is defined twice, so its call resolves to neither.
        std::fs::write(project.join("spare.rs"), "pub fn helper() {}\n").expect("spare");
        std::fs::write(project.join("guess.rs"), "fn start() { helper(); }\n").expect("guess");
        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        client.index_project(&project, None).await.expect("index");

        let shape = client
            .file_shape(&FileShapeRequest {
                project_id: project_id.clone(),
                file_path: "engine.rs".to_owned(),
                repository_root: None,
                task_text: None,
                context_run_id: None,
            })
            .await
            .expect("file shape");
        assert!(shape.symbols.iter().any(|s| s.label == "run"));
        assert!(shape.symbols.iter().any(|s| s.label == "Engine"));
        let callers = shape
            .callers
            .iter()
            .map(|caller| (caller.label.as_str(), caller.file_path.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            callers,
            [("boot", "caller.rs")],
            "cross-file caller resolved by a name only this file defines"
        );
        for (group, depends_on, used_by) in [
            ("engine.rs", &[][..], &["caller.rs"][..]),
            ("caller.rs", &["engine.rs"][..], &[][..]),
            ("guess.rs", &[][..], &[][..]),
            ("spare.rs", &[][..], &[][..]),
        ] {
            let module = client
                .module_symbols(&ModuleSymbolsRequest {
                    project_id: project_id.clone(),
                    group: group.to_owned(),
                })
                .await
                .expect("module symbols");
            assert_eq!(module.depends_on, depends_on, "{group} depends on");
            assert_eq!(module.used_by, used_by, "{group} used by");
        }

        let unknown = client
            .file_shape(&FileShapeRequest {
                project_id: project_id.clone(),
                file_path: "missing.rs".to_owned(),
                repository_root: None,
                task_text: None,
                context_run_id: None,
            })
            .await
            .expect("unknown file");
        assert!(unknown.symbols.is_empty() && unknown.callers.is_empty());

        let task = client
            .start_task(&TaskStartRequest {
                context_request_id: None,
                project_id: project_id.clone(),
                prompt: "wire the engine into the scheduler".to_owned(),
                session_id: None,
            })
            .await
            .expect("start task");
        let brief = client.tasks_brief(&project_id).await.expect("brief");
        assert_eq!(brief.open.len(), 1);
        assert_eq!(brief.open[0].id, task.id.as_str());
        assert!(brief.last_completed.is_none());

        client
            .finish_task(
                &task.id,
                &TaskFinishRequest {
                    outcome: TaskOutcome::Success,
                    accepted_by_user: Some(true),
                    final_commit: None,
                },
            )
            .await
            .expect("finish");
        let brief = client.tasks_brief(&project_id).await.expect("brief 2");
        assert!(brief.open.is_empty());
        assert_eq!(brief.last_completed.expect("completed").status, "success");

        let _ = shutdown_sender.send(());
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::too_many_lines)]
    async fn daemon_serves_health_and_serialized_project_registration() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".engram")).expect("project dir");
        std::fs::write(
            project.join(".engram/config.toml"),
            "[context]\nmax_injected_tokens = 200\nstable_prefix_tokens = 30\n",
        )
        .expect("project config");
        std::fs::write(project.join("lib.rs"), "pub struct Engine;\nfn run() {}\n")
            .expect("source");
        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let health = client.health().await.expect("health");
        assert_eq!(health.status, "ok");
        assert_eq!(health.read_pool.capacity, 4);
        let status = client.resolve_project(&project).await.expect("resolve");
        assert_eq!(status.resolved.root, project.canonicalize().expect("root"));
        assert_eq!(status.project_event_count, 2);
        let indexed = client.index_project(&project, None).await.expect("index");
        assert_eq!(indexed.index.parsed_files, 1);
        let nodes = client
            .search_world(&WorldSearchRequest {
                project_id: status.resolved.project.id.clone(),
                query: "Engine".to_owned(),
                limit: 10,
            })
            .await
            .expect("world search");
        assert_eq!(nodes[0].label, "Engine");

        let task = client
            .start_task(&TaskStartRequest {
                context_request_id: None,
                project_id: status.resolved.project.id.clone(),
                prompt: "fix engine cleanup".to_owned(),
                session_id: Some("daemon-test-session".to_owned()),
            })
            .await
            .expect("start task");
        let attempt = client
            .record_attempt(
                &task.id,
                &AttemptCreateRequest {
                    strategy_summary: "add an explicit cleanup path".to_owned(),
                    outcome: AttemptOutcome::Success,
                    files: vec!["lib.rs".to_owned()],
                },
            )
            .await
            .expect("record attempt");
        let evidence = client
            .record_evidence(&EvidenceCreateRequest {
                project_id: status.resolved.project.id.clone(),
                task_id: Some(task.id.clone()),
                attempt_id: Some(attempt.id),
                kind: "test_result".to_owned(),
                strength: EvidenceStrength::Deterministic,
                commit_hash: None,
                command: Some("cargo test".to_owned()),
                exit_code: Some(0),
                artifact_text: "test result: ok".to_owned(),
                metadata: serde_json::json!({"suite": "unit"}),
            })
            .await
            .expect("record evidence");
        assert_eq!(evidence.artifact_digest.len(), 64);
        let finished = client
            .finish_task(
                &task.id,
                &TaskFinishRequest {
                    outcome: TaskOutcome::Success,
                    accepted_by_user: Some(true),
                    final_commit: None,
                },
            )
            .await
            .expect("finish task");
        assert_eq!(finished.status, "success");

        let memory = client
            .create_memory(&MemoryCreateRequest {
                project_id: status.resolved.project.id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: status.resolved.project.id.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Engine cleanup must run after abort".to_owned(),
                    rationale: "Explicitly approved local rule".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["cleanup".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: vec![nodes[0].id.clone()],
                },
            })
            .await
            .expect("create memory");
        let after_create = client.doctor().await.expect("doctor after create");
        assert!(after_create.ok, "after create: {after_create:?}");
        let validated = client
            .promote_memory(
                &memory.id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await
            .expect("human validation");
        assert_eq!(validated.epistemic_state, EpistemicState::Validated);
        let compiled = client
            .compile_context(&ContextCompileRequest {
                request_id: "daemon-context-1".to_owned(),
                project_id: status.resolved.project.id.clone(),
                task_text: "fix engine cleanup".to_owned(),
                current_paths: vec!["lib.rs".to_owned()],
                token_budget: 200,
                mode: crate::context::ContextMode::Brain,
                repository_root: Some(project.clone()),
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect("compile context");
        assert_eq!(
            compiled
                .items
                .iter()
                .filter(|item| item.source_type == "memory")
                .count(),
            1
        );
        assert!(compiled.items.iter().any(|item| item.source_type == "code"));
        let usage = crate::effectiveness::UsageRecordRequest {
            sample_id: "fixture-provider-response".to_owned(),
            project_id: status.resolved.project.id.clone(),
            task_id: None,
            context_run_id: Some(compiled.id.clone()),
            session_id: None,
            source: "test_harness".to_owned(),
            provider: None,
            model: None,
            mode: Some(crate::effectiveness::UsageMode::Brain),
            metrics: crate::effectiveness::UsageMetrics {
                input_tokens: Some(123),
                ..Default::default()
            },
            observed_at_ms: None,
        };
        assert!(
            client
                .record_usage(&usage)
                .await
                .expect("record usage")
                .inserted
        );
        assert!(
            !client
                .record_usage(&usage)
                .await
                .expect("idempotent usage")
                .inserted
        );
        let report = client
            .effectiveness(&crate::effectiveness::EffectivenessRequest {
                project_ids: Some(vec![status.resolved.project.id.clone()]),
                ..Default::default()
            })
            .await
            .expect("effectiveness");
        assert_eq!(report.usage.totals.input_tokens, Some(123));
        assert_eq!(report.usage.totals.cost_usd, None);
        assert_eq!(report.projects[0].contexts.usage_linked_runs, 1);
        assert_eq!(
            client.judge_status().await.expect("judge status")["enabled"],
            false
        );
        assert!(client.judge_run().await.is_err());
        let project_limit = client
            .compile_context(&ContextCompileRequest {
                request_id: "daemon-context-over-budget".to_owned(),
                project_id: status.resolved.project.id.clone(),
                task_text: "prove project configuration is live".to_owned(),
                current_paths: Vec::new(),
                token_budget: 201,
                mode: crate::context::ContextMode::Brain,
                repository_root: Some(project.clone()),
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect_err("project context limit");
        assert!(project_limit.to_string().contains("configured maximum 200"));
        let after_compile = client.doctor().await.expect("doctor after compile");
        assert!(after_compile.ok, "after compile: {after_compile:?}");
        std::fs::write(
            project.join("lib.rs"),
            "pub struct Engine { pub active: bool }\nfn run() {}\n",
        )
        .expect("change linked symbol");
        let refreshed = client
            .index_project(&project, Some(vec![PathBuf::from("lib.rs")]))
            .await
            .expect("incremental refresh");
        assert_eq!(refreshed.invalidated_memories, 1);
        assert_eq!(
            client
                .get_memory(&memory.id)
                .await
                .expect("get suspect")
                .expect("memory remains queryable")
                .status,
            crate::memory::MemoryStatus::Suspect
        );
        let forgotten = client
            .forget_memory(&memory.id)
            .await
            .expect("logical forget");
        assert_eq!(forgotten.status, crate::memory::MemoryStatus::Archived);
        client.purge_memory(&memory.id).await.expect("purge");
        assert!(
            client
                .get_memory(&memory.id)
                .await
                .expect("get purged")
                .is_none()
        );
        let doctor = client.doctor().await.expect("doctor");
        assert!(doctor.ok, "{doctor:?}");

        let backup = temp.path().join("backup.sqlite");
        client.create_backup(&backup).await.expect("create backup");
        let backup_integrity = client.verify_backup(&backup).await.expect("verify backup");
        assert_eq!(backup_integrity.integrity, "ok");
        assert!(backup_integrity.foreign_keys);
        let export = temp.path().join("project.ndjson");
        let export_report = client
            .export_project(&ProjectExportRequest {
                project_id: status.resolved.project.id,
                destination: export.clone(),
            })
            .await
            .expect("export project");
        assert!(export_report.records > 0);
        assert!(export.exists());

        let metadata = std::fs::symlink_metadata(&socket_path).expect("socket metadata");
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

        client.shutdown().await.expect("shutdown request");
        drop(shutdown_sender);
        server.await.expect("server task").expect("server result");
        assert!(!socket_path.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn semantic_rebuild_and_scoped_semantic_context_compile_work() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(project.join(".engram")).expect("project dir");
        std::fs::write(
            project.join(".engram/config.toml"),
            "[context]\nsemantic_retrieval = true\n",
        )
        .expect("project config");
        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        let memory = client
            .create_memory(&MemoryCreateRequest {
                project_id: project_id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project_id.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Engine cleanup must release reservations".to_owned(),
                    rationale: "Explicitly approved local rule".to_owned(),
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
            .expect("create memory");
        client
            .promote_memory(
                &memory.id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await
            .expect("human validation");

        let rebuilt = client
            .rebuild_semantic_index(&SemanticRebuildRequest {
                project_id: project_id.clone(),
            })
            .await
            .expect("semantic rebuild");
        assert_eq!(rebuilt.vectors, 1);
        assert_eq!(rebuilt.project_id, project_id);

        let compiled = client
            .compile_context(&ContextCompileRequest {
                request_id: "daemon-semantic-context".to_owned(),
                project_id,
                task_text: "engine cleanup must release reservations".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: crate::context::ContextMode::Brain,
                repository_root: Some(project.clone()),
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect("semantic context compile");
        assert_eq!(compiled.items.len(), 1);
        assert!(
            compiled.items[0].why.semantic > 0.0,
            "project-scoped semantic retrieval must populate the semantic score"
        );

        client.shutdown().await.expect("shutdown request");
        drop(shutdown_sender);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn local_embedding_provider_rebuilds_over_the_api_and_serves_context() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let endpoint = crate::semantic::fake_endpoint::concept_model(4);
        let mut config = test_config(&temp);
        config.context.semantic_retrieval = true;
        config.semantic.provider = crate::semantic::EmbeddingProviderKind::Local;
        config.semantic.endpoint.clone_from(&endpoint.url);
        config.semantic.model = "embed-a".to_owned();
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(5));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        let memory = client
            .create_memory(&MemoryCreateRequest {
                project_id: project_id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project_id.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "Release reservations when abort cleanup runs".to_owned(),
                    rationale: "Explicitly approved local rule".to_owned(),
                    confidence: 0.5,
                    importance: 0.5,
                    tags: Vec::new(),
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            })
            .await
            .expect("create memory");
        client
            .promote_memory(
                &memory.id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await
            .expect("human validation");

        let rebuilt = client
            .rebuild_semantic_index(&SemanticRebuildRequest {
                project_id: project_id.clone(),
            })
            .await
            .expect("semantic rebuild");
        assert_eq!(rebuilt.vectors, 1);
        let model = rebuilt.model.expect("model rebuild report");
        assert_eq!(
            (model.embedder.as_str(), model.embedded, model.dimension),
            ("local:embed-a", 1, 4)
        );

        let compiled = client
            .compile_context(&ContextCompileRequest {
                request_id: "daemon-local-embedding-context".to_owned(),
                project_id,
                task_text: "stop the job and free its holds".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: crate::context::ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect("local embedding context compile");
        assert_eq!(
            compiled.items.len(),
            1,
            "the model space finds the paraphrase"
        );
        assert!(compiled.items[0].why.lexical.abs() < f64::EPSILON);
        assert!(compiled.items[0].why.semantic > 0.0);
        assert!(
            compiled.items[0]
                .why
                .note
                .ends_with("; semantic: local:embed-a")
        );

        client.shutdown().await.expect("shutdown request");
        drop(shutdown_sender);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_jobs_are_enqueued_over_the_api_and_executed_by_the_daemon() {
        let temp = TempDir::new().expect("temp dir");
        let mut config = test_config(&temp);
        config.jobs.poll_interval_ms = 25;
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        // A project-scoped type without a project is rejected at the boundary.
        let error = client
            .enqueue_job(&JobEnqueueRequest {
                job_type: JobType::SemanticRebuild,
                project_id: None,
                payload: None,
                dedupe_key: None,
            })
            .await
            .expect_err("project scope required");
        assert!(error.to_string().contains("requires a project scope"));

        let job = client
            .enqueue_job(&JobEnqueueRequest {
                job_type: JobType::FtsIntegrity,
                project_id: None,
                payload: None,
                dedupe_key: Some("api-test".to_owned()),
            })
            .await
            .expect("enqueue");
        assert_eq!(job.status, JobStatus::Pending);

        // Idempotent by (type, dedupe_key).
        let duplicate = client
            .enqueue_job(&JobEnqueueRequest {
                job_type: JobType::FtsIntegrity,
                project_id: None,
                payload: None,
                dedupe_key: Some("api-test".to_owned()),
            })
            .await
            .expect("duplicate enqueue");
        assert_eq!(duplicate.id, job.id);

        // The daemon-owned loop picks the job up on its fixed poll interval.
        let deadline = Instant::now() + Duration::from_secs(10);
        let executed = loop {
            let current = client
                .get_job(&job.id)
                .await
                .expect("get job")
                .expect("job exists");
            if current.status == JobStatus::Succeeded {
                break current;
            }
            assert!(Instant::now() < deadline, "job never executed: {current:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        let result = executed.result.expect("recorded result");
        assert_eq!(result["integrity"], "ok");

        let listed = client
            .list_jobs(&JobListRequest {
                status: Some(JobStatus::Succeeded),
                limit: 10,
            })
            .await
            .expect("list jobs");
        assert!(listed.iter().any(|listed| listed.id == job.id));

        // The daily maintenance jobs self-enqueued (dedupe-keyed by day).
        let all = client
            .list_jobs(&JobListRequest {
                status: None,
                limit: 50,
            })
            .await
            .expect("list all");
        assert!(
            all.iter()
                .any(|listed| listed.job_type == JobType::ConsolidationGc),
            "daily consolidation_gc must self-enqueue: {all:?}"
        );
        assert_eq!(
            all.iter()
                .filter(|listed| listed.job_type == JobType::ConsolidationGc)
                .count(),
            1,
            "daily self-enqueue is deduped by day"
        );

        client.shutdown().await.expect("shutdown request");
        drop(shutdown_sender);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::too_many_lines)]
    async fn metrics_endpoint_reflects_compiles_searches_and_memory_lifecycle() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(project.join("lib.rs"), "pub struct Meter;\nfn tick() {}\n")
            .expect("source");
        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        client.index_project(&project, None).await.expect("index");
        let memory = client
            .create_memory(&MemoryCreateRequest {
                project_id: project_id.clone(),
                memory: MemoryDraft {
                    scope_type: "project".to_owned(),
                    scope_id: project_id.to_string(),
                    kind: "lesson".to_owned(),
                    claim: "The meter must tick monotonically".to_owned(),
                    rationale: "Explicit test fixture".to_owned(),
                    confidence: 0.5,
                    importance: 0.9,
                    tags: vec!["meter".to_owned()],
                    valid_from_commit: None,
                    valid_to_commit: None,
                    trust_source: "human_instruction".to_owned(),
                    evidence_ids: Vec::new(),
                    source_task_ids: Vec::new(),
                    node_ids: Vec::new(),
                },
            })
            .await
            .expect("create memory");
        client
            .promote_memory(
                &memory.id,
                &MemoryPromoteRequest {
                    target: EpistemicState::Validated,
                    human_override: true,
                },
            )
            .await
            .expect("promote");
        let compiled = client
            .compile_context(&ContextCompileRequest {
                request_id: "metrics-brain".to_owned(),
                project_id: project_id.clone(),
                task_text: "make the meter tick".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: ContextMode::Brain,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect("brain compile");
        assert!(compiled.metrics.selected_tokens > 0);
        client
            .compile_context(&ContextCompileRequest {
                request_id: "metrics-off".to_owned(),
                project_id: project_id.clone(),
                task_text: "control run".to_owned(),
                current_paths: Vec::new(),
                token_budget: 400,
                mode: ContextMode::Off,
                repository_root: None,
                head: None,
                task_id: None,
                framing: None,
            })
            .await
            .expect("off compile");
        for _ in 0..2 {
            client
                .search_world(&WorldSearchRequest {
                    project_id: project_id.clone(),
                    query: "Meter".to_owned(),
                    limit: 10,
                })
                .await
                .expect("world search");
        }

        let response = client.metrics().await.expect("metrics");
        let counters = &response.metrics.counters;
        assert_eq!(counters.context_compiles_brain, 1);
        assert_eq!(counters.context_compiles_off, 1);
        assert!(counters.injected_items >= 1, "{counters:?}");
        assert!(
            counters.injected_tokens >= u64::from(compiled.metrics.selected_tokens),
            "{counters:?}"
        );
        assert!(counters.retrieval_candidates_considered >= 1);
        assert!(counters.retrieval_candidates_selected >= 1);
        assert_eq!(counters.memory_promotions, 1);
        assert_eq!(counters.memory_rejections, 0);
        assert_eq!(counters.stale_injections, 0);
        assert_eq!(response.metrics.context_compile_ms.count, 2);
        assert_eq!(response.metrics.retrieval_ms.count, 1);
        assert!(response.metrics.index_scan_ms.count >= 1);
        assert!(response.metrics.requests_ms["context"].count >= 2);
        assert!(response.metrics.requests_ms["world"].count >= 2);
        assert!(response.metrics.requests_ms["memory"].count >= 2);
        let cache = response.cache.expect("cache enabled by default");
        assert!(cache.hits >= 1, "repeat search must hit: {cache:?}");
        assert!(cache.misses >= 1);

        let breakdown = client.memory_stats().await.expect("memory stats");
        assert_eq!(breakdown.total, 1);
        assert_eq!(breakdown.by_status.get("active"), Some(&1));
        assert_eq!(breakdown.by_epistemic_state.get("validated"), Some(&1));

        client.purge_memory(&memory.id).await.expect("purge");
        let after_purge = client.metrics().await.expect("metrics after purge");
        assert_eq!(after_purge.metrics.counters.memory_purges, 1);

        drop(shutdown_sender);
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn opt_in_auto_synthesis_runs_after_a_successful_finish() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let mut config = test_config(&temp);
        config.synthesis.auto = true;
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let status = client.resolve_project(&project).await.expect("resolve");
        for ordinal in 0..3 {
            let task = client
                .start_task(&TaskStartRequest {
                    context_request_id: None,
                    project_id: status.resolved.project.id.clone(),
                    prompt: format!("auto synthesis episode {ordinal}"),
                    session_id: None,
                })
                .await
                .expect("start task");
            let attempt = client
                .record_attempt(
                    &task.id,
                    &AttemptCreateRequest {
                        strategy_summary: "quarantine flaky test".to_owned(),
                        outcome: AttemptOutcome::Success,
                        files: vec!["flaky.rs".to_owned()],
                    },
                )
                .await
                .expect("attempt");
            client
                .record_evidence(&EvidenceCreateRequest {
                    project_id: status.resolved.project.id.clone(),
                    task_id: Some(task.id.clone()),
                    attempt_id: Some(attempt.id),
                    kind: "test_result".to_owned(),
                    strength: EvidenceStrength::Deterministic,
                    commit_hash: None,
                    command: Some("cargo test".to_owned()),
                    exit_code: Some(0),
                    artifact_text: "ok".to_owned(),
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("evidence");
            client
                .finish_task(
                    &task.id,
                    &TaskFinishRequest {
                        outcome: TaskOutcome::Success,
                        accepted_by_user: Some(true),
                        final_commit: None,
                    },
                )
                .await
                .expect("finish");
        }

        // The third successful finish crossed the threshold; the opt-in auto
        // pass ran before its response returned, so the procedure exists
        // without any explicit synthesis call.
        let procedures = client
            .list_memories(&MemoryListRequest {
                project_id: status.resolved.project.id.clone(),
                kind: Some("procedure".to_owned()),
                limit: 10,
            })
            .await
            .expect("list procedures");
        assert_eq!(procedures.len(), 1);
        assert_eq!(procedures[0].claim, "Procedure: quarantine flaky test");

        drop(shutdown_sender);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn synthesis_endpoint_creates_a_procedure_from_repeated_episodes() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let status = client.resolve_project(&project).await.expect("resolve");
        for ordinal in 0..3 {
            let task = client
                .start_task(&TaskStartRequest {
                    context_request_id: None,
                    project_id: status.resolved.project.id.clone(),
                    prompt: format!("fix worker lifecycle timeout {ordinal}"),
                    session_id: None,
                })
                .await
                .expect("start task");
            let attempt = client
                .record_attempt(
                    &task.id,
                    &AttemptCreateRequest {
                        strategy_summary: "Inspect  Signal Recipient".to_owned(),
                        outcome: AttemptOutcome::Success,
                        files: vec!["worker.rs".to_owned()],
                    },
                )
                .await
                .expect("attempt");
            client
                .record_evidence(&EvidenceCreateRequest {
                    project_id: status.resolved.project.id.clone(),
                    task_id: Some(task.id.clone()),
                    attempt_id: Some(attempt.id),
                    kind: "test_result".to_owned(),
                    strength: EvidenceStrength::Deterministic,
                    commit_hash: None,
                    command: Some("cargo test".to_owned()),
                    exit_code: Some(0),
                    artifact_text: "ok".to_owned(),
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("evidence");
            client
                .finish_task(
                    &task.id,
                    &TaskFinishRequest {
                        outcome: TaskOutcome::Success,
                        accepted_by_user: Some(true),
                        final_commit: None,
                    },
                )
                .await
                .expect("finish");
        }

        let report = client
            .run_synthesis(&SynthesisRunRequest {
                path: project.clone(),
            })
            .await
            .expect("synthesis run");
        assert_eq!(report.examined_episodes, 3);
        assert_eq!(report.procedures.len(), 1);
        let procedure = &report.procedures[0];
        assert!(!procedure.reused_existing);
        assert_eq!(procedure.memory.kind, "procedure");
        assert_eq!(
            procedure.memory.claim,
            "Procedure: inspect signal recipient"
        );
        let fetched = client
            .get_memory(&procedure.memory.id)
            .await
            .expect("get")
            .expect("procedure memory");
        assert_eq!(fetched.epistemic_state, EpistemicState::Candidate);
        assert!((fetched.confidence - 0.5).abs() < f64::EPSILON);

        let repeat = client
            .run_synthesis(&SynthesisRunRequest { path: project })
            .await
            .expect("idempotent rerun");
        assert_eq!(repeat.procedures.len(), 1);
        assert!(repeat.procedures[0].reused_existing);
        assert_eq!(repeat.procedures[0].memory.id, procedure.memory.id);

        drop(shutdown_sender);
        server.await.expect("server task").expect("server result");
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_but_regular_file_is_preserved() {
        let temp = TempDir::new().expect("temp dir");
        let stale = temp.path().join("stale.sock");
        let listener = std::os::unix::net::UnixListener::bind(&stale).expect("stale listener");
        drop(listener);
        // macOS can briefly keep the closed listener connectable while the
        // kernel tears it down, especially under parallel test load. The
        // precondition here is a genuinely dead socket, so wait for the probe
        // that `bind_socket` itself uses to observe the teardown.
        let teardown_deadline = Instant::now() + Duration::from_secs(5);
        while std::os::unix::net::UnixStream::connect(&stale).is_ok() {
            assert!(
                Instant::now() < teardown_deadline,
                "closed stale listener stayed connectable"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_listener, guard) = bind_socket(&stale, 1).expect("replace stale socket");
        drop(guard);
        assert!(!stale.exists());

        let regular = temp.path().join("regular");
        std::fs::write(&regular, b"keep").expect("regular file");
        let Err(error) = bind_socket(&regular, 1) else {
            panic!("must preserve regular file");
        };
        assert!(error.to_string().contains("non-socket"));
        assert_eq!(std::fs::read(&regular).expect("preserved"), b"keep");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn world_read_cache_serves_hits_and_invalidates_on_reindex() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(project.join("lib.rs"), "pub fn cached_original() {}\n").expect("source");

        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let indexed = client
            .index_project_with_provider(&project, None, Some("tree_sitter".to_owned()), None)
            .await
            .expect("initial index");
        let project_id = indexed.resolved.project.id.clone();

        let first = search_labels(&client, &project_id, "cached_original").await;
        assert_eq!(first, ["cached_original"]);
        let second = search_labels(&client, &project_id, "cached_original").await;
        assert_eq!(second, first);
        let health = client.health().await.expect("health");
        let cache = health.cache.expect("cache enabled by default");
        assert!(cache.hits >= 1, "repeat query must hit: {cache:?}");
        assert!(cache.misses >= 1);

        // A graph mutation must invalidate before its response returns.
        std::fs::write(project.join("lib.rs"), "pub fn cached_renamed() {}\n").expect("rename");
        client
            .index_project_with_provider(&project, None, Some("tree_sitter".to_owned()), None)
            .await
            .expect("reindex");
        assert_eq!(
            search_labels(&client, &project_id, "cached_original").await,
            Vec::<String>::new(),
            "stale cached read survived a reindex"
        );
        assert_eq!(
            search_labels(&client, &project_id, "cached_renamed").await,
            ["cached_renamed"]
        );

        drop(shutdown_sender);
        let _ = server.await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::too_many_lines)]
    async fn procedure_promotes_to_skill_after_recorded_uses_via_api() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(project.join("lib.rs"), "pub fn stage() {}\n").expect("source");
        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;
        let status = client.resolve_project(&project).await.expect("resolve");
        let project_id = status.resolved.project.id.clone();
        client.index_project(&project, None).await.expect("index");

        // Three matching evidence-backed successful episodes -> procedure.
        for ordinal in 0..3 {
            let task = client
                .start_task(&TaskStartRequest {
                    context_request_id: None,
                    project_id: project_id.clone(),
                    prompt: format!("stage rollout {ordinal}"),
                    session_id: None,
                })
                .await
                .expect("task");
            client
                .record_attempt(
                    &task.id,
                    &AttemptCreateRequest {
                        strategy_summary: "stage the rollout behind the flag".to_owned(),
                        outcome: AttemptOutcome::Success,
                        files: vec!["lib.rs".to_owned()],
                    },
                )
                .await
                .expect("attempt");
            client
                .record_evidence(&EvidenceCreateRequest {
                    project_id: project_id.clone(),
                    task_id: Some(task.id.clone()),
                    attempt_id: None,
                    kind: "test_result".to_owned(),
                    strength: EvidenceStrength::Deterministic,
                    commit_hash: None,
                    command: Some("cargo test".to_owned()),
                    exit_code: Some(0),
                    artifact_text: "ok".to_owned(),
                    metadata: serde_json::json!({}),
                })
                .await
                .expect("evidence");
            client
                .finish_task(
                    &task.id,
                    &TaskFinishRequest {
                        outcome: TaskOutcome::Success,
                        accepted_by_user: Some(true),
                        final_commit: None,
                    },
                )
                .await
                .expect("finish");
        }
        let report = client
            .run_synthesis(&SynthesisRunRequest {
                path: project.clone(),
            })
            .await
            .expect("synthesis");
        assert_eq!(report.procedures.len(), 1);
        let procedure_id = report.procedures[0].memory.id.clone();

        // Premature promotion must refuse: not validated, no recorded uses.
        let premature = client
            .promote_skill(&SkillPromoteRequest {
                path: project.clone(),
                memory_id: procedure_id.clone(),
            })
            .await;
        assert!(premature.is_err(), "gating must refuse premature promotion");

        // Validate the procedure via human override, then record three
        // distinct successful uses through task-declared compiles.
        for state in [
            crate::memory::EpistemicState::Observed,
            crate::memory::EpistemicState::Supported,
            crate::memory::EpistemicState::Validated,
        ] {
            client
                .promote_memory(
                    &procedure_id,
                    &MemoryPromoteRequest {
                        target: state,
                        human_override: true,
                    },
                )
                .await
                .expect("promote");
        }
        for ordinal in 0..3 {
            let task = client
                .start_task(&TaskStartRequest {
                    context_request_id: None,
                    project_id: project_id.clone(),
                    prompt: format!("apply staged rollout {ordinal}"),
                    session_id: None,
                })
                .await
                .expect("use task");
            let compiled = client
                .compile_context(&ContextCompileRequest {
                    request_id: format!("use-{ordinal}"),
                    project_id: project_id.clone(),
                    task_text: "stage the rollout behind the flag".to_owned(),
                    current_paths: Vec::new(),
                    token_budget: 2_000,
                    mode: ContextMode::Brain,
                    repository_root: None,
                    head: None,
                    task_id: Some(task.id.clone()),
                    framing: None,
                })
                .await
                .expect("compile");
            assert!(
                compiled
                    .items
                    .iter()
                    .any(|item| item.source_id == procedure_id.to_string()),
                "procedure must inject for its own strategy text"
            );
            client
                .finish_task(
                    &task.id,
                    &TaskFinishRequest {
                        outcome: TaskOutcome::Success,
                        accepted_by_user: Some(true),
                        final_commit: None,
                    },
                )
                .await
                .expect("finish use");
        }
        let skill = client
            .promote_skill(&SkillPromoteRequest {
                path: project.clone(),
                memory_id: procedure_id.clone(),
            })
            .await
            .expect("skill promotion");
        assert_eq!(skill.kind, "skill");

        drop(shutdown_sender);
        let _ = server.await;
    }

    fn git(path: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
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

    async fn search_labels(
        client: &DaemonClient,
        project_id: &ProjectId,
        query: &str,
    ) -> Vec<String> {
        client
            .search_world(&WorldSearchRequest {
                project_id: project_id.clone(),
                query: query.to_owned(),
                limit: 10,
            })
            .await
            .expect("world search")
            .into_iter()
            .map(|node| node.label)
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freshness_rescans_files_reverted_while_others_stay_dirty() {
        let temp = TempDir::new().expect("temp dir");
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        let alpha_original = "pub fn alpha_original() {}\n";
        std::fs::write(project.join("alpha.rs"), alpha_original).expect("alpha");
        std::fs::write(project.join("beta.rs"), "pub fn beta_original() {}\n").expect("beta");
        git(&project, &["init", "-q"]);
        git(
            &project,
            &["config", "user.email", "engram@example.invalid"],
        );
        git(&project, &["config", "user.name", "Engram Tests"]);
        git(&project, &["add", "alpha.rs", "beta.rs"]);
        git(&project, &["commit", "-qm", "initial"]);

        let config = test_config(&temp);
        let socket_path = config.daemon.socket_path.clone();
        let client = DaemonClient::new(&socket_path, Duration::from_secs(2));
        let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
        let server = tokio::spawn(serve_until(config, async move {
            let _ = shutdown_receiver.await;
        }));
        wait_until_ready(&client).await;

        let indexed = client
            .index_project_with_provider(&project, None, Some("tree_sitter".to_owned()), None)
            .await
            .expect("initial index");
        let project_id = indexed.resolved.project.id.clone();

        // Dirty both files, then freshen so the graph holds dirty content.
        std::fs::write(project.join("alpha.rs"), "pub fn alpha_dirty() {}\n").expect("alpha dirty");
        std::fs::write(project.join("beta.rs"), "pub fn beta_dirty() {}\n").expect("beta dirty");
        client
            .ensure_project_fresh(&project, Vec::new())
            .await
            .expect("dirty refresh");
        assert_eq!(
            search_labels(&client, &project_id, "alpha_dirty").await,
            ["alpha_dirty"]
        );

        // Revert only alpha; beta stays dirty, so porcelain status is not
        // empty and a naive plan would skip the reverted file entirely.
        std::fs::write(project.join("alpha.rs"), alpha_original).expect("alpha reverted");
        client
            .ensure_project_fresh(&project, Vec::new())
            .await
            .expect("revert refresh");
        assert_eq!(
            search_labels(&client, &project_id, "alpha_dirty").await,
            Vec::<String>::new(),
            "stale dirty-content node must not survive a partial revert"
        );
        assert_eq!(
            search_labels(&client, &project_id, "alpha_original").await,
            ["alpha_original"]
        );
        assert_eq!(
            search_labels(&client, &project_id, "beta_dirty").await,
            ["beta_dirty"]
        );

        drop(shutdown_sender);
        let _ = server.await;
    }
}
