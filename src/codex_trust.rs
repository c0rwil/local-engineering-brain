//! Read-only inspection of Codex trust for Engram's Codex hooks.
//!
//! Codex runs a hook handler only after the user reviewed and trusted it
//! (`/hooks` in Codex). It records trust in `$CODEX_HOME/config.toml` as
//! `[hooks.state."<key>"] trusted_hash = "sha256:<hex>"`, and
//! `enabled = false` in the same table turns a handler off. For handlers in
//! the user-level `$CODEX_HOME/hooks.json`:
//!
//! - the key is `<canonical $CODEX_HOME>/hooks.json:<event>:<group>:<handler>`,
//!   with the event in `snake_case` and zero-based group and handler positions;
//! - the hash is the lowercase hex SHA-256 of a compact JSON object with
//!   lexicographically sorted keys: `event_name`, `hooks` (a one-element
//!   array holding the handler) and `matcher` (omitted when absent or empty).
//!   The handler carries `type`, `command`, `timeout` (seconds, default 600),
//!   `async` (default false) and, when set, `statusMessage` and
//!   `additionalContextLimit`. `commandWindows` is not part of the hash on
//!   macOS.
//!
//! Verified on 2026-10-04 against Codex CLI 0.155.0-alpha.2.6 (bundled with
//! the `ChatGPT` desktop app): its app-server `hooks/list` reported these keys
//! and hashes for synthetic hooks, `trustStatus` followed the stored hash
//! (`trusted`, `modified`, `untrusted`), and the rule reproduced the trusted
//! hashes Codex had stored for plugin hooks. Codex stays the authority: if a
//! later version changes the rule, trusted hooks show up here as changed and
//! `/hooks` shows the truth.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::Result;

/// Timeout Codex applies to a command hook that sets none.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Directory entries examined when counting recent Codex session files.
const MAX_SESSION_ENTRIES: usize = 50_000;

/// Whether Codex will run one handler.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookTrust {
    /// Trusted with the current definition; Codex runs it.
    Trusted,
    /// Never reviewed; Codex does not run it.
    Untrusted,
    /// Trusted once, but the definition changed since; Codex does not run it.
    Modified,
    /// Turned off in `/hooks` (`enabled = false`).
    Disabled,
}

/// Trust state of one Engram handler in `hooks.json`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct HookTrustEntry {
    /// The `hooks.state` key Codex uses for this handler.
    pub key: String,
    /// Codex event name in `snake_case`, for example `session_start`.
    pub event: String,
    /// The `trusted_hash` value that trusts the current definition, when the
    /// handler is a command hook this rule covers.
    pub current_hash: Option<String>,
    pub trust: HookTrust,
}

/// Trust state of every Engram handler in one Codex home.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CodexHookTrust {
    pub hooks_file: PathBuf,
    pub config_file: PathBuf,
    pub hooks: Vec<HookTrustEntry>,
    /// `[features] hooks = false`: Codex runs no hooks at all.
    pub hooks_feature_disabled: bool,
}

impl CodexHookTrust {
    /// Handlers that Codex will not run.
    pub fn inactive(&self) -> impl Iterator<Item = &HookTrustEntry> {
        self.hooks
            .iter()
            .filter(move |hook| self.hooks_feature_disabled || hook.trust != HookTrust::Trusted)
    }
}

/// The Codex home Engram installs into: `$CODEX_HOME` when set and
/// non-empty, otherwise `~/.codex`.
#[must_use]
pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))
}

/// Inspect Engram's handlers in `<codex_home>/hooks.json` against the trust
/// Codex recorded in `<codex_home>/config.toml`. Returns `Ok(None)` when no
/// `hooks.json` exists. Nothing is written.
///
/// # Errors
///
/// Returns an error when either file exists but cannot be read or parsed.
pub fn inspect(codex_home: &Path) -> Result<Option<CodexHookTrust>> {
    let home = fs::canonicalize(codex_home).unwrap_or_else(|_| codex_home.to_path_buf());
    let hooks_file = home.join("hooks.json");
    let hooks_text = match fs::read_to_string(&hooks_file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(crate::error::io_error(&hooks_file, source)),
    };
    let document: Value = serde_json::from_str(&hooks_text)?;
    let config_file = home.join("config.toml");
    let config = match fs::read_to_string(&config_file) {
        Ok(text) => toml::from_str::<toml::Table>(&text)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(source) => return Err(crate::error::io_error(&config_file, source)),
    };
    let states = config
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(toml::Value::as_table);
    let hooks_feature_disabled = config
        .get("features")
        .and_then(|features| features.get("hooks"))
        .and_then(toml::Value::as_bool)
        == Some(false);

    let mut hooks = Vec::new();
    let events = document
        .get("hooks")
        .and_then(Value::as_object)
        .into_iter()
        .flatten();
    for (event, groups) in events {
        let event = event_snake_case(event);
        for (group_index, group) in groups.as_array().into_iter().flatten().enumerate() {
            let matcher = group.get("matcher").and_then(Value::as_str);
            let handlers = group.get("hooks").and_then(Value::as_array);
            for (handler_index, handler) in handlers.into_iter().flatten().enumerate() {
                let is_engram = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(crate::agenthooks::is_engram_hook);
                if !is_engram {
                    continue;
                }
                let key = hook_key(&hooks_file, &event, group_index, handler_index);
                let state = states.and_then(|states| states.get(&key));
                let enabled = state
                    .and_then(|state| state.get("enabled"))
                    .and_then(toml::Value::as_bool)
                    != Some(false);
                let trusted_hash = state
                    .and_then(|state| state.get("trusted_hash"))
                    .and_then(toml::Value::as_str);
                let current_hash = trust_hash(&event, matcher, handler);
                let trust = if enabled {
                    match (trusted_hash, current_hash.as_deref()) {
                        (None, _) => HookTrust::Untrusted,
                        (Some(stored), Some(current)) if stored == current => HookTrust::Trusted,
                        (Some(_), _) => HookTrust::Modified,
                    }
                } else {
                    HookTrust::Disabled
                };
                hooks.push(HookTrustEntry {
                    key,
                    event: event.clone(),
                    current_hash,
                    trust,
                });
            }
        }
    }
    hooks.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(Some(CodexHookTrust {
        hooks_file,
        config_file,
        hooks,
        hooks_feature_disabled,
    }))
}

/// The `hooks.state` key Codex uses for one handler of the user-level
/// `hooks.json`.
#[must_use]
pub fn hook_key(hooks_file: &Path, event: &str, group: usize, handler: usize) -> String {
    format!("{}:{event}:{group}:{handler}", hooks_file.display())
}

/// The `sha256:<hex>` trust hash Codex computes for a command handler, or
/// `None` when the handler is not a command hook this rule covers.
#[must_use]
pub fn trust_hash(event: &str, matcher: Option<&str>, handler: &Value) -> Option<String> {
    let handler = handler.as_object()?;
    let kind = handler.get("type").and_then(Value::as_str)?;
    if kind != "command" {
        return None;
    }
    let mut identity = serde_json::Map::new();
    identity.insert("type".to_owned(), Value::from(kind));
    identity.insert("command".to_owned(), handler.get("command")?.clone());
    let timeout = match handler.get("timeout") {
        None | Some(Value::Null) => DEFAULT_TIMEOUT_SECS,
        Some(value) => value.as_u64()?,
    };
    identity.insert("timeout".to_owned(), Value::from(timeout));
    let asynchronous = handler
        .get("async")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    identity.insert("async".to_owned(), Value::from(asynchronous));
    for optional in ["statusMessage", "additionalContextLimit"] {
        if let Some(value) = handler.get(optional).filter(|value| !value.is_null()) {
            identity.insert(optional.to_owned(), value.clone());
        }
    }
    let mut group = serde_json::Map::new();
    group.insert("event_name".to_owned(), Value::from(event));
    if let Some(matcher) = matcher.filter(|matcher| !matcher.is_empty()) {
        group.insert("matcher".to_owned(), Value::from(matcher));
    }
    group.insert(
        "hooks".to_owned(),
        Value::Array(vec![Value::Object(identity)]),
    );
    let mut canonical = String::new();
    write_canonical_json(&Value::Object(group), &mut canonical);
    let digest = Sha256::digest(canonical.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Some(format!("sha256:{hex}"))
}

/// Count Codex session files modified within `window` (bounded walk of
/// `<codex_home>/sessions`).
#[must_use]
pub fn recent_session_files(codex_home: &Path, window: Duration) -> usize {
    let cutoff = SystemTime::now()
        .checked_sub(window)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut pending = vec![(codex_home.join("sessions"), 0_u8)];
    let mut examined = 0_usize;
    let mut recent = 0_usize;
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            examined += 1;
            if examined > MAX_SESSION_ENTRIES {
                return recent;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                if depth < 4 {
                    pending.push((entry.path(), depth + 1));
                }
            } else if metadata.modified().is_ok_and(|modified| modified >= cutoff) {
                recent += 1;
            }
        }
    }
    recent
}

/// `SessionStart` -> `session_start`, `PreToolUse` -> `pre_tool_use`.
fn event_snake_case(event: &str) -> String {
    let mut snake = String::with_capacity(event.len() + 4);
    for (index, character) in event.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                snake.push('_');
            }
            snake.push(character.to_ascii_lowercase());
        } else {
            snake.push(character);
        }
    }
    snake
}

/// Compact JSON with object keys in byte order, independent of whether the
/// build enables `serde_json/preserve_order`.
fn write_canonical_json(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(key.as_str()).to_string());
                out.push(':');
                write_canonical_json(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical_json(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // Ground truth: Codex CLI 0.155.0-alpha.2.6 `hooks/list` (`currentHash`)
    // for these synthetic definitions, captured 2026-10-04.
    const ENGRAM: &str = "'/opt/example/bin/engram'";

    fn engram_hook(event: &str, timeout: u64) -> Value {
        json!({"type": "command", "command": format!("{ENGRAM} hook {event} --codex"), "timeout": timeout})
    }

    #[test]
    fn trust_hash_matches_codex_for_generated_engram_hooks() {
        let cases = [
            (
                "session_start",
                None,
                engram_hook("session-start", 15),
                "05f4938e028b498e6195c0fa4197727720dcc3e3c5f934856ba9d2f2d9386a05",
            ),
            (
                "user_prompt_submit",
                None,
                engram_hook("prompt", 30),
                "832c3125bf24b8bab303ff6192060c3c8f33bf8573705d3f8c261f9c3af36b78",
            ),
            (
                "pre_tool_use",
                Some("Read|Edit|Write|Bash"),
                engram_hook("pretool", 10),
                "be099b495f071ca422fe6e594a484d65dc32080741992bac60d9b89dd1178552",
            ),
            (
                "post_tool_use",
                None,
                engram_hook("bash", 15),
                "f6d8e3b597a7caaf9275c6e51d6cacd69b8230df6ccd56820b06f2b3b9027435",
            ),
            (
                "stop",
                None,
                engram_hook("stop", 15),
                "506e86873046ef403a8a8a9ff8bf6b2995e5c63d73a65e3f94d971dc16949391",
            ),
            (
                "session_end",
                None,
                engram_hook("session-end", 3),
                "7589f10cd488ad1784f006a3c41cedcc2d253e2c0f1c3310ee7bbab03a44fadd",
            ),
        ];
        for (event, matcher, handler, expected) in cases {
            assert_eq!(
                trust_hash(event, matcher, &handler),
                Some(format!("sha256:{expected}")),
                "{event}"
            );
        }
    }

    #[test]
    fn trust_hash_applies_codex_defaults_and_optional_fields() {
        // No timeout: Codex hashes its 600 s default.
        let defaulted =
            json!({"type": "command", "command": "/opt/example/engram hook stop --codex"});
        assert_eq!(
            trust_hash("stop", None, &defaulted).as_deref(),
            Some("sha256:a027afa1f5a48849560e53f3e8f261fe3ae2313e9d8aa05c5fd631be49d13034")
        );
        // async and statusMessage are hashed when set.
        let decorated = json!({"type": "command", "command": "echo second", "timeout": 7,
                               "async": true, "statusMessage": "Second"});
        assert_eq!(
            trust_hash("stop", None, &decorated).as_deref(),
            Some("sha256:28c9f2f0da6c7feeb39d03a32f86f814c0cd52742b0ae18817eb2739dedd3c0e")
        );
        // An empty matcher is the same as no matcher.
        let plain = json!({"type": "command", "command": "echo third", "timeout": 9});
        assert_eq!(
            trust_hash("stop", Some(""), &plain).as_deref(),
            Some("sha256:a0a98263f2c6accff0ea8df10a4d0a5632bb4cf86f9e78bcdf0a86b2608eea71")
        );
        // commandWindows is ignored; additionalContextLimit is hashed.
        let windows = json!({"type": "command", "command": "echo start", "timeout": 4,
                             "commandWindows": "echo win", "additionalContextLimit": 100});
        assert_eq!(
            trust_hash("session_start", Some("startup"), &windows).as_deref(),
            Some("sha256:2af7b4dee17e0935f435e15c5765a61d4fe32a725c6cfda57615d95b519dd33c")
        );
        assert_eq!(trust_hash("stop", None, &json!({"type": "prompt"})), None);
    }

    #[test]
    fn event_names_use_codex_snake_case() {
        assert_eq!(event_snake_case("SessionStart"), "session_start");
        assert_eq!(event_snake_case("PreToolUse"), "pre_tool_use");
        assert_eq!(event_snake_case("UserPromptSubmit"), "user_prompt_submit");
        assert_eq!(event_snake_case("Stop"), "stop");
    }

    fn write_home(hooks: &Value, config: &str) -> tempfile::TempDir {
        let home = tempfile::TempDir::new().expect("codex home");
        fs::write(
            home.path().join("hooks.json"),
            serde_json::to_string_pretty(hooks).expect("hooks json"),
        )
        .expect("hooks.json");
        fs::write(home.path().join("config.toml"), config).expect("config.toml");
        home
    }

    #[test]
    fn inspect_reports_trust_per_engram_handler_and_ignores_foreign_hooks() {
        let hooks = json!({"hooks": {
            "Stop": [{"hooks": [
                {"type": "command", "command": "echo foreign-handler", "timeout": 5},
                engram_hook("stop", 15)
            ]}],
            "SessionStart": [{"hooks": [engram_hook("session-start", 15)]}],
            "SessionEnd": [{"hooks": [engram_hook("session-end", 3)]}],
            "UserPromptSubmit": [{"hooks": [engram_hook("prompt", 30)]}]
        }});
        // Write once to learn the canonical path Codex would key on.
        let home = write_home(&hooks, "");
        let hooks_file = fs::canonicalize(home.path())
            .expect("canonical home")
            .join("hooks.json");
        let key = |event: &str, group: usize, handler: usize| {
            hook_key(&hooks_file, event, group, handler)
        };
        let config = format!(
            "[features]\nhooks = true\n\n\
             [hooks.state.\"{}\"]\ntrusted_hash = \"sha256:506e86873046ef403a8a8a9ff8bf6b2995e5c63d73a65e3f94d971dc16949391\"\n\n\
             [hooks.state.\"{}\"]\ntrusted_hash = \"sha256:{}\"\n\n\
             [hooks.state.\"{}\"]\nenabled = false\ntrusted_hash = \"sha256:7589f10cd488ad1784f006a3c41cedcc2d253e2c0f1c3310ee7bbab03a44fadd\"\n",
            key("stop", 0, 1),
            key("session_start", 0, 0),
            "0".repeat(64),
            key("session_end", 0, 0),
        );
        fs::write(home.path().join("config.toml"), config).expect("config.toml");

        let report = inspect(home.path())
            .expect("inspect")
            .expect("hooks present");
        let trust = |event: &str| {
            report
                .hooks
                .iter()
                .find(|hook| hook.event == event)
                .map(|hook| hook.trust)
        };
        assert_eq!(report.hooks.len(), 4, "the foreign handler is not Engram's");
        assert_eq!(trust("stop"), Some(HookTrust::Trusted));
        let stop = report
            .hooks
            .iter()
            .find(|hook| hook.event == "stop")
            .expect("stop");
        assert_eq!(stop.key, key("stop", 0, 1));
        assert_eq!(
            stop.current_hash.as_deref(),
            Some("sha256:506e86873046ef403a8a8a9ff8bf6b2995e5c63d73a65e3f94d971dc16949391")
        );
        assert_eq!(trust("session_start"), Some(HookTrust::Modified));
        assert_eq!(trust("session_end"), Some(HookTrust::Disabled));
        assert_eq!(trust("user_prompt_submit"), Some(HookTrust::Untrusted));
        assert!(!report.hooks_feature_disabled);
        assert_eq!(report.inactive().count(), 3);
    }

    #[test]
    fn inspect_without_hooks_file_reports_nothing_and_flags_disabled_feature() {
        let empty = tempfile::TempDir::new().expect("codex home");
        assert_eq!(inspect(empty.path()).expect("inspect"), None);

        let home = write_home(
            &json!({"hooks": {"Stop": [{"hooks": [engram_hook("stop", 15)]}]}}),
            "[features]\nhooks = false\n",
        );
        let report = inspect(home.path())
            .expect("inspect")
            .expect("hooks present");
        assert!(report.hooks_feature_disabled);
        assert_eq!(report.inactive().count(), 1);
    }
}
