//! Stock-agent integration via Claude Code hooks: context injection and
//! automatic evidence capture with NO proxy and NO agent cooperation.
//!
//! `engram hook prompt` runs on `UserPromptSubmit`: the first prompt of a
//! session opens a task episode, compiles brain context for that exact text
//! (declaring the task for truthful usage provenance), and prints it —
//! Claude Code adds hook stdout to the model's context. Later prompts print
//! nothing, mirroring the proxy's session-pinned semantics.
//!
//! `engram hook bash` runs on `PostToolUse` for Bash: when the executed command
//! is a recognizable verification (test/build/lint/typecheck), its exit code
//! is recorded as deterministic evidence on the session's task — including
//! FAILURES, which is exactly what fail-then-pass preference distillation
//! feeds on. The agent no longer has to remember to record anything.
//!
//! Every hook fails OPEN: a broken brain must never break a coding session.

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

/// Retain only a bounded, redacted prompt until the first tool use.
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

/// Resolve explicit tool paths before falling back to the session cwd.
/// A simple leading `cd <path> &&` is recognized; shell code is never executed.
#[must_use]
pub fn tool_directory(cwd: &Path, input: &serde_json::Value) -> PathBuf {
    let explicit = input
        .get("cwd")
        .or_else(|| input.get("workdir"))
        .and_then(serde_json::Value::as_str);
    let command = input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let cd_path = command
        .trim()
        .strip_prefix("cd ")
        .and_then(|s| s.split_once("&&"))
        .map(|(path, _)| path.trim().trim_matches(['\'', '"']))
        .filter(|path| !path.contains(['$', '`', ';', '|', '\n']));
    let file = extract_file_path(input);
    let raw = explicit.or(cd_path).or(file.as_deref());
    let Some(raw) = raw else {
        return cwd.to_path_buf();
    };
    let path = PathBuf::from(raw);
    let mut path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    if explicit.is_none() && cd_path.is_none() {
        path.pop();
    }
    while !path.is_dir() {
        if !path.pop() {
            return cwd.to_path_buf();
        }
    }
    path
}

/// Pull the file path out of a tool input (Read/Edit/Write shapes).
#[must_use]
pub fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    for key in ["file_path", "filePath", "path", "notebook_path"] {
        if let Some(path) = tool_input.get(key).and_then(serde_json::Value::as_str) {
            if !path.trim().is_empty() {
                return Some(path.to_owned());
            }
        }
    }
    None
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
        let path = session_state_path(state_dir, session_id).with_extension(format!("{tag}.claim"));
        let _ = std::fs::remove_file(path);
    }
}

/// Sidecar tracking which files a session has already been briefed on, so
/// pre-tool shape injection fires once per file per session.
#[must_use]
pub fn session_files_path(state_dir: &Path, session_id: &str) -> PathBuf {
    session_state_path(state_dir, session_id).with_extension("files")
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
    let mut brief = format!("# Graph shape: {file_path}\n");
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
            "matcher": "Bash",
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook bash"),
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

/// The hooks block for Codex CLI (`~/.codex/hooks.json`): same events, same
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
            "matcher": "Read|Edit|Write",
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook pretool --codex"),
                "timeout": 10
            }]
        }],
        "PostToolUse": [{
            "matcher": "Bash|shell",
            "hooks": [{
                "type": "command",
                "command": format!("{engram_binary} hook bash"),
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
