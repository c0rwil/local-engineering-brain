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
//! daemon call the session marks the verification's result unknown in the
//! episodes it may belong to, found by the checkouts each episode records
//! when it opens; only a recorded observation replaces it, and the episode
//! closes from the latest result of each verification. Each record carries
//! content digests of the files the session touched or left dirty, which
//! ties it to the source it actually ran against. When the agent declared an
//! open Engram task through Engram's MCP server in the same session, evidence
//! goes to that task.
//!
//! Every hook fails OPEN: a broken brain must never break a coding session.

pub mod codex;
pub mod cursor;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{ProjectId, TaskId};

/// Evidence `metadata.source` written by the native hooks.
pub const CLAUDE_HOOK_SOURCE: &str = "claude-code-hook";
pub const CODEX_HOOK_SOURCE: &str = "codex-hook";
pub const CURSOR_HOOK_SOURCE: &str = "cursor-hook";
/// Evidence `metadata.source` of an operator-initiated re-verification
/// (`engram review reverify`): a recorded verification run again in a
/// disposable worktree at a commit, with digests of the files read there.
pub const REVERIFY_SOURCE: &str = "engram-reverify";
/// Labels whose source digests the judge accepts. Only the hook endpoint can
/// attach them; client metadata loses them.
const DIGEST_SOURCES: [&str; 4] = [
    CLAUDE_HOOK_SOURCE,
    CODEX_HOOK_SOURCE,
    CURSOR_HOOK_SOURCE,
    REVERIFY_SOURCE,
];
/// Verification output kept as the artifact. Runner summaries are at the end,
/// so longer output keeps its tail.
pub(crate) const VERIFICATION_EXCERPT_BYTES: usize = 4_096;
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
    /// First known checkout root of this episode's project, recorded when the
    /// episode is created (sidecars from earlier builds recorded it at the
    /// first resolved verification, or never).
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Further checkout roots of the project: each checkout a hook resolved
    /// for it in this session and the project's registered checkouts. With
    /// `root`, the daemon-free write-ahead finds the episode a verification
    /// belongs to by these. Bounded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkouts: Vec<PathBuf>,
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

/// Checkout roots kept per episode.
pub const EPISODE_CHECKOUTS_MAX: usize = 32;

impl HookSession {
    /// A new episode of `project_id` for one agent session.
    #[must_use]
    pub const fn new(task_id: TaskId, project_id: ProjectId, session_id: Option<String>) -> Self {
        Self {
            session_id,
            task_id,
            project_id,
            last_verification_exit: None,
            last_evidence_fingerprint: None,
            root: None,
            checkouts: Vec::new(),
            verification_results: Vec::new(),
        }
    }

    /// Every known checkout root of this episode's project.
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.root
            .iter()
            .chain(&self.checkouts)
            .map(PathBuf::as_path)
    }

    /// Whether the episode knows any checkout of its project.
    #[must_use]
    pub fn has_roots(&self) -> bool {
        self.root.is_some() || !self.checkouts.is_empty()
    }

    /// Remember a checkout root of this episode's project. Returns whether
    /// it was new and kept.
    pub fn note_checkout(&mut self, root: &Path) -> bool {
        if self.root.is_none() {
            self.root = Some(root.to_path_buf());
            return true;
        }
        if self.roots().any(|known| known == root) || self.checkouts.len() >= EPISODE_CHECKOUTS_MAX
        {
            return false;
        }
        self.checkouts.push(root.to_path_buf());
        true
    }

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
    // A host record is not a request: the previous request stays.
    if is_host_record(prompt) {
        return Ok(());
    }
    let path = session_state_path(state_dir, session_id).with_extension("pending");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::Config(e.to_string()))?;
    }
    // Command output never becomes task text. Redact before truncation so
    // the bound cannot split a credential.
    let prompt: String = redactor
        .redact_text(&prompt_intent(prompt))
        .chars()
        .take(8_000)
        .collect();
    write_private_sidecar(&path, prompt.as_bytes())
}

#[must_use]
pub fn pending_prompt(state_dir: &Path, session_id: &str) -> Option<String> {
    std::fs::read_to_string(session_state_path(state_dir, session_id).with_extension("pending"))
        .ok()
        // Earlier builds stored host records as prompts.
        .filter(|prompt| !is_host_record(prompt))
}

/// Blocks Claude Code submits as a prompt for a shell-mode turn
/// (`!command`), for the output of a local slash command, for a background
/// task's completion notice, and for a subagent's report.
const HOST_RECORD_TAGS: [&str; 8] = [
    "bash-input",
    "bash-stdout",
    "bash-stderr",
    "local-command-stdout",
    "local-command-stderr",
    "local-command-caveat",
    "task-notification",
    "agent-message",
];

/// One part of a prompt: text the user typed, or a host record block.
#[derive(Debug, PartialEq, Eq)]
enum PromptSegment<'a> {
    Text(&'a str),
    Block { tag: &'static str, inner: &'a str },
}

/// The host record tag whose opening tag (`<tag>` or `<tag attr="…">`)
/// starts `text`.
fn host_tag_opening(text: &str) -> Option<&'static str> {
    let after = text.strip_prefix('<')?;
    HOST_RECORD_TAGS.iter().copied().find(|tag| {
        after
            .strip_prefix(tag)
            .is_some_and(|rest| rest.starts_with('>') || rest.starts_with([' ', '\t']))
    })
}

fn next_host_opening(text: &str) -> Option<usize> {
    text.match_indices('<')
        .map(|(index, _)| index)
        .find(|index| host_tag_opening(&text[*index..]).is_some())
}

/// The host block starting `text`: its tag, inner text and byte length. It
/// ends at the first closing tag followed by the end of the prompt or another
/// block, else at the last closing tag, so tags quoted inside command output
/// do not end it early.
fn host_block(text: &str) -> Option<(&'static str, &str, usize)> {
    let tag = host_tag_opening(text)?;
    let opening_end = text.find('>')? + 1;
    if text[..opening_end].contains('\n') {
        return None;
    }
    let close = format!("</{tag}>");
    let body = &text[opening_end..];
    let ends = body
        .match_indices(close.as_str())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let end = ends
        .iter()
        .copied()
        .find(|index| {
            let after = body[index + close.len()..].trim_start();
            after.is_empty() || host_tag_opening(after).is_some()
        })
        .or_else(|| ends.last().copied())?;
    Some((tag, &body[..end], opening_end + end + close.len()))
}

/// Split a prompt into typed text and host record blocks. An unterminated
/// block and everything after it count as typed text.
fn prompt_segments(prompt: &str) -> Vec<PromptSegment<'_>> {
    let mut segments = Vec::new();
    let mut rest = prompt;
    loop {
        let Some(index) = next_host_opening(rest) else {
            if !rest.trim().is_empty() {
                segments.push(PromptSegment::Text(rest));
            }
            return segments;
        };
        if !rest[..index].trim().is_empty() {
            segments.push(PromptSegment::Text(&rest[..index]));
        }
        let Some((tag, inner, length)) = host_block(&rest[index..]) else {
            segments.push(PromptSegment::Text(&rest[index..]));
            return segments;
        };
        segments.push(PromptSegment::Block { tag, inner });
        rest = &rest[index + length..];
    }
}

/// A prompt made only of host record blocks reports a command the user ran,
/// its output, a finished background task or a subagent's report. It is not
/// a task request: retrieval keyed on it fetches whatever the record happens
/// to mention, and command output can hold secrets. Any typed text makes it
/// an ordinary prompt.
#[must_use]
pub fn is_host_record(prompt: &str) -> bool {
    let segments = prompt_segments(prompt);
    !segments.is_empty()
        && segments
            .iter()
            .all(|segment| matches!(segment, PromptSegment::Block { .. }))
}

/// A prompt as task intent: typed text and the commands the user ran stay;
/// command output, notices and reports are dropped.
#[must_use]
pub fn prompt_intent(prompt: &str) -> String {
    let segments = prompt_segments(prompt);
    if !segments
        .iter()
        .any(|segment| matches!(segment, PromptSegment::Block { .. }))
    {
        return prompt.to_owned();
    }
    segments
        .iter()
        .filter_map(|segment| match segment {
            PromptSegment::Text(text) => Some(text.trim().to_owned()),
            PromptSegment::Block {
                tag: "bash-input",
                inner,
            } => Some(format!("$ {}", inner.trim())),
            PromptSegment::Block { .. } => None,
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Read shell text from a tool input: `command` (Claude Code, Cursor, and
/// Codex hooks, which report `exec_command` as `Bash` with its `cmd` moved
/// to `command`) or `cmd` (the raw `exec_command` argument). Only explicit
/// string fields count: a code-mode `exec` cell's JavaScript is never read
/// as shell text (see [`is_shell_tool`]).
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

/// Claim one tool call's verification for this hook process. Claude Code runs
/// user- and project-level hooks concurrently, so two processes can handle the
/// same call; exactly one records its verification and writes its result.
/// Calls without an id cannot be told apart and are always handled. Claim
/// files age out with the session's other hook files.
#[must_use]
pub fn claim_verification(state_dir: &Path, session_id: &str, payload: &serde_json::Value) -> bool {
    let Some(id) = payload
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return true;
    };
    let digest = crate::experience::sha256(format!("tool:{id}").as_bytes());
    let digest = digest.get(..32).unwrap_or(&digest);
    claim_once(state_dir, session_id, &format!("verification-{digest}"))
}

/// Remove a session's claim files (session end).
pub fn clear_claims(state_dir: &Path, session_id: &str) {
    for tag in ["brief", "prompt", "episode", "context"] {
        release_claim(state_dir, session_id, tag);
    }
    // Briefs after compaction or `/clear` carry claims of their own.
    let Some(prefix) = session_state_path(state_dir, session_id)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| format!("{stem}.brief-"))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(state_dir.join("hook-sessions")) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".claim") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The claim for briefing a restarted conversation once. The user- and
/// project-level hooks handling one compaction see the same transcript, so
/// its length names the event; without a readable transcript the minute does.
#[must_use]
pub fn restart_claim(source: &str, payload: &serde_json::Value, now_ms: i64) -> String {
    payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .and_then(|path| std::fs::metadata(path).ok())
        .map_or_else(
            || format!("brief-{source}-m{}", now_ms / 60_000),
            |metadata| format!("brief-{source}-t{}", metadata.len()),
        )
}

/// `compact` or `clear` when a session start follows Claude Code compacting
/// or clearing the conversation, which drops everything hooks delivered.
#[must_use]
pub fn conversation_restart(payload: &serde_json::Value) -> Option<&'static str> {
    match payload.get("source").and_then(serde_json::Value::as_str) {
        Some("compact") => Some("compact"),
        Some("clear") => Some("clear"),
        _ => None,
    }
}

/// Let a compacted or cleared conversation receive its context again:
/// delivery receipts keep their attribution IDs but no longer suppress an
/// identical pack, and file briefs repeat. Subagent threads keep theirs,
/// because compaction leaves their conversations alone.
pub fn forget_deliveries(state_dir: &Path, session_id: &str) {
    // Every main-conversation scope of the session, with or without an
    // episode yet: a prompt can deliver context before the first tool call.
    let prefix = format!(
        "{}-",
        &crate::experience::sha256(session_id.as_bytes())[..24]
    );
    let mut scopes = project_sessions(state_dir, session_id)
        .into_iter()
        .map(|(scope, _)| scope)
        .collect::<std::collections::BTreeSet<_>>();
    if let Ok(entries) = std::fs::read_dir(state_dir.join("hook-sessions")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name
                .strip_suffix(".context-delivery")
                .or_else(|| name.strip_suffix(".files"))
            else {
                continue;
            };
            // Thread scopes add a suffix to the project id and are skipped.
            if stem
                .strip_prefix(&prefix)
                .is_some_and(|project| ProjectId::parse(project).is_ok())
            {
                scopes.insert(stem.to_owned());
            }
        }
    }
    for scope in scopes {
        if let Some(mut delivery) = context_delivery(state_dir, &scope) {
            delivery.delivery_digest = None;
            if let Ok(bytes) = serde_json::to_vec(&delivery) {
                let _ = write_private_sidecar(
                    &session_state_path(state_dir, &scope).with_extension("context-delivery"),
                    &bytes,
                );
            }
        }
        let _ = std::fs::remove_file(session_files_path(state_dir, &scope));
    }
}

/// Release a claimed action that could not be delivered, allowing a later retry.
pub fn release_claim(state_dir: &Path, session_id: &str, tag: &str) {
    let path = session_state_path(state_dir, session_id).with_extension(format!("{tag}.claim"));
    let _ = std::fs::remove_file(path);
}

/// Sidecar tracking which files a conversation has been briefed on, and for
/// which task, so pre-tool shape injection fires once per file per task.
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

/// Files one conversation keeps brief records for; past this, no file is
/// briefed for the first time.
const MAX_BRIEFED_FILES: usize = 200;

/// What a conversation was last briefed about one file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct BriefRecord {
    /// Digest of the task the file was last considered for.
    task: String,
    /// Source identity of the brief last delivered (see [`brief_sources`]).
    sources: String,
}

/// Claim delivery of `brief` for the task `task_text` (the session's
/// pending prompt). A file is briefed at most once per task: the first time,
/// then once more after the task changes, and only if the brief now lists
/// other sources than the one last delivered. A brief that differs only in
/// order, line spans or size is not repeated. Returns false to skip; the
/// claim is recorded before the brief is emitted.
#[must_use]
pub fn claim_file_brief(
    state_dir: &Path,
    scope: &str,
    brief: &FileBrief<'_>,
    task_text: &str,
) -> bool {
    let path = session_files_path(state_dir, scope);
    let task = crate::experience::sha256(task_text.as_bytes());
    let sources = brief_sources(brief);
    let mut records = read_brief_records(&path, &task);
    let full = records.len() >= MAX_BRIEFED_FILES;
    let deliver = match records.get_mut(brief.file_path) {
        Some(record) if record.task == task => false,
        Some(record) => {
            // A new task: considered once, whether or not it re-briefs.
            record.task.clone_from(&task);
            let changed = record.sources != sources;
            if changed {
                record.sources = sources;
            }
            changed
        }
        // Hard cap: stop briefing new files rather than growing unbounded.
        None if full => return false,
        None => {
            records.insert(brief.file_path.to_owned(), BriefRecord { task, sources });
            true
        }
    };
    let recorded = serde_json::to_vec(&records)
        .ok()
        .is_some_and(|bytes| write_private_sidecar(&path, &bytes).is_ok());
    deliver && recorded
}

fn read_brief_records(path: &Path, task: &str) -> BTreeMap<String, BriefRecord> {
    let Ok(bytes) = std::fs::read(path) else {
        return BTreeMap::new();
    };
    if let Ok(records) = serde_json::from_slice(&bytes) {
        return records;
    }
    // Older hooks listed one briefed path per line. Those briefs count as
    // delivered for the current task, so only a later task can re-brief.
    String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| !line.is_empty())
        .take(MAX_BRIEFED_FILES)
        .map(|line| {
            (
                line.to_owned(),
                BriefRecord {
                    task: task.to_owned(),
                    sources: String::new(),
                },
            )
        })
        .collect()
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

/// One definition in a file brief.
#[derive(Clone, Debug, PartialEq)]
pub struct BriefSymbol {
    pub kind: String,
    pub label: String,
    /// Lines, when checked against the checkout like task source map ranges.
    pub lines: Option<crate::world::LineRange>,
    /// Listed ahead of file order because the task selects, names or
    /// matches it.
    pub task_match: bool,
}

/// What the graph knows about a file a tool is about to touch.
#[derive(Clone, Copy, Debug)]
pub struct FileBrief<'a> {
    pub file_path: &'a str,
    /// Definitions, task matches first and the rest in file order.
    pub symbols: &'a [BriefSymbol],
    /// Label and file of each definition elsewhere that calls into the file.
    pub callers: &'a [(String, String)],
    pub imports: &'a [String],
    /// Size of the checkout's copy of the file, when known.
    pub size_bytes: Option<u64>,
}

/// Definitions, callers and imports listed in one file brief.
const BRIEF_SYMBOLS: usize = 20;
const BRIEF_CALLERS: usize = 12;
const BRIEF_IMPORTS: usize = 12;

/// Source identity of a brief: the definitions, callers and imports it
/// lists, ignoring their order, line spans and the size line, like the
/// delivery signature of a task source map.
#[must_use]
pub fn brief_sources(brief: &FileBrief<'_>) -> String {
    let mut symbols: Vec<(&str, &str)> = brief
        .symbols
        .iter()
        .take(BRIEF_SYMBOLS)
        .map(|symbol| (symbol.kind.as_str(), symbol.label.as_str()))
        .collect();
    symbols.sort_unstable();
    let mut callers: Vec<&(String, String)> = brief.callers.iter().take(BRIEF_CALLERS).collect();
    callers.sort_unstable();
    let mut imports: Vec<&String> = brief.imports.iter().take(BRIEF_IMPORTS).collect();
    imports.sort_unstable();
    crate::experience::sha256(
        &serde_json::to_vec(&(brief.file_path, symbols, callers, imports)).unwrap_or_default(),
    )
}

/// Render a pre-tool file-shape brief of at most `byte_cap` bytes: with the
/// checked line spans when that form fits, otherwise without them. A file of
/// at least `large_file_bytes` (when nonzero) gets one line stating its size.
/// `None` when the graph knows nothing about the file (inject silence, not
/// filler) or when no form fits.
#[must_use]
pub fn render_file_shape(
    brief: &FileBrief<'_>,
    large_file_bytes: u64,
    byte_cap: usize,
) -> Option<String> {
    if brief.symbols.is_empty() && brief.callers.is_empty() {
        return None;
    }
    [true, false]
        .into_iter()
        .map(|spans| file_brief_text(brief, spans, large_file_bytes))
        .find(|text| text.len() <= byte_cap)
}

fn file_brief_text(brief: &FileBrief<'_>, spans: bool, large_file_bytes: u64) -> String {
    // Labels and imports come from repository files: keep each on one line
    // with hidden characters and framing markup escaped.
    let inline = crate::security::neutralize_inline;
    let symbols = &brief.symbols[..brief.symbols.len().min(BRIEF_SYMBOLS)];
    let spans = spans && symbols.iter().any(|symbol| symbol.lines.is_some());
    let mut text = format!(
        "# Indexed reference data: {}\nVerify current source before relying on these symbols and relationships.\n",
        inline(brief.file_path)
    );
    if !symbols.is_empty() {
        let listed: Vec<String> = symbols
            .iter()
            .map(|symbol| {
                let (label, kind) = (inline(&symbol.label), inline(&symbol.kind));
                match symbol.lines.filter(|_| spans) {
                    Some(lines) => format!("{label} ({kind}) {lines}"),
                    None => format!("{label} ({kind})"),
                }
            })
            .collect();
        let qualifiers: Vec<&str> = [
            spans.then_some("line spans"),
            symbols
                .iter()
                .any(|symbol| symbol.task_match)
                .then_some("task matches first"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let heading = if qualifiers.is_empty() {
            "Defines".to_owned()
        } else {
            format!("Defines ({})", qualifiers.join(", "))
        };
        let _ = writeln!(text, "{heading}: {}", listed.join(", "));
    }
    if let Some(size) = brief
        .size_bytes
        .filter(|size| large_file_bytes > 0 && *size >= large_file_bytes)
    {
        let located = if spans {
            " The line spans locate the listed definitions."
        } else {
            ""
        };
        let _ = writeln!(text, "File size: {} KB.{located}", size.div_ceil(1024));
    }
    if !brief.callers.is_empty() {
        let listed: Vec<String> = brief
            .callers
            .iter()
            .take(BRIEF_CALLERS)
            .map(|(label, file)| format!("{} [{}]", inline(label), inline(file)))
            .collect();
        let _ = writeln!(text, "Called from elsewhere by: {}", listed.join(", "));
    }
    if !brief.imports.is_empty() {
        let _ = writeln!(
            text,
            "Imports: {}",
            brief
                .imports
                .iter()
                .take(BRIEF_IMPORTS)
                .map(|import| inline(import))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    text
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

/// The evidence kind of a `make` run, from its first target, when every target
/// it names is a verification target (`test`, `lint`, `check`, `build`).
/// Options that take a separate value (`-j 4`, `-f file`) skip that value, and
/// `NAME=value` overrides are not targets.
fn make_verification_kind(arguments: &[&str]) -> Option<&'static str> {
    const VALUE_OPTIONS: &[&str] = &[
        "-j",
        "-l",
        "-f",
        "-C",
        "-I",
        "-o",
        "-W",
        "--jobs",
        "--load-average",
        "--file",
        "--makefile",
        "--directory",
        "--include-dir",
        "--old-file",
        "--assume-old",
        "--new-file",
        "--assume-new",
        "--what-if",
    ];
    let mut targets = Vec::new();
    let mut skip_next = false;
    for argument in arguments {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        if matches!(*argument, "|" | "||" | "&&" | ";" | "&") {
            break;
        }
        let redirection = argument.trim_start_matches(|c: char| c.is_ascii_digit());
        if redirection.starts_with(['<', '>']) {
            // A bare operator (`>`, `2>`, `>>`) names its target in the next word.
            skip_next = redirection.chars().all(|c| matches!(c, '<' | '>'));
        } else if VALUE_OPTIONS.contains(argument) {
            skip_next = true;
        } else if !argument.starts_with('-') && !argument.contains('=') {
            targets.push(*argument);
        }
    }
    let kinds = targets
        .iter()
        .map(|target| match *target {
            "test" => Some("test_result"),
            "lint" => Some("lint_result"),
            "check" => Some("typecheck_result"),
            "build" => Some("build_result"),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    kinds.first().copied()
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
/// verifying them; neither is verification. `make` is verification only when
/// every target it is given is `test`, `lint`, `check` or `build`: a bare
/// `make` builds the default target, and `make test deploy` also deploys.
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
    if tokens.first() == Some(&"make") {
        return make_verification_kind(&tokens[1..]);
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
    /// Why the status is hidden; `None` exactly when `status_bearing`.
    pub hidden: Option<StatusHidden>,
}

/// Why a recognized verification's exit status cannot be recorded, in the
/// order the hook reports it.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusHidden {
    /// Shell syntax the hook does not model: subshells, groups,
    /// here-documents, backquotes or unbalanced quotes.
    Syntax,
    /// A background job (`&`) or a tool call run in the background.
    Background,
    /// `||` replaces a failure with another command's status.
    Or,
    /// A command after the first verification, joined by `;` or a newline.
    Sequence,
    /// A pipe after the verification without an unconditional `pipefail`.
    Pipe,
    /// A directory change that may not take effect before the verification.
    Directory,
    /// Not the shell: the coding client reported no exit status for the run
    /// (a Codex command without a rollout record, a failure without an exit
    /// code, an interrupted call). The ledger counts it; hooks note only
    /// shell causes, which the agent can change.
    Unreported,
}

impl StatusHidden {
    /// Stable label for claims, the ledger and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Background => "background",
            Self::Or => "or",
            Self::Sequence => "sequence",
            Self::Pipe => "pipe",
            Self::Directory => "directory",
            Self::Unreported => "unreported",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShellOperator {
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
            hidden: Some(StatusHidden::Syntax),
        });
    };
    let verifications: Vec<(usize, &'static str)> = commands
        .iter()
        .enumerate()
        .filter_map(|(index, (text, _))| detect_verification(text).map(|kind| (index, kind)))
        .collect();
    let (first, _) = *verifications.first()?;
    let (_, kind) = *verifications.last()?;
    let hidden = hidden_status(&commands, &verifications).or_else(|| {
        workdir_changes(&commands, first)
            .is_none()
            .then_some(StatusHidden::Directory)
    });
    Some(ShellVerification {
        kind,
        tests: verifications.iter().any(|(_, kind)| *kind == "test_result"),
        status_bearing: hidden.is_none(),
        hidden,
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

/// The first reason why some verification's own exit status does not
/// determine the shell's, or `None` when every one does.
fn hidden_status(
    commands: &ShellCommands,
    verifications: &[(usize, &str)],
) -> Option<StatusHidden> {
    use ShellOperator::{And, Background, Or, Pipe};
    let Some(&(first, _)) = verifications.first() else {
        return Some(StatusHidden::Syntax);
    };
    let any = |wanted: ShellOperator| {
        commands
            .iter()
            .any(|(_, operator)| *operator == Some(wanted))
    };
    if any(Background) {
        Some(StatusHidden::Background)
    } else if any(Or) {
        Some(StatusHidden::Or)
    } else if !commands[pipeline_end(commands, first)..]
        .iter()
        .all(|(_, operator)| matches!(operator, None | Some(And | Pipe)))
    {
        Some(StatusHidden::Sequence)
    } else if !verifications.iter().all(|&(index, _)| {
        pipeline_end(commands, index) == index
            || pipefail_before(commands, pipeline_start(commands, index))
    }) {
        Some(StatusHidden::Pipe)
    } else {
        None
    }
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
pub(crate) fn pipefail_setting(command: &str) -> Option<bool> {
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

/// A directory known to contain the one every verification in the command
/// runs in, when [`shell_workdir`] cannot say which: `base`, if every
/// directory change in the command is a static, relative descent. The shell
/// then stays below `base` whether or not each change takes effect (for
/// example `cd src; cargo test`). `None` for an absolute, home or parent
/// target, a target that resolves outside `base` through a symbolic link,
/// a change this reader cannot model, or shell syntax it cannot read.
#[must_use]
pub fn shell_workdir_bound(command: &str, base: &Path) -> Option<PathBuf> {
    if let Some(exact) = shell_workdir(command, base) {
        return Some(exact);
    }
    let canonical_base = base.canonicalize().ok();
    for (text, _) in shell_commands(command)? {
        match directory_change(&text) {
            DirectoryChange::Unchanged => {}
            DirectoryChange::Unmodeled => return None,
            DirectoryChange::To(target) => {
                let relative = Path::new(&target);
                if target.starts_with('~')
                    || !relative
                        .components()
                        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
                {
                    return None;
                }
                // A symbolic link can lead out of `base`.
                if let (Some(base), Ok(resolved)) =
                    (&canonical_base, base.join(relative).canonicalize())
                {
                    if !resolved.starts_with(base) {
                        return None;
                    }
                }
            }
        }
    }
    Some(base.to_path_buf())
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
pub(crate) enum DirectoryChange {
    /// The command leaves the directory alone.
    Unchanged,
    /// `cd`/`pushd` to a static target.
    To(String),
    /// A change this reader cannot model: `cd -`, a bare `cd`, expansions,
    /// `popd`, `source` of anything but a virtualenv `activate`, `eval`, `exec`.
    Unmodeled,
}

pub(crate) fn directory_change(command: &str) -> DirectoryChange {
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
    /// Evidence kind of the last recognized verification.
    pub kind: &'static str,
    /// Whether the command runs tests.
    pub tests: bool,
    /// The modeled directory the verifications run in.
    pub workdir: Option<PathBuf>,
    /// A directory known to contain the one the verifications run in:
    /// `workdir` when modeled, else a bound from [`shell_workdir_bound`].
    /// `None` when even that is unknown.
    pub within: Option<PathBuf>,
    /// Why the command hides the exit status, including a tool call the agent
    /// ran in the background; `None` when the status can be observed.
    pub hidden: Option<StatusHidden>,
}

/// Whether the tool runs shell text that can carry a verification.
///
/// Codex (0.155) reports every shell command as `Bash` with
/// `{"command": ...}`, whether the model called `exec_command` directly or
/// from a code-mode `exec` cell through `tools.exec_command(...)`. The cell
/// itself is not a shell tool: its input is JavaScript, Codex runs no hook
/// for it or for code mode's `wait`, and each command it runs arrives as its
/// own `Bash` call. Reading the cell as shell would misparse JavaScript and
/// count those commands twice, so `exec` and `wait` are not listed here.
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
    let base = tool_base_dir(cwd, input);
    let workdir = shell_workdir(command, &base).map(|directory| normalize_tool_path(&directory));
    let within = workdir.clone().or_else(|| {
        shell_workdir_bound(command, &base).map(|directory| normalize_tool_path(&directory))
    });
    // Claude Code's `run_in_background` returns before the command exits.
    let backgrounded = input
        .get("run_in_background")
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    Some(PendingVerification {
        key: verification_key(command, workdir.as_deref()),
        kind: verification.kind,
        tests: verification.tests,
        workdir,
        within,
        hidden: verification
            .hidden
            .or_else(|| backgrounded.then_some(StatusHidden::Background)),
    })
}

/// Write-ahead without the daemon: record a pending verification's result as
/// unknown in the episodes it may belong to, before recording can fail. See
/// [`write_ahead_targets`] for which episodes those are.
pub fn mark_verification_unknown(
    state_dir: &Path,
    session_id: &str,
    pending: &PendingVerification,
) {
    let sessions = project_sessions(state_dir, session_id);
    let targets = write_ahead_targets(&sessions, pending);
    for (index, (scope, mut session)) in sessions.into_iter().enumerate() {
        if !targets.contains(&index) {
            continue;
        }
        session.set_verification_result(&pending.key, pending.tests, None);
        session.last_verification_exit = None;
        let _ = save_session(state_dir, &scope, &session);
    }
}

/// The episodes (indexes into `sessions`) a pending verification may belong
/// to, by the checkout roots each episode knows:
///
/// - A modeled directory belongs to the episodes whose root most
///   specifically contains it. Outside every known root, it belongs to a
///   repository with no episode in this session, so no episode is marked,
///   provided every episode knows its checkouts.
/// - An unmodeled directory with a bound (see [`shell_workdir_bound`])
///   belongs to the episodes whose root most specifically contains the
///   bound, and to episodes whose roots lie below it.
/// - Otherwise, a directory that is truly unknown, outside every known root
///   and unmodeled, or a session with an episode whose checkouts are
///   unknown, marks every episode.
#[must_use]
pub fn write_ahead_targets(
    sessions: &[(String, HookSession)],
    pending: &PendingVerification,
) -> Vec<usize> {
    let every = || (0..sessions.len()).collect();
    let Some(location) = pending.workdir.as_deref().or(pending.within.as_deref()) else {
        return every();
    };
    let modeled = pending.workdir.is_some();
    let depth = |session: &HookSession| {
        session
            .roots()
            .filter(|root| location.starts_with(root))
            .map(|root| root.components().count())
            .max()
    };
    let deepest = sessions
        .iter()
        .filter_map(|(_, session)| depth(session))
        .max();
    let mut targets: Vec<usize> = (0..sessions.len())
        .filter(|&index| {
            let session = &sessions[index].1;
            (deepest.is_some() && depth(session) == deepest)
                // A bounded run may be anywhere below the bound, including
                // a nested checkout.
                || (!modeled
                    && session
                        .roots()
                        .any(|root| root != location && root.starts_with(location)))
        })
        .collect();
    if targets.is_empty() && !(modeled && sessions.iter().all(|(_, session)| session.has_roots())) {
        targets = every();
    }
    targets
}

/// Write-ahead for a run whose directory is unknown: every earlier result of
/// the same verification commands, under any directory, becomes unknown in
/// each episode of the session.
pub fn mark_verification_unknown_anywhere(
    state_dir: &Path,
    session_id: &str,
    pending: &PendingVerification,
) {
    // A key is the directory, a newline, then the normalized commands.
    fn commands(key: &str) -> Option<&str> {
        key.split_once('\n').map(|(_, commands)| commands)
    }
    let wanted = commands(&pending.key);
    for (scope, mut session) in project_sessions(state_dir, session_id) {
        let mut changed = false;
        for result in &mut session.verification_results {
            if result.exit_code.is_some() && commands(&result.key) == wanted {
                result.exit_code = None;
                changed = true;
            }
        }
        if changed {
            session.last_verification_exit = None;
            let _ = save_session(state_dir, &scope, &session);
        }
    }
}

/// Longest status note, well inside Codex's per-message and Cursor's context
/// limits.
const STATUS_NOTE_BYTES: usize = 900;
// A Cursor note shares one response with relay messages and repository
// context; together they stay under the 10,000 characters Cursor accepts.
const _: () = assert!(cursor::RELAY_BYTES + cursor::CONTEXT_BYTES + STATUS_NOTE_BYTES < 10_000);
/// Longest command a note quotes, in bytes; longer ones get the general
/// advice only, so a note stays well under [`STATUS_NOTE_BYTES`].
const STATUS_NOTE_COMMAND_BYTES: usize = 240;
/// Filters that read all of their input and exit 0, so with `pipefail` a
/// zero status still proves the verification passed and a failure is the
/// verification's. `head` can stop the producer early and `grep` fails when
/// nothing matches, so those are replaced in a suggested form.
const SAFE_FILTERS: &[&str] = &[
    "tail", "tee", "cat", "sed", "awk", "cut", "tr", "sort", "uniq", "wc",
];

/// Whether this hook may coach the agent: never in brain-off mode
/// (`ENGRAM_MODE=off`, as `engram eval run` and the launchers set it).
#[must_use]
pub fn status_notes_enabled() -> bool {
    !std::env::var("ENGRAM_MODE").is_ok_and(|mode| mode.trim().eq_ignore_ascii_case("off"))
}

/// Claim the session's single note for `reason`; `false` when it was given.
#[must_use]
pub fn claim_status_note(state_dir: &Path, session_id: &str, reason: StatusHidden) -> bool {
    claim_once(state_dir, session_id, &status_note_tag(reason))
}

/// Release a claimed note that could not be delivered, so it can be retried.
pub fn release_status_note(state_dir: &Path, session_id: &str, reason: StatusHidden) {
    release_claim(state_dir, session_id, &status_note_tag(reason));
}

fn status_note_tag(reason: StatusHidden) -> String {
    format!("status-note-{}", reason.as_str())
}

/// One short reference-data paragraph telling the agent why Engram could not
/// record this verification's result and how to run the same command so it
/// can: the recordable form when this reader can build and check one.
#[must_use]
pub fn status_note(command: &str, reason: StatusHidden) -> String {
    let status = "its exit status";
    let (hidden, cause) = match reason {
        StatusHidden::Syntax => (
            status,
            "Engram does not read subshells, groups, here-documents or backquotes",
        ),
        StatusHidden::Background => (
            status,
            "it ran in the background, so no exit status came back",
        ),
        StatusHidden::Or => (
            status,
            "`||` replaces a failure with another command's exit status",
        ),
        StatusHidden::Sequence => (
            status,
            "a command after it, joined by `;` or a newline, sets the exit status",
        ),
        StatusHidden::Pipe => (
            status,
            "a pipe reports its last command's exit status, not the verification's",
        ),
        StatusHidden::Directory => (
            "the repository it ran in",
            "a directory change before it may not take effect",
        ),
        StatusHidden::Unreported => (status, "the coding client reported none for it"),
    };
    // Running the command again changes nothing the client reports.
    let advice = match (reason != StatusHidden::Unreported)
        .then(|| recordable_form(command))
        .flatten()
        .filter(|(form, _)| form.len() <= STATUS_NOTE_COMMAND_BYTES)
    {
        Some((form, replaced)) => {
            let foreground = if reason == StatusHidden::Background {
                " in the foreground"
            } else {
                ""
            };
            let filters = if replaced {
                " (`head` can stop the run early and `grep` fails when nothing matches)"
            } else {
                ""
            };
            format!("run `{form}`{foreground}{filters}")
        }
        None => match reason {
            StatusHidden::Pipe => "start the command with `set -o pipefail;` or drop the pipe",
            StatusHidden::Sequence => {
                "run the verification on its own, or join the commands with `&&`"
            }
            StatusHidden::Or => "run it without the `||` part",
            StatusHidden::Background => "run it in the foreground",
            StatusHidden::Directory => "put `cd <path> &&` with a fixed path right before it",
            StatusHidden::Syntax => "run the verification as a plain command",
            StatusHidden::Unreported => {
                "record the result with Engram's `engram_record_evidence` tool"
            }
        }
        .to_owned(),
    };
    let mut note = format!(
        "# Engram verification note (reference data)\nEngram did not record this \
         verification's result because {hidden} was hidden: {cause}. To record it, {advice}. \
         This note appears once per session for this cause.\n"
    );
    if note.len() > STATUS_NOTE_BYTES {
        let mut end = STATUS_NOTE_BYTES;
        while !note.is_char_boundary(end) {
            end -= 1;
        }
        note.truncate(end);
    }
    note
}

/// The same command in a recordable shape, checked by this reader: setup
/// before the first verification and the verifications joined by `&&`,
/// anything after the last verification's pipeline dropped, unsafe filters
/// replaced by `tail -n 40`, and `set -o pipefail;` first when a
/// verification's output is still piped. The flag says filters were replaced.
fn recordable_form(command: &str) -> Option<(String, bool)> {
    let commands = shell_commands(command)?;
    let positions: Vec<usize> = (0..commands.len())
        .filter(|&index| detect_verification(&commands[index].0).is_some())
        .collect();
    let first_start = pipeline_start(&commands, *positions.first()?);
    let last_end = pipeline_end(&commands, *positions.last()?);
    let joined = |members: &[(String, Option<ShellOperator>)]| {
        members
            .iter()
            .map(|(text, _)| text.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let mut pipelines = Vec::new();
    let (mut piped, mut replaced) = (false, false);
    let mut start = 0;
    while start <= last_end {
        let end = pipeline_end(&commands, start);
        let members = &commands[start..=end];
        // Any earlier pipefail setting is replaced by one that counts.
        let setting =
            start < first_start && members.len() == 1 && pipefail_setting(&members[0].0).is_some();
        if !setting {
            match (start..=end).rev().find(|index| positions.contains(index)) {
                Some(verification) if verification < end => {
                    let (producer, filters) = members.split_at(verification - start + 1);
                    let safe = filters.iter().all(|(filter, _)| {
                        filter
                            .split_whitespace()
                            .next()
                            .is_some_and(|name| SAFE_FILTERS.contains(&name))
                    });
                    let filters = if safe {
                        joined(filters)
                    } else {
                        replaced = true;
                        "tail -n 40".to_owned()
                    };
                    pipelines.push(format!("{} | {filters}", joined(producer)));
                    piped = true;
                }
                _ => pipelines.push(joined(members)),
            }
        }
        start = end + 1;
    }
    let form = pipelines.join(" && ");
    let form = if piped {
        format!("set -o pipefail; {form}")
    } else {
        form
    };
    classify_verification(&form)
        .is_some_and(|verification| verification.status_bearing)
        .then_some((form, replaced))
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
pub(crate) fn shell_commands(text: &str) -> Option<Vec<(String, Option<ShellOperator>)>> {
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
/// have no known status. Neither does a Codex (0.155) shell payload: its
/// `PostToolUse` `tool_response` is the command's output text, and Codex
/// runs the hook for failing commands too. Its status comes from the
/// rollout instead (see [`codex::command_record`]).
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
pub(crate) fn bounded_tail(text: &str, limit: usize) -> String {
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

/// Source digests a native hook, or an operator-initiated re-verification,
/// recorded beside its verification. Evidence reported by agents, the CLI or
/// completions never qualifies.
#[must_use]
pub fn hook_source_digests(metadata: &serde_json::Value) -> Option<BTreeMap<String, String>> {
    if !metadata
        .get("source")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|source| DIGEST_SOURCES.contains(&source))
    {
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

/// Whether evidence came from an operator-initiated re-verification. Such a
/// run tested a commit in a disposable worktree, not the live checkout, so
/// only its recorded digests say which bytes it observed.
#[must_use]
pub fn is_reverification(metadata: &serde_json::Value) -> bool {
    metadata.get("source").and_then(serde_json::Value::as_str) == Some(REVERIFY_SOURCE)
}

/// Metadata as stored. Client-supplied metadata loses the reserved keys (a
/// `source` ending in `-hook` or equal to [`REVERIFY_SOURCE`], and
/// `source_digests`); hook-endpoint `provenance` with a known label adds them
/// back. This prevents accidental forgery by the CLI, MCP or other clients.
/// It is not a security boundary: any process of the same user can reach the
/// hook endpoint.
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
            .is_some_and(|source| source.ends_with("-hook") || source == REVERIFY_SOURCE)
        {
            fields.remove("source");
        }
        fields.remove("source_digests");
        fields.remove("source_digests_truncated");
    }
    let Some(provenance) =
        provenance.filter(|provenance| DIGEST_SOURCES.contains(&provenance.source.as_str()))
    else {
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
/// `reported` is a status the client reported outside the payload, with how
/// (`codex_rollout`); the payload's own status takes precedence.
#[must_use]
pub fn observe_verification(
    payload: &serde_json::Value,
    source: &'static str,
    root: &Path,
    git: Option<&crate::projects::GitState>,
    session_paths: &[String],
    reported: Option<(i32, &'static str)>,
) -> Option<ObservedVerification> {
    let input = payload
        .get("tool_input")
        .unwrap_or(&serde_json::Value::Null);
    let command = tool_command(input);
    let verification = classify_verification(command)?;
    if !verification.status_bearing {
        return None;
    }
    let (exit_code, exit_status) =
        observed_exit_status(payload, source == CLAUDE_HOOK_SOURCE).or(reported)?;
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
/// never stands in for one. Text never carries one either: a string result
/// is the command's own output, which can print anything.
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

    /// Codex code mode (0.155): the `exec` cell's input is JavaScript and no
    /// hook runs for it; were a build to report it, the cell is still not
    /// shell text, whichever field carries the source.
    #[test]
    fn codex_code_mode_cells_are_not_shell_text() {
        use serde_json::json;
        let cell = "const r = await tools.exec_command({ cmd: \"cargo test\" });\ntext(r.output);";
        for input in [
            json!(cell),
            json!({"command": cell}),
            json!({"input": cell}),
            json!({"code": cell}),
        ] {
            for tool in ["exec", "wait"] {
                let payload = json!({"hook_event_name":"PostToolUse","tool_name":tool,
                    "tool_use_id":"cell-1","tool_input":input,"tool_response":"test result: ok"});
                assert!(!is_shell_tool(&payload), "{tool}: {input}");
                assert!(
                    pending_verification(&payload, Path::new("/work")).is_none(),
                    "{tool}: {input}"
                );
            }
        }
        assert_eq!(tool_command(&json!(cell)), "");
        assert_eq!(tool_command(&json!({"input": cell})), "");
    }

    /// A shell command run from a code-mode cell reaches hooks as its own
    /// `Bash` call, in the shape Codex's hook schema gives every unified-exec
    /// command: `tool_input.command` and the output text as `tool_response`.
    /// It is a recognized verification without a status; only an explicit
    /// integer exit code, never text, would give it one.
    #[test]
    fn codex_shell_results_carry_no_exit_status_unless_explicit() {
        use serde_json::json;
        let nested = json!({"session_id":"s1","turn_id":"turn-1","cwd":"/work",
            "hook_event_name":"PostToolUse","model":"gpt-test","permission_mode":"default",
            "transcript_path":null,"tool_name":"Bash","tool_use_id":"call-1",
            "tool_input":{"command":"cargo test"},"tool_response":"test result: ok. 3 passed"});
        let pending = pending_verification(&nested, Path::new("/work")).expect("recognized");
        assert_eq!((pending.kind, pending.hidden), ("test_result", None));
        assert_eq!(
            observed_exit_status(&nested, false),
            None,
            "a completed Codex call is not a pass"
        );
        for text in [
            "Process exited with code 0",
            "Exit code 0",
            "{\"exit_code\":0}",
        ] {
            let mut forged = nested.clone();
            forged["tool_response"] = json!(text);
            assert_eq!(observed_exit_status(&forged, false), None, "{text}");
        }
        // Code mode's own result object: an explicit integer is a status, a
        // session still running has none.
        let mut explicit = nested.clone();
        explicit["tool_response"] =
            json!({"wall_time_seconds":1.5,"exit_code":101,"output":"FAILED"});
        assert_eq!(
            observed_exit_status(&explicit, false),
            Some((101, "tool_response"))
        );
        for response in [
            json!({"wall_time_seconds":10.0,"session_id":7,"output":"running"}),
            json!({"exit_code":null,"output":""}),
        ] {
            let mut unknown = nested.clone();
            unknown["tool_response"] = response;
            assert_eq!(observed_exit_status(&unknown, false), None);
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
        let mut session = HookSession::new(TaskId::new(), ProjectId::new(), None);
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
            // Every target runs: a verification target does not cover the rest.
            "make test deploy",
            "make build release",
            "make -j 4",
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
            ("make lint test", "lint_result"),
            ("make -j 4 test", "test_result"),
            ("make test CC=clang", "test_result"),
            ("make test 2>&1", "test_result"),
            ("make test > /dev/null", "test_result"),
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

    fn brief_of<'a>(file_path: &'a str, symbols: &'a [BriefSymbol]) -> FileBrief<'a> {
        FileBrief {
            file_path,
            symbols,
            callers: &[],
            imports: &[],
            size_bytes: None,
        }
    }

    #[test]
    fn file_briefing_dedupes_per_session() {
        let temp = tempfile::TempDir::new().expect("temp");
        let symbols = [def("Function", "run", None)];
        let (a, b) = (
            brief_of("src/a.rs", &symbols),
            brief_of("src/b.rs", &symbols),
        );
        assert!(claim_file_brief(temp.path(), "s1", &a, "task"));
        assert!(!claim_file_brief(temp.path(), "s1", &a, "task"));
        assert!(claim_file_brief(temp.path(), "s1", &b, "task"));
        assert!(
            claim_file_brief(temp.path(), "s2", &a, "task"),
            "per-session"
        );
    }

    #[test]
    fn a_new_task_rebriefs_a_file_once_when_its_sources_differ() {
        let temp = tempfile::TempDir::new().expect("temp");
        let first = [
            def("Function", "alpha", Some((1, 3))),
            def("Function", "beta", Some((5, 7))),
        ];
        let reordered = [
            def("Function", "beta", Some((6, 8))),
            def("Function", "alpha", Some((1, 3))),
        ];
        let other = [
            def("Function", "gamma", Some((9, 11))),
            def("Function", "alpha", Some((1, 3))),
        ];
        let claim = |symbols: &[BriefSymbol], task: &str| {
            claim_file_brief(temp.path(), "s", &brief_of("src/a.rs", symbols), task)
        };
        assert!(claim(&first, "fix alpha"));
        // Within one task a file is briefed once, even if its sources change.
        assert!(!claim(&other, "fix alpha"));
        // A new task whose brief only reorders or moves spans is not repeated,
        // and that task has had its one chance.
        assert!(!claim(&reordered, "fix beta"));
        assert!(!claim(&other, "fix beta"));
        // A later task whose brief lists other sources re-briefs once.
        assert!(claim(&other, "fix gamma"));
        assert!(!claim(&other, "fix gamma"));
        assert!(!claim(&first, "fix gamma"));
        // Going back to the first sources in yet another task is a change.
        assert!(claim(&first, "fix alpha again"));
        assert_ne!(
            brief_sources(&brief_of("src/a.rs", &first)),
            brief_sources(&brief_of("src/b.rs", &first)),
            "the file is part of the identity"
        );
    }

    #[test]
    fn older_brief_lists_count_as_delivered_for_the_current_task() {
        let temp = tempfile::TempDir::new().expect("temp");
        let path = session_files_path(temp.path(), "s");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(&path, "src/a.rs\n").expect("legacy list");
        let symbols = [def("Function", "alpha", None)];
        let brief = brief_of("src/a.rs", &symbols);
        assert!(!claim_file_brief(temp.path(), "s", &brief, "current task"));
        assert!(claim_file_brief(temp.path(), "s", &brief, "next task"));
        assert!(claim_file_brief(
            temp.path(),
            "s",
            &brief_of("src/b.rs", &symbols),
            "next task"
        ));
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
        let empty = file_brief(&[], &[], &[], None);
        assert_eq!(render_file_shape(&empty, LARGE, usize::MAX), None);
        let symbols = [def("Function", "validate", None)];
        let callers = [("login".to_owned(), "src/routes.rs".to_owned())];
        let imports = ["crate::db".to_owned()];
        let shape = render_file_shape(
            &file_brief(&symbols, &callers, &imports, Some(900)),
            LARGE,
            usize::MAX,
        )
        .expect("shape");
        assert!(shape.contains("Defines: validate (Function)\n"), "{shape}");
        assert!(shape.contains("login [src/routes.rs]"));
        assert!(shape.contains("crate::db"));
        assert!(!shape.contains("File size"), "small files get no size line");
    }

    const LARGE: u64 = 64 * 1024;

    fn file_brief<'a>(
        symbols: &'a [BriefSymbol],
        callers: &'a [(String, String)],
        imports: &'a [String],
        size_bytes: Option<u64>,
    ) -> FileBrief<'a> {
        FileBrief {
            file_path: "src/auth.rs",
            symbols,
            callers,
            imports,
            size_bytes,
        }
    }

    fn def(kind: &str, label: &str, lines: Option<(u32, u32)>) -> BriefSymbol {
        BriefSymbol {
            kind: kind.to_owned(),
            label: label.to_owned(),
            lines: lines.map(|(start, end)| crate::world::LineRange { start, end }),
            task_match: false,
        }
    }

    #[test]
    fn file_briefs_say_when_task_matches_come_first() {
        let mut named = def("Function", "engine_step_1500", Some((19_504, 19_515)));
        named.task_match = true;
        let symbols = vec![
            named.clone(),
            def("Function", "engine_step_0000", Some((4, 15))),
        ];
        let text = render_file_shape(&file_brief(&symbols, &[], &[], None), LARGE, usize::MAX)
            .expect("ranked");
        assert!(
            text.contains(
                "Defines (line spans, task matches first): engine_step_1500 (Function) 19504-19515, engine_step_0000 (Function) 4-15\n"
            ),
            "{text}"
        );
        named.lines = None;
        let text = render_file_shape(&file_brief(&[named], &[], &[], None), LARGE, usize::MAX)
            .expect("unspanned");
        assert!(
            text.contains("Defines (task matches first): engine_step_1500 (Function)\n"),
            "{text}"
        );
    }

    #[test]
    fn file_briefs_list_checked_line_spans_and_fall_back_without_them() {
        let current = vec![
            def("Type", "Engine", Some((3, 7))),
            def("Function", "Engine::run", Some((10, 13))),
            def("Section", "Notes", None),
        ];
        let text = render_file_shape(&file_brief(&current, &[], &[], None), LARGE, usize::MAX)
            .expect("ranged");
        assert!(
            text.contains(
                "Defines (line spans): Engine (Type) 3-7, Engine::run (Function) 10-13, Notes (Section)\n"
            ),
            "{text}"
        );
        // A checkout file that changed since indexing reports no spans.
        let stale = vec![
            def("Type", "Engine", None),
            def("Function", "Engine::run", None),
            def("Section", "Notes", None),
        ];
        let text = render_file_shape(&file_brief(&stale, &[], &[], None), LARGE, usize::MAX)
            .expect("plain");
        assert!(
            text.contains("Defines: Engine (Type), Engine::run (Function), Notes (Section)\n"),
            "{text}"
        );
        assert!(!text.contains("line spans"), "{text}");
    }

    #[test]
    fn large_files_get_one_size_line_that_names_spans_only_when_present() {
        let current = vec![def("Function", "compress", Some((1_200, 1_350)))];
        let stale = vec![def("Function", "compress", None)];
        let size = Some(812 * 1024 - 100);
        let ranged = render_file_shape(&file_brief(&current, &[], &[], size), LARGE, usize::MAX)
            .expect("ranged");
        assert!(
            ranged.contains(
                "Defines (line spans): compress (Function) 1200-1350\nFile size: 812 KB. The line spans locate the listed definitions.\n"
            ),
            "{ranged}"
        );
        assert_eq!(ranged.matches("File size").count(), 1);
        let plain = render_file_shape(&file_brief(&stale, &[], &[], size), LARGE, usize::MAX)
            .expect("plain");
        assert!(plain.ends_with("File size: 812 KB.\n"), "{plain}");
        for (size, threshold) in [(Some(LARGE - 1), LARGE), (None, LARGE), (Some(LARGE), 0)] {
            let text =
                render_file_shape(&file_brief(&current, &[], &[], size), threshold, usize::MAX)
                    .expect("brief");
            assert!(!text.contains("File size"), "{size:?} {threshold}: {text}");
        }
        assert!(
            render_file_shape(
                &file_brief(&current, &[], &[], Some(LARGE)),
                LARGE,
                usize::MAX
            )
            .expect("at the threshold")
            .contains("File size: 64 KB.")
        );
    }

    #[test]
    fn file_briefs_fit_each_clients_message_cap() {
        let definitions: Vec<_> = (0_u32..25)
            .map(|n| {
                def(
                    "Function",
                    &format!("handle_request_with_a_long_name_{n}"),
                    Some((n * 100 + 1, n * 100 + 90)),
                )
            })
            .collect();
        let callers: Vec<_> = (0..12)
            .map(|n| {
                (
                    format!("caller_number_{n}"),
                    format!("src/callers/module_{n}.rs"),
                )
            })
            .collect();
        let imports: Vec<_> = (0..12)
            .map(|n| format!("use crate::services::module_{n}::{{Engine, Settings}};"))
            .collect();
        let brief = file_brief(&definitions, &callers, &imports, Some(900 * 1024));
        let full = render_file_shape(&brief, LARGE, usize::MAX).expect("unbounded");
        assert_eq!(full.matches("handle_request_with_a_long_name_").count(), 20);
        assert!(full.contains("line spans"));
        // Below the ranged form's size, the same brief comes without spans.
        let plain = render_file_shape(&brief, LARGE, full.len() - 1).expect("plain");
        assert!(!plain.contains("line spans") && plain.len() < full.len());
        assert!(plain.contains("File size: 900 KB.\n"), "{plain}");
        assert_eq!(render_file_shape(&brief, LARGE, plain.len() - 1), None);
        // Every client's whole-message cap holds the ranged form.
        for client in [
            HookClient::ClaudeCode,
            HookClient::Codex,
            HookClient::Cursor,
        ] {
            let text = render_file_shape(&brief, LARGE, client.context_byte_cap()).expect("fits");
            assert!(text.len() <= client.context_byte_cap(), "{client:?}");
            assert_eq!(text, full, "{client:?}");
        }
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
    fn host_records_are_not_requests() {
        for record in [
            "<bash-input>claude auth login</bash-input>",
            "<bash-input>cargo test</bash-input><bash-stdout>ok</bash-stdout><bash-stderr></bash-stderr>",
            "  <bash-input>ls</bash-input>\n<bash-stdout>a\nb</bash-stdout>\n",
            "<local-command-caveat>Caveat</local-command-caveat>\n<local-command-stdout>Total cost: $1</local-command-stdout>",
            "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>Background command \"Run the full test suite\" completed (exit code 0)</summary>\n</task-notification>",
            "<agent-message from=\"a300a92a\">\n[Subagent hand-back] Review report\n</agent-message>",
            // Output that quotes a closing tag, such as `!git diff` of tests.
            "<bash-input>git diff</bash-input><bash-stdout>+ \"<bash-stdout>x</bash-stdout>\"\n</bash-stdout><bash-stderr></bash-stderr>",
        ] {
            assert!(is_host_record(record), "{record}");
        }
        for prompt in [
            "",
            "Fix the login",
            "<bash-input>cargo test</bash-input><bash-stdout>FAILED</bash-stdout>\nwhy did this fail?",
            "Look at this: <bash-stdout>FAILED</bash-stdout>",
            "<bash-input>cargo test",
            "<bash-inputs>x</bash-inputs>",
            "<command-name>/review</command-name><command-args>the auth module</command-args>",
        ] {
            assert!(!is_host_record(prompt), "{prompt}");
        }

        let temp = tempfile::TempDir::new().expect("temp");
        let redactor = crate::security::Redactor::new(&[]).expect("redactor");
        let record = "<bash-input>claude auth login</bash-input><bash-stdout>Login successful.</bash-stdout>";
        // Never saved now; a record an earlier build saved is ignored.
        save_pending_prompt(temp.path(), "older-build", record, &redactor).expect("save");
        assert_eq!(pending_prompt(temp.path(), "older-build"), None);
        std::fs::create_dir_all(temp.path().join("hook-sessions")).expect("state");
        write_private_sidecar(
            &session_state_path(temp.path(), "older-build").with_extension("pending"),
            record.as_bytes(),
        )
        .expect("legacy record");
        assert_eq!(pending_prompt(temp.path(), "older-build"), None);
        save_pending_prompt(temp.path(), "older-build", "Fix the login", &redactor).expect("save");
        assert_eq!(
            pending_prompt(temp.path(), "older-build").as_deref(),
            Some("Fix the login")
        );
        // A question about a command keeps the command, not its output.
        save_pending_prompt(
            temp.path(),
            "older-build",
            "<bash-input>cargo test</bash-input><bash-stdout>db_pass=hunter2 FAILED</bash-stdout>\nwhy did this fail?",
            &redactor,
        )
        .expect("save");
        assert_eq!(
            pending_prompt(temp.path(), "older-build").as_deref(),
            Some("$ cargo test\nwhy did this fail?")
        );
    }

    #[test]
    fn each_tool_call_verification_is_claimed_once() {
        let temp = tempfile::TempDir::new().expect("temp");
        let call = serde_json::json!({"tool_use_id":"toolu_1","tool_name":"Bash"});
        assert!(claim_verification(temp.path(), "s1", &call));
        assert!(
            !claim_verification(temp.path(), "s1", &call),
            "the concurrent second hook for the same call loses"
        );
        let next = serde_json::json!({"tool_use_id":"toolu_2","tool_name":"Bash"});
        assert!(
            claim_verification(temp.path(), "s1", &next),
            "new calls are new"
        );
        let anonymous = serde_json::json!({"tool_name":"Bash"});
        assert!(claim_verification(temp.path(), "s1", &anonymous));
        assert!(
            claim_verification(temp.path(), "s1", &anonymous),
            "calls without an id are always handled"
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
        let symbols = [BriefSymbol {
            kind: "Function".to_owned(),
            label: "compute".to_owned(),
            lines: None,
            task_match: false,
        }];
        let brief = FileBrief {
            file_path: "src/lib.rs",
            symbols: &symbols,
            callers: &[],
            imports: &[],
            size_bytes: None,
        };
        assert!(claim_file_brief(temp.path(), &worker, &brief, "task"));
        assert!(
            claim_file_brief(temp.path(), &scope, &brief, "task"),
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
                    status_bearing: true,
                    hidden: None,
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

    /// Each hidden status names its first cause, in the order the note
    /// reports it.
    #[test]
    fn hidden_statuses_name_their_cause() {
        use StatusHidden::{Background, Directory, Or, Pipe, Sequence, Syntax};
        for (command, reason) in [
            ("cargo test 'unterminated", Syntax),
            ("cargo test <<EOF\nx\nEOF", Syntax),
            ("cd sub && cargo test -- `cat names`", Syntax),
            ("cargo test &", Background),
            ("cargo test 2>&1 | tail -5 &", Background),
            ("cargo test || true", Or),
            ("cargo test | tail || true", Or),
            ("cargo test; echo done", Sequence),
            ("cargo test\necho $?", Sequence),
            ("cargo test | tail; echo done", Sequence),
            ("cargo test 2>&1 | tail -20", Pipe),
            ("set -o pipefail; set +o pipefail; cargo test | tail", Pipe),
            ("cd ../two; cargo test", Directory),
            ("cd $REPO && cargo test", Directory),
        ] {
            let found = classify_verification(command).expect("recognized");
            assert_eq!(found.hidden, Some(reason), "{command}");
            assert!(!found.status_bearing, "{command}");
        }
        let plain = classify_verification("set -o pipefail; cargo test | tail").expect("plain");
        assert_eq!((plain.status_bearing, plain.hidden), (true, None));
    }

    /// A pending verification run in the background (Claude Code's
    /// `run_in_background`) hides its status even when its text does not.
    #[test]
    fn background_tool_calls_hide_the_status() {
        let payload = serde_json::json!({"tool_name":"Bash",
            "tool_input":{"command":"cargo test","run_in_background":true}});
        let pending = pending_verification(&payload, Path::new("/work")).expect("pending");
        assert_eq!(pending.hidden, Some(StatusHidden::Background));
        assert_eq!(pending.kind, "test_result");
        let payload = serde_json::json!({"tool_name":"Bash","tool_input":{"command":"cargo test"}});
        let pending = pending_verification(&payload, Path::new("/work")).expect("pending");
        assert_eq!(pending.hidden, None);
    }

    /// The suggested command is the same verification in a recordable shape,
    /// checked by the classifier; unsafe filters are replaced.
    #[test]
    fn recordable_forms_are_checked_and_keep_safe_filters() {
        for (command, form, replaced) in [
            (
                "cargo test 2>&1 | tail -20",
                "set -o pipefail; cargo test 2>&1 | tail -20",
                false,
            ),
            (
                "cd repo && cargo test 2>&1 | grep -E 'test result|FAILED' | head",
                "set -o pipefail; cd repo && cargo test 2>&1 | tail -n 40",
                true,
            ),
            ("cargo test; echo done", "cargo test", false),
            ("cargo test || true", "cargo test", false),
            ("cargo test &", "cargo test", false),
            ("cd ../two; cargo test", "cd ../two && cargo test", false),
            (
                "cargo build; cargo test | tee out.log | wc -l; echo $?",
                "set -o pipefail; cargo build && cargo test | tee out.log | wc -l",
                false,
            ),
            (
                "set -o pipefail; cargo test | tail; echo done",
                "set -o pipefail; cargo test | tail",
                false,
            ),
            (
                "git diff | wc -l; cargo test 2>&1 | head -50",
                "set -o pipefail; git diff | wc -l && cargo test 2>&1 | tail -n 40",
                true,
            ),
        ] {
            assert_eq!(
                recordable_form(command),
                Some((form.to_owned(), replaced)),
                "{command}"
            );
            assert!(classify_verification(form).is_some_and(|found| found.status_bearing));
        }
        for command in [
            "(cargo test)",
            "cd $REPO && cargo test",
            "cargo build && cd sub && cargo test | tail",
        ] {
            assert_eq!(recordable_form(command), None, "{command}");
        }
    }

    /// One short reference-data paragraph: the cause and the recordable form,
    /// or general advice when no checked form exists.
    #[test]
    fn status_notes_explain_the_cause_and_the_recordable_form() {
        let note = status_note("cargo test 2>&1 | grep FAILED", StatusHidden::Pipe);
        assert!(note.starts_with("# Engram verification note (reference data)\n"));
        assert!(note.contains("Engram did not record this verification's result"));
        assert!(note.contains("a pipe reports its last command's exit status"));
        assert!(note.contains("`set -o pipefail; cargo test 2>&1 | tail -n 40`"));
        assert!(note.contains("`grep` fails when nothing matches"));
        assert_eq!(note.lines().count(), 2, "{note}");
        let background = status_note("cargo test", StatusHidden::Background);
        assert!(background.contains("run `cargo test` in the foreground"));
        let general = status_note("(cd sub && cargo test)", StatusHidden::Syntax);
        assert!(general.contains("because its exit status was hidden: Engram does not read"));
        assert!(general.contains("run the verification as a plain command"));
        let directory = status_note("cd $REPO && cargo test", StatusHidden::Directory);
        assert!(directory.contains("because the repository it ran in was hidden"));
        assert!(directory.contains("put `cd <path> &&` with a fixed path right before it"));
        let long = format!("cargo test {} | tail", "x".repeat(400));
        let long = status_note(&long, StatusHidden::Pipe);
        assert!(long.contains("start the command with `set -o pipefail;` or drop the pipe"));
        let wide = format!("cargo test {} | tail", "\u{1d535}".repeat(60));
        assert!(status_note(&wide, StatusHidden::Pipe).contains("drop the pipe"));
        // The example in docs/USER_GUIDE.md.
        assert_eq!(
            status_note("cargo test 2>&1 | tail -20", StatusHidden::Pipe),
            "# Engram verification note (reference data)\nEngram did not record this \
             verification's result because its exit status was hidden: a pipe reports its \
             last command's exit status, not the verification's. To record it, run \
             `set -o pipefail; cargo test 2>&1 | tail -20`. This note appears once per session \
             for this cause.\n"
        );
        // Rerunning cannot help when the client reports no status.
        let unreported = status_note("cargo test", StatusHidden::Unreported);
        assert!(unreported.contains("the coding client reported none for it"));
        assert!(unreported.contains("`engram_record_evidence`") && !unreported.contains("run `"));
        for reason in [
            StatusHidden::Syntax,
            StatusHidden::Background,
            StatusHidden::Or,
            StatusHidden::Sequence,
            StatusHidden::Pipe,
            StatusHidden::Directory,
            StatusHidden::Unreported,
        ] {
            for command in [
                "y".repeat(5_000),
                format!("cargo test {}", "x".repeat(5_000)),
            ] {
                let note = status_note(&command, reason);
                assert!(note.len() <= STATUS_NOTE_BYTES, "{reason:?}");
            }
        }
    }

    /// One note per session and cause: concurrent hooks race for one claim,
    /// a failed delivery releases it, and another cause has its own.
    #[test]
    fn status_notes_are_claimed_once_per_session_and_cause() {
        let temp = tempfile::TempDir::new().expect("temp");
        assert!(claim_status_note(temp.path(), "s1", StatusHidden::Pipe));
        assert!(!claim_status_note(temp.path(), "s1", StatusHidden::Pipe));
        assert!(claim_status_note(temp.path(), "s1", StatusHidden::Or));
        assert!(claim_status_note(temp.path(), "s2", StatusHidden::Pipe));
        release_status_note(temp.path(), "s1", StatusHidden::Pipe);
        assert!(claim_status_note(temp.path(), "s1", StatusHidden::Pipe));
        // Session-end cleanup keeps the note claims: a resumed session has
        // already seen its notes.
        clear_claims(temp.path(), "s1");
        assert!(!claim_status_note(temp.path(), "s1", StatusHidden::Pipe));
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

    /// An unmodeled directory is still bounded by the tool's directory when
    /// every change is a relative descent; anything else stays unknown.
    #[test]
    fn unmodeled_directories_are_bounded_by_relative_descents() {
        let base = Path::new("/work/one");
        for (command, bound) in [
            ("cargo test", Some("/work/one")),
            ("cd ../two && cargo test", Some("/work/one/../two")),
            ("cd src; cargo test", Some("/work/one")),
            ("cd a; cd b/c; cargo test", Some("/work/one")),
            ("cargo test && cd sub && cargo test", Some("/work/one")),
            ("cd ../two; cargo test", None),
            ("cd /srv/app; cargo test", None),
            ("cd ~/two; cargo test", None),
            ("cd $REPO && cargo test", None),
            ("cd - && cargo test", None),
            ("source env.sh && cargo test", None),
            ("(cd src && cargo test)", None),
        ] {
            assert_eq!(
                shell_workdir_bound(command, base),
                bound.map(PathBuf::from),
                "{command}"
            );
        }
        // A symbolic link that leads out of the directory does not bound it.
        let temp = tempfile::TempDir::new().expect("temp");
        let root = temp.path().canonicalize().expect("canonical");
        std::fs::create_dir_all(root.join("repo/src")).expect("repo");
        std::fs::create_dir_all(root.join("elsewhere")).expect("elsewhere");
        std::os::unix::fs::symlink(root.join("elsewhere"), root.join("repo/out")).expect("link");
        let repo = root.join("repo");
        assert_eq!(
            shell_workdir_bound("cd src; cargo test", &repo),
            Some(repo.clone())
        );
        assert_eq!(shell_workdir_bound("cd out; cargo test", &repo), None);
    }

    fn episode(roots: &[&str]) -> (String, HookSession) {
        let mut session = HookSession::new(TaskId::new(), ProjectId::new(), None);
        for root in roots {
            session.note_checkout(Path::new(root));
        }
        (format!("scope-{}", roots.join(",")), session)
    }

    fn pending(workdir: Option<&str>, within: Option<&str>) -> PendingVerification {
        PendingVerification {
            key: format!("{}\ncargo test", workdir.unwrap_or("?")),
            kind: "test_result",
            tests: true,
            workdir: workdir.map(PathBuf::from),
            within: within.or(workdir).map(PathBuf::from),
            hidden: None,
        }
    }

    /// The write-ahead marks only the episodes a verification may belong to,
    /// by the checkout roots each episode knows, and every episode when the
    /// directory is truly unknown.
    #[test]
    fn the_write_ahead_targets_episodes_by_their_checkouts() {
        let sessions = vec![
            episode(&["/w/a", "/wt/a-feature"]),
            episode(&["/w/b"]),
            episode(&["/w/a/sub"]),
        ];
        let targets = |workdir, within| write_ahead_targets(&sessions, &pending(workdir, within));
        // A modeled directory: the most specific checkout, any of its roots.
        assert_eq!(targets(Some("/w/b/src"), None), [1]);
        assert_eq!(targets(Some("/wt/a-feature"), None), [0]);
        assert_eq!(targets(Some("/w/a/sub/x"), None), [2]);
        assert_eq!(targets(Some("/w/a"), None), [0]);
        // Outside every known checkout: a repository without an episode.
        assert!(targets(Some("/w/c"), None).is_empty());
        // An unmodeled directory inside a checkout, including nested ones.
        assert_eq!(targets(None, Some("/w/b")), [1]);
        assert_eq!(targets(None, Some("/w/a")), [0, 2]);
        assert_eq!(targets(None, Some("/w")), [0, 1, 2]);
        // Unmodeled and outside every known checkout, or truly unknown.
        assert_eq!(targets(None, Some("/w/c")), [0, 1, 2]);
        assert_eq!(targets(None, None), [0, 1, 2]);
        // An episode whose checkouts are unknown may own any directory.
        let mut legacy = sessions.clone();
        legacy.push(episode(&[]));
        assert_eq!(
            write_ahead_targets(&legacy, &pending(Some("/w/c"), None)),
            [0, 1, 2, 3]
        );
        assert_eq!(
            write_ahead_targets(&legacy, &pending(Some("/w/b/src"), None)),
            [1]
        );
    }

    #[test]
    fn episodes_keep_each_checkout_once_and_bounded() {
        let mut session = HookSession::new(TaskId::new(), ProjectId::new(), None);
        assert!(!session.has_roots());
        assert!(session.note_checkout(Path::new("/w/a")));
        assert_eq!(session.root.as_deref(), Some(Path::new("/w/a")));
        assert!(!session.note_checkout(Path::new("/w/a")));
        assert!(session.note_checkout(Path::new("/wt/a")));
        assert!(!session.note_checkout(Path::new("/wt/a")));
        for index in 0..EPISODE_CHECKOUTS_MAX + 4 {
            session.note_checkout(&PathBuf::from(format!("/wt/{index}")));
        }
        assert_eq!(session.checkouts.len(), EPISODE_CHECKOUTS_MAX);
        assert_eq!(session.roots().count(), EPISODE_CHECKOUTS_MAX + 1);
        // Sidecars from an earlier build read without checkouts.
        let legacy: HookSession = serde_json::from_value(serde_json::json!({
            "task_id": TaskId::new(), "project_id": ProjectId::new(), "root": "/w/a"}))
        .expect("legacy sidecar");
        assert_eq!(legacy.roots().collect::<Vec<_>>(), [Path::new("/w/a")]);
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

    /// The re-verification label is reserved like the hook labels: client
    /// metadata loses it, and only hook-endpoint provenance attaches it with
    /// digests the judge reads.
    #[test]
    fn the_reverification_label_is_reserved_for_hook_provenance() {
        use serde_json::json;
        let digest = "b".repeat(64);
        let forged = json!({"source":REVERIFY_SOURCE,"source_digests":{"lib.rs":digest},
            "note":"kept"});
        let stored = evidence_metadata(forged, None);
        assert_eq!(stored, json!({"note":"kept"}));
        assert!(!is_reverification(&stored));
        assert!(hook_source_digests(&stored).is_none());
        let provenance = HookProvenance {
            source: REVERIFY_SOURCE.to_owned(),
            source_digests: BTreeMap::from([("lib.rs".to_owned(), digest.clone())]),
            source_digests_truncated: true,
        };
        let reverified = evidence_metadata(json!({"head":"c0ffee"}), Some(&provenance));
        assert_eq!(reverified["source"], REVERIFY_SOURCE);
        assert_eq!(reverified["source_digests_truncated"], true);
        assert!(is_reverification(&reverified));
        assert_eq!(
            hook_source_digests(&reverified),
            Some(BTreeMap::from([("lib.rs".to_owned(), digest)]))
        );
        let hooked = evidence_metadata(
            json!({}),
            Some(&HookProvenance {
                source: CLAUDE_HOOK_SOURCE.to_owned(),
                ..provenance
            }),
        );
        assert!(!is_reverification(&hooked));
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
        // A client that puts an explicit code in its result object.
        let explicit = json!({"hook_event_name":"PostToolUse","tool_name":"Bash",
            "tool_response":{"exit_code":3,"stdout":""}});
        assert_eq!(
            observed_exit_status(&explicit, false),
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
            None,
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
                observe_verification(
                    &payload(hidden),
                    CLAUDE_HOOK_SOURCE,
                    root,
                    None,
                    &touched,
                    None
                )
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
            None,
        )
        .expect("repeat");
        assert_eq!(
            repeat.fingerprint, observed.fingerprint,
            "a hook fired twice for one tool call is one observation"
        );
        let mut rerun = payload("cargo test");
        rerun["tool_use_id"] = json!("toolu_2");
        let rerun = observe_verification(&rerun, CLAUDE_HOOK_SOURCE, root, None, &touched, None)
            .expect("rerun");
        assert_ne!(
            rerun.fingerprint, observed.fingerprint,
            "a new tool call with identical output is a new observation"
        );
        let mut legacy = payload("cargo test");
        legacy
            .as_object_mut()
            .expect("payload")
            .remove("tool_use_id");
        let before = observe_verification(&legacy, CLAUDE_HOOK_SOURCE, root, None, &touched, None)
            .expect("legacy");
        std::fs::write(root.join("lib.rs"), b"pub fn a() { changed() }").expect("edit");
        let after = observe_verification(&legacy, CLAUDE_HOOK_SOURCE, root, None, &touched, None)
            .expect("legacy edit");
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "without call ids, a rerun on edited source is a new observation"
        );
    }

    /// Codex sends output text; its rollout supplies the status, unless the
    /// shell hides it.
    #[test]
    fn codex_verifications_take_their_status_from_the_rollout() {
        use serde_json::json;
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path();
        let touched: [String; 0] = [];
        let codex = |command: &str| {
            json!({"hook_event_name":"PostToolUse","tool_name":"Bash","tool_use_id":"exec-1",
                "tool_input":{"command":command},"tool_response":"test result: FAILED"})
        };
        let reported = Some((101, "codex_rollout"));
        let failed = observe_verification(
            &codex("cargo test"),
            CODEX_HOOK_SOURCE,
            root,
            None,
            &touched,
            reported,
        )
        .expect("rollout status");
        assert_eq!(failed.exit_code, 101);
        assert_eq!(failed.metadata["exit_status"], "codex_rollout");
        assert_eq!(failed.artifact, "test result: FAILED");
        assert!(
            observe_verification(
                &codex("cargo test | tail -3"),
                CODEX_HOOK_SOURCE,
                root,
                None,
                &touched,
                reported
            )
            .is_none()
        );
        assert!(
            observe_verification(
                &codex("cargo test"),
                CODEX_HOOK_SOURCE,
                root,
                None,
                &touched,
                None
            )
            .is_none(),
            "no record, no status"
        );
    }
}
