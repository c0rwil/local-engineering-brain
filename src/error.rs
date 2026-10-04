use std::path::{Path, PathBuf};

/// Errors produced by the Engram core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid project id `{0}`")]
    InvalidProjectId(String),

    #[error("invalid {kind} id `{value}`")]
    InvalidDomainId { kind: &'static str, value: String },

    #[error("Git command failed: {0}")]
    Git(String),

    #[error("structural analysis failed: {0}")]
    Structural(String),

    #[error("semantic index error: {0}")]
    Semantic(String),

    #[error("event `{0}` already exists with different content")]
    DuplicateEvent(String),

    #[error("unsupported event type `{event_type}`")]
    UnknownEventType { event_type: String },

    #[error("unsupported schema version {version} for event type `{event_type}`")]
    UnknownEventVersion { event_type: String, version: u32 },

    #[error("event schema version must be greater than zero")]
    MissingEventVersion,

    #[error("database migration error: {0}")]
    Migration(String),

    #[error("mutation queue is full; retry with backoff")]
    Backpressure,

    #[error(
        "daemon memory pressure is {rss_bytes} bytes (limit {limit_bytes} bytes); retry after background work drains"
    )]
    ResourcePressure { rss_bytes: u64, limit_bytes: u64 },

    #[error("mutation scheduler is stopped")]
    SchedulerStopped,

    #[error("mutation result channel closed unexpectedly")]
    SchedulerResponseClosed,

    #[error("mutation panicked; transaction was not acknowledged")]
    MutationPanicked,

    #[error("storage synchronization primitive was poisoned")]
    StorageLockPoisoned,

    #[error("timed out waiting for a bounded storage read connection")]
    StorageReadTimeout,

    #[error("daemon error: {0}")]
    Daemon(String),

    #[error(
        "event scope mismatch: envelope project `{envelope_project_id}` differs from payload project `{payload_project_id}`"
    )]
    EventScopeMismatch {
        envelope_project_id: String,
        payload_project_id: String,
    },

    #[error("project metadata at {} is invalid: {message}", path.display())]
    InvalidProjectMetadata { path: PathBuf, message: String },

    #[error("I/O error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The operating system refused this process access to a checkout path.
    /// The message names the path and process and says how to fix it.
    #[error("{message}")]
    AccessDenied { path: PathBuf, message: String },

    /// A project merge was refused; the message says why. Nothing changed.
    #[error("merge refused: {0}")]
    MergeRefused(String),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),

    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("YAML error: {0}")]
    Yaml(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_error(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}

/// Construct a path-aware I/O error for the CLI without exposing storage details.
#[doc(hidden)]
pub fn io_error_for_cli(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    io_error(path, source)
}

/// API error code a daemon uses for [`Error::AccessDenied`]. Clients see it
/// as the `permission_denied: ` prefix of a daemon error.
pub const ACCESS_DENIED_CODE: &str = "permission_denied";

/// Map a failed read or write of a project checkout. A permission refusal
/// (EPERM or EACCES) becomes [`Error::AccessDenied`] with remediation; every
/// other failure stays a path-aware I/O error.
pub fn checkout_io_error(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    let path = path.into();
    if source.kind() == std::io::ErrorKind::PermissionDenied {
        let message = access_denied_message(&path, &source);
        Error::AccessDenied { path, message }
    } else {
        Error::Io { path, source }
    }
}

/// Whether an error is a checkout access refusal, raised locally or reported
/// by the daemon.
#[must_use]
pub fn is_access_denied(error: &Error) -> bool {
    match error {
        Error::AccessDenied { .. } => true,
        Error::Daemon(message) => message
            .strip_prefix(ACCESS_DENIED_CODE)
            .is_some_and(|rest| rest.starts_with(':')),
        _ => false,
    }
}

/// One context line for an agent hook when the daemon cannot read the
/// session's repository. Hooks otherwise fail silently, which hid this state.
#[must_use]
pub fn access_denied_hook_notice(error: &Error) -> String {
    let detail = match error {
        Error::Daemon(message) => message
            .strip_prefix(ACCESS_DENIED_CODE)
            .map_or(message.as_str(), |rest| rest.trim_start_matches(':').trim()),
        Error::AccessDenied { message, .. } => message.as_str(),
        _ => "",
    };
    format!(
        "Engram cannot read this repository, so context injection and capture are off here. \
         {detail}. Run `engram doctor` for details."
    )
}

/// File name of the running executable, for messages that must say which
/// process lacks access (`engramd`, `engram`, `engram-mcp`).
#[must_use]
pub fn current_process_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "this process".to_owned())
}

/// Actionable text for a permission refusal on `path` in this process.
#[must_use]
pub fn access_denied_message(path: &Path, source: &std::io::Error) -> String {
    let process = current_process_name();
    let fix = if process == "engramd" {
        "restart the daemon from a terminal or app that can read this folder \
         (`engram daemon restart`)"
    } else {
        "run it from a terminal or app that can read this folder"
    };
    if cfg!(target_os = "macos") && source.raw_os_error() == Some(1) {
        format!(
            "macOS privacy permission denied for {} in {process} (os error 1); {process} \
             inherits folder access from the app that started it: {fix}, or grant access in \
             System Settings > Privacy & Security (Full Disk Access or Files and Folders; a \
             grant to engramd likely does not survive reinstalling, because each release \
             installs it at a new path)",
            path.display()
        )
    } else {
        format!(
            "permission denied for {} in {process} ({source}); make the path readable by the \
             user running {process}, or {fix}",
            path.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_refusals_become_actionable_access_errors() {
        let eperm = checkout_io_error("/repo", std::io::Error::from_raw_os_error(1));
        let eacces = checkout_io_error("/repo", std::io::Error::from_raw_os_error(13));
        for error in [&eperm, &eacces] {
            assert!(
                matches!(error, Error::AccessDenied { path, .. } if path == Path::new("/repo"))
            );
            assert!(is_access_denied(error));
            let text = error.to_string();
            assert!(text.contains("/repo"), "{text}");
            assert!(text.contains(&current_process_name()), "{text}");
        }
        if cfg!(target_os = "macos") {
            let text = eperm.to_string();
            assert!(
                text.starts_with("macOS privacy permission denied for /repo"),
                "{text}"
            );
            assert!(
                text.contains("System Settings > Privacy & Security"),
                "{text}"
            );
            assert!(
                eacces
                    .to_string()
                    .starts_with("permission denied for /repo")
            );
        }
        let missing = checkout_io_error("/repo", std::io::Error::from_raw_os_error(2));
        assert!(matches!(missing, Error::Io { .. }));
        assert!(!is_access_denied(&missing));
    }

    #[test]
    fn daemon_reported_denials_are_recognized_and_explained_to_hooks() {
        let reported = Error::Daemon(format!(
            "{ACCESS_DENIED_CODE}: macOS privacy permission denied for /repo in engramd"
        ));
        assert!(is_access_denied(&reported));
        assert!(!is_access_denied(&Error::Daemon(
            "permission_denied_elsewhere".to_owned()
        )));
        assert!(!is_access_denied(&Error::Daemon(
            "invalid_request: I/O error".to_owned()
        )));
        let notice = access_denied_hook_notice(&reported);
        assert!(
            notice.starts_with("Engram cannot read this repository"),
            "{notice}"
        );
        assert!(
            notice.contains("macOS privacy permission denied for /repo in engramd."),
            "{notice}"
        );
        assert!(!notice.contains(ACCESS_DENIED_CODE), "{notice}");
    }
}
