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
//!
//! Shell results: Codex 0.155 reports every shell command to `PostToolUse` as
//! `Bash` with `tool_input.command` and the output text as `tool_response`.
//! The payload carries neither the exit code nor the directory the command
//! ran in (`exec_command` takes a `workdir`; the payload `cwd` is the
//! session's). A paginated rollout records both in the `item_completed`
//! event of the command's `CommandExecution` item, whose `id` is the call id
//! that hooks send as `tool_use_id` (see [`command_record`]).

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

pub use crate::usage_capture::CodexThreadKind;

/// How long a post-tool hook waits for the record of a recognized
/// verification. Codex emits a finished command's item before the tool call
/// returns and its rollout writer appends it at once, so the record is
/// usually on disk before the hook starts; the wait covers the writer's lag.
pub const ROLLOUT_WAIT: Duration = Duration::from_millis(1_500);
const ROLLOUT_POLL: Duration = Duration::from_millis(75);
/// Rollout bytes searched first, from the end, where the record usually is.
const ROLLOUT_NEAR_BYTES: u64 = 256 * 1024;
/// Most rollout bytes ever searched. A record carries the command's output,
/// so one record can be megabytes long.
const ROLLOUT_TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// What a Codex rollout recorded about one finished shell command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandRecord {
    /// The command's own exit code. `None` when Codex recorded none: it
    /// writes -1 for a process without an exit code and for a command that
    /// could not run.
    pub exit_code: Option<i32>,
    /// Codex marked the command failed: a non-zero or missing exit code.
    pub failed: bool,
    /// The directory the command ran in.
    pub cwd: PathBuf,
}

/// The rollout record of the shell call that `payload` reports, waiting up
/// to `wait` for Codex to write it.
///
/// Only the rollout of the calling thread is read: a paginated root or
/// spawned thread of this session, as its `session_meta` says, agreeing with
/// the payload's `agent_id`. Internal sub-sessions such as the guardian are
/// never read. `None` when that rollout has no record of the call (yet), or
/// the record names no local directory.
#[must_use]
pub fn command_record(payload: &Value, wait: Duration) -> Option<CommandRecord> {
    let call_id = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| plain_call_id(id))?;
    let path = calling_thread_rollout(payload)?;
    let deadline = Instant::now() + wait;
    let mut rollout = RolloutTail {
        file: File::open(path).ok()?,
        call_id,
        offset: 0,
        aligned: true,
    };
    let length = rollout.file.metadata().ok()?.len();
    for window in [ROLLOUT_NEAR_BYTES, ROLLOUT_TAIL_BYTES] {
        let start = length.saturating_sub(window);
        if let Some(record) = rollout.scan(start, length, start > 0).ok()? {
            return Some(record);
        }
        if start == 0 {
            break;
        }
    }
    while let Some(remaining) = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
    {
        std::thread::sleep(remaining.min(ROLLOUT_POLL));
        let length = rollout.file.metadata().ok()?.len();
        if length < rollout.offset {
            return None; // truncated or replaced
        }
        if length > rollout.offset {
            let start = rollout
                .offset
                .max(length.saturating_sub(ROLLOUT_TAIL_BYTES));
            let skip = start > rollout.offset || !rollout.aligned;
            if let Some(record) = rollout.scan(start, length, skip).ok()? {
                return Some(record);
            }
        }
    }
    None
}

/// Call ids that appear verbatim in the rollout's JSON: no quote, backslash
/// or control character to escape.
fn plain_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.contains(['"', '\\'])
        && !id.chars().any(char::is_control)
}

/// The rollout of the thread that ran the tool, when it can hold records.
fn calling_thread_rollout(payload: &Value) -> Option<PathBuf> {
    let path = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)?;
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty())?;
    let meta = crate::usage_capture::codex_session_meta(&path)?;
    let spawned = payload
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    let calling_thread = match meta.kind {
        CodexThreadKind::Root => !spawned,
        CodexThreadKind::Spawned => spawned,
        CodexThreadKind::Internal(_) => false,
    };
    // Legacy rollouts never record command items.
    (calling_thread
        && meta.session_id.as_deref() == Some(session)
        && meta.history_mode.as_deref() == Some("paginated"))
    .then_some(path)
}

/// Incremental reader of a rollout's newest complete lines.
struct RolloutTail<'a> {
    file: File,
    call_id: &'a str,
    /// End of the last complete line read; the next read starts here.
    offset: u64,
    /// Whether `offset` is known to start a line.
    aligned: bool,
}

impl RolloutTail<'_> {
    /// Search the complete lines in `[start, end)`, newest first, skipping a
    /// partial first line when `skip_partial` is set.
    fn scan(
        &mut self,
        start: u64,
        end: u64,
        skip_partial: bool,
    ) -> std::io::Result<Option<CommandRecord>> {
        self.file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        (&mut self.file)
            .take(end.saturating_sub(start))
            .read_to_end(&mut bytes)?;
        let mut first = 0;
        if skip_partial {
            let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') else {
                // Inside one line longer than the window: unreadable.
                self.offset = start + bytes.len() as u64;
                self.aligned = false;
                return Ok(None);
            };
            first = newline + 1;
        }
        let Some(last) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(None); // a line still being written
        };
        self.offset = start + last as u64 + 1;
        self.aligned = true;
        if last < first {
            return Ok(None); // only the skipped partial line ended here
        }
        let quoted = format!("\"{}\"", self.call_id);
        Ok(bytes[first..last]
            .split(|byte| *byte == b'\n')
            .rev()
            .filter_map(|line| std::str::from_utf8(line).ok())
            .filter(|line| line.contains(&quoted) && line.contains("\"item_completed\""))
            .find_map(|line| command_item(line, self.call_id)))
    }
}

/// The record in one rollout line, when it is this call's command item.
fn command_item(line: &str, call_id: &str) -> Option<CommandRecord> {
    let row: Value = serde_json::from_str(line).ok()?;
    let payload = row.get("payload")?;
    let item = payload.get("item")?;
    if row.get("type").and_then(Value::as_str) != Some("event_msg")
        || payload.get("type").and_then(Value::as_str) != Some("item_completed")
        || item.get("type").and_then(Value::as_str) != Some("CommandExecution")
        || item.get("id").and_then(Value::as_str) != Some(call_id)
    {
        return None;
    }
    let status = item.get("status").and_then(Value::as_str);
    let exit_code = item
        .get("exit_code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
        .filter(|code| match status {
            Some("completed") => *code == 0,
            Some("failed") => *code > 0,
            _ => false,
        });
    Some(CommandRecord {
        exit_code,
        failed: status == Some("failed"),
        cwd: item
            .get("cwd")
            .and_then(Value::as_str)
            .and_then(file_path)?,
    })
}

/// A local absolute path from a `file:` URI (Codex writes the command's
/// directory as one) or a plain absolute path.
fn file_path(text: &str) -> Option<PathBuf> {
    let Some(rest) = text.strip_prefix("file://") else {
        return text.starts_with('/').then(|| PathBuf::from(text));
    };
    let encoded = if rest.starts_with('/') {
        rest
    } else {
        rest.strip_prefix("localhost")
            .filter(|path| path.starts_with('/'))?
    };
    if encoded.contains(['?', '#']) {
        return None; // a query or fragment, not part of a path
    }
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut input = encoded.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let high = char::from(input.next()?).to_digit(16)?;
            let low = char::from(input.next()?).to_digit(16)?;
            bytes.push(u8::try_from(high * 16 + low).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt as _;
        Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
    }
    #[cfg(not(unix))]
    {
        String::from_utf8(bytes).ok().map(PathBuf::from)
    }
}

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

    /// A synthetic paginated rollout: `session_meta`, then the given lines.
    fn rollout(meta: &Value, lines: &[Value]) -> tempfile::NamedTempFile {
        use std::fmt::Write as _;
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut text = String::new();
        for line in std::iter::once(&json!({"type":"session_meta","payload":meta})).chain(lines) {
            writeln!(text, "{line}").unwrap();
        }
        std::fs::write(file.path(), text).unwrap();
        file
    }

    fn root_meta() -> Value {
        json!({"id":"root","session_id":"root","source":"vscode","history_mode":"paginated"})
    }

    /// The `item_completed` line Codex writes for a finished command.
    fn command_line(id: &str, status: &str, exit_code: i64, cwd: &str) -> Value {
        json!({"timestamp":"2026-10-04T00:00:00.000Z","ordinal":7,"type":"event_msg",
            "payload":{"type":"item_completed","thread_id":"root","turn_id":"turn-1",
                "started_at_ms":1,"completed_at_ms":2,
                "item":{"type":"CommandExecution","id":id,"command":["/bin/zsh","-lc","cargo test"],
                    "cwd":cwd,"status":status,"exit_code":exit_code,"source":"unified_exec_startup",
                    "stdout":"synthetic output","stderr":"","aggregated_output":"synthetic output",
                    "formatted_output":"synthetic output","parsed_cmd":[],"process_id":"7",
                    "duration":{"secs":1,"nanos":0}}}})
    }

    fn call(file: &tempfile::NamedTempFile, id: &str) -> Value {
        json!({"session_id":"root","turn_id":"turn-1","tool_name":"Bash","tool_use_id":id,
            "transcript_path":file.path(),"tool_input":{"command":"cargo test"},
            "tool_response":"synthetic output"})
    }

    #[test]
    fn rollout_records_give_exit_codes_and_directories() {
        let file = rollout(
            &root_meta(),
            &[
                command_line("exec-pass", "completed", 0, "file:///work/my%20repo"),
                command_line("exec-fail", "failed", 101, "file://localhost/work/app"),
                command_line("exec-signal", "failed", -1, "file:///work/app"),
                command_line("exec-declined", "declined", -1, "file:///work/app"),
                command_line("exec-odd", "completed", 3, "file:///work/app"),
                command_line("exec-remote", "completed", 0, "file://build-host/work/app"),
                json!({"type":"response_item","payload":{"type":"function_call_output",
                    "call_id":"exec-text","output":"Process exited with code 0"}}),
            ],
        );
        let record = |id: &str| command_record(&call(&file, id), Duration::ZERO);
        assert_eq!(
            record("exec-pass"),
            Some(CommandRecord {
                exit_code: Some(0),
                failed: false,
                cwd: PathBuf::from("/work/my repo"),
            })
        );
        assert_eq!(
            record("exec-fail"),
            Some(CommandRecord {
                exit_code: Some(101),
                failed: true,
                cwd: PathBuf::from("/work/app"),
            })
        );
        // -1 is Codex's placeholder, never an exit code; a status that
        // contradicts the code is not trusted either.
        let signal = record("exec-signal").expect("record");
        assert_eq!((signal.exit_code, signal.failed), (None, true));
        let declined = record("exec-declined").expect("record");
        assert_eq!((declined.exit_code, declined.failed), (None, false));
        assert_eq!(record("exec-odd").expect("record").exit_code, None);
        assert_eq!(record("exec-remote"), None, "no local directory");
        assert_eq!(record("exec-text"), None, "output text is no record");
        assert_eq!(record("exec-missing"), None);
        assert_eq!(record("exec-\"pass"), None);
    }

    #[test]
    fn only_the_calling_threads_paginated_rollout_is_read() {
        let line = command_line("exec-1", "completed", 0, "file:///work/app");
        let worker_meta = json!({"id":"worker","session_id":"root","parent_thread_id":"root",
            "history_mode":"paginated","source":{"subagent":{"thread_spawn":{"depth":1}}}});
        let guardian_meta = json!({"id":"guardian","session_id":"root",
            "parent_thread_id":"root","history_mode":"paginated",
            "thread_source":"guardian_review","source":{"subagent":{"other":"guardian"}}});
        let root = rollout(&root_meta(), std::slice::from_ref(&line));
        let worker = rollout(&worker_meta, std::slice::from_ref(&line));
        let guardian = rollout(&guardian_meta, std::slice::from_ref(&line));
        let legacy = rollout(
            &json!({"id":"root","session_id":"root","source":"vscode","history_mode":"legacy"}),
            std::slice::from_ref(&line),
        );
        let found = |payload: &Value| command_record(payload, Duration::ZERO).is_some();
        assert!(found(&call(&root, "exec-1")));
        let mut spawned = call(&worker, "exec-1");
        spawned["agent_id"] = json!("worker");
        assert!(found(&spawned), "a worker's own rollout");
        assert!(
            !found(&call(&worker, "exec-1")),
            "a root call names a worker rollout"
        );
        let mut misnamed = call(&root, "exec-1");
        misnamed["agent_id"] = json!("worker");
        assert!(!found(&misnamed), "a worker call names the root rollout");
        assert!(!found(&call(&guardian, "exec-1")), "internal sub-sessions");
        assert!(
            !found(&call(&legacy, "exec-1")),
            "legacy rollouts hold no records"
        );
        let mut foreign = call(&root, "exec-1");
        foreign["session_id"] = json!("another-session");
        assert!(!found(&foreign), "another conversation's rollout");
        let mut missing = call(&root, "exec-1");
        missing["transcript_path"] = json!(null);
        assert!(!found(&missing));
    }

    #[test]
    fn a_record_written_while_the_hook_waits_is_found() {
        let file = rollout(&root_meta(), &[json!({"type":"turn_context","payload":{}})]);
        let path = file.path().to_path_buf();
        // Codex may still be writing the line when the hook first reads.
        let line = format!(
            "{}\n",
            command_line("exec-late", "failed", 2, "file:///work/app")
        );
        let (head, tail) = line.split_at(40);
        let writer = std::thread::spawn({
            let (head, tail) = (head.to_owned(), tail.to_owned());
            move || {
                use std::io::Write as _;
                let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
                std::thread::sleep(Duration::from_millis(150));
                file.write_all(head.as_bytes()).unwrap();
                file.flush().unwrap();
                std::thread::sleep(Duration::from_millis(200));
                file.write_all(tail.as_bytes()).unwrap();
            }
        });
        let started = Instant::now();
        let record = command_record(&call(&file, "exec-late"), Duration::from_secs(5));
        writer.join().unwrap();
        assert_eq!(record.map(|record| record.exit_code), Some(Some(2)));
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "found while waiting"
        );
        let started = Instant::now();
        assert_eq!(
            command_record(&call(&file, "exec-never"), Duration::from_millis(300)),
            None
        );
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(300) && waited < Duration::from_secs(3));
    }

    #[test]
    fn long_records_and_long_rollouts_are_searched_from_the_end() {
        // A record holding more output than the first window, behind earlier
        // history larger than the whole search.
        let filler = json!({"type":"response_item","payload":{"type":"message",
            "content":"x".repeat(64 * 1024)}});
        let mut lines = vec![command_line("exec-old", "completed", 0, "file:///work/app")];
        lines.extend(std::iter::repeat_n(filler, 140));
        let mut long = command_line("exec-long", "failed", 1, "file:///work/app");
        long["payload"]["item"]["aggregated_output"] = json!("y".repeat(400 * 1024));
        lines.push(long);
        lines.push(json!({"type":"event_msg","payload":{"type":"token_count"}}));
        let file = rollout(&root_meta(), &lines);
        assert!(std::fs::metadata(file.path()).unwrap().len() > ROLLOUT_TAIL_BYTES);
        let found = command_record(&call(&file, "exec-long"), Duration::ZERO).expect("record");
        assert_eq!(found.exit_code, Some(1));
        assert_eq!(
            command_record(&call(&file, "exec-old"), Duration::ZERO),
            None,
            "older than the searched tail"
        );
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
