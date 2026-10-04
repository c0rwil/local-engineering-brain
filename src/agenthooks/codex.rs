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
//!   prompt-time context.
//! - Internal sub-sessions judge plans, summarize or consolidate. Their hooks
//!   stay silent: no context, no prompt capture, no episode, no usage or
//!   evidence, and they never close the root session's episodes.

use std::path::Path;

use serde_json::Value;

pub use crate::usage_capture::CodexThreadKind;

/// Classify the Codex thread that fired a hook. Payload agent fields are
/// authoritative; otherwise the transcript's `session_meta` decides. Threads
/// without readable metadata are treated as root conversations.
#[must_use]
pub fn thread_kind(payload: &Value) -> CodexThreadKind {
    if payload
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
    {
        return CodexThreadKind::Spawned;
    }
    payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .and_then(|path| crate::usage_capture::codex_session_meta(Path::new(path)))
        .map_or(CodexThreadKind::Root, |meta| meta.kind)
}

/// Whether a hook event from this thread must stay silent (see module docs).
#[must_use]
pub fn skips_event(event: &str, kind: &CodexThreadKind) -> bool {
    match kind {
        CodexThreadKind::Root => false,
        CodexThreadKind::Spawned => !matches!(event, "pretool" | "bash"),
        CodexThreadKind::Internal(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(thread_kind(&payload), CodexThreadKind::Spawned);
        assert!(!skips_event("bash", &CodexThreadKind::Spawned));
        assert!(!skips_event("pretool", &CodexThreadKind::Spawned));
        for event in ["prompt", "session-start", "stop", "session-end"] {
            assert!(skips_event(event, &CodexThreadKind::Spawned), "{event}");
        }
    }

    #[test]
    fn guardian_transcripts_silence_every_event() {
        let file = transcript(&json!({"id":"guardian-thread","session_id":"root",
            "parent_thread_id":"root","thread_source":"guardian_review",
            "source":{"subagent":{"other":"guardian"}}}));
        let payload = json!({"session_id":"root","transcript_path":file.path(),
            "prompt":"Review the proposed command"});
        let kind = thread_kind(&payload);
        assert_eq!(kind, CodexThreadKind::Internal("guardian".to_owned()));
        for event in [
            "session-start",
            "prompt",
            "pretool",
            "bash",
            "stop",
            "session-end",
        ] {
            assert!(skips_event(event, &kind), "{event}");
        }
    }

    #[test]
    fn root_and_unreadable_threads_keep_existing_capture() {
        let file = transcript(&json!({"id":"root","session_id":"root","source":"vscode"}));
        for payload in [
            json!({"session_id":"root","transcript_path":file.path()}),
            json!({"session_id":"root","transcript_path":"/nonexistent/rollout.jsonl"}),
            json!({"session_id":"root","transcript_path":null}),
            json!({"session_id":"root","agent_id":"  "}),
        ] {
            assert_eq!(thread_kind(&payload), CodexThreadKind::Root, "{payload}");
        }
        for event in [
            "session-start",
            "prompt",
            "pretool",
            "bash",
            "stop",
            "session-end",
        ] {
            assert!(!skips_event(event, &CodexThreadKind::Root));
        }
    }
}
