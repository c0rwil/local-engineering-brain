//! Cursor agent hooks: installer documents and the `--cursor` payload adapter.
//!
//! Contract sources: Cursor's hooks reference (<https://cursor.com/docs/hooks>),
//! its third-party hooks page (<https://cursor.com/docs/reference/third-party-hooks>)
//! and its MCP reference (<https://cursor.com/docs/mcp>).
//!
//! - `hooks.json` is `{"version": 1, "hooks": {"<event>": [{"command", "timeout"}]}}`.
//!   User hooks live in `~/.cursor/hooks.json` and run from `~/.cursor/`, so
//!   the adapter takes directories from the payload, never from its own cwd.
//! - Every agent hook receives `conversation_id`, `hook_event_name`,
//!   `cursor_version`, `workspace_roots` and `transcript_path` (the shipped
//!   3.17 client also adds `session_id`, equal to the conversation id); tool
//!   hooks add `tool_name`, `tool_input`, `tool_use_id` and `cwd`.
//! - Engram registers observational events only. `preToolUse`,
//!   `beforeReadFile` and `beforeShellExecution` are permission hooks: Cursor
//!   blocks the action when their output is invalid, which would break the
//!   fail-open rule.
//! - Context reaches the model only through the documented
//!   `additional_context` output of `sessionStart`, `postToolUse` and
//!   `postToolUseFailure`. `beforeSubmitPrompt` documents no context field, so
//!   the prompt hook records the user's intent and repository context follows
//!   the first file or shell tool result of the turn.
//! - Responses never carry `followup_message` (it submits a new user message),
//!   `permission` or `continue: false`.
//! - Cursor also runs Claude Code hooks from `~/.claude/settings.json` by
//!   default and gives them this same input. Engram's Claude entries detect
//!   it with [`is_cursor_payload`] and stay silent, so the native entries
//!   capture each event once.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::error::{Error, Result};

/// Hard ceiling, in bytes, for repository context in one Cursor response.
/// The shipped Cursor 3.17 client discards (does not truncate)
/// `additional_context` above 10,000 characters; the reference documents no
/// limit, so Engram stays well below the observed one. UTF-8 byte length is
/// never smaller than the JavaScript string length Cursor measures.
pub const CONTEXT_BYTES: usize = 8_000;

/// Ceiling, in bytes, for relayed session messages in one Cursor response.
pub const RELAY_BYTES: usize = 1_000;

/// Key of Engram's server entry in Cursor's `mcp.json`.
pub const MCP_SERVER_NAME: &str = "engram";

/// Cursor events Engram handles, the shared handler each one runs, and its
/// timeout in seconds. `postToolUse` covers every tool, including edits
/// (`Write`), so `afterFileEdit` would count edits twice.
pub const HOOK_EVENTS: [(&str, &str, u32); 6] = [
    ("sessionStart", "session-start", 15),
    ("beforeSubmitPrompt", "prompt", 10),
    ("postToolUse", "bash", 30),
    ("postToolUseFailure", "bash", 30),
    ("stop", "stop", 15),
    ("sessionEnd", "session-end", 15),
];

/// Engram's entries for Cursor's `hooks.json`, as a complete version-1 document.
#[must_use]
pub fn cursor_hooks_json(binary: &str) -> Value {
    let hooks: Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|(event, handler, timeout)| {
            (
                (*event).to_owned(),
                json!([{
                    "command": format!("{binary} hook {handler} --cursor"),
                    "timeout": timeout,
                }]),
            )
        })
        .collect();
    json!({"version": 1, "hooks": hooks})
}

/// Merge Engram's hooks into a Cursor `hooks.json` document. Other events,
/// foreign entries and top-level keys survive; earlier Engram entries are
/// replaced, so repeated installs produce the same document.
///
/// # Errors
/// Rejects documents that are not Cursor's version-1 shape.
pub fn merge_cursor_hooks(mut document: Value, binary: &str) -> Result<Value> {
    remove_engram_entries(&mut document)?;
    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor hooks.json must be a JSON object".to_owned()))?;
    root.entry("version").or_insert_with(|| json!(1));
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor hooks.json `hooks` must be an object".to_owned()))?;
    let additions = cursor_hooks_json(binary);
    for (event, entries) in additions["hooks"].as_object().into_iter().flatten() {
        let slot = hooks
            .entry(event.clone())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| Error::Config(format!("Cursor hook event {event} must be an array")))?;
        slot.extend(entries.as_array().into_iter().flatten().cloned());
    }
    Ok(document)
}

/// Remove only Engram's entries from a Cursor `hooks.json` document and
/// report how many were removed. Events left empty by the removal are dropped.
///
/// # Errors
/// Rejects documents that are not Cursor's version-1 shape.
pub fn remove_cursor_hooks(mut document: Value) -> Result<(Value, usize)> {
    let removed = remove_engram_entries(&mut document)?;
    Ok((document, removed))
}

fn remove_engram_entries(document: &mut Value) -> Result<usize> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor hooks.json must be a JSON object".to_owned()))?;
    if let Some(version) = root.get("version") {
        if version.as_u64() != Some(1) {
            return Err(Error::Config(format!(
                "Cursor hooks.json version {version} is not supported (expected 1)"
            )));
        }
    }
    let Some(hooks) = root.get_mut("hooks") else {
        return Ok(0);
    };
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor hooks.json `hooks` must be an object".to_owned()))?;
    // Validate everything before changing anything.
    for (event, entries) in hooks.iter() {
        let valid = entries
            .as_array()
            .is_some_and(|entries| entries.iter().all(Value::is_object));
        if !valid {
            return Err(Error::Config(format!(
                "Cursor hook event {event} must be an array of objects"
            )));
        }
    }
    let mut removed = 0;
    let mut emptied = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(entries) = entries.as_array_mut() else {
            continue;
        };
        let before = entries.len();
        entries.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(super::is_engram_hook)
        });
        removed += before - entries.len();
        if before > 0 && entries.is_empty() {
            emptied.push(event.clone());
        }
    }
    for event in emptied {
        hooks.remove(&event);
    }
    Ok(removed)
}

/// Outcome of registering Engram's MCP server in Cursor's `mcp.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpRegistration {
    Added,
    /// Engram's own entry existed; only its command path was refreshed.
    Refreshed,
    /// A server under another name already launches Engram's MCP binary.
    AlreadyRegistered(String),
}

/// Engram's stdio server entry for Cursor's `mcp.json`.
#[must_use]
pub fn mcp_server_entry(command: &str) -> Value {
    json!({"type": "stdio", "command": command, "args": []})
}

fn launches_engram_mcp(server: &Value) -> bool {
    server
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| Path::new(command).file_name())
        .and_then(OsStr::to_str)
        .is_some_and(|name| matches!(name, "engram-mcp" | "leb-mcp"))
}

/// Register Engram's MCP server. Other servers and keys survive. An existing
/// working registration under another name is kept rather than duplicated.
///
/// # Errors
/// Rejects malformed documents, and an `engram` entry that launches something
/// other than Engram's MCP binary (it is left unchanged).
pub fn merge_cursor_mcp(mut document: Value, command: &str) -> Result<(Value, McpRegistration)> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor mcp.json must be a JSON object".to_owned()))?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            Error::Config("Cursor mcp.json `mcpServers` must be an object".to_owned())
        })?;
    let outcome = match servers.get(MCP_SERVER_NAME) {
        Some(existing) if launches_engram_mcp(existing) => McpRegistration::Refreshed,
        Some(_) => {
            return Err(Error::Config(format!(
                "Cursor mcp.json already has an `{MCP_SERVER_NAME}` server that does not launch \
                 engram-mcp; it was left unchanged"
            )));
        }
        None => match servers
            .iter()
            .find(|(_, server)| launches_engram_mcp(server))
            .map(|(name, _)| name.clone())
        {
            Some(name) => McpRegistration::AlreadyRegistered(name),
            None => McpRegistration::Added,
        },
    };
    match outcome {
        McpRegistration::Added => {
            servers.insert(MCP_SERVER_NAME.to_owned(), mcp_server_entry(command));
        }
        McpRegistration::Refreshed => {
            // Keep any operator additions (env, args); refresh the binary path.
            if let Some(entry) = servers
                .get_mut(MCP_SERVER_NAME)
                .and_then(Value::as_object_mut)
            {
                entry.insert("command".to_owned(), json!(command));
            }
        }
        McpRegistration::AlreadyRegistered(_) => {}
    }
    Ok((document, outcome))
}

/// Remove Engram's `engram` server entry, only when it launches Engram's MCP
/// binary. Returns whether an entry was removed.
///
/// # Errors
/// Rejects malformed documents.
pub fn remove_cursor_mcp(mut document: Value) -> Result<(Value, bool)> {
    let root = document
        .as_object_mut()
        .ok_or_else(|| Error::Config("Cursor mcp.json must be a JSON object".to_owned()))?;
    let Some(servers) = root.get_mut("mcpServers") else {
        return Ok((document, false));
    };
    let servers = servers.as_object_mut().ok_or_else(|| {
        Error::Config("Cursor mcp.json `mcpServers` must be an object".to_owned())
    })?;
    let ours = servers
        .get(MCP_SERVER_NAME)
        .is_some_and(launches_engram_mcp);
    if ours {
        servers.remove(MCP_SERVER_NAME);
    }
    Ok((document, ours))
}

/// Cursor's user configuration directory: an explicit path, then
/// `ENGRAM_CURSOR_HOME`, then `~/.cursor`.
#[must_use]
pub fn cursor_home(explicit: Option<&Path>, env: Option<&OsStr>, home: &Path) -> PathBuf {
    explicit
        .map(Path::to_path_buf)
        .or_else(|| env.filter(|value| !value.is_empty()).map(PathBuf::from))
        .unwrap_or_else(|| home.join(".cursor"))
}

/// Read a JSON configuration document. A missing or blank file yields
/// `default`.
///
/// # Errors
/// Returns read errors and invalid JSON; nothing is modified.
pub fn read_document(path: &Path, default: Value) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Ok(default),
        Ok(content) => serde_json::from_str(&content).map_err(|error| {
            Error::Config(format!("{} is not valid JSON: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(error) => Err(Error::Config(format!("read {}: {error}", path.display()))),
    }
}

/// Atomically replace a JSON configuration document. A symlinked file is
/// written at its target, and an existing file keeps its permissions.
///
/// # Errors
/// Returns filesystem errors; on failure the previous file stays intact.
pub fn write_document(path: &Path, document: &Value) -> Result<()> {
    use std::io::Write as _;
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let directory = target
        .parent()
        .ok_or_else(|| Error::Config(format!("{} has no parent directory", target.display())))?;
    std::fs::create_dir_all(directory)
        .map_err(|error| Error::Config(format!("create {}: {error}", directory.display())))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| Error::Config(error.to_string()))?;
    let mut text = serde_json::to_string_pretty(document)?;
    text.push('\n');
    file.write_all(text.as_bytes())
        .map_err(|error| Error::Config(error.to_string()))?;
    if let Ok(metadata) = std::fs::metadata(&target) {
        file.as_file()
            .set_permissions(metadata.permissions())
            .map_err(|error| Error::Config(error.to_string()))?;
    }
    file.persist(&target)
        .map_err(|error| Error::Config(error.to_string()))?;
    Ok(())
}

/// True for input Cursor sends to any hook, including Claude Code hooks it
/// imports.
#[must_use]
pub fn is_cursor_payload(payload: &Value) -> bool {
    payload
        .get("cursor_version")
        .and_then(Value::as_str)
        .is_some_and(|version| !version.trim().is_empty())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
}

/// Translate Cursor hook input into the shape the shared handlers read.
///
/// - `session_id` is Cursor's session id or, failing that, `conversation_id`.
/// - `cwd` is the tool's directory, else the first workspace root.
/// - The `Shell` tool becomes `Bash`, which the verification capture
///   recognizes; `tool_input.working_directory` becomes `cwd`.
/// - `tool_output` (a JSON-encoded string) becomes `tool_response`. A failure
///   becomes an error without an exit code, so it records no verification.
#[must_use]
pub fn normalize_payload(raw: &Value) -> Value {
    let mut payload = Map::new();
    if let Some(session) = text(raw, "session_id").or_else(|| text(raw, "conversation_id")) {
        payload.insert("session_id".to_owned(), json!(session));
    }
    let cwd = text(raw, "cwd").or_else(|| {
        raw.get("workspace_roots")
            .and_then(Value::as_array)
            .and_then(|roots| {
                roots
                    .iter()
                    .filter_map(Value::as_str)
                    .find(|root| !root.trim().is_empty())
            })
    });
    if let Some(cwd) = cwd {
        payload.insert("cwd".to_owned(), json!(cwd));
    }
    for key in ["prompt", "tool_use_id", "transcript_path"] {
        if let Some(value) = text(raw, key) {
            payload.insert(key.to_owned(), json!(value));
        }
    }
    if let Some(name) = text(raw, "tool_name") {
        let name = if name == "Shell" { "Bash" } else { name };
        payload.insert("tool_name".to_owned(), json!(name));
    }
    if let Some(input) = raw.get("tool_input") {
        payload.insert("tool_input".to_owned(), tool_input(input));
    }
    if let Some(response) = tool_response(raw) {
        payload.insert("tool_response".to_owned(), response);
    }
    Value::Object(payload)
}

fn tool_input(input: &Value) -> Value {
    // MCP tool arguments arrive as a JSON-encoded string.
    let mut input = match input {
        Value::String(encoded) => serde_json::from_str(encoded).unwrap_or_else(|_| json!({})),
        other => other.clone(),
    };
    if let Some(fields) = input.as_object_mut() {
        // The reference example names the shell directory `working_directory`;
        // the shipped client sends `cwd`. Empty means the workspace root.
        if let Some(directory) = fields.remove("working_directory") {
            fields.entry("cwd").or_insert(directory);
        }
        if fields
            .get("cwd")
            .and_then(Value::as_str)
            .is_some_and(|cwd| cwd.trim().is_empty())
        {
            fields.remove("cwd");
        }
        // `Write` sends the whole file; capture never needs file contents.
        fields.remove("content");
    }
    input
}

fn tool_response(raw: &Value) -> Option<Value> {
    if let Some(output) = raw.get("tool_output") {
        return Some(match output {
            Value::String(encoded) => match serde_json::from_str::<Value>(encoded) {
                Ok(parsed @ Value::Object(_)) => parsed,
                _ => json!({"output": encoded}),
            },
            Value::Object(_) => output.clone(),
            _ => json!({}),
        });
    }
    raw.get("error_message").map(|error| {
        json!({
            "error": error,
            "failure_type": raw.get("failure_type"),
            "is_interrupt": raw.get("is_interrupt"),
        })
    })
}

/// A recognized verification command that Cursor reported through
/// `postToolUseFailure`. The shipped Cursor 3.17 shell reports a non-zero exit
/// this way, without the exit code, so no evidence can be recorded and the
/// latest verification result is unknown.
#[must_use]
pub fn failed_verification(raw: &Value) -> bool {
    let failed = text(raw, "hook_event_name").map_or_else(
        || raw.get("error_message").is_some(),
        |event| event == "postToolUseFailure",
    );
    let command = raw
        .get("tool_input")
        .and_then(|input| input.get("command"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    // The shared handler strips a leading `cd DIR &&` the same way.
    let command = command
        .trim()
        .strip_prefix("cd ")
        .and_then(|rest| rest.split_once("&&"))
        .map_or(command, |(_, rest)| rest.trim());
    failed
        && text(raw, "tool_name") == Some("Shell")
        && super::detect_verification(command).is_some()
}

/// Tools after which the adapter delivers repository context: the file and
/// shell tools that the Claude Code and Codex pre-tool hooks cover.
#[must_use]
pub fn delivers_context(payload: &Value) -> bool {
    payload
        .get("tool_name")
        .and_then(Value::as_str)
        .is_some_and(|name| matches!(name, "Read" | "Write" | "Delete" | "Bash"))
}

/// The single JSON object a Cursor hook prints for a shared handler event.
/// Only documented output fields are used.
#[must_use]
pub fn response(event: &str, context: Option<&str>) -> Value {
    let context = context.filter(|text| !text.trim().is_empty());
    match (event, context) {
        ("prompt", _) => json!({"continue": true}),
        ("session-start" | "bash", Some(text)) => json!({"additional_context": text}),
        _ => json!({}),
    }
}

/// Render drained relay messages within [`RELAY_BYTES`].
#[must_use]
pub fn relay_block(messages: &[String]) -> String {
    if messages.is_empty() {
        return String::new();
    }
    let mut block = String::from("# Messages from other sessions (engram relay)\n");
    for message in messages {
        let _ = writeln!(block, "- {message}");
    }
    if block.len() > RELAY_BYTES {
        let mut cut = RELAY_BYTES - "…\n".len();
        while !block.is_char_boundary(cut) {
            cut -= 1;
        }
        block.truncate(cut);
        block.push_str("…\n");
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    const BINARY: &str = "'/opt/engram/bin/engram'";

    fn commands(document: &Value) -> Vec<String> {
        document["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|entries| entries.as_array().unwrap().iter())
            .filter_map(|entry| entry["command"].as_str().map(str::to_owned))
            .collect()
    }

    #[test]
    fn generated_hooks_cover_observational_events_only() {
        let document = cursor_hooks_json(BINARY);
        assert_eq!(document["version"], 1);
        let hooks = document["hooks"].as_object().unwrap();
        for permission_hook in ["preToolUse", "beforeReadFile", "beforeShellExecution"] {
            assert!(!hooks.contains_key(permission_hook), "{permission_hook}");
        }
        assert_eq!(
            hooks["beforeSubmitPrompt"][0]["command"],
            "'/opt/engram/bin/engram' hook prompt --cursor"
        );
        assert_eq!(hooks["postToolUse"][0]["timeout"], 30);
        assert!(hooks["postToolUse"][0].get("matcher").is_none());
        assert!(
            commands(&document)
                .iter()
                .all(|command| super::super::is_engram_hook(command))
        );
    }

    #[test]
    fn merge_preserves_foreign_entries_replaces_engram_and_is_idempotent() {
        let original = json!({"version":1, "hooks":{
            "afterFileEdit":[{"command":"./hooks/format.sh"}],
            "postToolUse":[
                {"command":"leb hook bash --cursor","timeout":5},
                {"command":"./hooks/audit.sh","matcher":"Shell"},
                {"command":"echo \"/opt/engram hook bash --cursor\""}
            ],
            "stop":[{"type":"prompt","prompt":"Is the task complete?"}]
        }, "comment":"keep me"});
        let merged = merge_cursor_hooks(original.clone(), BINARY).unwrap();
        assert_eq!(merged["comment"], "keep me");
        assert_eq!(
            merged["hooks"]["afterFileEdit"],
            original["hooks"]["afterFileEdit"]
        );
        let post = merged["hooks"]["postToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 3);
        assert_eq!(post[0]["command"], "./hooks/audit.sh");
        assert_eq!(
            post[1]["command"],
            "echo \"/opt/engram hook bash --cursor\""
        );
        assert_eq!(
            post[2]["command"],
            "'/opt/engram/bin/engram' hook bash --cursor"
        );
        assert_eq!(merged["hooks"]["stop"].as_array().unwrap().len(), 2);
        assert_eq!(merged["hooks"]["stop"][0]["type"], "prompt");
        assert!(!commands(&merged).iter().any(|c| c.starts_with("leb ")));
        assert_eq!(merge_cursor_hooks(merged.clone(), BINARY).unwrap(), merged);
        assert_eq!(
            merge_cursor_hooks(json!({}), BINARY).unwrap(),
            cursor_hooks_json(BINARY)
        );
    }

    #[test]
    fn uninstall_removes_only_engram_entries() {
        let installed = merge_cursor_hooks(
            json!({"version":1,"hooks":{"postToolUse":[{"command":"./audit.sh"}]}}),
            BINARY,
        )
        .unwrap();
        let (cleaned, removed) = remove_cursor_hooks(installed).unwrap();
        assert_eq!(removed, HOOK_EVENTS.len());
        assert_eq!(
            cleaned,
            json!({"version":1,"hooks":{"postToolUse":[{"command":"./audit.sh"}]}})
        );
        let (unchanged, removed) = remove_cursor_hooks(cleaned.clone()).unwrap();
        assert_eq!((unchanged, removed), (cleaned, 0));
    }

    #[test]
    fn malformed_or_unknown_versions_are_rejected() {
        for document in [
            json!([]),
            json!({"version":2,"hooks":{}}),
            json!({"version":1,"hooks":[]}),
            json!({"version":1,"hooks":{"stop":{"command":"x"}}}),
            json!({"version":1,"hooks":{"stop":["./script.sh"]}}),
        ] {
            assert!(
                merge_cursor_hooks(document.clone(), BINARY).is_err(),
                "{document}"
            );
            assert!(remove_cursor_hooks(document.clone()).is_err(), "{document}");
        }
    }

    #[test]
    fn mcp_registration_adds_refreshes_and_never_clobbers() {
        let foreign = json!({"mcpServers":{"linear":{"url":"https://mcp.example.test/sse"}}});
        let (added, outcome) = merge_cursor_mcp(foreign.clone(), "/opt/bin/engram-mcp").unwrap();
        assert_eq!(outcome, McpRegistration::Added);
        assert_eq!(
            added["mcpServers"]["linear"],
            foreign["mcpServers"]["linear"]
        );
        assert_eq!(
            added["mcpServers"]["engram"],
            json!({"type":"stdio","command":"/opt/bin/engram-mcp","args":[]})
        );
        let mut customized = added.clone();
        customized["mcpServers"]["engram"]["env"] = json!({"ENGRAM_LOG":"debug"});
        let (refreshed, outcome) = merge_cursor_mcp(customized, "/new/bin/engram-mcp").unwrap();
        assert_eq!(outcome, McpRegistration::Refreshed);
        assert_eq!(
            refreshed["mcpServers"]["engram"]["command"],
            "/new/bin/engram-mcp"
        );
        assert_eq!(
            refreshed["mcpServers"]["engram"]["env"]["ENGRAM_LOG"],
            "debug"
        );
        let legacy = json!({"mcpServers":{"leb":{"command":"/home/u/.local/bin/leb-mcp"}}});
        let (kept, outcome) = merge_cursor_mcp(legacy.clone(), "/opt/bin/engram-mcp").unwrap();
        assert_eq!(
            outcome,
            McpRegistration::AlreadyRegistered("leb".to_owned())
        );
        assert_eq!(kept, legacy);
        let taken = json!({"mcpServers":{"engram":{"command":"npx","args":["other"]}}});
        assert!(merge_cursor_mcp(taken.clone(), "/opt/bin/engram-mcp").is_err());
        let (untouched, removed) = remove_cursor_mcp(taken.clone()).unwrap();
        assert_eq!((untouched, removed), (taken, false));
        let (cleaned, removed) = remove_cursor_mcp(refreshed).unwrap();
        assert!(removed);
        assert_eq!(
            cleaned["mcpServers"]["linear"],
            foreign["mcpServers"]["linear"]
        );
        assert!(cleaned["mcpServers"].get("engram").is_none());
        assert!(merge_cursor_mcp(json!({"mcpServers":[]}), "/x/engram-mcp").is_err());
    }

    #[test]
    fn cursor_home_prefers_flag_then_environment_then_home() {
        let home = Path::new("/home/user");
        assert_eq!(
            cursor_home(
                Some(Path::new("/tmp/flag")),
                Some(OsStr::new("/tmp/env")),
                home
            ),
            Path::new("/tmp/flag")
        );
        assert_eq!(
            cursor_home(None, Some(OsStr::new("/tmp/env")), home),
            Path::new("/tmp/env")
        );
        assert_eq!(
            cursor_home(None, Some(OsStr::new("")), home),
            Path::new("/home/user/.cursor")
        );
    }

    #[cfg(unix)]
    #[test]
    fn documents_round_trip_atomically_through_symlinks() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("hooks.json");
        assert_eq!(
            read_document(&path, json!({"default":true})).unwrap()["default"],
            true
        );
        std::fs::write(&path, "  \n").unwrap();
        assert_eq!(read_document(&path, json!({})).unwrap(), json!({}));
        std::fs::write(&path, "{broken").unwrap();
        assert!(read_document(&path, json!({})).is_err());
        let real = temp.path().join("dotfiles-hooks.json");
        std::fs::write(&real, "{}").unwrap();
        let link = temp.path().join("linked.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_document(&link, &json!({"version":1})).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(read_document(&real, json!({})).unwrap()["version"], 1);
    }

    #[test]
    fn payloads_translate_cursor_fields_for_shared_handlers() {
        let shell = normalize_payload(&json!({
            "conversation_id":"conv-1", "generation_id":"gen-1",
            "hook_event_name":"postToolUse", "cursor_version":"3.17.19",
            "workspace_roots":["/work/repo"], "cwd":"",
            "tool_name":"Shell", "tool_use_id":"call-1",
            "tool_input":{"command":"cargo test","working_directory":"/work/repo/sub"},
            "tool_output":"{\"exitCode\":0,\"output\":\"test result: ok\"}",
            "duration":1200
        }));
        assert_eq!(shell["session_id"], "conv-1");
        assert_eq!(shell["cwd"], "/work/repo");
        assert_eq!(shell["tool_name"], "Bash");
        assert_eq!(shell["tool_input"]["cwd"], "/work/repo/sub");
        assert!(shell["tool_input"].get("working_directory").is_none());
        assert_eq!(
            super::super::extract_exit_code(&shell["tool_response"]),
            Some(0)
        );
        assert!(shell.get("cursor_version").is_none());

        let write = normalize_payload(&json!({
            "session_id":"conv-2", "conversation_id":"conv-2-other", "cwd":"/work/repo",
            "tool_name":"Write", "tool_use_id":"call-2",
            "tool_input":{"file_path":"/work/repo/lib.rs","content":"fn main() {}"},
            "tool_output":"not json"
        }));
        assert_eq!(write["session_id"], "conv-2");
        assert_eq!(
            write["tool_input"],
            json!({"file_path":"/work/repo/lib.rs"})
        );
        assert_eq!(write["tool_response"], json!({"output":"not json"}));

        let failure = normalize_payload(&json!({
            "conversation_id":"conv-3", "workspace_roots":["", "/work/other"],
            "tool_name":"Shell", "tool_use_id":"call-3",
            "tool_input":{"command":"cargo test","cwd":""},
            "error_message":"Command failed", "failure_type":"error", "is_interrupt":false
        }));
        assert_eq!(failure["cwd"], "/work/other");
        assert!(failure["tool_input"].get("cwd").is_none());
        assert_eq!(
            super::super::extract_exit_code(&failure["tool_response"]),
            None
        );

        let mcp = normalize_payload(&json!({"conversation_id":"c","tool_name":"MCP:search",
            "tool_input":"{\"query\":\"x\"}"}));
        assert_eq!(mcp["tool_input"], json!({"query":"x"}));
        assert!(delivers_context(&shell) && delivers_context(&write));
        assert!(!delivers_context(&mcp));

        let prompt = normalize_payload(&json!({"conversation_id":"c","prompt":"Fix it",
            "attachments":[], "workspace_roots":["/work/repo"]}));
        assert_eq!(
            prompt,
            json!({"session_id":"c","cwd":"/work/repo","prompt":"Fix it"})
        );
    }

    #[test]
    fn responses_use_documented_fields_only() {
        assert_eq!(
            response("prompt", Some("ignored")),
            json!({"continue":true})
        );
        assert_eq!(response("prompt", None), json!({"continue":true}));
        assert_eq!(
            response("session-start", Some("brief")),
            json!({"additional_context":"brief"})
        );
        assert_eq!(
            response("bash", Some("ctx")),
            json!({"additional_context":"ctx"})
        );
        assert_eq!(response("bash", Some("  ")), json!({}));
        for event in ["stop", "session-end", "pretool", "unknown"] {
            assert_eq!(response(event, Some("ctx")), json!({}), "{event}");
        }
        assert!(is_cursor_payload(&json!({"cursor_version":"3.17.19"})));
        assert!(!is_cursor_payload(&json!({"session_id":"claude"})));
    }

    #[test]
    fn failed_verifications_are_recognized_without_exit_codes() {
        let failure = |command: &str| {
            json!({"hook_event_name":"postToolUseFailure","tool_name":"Shell",
                "tool_input":{"command":command,"cwd":""},
                "error_message":"test result: FAILED","failure_type":"error"})
        };
        assert!(failed_verification(&failure("cargo test")));
        assert!(failed_verification(&failure(
            "cd crates/core && cargo test --lib"
        )));
        assert!(!failed_verification(&failure("git status")));
        let mut read = failure("cargo test");
        read["tool_name"] = json!("Read");
        assert!(!failed_verification(&read));
        let success = json!({"hook_event_name":"postToolUse","tool_name":"Shell",
            "tool_input":{"command":"cargo test"},"tool_output":"{\"exitCode\":0}"});
        assert!(!failed_verification(&success));
        let unlabeled = json!({"tool_name":"Shell","tool_input":{"command":"pytest"},
            "error_message":"failed"});
        assert!(failed_verification(&unlabeled));
    }

    #[test]
    fn relay_blocks_stay_within_budget_at_character_boundaries() {
        assert!(relay_block(&[]).is_empty());
        let short = relay_block(&["from operator: check tests".to_owned()]);
        assert!(short.ends_with("- from operator: check tests\n"));
        let long = relay_block(&["é".repeat(2_000)]);
        assert!(long.len() <= RELAY_BYTES);
        assert!(long.ends_with("…\n"));
    }
}
