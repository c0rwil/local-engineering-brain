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
//! Every hook fails OPEN: a broken brain must never break a coding session.

pub mod codex;
pub mod cursor;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{ProjectId, TaskId};

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
    let explicit = input
        .get("cwd")
        .or_else(|| input.get("workdir"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from);
    let base = explicit.map_or_else(
        || cwd.to_path_buf(),
        |path| {
            if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            }
        },
    );
    let command = tool_command(input);
    let cd_path = command
        .trim()
        .strip_prefix("cd ")
        .and_then(|s| s.split_once("&&"))
        .map(|(path, _)| path.trim().trim_matches(['\'', '"']))
        .filter(|path| !path.contains(['$', '`', ';', '|', '\n']));
    let path = cd_path.map_or(base.clone(), |path| base.join(path));
    normalize_tool_path(&path)
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
            Self::ClaudeCode => "claude-code-hook",
            Self::Codex => "codex-hook",
            Self::Cursor => "cursor-hook",
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
    let mut brief = format!("# Engram session brief — {project_name}\n");
    if !open.is_empty() {
        brief.push_str("Open episodes from earlier sessions (resume or close them):\n");
        for (id, prompt, started_at_ms) in open.iter().take(3) {
            let mut prompt: String = prompt.chars().take(110).collect();
            if let Some(cut) = prompt.rfind(' ').filter(|_| prompt.len() >= 110) {
                prompt.truncate(cut);
            }
            let age_hours = (now_ms.saturating_sub(*started_at_ms)) / 3_600_000;
            let _ = writeln!(brief, "- [{id}] {prompt} ({age_hours}h ago)");
        }
    }
    if let Some((status, prompt)) = last_completed {
        let prompt: String = prompt.chars().take(90).collect();
        let _ = writeln!(brief, "Last completed episode: {status} — {prompt}");
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
    let mut brief = format!(
        "# Indexed reference data: {file_path}\nVerify current source before relying on these symbols and relationships.\n"
    );
    if !symbols.is_empty() {
        let listed: Vec<String> = symbols
            .iter()
            .take(20)
            .map(|(kind, label)| format!("{label} ({kind})"))
            .collect();
        let _ = writeln!(brief, "Defines: {}", listed.join(", "));
    }
    if !callers.is_empty() {
        let listed: Vec<String> = callers
            .iter()
            .take(12)
            .map(|(label, file)| format!("{label} [{file}]"))
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
                .cloned()
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
#[must_use]
pub fn detect_verification(command: &str) -> Option<&'static str> {
    let command = command.trim();
    // First meaningful token sequence; tolerate env-var prefixes.
    let normalized = command
        .split_whitespace()
        .skip_while(|token| token.contains('=') && !token.starts_with('/'))
        .collect::<Vec<_>>()
        .join(" ");
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
        return Some("test_result");
    }
    if matches_start(&[
        "cargo clippy",
        "cargo fmt",
        "eslint",
        "npx eslint",
        "ruff",
        "ruff check",
        "flake8",
        "golangci-lint",
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
        "pyright",
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
        "make",
        "make build",
        "./gradlew build",
        "swift build",
    ]) {
        return Some("build_result");
    }
    None
}

/// Pull an exit code out of the many shapes Claude Code tool results take.
#[must_use]
pub fn extract_exit_code(tool_result: &serde_json::Value) -> Option<i32> {
    for key in ["exitCode", "exit_code", "code", "returnCode", "returncode"] {
        if let Some(code) = tool_result.get(key).and_then(serde_json::Value::as_i64) {
            return i32::try_from(code).ok();
        }
    }
    match tool_result
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
    {
        Some(true) => Some(1),
        Some(false) => Some(0),
        None => None,
    }
}

/// Bounded artifact excerpt from a tool result.
#[must_use]
pub fn extract_artifact(tool_result: &serde_json::Value) -> String {
    for key in ["stdout", "output", "content", "stderr"] {
        if let Some(text) = tool_result.get(key).and_then(serde_json::Value::as_str) {
            if !text.trim().is_empty() {
                let mut excerpt = text.to_owned();
                excerpt.truncate(2_000);
                return excerpt;
            }
        }
    }
    String::new()
}

/// Conservative outcome for an auto-closed session: `success` only when
/// the last recorded verification passed; `abandoned` otherwise. User
/// acceptance stays unknown. Successful verification enables distillation;
/// ROI acceptance remains a separate explicit signal.
#[must_use]
pub fn auto_close_outcome(last_verification_exit: Option<i32>) -> &'static str {
    match last_verification_exit {
        Some(0) => "success",
        Some(_) => "failure",
        None => "abandoned",
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

fn is_engram_hook(command: &str) -> bool {
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
        assert_eq!(
            extract_exit_code(&serde_json::json!({"is_error": true})),
            Some(1)
        );
        assert_eq!(
            extract_exit_code(&serde_json::json!({"is_error": false})),
            Some(0)
        );
        assert_eq!(extract_exit_code(&serde_json::json!({"stdout": "x"})), None);
    }

    #[test]
    fn auto_close_is_conservative() {
        assert_eq!(auto_close_outcome(Some(0)), "success");
        assert_eq!(auto_close_outcome(Some(1)), "failure");
        assert_eq!(auto_close_outcome(None), "abandoned");
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
}
