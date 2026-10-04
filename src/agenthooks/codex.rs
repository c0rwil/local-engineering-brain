//! Codex thread roles for hook routing.
//!
//! Codex runs subagents in their own threads, but every hook payload carries
//! the root conversation's `session_id` ("Subagent hooks use the parent session
//! id", Codex hooks documentation). Spawned workers add `agent_id` and
//! `agent_type` to their prompt and tool hooks. Internal sub-sessions, such as
//! the guardian that reviews approval requests, add neither; the leading
//! `session_meta` record of the transcript named by `transcript_path`
//! identifies them.
//!
//! Routing policy:
//! - Spawned workers write code for the root task. Their tool calls and
//!   verifications are captured on the root session's episode. Their prompts
//!   are messages from the parent agent, so they never replace the user's
//!   pending task prompt, drain the root's relay inbox, or receive
//!   prompt-time context. Context they receive at tool time is tracked per
//!   thread (see `thread_scope`), so it never suppresses the root's.
//! - Internal sub-sessions judge plans, summarize or consolidate. Their hooks
//!   stay silent: no context, no prompt capture, no episode, no usage or
//!   evidence, and they never close the root session's episodes.
//! - A thread whose named transcript cannot be read is unverified. Codex
//!   writes the rollout before it runs hooks, so this is unusual; an internal
//!   sub-session is one explanation. It must not replace an active task prompt.

use std::path::Path;

use serde_json::Value;

pub use crate::usage_capture::CodexThreadKind;

/// The role of the Codex thread that fired a hook.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadRole {
    /// A root conversation: its transcript says so or uses a format this
    /// reader does not know, or Codex sent no transcript at all (a thread
    /// store without local files).
    Root,
    /// Codex named a transcript that cannot be read, so the thread cannot be
    /// told apart from an internal sub-session.
    Unverified,
    /// A worker that another thread spawned.
    Spawned,
    /// A Codex-internal sub-session, named by its source label.
    Internal(String),
}

/// Classify the Codex thread that fired a hook. Payload agent fields are
/// authoritative; otherwise the transcript's `session_meta` decides.
#[must_use]
pub fn thread_role(payload: &Value) -> ThreadRole {
    if payload
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
    {
        return ThreadRole::Spawned;
    }
    let Some(path) = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(Path::new)
    else {
        return ThreadRole::Root;
    };
    match crate::usage_capture::codex_session_meta(path) {
        Some(meta) => match meta.kind {
            CodexThreadKind::Root => ThreadRole::Root,
            CodexThreadKind::Spawned => ThreadRole::Spawned,
            CodexThreadKind::Internal(name) => ThreadRole::Internal(name),
        },
        None if readable_file(path) => ThreadRole::Root,
        None => ThreadRole::Unverified,
    }
}

// Check the type first: opening a FIFO for reading would block the hook.
fn readable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
        && std::fs::File::open(path).is_ok()
}

/// Whether a hook event from this thread must stay silent (see module docs).
/// `session_active` reports whether the session already has a pending task
/// prompt or an open episode; it is evaluated only for unverified prompts.
#[must_use]
pub fn skips_event(event: &str, role: &ThreadRole, session_active: impl FnOnce() -> bool) -> bool {
    match role {
        ThreadRole::Root => false,
        ThreadRole::Spawned => !matches!(event, "pretool" | "bash"),
        ThreadRole::Internal(_) => true,
        // Usage capture needs a readable transcript, so Stop has nothing to do.
        // Codex runs SessionEnd for the root thread only, including after the
        // user deleted the conversation and its transcript, so it still
        // closes the session's episodes.
        ThreadRole::Unverified => match event {
            "stop" => true,
            "prompt" => session_active(),
            _ => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EVENTS: [&str; 6] = [
        "session-start",
        "prompt",
        "pretool",
        "bash",
        "stop",
        "session-end",
    ];

    fn transcript(meta: &Value) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            format!("{}\n", json!({"type":"session_meta","payload":meta})),
        )
        .unwrap();
        file
    }

    #[test]
    fn agent_fields_mark_spawned_workers_without_reading_transcripts() {
        let payload = json!({"session_id":"root","agent_id":"worker-thread",
            "agent_type":"worker","transcript_path":"/nonexistent/rollout.jsonl"});
        let role = thread_role(&payload);
        assert_eq!(role, ThreadRole::Spawned);
        assert!(!skips_event("bash", &role, || true));
        assert!(!skips_event("pretool", &role, || true));
        for event in ["prompt", "session-start", "stop", "session-end"] {
            assert!(skips_event(event, &role, || false), "{event}");
        }
    }

    #[test]
    fn guardian_transcripts_silence_every_event() {
        let file = transcript(&json!({"id":"guardian-thread","session_id":"root",
            "parent_thread_id":"root","thread_source":"guardian_review",
            "source":{"subagent":{"other":"guardian"}}}));
        let payload = json!({"session_id":"root","transcript_path":file.path(),
            "prompt":"Review the proposed command"});
        let role = thread_role(&payload);
        assert_eq!(role, ThreadRole::Internal("guardian".to_owned()));
        for event in EVENTS {
            assert!(skips_event(event, &role, || false), "{event}");
        }
    }

    #[test]
    fn root_and_unknown_formats_keep_existing_capture() {
        let file = transcript(&json!({"id":"root","session_id":"root","source":"vscode"}));
        let other_format = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(other_format.path(), "{\"id\":\"legacy\"}\n").unwrap();
        for payload in [
            json!({"session_id":"root","transcript_path":file.path()}),
            json!({"session_id":"root","transcript_path":other_format.path()}),
            json!({"session_id":"root","transcript_path":null}),
            json!({"session_id":"root","agent_id":"  "}),
        ] {
            assert_eq!(thread_role(&payload), ThreadRole::Root, "{payload}");
        }
        for event in EVENTS {
            assert!(!skips_event(event, &ThreadRole::Root, || true));
        }
    }

    #[test]
    fn unreadable_transcripts_cannot_replace_an_active_prompt() {
        let directory = tempfile::TempDir::new().unwrap();
        for path in [
            directory.path().join("missing.jsonl"),
            directory.path().to_path_buf(),
        ] {
            let role = thread_role(&json!({"session_id":"root","transcript_path":path}));
            assert_eq!(role, ThreadRole::Unverified, "{}", path.display());
        }
        let role = ThreadRole::Unverified;
        assert!(skips_event("prompt", &role, || true));
        assert!(
            !skips_event("prompt", &role, || false),
            "a first prompt is kept"
        );
        assert!(skips_event("stop", &role, || false));
        for event in ["session-start", "pretool", "bash", "session-end"] {
            assert!(
                !skips_event(event, &role, || panic!("not consulted")),
                "{event}"
            );
        }
    }
}
