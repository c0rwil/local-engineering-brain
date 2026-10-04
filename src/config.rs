use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result, io_error};

/// Fully resolved runtime configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Active isolated knowledge workspace. `default` preserves existing paths.
    pub workspace: String,
    pub brain: BrainConfig,
    pub cache: CacheConfig,
    pub context: ContextConfig,
    pub daemon: DaemonConfig,
    pub jobs: JobsConfig,
    pub semantic: SemanticConfig,
    pub storage: StorageConfig,
    pub structural: StructuralConfig,
    pub embeddings: EmbeddingsConfig,
    pub policy: PolicyConfig,
    pub proxy: ProxyConfig,
    pub security: SecurityConfig,
    pub network: NetworkConfig,
    pub evaluation: EvaluationConfig,
    pub synthesis: SynthesisConfig,
    pub vault: VaultConfig,
    pub links: LinksConfig,
    pub architect: ArchitectConfig,
    pub web: WebConfig,
    pub watch: WatchConfig,
    pub sync: SyncConfig,
    pub judge: crate::judge::JudgeConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            workspace: crate::workspaces::DEFAULT_WORKSPACE.to_owned(),
            brain: BrainConfig::default(),
            cache: CacheConfig::default(),
            context: ContextConfig::default(),
            daemon: DaemonConfig::default(),
            jobs: JobsConfig::default(),
            semantic: SemanticConfig::default(),
            storage: StorageConfig::default(),
            structural: StructuralConfig::default(),
            embeddings: EmbeddingsConfig::default(),
            policy: PolicyConfig::default(),
            proxy: ProxyConfig::default(),
            security: SecurityConfig::default(),
            network: NetworkConfig::default(),
            evaluation: EvaluationConfig::default(),
            synthesis: SynthesisConfig::default(),
            vault: VaultConfig::default(),
            links: LinksConfig::default(),
            architect: ArchitectConfig::default(),
            web: WebConfig::default(),
            watch: WatchConfig::default(),
            sync: SyncConfig::default(),
            judge: crate::judge::JudgeConfig::default(),
        }
    }
}

/// Cross-instance knowledge sync (`src/sync.rs`). Down-sync replicates the
/// central instance's validated knowledge; up-sync offers local validated
/// memories as central candidates.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Central instance base URL (e.g. `http://engram.internal:11436`).
    /// Empty disables sync entirely.
    pub central_url: String,
    /// Bearer token for the central instance's API.
    pub token: Option<String>,
    /// Automatic sync cadence in the daemon watcher. 0 = manual `engram sync`
    /// only.
    pub every_secs: u64,
    /// Also offer local validated memories upstream.
    pub push: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            central_url: String::new(),
            token: None,
            every_secs: 900,
            push: true,
        }
    }
}

/// Automatic commit-driven refresh (`src/daemon.rs` watcher task).
/// Uncommitted edits never trigger anything: the watcher compares committed
/// HEADs only, so the encyclopedia follows commits and branch switches.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// Run the watcher inside the daemon.
    pub enabled: bool,
    /// How often to compare each checkout's committed HEAD with its last
    /// indexed head (cheap: one `git rev-parse` per repo).
    pub local_poll_secs: u64,
    /// How often to fetch `origin` and refresh the shared-truth view when
    /// the remote default branch has advanced. 0 disables origin polling.
    pub origin_poll_secs: u64,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            local_poll_secs: 30,
            origin_poll_secs: 900,
        }
    }
}

/// The embedded workspace browser (`src/web.rs`), served by the daemon so
/// the encyclopedia is always available while Engram is alive.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    /// Serve the browser from the daemon. On by default: the listener binds
    /// 127.0.0.1 only and reuses daemon data paths.
    pub enabled: bool,
    /// Port for the browser.
    pub port: u16,
    /// Bind address. Anything other than loopback (a central team server)
    /// REQUIRES `token`; the daemon refuses to expose the browser to a
    /// network without authentication.
    pub bind: String,
    /// Bearer token required on every /api and /webhook request when set.
    /// The page itself stays fetchable (static HTML); data does not.
    pub token: Option<String>,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 11_436,
            bind: "127.0.0.1".to_owned(),
            token: None,
        }
    }
}

/// LLM-assisted architecture synthesis (`engram architect`, `src/architect.rs`).
/// The output is a derived vault document, never evidence-gated memory.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchitectConfig {
    /// OpenAI-compatible chat-completions endpoint. Non-loopback URLs are
    /// refused unless `network.local_only = false` (same stance as the
    /// proxy).
    pub runtime_url: String,
    /// Model name sent to the runtime. Empty means the CLI requires
    /// `--model`.
    pub model: String,
    /// Automatic regeneration when the guide's stable digest drifts:
    /// `"off"` (badge only), `"local"` (the HTTP runtime above), or
    /// `"claude"` (the local `claude` CLI / subscription). Explicit opt-in:
    /// both non-off modes make the daemon call a model on its own.
    pub auto: String,
    /// Minimum seconds between automatic regenerations.
    pub auto_min_secs: u64,
}

impl Default for ArchitectConfig {
    fn default() -> Self {
        Self {
            runtime_url: "http://127.0.0.1:8010/v1/chat/completions".to_owned(),
            model: String::new(),
            auto: "off".to_owned(),
            auto_min_secs: 3_600,
        }
    }
}

/// Automatic cross-repository link discovery (`src/linkscan.rs`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinksConfig {
    /// After each structural index, scan the repo's own manifests (Cargo
    /// path deps, npm `file:` deps, pip editable installs, git submodules)
    /// and auto-link DEFINITIVE matches to registered projects, with the
    /// evidence recorded in the link note. Name-only matches are never
    /// auto-applied — `engram link scan` surfaces them as proposals.
    pub auto_scan: bool,
}

impl Default for LinksConfig {
    fn default() -> Self {
        Self { auto_scan: true }
    }
}

/// Derived Obsidian-vault knowledge export (`src/vault.rs`).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VaultConfig {
    /// Master switch. Off by default: the vault writes Markdown into
    /// `root`, which is user-visible filesystem output.
    pub enabled: bool,
    /// Vault root directory. Point Obsidian at this folder.
    pub root: PathBuf,
    /// Re-export automatically (via a durable `vault_export` job) whenever a
    /// structural scan changes a project's graph, so notes and mermaid
    /// diagrams track the code without manual rebuilds.
    pub auto_refresh: bool,
}

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            root: PathBuf::from("~/.engram/vault"),
            auto_refresh: true,
        }
    }
}

/// Conservative multi-episode procedure synthesis (spec §16.3–§16.5).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SynthesisConfig {
    /// Distinct successful evidence-backed episodes that must share one
    /// normalized strategy signature before a procedure draft is synthesized.
    pub min_supporting_episodes: u32,
    /// Distinct successfully-served tasks a procedure must have been injected
    /// for before it can be promoted to the `skill` kind.
    pub min_distinct_uses: u32,
    /// Run a bounded synthesis pass automatically after a successful
    /// `finish_task`. Off by default: V1 keeps learning explicit (spec P9).
    pub auto: bool,
    /// Allow validated procedures from other projects to enter this project as
    /// explicitly marked hypothesis memories (spec §16.5). Off by default.
    pub cross_project_hypotheses: bool,
}

impl Default for SynthesisConfig {
    fn default() -> Self {
        Self {
            min_supporting_episodes: 3,
            min_distinct_uses: 3,
            auto: false,
            cross_project_hypotheses: false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrainConfig {
    pub auto_learn: bool,
    pub conservative_promotion: bool,
    pub raw_transcript_retention_days: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Enable the daemon's byte-bounded derived read caches. Cached values are
    /// never canonical: every key is scoped by project and storage generation,
    /// and graph mutations bump the generation before responses return.
    pub enabled: bool,
    /// Hard aggregate budget across every active project partition.
    pub total_budget_mb: u64,
    /// Maximum concurrently cached projects; least-recent partitions evict.
    pub max_active_projects: u16,
    /// Objects heavier than this bypass the cache entirely.
    pub max_item_kb: u32,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            total_budget_mb: 64,
            max_active_projects: 8,
            max_item_kb: 512,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub max_connections: u16,
    pub max_request_bytes: u32,
    pub request_timeout_ms: u64,
    pub read_connections: u16,
    pub read_wait_timeout_ms: u64,
    pub writer_queue_capacity: u32,
    pub background_concurrency: u16,
    pub rss_soft_limit_mb: u32,
    pub rss_hard_limit_mb: u32,
}

/// Durable daemon-owned background job execution over the canonical `jobs`
/// table. The runner polls on a fixed interval, executes under the daemon's
/// bounded background slots, and never starts new jobs under hard RSS pressure.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct JobsConfig {
    /// Run the background job loop inside the daemon.
    pub enabled: bool,
    /// Fixed (jitterless) queue poll interval.
    pub poll_interval_ms: u64,
    /// Attempts before a repeatedly failing job is marked `dead`.
    pub max_attempts: u32,
    /// `context_runs`/`context_items` older than this are deleted by the
    /// daily `consolidation_gc` job in bounded batches (spec section 16).
    pub context_run_retention_days: u32,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_ms: 5_000,
            max_attempts: 5,
            context_run_retention_days: 90,
        }
    }
}

// Each bool is an independent, benchmarked ablation switch; collapsing them
// into an enum would forbid measured combinations.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub max_injected_tokens: u32,
    pub stable_prefix_tokens: u32,
    pub semantic_retrieval: bool,
    /// Hard cap on injected memories per compile; 0 means budget-limited
    /// only. Family-G v1 showed indiscriminate injection taxes wall time:
    /// the agent tries to honor every injected rule (perf-evidence
    /// 2026-08-30).
    pub max_injected_memories: u32,
    /// Render `preference` memories as an explicit review-standards section
    /// (imperative, consequence-framed, placed last for salience). Default
    /// ON: the best-performing composition at frontier tier — 6/6 fidelity
    /// passes, fastest walls, ~35% fewer injected bytes than inclusive
    /// rendering (perf-evidence 2026-08-31). Disable for ablation.
    pub review_standards_section: bool,
    /// Render validated memories from the deterministic derived compact
    /// projection. Default ON since 2026-09-01: success parity in both the
    /// capability A/B (−33% injected bytes) and the fidelity-family gate
    /// (6/6 vs 6/6); disable for a verbose-mode ablation.
    pub compact_injection: bool,
    /// Also inject relevance-gated memories from explicitly linked projects
    /// (`engram link add`), each under a heading naming the source repository.
    /// Default ON: links themselves are always an explicit operator action,
    /// and unlinked projects are never eligible.
    pub include_linked_projects: bool,
}

/// Bounds and embedding provider for the optional semantic retrieval stage.
/// The stage itself is toggled by `context.semantic_retrieval` (default
/// `false`). The default hashed embedder has a compile-time dimension
/// ([`crate::semantic::EMBEDDING_DIMENSION`]); the optional local provider
/// calls an `OpenAI`-compatible embeddings endpoint on a loopback address.
/// Only the two bounds may be set by a project's own configuration; provider
/// settings are machine-level (user or workspace configuration).
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SemanticConfig {
    /// Maximum semantic candidates merged into hybrid retrieval per request.
    pub max_candidates: u32,
    /// Hard bound on memories scanned per brute-force cosine search.
    pub max_scanned: u32,
    /// `hashed` (default) or `local`.
    pub provider: crate::semantic::EmbeddingProviderKind,
    /// Loopback `OpenAI`-compatible `POST /v1/embeddings` URL.
    pub endpoint: String,
    /// Embedding model served by the endpoint; required for `local`.
    pub model: String,
    /// Required vector dimension; 0 accepts the model's dimension.
    pub dimension: u32,
    /// Texts per embeddings request during a rebuild.
    pub batch_size: u32,
    /// Bound on each rebuild embeddings request.
    pub timeout_ms: u64,
    /// Bound on the single query embedding in the context hot path.
    pub query_timeout_ms: u64,
    /// Cosine similarity at or below this floor adds no semantic score.
    pub min_similarity: f64,
    /// Prepended to task text before embedding it as a query.
    pub query_prefix: String,
    /// Prepended to memory text before embedding it for storage.
    pub document_prefix: String,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            max_candidates: 50,
            max_scanned: 50_000,
            provider: crate::semantic::EmbeddingProviderKind::Hashed,
            endpoint: "http://127.0.0.1:11434/v1/embeddings".to_owned(),
            model: String::new(),
            dimension: 0,
            batch_size: 16,
            timeout_ms: 30_000,
            query_timeout_ms: 2_000,
            min_similarity: 0.0,
            query_prefix: String::new(),
            document_prefix: String::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub db_path: PathBuf,
    pub blob_budget_gb: u32,
    /// Redacted evidence artifacts larger than this move from the canonical row
    /// into the content-addressed filesystem blob store.
    pub blob_threshold_bytes: u32,
    pub vector_backend: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StructuralConfig {
    pub provider: String,
    pub max_files: u32,
    pub max_file_bytes: u64,
    pub git_history_commits: u32,
    pub graphify_export: Option<PathBuf>,
    pub max_graph_nodes: u32,
    pub max_graph_edges: u32,
    pub max_prepared_bytes: u64,
    pub max_graphify_export_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EmbeddingsConfig {
    pub provider: String,
    pub model: String,
    /// A remote provider requires this flag and `network.local_only = false`.
    pub allow_remote: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    pub default_enforcement: Enforcement,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Enforcement {
    #[default]
    Warn,
    RequireValidation,
    Block,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    pub local_only: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProxyConfig {
    pub bind: String,
    pub runtime_url: String,
    /// Permit a non-loopback runtime only when `network.local_only` is also
    /// disabled. Remote runtimes must use HTTPS and may not embed credentials.
    pub allow_remote_runtime: bool,
    pub failure_mode: ProxyFailureMode,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecurityConfig {
    /// Additional Rust-regex patterns whose complete matches are redacted.
    pub custom_redaction_patterns: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyFailureMode {
    #[default]
    FailOpen,
    FailClosed,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvaluationConfig {
    pub record_trajectory_metrics: bool,
}

/// Command-line overrides, which have the highest configuration priority.
#[derive(Clone, Debug, Default)]
pub struct ConfigOverrides {
    pub workspace: Option<String>,
    pub db_path: Option<PathBuf>,
    pub local_only: Option<bool>,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            auto_learn: false,
            conservative_promotion: true,
            raw_transcript_retention_days: 0,
        }
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from("~/.engram/engramd.sock"),
            max_connections: 128,
            max_request_bytes: 1_048_576,
            request_timeout_ms: 30_000,
            read_connections: 4,
            read_wait_timeout_ms: 5_000,
            writer_queue_capacity: 4_096,
            background_concurrency: 2,
            rss_soft_limit_mb: 600,
            rss_hard_limit_mb: 900,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_injected_tokens: 6_000,
            stable_prefix_tokens: 1_800,
            semantic_retrieval: false,
            max_injected_memories: 0,
            review_standards_section: true,
            compact_injection: true,
            include_linked_projects: true,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("~/.engram/brain.sqlite"),
            blob_budget_gb: 0,
            blob_threshold_bytes: 16_384,
            vector_backend: "disabled".to_owned(),
        }
    }
}

impl Default for StructuralConfig {
    fn default() -> Self {
        Self {
            provider: "tree_sitter".to_owned(),
            max_files: 100_000,
            max_file_bytes: 2 * 1_024 * 1_024,
            git_history_commits: 50_000,
            graphify_export: None,
            max_graph_nodes: 2_000_000,
            max_graph_edges: 8_000_000,
            max_prepared_bytes: 256 * 1_024 * 1_024,
            max_graphify_export_bytes: 128 * 1_024 * 1_024,
        }
    }
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        Self {
            provider: "disabled".to_owned(),
            model: String::new(),
            allow_remote: false,
        }
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            default_enforcement: Enforcement::Warn,
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { local_only: true }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:11435".to_owned(),
            runtime_url: "http://127.0.0.1:8000".to_owned(),
            allow_remote_runtime: false,
            failure_mode: ProxyFailureMode::FailOpen,
        }
    }
}

impl Config {
    /// Load a client's current repository/workspace selection without indexing
    /// or registering anything. Git roots are discovered read-only; non-Git
    /// directories use their nearest ancestor carrying a project config.
    ///
    /// # Errors
    /// Returns working-directory, path resolution, or configuration errors.
    pub fn load_for_current_dir(overrides: &ConfigOverrides) -> Result<Self> {
        let current = std::env::current_dir().map_err(|source| io_error(".", source))?;
        let root = configuration_root(&current)?;
        Self::load_for(Some(&root), overrides)
    }

    /// Load defaults, user config, project config, then CLI overrides.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable/malformed layer or an invalid merged configuration.
    pub fn load_for(project_root: Option<&Path>, overrides: &ConfigOverrides) -> Result<Self> {
        let home = home_dir();
        if let Some(home) = home.as_deref() {
            migrate_legacy_state(home);
        }
        let user_config = home.as_ref().map(|home| {
            let current = home.join(".config/engram/config.toml");
            let legacy = home.join(".config/leb/config.toml");
            if !current.exists() && legacy.exists() {
                legacy
            } else {
                current
            }
        });
        let project_config = project_root.map(|root| {
            let current = root.join(".engram/config.toml");
            let legacy = root.join(".leb/config.toml");
            if !current.exists() && legacy.exists() {
                legacy
            } else {
                current
            }
        });
        Self::load_layers_inner(
            user_config.as_deref(),
            project_config.as_deref(),
            overrides,
            home.as_deref(),
            true,
        )
    }

    /// Load explicitly supplied layers. This is public to keep configuration tests deterministic.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable/malformed layer or an invalid merged configuration.
    pub fn load_layers(
        user_config: Option<&Path>,
        project_config: Option<&Path>,
        overrides: &ConfigOverrides,
        home: &Path,
    ) -> Result<Self> {
        Self::load_layers_inner(user_config, project_config, overrides, Some(home), false)
    }

    fn load_layers_inner(
        user_config: Option<&Path>,
        project_config: Option<&Path>,
        overrides: &ConfigOverrides,
        home: Option<&Path>,
        apply_environment: bool,
    ) -> Result<Self> {
        let environment_workspace = if apply_environment {
            match std::env::var("ENGRAM_WORKSPACE") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err(Error::Config(
                        "ENGRAM_WORKSPACE must be valid UTF-8".to_owned(),
                    ));
                }
            }
        } else {
            None
        };
        let (merged, workspace_root) = workspace_layers(
            user_config,
            project_config,
            overrides,
            home,
            environment_workspace.as_deref(),
        )?;

        let mut config: Self = merged.try_into().map_err(Error::TomlDeserialize)?;
        let default_socket = DaemonConfig::default().socket_path;
        let database_overridden = overrides.db_path.is_some()
            || (apply_environment && std::env::var_os("ENGRAM_DB_PATH").is_some());
        let socket_overridden =
            apply_environment && std::env::var_os("ENGRAM_SOCKET_PATH").is_some();
        let socket_was_default = config.daemon.socket_path == default_socket
            || workspace_root
                .as_ref()
                .is_some_and(|root| config.daemon.socket_path == root.join("engramd.sock"));
        if apply_environment {
            if let Some(path) = std::env::var_os("ENGRAM_DB_PATH") {
                config.storage.db_path = PathBuf::from(path);
            }
            if let Some(path) = std::env::var_os("ENGRAM_SOCKET_PATH") {
                config.daemon.socket_path = PathBuf::from(path);
            }
        }
        if let Some(db_path) = &overrides.db_path {
            config.storage.db_path.clone_from(db_path);
        }
        if let Some(local_only) = overrides.local_only {
            config.network.local_only = local_only;
        }
        config.storage.db_path = expand_home(&config.storage.db_path, home)?;
        if config.vault.enabled || workspace_root.is_some() {
            config.vault.root = expand_home(&config.vault.root, home)?;
        }
        config.daemon.socket_path =
            if database_overridden && !socket_overridden && socket_was_default {
                socket_for_database(&config.storage.db_path)
            } else if home.is_none() && config.daemon.socket_path.to_string_lossy().starts_with('~')
            {
                config
                    .storage
                    .db_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("engramd.sock")
            } else {
                expand_home(&config.daemon.socket_path, home)?
            };
        if let Some(root) = workspace_root {
            crate::workspaces::validate_paths(&config, &root)?;
        }
        config.validate()?;
        Ok(config)
    }

    /// Read a complete resolved configuration snapshot used by daemon startup.
    /// The private snapshot already contains all user, profile, project, and
    /// command-line settings; loading more layers would change its scope.
    ///
    /// # Errors
    ///
    /// Returns an error for unreadable, malformed, or invalid snapshots.
    pub fn load_resolved(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path).map_err(|source| io_error(path, source))?;
        let config: Self = toml::from_str(&raw)?;
        config.validate()?;
        Ok(config)
    }

    fn validate_jobs(&self) -> Result<()> {
        if self.jobs.poll_interval_ms == 0 || self.jobs.poll_interval_ms > 3_600_000 {
            return Err(Error::Config(
                "jobs.poll_interval_ms must be between 1 and 3600000".to_owned(),
            ));
        }
        if self.jobs.max_attempts == 0 || self.jobs.max_attempts > 20 {
            return Err(Error::Config(
                "jobs.max_attempts must be between 1 and 20".to_owned(),
            ));
        }
        if self.jobs.context_run_retention_days == 0 || self.jobs.context_run_retention_days > 3_650
        {
            return Err(Error::Config(
                "jobs.context_run_retention_days must be between 1 and 3650".to_owned(),
            ));
        }
        Ok(())
    }

    /// Validate safety constraints and token budgets.
    ///
    /// # Errors
    ///
    /// Returns an error when token budgets conflict or a remote provider lacks explicit opt-in.
    pub fn validate(&self) -> Result<()> {
        crate::workspaces::validate_name(&self.workspace)?;
        crate::workspaces::validate_config(self)?;
        self.judge.validate()?;
        if self.judge.enabled
            && self.judge.provider == crate::judge::JudgeProvider::Typesafe
            && self.network.local_only
        {
            return Err(Error::Config(
                "TypeSafe judge requires network.local_only = false".to_owned(),
            ));
        }
        if self.context.max_injected_tokens == 0 {
            return Err(Error::Config(
                "context.max_injected_tokens must be greater than zero".to_owned(),
            ));
        }
        if self.context.stable_prefix_tokens > self.context.max_injected_tokens {
            return Err(Error::Config(
                "context.stable_prefix_tokens cannot exceed max_injected_tokens".to_owned(),
            ));
        }

        validate_supported_v1_features(self)?;
        crate::semantic::validate_config(&self.semantic, self.network.local_only)?;
        if self.daemon.max_connections == 0
            || self.daemon.max_request_bytes == 0
            || self.daemon.request_timeout_ms == 0
            || self.daemon.read_connections == 0
            || self.daemon.read_wait_timeout_ms == 0
            || self.daemon.writer_queue_capacity == 0
            || self.daemon.background_concurrency == 0
        {
            return Err(Error::Config(
                "daemon connection, request, queue, and background limits must be greater than zero"
                    .to_owned(),
            ));
        }
        if self.daemon.rss_soft_limit_mb == 0
            || self.daemon.rss_soft_limit_mb >= self.daemon.rss_hard_limit_mb
        {
            return Err(Error::Config(
                "daemon RSS limits must be positive with soft < hard".to_owned(),
            ));
        }
        self.validate_jobs()?;
        if self.structural.max_files == 0
            || self.structural.max_file_bytes == 0
            || self.structural.git_history_commits == 0
            || self.structural.git_history_commits > 100_000
            || self.structural.max_graph_nodes == 0
            || self.structural.max_graph_edges == 0
            || self.structural.max_prepared_bytes == 0
            || self.structural.max_graphify_export_bytes == 0
        {
            return Err(Error::Config(
                "structural limits must be positive and git_history_commits must be <= 100000"
                    .to_owned(),
            ));
        }
        let rss_hard_bytes = u64::from(self.daemon.rss_hard_limit_mb) * 1_024 * 1_024;
        if self.structural.max_prepared_bytes > rss_hard_bytes / 2
            || self.structural.max_graphify_export_bytes > rss_hard_bytes / 2
        {
            return Err(Error::Config(
                "structural prepared data and Graphify input must each fit half the RSS hard limit"
                    .to_owned(),
            ));
        }
        if !matches!(
            self.structural.provider.as_str(),
            "tree_sitter" | "graphify"
        ) {
            return Err(Error::Config(
                "structural.provider must be `tree_sitter` or `graphify`".to_owned(),
            ));
        }
        if self.structural.provider == "graphify" && self.structural.graphify_export.is_none() {
            return Err(Error::Config(
                "structural.graphify_export is required for the graphify provider".to_owned(),
            ));
        }
        if self.storage.blob_threshold_bytes < 1_024
            || self.storage.blob_threshold_bytes > 1_048_576
        {
            return Err(Error::Config(
                "storage.blob_threshold_bytes must be between 1024 and 1048576".to_owned(),
            ));
        }
        crate::security::Redactor::new(&self.security.custom_redaction_patterns)?;

        if !(2..=64).contains(&self.synthesis.min_supporting_episodes) {
            return Err(Error::Config(
                "synthesis.min_supporting_episodes must be between 2 and 64".to_owned(),
            ));
        }
        if !(1..=10_000).contains(&self.synthesis.min_distinct_uses) {
            return Err(Error::Config(
                "synthesis.min_distinct_uses must be between 1 and 10000".to_owned(),
            ));
        }

        validate_proxy_network(self)?;
        Ok(())
    }
}

fn configuration_root(directory: &Path) -> Result<PathBuf> {
    let directory = directory
        .canonicalize()
        .map_err(|source| io_error(directory, source))?;
    // A worktree's .git is a file; a primary checkout's .git is a directory.
    // Configuration selection needs only this boundary, not a full GitState
    // scan (which computes content fingerprints of dirty worktrees).
    if let Some(root) = directory
        .ancestors()
        .find(|parent| parent.join(".git").exists())
    {
        return Ok(root.to_path_buf());
    }
    Ok(directory
        .ancestors()
        .find(|parent| {
            parent.join(".engram/config.toml").is_file()
                || parent.join(".leb/config.toml").is_file()
        })
        .unwrap_or(&directory)
        .to_path_buf())
}

fn validate_supported_v1_features(config: &Config) -> Result<()> {
    if config.brain.auto_learn
        || !config.brain.conservative_promotion
        || config.brain.raw_transcript_retention_days != 0
    {
        return Err(Error::Config(
            "V1 requires explicit learning, conservative promotion, and zero raw-transcript retention"
                .to_owned(),
        ));
    }
    if config.cache.enabled {
        let per_project = config
            .cache
            .total_budget_mb
            .saturating_mul(1024 * 1024)
            .checked_div(u64::from(config.cache.max_active_projects))
            .unwrap_or(0);
        if per_project == 0 {
            return Err(Error::Config(
                "cache.total_budget_mb and cache.max_active_projects must be positive".to_owned(),
            ));
        }
        if u64::from(config.cache.max_item_kb).saturating_mul(1024) > per_project {
            return Err(Error::Config(
                "cache.max_item_kb must fit within one project's share of the cache budget"
                    .to_owned(),
            ));
        }
    }
    if config.semantic.max_candidates == 0
        || config.semantic.max_candidates > 1_000
        || config.semantic.max_scanned < config.semantic.max_candidates
        || config.semantic.max_scanned > 1_000_000
    {
        return Err(Error::Config(
            "semantic.max_candidates must be in 1..=1000 and semantic.max_scanned must be \
             between max_candidates and 1000000"
                .to_owned(),
        ));
    }
    if config.storage.blob_budget_gb != 0 || config.storage.vector_backend != "disabled" {
        return Err(Error::Config(
            "blob and vector storage backends are unavailable in V1".to_owned(),
        ));
    }
    if config.embeddings.provider != "disabled"
        || !config.embeddings.model.is_empty()
        || config.embeddings.allow_remote
    {
        return Err(Error::Config(
            "embedding providers are unavailable in V1 and must remain disabled".to_owned(),
        ));
    }
    if config.policy.default_enforcement != Enforcement::Warn {
        return Err(Error::Config(
            "policy enforcement beyond warn is unavailable in V1".to_owned(),
        ));
    }
    if config.evaluation.record_trajectory_metrics {
        return Err(Error::Config(
            "implicit trajectory capture is unavailable; benchmark metrics must be explicit"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_proxy_network(config: &Config) -> Result<()> {
    let bind = config
        .proxy
        .bind
        .parse::<std::net::SocketAddr>()
        .map_err(|_| {
            Error::Config("proxy.bind must be an explicit IP socket address".to_owned())
        })?;
    if !bind.ip().is_loopback() {
        return Err(Error::Config(
            "proxy.bind must be loopback; external exposure is unsupported".to_owned(),
        ));
    }
    let runtime = url::Url::parse(&config.proxy.runtime_url)
        .map_err(|error| Error::Config(format!("invalid proxy.runtime_url: {error}")))?;
    if runtime.scheme() != "http" && runtime.scheme() != "https" {
        return Err(Error::Config(
            "proxy.runtime_url must use http or https".to_owned(),
        ));
    }
    if !runtime.username().is_empty() || runtime.password().is_some() {
        return Err(Error::Config(
            "proxy.runtime_url must not embed credentials".to_owned(),
        ));
    }
    let host = runtime
        .host()
        .ok_or_else(|| Error::Config("proxy.runtime_url must include a host".to_owned()))?;
    if runtime.query().is_some() || runtime.fragment().is_some() {
        return Err(Error::Config(
            "proxy.runtime_url must not include a query or fragment".to_owned(),
        ));
    }
    let local = match host {
        url::Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    };
    if !local {
        if !config.proxy.allow_remote_runtime {
            return Err(Error::Config(
                "remote proxy.runtime_url requires proxy.allow_remote_runtime = true".to_owned(),
            ));
        }
        if config.network.local_only {
            return Err(Error::Config(
                "remote proxy.runtime_url requires network.local_only = false".to_owned(),
            ));
        }
        if runtime.scheme() != "https" {
            return Err(Error::Config(
                "remote proxy.runtime_url must use https".to_owned(),
            ));
        }
    }
    Ok(())
}

fn workspace_layers(
    user_config: Option<&Path>,
    project_config: Option<&Path>,
    overrides: &ConfigOverrides,
    home: Option<&Path>,
    environment: Option<&str>,
) -> Result<(toml::Value, Option<PathBuf>)> {
    let user_layer = read_layer(user_config)?;
    let project_layer = read_layer(project_config)?;
    if let Some(layer) = &project_layer {
        reject_project_embedding_provider(layer)?;
    }
    let mut merged = toml::Value::try_from(Config::default())?;
    if let Some(layer) = &user_layer {
        merge_toml(&mut merged, layer.clone());
    }
    let mut selection = merged.clone();
    if let Some(layer) = &project_layer {
        merge_toml(&mut selection, layer.clone());
    }
    // Reject malformed tables before assigning isolated paths below.
    let _: Config = selection
        .clone()
        .try_into()
        .map_err(Error::TomlDeserialize)?;
    let workspace = selected_workspace(&selection, overrides, environment)?;
    let workspace_root = if workspace == crate::workspaces::DEFAULT_WORKSPACE {
        None
    } else {
        let home =
            home.ok_or_else(|| Error::Config("HOME is required for named workspaces".to_owned()))?;
        let root = crate::workspaces::root(home, &workspace)?;
        // Global tuning is inherited, but global knowledge paths and sync
        // destinations must never silently point a new workspace at the
        // default workspace's data or central instance.
        merged["storage"]["db_path"] = path_value(&root.join("brain.sqlite"));
        merged["daemon"]["socket_path"] = path_value(&root.join("engramd.sock"));
        merged["vault"]["root"] = path_value(&root.join("vault"));
        merged["web"]["enabled"] = toml::Value::Boolean(false);
        merged["web"]["port"] = toml::Value::Integer(i64::from(WebConfig::default().port));
        merged["sync"] = toml::Value::try_from(SyncConfig::default())?;
        if let Some(layer) = read_layer(Some(&root.join("config.toml")))? {
            if layer.get("workspace").is_some() {
                return Err(Error::Config(
                    "a named workspace profile cannot select a different workspace; select it with --workspace, ENGRAM_WORKSPACE, or project config".to_owned(),
                ));
            }
            merge_toml(&mut merged, layer);
        }
        Some(root)
    };
    if let Some(layer) = project_layer {
        merge_toml(&mut merged, layer);
    }
    merged["workspace"] = toml::Value::String(workspace);

    Ok((merged, workspace_root))
}

/// Embedding provider settings are machine-level: a repository's own
/// `.engram/config.toml` must not choose where task and memory text is sent.
/// Projects may still tune the semantic bounds.
fn reject_project_embedding_provider(layer: &toml::Value) -> Result<()> {
    let Some(semantic) = layer.get("semantic").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    match semantic
        .keys()
        .find(|key| !matches!(key.as_str(), "max_candidates" | "max_scanned"))
    {
        Some(key) => Err(Error::Config(format!(
            "project configuration cannot set semantic.{key}: embedding provider settings are \
             machine-level (~/.config/engram/config.toml or a workspace profile); projects may \
             set only semantic.max_candidates and semantic.max_scanned"
        ))),
        None => Ok(()),
    }
}

fn read_layer(path: Option<&Path>) -> Result<Option<toml::Value>> {
    let Some(path) = path.filter(|path| path.exists()) else {
        return Ok(None);
    };
    let raw = fs::read_to_string(path).map_err(|source| io_error(path, source))?;
    Ok(Some(toml::from_str(&raw)?))
}

fn path_value(path: &Path) -> toml::Value {
    toml::Value::String(path.to_string_lossy().into_owned())
}

fn selected_workspace(
    merged: &toml::Value,
    overrides: &ConfigOverrides,
    environment: Option<&str>,
) -> Result<String> {
    let configured = merged
        .get("workspace")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| Error::Config("workspace must be a string".to_owned()))?;
    let selected = overrides
        .workspace
        .as_deref()
        .or(environment)
        .unwrap_or(configured);
    crate::workspaces::validate_name(selected)?;
    Ok(selected.to_owned())
}

fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = base.get_mut(&key) {
                    merge_toml(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// One-time rename of pre-Engram state directories (`~/.leb` -> `~/.engram`,
/// `~/.config/leb` -> `~/.config/engram`). Only when the new location does
/// not exist yet, so a machine that already migrated is untouched; the
/// daemon is stopped by the installer before this can run.
fn migrate_legacy_state(home: &Path) {
    for (legacy, current) in [
        (home.join(".leb"), home.join(".engram")),
        (home.join(".config/leb"), home.join(".config/engram")),
    ] {
        if legacy.is_dir() && !current.exists() {
            let _ = std::fs::rename(&legacy, &current);
        }
    }
    // Path VALUES inside the migrated config must follow the directories:
    // a pinned `root = "~/.leb/vault"` would otherwise point at a folder
    // that no longer exists (observed: the encyclopedia lost its guide).
    let config_path = home.join(".config/engram/config.toml");
    if let Ok(text) = std::fs::read_to_string(&config_path) {
        let home_str = home.to_string_lossy();
        let rewritten = text
            .replace("~/.leb/", "~/.engram/")
            .replace("~/.leb\"", "~/.engram\"")
            .replace(
                &format!("{home_str}/.leb/"),
                &format!("{home_str}/.engram/"),
            );
        if rewritten != text {
            let _ = std::fs::write(&config_path, rewritten);
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn expand_home(path: &Path, home: Option<&Path>) -> Result<PathBuf> {
    let value = path.to_string_lossy();
    if value == "~" {
        return home.map(Path::to_path_buf).ok_or_else(|| {
            Error::Config("HOME is unavailable and storage.db_path uses `~`".to_owned())
        });
    }
    if let Some(suffix) = value.strip_prefix("~/") {
        return home.map(|home| home.join(suffix)).ok_or_else(|| {
            Error::Config("HOME is unavailable and storage.db_path uses `~`".to_owned())
        });
    }
    if value.starts_with('~') {
        return Err(Error::Config(format!(
            "only `~` and `~/...` path expansion are supported: {}",
            path.display()
        )));
    }
    Ok(path.to_path_buf())
}

fn socket_for_database(database: &Path) -> PathBuf {
    let digest = Sha256::digest(database.as_os_str().as_encoded_bytes());
    let mut short = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write as _;
        let _ = write!(short, "{byte:02x}");
    }
    database
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".engramd-{short}.sock"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_state_migration_moves_dirs_and_rewrites_config_paths() {
        let temp = tempfile::TempDir::new().expect("temp");
        let home = temp.path();
        std::fs::create_dir_all(home.join(".leb/vault")).expect("legacy state");
        std::fs::create_dir_all(home.join(".config/leb")).expect("legacy config dir");
        std::fs::write(
            home.join(".config/leb/config.toml"),
            "[vault]\nroot = \"~/.leb/vault\"\n",
        )
        .expect("legacy config");
        super::migrate_legacy_state(home);
        assert!(home.join(".engram/vault").is_dir());
        assert!(!home.join(".leb").exists());
        let migrated =
            std::fs::read_to_string(home.join(".config/engram/config.toml")).expect("migrated");
        assert!(migrated.contains("~/.engram/vault"), "{migrated}");
        assert!(!migrated.contains("~/.leb"));
    }

    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn test_cli_overrides_project_overrides_user() {
        let temp = TempDir::new().expect("temp dir");
        let user = temp.path().join("user.toml");
        let project = temp.path().join("project.toml");
        fs::write(&user, "[context]\nmax_injected_tokens = 1000\n").expect("user config");
        fs::write(&project, "[context]\nmax_injected_tokens = 2000\n").expect("project");
        let cli_db = temp.path().join("cli.sqlite");

        let config = Config::load_layers(
            Some(&user),
            Some(&project),
            &ConfigOverrides {
                db_path: Some(cli_db.clone()),
                local_only: None,
                workspace: None,
            },
            temp.path(),
        )
        .expect("valid config");

        assert_eq!(config.context.max_injected_tokens, 2_000);
        assert_eq!(config.storage.db_path, cli_db);
    }

    #[test]
    fn test_invalid_token_budget_rejected() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("invalid.toml");
        fs::write(&path, "[context]\nmax_injected_tokens = 0\n").expect("config");
        let error =
            Config::load_layers(Some(&path), None, &ConfigOverrides::default(), temp.path())
                .expect_err("invalid budget");
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn test_remote_embedding_requires_explicit_enable() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("remote.toml");
        fs::write(&path, "[embeddings]\nprovider = 'openai'\n").expect("config");
        let error =
            Config::load_layers(Some(&path), None, &ConfigOverrides::default(), temp.path())
                .expect_err("remote disabled");
        assert!(error.to_string().contains("unavailable in V1"));
    }

    #[test]
    fn semantic_retrieval_is_opt_in_and_bounded() {
        assert!(
            !Config::default().context.semantic_retrieval,
            "semantic retrieval must stay disabled until measured (Appendix E)"
        );

        let temp = TempDir::new().expect("temp dir");
        let enabled = temp.path().join("semantic.toml");
        fs::write(
            &enabled,
            "[context]\nsemantic_retrieval = true\n\n[semantic]\nmax_candidates = 25\nmax_scanned = 10000\n",
        )
        .expect("config");
        let config = Config::load_layers(
            Some(&enabled),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect("semantic retrieval is configurable");
        assert!(config.context.semantic_retrieval);
        assert_eq!(config.semantic.max_candidates, 25);
        assert_eq!(config.semantic.max_scanned, 10_000);

        let invalid = temp.path().join("semantic-invalid.toml");
        fs::write(
            &invalid,
            "[semantic]\nmax_candidates = 100\nmax_scanned = 10\n",
        )
        .expect("config");
        let error = Config::load_layers(
            Some(&invalid),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect_err("scan bound below candidate bound");
        assert!(error.to_string().contains("semantic.max_candidates"));
    }

    #[test]
    fn local_embedding_provider_is_machine_level_loopback_and_off_by_default() {
        assert_eq!(
            Config::default().semantic.provider,
            crate::semantic::EmbeddingProviderKind::Hashed,
            "the hashed embedder stays the default until a model is measured"
        );
        let temp = TempDir::new().expect("temp dir");
        let user = temp.path().join("user.toml");
        fs::write(
            &user,
            "[context]\nsemantic_retrieval = true\n\n[semantic]\nprovider = \"local\"\n\
             model = \"nomic-embed-text\"\ndimension = 768\nquery_prefix = \"search_query: \"\n\
             document_prefix = \"search_document: \"\nmin_similarity = 0.5\n",
        )
        .expect("user config");
        let project = temp.path().join("project.toml");
        fs::write(
            &project,
            "[semantic]\nmax_candidates = 10\nmax_scanned = 500\n",
        )
        .expect("project config");
        let config = Config::load_layers(
            Some(&user),
            Some(&project),
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect("machine-level provider with project-scoped bounds");
        assert_eq!(
            config.semantic.provider,
            crate::semantic::EmbeddingProviderKind::Local
        );
        assert_eq!(config.semantic.max_candidates, 10);
        assert_eq!(
            config.semantic.endpoint,
            "http://127.0.0.1:11434/v1/embeddings"
        );

        for setting in [
            "provider = \"local\"",
            "endpoint = \"http://127.0.0.1:9/v1/embeddings\"",
            "model = \"other\"",
            "document_prefix = \"x\"",
        ] {
            fs::write(&project, format!("[semantic]\n{setting}\n")).expect("project config");
            let error = Config::load_layers(
                Some(&user),
                Some(&project),
                &ConfigOverrides::default(),
                temp.path(),
            )
            .expect_err("a repository cannot redirect embeddings");
            assert!(error.to_string().contains("machine-level"), "{error}");
        }

        let remote = temp.path().join("remote.toml");
        fs::write(
            &remote,
            "[network]\nlocal_only = false\n\n[semantic]\nprovider = \"local\"\nmodel = \"m\"\n\
             endpoint = \"https://embeddings.example.com/v1/embeddings\"\n",
        )
        .expect("remote config");
        let error = Config::load_layers(
            Some(&remote),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect_err("remote embedding endpoints stay refused");
        assert!(error.to_string().contains("loopback"), "{error}");
    }

    #[test]
    fn compact_injection_defaults_on_and_stays_ablatable() {
        // Default ON since the 2026-09-01 A/B pair (capability parity at
        // −33% bytes; fidelity-gate parity 6/6 vs 6/6). The verbose ablation
        // must remain one flag away.
        assert!(Config::default().context.compact_injection);
        let temp = TempDir::new().expect("temp dir");
        let disabled = temp.path().join("compact.toml");
        fs::write(&disabled, "[context]\ncompact_injection = false\n").expect("config");
        let config = Config::load_layers(
            Some(&disabled),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect("compact injection is configurable");
        assert!(!config.context.compact_injection);
    }

    #[test]
    fn shipped_example_config_parses_and_validates() {
        let temp = TempDir::new().expect("temp dir");
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/engram.example.toml");
        let config = Config::load_layers(
            Some(&example),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect("example config must stay valid");
        assert!(config.jobs.enabled);
        assert_eq!(config.jobs.poll_interval_ms, 5_000);
        assert_eq!(config.jobs.max_attempts, 5);
        assert_eq!(config.jobs.context_run_retention_days, 90);
    }

    #[test]
    fn jobs_limits_are_validated() {
        let temp = TempDir::new().expect("temp dir");
        for (name, body, needle) in [
            (
                "poll.toml",
                "[jobs]\npoll_interval_ms = 0\n",
                "poll_interval_ms",
            ),
            (
                "attempts.toml",
                "[jobs]\nmax_attempts = 21\n",
                "max_attempts",
            ),
            (
                "retention.toml",
                "[jobs]\ncontext_run_retention_days = 0\n",
                "context_run_retention_days",
            ),
        ] {
            let path = temp.path().join(name);
            fs::write(&path, body).expect("config");
            let error =
                Config::load_layers(Some(&path), None, &ConfigOverrides::default(), temp.path())
                    .expect_err("invalid jobs limits");
            assert!(error.to_string().contains(needle), "{name}: {error}");
        }
    }

    #[test]
    fn synthesis_defaults_are_conservative_and_thresholds_are_validated() {
        let defaults = Config::default().synthesis;
        assert_eq!(defaults.min_supporting_episodes, 3);
        assert_eq!(defaults.min_distinct_uses, 3);
        assert!(!defaults.auto, "auto synthesis must be opt-in (spec P9)");
        assert!(
            !defaults.cross_project_hypotheses,
            "cross-project transfer must be opt-in (spec 16.5)"
        );

        let temp = TempDir::new().expect("temp dir");
        let invalid = temp.path().join("synthesis.toml");
        fs::write(&invalid, "[synthesis]\nmin_supporting_episodes = 1\n").expect("config");
        let error = Config::load_layers(
            Some(&invalid),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect_err("single-episode procedures are not allowed");
        assert!(error.to_string().contains("min_supporting_episodes"));

        let zero_uses = temp.path().join("synthesis-uses.toml");
        fs::write(&zero_uses, "[synthesis]\nmin_distinct_uses = 0\n").expect("config");
        let error = Config::load_layers(
            Some(&zero_uses),
            None,
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect_err("zero-use skill promotion is not allowed");
        assert!(error.to_string().contains("min_distinct_uses"));
    }

    #[test]
    fn test_policy_block_not_default() {
        assert_eq!(
            Config::default().policy.default_enforcement,
            Enforcement::Warn
        );
    }

    #[test]
    fn test_paths_expand_user_safely() {
        let temp = TempDir::new().expect("temp dir");
        let config = Config::load_layers(None, None, &ConfigOverrides::default(), temp.path())
            .expect("config");
        assert_eq!(
            config.storage.db_path,
            temp.path().join(".engram/brain.sqlite")
        );
        assert_eq!(config.workspace, "default");
        assert_eq!(
            config.daemon.socket_path,
            temp.path().join(".engram/engramd.sock")
        );
        assert!(config.web.enabled);
    }

    #[test]
    fn named_workspaces_isolate_paths_and_inherit_tuning_without_default_sync() {
        let temp = TempDir::new().expect("temp");
        let user = temp.path().join("user.toml");
        fs::write(
            &user,
            r#"
[storage]
db_path = "/a/custom/default.sqlite"
[daemon]
socket_path = "/a/custom/default.sock"
[vault]
enabled = true
root = "/a/custom/vault"
[context]
max_injected_tokens = 4200
[sync]
central_url = "https://default.example.com"
"#,
        )
        .expect("user config");
        let resolve = |name: &str| {
            Config::load_layers(
                Some(&user),
                None,
                &ConfigOverrides {
                    workspace: Some(name.to_owned()),
                    ..ConfigOverrides::default()
                },
                temp.path(),
            )
            .expect("workspace")
        };
        let first = resolve("first");
        let second = resolve("second");
        assert_eq!(
            first.storage.db_path,
            temp.path().join(".engram/workspaces/first/brain.sqlite")
        );
        assert_ne!(first.storage.db_path, second.storage.db_path);
        assert_ne!(first.daemon.socket_path, second.daemon.socket_path);
        assert_ne!(first.vault.root, second.vault.root);
        assert_eq!(first.context.max_injected_tokens, 4200);
        assert!(!first.web.enabled);
        assert!(first.sync.central_url.is_empty());
    }

    #[test]
    fn workspace_profile_and_project_layers_have_defined_precedence() {
        let temp = TempDir::new().expect("temp");
        let user = temp.path().join("user.toml");
        let project = temp.path().join("project.toml");
        let profile = temp.path().join(".engram/workspaces/team/config.toml");
        fs::create_dir_all(profile.parent().expect("parent")).expect("profile directory");
        fs::write(
            &user,
            "workspace = \"personal\"\n[context]\nmax_injected_tokens = 3000\n",
        )
        .expect("user");
        fs::write(&profile, "[context]\nmax_injected_tokens = 4000\nstable_prefix_tokens = 2000\n[web]\nenabled = true\nport = 12436\n").expect("profile");
        fs::write(
            &project,
            "workspace = \"team\"\n[context]\nmax_injected_tokens = 5000\n",
        )
        .expect("project");
        let config = Config::load_layers(
            Some(&user),
            Some(&project),
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect("config");
        assert_eq!(config.workspace, "team");
        assert_eq!(config.context.max_injected_tokens, 5000);
        assert_eq!(config.context.stable_prefix_tokens, 2000);
        assert!(config.web.enabled);
        assert_eq!(config.web.port, 12436);
        let overridden = Config::load_layers(
            Some(&user),
            Some(&project),
            &ConfigOverrides {
                workspace: Some("default".to_owned()),
                ..ConfigOverrides::default()
            },
            temp.path(),
        )
        .expect("explicit default");
        assert_eq!(overridden.workspace, "default");
        assert_eq!(
            overridden.storage.db_path,
            temp.path().join(".engram/brain.sqlite")
        );
        assert_eq!(overridden.context.stable_prefix_tokens, 1800);
    }

    #[test]
    fn cli_workspace_overrides_environment_overrides_config() {
        let merged = toml::from_str("workspace = \"project\"").expect("toml");
        assert_eq!(
            selected_workspace(&merged, &ConfigOverrides::default(), None).expect("project"),
            "project"
        );
        assert_eq!(
            selected_workspace(&merged, &ConfigOverrides::default(), Some("environment"))
                .expect("env"),
            "environment"
        );
        assert_eq!(
            selected_workspace(
                &merged,
                &ConfigOverrides {
                    workspace: Some("cli".to_owned()),
                    ..ConfigOverrides::default()
                },
                Some("environment")
            )
            .expect("cli"),
            "cli"
        );
    }

    #[test]
    fn client_configuration_root_finds_parent_config_without_registering_repo() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("project");
        let nested = root.join("src/module");
        fs::create_dir_all(root.join(".engram")).expect("config directory");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::write(root.join(".engram/config.toml"), "workspace = \"team\"\n")
            .expect("project config");
        assert_eq!(
            configuration_root(&nested).expect("read-only discovery"),
            root.canonicalize().expect("canonical root")
        );
        assert!(!root.join(".engram/project.toml").exists());
    }

    #[test]
    fn client_configuration_root_stops_at_worktree_boundary() {
        let temp = TempDir::new().expect("temp");
        let root = temp.path().join("worktree");
        let nested = root.join("src");
        fs::create_dir_all(temp.path().join(".engram")).expect("outer config directory");
        fs::write(
            temp.path().join(".engram/config.toml"),
            "workspace = \"unrelated\"\n",
        )
        .expect("outer config");
        fs::create_dir_all(&nested).expect("nested directory");
        fs::write(
            root.join(".git"),
            "gitdir: /unused/parent/.git/worktrees/tree\n",
        )
        .expect("worktree marker");
        assert_eq!(
            configuration_root(&nested).expect("read-only discovery"),
            root.canonicalize().expect("canonical root")
        );
    }

    #[test]
    fn named_workspace_cannot_override_paths_into_another_workspace() {
        let temp = TempDir::new().expect("temp");
        let error = Config::load_layers(
            None,
            None,
            &ConfigOverrides {
                workspace: Some("team".to_owned()),
                db_path: Some(temp.path().join(".engram/brain.sqlite")),
                ..ConfigOverrides::default()
            },
            temp.path(),
        )
        .expect_err("default database must not leak");
        assert!(error.to_string().contains("must stay inside"));
    }

    #[test]
    fn named_workspace_web_cannot_take_default_port() {
        let temp = TempDir::new().expect("temp");
        let project = temp.path().join("project.toml");
        fs::write(&project, "workspace = \"team\"\n[web]\nenabled = true\n").expect("project");
        let error = Config::load_layers(
            None,
            Some(&project),
            &ConfigOverrides::default(),
            temp.path(),
        )
        .expect_err("port conflict");
        assert!(error.to_string().contains("own explicit web.port"));
    }

    #[test]
    fn named_workspace_validation_rejects_subsequent_socket_override() {
        let temp = TempDir::new().expect("temp");
        let mut config = Config::load_layers(
            None,
            None,
            &ConfigOverrides {
                workspace: Some("team".to_owned()),
                ..ConfigOverrides::default()
            },
            temp.path(),
        )
        .expect("workspace");
        config.daemon.socket_path = temp.path().join(".engram/engramd.sock");
        assert!(
            config
                .validate()
                .expect_err("socket outside workspace")
                .to_string()
                .contains("must stay inside")
        );
        config.storage.db_path = temp.path().join(".engram/brain.sqlite");
        assert!(
            config
                .validate()
                .expect_err("database outside workspace")
                .to_string()
                .contains("must use its own")
        );
    }

    #[test]
    fn absolute_cli_database_does_not_require_home() {
        let config = Config::load_layers_inner(
            None,
            None,
            &ConfigOverrides {
                db_path: Some(PathBuf::from("/tmp/engram-absolute.sqlite")),
                local_only: None,
                workspace: None,
            },
            None,
            false,
        )
        .expect("absolute override");
        assert_eq!(
            config.storage.db_path,
            Path::new("/tmp/engram-absolute.sqlite")
        );
    }

    #[test]
    fn cache_budgets_are_validated() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("invalid-cache.toml");
        fs::write(
            &path,
            "[cache]\nenabled = true\ntotal_budget_mb = 1\nmax_active_projects = 4\nmax_item_kb = 1024\n",
        )
        .expect("config");
        let error =
            Config::load_layers(Some(&path), None, &ConfigOverrides::default(), temp.path())
                .expect_err("oversized cache item bound");
        assert!(error.to_string().contains("max_item_kb"));

        let zero = temp.path().join("zero-cache.toml");
        fs::write(&zero, "[cache]\nenabled = true\ntotal_budget_mb = 0\n").expect("config");
        let error =
            Config::load_layers(Some(&zero), None, &ConfigOverrides::default(), temp.path())
                .expect_err("zero cache budget");
        assert!(error.to_string().contains("positive"));
    }

    #[test]
    fn daemon_resource_limits_are_validated() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("invalid-daemon.toml");
        fs::write(
            &path,
            "[daemon]\nrss_soft_limit_mb = 900\nrss_hard_limit_mb = 900\n",
        )
        .expect("config");
        let error =
            Config::load_layers(Some(&path), None, &ConfigOverrides::default(), temp.path())
                .expect_err("invalid RSS limits");
        assert!(error.to_string().contains("soft < hard"));
    }

    #[test]
    fn proxy_rejects_non_loopback_bind() {
        let mut config = Config::default();
        config.proxy.bind = "0.0.0.0:11435".to_owned();
        let error = config.validate().expect_err("external bind must fail");
        assert!(error.to_string().contains("must be loopback"));
    }

    #[test]
    fn local_only_rejects_remote_runtime() {
        let mut config = Config::default();
        config.proxy.runtime_url = "https://runtime.example.com".to_owned();
        let error = config.validate().expect_err("remote runtime must fail");
        assert!(error.to_string().contains("allow_remote_runtime"));
    }

    #[test]
    fn remote_runtime_requires_both_explicit_network_opt_ins() {
        let mut config = Config::default();
        config.proxy.runtime_url = "https://runtime.example.com".to_owned();
        config.proxy.allow_remote_runtime = true;
        let error = config
            .validate()
            .expect_err("proxy opt-in alone must not disable local-only mode");
        assert!(error.to_string().contains("network.local_only = false"));

        config.proxy.allow_remote_runtime = false;
        config.network.local_only = false;
        let error = config
            .validate()
            .expect_err("network opt-in alone must not enable remote proxying");
        assert!(error.to_string().contains("allow_remote_runtime"));

        config.proxy.allow_remote_runtime = true;
        config
            .validate()
            .expect("both explicit opt-ins allow HTTPS");
    }

    #[test]
    fn remote_runtime_rejects_insecure_or_credentialed_urls() {
        let mut config = Config::default();
        config.network.local_only = false;
        config.proxy.allow_remote_runtime = true;

        config.proxy.runtime_url = "http://runtime.example.com".to_owned();
        let error = config
            .validate()
            .expect_err("remote plaintext HTTP must fail");
        assert!(error.to_string().contains("must use https"));

        config.proxy.runtime_url = "https://token@runtime.example.com".to_owned();
        let error = config.validate().expect_err("URL credentials must fail");
        assert!(error.to_string().contains("must not embed credentials"));

        config.proxy.runtime_url = "https://runtime.example.com?token=secret".to_owned();
        let error = config
            .validate()
            .expect_err("URL query credentials must not be accepted");
        assert!(error.to_string().contains("query or fragment"));
    }

    #[test]
    fn loopback_runtime_behavior_is_unchanged() {
        for runtime_url in [
            "http://127.0.0.1:8000",
            "http://[::1]:8000",
            "http://localhost:8000",
            "https://localhost:8000",
        ] {
            let mut config = Config::default();
            config.proxy.runtime_url = runtime_url.to_owned();
            config
                .validate()
                .unwrap_or_else(|error| panic!("{runtime_url} should remain valid: {error}"));
        }
    }
}
