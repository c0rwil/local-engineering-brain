//! Delivered-context use and Read volume from hook-observed tool calls.
//!
//! Native hooks report which repository files an agent read or edited, plus
//! Read flags and result sizes. Reports associate delivered source pointers
//! with later touches in the same agent session. A later read of a pointed
//! file does not prove that the pointer caused it. Observations keep
//! repository-relative paths, flags and sizes only: never file content,
//! shell commands or error text.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{ContextRunId, ProjectId, TaskId};
use crate::storage::Database;

use super::{EffectivenessStore, UsageMode, read_count, unix_time_ms};

/// Mode label for calls observed before any context was delivered in that
/// session and repository. It is not a control group.
pub const UNATTRIBUTED_MODE: &str = "unattributed";
const MAX_PATHS: usize = 32;
const MAX_PATH_BYTES: usize = 4096;
const SIZE_LIMIT_ERROR: &str = "exceeds maximum allowed";

/// Coarse class of an agent tool call. Only reads and edits (including file
/// writes) count as touches.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAction {
    Read,
    Edit,
    Search,
    Shell,
    Other,
}

impl ToolAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Edit => "edit",
            Self::Search => "search",
            Self::Shell => "shell",
            Self::Other => "other",
        }
    }

    /// Classify Claude Code and Codex tool names. Unknown tools are `other`.
    #[must_use]
    pub fn classify(tool_name: &str) -> Self {
        let name = tool_name.trim();
        let name = name.strip_prefix("functions.").unwrap_or(name);
        match name.to_ascii_lowercase().as_str() {
            "read" | "read_file" => Self::Read,
            "edit" | "multiedit" | "write" | "notebookedit" | "apply_patch" => Self::Edit,
            "grep" | "glob" | "grep_files" | "list_dir" | "ls" => Self::Search,
            "bash" | "shell" | "exec_command" | "local_shell" => Self::Shell,
            _ => Self::Other,
        }
    }

    const fn touches_files(self) -> bool {
        matches!(self, Self::Read | Self::Edit)
    }
}

/// The hook event that reported a call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    /// Before execution. Records the touch before any context it triggers.
    Pre,
    /// After a completed call.
    Post,
    /// After a failed or interrupted call.
    Failure,
}

/// One hook observation of an agent tool call in one repository. Pre- and
/// post-tool observations of the same call merge into a single record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolObservationRequest {
    pub project_id: ProjectId,
    /// The agent's session ID.
    pub session_id: String,
    /// The agent's tool-use ID. Only its digest with the session is stored.
    pub tool_use_id: String,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    /// Latest brain context delivered to this session and repository.
    #[serde(default)]
    pub context_run_id: Option<ContextRunId>,
    /// Producer, for example `claude_code` or `codex`.
    pub source: String,
    pub tool_name: String,
    pub phase: ToolPhase,
    /// Repository-relative files named by the call.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Read calls only: offset, limit or a page range was supplied.
    #[serde(default)]
    pub ranged: Option<bool>,
    /// Read calls only: UTF-8 bytes of text the tool reported returning.
    #[serde(default)]
    pub result_bytes: Option<i64>,
    /// Read calls only: rejected or truncated by the agent's Read limits.
    #[serde(default)]
    pub limit_hit: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolObservationResult {
    pub id: String,
    /// Digest of the session and tool-use IDs, shared with tool-call usage samples.
    pub call_id: String,
    pub inserted: bool,
}

/// The daemon's acknowledgement of a validated, timestamped and queued
/// observation. The write commits later at the lowest priority; project,
/// task and context scope are checked when it runs.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolObservationReceipt {
    pub call_id: String,
    /// Receipt time. It orders the touch against contexts compiled later.
    pub observed_at_ms: i64,
    pub queued: bool,
}

impl ToolObservationRequest {
    /// Check the request shape and return the stored call digest.
    ///
    /// # Errors
    /// Rejects malformed observations. Scope is checked by the write.
    pub fn call_id(&self) -> Result<String> {
        validate_observation(self)?;
        Ok(crate::agenthooks::tool_usage_sample_id(
            &self.session_id,
            &self.tool_use_id,
        ))
    }
}

/// Delivered-context use in one repository and period. Association only.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ContextUse {
    /// Distinct agent tool calls that hooks observed in this repository.
    pub observed_tool_calls: u64,
    /// Agent sessions with at least one observed call in this repository.
    pub observed_sessions: u64,
    /// Observed calls that named at least one file in this repository.
    pub observed_calls_with_paths: u64,
    /// Distinct session files first read or edited, by a completed call, in
    /// this period.
    pub touched_files: u64,
    /// Of those, files first touched before any context for this repository
    /// was delivered in that session.
    pub touched_files_before_context: u64,
    /// Delivered file pointers and later observed touches, by context mode.
    pub pointers_by_mode: BTreeMap<String, PointerUse>,
    /// Observed Read calls, by mode when the call was first observed.
    pub reads_by_mode: BTreeMap<String, ReadVolume>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct PointerUse {
    /// Task-linked contexts compiled for this repository in this period.
    pub delivered_contexts: u64,
    /// Delivered contexts with at least one file pointer.
    pub contexts_with_pointers: u64,
    /// Contexts with pointers followed by at least one tool call in the same
    /// agent session whose post-tool outcome was observed.
    pub observed_contexts: u64,
    /// Distinct context/file pointer pairs in observed contexts.
    pub pointers: u64,
    /// Pointer pairs in contexts without a later observed call. Excluded below.
    pub unobserved_pointers: u64,
    /// Source pointers that could not be resolved to a file path.
    pub unresolved_pointers: u64,
    /// Pointed files already read or edited in that session before compilation.
    /// Only completed calls are touches; failed and unfinished calls are not.
    pub already_touched: u64,
    /// Other pointed files read or edited by a completed call later in the
    /// same session.
    pub touched_later: u64,
    /// `touched_later / (pointers - already_touched)`; null without a denominator.
    pub precision: Option<f64>,
    /// Observed calls from delivery to the first touch, including that call and
    /// any failed or unfinished calls in between.
    pub first_touch_calls: FirstTouch,
    /// Distinct session files first touched in this period after a context of
    /// this mode was delivered in that session for their repository, or from a
    /// linked repository with a pointer to them.
    pub touched_files: u64,
    /// Of those, files that an earlier context in the session pointed to.
    pub touched_files_pointed: u64,
    /// `touched_files_pointed / touched_files`; null without a denominator.
    pub coverage: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct FirstTouch {
    pub samples: u64,
    pub mean: Option<f64>,
    pub median: Option<f64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct ReadVolume {
    /// Read calls with a reported post-tool outcome, completed or failed.
    pub reads: u64,
    /// Reads with offset, limit or a page range.
    pub ranged: u64,
    /// Reads without offset, limit or a page range.
    pub whole_file: u64,
    /// `ranged / (ranged + whole_file)`; reads without input flags are excluded.
    pub ranged_share: Option<f64>,
    /// Reads whose post-tool result reported a text size.
    pub sized_reads: u64,
    pub result_bytes: u64,
    /// `result_bytes / sized_reads`; null without sized reads.
    pub mean_result_bytes: Option<f64>,
    /// Reads rejected or truncated by the agent's Read size limits.
    pub limit_hits: u64,
    /// Reads whose post-tool hook reported a failure. Not file touches.
    pub failed: u64,
    /// Read calls seen only before execution: denied, interrupted, still
    /// running or missing their post-tool hook. Excluded from every figure above.
    pub unfinished: u64,
}

/// The phase a post-tool hook payload reports: completed or failed.
#[must_use]
pub fn post_tool_phase(payload: &serde_json::Value) -> ToolPhase {
    let failure = payload
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
        == Some("PostToolUseFailure")
        || payload.get("error").is_some_and(|error| !error.is_null());
    if failure {
        ToolPhase::Failure
    } else {
        ToolPhase::Post
    }
}

/// Build an observation from a native hook payload. Returns `None` unless the
/// agent supplied stable session and tool-use IDs. Only flags, sizes and the
/// given repository-relative paths are extracted.
#[must_use]
pub fn hook_tool_observation(
    payload: &serde_json::Value,
    session: &crate::agenthooks::HookSession,
    context_run_id: Option<ContextRunId>,
    paths: &[String],
    phase: ToolPhase,
    client: crate::agenthooks::HookClient,
) -> Option<ToolObservationRequest> {
    let session_id = session
        .session_id
        .as_ref()
        .filter(|id| !id.is_empty() && id.as_str() != "unknown-session")?;
    let tool_use_id = payload
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())?;
    let tool_name = payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("unknown");
    let mut request = ToolObservationRequest {
        project_id: session.project_id.clone(),
        session_id: session_id.clone(),
        tool_use_id: tool_use_id.to_owned(),
        task_id: Some(session.task_id.clone()),
        context_run_id,
        source: client.usage_source().to_owned(),
        tool_name: tool_name.chars().take(128).collect(),
        phase,
        // A repository root resolves to an empty path; keep the call, not the path.
        paths: paths
            .iter()
            .filter(|path| repository_relative(path))
            .take(MAX_PATHS)
            .cloned()
            .collect(),
        ranged: None,
        result_bytes: None,
        limit_hit: None,
    };
    if ToolAction::classify(tool_name) == ToolAction::Read {
        request.ranged = payload
            .get("tool_input")
            .and_then(serde_json::Value::as_object)
            .map(|input| {
                ["offset", "limit", "pages"]
                    .iter()
                    .any(|key| input.get(*key).is_some_and(|value| !value.is_null()))
            });
        match phase {
            ToolPhase::Pre => {}
            // A missing result or error text leaves the limit flag unknown.
            ToolPhase::Post => {
                let response = payload
                    .get("tool_response")
                    .or_else(|| payload.get("tool_result"));
                request.result_bytes = response.and_then(read_result_bytes);
                request.limit_hit = response.map(read_limit_hit);
            }
            ToolPhase::Failure => {
                request.limit_hit = payload
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(mentions_size_limit);
            }
        }
    }
    Some(request)
}

// Text size only. Images, PDFs and unknown shapes stay unknown.
fn read_result_bytes(response: &serde_json::Value) -> Option<i64> {
    let bytes = match response {
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Object(object) => {
            if object.get("type").and_then(serde_json::Value::as_str) == Some("file_unchanged") {
                0
            } else if let Some(content) = object
                .get("file")
                .and_then(|file| file.get("content"))
                .or_else(|| object.get("content"))
                .or_else(|| object.get("output"))
                .and_then(serde_json::Value::as_str)
            {
                content.len()
            } else {
                return None;
            }
        }
        _ => return None,
    };
    i64::try_from(bytes).ok()
}

fn read_limit_hit(response: &serde_json::Value) -> bool {
    match response {
        serde_json::Value::String(text) => mentions_size_limit(text),
        serde_json::Value::Object(object) => object
            .get("file")
            .and_then(|file| file.get("truncatedByTokenCap"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        _ => false,
    }
}

fn mentions_size_limit(text: &str) -> bool {
    text.to_ascii_lowercase().contains(SIZE_LIMIT_ERROR)
}

/// Strip a trailing line span (`:12`, `:12-40`, `#L12-L40`) so pointers
/// rendered with or without line ranges attribute to the same file.
#[must_use]
pub fn pointer_file(path: &str) -> &str {
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    let span = |value: &str, prefix: &str| match value.split_once('-') {
        Some((start, end)) => digits(start) && digits(end.strip_prefix(prefix).unwrap_or(end)),
        None => digits(value),
    };
    for (separator, prefix) in [("#L", "L"), (":", "")] {
        if let Some((file, range)) = path.rsplit_once(separator) {
            if !file.is_empty() && span(range, prefix) {
                return file;
            }
        }
    }
    path
}

impl EffectivenessStore {
    /// Record or merge one hook observation of an agent tool call.
    ///
    /// # Errors
    /// Rejects invalid observations, unknown or inconsistent references and
    /// storage failures.
    pub fn record_tool_observation(
        &self,
        request: &ToolObservationRequest,
    ) -> Result<ToolObservationResult> {
        self.record_tool_observation_at(request, unix_time_ms())
    }

    pub(crate) fn record_tool_observation_at(
        &self,
        request: &ToolObservationRequest,
        observed_at_ms: i64,
    ) -> Result<ToolObservationResult> {
        let action = validate_observation(request)?;
        if observed_at_ms < 0 {
            return Err(Error::Config(
                "tool observation time cannot be negative".to_owned(),
            ));
        }
        let call_id =
            crate::agenthooks::tool_usage_sample_id(&request.session_id, &request.tool_use_id);
        let mut paths: Vec<&str> = Vec::new();
        for path in &request.paths {
            if !paths.contains(&path.as_str()) {
                paths.push(path.as_str());
            }
        }
        let paths_json = serde_json::to_string(&paths)?;
        let mode = request.context_run_id.as_ref().map(|_| UsageMode::Brain);
        let outcome = match request.phase {
            ToolPhase::Pre => None,
            ToolPhase::Post => Some("ok"),
            ToolPhase::Failure => Some("failed"),
        };
        let now = unix_time_ms();
        self.database.write_immediate(|tx| {
            validate_observation_scope(tx, request)?;
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM tool_observations WHERE project_id = ?1 AND call_id = ?2",
                    params![request.project_id.as_str(), call_id],
                    |row| row.get(0),
                )
                .optional()?;
            let id = existing
                .clone()
                .unwrap_or_else(|| format!("tob_{}", uuid::Uuid::new_v4()));
            // The earliest observation defines when the touch happened and
            // which delivered context preceded it; results fill in later.
            tx.execute(
                "INSERT INTO tool_observations
                 (id, project_id, call_id, session_id, task_id, context_run_id, mode, source,
                  tool_name, action, paths_json, ranged, result_bytes, limit_hit, outcome,
                  observed_at_ms, recorded_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                         ?16, ?17, ?17)
                 ON CONFLICT(project_id, call_id) DO UPDATE SET
                   task_id = coalesce(tool_observations.task_id, excluded.task_id),
                   context_run_id = CASE WHEN excluded.observed_at_ms < tool_observations.observed_at_ms
                     THEN excluded.context_run_id ELSE tool_observations.context_run_id END,
                   mode = CASE WHEN excluded.observed_at_ms < tool_observations.observed_at_ms
                     THEN excluded.mode ELSE tool_observations.mode END,
                   paths_json = CASE WHEN tool_observations.paths_json = '[]'
                     THEN excluded.paths_json ELSE tool_observations.paths_json END,
                   ranged = coalesce(tool_observations.ranged, excluded.ranged),
                   result_bytes = coalesce(tool_observations.result_bytes, excluded.result_bytes),
                   limit_hit = CASE WHEN tool_observations.limit_hit IS NULL THEN excluded.limit_hit
                     WHEN excluded.limit_hit IS NULL THEN tool_observations.limit_hit
                     ELSE max(tool_observations.limit_hit, excluded.limit_hit) END,
                   outcome = CASE WHEN 'failed' IN (tool_observations.outcome, excluded.outcome)
                     THEN 'failed' ELSE coalesce(tool_observations.outcome, excluded.outcome) END,
                   observed_at_ms = min(tool_observations.observed_at_ms, excluded.observed_at_ms),
                   updated_at_ms = excluded.updated_at_ms",
                params![
                    id,
                    request.project_id.as_str(),
                    call_id,
                    request.session_id,
                    request.task_id.as_ref().map(TaskId::as_str),
                    request.context_run_id.as_ref().map(ContextRunId::as_str),
                    mode.map(UsageMode::as_str),
                    request.source,
                    request.tool_name,
                    action.as_str(),
                    paths_json,
                    request.ranged,
                    request.result_bytes,
                    request.limit_hit,
                    outcome,
                    observed_at_ms,
                    now,
                ],
            )?;
            Ok(ToolObservationResult {
                id,
                call_id: call_id.clone(),
                inserted: existing.is_none(),
            })
        })
    }
}

/// A bounded path inside the repository, in the normalized form hooks derive
/// from the checkout root: relative, without control characters, and without
/// empty, `.` or `..` segments, which could name files outside the repository
/// or never match a pointer.
fn repository_relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= MAX_PATH_BYTES
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|segment| !matches!(segment, "" | "." | ".."))
}

fn validate_observation(request: &ToolObservationRequest) -> Result<ToolAction> {
    for (label, value, limit) in [
        ("session_id", request.session_id.as_str(), 256),
        ("tool_use_id", request.tool_use_id.as_str(), 256),
        ("source", request.source.as_str(), 128),
        ("tool_name", request.tool_name.as_str(), 128),
    ] {
        if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control) {
            return Err(Error::Config(format!(
                "tool observation {label} must be nonempty, bounded text without control characters"
            )));
        }
    }
    if request.paths.len() > MAX_PATHS {
        return Err(Error::Config(format!(
            "tool observation accepts at most {MAX_PATHS} paths"
        )));
    }
    if !request.paths.iter().all(|path| repository_relative(path)) {
        return Err(Error::Config(
            "tool observation paths must be bounded, normalized repository-relative paths without `.` or `..` segments"
                .to_owned(),
        ));
    }
    let action = ToolAction::classify(&request.tool_name);
    let read_metrics =
        request.ranged.is_some() || request.result_bytes.is_some() || request.limit_hit.is_some();
    if action != ToolAction::Read && read_metrics {
        return Err(Error::Config(
            "Read measurements apply only to read calls".to_owned(),
        ));
    }
    if request.phase == ToolPhase::Pre
        && (request.result_bytes.is_some() || request.limit_hit.is_some())
    {
        return Err(Error::Config(
            "pre-tool observations cannot report results".to_owned(),
        ));
    }
    if request.result_bytes.is_some_and(|bytes| bytes < 0) {
        return Err(Error::Config(
            "tool observation result_bytes cannot be negative".to_owned(),
        ));
    }
    Ok(action)
}

fn validate_observation_scope(
    connection: &Connection,
    request: &ToolObservationRequest,
) -> Result<()> {
    let project_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND tombstoned_at_ms IS NULL)",
        [request.project_id.as_str()],
        |row| row.get(0),
    )?;
    if !project_exists {
        return Err(Error::Config(
            "tool observation project is unknown or tombstoned".to_owned(),
        ));
    }
    if let Some(task_id) = &request.task_id {
        let task: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT project_id, session_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, session)) = task else {
            return Err(Error::Config("tool observation task is unknown".to_owned()));
        };
        if project != request.project_id.as_str()
            || session.is_some_and(|session| session != request.session_id)
        {
            return Err(Error::Config(
                "tool observation task does not match the project or session".to_owned(),
            ));
        }
    }
    if let Some(context_id) = &request.context_run_id {
        let context: Option<(String, String)> = connection
            .query_row(
                "SELECT project_id, mode FROM context_runs WHERE id = ?1",
                [context_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, mode)) = context else {
            return Err(Error::Config(
                "tool observation context is unknown".to_owned(),
            ));
        };
        if project != request.project_id.as_str() || mode != UsageMode::Brain.as_str() {
            return Err(Error::Config(
                "tool observation context must be brain context for the same project".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Delete one bounded batch of observations older than the retention cutoff.
///
/// # Errors
/// Returns storage errors.
pub fn delete_tool_observation_batch(
    database: &Database,
    cutoff_ms: i64,
    limit: usize,
) -> Result<u64> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    database.write_immediate(|transaction| {
        let deleted = transaction.execute(
            "DELETE FROM tool_observations WHERE id IN (
                 SELECT id FROM tool_observations WHERE observed_at_ms < ?1
                 ORDER BY observed_at_ms, id LIMIT ?2)",
            params![cutoff_ms, limit],
        )?;
        u64::try_from(deleted).map_err(|_| Error::Config("GC batch exceeds u64".to_owned()))
    })
}

/// A task-linked context and its distinct file pointers.
struct Pack {
    id: String,
    project_id: String,
    mode: String,
    created_at_ms: i64,
    session_id: Option<String>,
    pointers: Vec<(String, String)>,
    unresolved: u64,
}

impl Pack {
    fn points_to(&self, project: &str, path: &str) -> bool {
        self.pointers
            .iter()
            .any(|(pointer_project, file)| pointer_project == project && file == path)
    }
}

struct Observation {
    call_id: String,
    project_id: String,
    action: ToolAction,
    paths: Vec<String>,
    observed_at_ms: i64,
    /// A post-tool hook reported an outcome, completed or failed.
    reported: bool,
    /// A post-tool hook reported the call as completed.
    completed: bool,
}

impl Observation {
    /// A completed read or edit of the file. Calls seen only before execution
    /// (denied, interrupted or unfinished) and failed calls are not touches.
    /// A completed call keeps its earliest observation time.
    fn touches(&self, project: &str, path: &str) -> bool {
        self.completed
            && self.action.touches_files()
            && self.project_id == project
            && self.paths.iter().any(|candidate| candidate == path)
    }
}

/// Summarize hook-observed context use for one repository and period.
pub(super) fn report(
    connection: &Connection,
    project: &ProjectId,
    since: i64,
    until: i64,
) -> Result<ContextUse> {
    let scope = params![project.as_str(), since, until];
    let mut usage = connection.query_row(
        "SELECT count(DISTINCT call_id), count(DISTINCT session_id),
                count(DISTINCT CASE WHEN paths_json <> '[]' THEN call_id END)
         FROM tool_observations
         WHERE project_id = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3",
        scope,
        |row| {
            Ok(ContextUse {
                observed_tool_calls: read_count(row, 0)?,
                observed_sessions: read_count(row, 1)?,
                observed_calls_with_paths: read_count(row, 2)?,
                ..ContextUse::default()
            })
        },
    )?;
    usage.reads_by_mode = read_volume(connection, project, since, until)?;
    let period = pack_headers(
        connection,
        "c.project_id = ?1 AND c.created_at_ms >= ?2 AND c.created_at_ms < ?3",
        scope,
    )?;
    let observed_sessions: Vec<String> = connection
        .prepare(
            "SELECT DISTINCT session_id FROM tool_observations
             WHERE project_id = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3
             ORDER BY session_id",
        )?
        .query_map(scope, |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let sessions: BTreeSet<&str> = period
        .iter()
        .filter_map(|pack| pack.session_id.as_deref())
        .chain(observed_sessions.iter().map(String::as_str))
        .collect();
    let sessions_json = serde_json::to_string(&sessions)?;
    let mut packs = period;
    for pack in pack_headers(
        connection,
        "c.created_at_ms < ?2 AND t.session_id IN (SELECT value FROM json_each(?1))",
        params![sessions_json, until],
    )? {
        if !packs.iter().any(|known| known.id == pack.id) {
            packs.push(pack);
        }
    }
    resolve_pointers(connection, &mut packs)?;
    let observations = session_observations(connection, &sessions_json, until)?;
    let mut distances: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for pack in packs
        .iter()
        .filter(|pack| pack.project_id == project.as_str() && pack.created_at_ms >= since)
    {
        let calls = pack
            .session_id
            .as_ref()
            .and_then(|session| observations.get(session))
            .map_or(&[][..], Vec::as_slice);
        let entry = usage.pointers_by_mode.entry(pack.mode.clone()).or_default();
        attribute_pack(
            entry,
            pack,
            calls,
            distances.entry(pack.mode.clone()).or_default(),
        );
    }
    let mut by_session: HashMap<&str, Vec<&Pack>> = HashMap::new();
    for pack in &packs {
        if let Some(session) = pack.session_id.as_deref() {
            by_session.entry(session).or_default().push(pack);
        }
    }
    for session in &observed_sessions {
        let calls = observations.get(session).map_or(&[][..], Vec::as_slice);
        let earlier = by_session
            .get(session.as_str())
            .map_or(&[][..], Vec::as_slice);
        attribute_coverage(&mut usage, project.as_str(), since, until, calls, earlier);
    }
    for (mode, entry) in &mut usage.pointers_by_mode {
        entry.precision = ratio(
            entry.touched_later,
            entry.pointers.saturating_sub(entry.already_touched),
        );
        entry.coverage = ratio(entry.touched_files_pointed, entry.touched_files);
        entry.first_touch_calls = summarize(distances.remove(mode).unwrap_or_default());
    }
    Ok(usage)
}

// Precision for one delivered context: a pointed file read or edited before
// compilation was already in scope; one touched afterwards may have followed it.
fn attribute_pack(
    entry: &mut PointerUse,
    pack: &Pack,
    calls: &[Observation],
    distances: &mut Vec<u64>,
) {
    entry.delivered_contexts += 1;
    entry.unresolved_pointers += pack.unresolved;
    if pack.pointers.is_empty() {
        return;
    }
    entry.contexts_with_pointers += 1;
    let pointers = u64::try_from(pack.pointers.len()).unwrap_or(u64::MAX);
    // Without a later call whose post-tool outcome was observed there was no
    // observable opportunity to follow the pointers; that is unknown, not unused.
    if !calls
        .iter()
        .any(|call| call.reported && call.observed_at_ms > pack.created_at_ms)
    {
        entry.unobserved_pointers += pointers;
        return;
    }
    entry.observed_contexts += 1;
    entry.pointers += pointers;
    for (project, path) in &pack.pointers {
        // Calls are ordered by observation time.
        let Some(first) = calls.iter().find(|call| call.touches(project, path)) else {
            continue;
        };
        if first.observed_at_ms <= pack.created_at_ms {
            entry.already_touched += 1;
            continue;
        }
        entry.touched_later += 1;
        let between: BTreeSet<&str> = calls
            .iter()
            .filter(|call| {
                call.observed_at_ms > pack.created_at_ms
                    && call.observed_at_ms <= first.observed_at_ms
            })
            .map(|call| call.call_id.as_str())
            .collect();
        distances.push(u64::try_from(between.len()).unwrap_or(u64::MAX));
    }
}

// Coverage for one session: the first touch of each repository file, against
// earlier contexts that could point into this repository.
fn attribute_coverage(
    usage: &mut ContextUse,
    project: &str,
    since: i64,
    until: i64,
    calls: &[Observation],
    packs: &[&Pack],
) {
    let mut first_touches: BTreeMap<&str, i64> = BTreeMap::new();
    for call in calls
        .iter()
        .filter(|call| call.completed && call.action.touches_files() && call.project_id == project)
    {
        for path in &call.paths {
            first_touches
                .entry(path.as_str())
                .or_insert(call.observed_at_ms);
        }
    }
    for (path, first) in first_touches {
        if first < since || first >= until {
            continue;
        }
        usage.touched_files += 1;
        let earlier: Vec<&Pack> = packs
            .iter()
            .copied()
            .filter(|pack| {
                pack.created_at_ms < first
                    && (pack.project_id == project || pack.points_to(project, path))
            })
            .collect();
        if earlier.is_empty() {
            usage.touched_files_before_context += 1;
            continue;
        }
        let modes: BTreeSet<&str> = earlier.iter().map(|pack| pack.mode.as_str()).collect();
        for mode in modes {
            let entry = usage.pointers_by_mode.entry(mode.to_owned()).or_default();
            entry.touched_files += 1;
            if earlier
                .iter()
                .any(|pack| pack.mode == mode && pack.points_to(project, path))
            {
                entry.touched_files_pointed += 1;
            }
        }
    }
}

fn pack_headers(
    connection: &Connection,
    filter: &str,
    parameters: impl rusqlite::Params,
) -> Result<Vec<Pack>> {
    // A context is linked to at most one task; keep the first link if legacy
    // rows disagree.
    let sql = format!(
        "SELECT c.id, c.project_id, c.mode, c.created_at_ms, t.session_id
         FROM context_runs c
         JOIN usage_events u ON u.context_run_id = c.id AND u.source_type = 'task'
              AND u.usage_kind = 'context_served'
         JOIN tasks t ON t.id = u.source_id
         WHERE {filter}
         ORDER BY c.created_at_ms, c.id, u.created_at_ms, t.id"
    );
    let mut packs: Vec<Pack> = Vec::new();
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(parameters, |row| {
        Ok(Pack {
            id: row.get(0)?,
            project_id: row.get(1)?,
            mode: row.get(2)?,
            created_at_ms: row.get(3)?,
            session_id: row.get(4)?,
            pointers: Vec::new(),
            unresolved: 0,
        })
    })?;
    for pack in rows {
        let pack = pack?;
        if packs.last().is_none_or(|last| last.id != pack.id) {
            packs.push(pack);
        }
    }
    Ok(packs)
}

// Pointers resolve through their indexed node to a file path, so rendering
// with or without line ranges does not change attribution.
fn resolve_pointers(connection: &Connection, packs: &mut [Pack]) -> Result<()> {
    let ids: Vec<&str> = packs.iter().map(|pack| pack.id.as_str()).collect();
    let mut statement = connection.prepare(
        "SELECT i.context_run_id, i.source_id, n.project_id, n.file_path
         FROM context_items i LEFT JOIN nodes n ON n.id = i.source_id
         WHERE i.context_run_id IN (SELECT value FROM json_each(?1))
           AND i.source_type IN ('code', 'code_dependency', 'code_use')
         ORDER BY i.context_run_id, i.ordinal",
    )?;
    let rows = statement
        .query_map([serde_json::to_string(&ids)?], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut node = connection.prepare(
        "SELECT project_id, file_path FROM nodes WHERE id = ?1 AND file_path IS NOT NULL",
    )?;
    let index: HashMap<String, usize> = packs
        .iter()
        .enumerate()
        .map(|(position, pack)| (pack.id.clone(), position))
        .collect();
    for (run, source, node_project, node_path) in rows {
        let Some(&position) = index.get(&run) else {
            continue;
        };
        let pack = &mut packs[position];
        // A source ID carrying a line span still names its node. Anything
        // else stays unresolved and outside precision, rather than guessed.
        let resolved = match (node_project, node_path) {
            (Some(project), Some(path)) => Some((project, path)),
            _ if pointer_file(&source) != source => node
                .query_row([pointer_file(&source)], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?,
            _ => None,
        };
        let Some((project, path)) = resolved else {
            pack.unresolved += 1;
            continue;
        };
        let pointer = (project, pointer_file(&path).to_owned());
        if !pack.pointers.contains(&pointer) {
            pack.pointers.push(pointer);
        }
    }
    Ok(())
}

fn session_observations(
    connection: &Connection,
    sessions_json: &str,
    until: i64,
) -> Result<HashMap<String, Vec<Observation>>> {
    let mut statement = connection.prepare(
        "SELECT session_id, call_id, project_id, action, paths_json, observed_at_ms,
                outcome IS NOT NULL, outcome IS 'ok'
         FROM tool_observations
         WHERE observed_at_ms < ?2 AND session_id IN (SELECT value FROM json_each(?1))
         ORDER BY session_id, observed_at_ms, call_id, project_id",
    )?;
    let rows = statement
        .query_map(params![sessions_json, until], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, bool>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut observations: HashMap<String, Vec<Observation>> = HashMap::new();
    for (session, call_id, project_id, action, paths, observed_at_ms, reported, completed) in rows {
        let paths: Vec<String> = serde_json::from_str(&paths)?;
        observations.entry(session).or_default().push(Observation {
            call_id,
            project_id,
            action: parse_action(&action)?,
            paths: paths
                .iter()
                .map(|path| pointer_file(path).to_owned())
                .collect(),
            observed_at_ms,
            reported,
            completed,
        });
    }
    Ok(observations)
}

fn parse_action(value: &str) -> Result<ToolAction> {
    Ok(match value {
        "read" => ToolAction::Read,
        "edit" => ToolAction::Edit,
        "search" => ToolAction::Search,
        "shell" => ToolAction::Shell,
        "other" => ToolAction::Other,
        _ => {
            return Err(Error::Config(
                "invalid stored tool observation action".to_owned(),
            ));
        }
    })
}

fn read_volume(
    connection: &Connection,
    project: &ProjectId,
    since: i64,
    until: i64,
) -> Result<BTreeMap<String, ReadVolume>> {
    // Only calls with a post-tool outcome executed. Pre-only rows (denied,
    // interrupted or unfinished calls) are counted apart, never as reads.
    let mut statement = connection.prepare(
        "SELECT coalesce(mode, ?4), count(outcome),
                count(CASE WHEN outcome IS NOT NULL AND ranged = 1 THEN 1 END),
                count(CASE WHEN outcome IS NOT NULL AND ranged = 0 THEN 1 END),
                count(CASE WHEN outcome IS NOT NULL THEN result_bytes END),
                coalesce(sum(CASE WHEN outcome IS NOT NULL THEN result_bytes END), 0),
                count(CASE WHEN outcome IS NOT NULL AND limit_hit = 1 THEN 1 END),
                count(CASE WHEN outcome = 'failed' THEN 1 END),
                count(CASE WHEN outcome IS NULL THEN 1 END)
         FROM tool_observations
         WHERE project_id = ?1 AND action = 'read' AND observed_at_ms >= ?2 AND observed_at_ms < ?3
         GROUP BY 1",
    )?;
    let rows = statement.query_map(
        params![project.as_str(), since, until, UNATTRIBUTED_MODE],
        |row| {
            let reads = ReadVolume {
                reads: read_count(row, 1)?,
                ranged: read_count(row, 2)?,
                whole_file: read_count(row, 3)?,
                sized_reads: read_count(row, 4)?,
                result_bytes: read_count(row, 5)?,
                limit_hits: read_count(row, 6)?,
                failed: read_count(row, 7)?,
                unfinished: read_count(row, 8)?,
                ..ReadVolume::default()
            };
            Ok((row.get::<_, String>(0)?, reads))
        },
    )?;
    let mut volume = BTreeMap::new();
    for row in rows {
        let (mode, mut reads) = row?;
        reads.ranged_share = ratio(reads.ranged, reads.ranged + reads.whole_file);
        reads.mean_result_bytes = ratio(reads.result_bytes, reads.sized_reads);
        volume.insert(mode, reads);
    }
    Ok(volume)
}

#[allow(clippy::cast_precision_loss)]
fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

#[allow(clippy::cast_precision_loss)]
fn summarize(mut samples: Vec<u64>) -> FirstTouch {
    samples.sort_unstable();
    let count = samples.len();
    let median = match count {
        0 => None,
        _ if count % 2 == 1 => Some(samples[count / 2] as f64),
        _ => Some((samples[count / 2 - 1] + samples[count / 2]) as f64 / 2.0),
    };
    FirstTouch {
        samples: u64::try_from(count).unwrap_or(u64::MAX),
        mean: ratio(
            samples.iter().sum(),
            u64::try_from(count).unwrap_or(u64::MAX),
        ),
        median,
    }
}

impl ContextUse {
    /// Terminal summary lines, empty when nothing was delivered or observed.
    /// Unknown ratios print as unknown, never zero.
    #[must_use]
    pub fn summary_lines(&self) -> Vec<String> {
        if self.observed_tool_calls == 0 && self.pointers_by_mode.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "  context use (association, not causation): observed tool calls {}, agent sessions {}; \
             files first touched {}, before any delivered context {}",
            self.observed_tool_calls,
            self.observed_sessions,
            self.touched_files,
            self.touched_files_before_context
        )];
        for (mode, pointers) in &self.pointers_by_mode {
            lines.push(format!(
                "    {mode} pointers: observed contexts {} of {} with file pointers; already touched {}; \
                 touched later {} of {} ({}); median calls to first touch {}",
                pointers.observed_contexts,
                pointers.contexts_with_pointers,
                pointers.already_touched,
                pointers.touched_later,
                pointers.pointers.saturating_sub(pointers.already_touched),
                percent(pointers.precision),
                pointers
                    .first_touch_calls
                    .median
                    .map_or_else(|| "unknown".to_owned(), |median| format!("{median:.1}")),
            ));
            lines.push(format!(
                "    {mode} coverage: pointed to before first touch {} of {} files ({})",
                pointers.touched_files_pointed,
                pointers.touched_files,
                percent(pointers.coverage)
            ));
        }
        for (mode, reads) in &self.reads_by_mode {
            lines.push(format!(
                "    {mode} reads: {} ({} ranged, {} whole-file; {} ranged); mean result {} over {} sized; \
                 limit hits {}; failed {}; unfinished {}",
                reads.reads,
                reads.ranged,
                reads.whole_file,
                percent(reads.ranged_share),
                reads.mean_result_bytes.map_or_else(
                    || "unknown".to_owned(),
                    |bytes| format!("{:.1} KiB", bytes / 1024.0)
                ),
                reads.sized_reads,
                reads.limit_hits,
                reads.failed,
                reads.unfinished
            ));
        }
        lines
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(
        || "unknown".to_owned(),
        |value| format!("{:.1}%", value * 100.0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const APP_A: &str = "nod_app_a";
    const APP_B: &str = "nod_app_b";
    const APP_C: &str = "nod_app_c";
    const APP_D: &str = "nod_app_d";
    const LIB_L: &str = "nod_lib_l";

    struct Fixture {
        _temp: tempfile::TempDir,
        database: Database,
        store: EffectivenessStore,
        app: ProjectId,
        lib: ProjectId,
        app_task: TaskId,
        lib_task: TaskId,
        other_task: TaskId,
        detached_task: TaskId,
    }

    fn fixture() -> Fixture {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let (app, lib) = (ProjectId::new(), ProjectId::new());
        let tasks = [TaskId::new(), TaskId::new(), TaskId::new(), TaskId::new()];
        database
            .write_immediate(|tx| {
                for (project, name) in [(&app, "app"), (&lib, "lib")] {
                    tx.execute(
                        "INSERT INTO projects (id,name,created_at_ms) VALUES (?1,?2,1)",
                        params![project.as_str(), name],
                    )?;
                }
                for (id, project, path) in [
                    (APP_A, &app, "src/a.rs"),
                    (APP_B, &app, "src/b.rs"),
                    (APP_C, &app, "src/c.rs"),
                    (APP_D, &app, "src/d.rs"),
                    (LIB_L, &lib, "lib/l.rs"),
                ] {
                    tx.execute(
                        "INSERT INTO nodes (id,project_id,kind,stable_key,label,file_path,current_digest,source_kind,source_ref,extractor_version,observed_at_ms)
                         VALUES (?1,?2,'Function',?1,?1,?3,'digest','static_analysis','head','v1',1)",
                        params![id, project.as_str(), path],
                    )?;
                }
                for (task, project, session) in [
                    (&tasks[0], &app, Some("s1")),
                    (&tasks[1], &lib, Some("s1")),
                    (&tasks[2], &app, Some("s2")),
                    (&tasks[3], &app, None),
                ] {
                    tx.execute(
                        "INSERT INTO tasks (id,project_id,session_id,prompt,prompt_digest,status,started_at_ms)
                         VALUES (?1,?2,?3,'synthetic task','digest','open',1)",
                        params![task.as_str(), project.as_str(), session],
                    )?;
                }
                Ok(())
            })
            .expect("fixtures");
        let [app_task, lib_task, other_task, detached_task] = tasks;
        Fixture {
            _temp: temp,
            store: EffectivenessStore::new(database.clone()),
            database,
            app,
            lib,
            app_task,
            lib_task,
            other_task,
            detached_task,
        }
    }

    impl Fixture {
        /// A task-linked brain context whose code items point at nodes.
        fn context(&self, task: &TaskId, created_at_ms: i64, sources: &[&str]) -> ContextRunId {
            let id = ContextRunId::new();
            self.database
                .write_immediate(|tx| {
                    tx.execute(
                        "INSERT INTO context_runs (id,request_id,project_id,request_fingerprint,mode,task_text_digest,token_budget,selected_tokens,candidate_count,stable_prefix_hash,stable_prefix,dynamic_context,compile_ms,created_at_ms)
                         SELECT ?1,?1,project_id,'fingerprint','brain','digest',1000,10,3,'hash','','map',1,?3 FROM tasks WHERE id=?2",
                        params![id.as_str(), task.as_str(), created_at_ms],
                    )?;
                    for (ordinal, source) in sources.iter().enumerate() {
                        let kind = if source.starts_with("mem_") { "memory" } else { "code" };
                        tx.execute(
                            "INSERT INTO context_items (context_run_id,ordinal,source_type,source_id,score,tokens,why_json) VALUES (?1,?2,?3,?4,1.0,5,'{}')",
                            params![id.as_str(), i64::try_from(ordinal).unwrap() + 1, kind, source],
                        )?;
                    }
                    tx.execute(
                        "INSERT INTO usage_events (id,source_type,source_id,context_run_id,usage_kind,created_at_ms) VALUES (?1,'task',?2,?3,'context_served',?4)",
                        params![format!("use_{}", uuid::Uuid::new_v4()), task.as_str(), id.as_str(), created_at_ms],
                    )?;
                    Ok(())
                })
                .expect("context");
            id
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        project: &ProjectId,
        task: &TaskId,
        session: &str,
        tool_use_id: &str,
        tool_name: &str,
        phase: ToolPhase,
        paths: &[&str],
        context: Option<&ContextRunId>,
    ) -> ToolObservationRequest {
        ToolObservationRequest {
            project_id: project.clone(),
            session_id: session.to_owned(),
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task.clone()),
            context_run_id: context.cloned(),
            source: "claude_code".to_owned(),
            tool_name: tool_name.to_owned(),
            phase,
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
            ranged: None,
            result_bytes: None,
            limit_hit: None,
        }
    }

    fn read(
        mut request: ToolObservationRequest,
        ranged: bool,
        bytes: Option<i64>,
        limit_hit: Option<bool>,
    ) -> ToolObservationRequest {
        request.ranged = Some(ranged);
        request.result_bytes = bytes;
        request.limit_hit = limit_hit;
        request
    }

    fn session(
        project: &ProjectId,
        task: &TaskId,
        id: Option<&str>,
    ) -> crate::agenthooks::HookSession {
        serde_json::from_value(json!({
            "session_id": id, "task_id": task, "project_id": project
        }))
        .expect("hook session")
    }

    #[test]
    fn tool_names_classify_into_touch_actions() {
        for (name, action) in [
            ("Read", ToolAction::Read),
            ("read_file", ToolAction::Read),
            ("functions.read_file", ToolAction::Read),
            ("MultiEdit", ToolAction::Edit),
            ("Write", ToolAction::Edit),
            ("apply_patch", ToolAction::Edit),
            ("NotebookEdit", ToolAction::Edit),
            ("Grep", ToolAction::Search),
            ("Bash", ToolAction::Shell),
            ("functions.exec_command", ToolAction::Shell),
            ("WebFetch", ToolAction::Other),
        ] {
            assert_eq!(ToolAction::classify(name), action, "{name}");
        }
    }

    #[test]
    fn pointer_files_ignore_line_spans_only() {
        for (input, expected) in [
            ("src/a.rs", "src/a.rs"),
            ("src/a.rs:12", "src/a.rs"),
            ("src/a.rs:12-40", "src/a.rs"),
            ("src/a.rs#L12-L40", "src/a.rs"),
            ("src/a.rs#L7", "src/a.rs"),
            ("nod_x:3-9", "nod_x"),
            ("docs/a:b.md", "docs/a:b.md"),
            ("src/a.rs:", "src/a.rs:"),
            (":12", ":12"),
            ("src/a.rs:12-", "src/a.rs:12-"),
        ] {
            assert_eq!(pointer_file(input), expected, "{input}");
        }
    }

    #[test]
    fn claude_read_payloads_yield_flags_and_sizes_without_content() {
        let f = fixture();
        let hook = session(&f.app, &f.app_task, Some("s1"));
        let paths = vec!["src/a.rs".to_owned()];
        let pre = json!({"session_id":"s1","tool_name":"Read","tool_use_id":"toolu_1",
            "hook_event_name":"PreToolUse",
            "tool_input":{"file_path":"/repo/src/a.rs","offset":10,"limit":40}});
        let observed = hook_tool_observation(
            &pre,
            &hook,
            None,
            &paths,
            ToolPhase::Pre,
            crate::agenthooks::HookClient::ClaudeCode,
        )
        .expect("observation");
        assert_eq!(
            (observed.ranged, observed.result_bytes, observed.limit_hit),
            (Some(true), None, None)
        );
        assert_eq!(observed.source, "claude_code");
        let mut post = pre.clone();
        post["hook_event_name"] = json!("PostToolUse");
        post["tool_input"] = json!({"file_path":"/repo/src/a.rs"});
        post["tool_response"] = json!({"type":"text","file":{"filePath":"/repo/src/a.rs",
            "content":"synthetic body","numLines":1,"startLine":1,"totalLines":1}});
        assert_eq!(post_tool_phase(&post), ToolPhase::Post);
        let observed = hook_tool_observation(
            &post,
            &hook,
            None,
            &paths,
            post_tool_phase(&post),
            crate::agenthooks::HookClient::ClaudeCode,
        )
        .expect("observation");
        assert_eq!(
            (observed.ranged, observed.result_bytes, observed.limit_hit),
            (Some(false), Some(14), Some(false))
        );
        let serialized = serde_json::to_string(&observed).unwrap();
        assert!(
            !serialized.contains("synthetic body"),
            "content never leaves the hook"
        );
        post["tool_response"]["file"]["truncatedByTokenCap"] = json!(true);
        let truncated = hook_tool_observation(
            &post,
            &hook,
            None,
            &paths,
            ToolPhase::Post,
            crate::agenthooks::HookClient::ClaudeCode,
        )
        .unwrap();
        assert_eq!(truncated.limit_hit, Some(true));
        post["tool_response"] =
            json!({"type":"file_unchanged","file":{"filePath":"/repo/src/a.rs"}});
        let unchanged = hook_tool_observation(
            &post,
            &hook,
            None,
            &paths,
            ToolPhase::Post,
            crate::agenthooks::HookClient::ClaudeCode,
        )
        .unwrap();
        assert_eq!(unchanged.result_bytes, Some(0));
        post["tool_response"] = json!({"type":"image","file":{"base64":"AAAA","type":"image/png"}});
        let image = hook_tool_observation(
            &post,
            &hook,
            None,
            &paths,
            ToolPhase::Post,
            crate::agenthooks::HookClient::ClaudeCode,
        )
        .unwrap();
        assert_eq!(
            image.result_bytes, None,
            "non-text results have unknown size"
        );
        let failure = json!({"session_id":"s1","tool_name":"Read","tool_use_id":"toolu_2",
            "hook_event_name":"PostToolUseFailure","tool_input":{"file_path":"/repo/big.rs"},
            "error":"File content (300KB) exceeds maximum allowed size (256KB).","is_interrupt":false});
        assert_eq!(post_tool_phase(&failure), ToolPhase::Failure);
        let failed = hook_tool_observation(
            &failure,
            &hook,
            None,
            &paths,
            ToolPhase::Failure,
            crate::agenthooks::HookClient::ClaudeCode,
        )
        .unwrap();
        assert_eq!(
            (failed.ranged, failed.result_bytes, failed.limit_hit),
            (Some(false), None, Some(true))
        );
        assert!(!serde_json::to_string(&failed).unwrap().contains("300KB"));
    }

    #[test]
    fn codex_and_non_read_payloads_keep_unknowns_and_require_real_ids() {
        let f = fixture();
        let hook = session(&f.app, &f.app_task, Some("s1"));
        let paths = vec!["src/a.rs".to_owned(), "src/b.rs".to_owned()];
        let read = json!({"session_id":"s1","tool_name":"read_file","tool_use_id":"call_1",
            "tool_input":{"file_path":"src/a.rs"},"tool_response":"L1: synthetic\nL2: text"});
        let observed = hook_tool_observation(
            &read,
            &hook,
            None,
            &paths[..1],
            ToolPhase::Post,
            crate::agenthooks::HookClient::Codex,
        )
        .unwrap();
        assert_eq!(observed.source, "codex");
        assert_eq!(
            (observed.ranged, observed.result_bytes, observed.limit_hit),
            (Some(false), Some(22), Some(false))
        );
        let patch = json!({"session_id":"s1","tool_name":"apply_patch","tool_use_id":"call_2",
            "tool_input":{"command":"*** Begin Patch\n*** End Patch"},"tool_response":{"exit_code":0}});
        let edit = hook_tool_observation(
            &patch,
            &hook,
            None,
            &paths,
            ToolPhase::Post,
            crate::agenthooks::HookClient::Codex,
        )
        .unwrap();
        assert_eq!(edit.paths, paths);
        assert_eq!(
            (edit.ranged, edit.result_bytes, edit.limit_hit),
            (None, None, None)
        );
        // A repository-root argument keeps the call but not an empty path.
        let rooted = [
            String::new(),
            "/abs/x.rs".to_owned(),
            "../outside.rs".to_owned(),
            "src/a.rs".to_owned(),
        ];
        let scoped = hook_tool_observation(
            &patch,
            &hook,
            None,
            &rooted,
            ToolPhase::Post,
            crate::agenthooks::HookClient::Codex,
        )
        .unwrap();
        assert_eq!(scoped.paths, vec!["src/a.rs".to_owned()]);
        let mut anonymous = patch.clone();
        anonymous.as_object_mut().unwrap().remove("tool_use_id");
        assert!(
            hook_tool_observation(
                &anonymous,
                &hook,
                None,
                &paths,
                ToolPhase::Post,
                crate::agenthooks::HookClient::Codex
            )
            .is_none()
        );
        let unknown = session(&f.app, &f.app_task, Some("unknown-session"));
        assert!(
            hook_tool_observation(
                &patch,
                &unknown,
                None,
                &paths,
                ToolPhase::Post,
                crate::agenthooks::HookClient::Codex
            )
            .is_none()
        );
        let legacy = session(&f.app, &f.app_task, None);
        assert!(
            hook_tool_observation(
                &patch,
                &legacy,
                None,
                &paths,
                ToolPhase::Post,
                crate::agenthooks::HookClient::Codex
            )
            .is_none()
        );
    }

    /// Time, mode, context, ranged, bytes, limit hit, outcome and paths.
    type MergedRow = (
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        String,
        String,
    );

    #[test]
    fn pre_and_post_observations_merge_into_one_earliest_call() {
        let f = fixture();
        let context = f.context(&f.app_task, 100, &[APP_A]);
        let pre = read(
            call(
                &f.app,
                &f.app_task,
                "s1",
                "toolu_1",
                "Read",
                ToolPhase::Pre,
                &["src/a.rs"],
                None,
            ),
            true,
            None,
            None,
        );
        let first = f.store.record_tool_observation_at(&pre, 90).expect("pre");
        assert!(first.inserted);
        // The post-tool hook sees the context its own pre-tool hook delivered.
        let mut post = read(pre.clone(), true, Some(400), Some(false));
        post.phase = ToolPhase::Post;
        post.context_run_id = Some(context.clone());
        let merged = f
            .store
            .record_tool_observation_at(&post, 120)
            .expect("post");
        assert!(!merged.inserted);
        assert_eq!(merged.call_id, first.call_id);
        assert_eq!(
            merged.call_id,
            crate::agenthooks::tool_usage_sample_id("s1", "toolu_1")
        );
        // A duplicate user- and project-level hook changes nothing.
        f.store
            .record_tool_observation_at(&post, 121)
            .expect("duplicate");
        let row: MergedRow = f
            .database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT observed_at_ms, mode, context_run_id, ranged, result_bytes, limit_hit, outcome, paths_json FROM tool_observations",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?)),
                )?)
            })
            .expect("row");
        assert_eq!(
            row,
            (
                90,
                None,
                None,
                Some(1),
                Some(400),
                Some(0),
                "ok".to_owned(),
                "[\"src/a.rs\"]".to_owned()
            )
        );
        let mut failed = post.clone();
        failed.phase = ToolPhase::Failure;
        failed.result_bytes = None;
        failed.limit_hit = Some(true);
        f.store
            .record_tool_observation_at(&failed, 130)
            .expect("failure");
        let (outcome, limit): (String, i64) = f
            .database
            .with_reader(|connection| {
                Ok(connection.query_row(
                    "SELECT outcome, limit_hit FROM tool_observations",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!((outcome.as_str(), limit), ("failed", 1));
    }

    #[test]
    fn invalid_or_foreign_observations_are_rejected() {
        let f = fixture();
        let lib_context = f.context(&f.lib_task, 10, &[LIB_L]);
        let base = call(
            &f.app,
            &f.app_task,
            "s1",
            "toolu_1",
            "Edit",
            ToolPhase::Post,
            &["src/a.rs"],
            None,
        );
        let mut cases = Vec::new();
        // Absolute, escaping or unnormalized paths would pollute attribution.
        for path in [
            "/etc/hosts",
            "../../outside/secret.txt",
            "src/../../secret.txt",
            "src/..",
            "./src/a.rs",
            "src/./a.rs",
            "src//a.rs",
            "src/",
        ] {
            let mut invalid = base.clone();
            invalid.paths = vec![path.to_owned()];
            assert!(invalid.call_id().is_err(), "{path} must be rejected");
            cases.push(invalid);
        }
        // Dots inside a file name are ordinary characters.
        let mut dotted = base.clone();
        dotted.paths = vec![
            "src/..hidden/a..b.rs".to_owned(),
            ".github/ci.yml".to_owned(),
        ];
        assert!(dotted.call_id().is_ok());
        let mut metrics = base.clone();
        metrics.result_bytes = Some(10);
        cases.push(metrics);
        let mut pre_result = read(base.clone(), false, Some(10), None);
        pre_result.tool_name = "Read".to_owned();
        pre_result.phase = ToolPhase::Pre;
        cases.push(pre_result);
        let mut foreign_session = base.clone();
        foreign_session.session_id = "s2".to_owned();
        cases.push(foreign_session);
        let mut foreign_task = base.clone();
        foreign_task.task_id = Some(f.lib_task.clone());
        cases.push(foreign_task);
        let mut foreign_context = base.clone();
        foreign_context.context_run_id = Some(lib_context);
        cases.push(foreign_context);
        let mut unknown_context = base.clone();
        unknown_context.context_run_id = Some(ContextRunId::new());
        cases.push(unknown_context);
        let mut control = base.clone();
        control.tool_name = "Bash\nrm".to_owned();
        cases.push(control);
        let mut too_many = base.clone();
        too_many.paths = (0..33).map(|n| format!("src/{n}.rs")).collect();
        cases.push(too_many);
        for (index, case) in cases.iter().enumerate() {
            assert!(
                f.store.record_tool_observation_at(case, 10).is_err(),
                "case {index} must be rejected"
            );
        }
        assert!(f.store.record_tool_observation_at(&base, 10).is_ok());
    }

    /// Session s1 touches app and lib files around one delivered app context.
    fn scenario(f: &Fixture) -> ContextRunId {
        // A context compiled before the report period still precedes touches.
        f.context(&f.app_task, 10, &[APP_C]);
        let pack = f.context(
            &f.app_task,
            100,
            &[
                APP_A,
                APP_B,
                LIB_L,
                APP_D,
                "mem_lesson",
                "nod_missing",
                APP_B,
            ],
        );
        let app = |id: &str, name: &str, phase, paths: &[&str], context: Option<&ContextRunId>| {
            call(&f.app, &f.app_task, "s1", id, name, phase, paths, context)
        };
        let record = |request: &ToolObservationRequest, at: i64| {
            f.store
                .record_tool_observation_at(request, at)
                .expect("observation");
        };
        // The read that triggered the delivery is observed before compilation.
        let trigger = read(
            app("t1", "Read", ToolPhase::Pre, &["src/a.rs"], None),
            true,
            None,
            None,
        );
        record(&trigger, 100);
        let mut trigger_post = read(trigger.clone(), true, Some(400), Some(false));
        trigger_post.phase = ToolPhase::Post;
        trigger_post.context_run_id = Some(pack.clone());
        record(&trigger_post, 105);
        record(&app("t2", "Grep", ToolPhase::Post, &[], Some(&pack)), 110);
        record(
            &read(
                app("t3", "Read", ToolPhase::Post, &["src/b.rs"], Some(&pack)),
                false,
                Some(1000),
                Some(false),
            ),
            120,
        );
        record(
            &call(
                &f.lib,
                &f.lib_task,
                "s1",
                "t4",
                "Edit",
                ToolPhase::Post,
                &["lib/l.rs"],
                None,
            ),
            130,
        );
        record(
            &read(
                app("t5", "Read", ToolPhase::Failure, &["src/c.rs"], Some(&pack)),
                false,
                None,
                Some(true),
            ),
            140,
        );
        // Another session reads a pointed file, then receives a context with
        // no later call: that context is unobserved, not unused.
        let other = |id: &str, at: i64| {
            let request = read(
                call(
                    &f.app,
                    &f.other_task,
                    "s2",
                    id,
                    "Read",
                    ToolPhase::Post,
                    &["src/a.rs"],
                    None,
                ),
                false,
                Some(50),
                Some(false),
            );
            f.store
                .record_tool_observation_at(&request, at)
                .expect("other");
        };
        other("o1", 150);
        f.context(&f.other_task, 200, &[APP_A]);
        // A context without an agent session cannot be matched to observations.
        f.context(&f.detached_task, 210, &[APP_B]);
        pack
    }

    #[test]
    fn delivered_pointers_associate_with_later_touches_and_coverage() {
        let f = fixture();
        scenario(&f);
        let usage = f
            .database
            .with_reader(|connection| report(connection, &f.app, 50, 1_000))
            .expect("report");
        assert_eq!(usage.observed_tool_calls, 5, "app calls t1,t2,t3,t5 and o1");
        assert_eq!(usage.observed_sessions, 2);
        assert_eq!(usage.observed_calls_with_paths, 4);
        let brain = &usage.pointers_by_mode["brain"];
        assert_eq!(
            brain.delivered_contexts, 3,
            "the pre-period context is excluded"
        );
        assert_eq!(brain.contexts_with_pointers, 3);
        assert_eq!(brain.observed_contexts, 1);
        assert_eq!(brain.unobserved_pointers, 2, "no later call, or no session");
        assert_eq!(brain.unresolved_pointers, 1);
        // a.rs triggered the delivery; b.rs and lib/l.rs followed; d.rs never.
        assert_eq!(brain.pointers, 4);
        assert_eq!(brain.already_touched, 1);
        assert_eq!(brain.touched_later, 2);
        assert_eq!(brain.precision, Some(2.0 / 3.0));
        assert_eq!(
            brain.first_touch_calls,
            FirstTouch {
                samples: 2,
                mean: Some(2.5),
                median: Some(2.5)
            }
        );
        // First touches: a.rs at 100 (the pre-period context was earlier but
        // pointed elsewhere) and b.rs, pointed by the delivery. The failed read
        // of c.rs is not a touch. s2 read a.rs before any context for that session.
        assert_eq!(usage.touched_files, 3);
        assert_eq!(usage.touched_files_before_context, 1);
        assert_eq!(brain.touched_files, 2);
        assert_eq!(brain.touched_files_pointed, 1);
        assert_eq!(brain.coverage, Some(0.5));
        let reads = &usage.reads_by_mode;
        assert_eq!(
            reads["brain"],
            ReadVolume {
                reads: 2,
                ranged: 0,
                whole_file: 2,
                ranged_share: Some(0.0),
                sized_reads: 1,
                result_bytes: 1000,
                mean_result_bytes: Some(1000.0),
                limit_hits: 1,
                failed: 1,
                unfinished: 0,
            }
        );
        assert_eq!(
            reads[UNATTRIBUTED_MODE],
            ReadVolume {
                reads: 2,
                ranged: 1,
                whole_file: 1,
                ranged_share: Some(0.5),
                sized_reads: 2,
                result_bytes: 450,
                mean_result_bytes: Some(225.0),
                limit_hits: 0,
                failed: 0,
                unfinished: 0,
            }
        );
        // A cross-repository pointer covers the linked repository's file.
        let lib = f
            .database
            .with_reader(|connection| report(connection, &f.lib, 50, 1_000))
            .expect("lib report");
        assert_eq!(
            (lib.touched_files, lib.touched_files_before_context),
            (1, 0)
        );
        assert_eq!(lib.pointers_by_mode["brain"].touched_files_pointed, 1);
        assert_eq!(lib.pointers_by_mode["brain"].delivered_contexts, 0);
        assert!(lib.reads_by_mode.is_empty());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn unfinished_and_failed_calls_are_not_touches() {
        let f = fixture();
        let pack = f.context(&f.app_task, 100, &[APP_A, APP_B, APP_C, APP_D]);
        let record = |request: ToolObservationRequest, at: i64| {
            f.store
                .record_tool_observation_at(&request, at)
                .expect("observation");
        };
        let app = |id: &str, name: &str, phase, path: &str| {
            call(
                &f.app,
                &f.app_task,
                "s1",
                id,
                name,
                phase,
                &[path],
                Some(&pack),
            )
        };
        // Denied or interrupted: the pre-tool hook saw it, no post-tool hook did.
        record(
            read(
                app("denied", "Read", ToolPhase::Pre, "src/a.rs"),
                false,
                None,
                None,
            ),
            110,
        );
        // A read rejected by the size limit, then an edit that failed.
        record(
            read(
                app("failed", "Read", ToolPhase::Pre, "src/b.rs"),
                false,
                None,
                None,
            ),
            120,
        );
        record(
            read(
                app("failed", "Read", ToolPhase::Failure, "src/b.rs"),
                false,
                None,
                Some(true),
            ),
            125,
        );
        record(app("edit", "Edit", ToolPhase::Failure, "src/c.rs"), 130);
        record(
            read(
                app("read", "Read", ToolPhase::Post, "src/d.rs"),
                true,
                Some(10),
                Some(false),
            ),
            140,
        );
        // A context whose only later call never finished stays unobserved.
        let other = f.context(&f.other_task, 300, &[APP_A]);
        let pending = read(
            call(
                &f.app,
                &f.other_task,
                "s2",
                "pending",
                "Read",
                ToolPhase::Pre,
                &["src/a.rs"],
                Some(&other),
            ),
            false,
            None,
            None,
        );
        f.store.record_tool_observation_at(&pending, 310).unwrap();
        let usage = f
            .database
            .with_reader(|connection| report(connection, &f.app, 0, 1_000))
            .expect("report");
        let brain = &usage.pointers_by_mode["brain"];
        assert_eq!((brain.delivered_contexts, brain.observed_contexts), (2, 1));
        assert_eq!(
            brain.unobserved_pointers, 1,
            "the pending call has no outcome"
        );
        assert_eq!(brain.pointers, 4);
        assert_eq!((brain.already_touched, brain.touched_later), (0, 1));
        assert_eq!(
            brain.precision,
            Some(0.25),
            "only the completed read of d.rs"
        );
        // Every observed call is an agent step on the way to the first touch.
        assert_eq!(brain.first_touch_calls.median, Some(4.0));
        assert_eq!(usage.touched_files, 1);
        assert_eq!((brain.touched_files, brain.touched_files_pointed), (1, 1));
        assert_eq!(
            usage.reads_by_mode["brain"],
            ReadVolume {
                reads: 2,
                ranged: 1,
                whole_file: 1,
                ranged_share: Some(0.5),
                sized_reads: 1,
                result_bytes: 10,
                mean_result_bytes: Some(10.0),
                limit_hits: 1,
                failed: 1,
                unfinished: 2,
            }
        );
        let text = usage.summary_lines().join("\n");
        assert!(
            text.contains("limit hits 1; failed 1; unfinished 2"),
            "{text}"
        );
    }

    #[test]
    fn periods_bound_observations_and_unobserved_history_stays_unknown() {
        let f = fixture();
        scenario(&f);
        // Before any call: the context is delivered but has no observed follow-up.
        let early = f
            .database
            .with_reader(|connection| report(connection, &f.app, 50, 101))
            .expect("early report");
        let brain = &early.pointers_by_mode["brain"];
        assert_eq!((brain.delivered_contexts, brain.observed_contexts), (1, 0));
        assert_eq!(brain.unobserved_pointers, 4);
        assert_eq!(brain.precision, None);
        // a.rs was first touched after the pre-period context, which pointed elsewhere.
        assert_eq!(brain.coverage, Some(0.0));
        assert_eq!(brain.first_touch_calls, FirstTouch::default());
        // Historical contexts without hook observations remain unknown.
        let f = fixture();
        f.context(&f.app_task, 100, &[APP_A, APP_B]);
        let history = f
            .database
            .with_reader(|connection| report(connection, &f.app, 0, 1_000))
            .expect("history");
        assert_eq!(history.observed_tool_calls, 0);
        assert_eq!(history.pointers_by_mode["brain"].unobserved_pointers, 2);
        assert_eq!(history.pointers_by_mode["brain"].precision, None);
        assert!(history.summary_lines()[1].contains("unknown"));
        let empty = f
            .database
            .with_reader(|connection| report(connection, &f.lib, 0, 1_000))
            .expect("empty");
        assert_eq!(empty, ContextUse::default());
        assert!(empty.summary_lines().is_empty());
    }

    #[test]
    fn effectiveness_report_and_text_expose_context_use() {
        let f = fixture();
        scenario(&f);
        let report = f
            .store
            .report(&super::super::EffectivenessRequest {
                project_ids: Some(vec![f.app.clone()]),
                since_ms: Some(50),
                until_ms: Some(1_000),
            })
            .expect("report");
        let use_ = &report.projects[0].context_use;
        assert_eq!(use_.pointers_by_mode["brain"].touched_later, 2);
        let value = serde_json::to_value(&report).unwrap();
        let context_use = &value["projects"][0]["context_use"];
        assert!(context_use["pointers_by_mode"]["brain"]["precision"].is_f64());
        assert!(context_use["reads_by_mode"]["unattributed"]["ranged_share"].is_f64());
        assert!(
            report
                .limitations
                .iter()
                .any(|note| note.contains("association, not causation"))
        );
        let text = use_.summary_lines().join("\n");
        assert!(text.contains("touched later 2 of 3 (66.7%)"), "{text}");
        assert!(
            text.contains("pointed to before first touch 1 of 2 files (50.0%)"),
            "{text}"
        );
        assert!(
            text.contains(
                "brain reads: 2 (0 ranged, 2 whole-file; 0.0% ranged); mean result 1.0 KiB"
            ),
            "{text}"
        );
        // Older daemons omit the section; newer clients read it as empty.
        let mut legacy = value["projects"][0].clone();
        legacy.as_object_mut().unwrap().remove("context_use");
        let parsed: super::super::ProjectEffectiveness = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.context_use, ContextUse::default());
    }

    #[test]
    fn retention_deletes_only_expired_observations() {
        let f = fixture();
        for (id, at) in [("old", 10), ("new", 500)] {
            let request = call(
                &f.app,
                &f.app_task,
                "s1",
                id,
                "Bash",
                ToolPhase::Post,
                &[],
                None,
            );
            f.store.record_tool_observation_at(&request, at).unwrap();
        }
        assert_eq!(
            delete_tool_observation_batch(&f.database, 100, 10).unwrap(),
            1
        );
        assert_eq!(
            delete_tool_observation_batch(&f.database, 100, 10).unwrap(),
            0
        );
        let remaining: i64 = f
            .database
            .with_reader(|connection| {
                Ok(
                    connection.query_row("SELECT count(*) FROM tool_observations", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        assert_eq!(remaining, 1);
    }
}
