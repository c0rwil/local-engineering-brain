use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result, io_error};

/// Fully resolved runtime configuration.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub brain: BrainConfig,
    pub cache: CacheConfig,
    pub context: ContextConfig,
    pub daemon: DaemonConfig,
    pub storage: StorageConfig,
    pub structural: StructuralConfig,
    pub embeddings: EmbeddingsConfig,
    pub policy: PolicyConfig,
    pub proxy: ProxyConfig,
    pub security: SecurityConfig,
    pub network: NetworkConfig,
    pub evaluation: EvaluationConfig,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrainConfig {
    pub auto_learn: bool,
    pub conservative_promotion: bool,
    pub raw_transcript_retention_days: u32,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Reserved for the post-V1 derived-cache integration. Enabling it is
    /// rejected until cache invalidation is wired to canonical generations.
    pub enabled: bool,
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

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextConfig {
    pub max_injected_tokens: u32,
    pub stable_prefix_tokens: u32,
    pub semantic_retrieval: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub db_path: PathBuf,
    pub blob_budget_gb: u32,
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
            socket_path: PathBuf::from("~/.leb/lebd.sock"),
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
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            db_path: PathBuf::from("~/.leb/brain.sqlite"),
            blob_budget_gb: 0,
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
            failure_mode: ProxyFailureMode::FailOpen,
        }
    }
}

impl Config {
    /// Load defaults, user config, project config, then CLI overrides.
    ///
    /// # Errors
    ///
    /// Returns an error for an unreadable/malformed layer or an invalid merged configuration.
    pub fn load_for(project_root: Option<&Path>, overrides: &ConfigOverrides) -> Result<Self> {
        let home = home_dir();
        let user_config = home
            .as_ref()
            .map(|home| home.join(".config/leb/config.toml"));
        let project_config = project_root.map(|root| root.join(".leb/config.toml"));
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
        let mut merged = toml::Value::try_from(Self::default())?;
        for path in [user_config, project_config].into_iter().flatten() {
            if path.exists() {
                let raw = fs::read_to_string(path).map_err(|source| io_error(path, source))?;
                let layer = toml::from_str::<toml::Value>(&raw).map_err(Error::TomlDeserialize)?;
                merge_toml(&mut merged, layer);
            }
        }

        let mut config: Self = merged.try_into().map_err(Error::TomlDeserialize)?;
        let default_socket = DaemonConfig::default().socket_path;
        let database_overridden = overrides.db_path.is_some()
            || (apply_environment && std::env::var_os("LEB_DB_PATH").is_some());
        let socket_overridden = apply_environment && std::env::var_os("LEB_SOCKET_PATH").is_some();
        let socket_was_default = config.daemon.socket_path == default_socket;
        if apply_environment {
            if let Some(path) = std::env::var_os("LEB_DB_PATH") {
                config.storage.db_path = PathBuf::from(path);
            }
            if let Some(path) = std::env::var_os("LEB_SOCKET_PATH") {
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
                    .join("lebd.sock")
            } else {
                expand_home(&config.daemon.socket_path, home)?
            };
        config.validate()?;
        Ok(config)
    }

    /// Validate safety constraints and token budgets.
    ///
    /// # Errors
    ///
    /// Returns an error when token budgets conflict or a remote provider lacks explicit opt-in.
    pub fn validate(&self) -> Result<()> {
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
        crate::security::Redactor::new(&self.security.custom_redaction_patterns)?;

        validate_proxy_network(self)?;
        Ok(())
    }
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
        return Err(Error::Config(
            "derived in-process caching is reserved until generation invalidation is integrated"
                .to_owned(),
        ));
    }
    if config.context.semantic_retrieval {
        return Err(Error::Config(
            "semantic retrieval is unavailable in V1; use deterministic lexical/structural retrieval"
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
    if config.network.local_only {
        let local = runtime.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if !local {
            return Err(Error::Config(
                "local-only mode requires a loopback proxy.runtime_url".to_owned(),
            ));
        }
    }
    Ok(())
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
        .join(format!(".lebd-{short}.sock"))
}

#[cfg(test)]
mod tests {
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
            temp.path().join(".leb/brain.sqlite")
        );
    }

    #[test]
    fn absolute_cli_database_does_not_require_home() {
        let config = Config::load_layers_inner(
            None,
            None,
            &ConfigOverrides {
                db_path: Some(PathBuf::from("/tmp/leb-absolute.sqlite")),
                local_only: None,
            },
            None,
            false,
        )
        .expect("absolute override");
        assert_eq!(
            config.storage.db_path,
            Path::new("/tmp/leb-absolute.sqlite")
        );
    }

    #[test]
    fn unsupported_cache_enable_is_rejected() {
        let temp = TempDir::new().expect("temp dir");
        let path = temp.path().join("invalid-cache.toml");
        fs::write(&path, "[cache]\nenabled = true\n").expect("config");
        let error =
            Config::load_layers(Some(&path), None, &ConfigOverrides::default(), temp.path())
                .expect_err("unsupported cache");
        assert!(error.to_string().contains("reserved"));
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
        assert!(error.to_string().contains("loopback proxy.runtime_url"));
    }
}
