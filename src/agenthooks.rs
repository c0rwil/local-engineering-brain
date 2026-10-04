//! Stock-agent integration via Claude Code hooks: context injection and
//! automatic evidence capture with NO proxy and NO agent cooperation.
//!
//! `engram hook prompt` runs on `UserPromptSubmit`: the first prompt of a
//! session compiles brain context for that text. Later prompts refresh context
//! as repository knowledge changes. Tool use lazily opens one episode per
//! project; repeated semantically identical context stays silent.
//!
//! `engram hook bash` runs on `PostToolUse` for Bash: when the executed command
//! is a recognizable verification (test/build/lint/typecheck), its exit code
//! is recorded as deterministic evidence on the session's task — including
//! FAILURES, which is exactly what fail-then-pass preference distillation
//! feeds on. The agent no longer has to remember to record anything.
//!
//! Only the harness's own report counts as the exit status: an explicit code,
//! Claude Code's `PostToolUseFailure` "Exit code N", or a completed foreground
//! `PostToolUse` (Claude Code reports every non-zero exit as a failure). The
//! status must also belong to every verification in the command, in one
//! modeled directory: pipes, `||`, `;` after the first verification, background
//! jobs and unmodeled `cd`s hide it, so those runs are not recorded. Before any
//! daemon call the session marks the verification's result unknown; only a
//! recorded observation replaces it, and the episode closes from the latest
//! result of each verification. Each record carries content digests of the
//! files the session touched or left dirty, which ties it to the source it
//! actually ran against. When the agent declared an open Engram task through
//! Engram's MCP server in the same session, evidence goes to that task.
//!
//! Every hook fails OPEN: a broken brain must never break a coding session.

pub mod codex;
pub mod cursor;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{ProjectId, TaskId};

/// Evidence `metadata.source` written by the native hooks.
pub const CLAUDE_HOOK_SOURCE: &str = "claude-code-hook";
pub const CODEX_HOOK_SOURCE: &str = "codex-hook";
pub const CURSOR_HOOK_SOURCE: &str = "cursor-hook";
/// Verification output kept as the artifact. Runner summaries are at the end,
/// so longer output keeps its tail.
const VERIFICATION_EXCERPT_BYTES: usize = 4_096;
/// Bounds for the source digests recorded beside one verification.
const SOURCE_DIGEST_FILES: usize = 64;
const SOURCE_DIGEST_FILE_BYTES: u64 = 512 * 1024;
const SOURCE_DIGEST_TOTAL_BYTES: u64 = 4 * 1024 * 1024;
const SOURCE_DIGEST_PATH_BYTES: usize = 512;

/// Session → open-task linkage, stored beside the brain so all hook
/// invocations of one session agree.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HookSession {
    #[serde(default)]
    pub session_id: Option<String>,
    pub task_id: TaskId,
    pub project_id: ProjectId,
    /// Exit code of the most recent recorded verification, driving the
    /// conservative auto-close outcome at session end.
    #[serde(default)]
    pub last_verification_exit: Option<i32>,
    /// Fingerprint of the most recent recorded evidence. When user-level
    /// AND repo-level hooks both fire for one Bash call, the second
    /// invocation sees its own fingerprint and skips the duplicate row.
    #[serde(default)]
    pub last_evidence_fingerprint: Option<String>,
    /// Checkout root of this episode's project, recorded once a verification
    /// resolves it, so a later write-ahead can find the episode daemon-free.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Latest result per verification identity, oldest first and bounded.
    /// The episode auto-closes from these, never from one last exit code.
    #[serde(default)]
    pub verification_results: Vec<VerificationResult>,
}

/// The latest result of one verification identity in a session.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct VerificationResult {
    /// Directory and normalized verification commands; see [`PendingVerification`].
    pub key: String,
    /// Whether the command runs tests.
    #[serde(default)]
    pub tests: bool,
    /// Observed exit status; `None` while the result is unknown.
    #[serde(default)]
    pub exit_code: Option<i32>,
}

/// Verification identities kept per episode. Eviction removes the oldest
/// pass first, so a retained failure or unknown result still blocks success.
const VERIFICATION_RESULTS_MAX: usize = 64;

impl HookSession {
    /// Set the latest result of one verification identity (`None` = unknown).
    pub fn set_verification_result(&mut self, key: &str, tests: bool, exit_code: Option<i32>) {
        self.verification_results.retain(|result| result.key != key);
        self.verification_results.push(VerificationResult {
            key: key.to_owned(),
            tests,
            exit_code,
        });
        if self.verification_results.len() > VERIFICATION_RESULTS_MAX {
            let evicted = self
                .verification_results
                .iter()
                .position(|result| result.exit_code == Some(0))
                .unwrap_or(0);
            self.verification_results.remove(evicted);
        }
    }

    /// The latest verification result is unknown: forget the last recorded
    /// exit and fingerprint, so neither an older pass nor the duplicate check
    /// stands in for it.
    pub fn forget_last_result(&mut self) {
        self.last_verification_exit = None;
        self.last_evidence_fingerprint = None;
    }
}

/// Fingerprint identifying one verification result for double-fire dedupe.
#[must_use]
pub fn evidence_fingerprint(command: &str, exit_code: i32) -> String {
    crate::experience::sha256(format!("{exit_code}:{}", command.trim()).as_bytes())
}

/// Stable receipt identity from the agent's actual session and tool-use ids.
#[must_use]
pub fn tool_usage_sample_id(session_id: &str, tool_use_id: &str) -> String {
    let identity = serde_json::to_vec(&(session_id, tool_use_id)).unwrap_or_default();
    format!("tool:{}", crate::experience::sha256(&identity))
}

/// A session can edit several repositories. Each gets a separate episode.
#[must_use]
pub fn scoped_session_id(session_id: &str, project_id: &ProjectId) -> String {
    format!(
        "{}-{project_id}",
        &crate::experience::sha256(session_id.as_bytes())[..24]
    )
}

/// The scope for one conversation's context-delivery receipt and file-brief
/// list. A subagent thread (`agent_id`, such as a spawned Codex worker) shares
/// the session's episode `scope` but not its model context, so it keeps its
/// own receipt and brief list: neither conversation suppresses the other's.
#[must_use]
pub fn thread_scope(scope: &str, payload: &serde_json::Value) -> String {
    let Some(agent) = payload
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|agent| !agent.is_empty())
    else {
        return scope.to_owned();
    };
    // 65-character episode scopes plus 15 stay within the 80-character
    // sidecar names `session_state_path` keeps.
    format!(
        "{scope}-{}",
        &crate::experience::sha256(agent.as_bytes())[..14]
    )
}

/// Remove the per-thread delivery sidecars derived from an episode `scope`
/// (session end).
pub fn clear_thread_scopes(state_dir: &Path, scope: &str) {
    let Some(prefix) = session_state_path(state_dir, scope)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| format!("{stem}-"))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(state_dir.join("hook-sessions")) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        // Delivery locks are directories; never follow a link out of state.
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Load all project episodes for this session, including pre-upgrade sidecars.
#[must_use]
pub fn project_sessions(state_dir: &Path, session_id: &str) -> Vec<(String, HookSession)> {
    let prefix = format!(
        "{}-",
        &crate::experience::sha256(session_id.as_bytes())[..24]
    );
    let legacy = session_state_path(state_dir, session_id);
    active_sessions(state_dir)
        .into_iter()
        .filter(|(key, _, _)| {
            key.starts_with(&prefix) || legacy == session_state_path(state_dir, key)
        })
        .map(|(key, session, _)| (key, session))
        .collect()
}

/// Retain the latest bounded, redacted prompt for later tool use and context.
///
/// # Errors
/// Returns filesystem errors.
pub fn save_pending_prompt(
    state_dir: &Path,
    session_id: &str,
    prompt: &str,
    redactor: &crate::security::Redactor,
) -> Result<()> {
    let path = session_state_path(state_dir, session_id).with_extension("pending");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Config(e.to_string()))?;
    }
    // Redact before truncation so the bound cannot split a credential.
    let prompt: String = redactor.redact_text(prompt).chars().take(8_000).collect();
    write_private_sidecar(&path, prompt.as_bytes())
}

#[must_use]
pub fn pending_prompt(state_dir: &Path, session_id: &str) -> Option<String> {
    std::fs::read_to_string(session_state_path(state_dir, session_id).with_extension("pending"))
        .ok()
}

/// Read shell text from Claude and native Codex tool argument shapes.
#[must_use]
pub fn tool_command(input: &serde_json::Value) -> &str {
    input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .or_else(|| input.get("cmd").and_then(serde_json::Value::as_str))
        .unwrap_or("")
}

/// Resolve the tool's working directory without executing shell text.
fn tool_workdir(cwd: &Path, input: &serde_json::Value) -> PathBuf {
    let base = tool_base_dir(cwd, input);
    let path = shell_workdir(tool_command(input), &base).unwrap_or(base);
    normalize_tool_path(&path)
}

/// The directory a tool starts in: its explicit `cwd`/`workdir`, else `cwd`.
fn tool_base_dir(cwd: &Path, input: &serde_json::Value) -> PathBuf {
    let explicit = input
        .get("cwd")
        .or_else(|| input.get("workdir"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    explicit.map_or_else(
        || cwd.to_path_buf(),
        |path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        },
    )
}

// Canonicalize existing ancestors too: newly created files still need to
// agree with canonical repository roots (notably /tmp on macOS).
fn normalize_tool_path(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let Some(parent) = path.parent().filter(|parent| *parent != path) else {
        return path.to_path_buf();
    };
    let parent = normalize_tool_path(parent);
    match path.components().next_back() {
        Some(std::path::Component::ParentDir) => parent.parent().unwrap_or(&parent).to_path_buf(),
        Some(std::path::Component::Normal(name)) => parent.join(name),
        _ => parent,
    }
}

/// Resolve explicit file arguments exactly once against the effective tool cwd.
#[must_use]
pub fn resolve_tool_paths(cwd: &Path, input: &serde_json::Value) -> Vec<PathBuf> {
    let base = tool_workdir(cwd, input);
    extract_file_paths(input)
        .into_iter()
        .map(|path| normalize_tool_path(&base.join(path)))
        .collect()
}

/// Existing directory for a resolved file path, including files not yet created.
#[must_use]
pub fn tool_path_directory(path: &Path) -> PathBuf {
    let mut directory = path.parent().unwrap_or(path).to_path_buf();
    while !directory.is_dir() {
        if !directory.pop() {
            break;
        }
    }
    directory
}

/// Resolve explicit tool workdirs or the first file's containing directory.
#[must_use]
pub fn tool_directory(cwd: &Path, input: &serde_json::Value) -> PathBuf {
    resolve_tool_paths(cwd, input).first().map_or_else(
        || tool_workdir(cwd, input),
        |path| tool_path_directory(path),
    )
}

/// Pull the file path out of a tool input (Read/Edit/Write shapes).
#[must_use]
pub fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    extract_file_paths(tool_input).into_iter().next()
}

/// Explicit file arguments or `apply_patch` headers; never execute shell text.
#[must_use]
pub fn extract_file_paths(tool_input: &serde_json::Value) -> Vec<String> {
    for key in ["file_path", "filePath", "path", "notebook_path"] {
        if let Some(path) = tool_input.get(key).and_then(serde_json::Value::as_str) {
            if !path.trim().is_empty() {
                return vec![path.to_owned()];
            }
        }
    }
    let command = tool_command(tool_input);
    if !command.starts_with("*** Begin Patch\n") {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for line in command.lines() {
        for prefix in [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(path) = line.strip_prefix(prefix).filter(|p| !p.is_empty()) {
                if !paths.iter().any(|p| p == path) {
                    paths.push(path.to_owned());
                }
            }
        }
        if paths.len() >= 32 {
            break;
        }
    }
    paths
}

/// Atomically claim a once-per-session action (`tag`, e.g. `brief` or
/// `prompt`). Claude Code runs user-level and project-level hooks
/// CONCURRENTLY, so a read-then-write existence check races and both fire;
/// `create_new` makes exactly one caller win.
#[must_use]
pub fn claim_once(state_dir: &Path, session_id: &str, tag: &str) -> bool {
    let path = session_state_path(state_dir, session_id).with_extension(format!("{tag}.claim"));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .is_ok()
}

/// Remove a session's claim files (session end).
pub fn clear_claims(state_dir: &Path, session_id: &str) {
    for tag in ["brief", "prompt", "episode", "context"] {
        release_claim(state_dir, session_id, tag);
    }
}

/// Release a claimed action that could not be delivered, allowing a later retry.
pub fn release_claim(state_dir: &Path, session_id: &str, tag: &str) {
    let path = session_state_path(state_dir, session_id).with_extension(format!("{tag}.claim"));
    let _ = std::fs::remove_file(path);
}

/// Sidecar tracking which files a session has already been briefed on, so
/// pre-tool shape injection fires once per file per session.
#[must_use]
pub fn session_files_path(state_dir: &Path, session_id: &str) -> PathBuf {
    session_state_path(state_dir, session_id).with_extension("files")
}

/// Read the context previously delivered for this exact project/session.
#[must_use]
pub fn context_run_id(state_dir: &Path, scope: &str) -> Option<crate::ids::ContextRunId> {
    context_delivery(state_dir, scope)
        .and_then(|delivery| delivery.run_id)
        .or_else(|| legacy_context_run_id(state_dir, scope))
}

fn legacy_context_run_id(state_dir: &Path, scope: &str) -> Option<crate::ids::ContextRunId> {
    let bytes =
        std::fs::read(session_state_path(state_dir, scope).with_extension("context-run")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Receipt for the last context successfully emitted for this project/session.
#[derive(Deserialize, Serialize)]
struct ContextDelivery {
    fingerprint: String,
    request_id: String,
    #[serde(default)]
    run_id: Option<crate::ids::ContextRunId>,
    #[serde(default, alias = "text_digest")]
    delivery_digest: Option<String>,
}

/// A transient delivery lock. Dropping an unsuccessful attempt permits retry.
pub struct ContextDeliveryClaim {
    state_dir: PathBuf,
    scope: String,
    fingerprint: String,
    lock_directory: PathBuf,
    owner_path: PathBuf,
}

impl ContextDeliveryClaim {
    /// Compare exact text for callers without compiler delivery metadata.
    ///
    /// # Errors
    /// Returns filesystem errors writing the updated input fingerprint.
    pub fn should_emit(&self, text: &str) -> Result<bool> {
        self.should_emit_digest(&crate::experience::sha256(text.as_bytes()))
    }

    /// Suppress only the last successfully delivered semantic pack.
    /// The last emitted run and request stay authoritative for usage attribution.
    /// Removals and returning to an older set are changes, not permanent ID hits.
    ///
    /// # Errors
    /// Returns filesystem errors writing the updated input fingerprint.
    pub fn should_emit_digest(&self, digest: &str) -> Result<bool> {
        let Some(mut previous) = context_delivery(&self.state_dir, &self.scope) else {
            return Ok(true);
        };
        if previous.delivery_digest.as_deref() != Some(digest) {
            return Ok(true);
        }
        previous.fingerprint.clone_from(&self.fingerprint);
        if previous.run_id.is_none() {
            previous.run_id = legacy_context_run_id(&self.state_dir, &self.scope);
        }
        write_private_sidecar(
            &session_state_path(&self.state_dir, &self.scope).with_extension("context-delivery"),
            &serde_json::to_vec(&previous)?,
        )?;
        Ok(false)
    }

    /// Mark emitted context as delivered, without replacing prior receipts on failure.
    ///
    /// # Errors
    /// Returns filesystem errors writing the single atomic delivery receipt.
    pub fn delivered(
        &self,
        request_id: &str,
        run_id: &crate::ids::ContextRunId,
        text: &str,
    ) -> Result<()> {
        self.delivered_digest(
            request_id,
            run_id,
            &crate::experience::sha256(text.as_bytes()),
        )
    }

    /// Persist a compiler signature only after the complete output succeeds.
    ///
    /// # Errors
    /// Returns filesystem errors writing the atomic receipt.
    pub fn delivered_digest(
        &self,
        request_id: &str,
        run_id: &crate::ids::ContextRunId,
        digest: &str,
    ) -> Result<()> {
        write_private_sidecar(
            &session_state_path(&self.state_dir, &self.scope).with_extension("context-delivery"),
            &serde_json::to_vec(&ContextDelivery {
                fingerprint: self.fingerprint.clone(),
                request_id: request_id.to_owned(),
                run_id: Some(run_id.clone()),
                delivery_digest: Some(digest.to_owned()),
            })?,
        )
    }

    /// Forget the comparison baseline when a successful compilation is empty.
    /// This records no delivery and preserves the last emitted attribution IDs.
    /// A later restored pack must be delivered again after removal/invalidation.
    ///
    /// # Errors
    /// Returns filesystem errors updating an existing receipt.
    pub fn invalidate_empty(&self) -> Result<()> {
        let Some(mut previous) = context_delivery(&self.state_dir, &self.scope) else {
            return Ok(());
        };
        previous.delivery_digest = None;
        previous.fingerprint.clone_from(&self.fingerprint);
        write_private_sidecar(
            &session_state_path(&self.state_dir, &self.scope).with_extension("context-delivery"),
            &serde_json::to_vec(&previous)?,
        )
    }
}

impl Drop for ContextDeliveryClaim {
    fn drop(&mut self) {
        if std::fs::remove_file(&self.owner_path).is_ok() {
            let _ = std::fs::remove_dir(&self.lock_directory);
        }
    }
}

// Publish a complete owner directory atomically. Recovery removes only the
// dead owner's unique file; a concurrent replacement stays nonempty and safe.
fn delivery_lock(state_dir: &Path, scope: &str) -> Option<(PathBuf, PathBuf)> {
    let directory = session_state_path(state_dir, scope).with_extension("context-delivering.lock");
    let parent = directory.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let temporary = tempfile::tempdir_in(parent).ok()?;
    let owner = format!("{}-{}", std::process::id(), uuid::Uuid::new_v4());
    std::fs::write(temporary.path().join(&owner), []).ok()?;
    if std::fs::rename(temporary.path(), &directory).is_err() {
        let owners: Vec<_> = std::fs::read_dir(&directory).ok()?.take(2).collect();
        if owners.len() != 1 {
            return None;
        }
        let stale = owners.into_iter().next()?.ok()?;
        let filename = stale.file_name();
        let pid = filename.to_str()?.split_once('-')?.0.parse::<u32>().ok()?;
        if pid <= 1 {
            return None;
        }
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if alive || std::fs::remove_file(stale.path()).is_err() {
            return None;
        }
        // If another hook already recovered the lock, its nonempty owner
        // directory makes both this removal and the following rename fail.
        let _ = std::fs::remove_dir(&directory);
        std::fs::rename(temporary.path(), &directory).ok()?;
    }
    let owner_path = directory.join(owner);
    Some((directory, owner_path))
}

fn context_delivery(state_dir: &Path, scope: &str) -> Option<ContextDelivery> {
    let bytes =
        std::fs::read(session_state_path(state_dir, scope).with_extension("context-delivery"))
            .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Request identity for linking a lazy episode to context actually delivered.
#[must_use]
pub fn context_request_id(state_dir: &Path, scope: &str) -> Option<String> {
    context_delivery(state_dir, scope).map(|delivery| delivery.request_id)
}

/// Serialize delivery attempts. Input equality cannot prove content equality:
/// indexed sources, lessons and facts may change without prompt/path/Git changes.
#[must_use]
pub fn claim_context_delivery(
    state_dir: &Path,
    scope: &str,
    prompt: &str,
    current_paths: &[String],
    token_budget: u32,
    git: Option<&crate::projects::GitState>,
) -> Option<ContextDeliveryClaim> {
    let (lock_directory, owner_path) = delivery_lock(state_dir, scope)?;
    let mut paths: Vec<&str> = current_paths.iter().map(String::as_str).collect();
    paths.sort_unstable();
    paths.dedup();
    let fingerprint = crate::experience::sha256(
        &serde_json::to_vec(&(
            prompt.trim(),
            paths,
            token_budget,
            git.map(|git| (&git.head, &git.diff_fingerprint)),
        ))
        .unwrap_or_default(),
    );
    let claim = ContextDeliveryClaim {
        state_dir: state_dir.to_path_buf(),
        scope: scope.to_owned(),
        fingerprint,
        lock_directory,
        owner_path,
    };
    Some(claim)
}

/// Recently observed repository-relative tool paths, bounded per session/project.
#[must_use]
pub fn recent_paths(state_dir: &Path, scope: &str) -> Vec<String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    let Ok(file) =
        std::fs::File::open(session_state_path(state_dir, scope).with_extension("recent-paths"))
    else {
        return Vec::new();
    };
    if file.take(256 * 1024 + 1).read_to_end(&mut bytes).is_err() || bytes.len() > 256 * 1024 {
        return Vec::new();
    }
    Some(bytes)
        .and_then(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|path| !path.is_empty() && path.len() <= 4096)
        .take(32)
        .collect()
}

/// Retain actual observed paths independently of whether context was emitted.
///
/// # Errors
/// Returns filesystem errors writing the bounded private path sidecar.
pub fn remember_paths(state_dir: &Path, scope: &str, observed: &[String]) -> Result<()> {
    let mut paths = recent_paths(state_dir, scope);
    for path in observed
        .iter()
        .filter(|path| !path.is_empty() && path.len() <= 4096)
    {
        paths.retain(|previous| previous != path);
        paths.insert(0, path.clone());
        paths.truncate(32);
    }
    write_private_sidecar(
        &session_state_path(state_dir, scope).with_extension("recent-paths"),
        &serde_json::to_vec(&paths)?,
    )
}

/// Stable session-wide scope for usage baseline and acknowledged sample ids.
#[must_use]
pub fn usage_session_scope(session_id: &str) -> String {
    format!(
        "usage-{}",
        &crate::experience::sha256(session_id.as_bytes())[..24]
    )
}

/// Local receipts avoid resending transcripts and define the observed baseline.
/// `None` means this workspace has not established a baseline for the session.
///
/// # Errors
/// Rejects unreadable, malformed, or excessively large receipt metadata.
pub fn usage_receipts(
    state_dir: &Path,
    scope: &str,
) -> Result<Option<std::collections::BTreeSet<String>>> {
    let path = session_state_path(state_dir, scope).with_extension("usage-receipts");
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(crate::error::io_error(&path, error)),
    };
    if metadata.len() > 4 * 1024 * 1024 {
        return Err(Error::Config(
            "usage receipt metadata exceeds 4 MiB".to_owned(),
        ));
    }
    let bytes = std::fs::read(&path).map_err(|error| crate::error::io_error(&path, error))?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Persist sample ids already acknowledged or deliberately consumed as an
/// unattributed baseline/ambiguous interval. No transcript content is retained.
///
/// # Errors
/// Returns errors writing private receipt sidecars.
pub fn save_usage_receipts(
    state_dir: &Path,
    scope: &str,
    samples: &std::collections::BTreeSet<String>,
) -> Result<()> {
    write_private_sidecar(
        &session_state_path(state_dir, scope).with_extension("usage-receipts"),
        &serde_json::to_vec(samples)?,
    )
}

/// Record `file_path` as briefed; returns false when it already was (skip).
#[must_use]
pub fn mark_file_briefed(state_dir: &Path, session_id: &str, file_path: &str) -> bool {
    let path = session_files_path(state_dir, session_id);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line == file_path) {
        return false;
    }
    if existing.lines().count() >= 200 {
        return false; // hard cap: stop briefing rather than growing unbounded
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut updated = existing;
    updated.push_str(file_path);
    updated.push('\n');
    std::fs::write(&path, updated).is_ok()
}

/// Root of the session relay: per-session inbox directories under the
/// brain's state dir. Messages are plain files; delivery happens when the
/// recipient session's next hook fires. No daemon involvement, so the
/// relay works even while the daemon is down.
#[must_use]
pub fn relay_inbox(state_dir: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    state_dir.join("relay").join(safe)
}

/// Deposit one relay message for a session.
///
/// # Errors
///
/// Returns filesystem errors.
pub fn relay_send(state_dir: &Path, session_id: &str, from: &str, message: &str) -> Result<()> {
    let inbox = relay_inbox(state_dir, session_id);
    std::fs::create_dir_all(&inbox)
        .map_err(|error| Error::Config(format!("relay inbox: {error}")))?;
    let name = format!(
        "{}-{}.txt",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default(),
        &uuid::Uuid::new_v4().to_string()[..8]
    );
    let mut body: String = message.chars().take(4_000).collect();
    body.insert_str(0, &format!("from {from}: "));
    std::fs::write(inbox.join(name), body)
        .map_err(|error| Error::Config(format!("relay write: {error}")))
}

/// Drain a session's relay inbox: return pending messages oldest-first and
/// delete them. Empty when there is nothing (the common case — one cheap
/// directory probe).
#[must_use]
pub fn relay_drain(state_dir: &Path, session_id: &str) -> Vec<String> {
    let inbox = relay_inbox(state_dir, session_id);
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    let mut messages = Vec::new();
    for path in files.into_iter().take(20) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            messages.push(content);
        }
        let _ = std::fs::remove_file(&path);
    }
    messages
}

/// Active hook sessions (the relay board's data): session id, project id,
/// and last-activity time, from the session state files.
#[must_use]
pub fn active_sessions(state_dir: &Path) -> Vec<(String, HookSession, std::time::SystemTime)> {
    let dir = state_dir.join("hook-sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut sessions = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(session) = serde_json::from_str::<HookSession>(&content) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        sessions.push((stem, session, modified));
    }
    sessions.sort_by_key(|entry| std::cmp::Reverse(entry.2));
    sessions
}

/// The coding agent that invoked a hook. Capture logic is shared; the client
/// selects provenance labels and output bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookClient {
    ClaudeCode,
    Codex,
    Cursor,
}

impl HookClient {
    /// `resource_usage.source` label for usage this client reports.
    #[must_use]
    pub fn usage_source(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude_code",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
        }
    }

    /// Evidence `metadata.source` label for verifications its hooks observe.
    #[must_use]
    pub fn evidence_source(self) -> &'static str {
        match self {
            Self::ClaudeCode => CLAUDE_HOOK_SOURCE,
            Self::Codex => CODEX_HOOK_SOURCE,
            Self::Cursor => CURSOR_HOOK_SOURCE,
        }
    }

    /// Whether one tool-time context message must fit the configured token
    /// budget. Claude Code reads each emitted block separately.
    #[must_use]
    pub fn bounded_context(self) -> bool {
        !matches!(self, Self::ClaudeCode)
    }

    /// Hard byte ceiling for one tool-time context message.
    #[must_use]
    pub fn context_byte_cap(self) -> usize {
        match self {
            Self::Cursor => cursor::CONTEXT_BYTES,
            Self::ClaudeCode | Self::Codex => usize::MAX,
        }
    }

    /// Whether the prompt hook can return context to the model. Cursor's
    /// documented `beforeSubmitPrompt` output has no context field.
    #[must_use]
    pub fn prompt_context(self) -> bool {
        !matches!(self, Self::Cursor)
    }
}

/// Wrap hook context in the `hookSpecificOutput.additionalContext` JSON
/// envelope. Claude Code accepts it for PreToolUse/PostToolUse; Codex
/// expects it for every event.
#[must_use]
pub fn additional_context_output(event_name: &str, context: &str) -> String {
    serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": event_name,
            "additionalContext": context,
        }
    })
    .to_string()
}

/// Render the session-start standing brief from episodic state. Returns
/// `None` when there is nothing worth saying (a quiet project stays quiet).
#[must_use]
pub fn render_session_brief(
    project_name: &str,
    open: &[(String, String, i64)],
    last_completed: Option<(&str, &str)>,
    now_ms: i64,
) -> Option<String> {
    if open.is_empty() && last_completed.is_none() {
        return None;
    }
    let inline = crate::security::neutralize_inline;
    let mut brief = format!("# Engram session brief — {}\n", inline(project_name));
    if !open.is_empty() {
        brief.push_str("Open episodes from earlier sessions (resume or close them):\n");
        for (id, prompt, started_at_ms) in open.iter().take(3) {
            let mut prompt: String = prompt.chars().take(110).collect();
            if let Some(cut) = prompt.rfind(' ').filter(|_| prompt.len() >= 110) {
                prompt.truncate(cut);
            }
            let age_hours = (now_ms.saturating_sub(*started_at_ms)) / 3_600_000;
            let _ = writeln!(brief, "- [{id}] {} ({age_hours}h ago)", inline(&prompt));
        }
    }
    if let Some((status, prompt)) = last_completed {
        let prompt: String = prompt.chars().take(90).collect();
        let _ = writeln!(
            brief,
            "Last completed episode: {status} — {}",
            inline(&prompt)
        );
    }
    Some(brief)
}

/// Render a pre-tool file-shape brief. `None` when the graph knows nothing
/// about the file (inject silence, not filler).
#[must_use]
pub fn render_file_shape(
    file_path: &str,
    symbols: &[(String, String)],
    callers: &[(String, String)],
    imports: &[String],
) -> Option<String> {
    if symbols.is_empty() && callers.is_empty() {
        return None;
    }
    // Labels and imports come from repository files: keep each on one line
    // with hidden characters and framing markup escaped.
    let inline = crate::security::neutralize_inline;
    let mut brief = format!(
        "# Indexed reference data: {}\nVerify current source before relying on these symbols and relationships.\n",
        inline(file_path)
    );
    if !symbols.is_empty() {
        let listed: Vec<String> = symbols
            .iter()
            .take(20)
            .map(|(kind, label)| format!("{} ({})", inline(label), inline(kind)))
            .collect();
        let _ = writeln!(brief, "Defines: {}", listed.join(", "));
    }
    if !callers.is_empty() {
        let listed: Vec<String> = callers
            .iter()
            .take(12)
            .map(|(label, file)| format!("{} [{}]", inline(label), inline(file)))
            .collect();
        let _ = writeln!(brief, "Called from elsewhere by: {}", listed.join(", "));
    }
    if !imports.is_empty() {
        let _ = writeln!(
            brief,
            "Imports: {}",
            imports
                .iter()
                .take(12)
                .map(|import| inline(import))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Some(brief)
}

/// Where a session's state lives (under the brain's state dir).
#[must_use]
pub fn session_state_path(state_dir: &Path, session_id: &str) -> PathBuf {
    let safe: String = session_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    state_dir.join("hook-sessions").join(format!("{safe}.json"))
}

/// Load a session's state if the file exists and parses.
#[must_use]
pub fn load_session(state_dir: &Path, session_id: &str) -> Option<HookSession> {
    let path = session_state_path(state_dir, session_id);
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Persist a session's state.
///
/// # Errors
///
/// Returns filesystem errors.
pub fn save_session(state_dir: &Path, session_id: &str, session: &HookSession) -> Result<()> {
    let path = session_state_path(state_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| Error::Config(format!("hook session dir: {error}")))?;
    }
    write_private_sidecar(&path, &serde_json::to_vec(session)?)
}

fn write_private_sidecar(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .ok_or_else(|| Error::Config("sidecar needs a parent".to_owned()))?;
    std::fs::create_dir_all(parent).map_err(|e| Error::Config(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| Error::Config(e.to_string()))?;
    }
    let mut file =
        tempfile::NamedTempFile::new_in(parent).map_err(|e| Error::Config(e.to_string()))?;
    file.write_all(bytes)
        .map_err(|e| Error::Config(e.to_string()))?;
    file.persist(path)
        .map_err(|e| Error::Config(e.to_string()))?;
    Ok(())
}

/// Classify a shell command as a verification run. Returns the evidence
/// kind (`test_result` | `lint_result` | `build_result` |
/// `typecheck_result`) or `None` for ordinary commands.
///
/// Environment assignments, `env`/`time`/`timeout`/`nice` wrappers and runners
/// that return their tool's exit status (`uv run`, `poetry run`, `pdm run`,
/// `pipenv run`, `npx`, `bunx`, `uvx`, `pnpm exec`) are skipped. Help, version
/// and listing invocations run nothing and are not verification. Formatters
/// count only in check mode, and `--fix` runs rewrite files instead of
/// verifying them; neither is verification. A bare `make` (or any target
/// other than `test`, `lint`, `check` or `build`) is not either.
#[must_use]
pub fn detect_verification(command: &str) -> Option<&'static str> {
    let tokens = verification_tokens(command);
    let has = |flag: &str| tokens.contains(&flag);
    let formatter = tokens.starts_with(&["cargo", "fmt"])
        || tokens.starts_with(&["terraform", "fmt"])
        || tokens.starts_with(&["ruff", "format"])
        || tokens.starts_with(&["python", "-m", "ruff", "format"]);
    if tokens
        .iter()
        .any(|token| NON_RUN_FLAGS.contains(token) || *token == "--listTests")
        || tokens.starts_with(&["cargo", "nextest", "list"])
        || has("--fix")
        || has("--fix-only")
        || (formatter && !has("--check") && !has("-check"))
    {
        return None;
    }
    let normalized = tokens.join(" ");
    let matches_start = |needles: &[&str]| {
        needles
            .iter()
            .any(|needle| normalized == *needle || normalized.starts_with(&format!("{needle} ")))
    };
    if matches_start(&[
        "cargo test",
        "cargo nextest",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "bun test",
        "pytest",
        "python -m pytest",
        "go test",
        "jest",
        "vitest",
        "./gradlew test",
        "gradle test",
        "swift test",
        "mvn test",
        "make test",
    ]) {
        // `--no-run` compiles the tests without executing any of them.
        return Some(if tokens.contains(&"--no-run") {
            "build_result"
        } else {
            "test_result"
        });
    }
    if matches_start(&[
        "cargo clippy",
        "cargo fmt",
        "eslint",
        "npx eslint",
        "ruff",
        "ruff check",
        "python -m ruff",
        "flake8",
        "python -m flake8",
        "golangci-lint",
        "actionlint",
        "terraform fmt",
        "make lint",
        "npm run lint",
        "pnpm lint",
        "pnpm run lint",
    ]) {
        return Some("lint_result");
    }
    if matches_start(&[
        "tsc",
        "npx tsc",
        "mypy",
        "python -m mypy",
        "pyright",
        "terraform validate",
        "cargo check",
        "make check",
    ]) {
        return Some("typecheck_result");
    }
    if matches_start(&[
        "cargo build",
        "npm run build",
        "pnpm build",
        "pnpm run build",
        "yarn build",
        "go build",
        "make build",
        "./gradlew build",
        "swift build",
    ]) {
        return Some("build_result");
    }
    None
}

/// Flags that print help, versions or test lists instead of running anything.
const NON_RUN_FLAGS: &[&str] = &[
    "--help",
    "-h",
    "--version",
    "-V",
    "--list",
    "--collect-only",
    "--co",
];

/// Command tokens after assignments, wrappers and status-preserving runners.
fn verification_tokens(command: &str) -> Vec<&str> {
    let mut tokens: Vec<&str> = command.split_whitespace().collect();
    loop {
        let assignments = tokens
            .iter()
            .take_while(|token| token.contains('=') && !token.starts_with('/'))
            .count();
        tokens.drain(..assignments);
        let wrapper = match tokens.as_slice() {
            ["time", "-p", ..]
            | ["uv" | "poetry" | "pdm" | "pipenv", "run", ..]
            | ["pnpm", "exec", ..] => 2,
            ["nice", "-n", _, ..] => 3,
            ["timeout", duration, ..] if is_duration(duration) => 2,
            ["env" | "time" | "nice" | "npx" | "bunx" | "uvx", ..] => 1,
            _ => 0,
        };
        if wrapper > 0 {
            tokens.drain(..wrapper);
        } else if tokens.first() == Some(&"cargo")
            && tokens.get(1).is_some_and(|t| t.starts_with('+'))
        {
            tokens.remove(1); // `cargo +toolchain test`
        } else if tokens.first() == Some(&"python3") {
            tokens[0] = "python";
        } else if assignments == 0 {
            return tokens;
        }
    }
}

fn is_duration(token: &str) -> bool {
    let digits = token.trim_end_matches(['s', 'm', 'h', 'd']);
    !digits.is_empty()
        && digits
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

/// A recognized verification inside a shell command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellVerification {
    /// Evidence kind of the last recognized verification in the command.
    pub kind: &'static str,
    /// Whether any recognized verification in the command runs tests.
    pub tests: bool,
    /// True when the shell's exit status is every verification's own and all
    /// of them run in one modeled directory: a zero status then proves that
    /// every recognized verification exited 0 there.
    pub status_bearing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellOperator {
    And,
    Or,
    Sequence,
    Background,
    Pipe,
}

/// Top-level simple commands, each with the operator that follows it.
type ShellCommands = Vec<(String, Option<ShellOperator>)>;

/// Find the recognized verifications in shell text and whether their exit
/// status reaches the shell. Status-bearing requires, for every verification:
/// no `||` or background job anywhere; nothing after the first verification
/// is joined by `;` or a newline, so only `&&` (or pipelines joined by `&&`)
/// follow it; each verification ends its pipeline unless an unconditional
/// `set -o pipefail` ran first; and every directory change is modeled (see
/// [`shell_workdir`]). Syntax this reader does not model (subshells, groups,
/// here-documents, backquotes, unbalanced quotes) is never status-bearing.
#[must_use]
pub fn classify_verification(command: &str) -> Option<ShellVerification> {
    let Some(commands) = shell_commands(command) else {
        let kind = detect_verification(legacy_verification_text(command))?;
        return Some(ShellVerification {
            kind,
            tests: kind == "test_result",
            status_bearing: false,
        });
    };
    let verifications: Vec<(usize, &'static str)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, (text, _))| detect_verification(text).map(|kind| (index, kind)))
        .collect();
    let (first, _) = *verifications.first()?;
    let (_, kind) = *verifications.last()?;
    let status_bearing = statuses_reach_shell(&commands, &verifications)
        && workdir_changes(&commands, first).is_some();
    Some(ShellVerification {
        kind,
        tests: verifications.iter().any(|(_, kind)| *kind == "test_result"),
        status_bearing,
    })
}

/// Text checked when the shell syntax cannot be modeled: an optional leading
/// `cd DIR &&` is skipped.
fn legacy_verification_text(command: &str) -> &str {
    command
        .trim()
        .strip_prefix("cd ")
        .and_then(|rest| rest.split_once("&&"))
        .map_or(command, |(_, rest)| rest.trim())
}

fn pipeline_start(commands: &ShellCommands, index: usize) -> usize {
    let mut start = index;
    while start > 0 && commands[start - 1].1 == Some(ShellOperator::Pipe) {
        start -= 1;
    }
    start
}

fn pipeline_end(commands: &ShellCommands, index: usize) -> usize {
    let mut end = index;
    while end + 1 < commands.len() && commands[end].1 == Some(ShellOperator::Pipe) {
        end += 1;
    }
    end
}

/// Whether every verification's own exit status determines the shell's.
fn statuses_reach_shell(commands: &ShellCommands, verifications: &[(usize, &str)]) -> bool {
    use ShellOperator::{And, Background, Or, Pipe};
    let Some(&(first, _)) = verifications.first() else {
        return false;
    };
    let joined_by_and = commands[pipeline_end(commands, first)..]
        .iter()
        .all(|(_, operator)| matches!(operator, None | Some(And | Pipe)));
    joined_by_and
        && !commands
            .iter()
            .any(|(_, operator)| matches!(operator, Some(Or | Background)))
        && verifications.iter().all(|&(index, _)| {
            pipeline_end(commands, index) == index
                || pipefail_before(commands, pipeline_start(commands, index))
        })
}

/// Whether `pipefail` is on before command `start`. Only an unconditional
/// statement enables it (the first command, or one after `;` or a newline,
/// outside a pipeline); any statement that may disable it disables it.
fn pipefail_before(commands: &ShellCommands, start: usize) -> bool {
    let mut pipefail = false;
    for (index, (text, operator)) in commands[..start].iter().enumerate() {
        let previous = index
            .checked_sub(1)
            .and_then(|previous| commands[previous].1);
        let unconditional = matches!(previous, None | Some(ShellOperator::Sequence))
            && *operator != Some(ShellOperator::Pipe);
        match pipefail_setting(text) {
            Some(true) if unconditional => pipefail = true,
            Some(false) => pipefail = false,
            _ => {}
        }
    }
    pipefail
}

/// `Some(true)` when the command enables `pipefail` (`set -o pipefail`,
/// `set -euo pipefail`, zsh `setopt pipe_fail`), `Some(false)` when it
/// disables it (`set +o pipefail`, `setopt no_pipe_fail`, `unsetopt
/// pipefail`).
fn pipefail_setting(command: &str) -> Option<bool> {
    let option =
        |name: &str, enable: bool| match name.replace('_', "").to_ascii_lowercase().as_str() {
            "pipefail" => Some(enable),
            "nopipefail" => Some(!enable),
            _ => None,
        };
    let tokens: Vec<&str> = command.split_whitespace().collect();
    match tokens.as_slice() {
        ["set", options @ ..] => options.windows(2).rev().find_map(|pair| {
            let (flag, name) = (pair[0], pair[1]);
            let short_option = flag.len() >= 2 && flag.ends_with('o') && !flag.starts_with("--");
            match flag.as_bytes().first() {
                Some(b'-') if short_option => option(name, true),
                Some(b'+') if short_option => option(name, false),
                _ => None,
            }
        }),
        [command @ ("setopt" | "unsetopt"), options @ ..] => options
            .iter()
            .rev()
            .find_map(|name| option(name, *command == "setopt")),
        _ => None,
    }
}

/// The directory the command's verifications run in: `base` changed by the
/// static `cd`/`pushd` targets before the first verification. Without a
/// verification, leading `cd`/`pushd` commands joined by `&&` apply. `None`
/// when the shell text does not model the directory: a change joined by `;`
/// or a newline (it may fail while later commands still run), a change in a
/// pipeline, at or after the first verification, `cd -`, a bare `cd`,
/// expansions, `popd`, `source` of anything but a virtualenv `activate`
/// script, `eval` or `exec`.
#[must_use]
pub fn shell_workdir(command: &str, base: &Path) -> Option<PathBuf> {
    let commands = shell_commands(command)?;
    let changes = match commands
        .iter()
        .position(|(text, _)| detect_verification(text).is_some())
    {
        Some(first) => workdir_changes(&commands, first)?,
        None => leading_directory_changes(&commands)?,
    };
    Some(
        changes
            .iter()
            .fold(base.to_path_buf(), |directory, target| {
                directory.join(target)
            }),
    )
}

/// Directory changes before the first verification, when every one is
/// modeled and joined to the first verification by `&&` alone.
fn workdir_changes(commands: &ShellCommands, first: usize) -> Option<Vec<String>> {
    let mut changes = Vec::new();
    for (index, (text, operator)) in commands.iter().enumerate() {
        let change = directory_change(text);
        if change == DirectoryChange::Unchanged {
            continue;
        }
        let in_pipeline = *operator == Some(ShellOperator::Pipe)
            || (index > 0 && commands[index - 1].1 == Some(ShellOperator::Pipe));
        let DirectoryChange::To(target) = change else {
            return None;
        };
        if index >= first
            || in_pipeline
            || !commands[index..first]
                .iter()
                .all(|(_, operator)| *operator == Some(ShellOperator::And))
        {
            return None;
        }
        changes.push(target);
    }
    Some(changes)
}

/// Leading `cd`/`pushd` commands joined by `&&` (routing for commands without
/// a verification).
fn leading_directory_changes(commands: &ShellCommands) -> Option<Vec<String>> {
    let mut changes = Vec::new();
    for (text, operator) in commands {
        match directory_change(text) {
            DirectoryChange::Unchanged => break,
            DirectoryChange::To(target) if *operator == Some(ShellOperator::And) => {
                changes.push(target);
            }
            DirectoryChange::To(_) | DirectoryChange::Unmodeled => return None,
        }
    }
    Some(changes)
}

/// How one simple command can change the shell's working directory.
#[derive(Debug, PartialEq, Eq)]
enum DirectoryChange {
    /// The command leaves the directory alone.
    Unchanged,
    /// `cd`/`pushd` to a static target.
    To(String),
    /// A change this reader cannot model: `cd -`, a bare `cd`, expansions,
    /// `popd`, `source` of anything but a virtualenv `activate`, `eval`, `exec`.
    Unmodeled,
}

fn directory_change(command: &str) -> DirectoryChange {
    let raw: Vec<&str> = command
        .split_whitespace()
        .skip_while(|token| token.contains('=') && !token.starts_with('/'))
        .collect();
    let name = match raw.as_slice() {
        ["builtin" | "command", name, ..] | [name, ..] => *name,
        [] => return DirectoryChange::Unchanged,
    };
    match name {
        "cd" | "pushd" => match shell_words(command).as_deref() {
            Some([.., change, target])
                if (change == "cd" || change == "pushd")
                    && !target.is_empty()
                    && !target.starts_with(['-', '~']) =>
            {
                DirectoryChange::To(target.clone())
            }
            _ => DirectoryChange::Unmodeled,
        },
        "popd" | "eval" | "exec" => DirectoryChange::Unmodeled,
        "source" | "." => match shell_words(command).as_deref() {
            // Activating a virtualenv changes the environment, not the directory.
            Some([_, script]) if script == "activate" || script.ends_with("/activate") => {
                DirectoryChange::Unchanged
            }
            _ => DirectoryChange::Unmodeled,
        },
        _ => DirectoryChange::Unchanged,
    }
}

/// Words of one simple command when none of them needs expansion.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match ch {
            ' ' | '\t' => {
                if std::mem::take(&mut in_word) {
                    words.push(std::mem::take(&mut word));
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        literal => word.push(literal),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '$' | '`' | '\\' => return None,
                        literal => word.push(literal),
                    }
                }
            }
            '$' | '`' | '\\' | '*' | '?' | '[' => return None,
            literal => {
                in_word = true;
                word.push(literal);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Some(words)
}

/// Identity of one verification run for per-command results: the modeled
/// directory (`?` when unknown) and the recognized verification commands
/// without wrappers or redirections. `cargo test 2>&1 | tail` and
/// `cargo test` share a result; `cargo test -p a` and `cargo test -p b` do not.
fn verification_key(command: &str, workdir: Option<&Path>) -> String {
    let verifications: Vec<String> = shell_commands(command).map_or_else(
        || vec![normalized_verification(legacy_verification_text(command))],
        |commands| {
            commands
                .iter()
                .filter(|(text, _)| detect_verification(text).is_some())
                .map(|(text, _)| normalized_verification(text))
                .collect()
        },
    );
    format!(
        "{}\n{}",
        workdir.map_or_else(|| "?".to_owned(), |dir| dir.display().to_string()),
        verifications.join(" && ")
    )
}

/// Verification tokens without redirections such as `2>&1` or `> log`.
fn normalized_verification(text: &str) -> String {
    let mut kept = Vec::new();
    let mut target = false;
    for token in verification_tokens(text) {
        if std::mem::take(&mut target) {
            continue;
        }
        let body = token.trim_start_matches(|ch: char| ch.is_ascii_digit());
        if body.starts_with(['>', '<']) || body.starts_with("&>") {
            target = matches!(body, ">" | ">>" | ">|" | "<" | "&>" | "&>>");
            continue;
        }
        kept.push(token);
    }
    kept.join(" ")
}

/// A recognized verification in a shell tool call, before its result is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingVerification {
    /// Directory and normalized verification commands; its latest result is
    /// tracked per episode.
    pub key: String,
    /// Whether the command runs tests.
    pub tests: bool,
    /// The modeled directory the verifications run in.
    pub workdir: Option<PathBuf>,
}

/// Whether the tool runs shell text that can carry a verification.
#[must_use]
pub fn is_shell_tool(payload: &serde_json::Value) -> bool {
    payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|name| {
            matches!(
                name,
                "Bash" | "shell" | "exec_command" | "functions.exec_command"
            )
        })
}

/// The recognized verification in a shell tool call, from its text alone.
/// No daemon is needed, so its result can be marked unknown before anything
/// else can fail or time out.
#[must_use]
pub fn pending_verification(
    payload: &serde_json::Value,
    cwd: &Path,
) -> Option<PendingVerification> {
    if !is_shell_tool(payload) {
        return None;
    }
    let input = payload
        .get("tool_input")
        .unwrap_or(&serde_json::Value::Null);
    let command = tool_command(input);
    let verification = classify_verification(command)?;
    let workdir = shell_workdir(command, &tool_base_dir(cwd, input))
        .map(|directory| normalize_tool_path(&directory));
    Some(PendingVerification {
        key: verification_key(command, workdir.as_deref()),
        tests: verification.tests,
        workdir,
    })
}

/// Write-ahead without the daemon: record a pending verification's result as
/// unknown in the episodes it may belong to, before recording can fail. The
/// episodes whose recorded root most specifically contains its directory are
/// marked; without such a root, every episode of the session is.
pub fn mark_verification_unknown(
    state_dir: &Path,
    session_id: &str,
    pending: &PendingVerification,
) {
    let sessions = project_sessions(state_dir, session_id);
    let depth = |session: &HookSession| {
        let directory = pending.workdir.as_ref()?;
        let root = session.root.as_ref()?;
        directory
            .starts_with(root)
            .then(|| root.components().count())
    };
    let deepest = sessions
        .iter()
        .filter_map(|(_, session)| depth(session))
        .max();
    for (scope, mut session) in sessions {
        if deepest.is_some_and(|deepest| depth(&session) != Some(deepest)) {
            continue;
        }
        session.set_verification_result(&pending.key, pending.tests, None);
        session.last_verification_exit = None;
        let _ = save_session(state_dir, &scope, &session);
    }
}

/// Copies opaque shell spans (quotes, substitutions) into the current command.
struct ShellScanner<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    current: String,
}

impl ShellScanner<'_> {
    /// Copy a backslash escape and the character it protects.
    fn escaped(&mut self) {
        self.current.push('\\');
        if let Some(next) = self.chars.next() {
            self.current.push(next);
        }
    }

    /// Copy a quoted span after its opening quote; `None` when unterminated
    /// or when a double-quoted span contains a backquote substitution.
    fn quoted(&mut self, open: char) -> Option<()> {
        self.current.push(open);
        while let Some(ch) = self.chars.next() {
            match ch {
                '\\' if open == '"' => self.escaped(),
                '`' if open == '"' => return None,
                _ => {
                    self.current.push(ch);
                    if ch == open {
                        return Some(());
                    }
                }
            }
        }
        None
    }

    /// Copy `$( … )` after its opening `$(`, including nested parentheses.
    fn substitution(&mut self) -> Option<()> {
        self.current.push_str("$(");
        let mut depth = 1_usize;
        while let Some(ch) = self.chars.next() {
            match ch {
                '\'' | '"' => self.quoted(ch)?,
                '\\' => self.escaped(),
                '`' => return None,
                _ => {
                    self.current.push(ch);
                    match ch {
                        '(' => depth += 1,
                        ')' if depth == 1 => return Some(()),
                        ')' => depth -= 1,
                        _ => {}
                    }
                }
            }
        }
        None
    }

    /// Copy `${ … }` after its `$`.
    fn parameter(&mut self) -> Option<()> {
        self.current.push('$');
        for ch in self.chars.by_ref() {
            self.current.push(ch);
            if ch == '}' {
                return Some(());
            }
        }
        None
    }
}

/// Top-level simple commands of shell text, each with the operator after it.
fn shell_commands(text: &str) -> Option<Vec<(String, Option<ShellOperator>)>> {
    let mut scanner = ShellScanner {
        chars: text.chars().peekable(),
        current: String::new(),
    };
    let mut commands = Vec::new();
    let mut word_start = true;
    while let Some(ch) = scanner.chars.next() {
        let starts_word = word_start;
        word_start = ch.is_whitespace();
        let operator = match ch {
            '\'' | '"' => scanner.quoted(ch).map(|()| None)?,
            '\\' => {
                scanner.escaped();
                None
            }
            '$' if scanner.chars.next_if_eq(&'(').is_some() => {
                scanner.substitution().map(|()| None)?
            }
            '$' if scanner.chars.peek() == Some(&'{') => scanner.parameter().map(|()| None)?,
            '`' | '(' | ')' | '{' | '}' => return None,
            '#' if starts_word => {
                while scanner.chars.next_if(|next| *next != '\n').is_some() {}
                None
            }
            '<' if scanner.chars.peek() == Some(&'<') => return None,
            '|' if scanner.chars.next_if_eq(&'|').is_some() => Some(ShellOperator::Or),
            '|' => {
                let _ = scanner.chars.next_if_eq(&'&'); // `|&` also pipes stderr
                Some(ShellOperator::Pipe)
            }
            '&' if scanner.chars.next_if_eq(&'&').is_some() => Some(ShellOperator::And),
            '&' if scanner.chars.peek() == Some(&'>') || scanner.current.ends_with(['>', '<']) => {
                scanner.current.push(ch); // redirection such as `2>&1` or `&>`
                None
            }
            '&' => Some(ShellOperator::Background),
            ';' if scanner.chars.peek() == Some(&';') => return None,
            ';' | '\n' => Some(ShellOperator::Sequence),
            _ => {
                scanner.current.push(ch);
                None
            }
        };
        if let Some(operator) = operator {
            word_start = true;
            let command = scanner.current.trim().to_owned();
            scanner.current.clear();
            if !command.is_empty() {
                commands.push((command, Some(operator)));
            } else if operator != ShellOperator::Sequence {
                return None;
            }
        }
    }
    let last = scanner.current.trim();
    if !last.is_empty() {
        commands.push((last.to_owned(), None));
    } else if commands.last().is_some_and(|(_, operator)| {
        matches!(
            operator,
            Some(ShellOperator::And | ShellOperator::Or | ShellOperator::Pipe)
        )
    }) {
        return None;
    }
    Some(commands)
}

/// The exit status the harness itself reported for a shell tool call, with
/// how it was observed: `tool_response` (an explicit code), `failure_event`
/// (Claude Code `PostToolUseFailure` "Exit code N") or `success_event`
/// (a completed foreground Claude Code `PostToolUse` Bash call; Claude Code
/// reports every non-zero exit as a failure event, so only `claude_code`
/// payloads may infer 0). Interrupted, backgrounded or reinterpreted
/// (`returnCodeInterpretation`) calls, and failures without an exit code,
/// have no known status.
#[must_use]
pub fn observed_exit_status(
    payload: &serde_json::Value,
    claude_code: bool,
) -> Option<(i32, &'static str)> {
    use serde_json::Value;
    let event = payload.get("hook_event_name").and_then(Value::as_str);
    if event == Some("PostToolUseFailure") {
        // Only Claude Code writes "Exit code N" itself. Other clients' failure
        // text is the command's own output, which can begin with any line.
        if !claude_code || payload.get("is_interrupt").and_then(Value::as_bool) == Some(true) {
            return None;
        }
        let error = payload.get("error").and_then(Value::as_str)?;
        let line = error.lines().next()?.trim();
        let code = line
            .strip_prefix("Error: ")
            .unwrap_or(line)
            .strip_prefix("Exit code ")?
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|code| *code != 0)?;
        return Some((code, "failure_event"));
    }
    let empty = Value::Null;
    let response = payload
        .get("tool_result")
        .or_else(|| payload.get("tool_response"))
        .unwrap_or(&empty);
    if let Some(code) = extract_exit_code(response) {
        return Some((code, "tool_response"));
    }
    if !claude_code
        || event != Some("PostToolUse")
        || payload.get("tool_name").and_then(Value::as_str) != Some("Bash")
        || payload
            .pointer("/tool_input/run_in_background")
            .and_then(Value::as_bool)
            == Some(true)
    {
        return None;
    }
    let result = response.as_object()?;
    let completed = result.get("stdout").is_some_and(Value::is_string)
        && result.get("interrupted").and_then(Value::as_bool) == Some(false)
        && [
            "backgroundTaskId",
            "backgroundedByUser",
            "timedOutAfterMs",
            "returnCodeInterpretation",
        ]
        .iter()
        .all(|key| {
            result
                .get(*key)
                .is_none_or(|value| value.is_null() || *value == Value::Bool(false))
        });
    completed.then_some((0, "success_event"))
}

/// Bounded verification output: stdout and stderr, or the failure text.
#[must_use]
pub fn verification_output(payload: &serde_json::Value) -> String {
    use serde_json::Value;
    let mut text = String::new();
    match payload
        .get("tool_result")
        .or_else(|| payload.get("tool_response"))
    {
        Some(Value::String(output)) => text.push_str(output),
        Some(Value::Object(result)) => {
            let nonempty = |key: &str| {
                result
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
            };
            if let Some(output) = [
                "stdout",
                "aggregated_output",
                "formatted_output",
                "output",
                "content",
            ]
            .into_iter()
            .find_map(nonempty)
            {
                text.push_str(output);
            }
            if let Some(stderr) = nonempty("stderr") {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(stderr);
            }
        }
        _ => {}
    }
    if text.trim().is_empty() {
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            text.push_str(error);
        }
    }
    bounded_tail(&text, VERIFICATION_EXCERPT_BYTES)
}

/// Keep the end of long output, where runners print their verdicts.
fn bounded_tail(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut start = text.len() - limit.saturating_sub(64);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    if let Some(newline) = text[start..].find('\n').filter(|offset| *offset < 256) {
        start += newline + 1;
    }
    format!("[{start} earlier bytes omitted]\n{}", &text[start..])
}

/// SHA-256 of files a verification plausibly exercised, keyed by
/// checkout-relative path. Paths must stay inside `root`; missing files are
/// skipped. Returns whether size or count bounds left candidates out.
#[must_use]
pub fn source_digests(root: &Path, candidates: &[String]) -> (BTreeMap<String, String>, bool) {
    use std::io::Read as _;
    let mut digests = BTreeMap::new();
    let Ok(root) = root.canonicalize() else {
        return (digests, !candidates.is_empty());
    };
    let mut truncated = false;
    let mut remaining = SOURCE_DIGEST_TOTAL_BYTES;
    for relative in candidates {
        let path = Path::new(relative);
        if digests.contains_key(relative)
            || relative.is_empty()
            || relative.len() > SOURCE_DIGEST_PATH_BYTES
            || !path
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            continue;
        }
        if digests.len() >= SOURCE_DIGEST_FILES {
            truncated = true;
            break;
        }
        let Ok(canonical) = root.join(path).canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&root) || !canonical.is_file() {
            continue;
        }
        let mut bytes = Vec::new();
        let limit = SOURCE_DIGEST_FILE_BYTES.min(remaining);
        if std::fs::File::open(&canonical)
            .and_then(|file| file.take(limit + 1).read_to_end(&mut bytes))
            .is_err()
        {
            continue;
        }
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if size > limit {
            truncated = true;
            continue;
        }
        remaining -= size;
        digests.insert(relative.clone(), crate::experience::sha256(&bytes));
    }
    (digests, truncated)
}

/// Source digests a native hook recorded beside its verification. Evidence
/// reported by agents, the CLI or completions never qualifies.
#[must_use]
pub fn hook_source_digests(metadata: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    if !matches!(
        metadata.get("source").and_then(serde_json::Value::as_str),
        Some(CLAUDE_HOOK_SOURCE | CODEX_HOOK_SOURCE | CURSOR_HOOK_SOURCE)
    ) {
        return None;
    }
    let digests = metadata.get("source_digests")?.as_object()?;
    Some(
        digests
            .iter()
            .filter_map(|(path, digest)| {
                digest
                    .as_str()
                    .filter(|digest| {
                        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .map(|digest| (path.clone(), digest.to_ascii_lowercase()))
            })
            .collect(),
    )
}

/// Provenance only the native hook path may attach to evidence: the client's
/// hook label and the content digests it observed. The daemon accepts it only
/// on its hook endpoint, which the CLI and MCP do not expose.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct HookProvenance {
    pub source: String,
    #[serde(default)]
    pub source_digests: BTreeMap<String, String>,
    #[serde(default)]
    pub source_digests_truncated: bool,
}

/// Metadata as stored. Client-supplied metadata loses the reserved hook keys
/// (a `source` ending in `-hook`, `source_digests`); hook-endpoint
/// `provenance` with a known hook label adds them back. This prevents
/// accidental forgery by the CLI, MCP or other clients. It is not a security
/// boundary: any process of the same user can reach the hook endpoint.
#[must_use]
pub fn evidence_metadata(
    metadata: serde_json::Value,
    provenance: Option<&HookProvenance>,
) -> serde_json::Value {
    use serde_json::Value;
    let mut metadata = metadata;
    if let Value::Object(fields) = &mut metadata {
        if fields
            .get("source")
            .and_then(Value::as_str)
            .is_some_and(|source| source.ends_with("-hook"))
        {
            fields.remove("source");
        }
        fields.remove("source_digests");
        fields.remove("source_digests_truncated");
    }
    let Some(provenance) = provenance.filter(|provenance| {
        matches!(
            provenance.source.as_str(),
            CLAUDE_HOOK_SOURCE | CODEX_HOOK_SOURCE | CURSOR_HOOK_SOURCE
        )
    }) else {
        return metadata;
    };
    if !metadata.is_object() {
        metadata = Value::Object(serde_json::Map::new());
    }
    metadata["source"] = provenance.source.clone().into();
    metadata["source_digests"] = serde_json::json!(provenance.source_digests);
    if provenance.source_digests_truncated {
        metadata["source_digests_truncated"] = true.into();
    }
    metadata
}

/// A harness-observed verification ready to record as hook evidence.
#[derive(Clone, Debug, PartialEq)]
pub struct ObservedVerification {
    pub kind: &'static str,
    pub command: String,
    pub exit_code: i32,
    pub artifact: String,
    /// How the status was observed, the Git HEAD and the tool call id.
    pub metadata: serde_json::Value,
    /// Hook label and source digests, sent only through the hook endpoint.
    pub provenance: HookProvenance,
    /// The tool call (or, without a call id, the observed content).
    pub fingerprint: String,
}

/// Build evidence for a shell call whose verification status the harness
/// reported. `None` for ordinary commands, an unknown status, or a status the
/// shell syntax hides. `source` is the client's hook label (for example
/// `CLAUDE_HOOK_SOURCE`); `session_paths` are paths this session touched.
#[must_use]
pub fn observe_verification(
    payload: &serde_json::Value,
    source: &'static str,
    root: &Path,
    git: Option<&crate::projects::GitState>,
    session_paths: &[String],
) -> Option<ObservedVerification> {
    let input = payload
        .get("tool_input")
        .unwrap_or(&serde_json::Value::Null);
    let command = tool_command(input);
    let verification = classify_verification(command)?;
    if !verification.status_bearing {
        return None;
    }
    let (exit_code, exit_status) = observed_exit_status(payload, source == CLAUDE_HOOK_SOURCE)?;
    let mut candidates = session_paths.to_vec();
    if let Some(git) = git {
        candidates.extend(git.changed_paths.iter().filter_map(|changed| {
            git.repository_root
                .join(changed)
                .strip_prefix(root)
                .ok()
                .map(|relative| relative.to_string_lossy().into_owned())
        }));
    }
    let (digests, truncated) = source_digests(root, &candidates);
    let command: String = command.chars().take(500).collect();
    let tool_use_id = payload
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256);
    // One tool call is one observation: a hook fired twice for the same call
    // is a duplicate, while a new call with identical output is a new run.
    // Clients without call ids fall back to the observed content.
    let fingerprint = tool_use_id.map_or_else(
        || {
            crate::experience::sha256(
                &serde_json::to_vec(&(command.trim(), exit_code, &digests)).unwrap_or_default(),
            )
        },
        |id| crate::experience::sha256(format!("tool:{id}").as_bytes()),
    );
    let mut metadata = serde_json::json!({
        "exit_status": exit_status,
        "head": git.and_then(|git| git.head.clone()),
    });
    if let Some(id) = tool_use_id {
        metadata["tool_use_id"] = id.into();
    }
    Some(ObservedVerification {
        kind: verification.kind,
        command,
        exit_code,
        artifact: verification_output(payload),
        metadata,
        provenance: HookProvenance {
            source: source.to_owned(),
            source_digests: digests,
            source_digests_truncated: truncated,
        },
        fingerprint,
    })
}

/// An Engram MCP call naming the task this session works on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskDeclaration {
    /// The agent started or recorded work on this task.
    Active {
        task_id: TaskId,
        project_id: Option<ProjectId>,
    },
    /// The agent closed this task; later verification is not part of it.
    Closed(TaskId),
}

/// MCP server names Engram registers under; `leb` is the pre-rename name.
const ENGRAM_MCP_SERVERS: &[&str] = &["engram", "leb"];

/// Read a successful Engram MCP task call (`engram_task_start`, attempts,
/// evidence, completion, outcome) from a post-tool hook payload. Only
/// Engram's own servers count (`mcp__engram__…`, `mcp__leb__…`, as Claude
/// Code and Codex name MCP tools); another server's tool of the same name
/// declares nothing.
#[must_use]
pub fn task_declaration(payload: &serde_json::Value) -> Option<TaskDeclaration> {
    use serde_json::Value;
    if payload.get("hook_event_name").and_then(Value::as_str) == Some("PostToolUseFailure") {
        return None;
    }
    let (server, name) = payload
        .get("tool_name")?
        .as_str()?
        .strip_prefix("mcp__")?
        .split_once("__")?;
    if !ENGRAM_MCP_SERVERS.contains(&server) {
        return None;
    }
    let tool = [
        "engram_task_start",
        "engram_record_attempt",
        "engram_record_evidence",
        "engram_complete_task",
        "engram_mark_outcome",
    ]
    .into_iter()
    .find(|tool| *tool == name)?;
    let input = payload.get("tool_input").unwrap_or(&Value::Null);
    let task = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(|id| TaskId::parse(id).ok())
    };
    let project = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .and_then(|id| ProjectId::parse(id).ok())
    };
    match tool {
        "engram_task_start" => {
            let record = mcp_result_object(
                payload
                    .get("tool_response")
                    .or_else(|| payload.get("tool_result"))?,
                0,
            )?;
            if record
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| status != "open")
            {
                return None;
            }
            Some(TaskDeclaration::Active {
                task_id: task(record.get("id"))?,
                project_id: project(record.get("project_id"))
                    .or_else(|| project(input.get("project_id"))),
            })
        }
        "engram_complete_task" | "engram_mark_outcome" => {
            Some(TaskDeclaration::Closed(task(input.get("task_id"))?))
        }
        _ => Some(TaskDeclaration::Active {
            task_id: task(input.get("task_id"))?,
            project_id: project(input.get("project_id")),
        }),
    }
}

/// The JSON object an MCP tool returned, from the shapes hosts pass to hooks.
fn mcp_result_object(
    value: &serde_json::Value,
    depth: usize,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    use serde_json::Value;
    if depth > 4 {
        return None;
    }
    match value {
        Value::Object(object) if object.contains_key("id") => Some(object.clone()),
        Value::Object(object) => object
            .get("structuredContent")
            .or_else(|| object.get("content"))
            .and_then(|inner| mcp_result_object(inner, depth + 1)),
        Value::Array(items) => items.iter().find_map(|item| {
            item.get("text")
                .and_then(|text| mcp_result_object(text, depth + 1))
        }),
        Value::String(text) if text.len() <= 64 * 1024 => serde_json::from_str::<Value>(text)
            .ok()
            .and_then(|parsed| mcp_result_object(&parsed, depth + 1)),
        _ => None,
    }
}

/// Route this session's later verification evidence for a project to the
/// declared task, or stop routing to a task the agent closed. Without a
/// project in the call, the declaration applies to `fallback_scope`.
///
/// # Errors
/// Returns filesystem errors writing the private declaration sidecar.
pub fn record_task_declaration(
    state_dir: &Path,
    session_id: &str,
    fallback_scope: &str,
    declaration: &TaskDeclaration,
) -> Result<()> {
    match declaration {
        TaskDeclaration::Active {
            task_id,
            project_id,
        } => {
            let scope = project_id.as_ref().map_or_else(
                || fallback_scope.to_owned(),
                |project| scoped_session_id(session_id, project),
            );
            write_private_sidecar(
                &session_state_path(state_dir, &scope).with_extension("declared-task"),
                task_id.as_str().as_bytes(),
            )
        }
        TaskDeclaration::Closed(task_id) => {
            for path in declaration_paths(state_dir, session_id) {
                if std::fs::read_to_string(&path)
                    .is_ok_and(|stored| stored.trim() == task_id.as_str())
                {
                    let _ = std::fs::remove_file(&path);
                }
            }
            Ok(())
        }
    }
}

/// The open task this session declared for one project scope.
#[must_use]
pub fn declared_task(state_dir: &Path, scope: &str) -> Option<TaskId> {
    let text = std::fs::read_to_string(
        session_state_path(state_dir, scope).with_extension("declared-task"),
    )
    .ok()?;
    TaskId::parse(text.trim()).ok()
}

/// Stop routing a scope's evidence to `task_id`: it closed outside the hook's
/// view or refused evidence that the session's own episode accepted.
pub fn drop_task_declaration(state_dir: &Path, scope: &str, task_id: &TaskId) {
    if declared_task(state_dir, scope).as_ref() == Some(task_id) {
        let _ = std::fs::remove_file(
            session_state_path(state_dir, scope).with_extension("declared-task"),
        );
    }
}

/// Forget every task declaration of a session (session end).
pub fn clear_task_declarations(state_dir: &Path, session_id: &str) {
    for path in declaration_paths(state_dir, session_id) {
        let _ = std::fs::remove_file(path);
    }
}

fn declaration_paths(state_dir: &Path, session_id: &str) -> Vec<PathBuf> {
    let prefix = format!(
        "{}-",
        &crate::experience::sha256(session_id.as_bytes())[..24]
    );
    let Ok(entries) = std::fs::read_dir(state_dir.join("hook-sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension().is_some_and(|ext| ext == "declared-task")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect()
}

/// Pull an explicit exit code out of the shapes tool results take. A bare
/// `is_error` flag is not an exit status: it reports whether the tool call
/// failed, which a client may decide without the command's status, so it
/// never stands in for one.
#[must_use]
pub fn extract_exit_code(tool_result: &serde_json::Value) -> Option<i32> {
    for key in ["exitCode", "exit_code", "code", "returnCode", "returncode"] {
        if let Some(code) = tool_result.get(key).and_then(serde_json::Value::as_i64) {
            return i32::try_from(code).ok();
        }
    }
    None
}

/// Bounded artifact excerpt from a tool result.
#[must_use]
pub fn extract_artifact(tool_result: &serde_json::Value) -> String {
    for key in ["stdout", "output", "content", "stderr"] {
        if let Some(text) = tool_result.get(key).and_then(serde_json::Value::as_str) {
            if !text.trim().is_empty() {
                // Truncate on a character boundary; multibyte output must not panic.
                let mut end = text.len().min(2_000);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                return text[..end].to_owned();
            }
        }
    }
    String::new()
}

/// Conservative outcome for an auto-closed session from the latest result of
/// each verification identity: `failure` when any latest result failed;
/// `success` only when every latest result is known and passed and at least
/// one of them ran tests; `abandoned` otherwise, including any unknown result.
/// A later lint, format, check or build pass never hides a failed test. User
/// acceptance stays unknown; ROI acceptance remains a separate signal.
#[must_use]
pub fn auto_close_outcome(results: &[VerificationResult]) -> &'static str {
    if results
        .iter()
        .any(|result| result.exit_code.is_some_and(|code| code != 0))
    {
        "failure"
    } else if results.iter().all(|result| result.exit_code == Some(0))
        && results.iter().any(|result| result.tests)
    {
        "success"
    } else {
        "abandoned"
    }
}

/// The hooks block written into a repo's `.claude/settings.json`.
#[must_use]
pub fn claude_hooks_json(engram_binary: &str) -> serde_json::Value {
    serde_json::json!({
        "SessionStart": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook session-start"),
                "timeout": 15
            }]
        }],
        "UserPromptSubmit": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook prompt"),
                "timeout": 30
            }]
        }],
        "PreToolUse": [{
            "matcher": "Read|Edit|Write|MultiEdit",
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook pretool"),
                "timeout": 10
            }]
        }],
        "PostToolUse": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook bash"),
                "timeout": 15
            }]
        }],
        "PostToolUseFailure": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook bash"),
                "timeout": 15
            }]
        }],
        "Stop": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook stop"),
                "timeout": 15
            }]
        }],
        "SessionEnd": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook session-end"),
                "timeout": 15
            }]
        }]
    })
}

/// The hooks block for local Codex clients (`~/.codex/hooks.json`): same events, same
/// stdin shape, but context returns via `hookSpecificOutput.additionalContext`
/// — the `--codex` flag switches the emitter. Evidence capture and session
/// close are format-agnostic.
#[must_use]
pub fn codex_hooks_json(engram_binary: &str) -> serde_json::Value {
    serde_json::json!({
        "SessionStart": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook session-start --codex"),
                "timeout": 15
            }]
        }],
        "UserPromptSubmit": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook prompt --codex"),
                "timeout": 30
            }]
        }],
        "PreToolUse": [{
            "matcher": "Read|Edit|Write|Bash",
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook pretool --codex"),
                "timeout": 10
            }]
        }],
        "PostToolUse": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook bash --codex"),
                "timeout": 15
            }]
        }],
        "Stop": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook stop --codex"),
                "timeout": 15
            }]
        }],
        "SessionEnd": [{
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook session-end --codex"),
                "timeout": 3
            }]
        }]
    })
}

/// Merge native Codex hooks, repairing the flat layout emitted by older Engram.
/// Foreign handlers and metadata survive upgrades, including mixed handler groups.
///
/// # Errors
/// Rejects malformed hook documents without replacing their contents.
pub fn merge_codex_hooks(
    mut document: serde_json::Value,
    binary: &str,
) -> Result<serde_json::Value> {
    merge_native_hooks(&mut document, &codex_hooks_json(binary), true)?;
    Ok(document)
}

/// Refresh Claude Code handlers without replacing other handlers or settings.
/// # Errors
/// Rejects malformed hook documents.
pub fn merge_claude_hooks(
    mut document: serde_json::Value,
    binary: &str,
) -> Result<serde_json::Value> {
    merge_native_hooks(&mut document, &claude_hooks_json(binary), false)?;
    Ok(document)
}

fn merge_native_hooks(
    document: &mut serde_json::Value,
    additions: &serde_json::Value,
    repair_flat: bool,
) -> Result<()> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Config("Hook settings must be a JSON object".into()))?;
    let mut hooks = root
        .remove("hooks")
        .unwrap_or_else(|| serde_json::json!({}));
    let target = hooks
        .as_object_mut()
        .ok_or_else(|| Error::Config("Hook settings.hooks must be an object".into()))?;
    for event in additions
        .as_object()
        .expect("generated object")
        .keys()
        .filter(|_| repair_flat)
    {
        if let Some(legacy) = root.remove(event) {
            let entries = legacy
                .as_array()
                .ok_or_else(|| Error::Config(format!("Codex legacy {event} must be an array")))?;
            let slot = target
                .entry(event.clone())
                .or_insert_with(|| serde_json::json!([]));
            slot.as_array_mut()
                .ok_or_else(|| Error::Config(format!("Hook event {event} must be an array")))?
                .extend(entries.iter().cloned());
        }
    }
    for (event, groups) in target.iter_mut() {
        let groups = groups
            .as_array_mut()
            .ok_or_else(|| Error::Config(format!("Hook event {event} must be an array")))?;
        for group in groups.iter_mut() {
            let handlers = group
                .get_mut("hooks")
                .and_then(serde_json::Value::as_array_mut)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "Hook event {event} group must contain a hooks array"
                    ))
                })?;
            handlers.retain(|handler| {
                !handler
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(is_engram_hook)
            });
        }
        groups.retain(|group| group["hooks"].as_array().is_some_and(|v| !v.is_empty()));
    }
    for (event, groups) in additions.as_object().expect("generated object") {
        target
            .entry(event.clone())
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .expect("validated array")
            .extend(groups.as_array().expect("generated array").iter().cloned());
    }
    root.insert("hooks".into(), hooks);
    Ok(())
}

pub(crate) fn is_engram_hook(command: &str) -> bool {
    // Decode only the executable shell word. Never treat an argument that
    // mentions an Engram path as ownership of the enclosing command.
    let mut program = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut end = command.len();
    let command = command.trim_start();
    for (index, ch) in command.char_indices() {
        if escaped {
            program.push(ch);
            escaped = false;
        } else if ch == '\\' && quote != Some('\'') {
            escaped = true;
        } else if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if quote.is_none() && ch.is_whitespace() {
            end = index;
            break;
        } else {
            program.push(ch);
        }
    }
    if quote.is_some() || escaped || end >= command.len() {
        return false;
    }
    let args: Vec<_> = command[end..].split_whitespace().collect();
    if !matches!(
        args.as_slice(),
        [
            "hook",
            "session-start" | "prompt" | "pretool" | "bash" | "stop" | "session-end"
        ] | [
            "hook",
            "session-start" | "prompt" | "pretool" | "bash" | "stop" | "session-end",
            "--codex" | "--cursor"
        ]
    ) {
        return false;
    }
    matches!(
        Path::new(&program).file_name().and_then(|v| v.to_str()),
        Some("engram" | "leb")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_detection_classifies_and_ignores() {
        assert_eq!(
            detect_verification("cargo test --lib foo"),
            Some("test_result")
        );
        assert_eq!(
            detect_verification("RUST_LOG=debug cargo test"),
            Some("test_result"),
            "env-var prefixes are tolerated"
        );
        assert_eq!(detect_verification("pnpm run lint"), Some("lint_result"));
        assert_eq!(
            detect_verification("npx tsc --noEmit"),
            Some("typecheck_result")
        );
        assert_eq!(
            detect_verification("cargo build --release"),
            Some("build_result")
        );
        assert_eq!(detect_verification("git status"), None);
        assert_eq!(detect_verification("cat cargo test"), None);
        assert_eq!(
            detect_verification("cargotest"),
            None,
            "prefix matching requires a token boundary"
        );
    }

    #[test]
    fn exit_codes_come_from_any_common_shape() {
        assert_eq!(
            extract_exit_code(&serde_json::json!({"exitCode": 2})),
            Some(2)
        );
        assert_eq!(
            extract_exit_code(&serde_json::json!({"exit_code": 0})),
            Some(0)
        );
        assert_eq!(extract_exit_code(&serde_json::json!({"stdout": "x"})), None);
    }

    /// Item 8: a tool-call error flag is not a command exit status.
    #[test]
    fn an_error_flag_alone_is_not_an_exit_status() {
        for flag in [true, false] {
            let response = serde_json::json!({"is_error": flag, "stdout": "ok"});
            assert_eq!(extract_exit_code(&response), None, "is_error={flag}");
            let payload = serde_json::json!({"hook_event_name":"PostToolUse",
                "tool_name":"Bash","tool_input":{"command":"cargo test"},
                "tool_response":response});
            // Codex and Cursor payloads (no Claude Code inference) stay unknown.
            assert_eq!(
                observed_exit_status(&payload, false),
                None,
                "is_error={flag}"
            );
        }
    }

    fn result(key: &str, tests: bool, exit_code: Option<i32>) -> VerificationResult {
        VerificationResult {
            key: key.to_owned(),
            tests,
            exit_code,
        }
    }

    #[test]
    fn auto_close_is_conservative() {
        assert_eq!(auto_close_outcome(&[result("t", true, Some(0))]), "success");
        assert_eq!(auto_close_outcome(&[result("t", true, Some(1))]), "failure");
        assert_eq!(auto_close_outcome(&[result("t", true, None)]), "abandoned");
        assert_eq!(auto_close_outcome(&[]), "abandoned");
        assert_eq!(
            auto_close_outcome(&[result("lint", false, Some(0))]),
            "abandoned",
            "success needs a passing test"
        );
    }

    /// Item 1: a later passing lint, format, check or build never hides a
    /// failed test; only the same verification passing again clears it.
    #[test]
    fn a_failed_test_is_not_masked_by_later_passing_checks() {
        let mut session = HookSession {
            session_id: None,
            task_id: TaskId::new(),
            project_id: ProjectId::new(),
            last_verification_exit: None,
            last_evidence_fingerprint: None,
            root: None,
            verification_results: Vec::new(),
        };
        session.set_verification_result("/r\ncargo test", true, Some(101));
        for key in [
            "/r\ncargo fmt --check",
            "/r\ncargo check",
            "/r\ncargo build",
        ] {
            session.set_verification_result(key, false, Some(0));
        }
        assert_eq!(auto_close_outcome(&session.verification_results), "failure");
        session.set_verification_result("/r\ncargo test", true, Some(0));
        assert_eq!(auto_close_outcome(&session.verification_results), "success");
        session.set_verification_result("/r\ncargo test", true, None);
        assert_eq!(
            auto_close_outcome(&session.verification_results),
            "abandoned",
            "an unknown latest result is never success"
        );
        // Eviction keeps failures: passes leave first.
        session.set_verification_result("/r\ncargo test", true, Some(101));
        for index in 0..(VERIFICATION_RESULTS_MAX + 5) {
            session.set_verification_result(&format!("/r\ncargo test -p p{index}"), true, Some(0));
        }
        assert_eq!(session.verification_results.len(), VERIFICATION_RESULTS_MAX);
        assert_eq!(auto_close_outcome(&session.verification_results), "failure");
        // Rewriting tools and bare make targets are not verification.
        for command in [
            "cargo fmt",
            "cargo fmt --all",
            "terraform fmt",
            "ruff format .",
            "eslint . --fix",
            "ruff check --fix .",
            "cargo clippy --fix --allow-dirty",
            "make",
            "make install",
            "make deploy",
        ] {
            assert_eq!(detect_verification(command), None, "{command}");
        }
        for (command, kind) in [
            ("cargo fmt --check", "lint_result"),
            ("cargo fmt --all -- --check", "lint_result"),
            ("terraform fmt -check", "lint_result"),
            ("ruff format --check .", "lint_result"),
            ("make build", "build_result"),
            ("make test", "test_result"),
        ] {
            assert_eq!(detect_verification(command), Some(kind), "{command}");
        }
    }

    #[test]
    fn evidence_fingerprints_dedupe_identical_results_only() {
        let first = evidence_fingerprint("cargo test --lib", 0);
        assert_eq!(first, evidence_fingerprint("  cargo test --lib  ", 0));
        assert_ne!(first, evidence_fingerprint("cargo test --lib", 1));
        assert_ne!(first, evidence_fingerprint("cargo test", 0));
    }

    #[test]
    fn file_paths_extract_from_common_tool_shapes() {
        assert_eq!(
            extract_file_path(&serde_json::json!({"file_path": "src/a.rs"})).as_deref(),
            Some("src/a.rs")
        );
        assert_eq!(
            extract_file_path(&serde_json::json!({"filePath": "b.ts"})).as_deref(),
            Some("b.ts")
        );
        assert_eq!(
            extract_file_path(&serde_json::json!({"command": "ls"})),
            None
        );
    }

    #[test]
    fn delivery_lock_recovers_dead_owners_but_preserves_live_owners() {
        let temp = tempfile::TempDir::new().unwrap();
        let claim =
            claim_context_delivery(temp.path(), "session", "inspect", &[], 256, None).unwrap();
        assert!(
            claim_context_delivery(temp.path(), "session", "inspect", &[], 256, None).is_none()
        );
        drop(claim);
        let mut dead = std::process::Command::new("true").spawn().unwrap();
        let pid = dead.id();
        dead.wait().unwrap();
        let lock =
            session_state_path(temp.path(), "session").with_extension("context-delivering.lock");
        std::fs::create_dir(&lock).unwrap();
        std::fs::write(lock.join(format!("{pid}-dead")), []).unwrap();
        let recovered =
            claim_context_delivery(temp.path(), "session", "inspect", &[], 256, None).unwrap();
        let run_id = crate::ids::ContextRunId::new();
        recovered.delivered("request", &run_id, "context").unwrap();
        drop(recovered);
        let repeated = claim_context_delivery(temp.path(), "session", "inspect", &[], 256, None)
            .expect("unchanged inputs must still check changed knowledge");
        assert!(!repeated.should_emit("context").unwrap());
        drop(repeated);
        assert!(
            claim_context_delivery(temp.path(), "session", "changed", &[], 256, None).is_some()
        );
    }

    #[test]
    fn delivery_receipt_atomically_replaces_ids_and_reads_legacy_sidecars() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = session_state_path(temp.path(), "session");
        let legacy_id = crate::ids::ContextRunId::new();
        write_private_sidecar(
            &path.with_extension("context-run"),
            &serde_json::to_vec(&legacy_id).unwrap(),
        )
        .unwrap();
        assert_eq!(
            context_run_id(temp.path(), "session"),
            Some(legacy_id.clone())
        );
        let run_id = crate::ids::ContextRunId::new();
        let claim =
            claim_context_delivery(temp.path(), "session", "inspect", &[], 256, None).unwrap();
        claim.delivered("new-request", &run_id, "context").unwrap();
        drop(claim);
        let receipt: serde_json::Value = serde_json::from_slice(
            &std::fs::read(path.with_extension("context-delivery")).unwrap(),
        )
        .unwrap();
        assert_eq!(receipt["run_id"], run_id.as_str());
        assert_eq!(receipt["request_id"], "new-request");
        assert_eq!(context_run_id(temp.path(), "session"), Some(run_id.clone()));
        assert_eq!(
            legacy_context_run_id(temp.path(), "session"),
            Some(legacy_id),
            "legacy compatibility is read-only"
        );
        // A new budget rechecks inputs, but identical text keeps both emitted IDs.
        let claim =
            claim_context_delivery(temp.path(), "session", "inspect", &[], 512, None).unwrap();
        assert!(!claim.should_emit("context").unwrap());
        drop(claim);
        assert_eq!(context_run_id(temp.path(), "session"), Some(run_id));
        assert_eq!(
            context_request_id(temp.path(), "session").as_deref(),
            Some("new-request")
        );
        let repeated =
            claim_context_delivery(temp.path(), "session", "inspect", &[], 512, None).unwrap();
        assert!(!repeated.should_emit("context").unwrap());
    }

    #[test]
    fn legacy_text_receipts_reemit_once_before_semantic_suppression() {
        let temp = tempfile::TempDir::new().unwrap();
        let run = crate::ids::ContextRunId::new();
        let path = session_state_path(temp.path(), "legacy").with_extension("context-delivery");
        write_private_sidecar(
            &path,
            &serde_json::to_vec(&serde_json::json!({
                "fingerprint": "old-inputs", "request_id": "old-request", "run_id": run,
                "text_digest": crate::experience::sha256(b"rendered context")
            }))
            .unwrap(),
        )
        .unwrap();
        let claim =
            claim_context_delivery(temp.path(), "legacy", "inspect", &[], 256, None).unwrap();
        assert_eq!(context_run_id(temp.path(), "legacy"), Some(run.clone()));
        assert!(claim.should_emit_digest("semantic-context-v1").unwrap());
        claim
            .delivered_digest("new-request", &run, "semantic-context-v1")
            .unwrap();
        assert!(!claim.should_emit_digest("semantic-context-v1").unwrap());
    }

    #[test]
    fn semantic_receipts_retry_failures_and_track_only_the_latest_successful_pack() {
        let temp = tempfile::TempDir::new().unwrap();
        let acquire =
            |scope| claim_context_delivery(temp.path(), scope, "inspect", &[], 256, None).unwrap();
        let first = crate::ids::ContextRunId::new();
        let claim = acquire("session-project");
        assert!(claim.should_emit_digest("a+b@revision-1").unwrap());
        claim
            .delivered_digest("first", &first, "a+b@revision-1")
            .unwrap();
        drop(claim);

        // A failed output never calls delivered_digest, so the exact attempt retries.
        let failed = acquire("session-project");
        assert!(failed.should_emit_digest("a+b@revision-2").unwrap());
        drop(failed);
        let retry = acquire("session-project");
        assert!(retry.should_emit_digest("a+b@revision-2").unwrap());
        assert!(!retry.should_emit_digest("a+b@revision-1").unwrap());
        let second = crate::ids::ContextRunId::new();
        retry
            .delivered_digest("second", &second, "a@revision-1")
            .unwrap();
        drop(retry);

        // A removal is a new pack; restoring an old set is eligible again.
        let restored = acquire("session-project");
        assert!(restored.should_emit_digest("a+b@revision-1").unwrap());
        restored.invalidate_empty().unwrap();
        assert!(restored.should_emit_digest("a@revision-1").unwrap());
        assert_eq!(context_run_id(temp.path(), "session-project"), Some(second));
        assert_eq!(
            context_request_id(temp.path(), "session-project").as_deref(),
            Some("second")
        );
        drop(restored);

        for scope in ["other-task-project", "session-other-project"] {
            assert!(acquire(scope).should_emit_digest("a+b@revision-1").unwrap());
        }
    }

    #[test]
    fn recent_paths_are_bounded_and_track_observation_recency() {
        let temp = tempfile::TempDir::new().unwrap();
        let paths: Vec<_> = (0..40).map(|n| format!("src/{n}.rs")).collect();
        remember_paths(temp.path(), "session", &paths).unwrap();
        remember_paths(temp.path(), "session", &["src/12.rs".to_owned()]).unwrap();
        let recent = recent_paths(temp.path(), "session");
        assert_eq!(recent.len(), 32);
        assert_eq!(recent[0], "src/12.rs");
        assert!(!recent.contains(&"src/0.rs".to_owned()));
        assert_eq!(
            recent
                .iter()
                .filter(|path| path.as_str() == "src/12.rs")
                .count(),
            1
        );
    }

    #[test]
    fn file_briefing_dedupes_per_session() {
        let temp = tempfile::TempDir::new().expect("temp");
        assert!(mark_file_briefed(temp.path(), "s1", "src/a.rs"));
        assert!(!mark_file_briefed(temp.path(), "s1", "src/a.rs"));
        assert!(mark_file_briefed(temp.path(), "s1", "src/b.rs"));
        assert!(
            mark_file_briefed(temp.path(), "s2", "src/a.rs"),
            "per-session"
        );
    }

    #[test]
    fn additional_context_envelope_is_well_formed() {
        let output = additional_context_output("PreToolUse", "shape here");
        let parsed: serde_json::Value = serde_json::from_str(&output).expect("json");
        assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            "shape here"
        );
    }

    #[test]
    fn session_brief_renders_open_and_stays_silent_when_quiet() {
        assert_eq!(render_session_brief("app", &[], None, 0), None);
        let open = vec![(
            "tsk_1".to_owned(),
            "fix the flaky auth test".to_owned(),
            0_i64,
        )];
        let brief = render_session_brief(
            "app",
            &open,
            Some(("success", "add rate limits")),
            7_200_000,
        )
        .expect("brief");
        assert!(brief.contains("tsk_1"));
        assert!(brief.contains("2h ago"));
        assert!(brief.contains("success — add rate limits"));
    }

    #[test]
    fn file_shape_renders_bounded_and_skips_unknown_files() {
        assert_eq!(render_file_shape("x.rs", &[], &[], &[]), None);
        let shape = render_file_shape(
            "src/auth.rs",
            &[("Function".to_owned(), "validate".to_owned())],
            &[("login".to_owned(), "src/routes.rs".to_owned())],
            &["crate::db".to_owned()],
        )
        .expect("shape");
        assert!(shape.contains("validate (Function)"));
        assert!(shape.contains("login [src/routes.rs]"));
        assert!(shape.contains("crate::db"));
    }

    #[test]
    fn claims_are_won_exactly_once_per_session_and_tag() {
        let temp = tempfile::TempDir::new().expect("temp");
        assert!(claim_once(temp.path(), "s1", "prompt"));
        assert!(
            !claim_once(temp.path(), "s1", "prompt"),
            "second racer loses"
        );
        assert!(
            claim_once(temp.path(), "s1", "brief"),
            "tags are independent"
        );
        assert!(
            claim_once(temp.path(), "s2", "prompt"),
            "sessions are independent"
        );
        clear_claims(temp.path(), "s1");
        assert!(
            claim_once(temp.path(), "s1", "prompt"),
            "cleared at session end"
        );
    }

    #[test]
    fn relay_messages_queue_and_drain_once() {
        let temp = tempfile::TempDir::new().expect("temp");
        relay_send(temp.path(), "sess-a", "operator", "check the auth tests").expect("send");
        relay_send(temp.path(), "sess-a", "sess-b", "backend contract changed").expect("send 2");
        let drained = relay_drain(temp.path(), "sess-a");
        assert_eq!(drained.len(), 2);
        // Messages created in the same millisecond have no relative age.
        assert!(
            drained
                .iter()
                .any(|m| m == "from operator: check the auth tests")
        );
        assert!(
            drained
                .iter()
                .any(|m| m == "from sess-b: backend contract changed")
        );
        assert!(
            relay_drain(temp.path(), "sess-a").is_empty(),
            "drain consumes"
        );
        assert!(relay_drain(temp.path(), "sess-c").is_empty(), "empty inbox");
    }

    #[test]
    fn session_paths_are_sanitized() {
        let path = session_state_path(Path::new("/state"), "abc/../../etc:passwd");
        assert!(
            path.to_string_lossy()
                .contains("hook-sessions/abcetcpasswd.json")
        );
    }

    #[test]
    fn subagent_threads_keep_separate_delivery_state_that_session_end_clears() {
        let temp = tempfile::TempDir::new().unwrap();
        let scope = scoped_session_id("root-session", &ProjectId::new());
        let root = thread_scope(&scope, &serde_json::json!({"tool_name":"Read"}));
        assert_eq!(root, scope);
        let worker = thread_scope(&scope, &serde_json::json!({"agent_id":"worker-1"}));
        let other = thread_scope(&scope, &serde_json::json!({"agent_id":"worker-2"}));
        assert_ne!(worker, scope);
        assert_ne!(worker, other);
        // Sidecar names keep 80 characters; a truncated name would merge threads.
        let stem = |id: &str| {
            session_state_path(temp.path(), id)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(stem(&worker), worker);
        assert!(mark_file_briefed(temp.path(), &worker, "src/lib.rs"));
        assert!(
            mark_file_briefed(temp.path(), &scope, "src/lib.rs"),
            "the root conversation was never briefed"
        );
        let claim = claim_context_delivery(temp.path(), &worker, "task", &[], 256, None).unwrap();
        claim
            .delivered("request", &crate::ids::ContextRunId::new(), "context")
            .unwrap();
        drop(claim);
        let root_claim =
            claim_context_delivery(temp.path(), &scope, "task", &[], 256, None).unwrap();
        assert!(root_claim.should_emit("context").unwrap());
        drop(root_claim);
        clear_thread_scopes(temp.path(), &scope);
        let remaining: Vec<String> = std::fs::read_dir(temp.path().join("hook-sessions"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            remaining.iter().all(|name| !name.starts_with(&worker)),
            "{remaining:?}"
        );
        assert!(remaining.iter().any(|name| name.starts_with(&scope)));
    }

    #[test]
    fn runners_and_wrappers_classify_while_listings_run_nothing() {
        for (command, kind) in [
            ("uv run pytest -q", "test_result"),
            ("python3 -m pytest tests", "test_result"),
            ("timeout 600 cargo test -p engram", "test_result"),
            ("env RUST_LOG=debug cargo +nightly test", "test_result"),
            ("npx vitest run", "test_result"),
            ("uvx ruff check .", "lint_result"),
            ("terraform validate", "typecheck_result"),
            ("cargo test --no-run", "build_result"),
        ] {
            assert_eq!(detect_verification(command), Some(kind), "{command}");
        }
        for command in [
            "cargo test -- --list",
            "pytest --collect-only",
            "cargo nextest list",
            "cargo test --help",
            "python3 script.py",
            "timeout cargo test",
        ] {
            assert_eq!(detect_verification(command), None, "{command}");
        }
    }

    #[test]
    fn exit_status_reaches_the_shell_only_through_and_chains_or_pipefail() {
        for command in [
            "cargo test",
            "cd '/tmp/a b' && cargo test --lib",
            "RUST_LOG=info cargo test 2>&1",
            "cargo fmt --check && cargo clippy -- -D warnings && cargo test",
            "cargo test && echo done",
            "cargo test && git diff | head",
            "echo y | cargo test",
            "set -o pipefail; cargo test 2>&1 | tail -n 40",
            "set -euo pipefail && cargo test |& tee out.log | tail -5",
            "cargo test # every suite",
            "cargo test --manifest-path \"$(pwd)/Cargo.toml\" > out.txt 2>&1",
            "cargo test -- --test-threads=$(nproc)",
            "export A=1; cd sub && cargo test",
            "cd a && cd b && cargo test",
            "source .venv/bin/activate && cargo test",
        ] {
            assert_eq!(
                classify_verification(command),
                Some(ShellVerification {
                    kind: "test_result",
                    tests: true,
                    status_bearing: true
                }),
                "{command}"
            );
        }
        for command in [
            "cargo test 2>&1 | tail -20",
            "cd repo && cargo test 2>&1 | grep -E 'test result|FAILED' | head",
            "cargo test || true",
            "false || cargo test",
            "cargo test; echo done",
            "cargo test &",
            "set -o pipefail | cat; cargo test | tail",
            "set -o pipefail; set +o pipefail; cargo test | tail",
            "(cargo test)",
            "cargo test <<EOF\nx\nEOF",
            "echo `date` && cargo test",
            "cargo test 'unterminated",
            "cargo test |",
            "cd \"${HOME}/repo\" && cargo test",
        ] {
            assert!(
                !classify_verification(command).is_some_and(|found| found.status_bearing),
                "{command}"
            );
        }
        assert_eq!(classify_verification("git status && echo cargo test"), None);
        assert_eq!(classify_verification("ls | grep test"), None);
    }

    /// Item 2: every recognized verification must carry its own status, not
    /// only the last one.
    #[test]
    fn every_verification_in_a_command_must_be_status_bearing() {
        for command in [
            "cargo test; cargo clippy",
            "cargo test\ncargo clippy",
            "npm test; npm run lint",
            "cargo test | tail -5 && cargo build",
            "cargo test && cargo clippy 2>&1 | tail",
            "cargo build && cargo test; echo done",
        ] {
            let found = classify_verification(command).expect("recognized");
            assert!(!found.status_bearing, "{command}");
        }
        let both = classify_verification("cargo build && cargo test").expect("recognized");
        assert!(both.status_bearing && both.tests);
        assert_eq!(both.kind, "test_result");
        let lint_only = classify_verification("cargo fmt --check && cargo clippy").expect("lint");
        assert!(lint_only.status_bearing && !lint_only.tests);
    }

    /// Item 3: the directory comes from the same parse; a change that may not
    /// take effect makes the run unmodeled.
    #[test]
    fn verification_directories_are_modeled_or_unknown() {
        let base = Path::new("/work/one");
        for (command, expected) in [
            ("cargo test", Some("/work/one")),
            ("cd ../two && cargo test", Some("/work/one/../two")),
            (
                "export A=1; cd ../two && cargo test",
                Some("/work/one/../two"),
            ),
            ("cd a && cd b && cargo test", Some("/work/one/a/b")),
            ("cd /abs && cargo test", Some("/abs")),
            ("cd '/tmp/a b' && cargo test", Some("/tmp/a b")),
            ("pushd ../two && cargo test", Some("/work/one/../two")),
            ("cd ../two; cargo test", None),
            ("cd ../two\ncargo test", None),
            ("cd ../two && export A=1; cargo test", None),
            ("cd - && cargo test", None),
            ("cd && cargo test", None),
            ("cd ~/two && cargo test", None),
            ("cd $REPO && cargo test", None),
            ("cargo test && cd ../two && cargo test", None),
            ("popd && cargo test", None),
            ("source env.sh && cargo test", None),
            ("cd ../two | true && cargo test", None),
            // Without a verification only leading `cd ... &&` routes the tool.
            ("cd ../two && git status", Some("/work/one/../two")),
            ("cd ../two; git status", None),
        ] {
            assert_eq!(
                shell_workdir(command, base),
                expected.map(PathBuf::from),
                "{command}"
            );
            if expected.is_none() {
                assert!(
                    !classify_verification(command).is_some_and(|found| found.status_bearing),
                    "{command}"
                );
            }
        }
        // Routing follows the parse too: `cd X; cmd` stays in the tool's
        // directory, and the verification there is not status-bearing.
        let input = serde_json::json!({"command":"export A=1; cd ../two && cargo test"});
        assert_eq!(tool_workdir(base, &input), PathBuf::from("/work/two"));
    }

    /// Item 6: only Engram's own MCP servers declare tasks.
    #[test]
    fn only_engram_mcp_servers_declare_tasks() {
        use serde_json::json;
        let task = TaskId::new();
        for name in [
            "mcp__engram__engram_record_attempt",
            "mcp__leb__engram_record_attempt",
        ] {
            assert!(
                task_declaration(&json!({"tool_name":name,"tool_input":{"task_id":task}}))
                    .is_some(),
                "{name}"
            );
        }
        for name in [
            "mcp__other__engram_record_attempt",
            "mcp__engramx__engram_record_attempt",
            "mcp__engram__engram_record_attempt_v2",
            "MCP:engram_record_attempt",
            "engram_record_attempt",
            "engram.engram_record_attempt",
        ] {
            assert_eq!(
                task_declaration(&json!({"tool_name":name,"tool_input":{"task_id":task}})),
                None,
                "{name}"
            );
        }
    }

    /// Item 9: client metadata cannot claim hook provenance; only the hook
    /// endpoint's provenance can.
    #[test]
    fn client_metadata_cannot_claim_hook_provenance() {
        use serde_json::json;
        let digest = "a".repeat(64);
        let forged = json!({"source":CLAUDE_HOOK_SOURCE,"source_digests":{"lib.rs":digest},
            "source_digests_truncated":true,"note":"kept"});
        let stored = evidence_metadata(forged.clone(), None);
        assert_eq!(stored, json!({"note":"kept"}));
        assert!(hook_source_digests(&stored).is_none());
        // Ordinary `source` labels that are not hook labels stay.
        assert_eq!(
            evidence_metadata(json!({"source":"ci"}), None),
            json!({"source":"ci"})
        );
        let provenance = HookProvenance {
            source: CODEX_HOOK_SOURCE.to_owned(),
            source_digests: BTreeMap::from([("lib.rs".to_owned(), digest.clone())]),
            source_digests_truncated: false,
        };
        let hooked = evidence_metadata(json!({"exit_status":"tool_response"}), Some(&provenance));
        assert_eq!(hooked["source"], CODEX_HOOK_SOURCE);
        assert_eq!(hooked["exit_status"], "tool_response");
        assert_eq!(hook_source_digests(&hooked).expect("hook").len(), 1);
        // An unknown label in hook provenance claims nothing.
        let unknown = HookProvenance {
            source: "made-up-hook".to_owned(),
            ..provenance
        };
        assert!(hook_source_digests(&evidence_metadata(forged, Some(&unknown))).is_none());
    }

    /// Item 7: only an unconditional statement enables pipefail, and every
    /// negative form disables it.
    #[test]
    fn pipefail_counts_only_when_set_unconditionally() {
        for command in [
            "set -o pipefail; cargo test | tail -3",
            "set -eo pipefail; cargo test | tail -3",
            "setopt pipefail; cargo test | tail -3",
            "setopt PIPE_FAIL; cargo test | tail -3",
            "unsetopt no_pipe_fail; cargo test | tail -3",
        ] {
            assert!(
                classify_verification(command).is_some_and(|found| found.status_bearing),
                "{command}"
            );
        }
        for command in [
            "setopt pipefail; setopt no_pipe_fail; cargo test | tail",
            "setopt pipefail; setopt nopipefail; cargo test | tail",
            "set -o pipefail; set +o pipefail; cargo test | tail",
            "set -o pipefail; unsetopt pipefail; cargo test | tail",
            "false && set -o pipefail; cargo test | tail -3",
            "true && set -o pipefail && cargo test | tail -3",
            "set -o pipefail | cat; cargo test | tail -3",
            "set -o pipefail; false && set +o pipefail; cargo test | tail -3",
        ] {
            assert!(
                !classify_verification(command).is_some_and(|found| found.status_bearing),
                "{command}"
            );
        }
    }

    #[test]
    fn claude_code_exit_status_comes_from_the_event_not_the_output() {
        use serde_json::json;
        let success = json!({"hook_event_name":"PostToolUse","tool_name":"Bash",
            "tool_input":{"command":"cargo test"},
            "tool_response":{"stdout":"test result: ok. 3 passed","stderr":"",
                "interrupted":false,"isImage":false,"noOutputExpected":false}});
        assert_eq!(
            observed_exit_status(&success, true),
            Some((0, "success_event"))
        );
        assert_eq!(
            observed_exit_status(&success, false),
            None,
            "only Claude Code payloads may infer status 0"
        );
        for (key, value) in [
            ("interrupted", json!(true)),
            ("backgroundTaskId", json!("bg-1")),
            ("returnCodeInterpretation", json!("No matches found")),
            ("timedOutAfterMs", json!(120_000)),
        ] {
            let mut changed = success.clone();
            changed["tool_response"][key] = value;
            assert_eq!(observed_exit_status(&changed, true), None, "{key}");
        }
        let mut background = success.clone();
        background["tool_input"]["run_in_background"] = json!(true);
        assert_eq!(observed_exit_status(&background, true), None);
        let mut other_tool = success.clone();
        other_tool["tool_name"] = json!("Read");
        assert_eq!(observed_exit_status(&other_tool, true), None);
        let failure = json!({"hook_event_name":"PostToolUseFailure","tool_name":"Bash",
            "tool_input":{"command":"cargo test"},
            "error":"Exit code 101\nerror: test failed","is_interrupt":false});
        assert_eq!(
            observed_exit_status(&failure, true),
            Some((101, "failure_event"))
        );
        let mut prefixed = failure.clone();
        prefixed["error"] = json!("Error: Exit code 2");
        assert_eq!(
            observed_exit_status(&prefixed, true),
            Some((2, "failure_event"))
        );
        for (key, value) in [
            ("is_interrupt", json!(true)),
            ("error", json!("Permission to use Bash has been denied")),
            ("error", json!("Exit code 0")),
        ] {
            let mut changed = failure.clone();
            changed[key] = value;
            assert_eq!(observed_exit_status(&changed, true), None, "{key}");
        }
        // Other clients' failure text is command output; a first line that
        // happens to read "Exit code N" must not become a recorded status.
        assert_eq!(observed_exit_status(&failure, false), None);
        let codex = json!({"hook_event_name":"PostToolUse","tool_name":"exec_command",
            "tool_response":{"exit_code":3,"stdout":""}});
        assert_eq!(
            observed_exit_status(&codex, false),
            Some((3, "tool_response"))
        );
        let legacy =
            json!({"tool_name":"Bash","tool_response":{"stdout":"ok","interrupted":false}});
        assert_eq!(
            observed_exit_status(&legacy, true),
            None,
            "without an event name a completed call proves nothing"
        );
    }

    #[test]
    fn verification_output_keeps_a_bounded_utf8_tail() {
        let long = format!("{}\ntest result: ok. 9 passed", "é".repeat(5_000));
        let excerpt = verification_output(&serde_json::json!({
            "tool_response":{"stdout":long,"stderr":""}
        }));
        assert!(excerpt.len() <= VERIFICATION_EXCERPT_BYTES);
        assert!(excerpt.starts_with('[') && excerpt.ends_with("test result: ok. 9 passed"));
        assert_eq!(
            verification_output(&serde_json::json!({"error":"Exit code 101\nthread panicked"})),
            "Exit code 101\nthread panicked"
        );
        assert_eq!(
            verification_output(&serde_json::json!({
                "tool_response":{"stdout":"out","stderr":"warning: unused"}
            })),
            "out\nwarning: unused"
        );
        // Three-byte characters straddle the old 2,000-byte cut; it must not panic.
        assert_eq!(
            extract_artifact(&serde_json::json!({"stdout":"€".repeat(1_000)})).len(),
            1_998
        );
    }

    #[test]
    fn engram_mcp_calls_declare_and_close_the_session_task() {
        use serde_json::json;
        let task = TaskId::new();
        let project = ProjectId::new();
        let record =
            json!({"accepted_by_user":null,"id":task,"project_id":project,"status":"open"})
                .to_string();
        for response in [
            json!(record),
            json!([{"type":"text","text":record}]),
            json!({"content":[{"type":"text","text":record}],"isError":false}),
        ] {
            let start = json!({"hook_event_name":"PostToolUse",
                "tool_name":"mcp__leb__engram_task_start",
                "tool_input":{"project_id":project,"prompt":"Fix"},"tool_response":response});
            assert_eq!(
                task_declaration(&start),
                Some(TaskDeclaration::Active {
                    task_id: task.clone(),
                    project_id: Some(project.clone())
                })
            );
        }
        assert_eq!(
            task_declaration(
                &json!({"tool_name":"mcp__engram__engram_record_attempt","tool_input":{"task_id":task}})
            ),
            Some(TaskDeclaration::Active {
                task_id: task.clone(),
                project_id: None
            })
        );
        assert_eq!(
            task_declaration(&json!({"tool_name":"mcp__engram__engram_complete_task",
                "tool_input":{"task_id":task}})),
            Some(TaskDeclaration::Closed(task.clone()))
        );
        assert_eq!(
            task_declaration(&json!({"hook_event_name":"PostToolUseFailure",
                "tool_name":"mcp__leb__engram_record_attempt","tool_input":{"task_id":task}})),
            None,
            "a failed call declares nothing"
        );
        for tool in ["mcp__leb__engram_recall", "Bash", "xengram_task_start"] {
            assert_eq!(
                task_declaration(&json!({"tool_name":tool,"tool_input":{"task_id":task}})),
                None,
                "{tool}"
            );
        }
        assert_eq!(
            task_declaration(
                &json!({"tool_name":"mcp__engram__engram_record_attempt","tool_input":{"task_id":"x"}})
            ),
            None
        );

        let temp = tempfile::tempdir().expect("temp");
        let state = temp.path();
        let scope = scoped_session_id("session-a", &project);
        let active = TaskDeclaration::Active {
            task_id: task.clone(),
            project_id: Some(project.clone()),
        };
        record_task_declaration(state, "session-a", "fallback", &active).expect("declare");
        assert_eq!(declared_task(state, &scope), Some(task.clone()));
        for (session, closed) in [("session-a", TaskId::new()), ("session-b", task.clone())] {
            record_task_declaration(state, session, "fallback", &TaskDeclaration::Closed(closed))
                .expect("close");
            assert_eq!(
                declared_task(state, &scope),
                Some(task.clone()),
                "only the declaring session closing this task stops routing"
            );
        }
        record_task_declaration(
            state,
            "session-a",
            "fallback",
            &TaskDeclaration::Closed(task.clone()),
        )
        .expect("close");
        assert_eq!(declared_task(state, &scope), None);
        let projectless = TaskDeclaration::Active {
            task_id: task.clone(),
            project_id: None,
        };
        record_task_declaration(state, "session-a", &scope, &projectless).expect("fallback");
        assert_eq!(declared_task(state, &scope), Some(task));
        clear_task_declarations(state, "session-a");
        assert_eq!(declared_task(state, &scope), None);
    }

    #[test]
    fn source_digests_stay_inside_the_checkout_and_bounded() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("repo");
        std::fs::create_dir_all(root.join("src")).expect("source dir");
        std::fs::write(root.join("src/lib.rs"), b"pub fn a() {}").expect("source");
        std::fs::write(temp.path().join("outside.rs"), b"private").expect("outside");
        #[cfg(unix)]
        std::os::unix::fs::symlink(temp.path().join("outside.rs"), root.join("src/link.rs"))
            .expect("symlink");
        let candidates = [
            "src/lib.rs",
            "../outside.rs",
            "/etc/hosts",
            "src/link.rs",
            "src/missing.rs",
            "src",
            "src/lib.rs",
        ]
        .map(str::to_owned);
        let (digests, truncated) = source_digests(&root, &candidates);
        assert_eq!(
            digests.into_iter().collect::<Vec<_>>(),
            vec![(
                "src/lib.rs".to_owned(),
                crate::experience::sha256(b"pub fn a() {}")
            )]
        );
        assert!(!truncated);
        let many: Vec<String> = (0..70)
            .map(|index| {
                std::fs::write(root.join(format!("f{index}.rs")), b"x").expect("file");
                format!("f{index}.rs")
            })
            .collect();
        let (digests, truncated) = source_digests(&root, &many);
        assert_eq!(digests.len(), SOURCE_DIGEST_FILES);
        assert!(truncated);
    }

    #[test]
    fn only_native_hook_metadata_carries_source_digests() {
        use serde_json::json;
        let digest = "a".repeat(64);
        for source in [CLAUDE_HOOK_SOURCE, CODEX_HOOK_SOURCE, CURSOR_HOOK_SOURCE] {
            let hook = json!({"source":source,
                "source_digests":{"src/lib.rs":digest,"bad":"not-a-digest"}});
            assert_eq!(
                hook_source_digests(&hook).expect("hook digests").len(),
                1,
                "{source}"
            );
        }
        for metadata in [
            json!({"capture":"completion","source_digests":{"src/lib.rs":digest}}),
            json!({"source":"cli","source_digests":{"src/lib.rs":digest}}),
            json!({"source":CODEX_HOOK_SOURCE}),
            json!({}),
        ] {
            assert!(hook_source_digests(&metadata).is_none_or(|found| found.is_empty()));
        }
    }

    #[test]
    fn observed_verification_records_status_output_and_source_digests() {
        use serde_json::json;
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        std::fs::write(root.join("lib.rs"), b"pub fn a() {}").expect("source");
        let payload = |command: &str| {
            json!({"hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"toolu_1",
                "tool_input":{"command":command},
                "tool_response":{"stdout":"test result: ok. 1 passed","stderr":"",
                    "interrupted":false,"isImage":false}})
        };
        let touched = ["lib.rs".to_owned()];
        let observed = observe_verification(
            &payload("cargo test"),
            CLAUDE_HOOK_SOURCE,
            root,
            None,
            &touched,
        )
        .expect("observed");
        assert_eq!((observed.kind, observed.exit_code), ("test_result", 0));
        assert_eq!(observed.artifact, "test result: ok. 1 passed");
        assert_eq!(observed.provenance.source, CLAUDE_HOOK_SOURCE);
        assert!(observed.metadata.get("source").is_none());
        assert_eq!(observed.metadata["exit_status"], "success_event");
        assert_eq!(observed.metadata["tool_use_id"], "toolu_1");
        assert_eq!(
            observed.provenance.source_digests["lib.rs"],
            crate::experience::sha256(b"pub fn a() {}")
        );
        for hidden in ["cargo test | tail -3", "cargo test || true", "git status"] {
            assert!(
                observe_verification(&payload(hidden), CLAUDE_HOOK_SOURCE, root, None, &touched)
                    .is_none(),
                "{hidden}"
            );
        }
        let repeat = observe_verification(
            &payload("cargo test"),
            CLAUDE_HOOK_SOURCE,
            root,
            None,
            &touched,
        )
        .expect("repeat");
        assert_eq!(
            repeat.fingerprint, observed.fingerprint,
            "a hook fired twice for one tool call is one observation"
        );
        let mut rerun = payload("cargo test");
        rerun["tool_use_id"] = json!("toolu_2");
        let rerun =
            observe_verification(&rerun, CLAUDE_HOOK_SOURCE, root, None, &touched).expect("rerun");
        assert_ne!(
            rerun.fingerprint, observed.fingerprint,
            "a new tool call with identical output is a new observation"
        );
        let mut legacy = payload("cargo test");
        legacy
            .as_object_mut()
            .expect("payload")
            .remove("tool_use_id");
        let before = observe_verification(&legacy, CLAUDE_HOOK_SOURCE, root, None, &touched)
            .expect("legacy");
        std::fs::write(root.join("lib.rs"), b"pub fn a() { changed() }").expect("edit");
        let after = observe_verification(&legacy, CLAUDE_HOOK_SOURCE, root, None, &touched)
            .expect("legacy edit");
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "without call ids, a rerun on edited source is a new observation"
        );
    }
}
