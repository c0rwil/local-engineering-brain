//! Stock-agent integration via Claude Code hooks: context injection and
//! automatic evidence capture with NO proxy and NO agent cooperation.
//!
//! `leb hook prompt` runs on `UserPromptSubmit`: the first prompt of a
//! session opens a task episode, compiles brain context for that exact text
//! (declaring the task for truthful usage provenance), and prints it —
//! Claude Code adds hook stdout to the model's context. Later prompts print
//! nothing, mirroring the proxy's session-pinned semantics.
//!
//! `leb hook bash` runs on `PostToolUse` for Bash: when the executed command
//! is a recognizable verification (test/build/lint/typecheck), its exit code
//! is recorded as deterministic evidence on the session's task — including
//! FAILURES, which is exactly what fail-then-pass preference distillation
//! feeds on. The agent no longer has to remember to record anything.
//!
//! Every hook fails OPEN: a broken brain must never break a coding session.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{ProjectId, TaskId};

/// Session → open-task linkage, stored beside the brain so all hook
/// invocations of one session agree.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HookSession {
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
    format!("{exit_code}:{}", command.trim())
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
    std::fs::write(&path, serde_json::to_string(session)?)
        .map_err(|error| Error::Config(format!("hook session write: {error}")))
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
/// acceptance stays unknown, so auto-closed episodes count toward memory
/// ROI (wins) but never feed distillation, which requires explicit
/// acceptance.
#[must_use]
pub fn auto_close_outcome(last_verification_exit: Option<i32>) -> &'static str {
    match last_verification_exit {
        Some(0) => "success",
        _ => "abandoned",
    }
}

/// The hooks block written into a repo's `.claude/settings.json`.
#[must_use]
pub fn claude_hooks_json(leb_binary: &str) -> serde_json::Value {
    serde_json::json!({
        "UserPromptSubmit": [{
            "hooks": [{
                "type": "command",
                "command": format!("{leb_binary} hook prompt"),
                "timeout": 30
            }]
        }],
        "PostToolUse": [{
            "matcher": "Bash",
            "hooks": [{
                "type": "command",
                "command": format!("{leb_binary} hook bash"),
                "timeout": 15
            }]
        }],
        "SessionEnd": [{
            "hooks": [{
                "type": "command",
                "command": format!("{leb_binary} hook session-end"),
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
        assert_eq!(auto_close_outcome(Some(1)), "abandoned");
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
    fn session_paths_are_sanitized() {
        let path = session_state_path(Path::new("/state"), "abc/../../etc:passwd");
        assert!(
            path.to_string_lossy()
                .contains("hook-sessions/abcetcpasswd.json")
        );
    }
}
