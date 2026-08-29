use std::path::PathBuf;

/// Errors produced by the LEB core.
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
