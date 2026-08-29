//! Reproducible, paired brain-on/brain-off benchmark execution.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ignore::WalkBuilder;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result, io_error};
use crate::ids::EvaluationRunId;
use crate::storage::Database;

const MAX_CONFIG_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_METRICS_BYTES: u64 = 2 * 1_024 * 1_024;
const MAX_COPY_FILES: usize = 1_000_000;
const MAX_COPY_BYTES: u64 = 100 * 1_024 * 1_024 * 1_024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationMode {
    Off,
    Brain,
}

impl EvaluationMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Brain => "brain",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkConfig {
    pub benchmark_id: String,
    pub model_hash: String,
    pub modes: Vec<EvaluationMode>,
    pub seeds: Vec<u64>,
    #[serde(default = "default_wall_seconds")]
    pub wall_seconds: u64,
    pub artifact_dir: PathBuf,
    /// Optional immutable backup created by `leb backup create`. Every arm
    /// receives its own byte-identical copy.
    #[serde(default)]
    pub brain_snapshot: Option<PathBuf>,
    #[serde(default)]
    pub runtime_config: Value,
    pub tasks: Vec<BenchmarkTask>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkTask {
    pub id: String,
    pub repo: PathBuf,
    pub prompt_file: PathBuf,
    #[serde(default)]
    pub setup: Option<CommandSpec>,
    pub agent: CommandSpec,
    pub success: CommandSpec,
    #[serde(default)]
    pub metrics_file: Option<PathBuf>,
    #[serde(default)]
    pub limits: Option<TaskLimits>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSpec {
    pub command: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskLimits {
    pub wall_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EvaluationRecord {
    pub run_id: EvaluationRunId,
    pub benchmark_id: String,
    pub task_id: String,
    pub mode: EvaluationMode,
    pub seed: u64,
    pub ordinal: u64,
    pub model_hash: String,
    pub config_hash: String,
    pub brain_snapshot_hash: String,
    pub success: bool,
    pub timed_out: bool,
    pub setup_exit_code: Option<i32>,
    pub agent_exit_code: Option<i32>,
    pub validation_exit_code: Option<i32>,
    pub wall_ms: u64,
    pub metrics: Value,
    pub artifact_path: PathBuf,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModeSummary {
    pub mode: EvaluationMode,
    pub runs: u64,
    pub successes: u64,
    pub success_rate: f64,
    pub mean_wall_ms: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ComparisonReport {
    pub benchmark_id: String,
    pub model_hash: String,
    pub config_hash: String,
    pub paired_runs: u64,
    pub off: ModeSummary,
    pub brain: ModeSummary,
    pub success_rate_delta: f64,
    pub mean_wall_time_delta_pct: Option<f64>,
    pub generated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvaluationArtifacts {
    pub jsonl_path: PathBuf,
    pub comparison_json_path: PathBuf,
    pub comparison_markdown_path: PathBuf,
    pub records: Vec<EvaluationRecord>,
    pub comparison: ComparisonReport,
}

#[derive(Clone, Debug)]
pub struct EvaluationStore {
    database: Database,
}

impl EvaluationStore {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    /// Persist one completed run's compact metrics.
    ///
    /// # Errors
    ///
    /// Returns an error if the record is invalid or storage fails.
    pub fn record(&self, record: &EvaluationRecord) -> Result<()> {
        let metrics = serde_json::to_string(record)?;
        self.database.write_immediate(|transaction| {
            transaction.execute(
                "INSERT INTO eval_runs
                 (id, benchmark_id, mode, model_hash, config_hash, metrics_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO NOTHING",
                params![
                    record.run_id.as_str(),
                    record.benchmark_id,
                    record.mode.as_str(),
                    record.model_hash,
                    record.config_hash,
                    metrics,
                    record.created_at_ms
                ],
            )?;
            Ok(())
        })
    }
}

pub struct EvaluationRunner;

impl EvaluationRunner {
    /// Run every task/seed as a strict off/brain pair in isolated repository copies.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe configuration, isolation failures, command
    /// launch failures, artifact failures, or an incomplete pair.
    pub fn run(config_path: &Path, output_override: Option<&Path>) -> Result<EvaluationArtifacts> {
        let (config, raw_config_hash) = load_config(config_path)?;
        validate_config(&config)?;
        let base = config_path.parent().unwrap_or_else(|| Path::new("."));
        let artifact_dir = output_override.map_or_else(
            || resolve_existing_or_relative(base, &config.artifact_dir),
            Path::to_path_buf,
        );
        prepare_artifact_dir(&artifact_dir)?;
        let jsonl_path = artifact_dir.join("runs.jsonl");
        let mut jsonl = create_private_file(&jsonl_path, true)?;
        let isolation = tempfile::Builder::new()
            .prefix("leb-eval-")
            .tempdir()
            .map_err(|source| io_error(std::env::temp_dir(), source))?;
        let (baseline, brain_snapshot_hash) =
            prepare_brain_baseline(&config, base, isolation.path())?;
        let config_hash =
            sha256(format!("{raw_config_hash}\0brain-snapshot\0{brain_snapshot_hash}").as_bytes());
        let mut records = Vec::new();
        let mut ordinal = 0_u64;

        for (task_index, task) in config.tasks.iter().enumerate() {
            for (seed_index, seed) in config.seeds.iter().copied().enumerate() {
                let order = if (task_index + seed_index) % 2 == 0 {
                    [EvaluationMode::Off, EvaluationMode::Brain]
                } else {
                    [EvaluationMode::Brain, EvaluationMode::Off]
                };
                for mode in order {
                    ordinal = ordinal.saturating_add(1);
                    let record = run_one(
                        &config,
                        &config_hash,
                        base,
                        task,
                        seed,
                        mode,
                        ordinal,
                        isolation.path(),
                        &artifact_dir,
                        &baseline,
                        &brain_snapshot_hash,
                    )?;
                    serde_json::to_writer(&mut jsonl, &record)?;
                    jsonl
                        .write_all(b"\n")
                        .map_err(|source| io_error(&jsonl_path, source))?;
                    jsonl
                        .flush()
                        .map_err(|source| io_error(&jsonl_path, source))?;
                    records.push(record);
                }
            }
        }
        jsonl
            .sync_all()
            .map_err(|source| io_error(&jsonl_path, source))?;
        let comparison = compare_records(
            &config.benchmark_id,
            &config.model_hash,
            &config_hash,
            &records,
        )?;
        let comparison_json_path = artifact_dir.join("comparison.json");
        write_atomic(
            &comparison_json_path,
            &serde_json::to_vec_pretty(&comparison)?,
        )?;
        let comparison_markdown_path = artifact_dir.join("comparison.md");
        write_atomic(
            &comparison_markdown_path,
            comparison_markdown(&comparison).as_bytes(),
        )?;
        Ok(EvaluationArtifacts {
            jsonl_path,
            comparison_json_path,
            comparison_markdown_path,
            records,
            comparison,
        })
    }
}

fn prepare_brain_baseline(
    config: &BenchmarkConfig,
    base: &Path,
    isolation_root: &Path,
) -> Result<(PathBuf, String)> {
    let baseline = isolation_root.join("frozen-brain.sqlite");
    if let Some(snapshot) = &config.brain_snapshot {
        let source = canonical_required(base, snapshot)?;
        let metadata = fs::metadata(&source).map_err(|error| io_error(&source, error))?;
        if !metadata.is_file() || metadata.len() > MAX_COPY_BYTES {
            return Err(Error::Config(
                "brain_snapshot must be a regular file within the evaluation copy limit".to_owned(),
            ));
        }
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = source.as_os_str().to_os_string();
            sidecar.push(suffix);
            let sidecar = PathBuf::from(sidecar);
            if sidecar.metadata().is_ok_and(|metadata| metadata.len() > 0) {
                return Err(Error::Config(format!(
                    "brain_snapshot must be a standalone `leb backup create` file; live sidecar exists: {}",
                    sidecar.display()
                )));
            }
        }
        Database::verify_existing(&source)?;
        fs::copy(&source, &baseline).map_err(|error| io_error(&baseline, error))?;
    } else {
        let seed = isolation_root.join("empty-seed.sqlite");
        let database = Database::open(&seed)?;
        database.backup(&baseline)?;
        drop(database);
    }
    let report = Database::verify_existing(&baseline)?;
    if report.integrity != "ok" {
        return Err(Error::Config(format!(
            "brain snapshot integrity check failed: {}",
            report.integrity
        )));
    }
    let hash = sha256_file(&baseline)?;
    Ok((baseline, hash))
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
fn run_one(
    config: &BenchmarkConfig,
    config_hash: &str,
    base: &Path,
    task: &BenchmarkTask,
    seed: u64,
    mode: EvaluationMode,
    ordinal: u64,
    isolation_root: &Path,
    artifact_dir: &Path,
    brain_baseline: &Path,
    brain_snapshot_hash: &str,
) -> Result<EvaluationRecord> {
    let source_repo = canonical_required(base, &task.repo)?;
    let prompt_path = canonical_required(base, &task.prompt_file)?;
    let prompt_metadata =
        fs::metadata(&prompt_path).map_err(|source| io_error(&prompt_path, source))?;
    if !prompt_metadata.is_file() || prompt_metadata.len() > MAX_CONFIG_BYTES {
        return Err(Error::Config(format!(
            "benchmark prompt must be a regular file no larger than {MAX_CONFIG_BYTES} bytes"
        )));
    }
    let run_name = format!("{:06}-{}-{}-{seed}", ordinal, task.id, mode.as_str());
    let worktree = isolation_root.join(&run_name);
    copy_repository(&source_repo, &worktree)?;
    let run_artifacts = artifact_dir.join(&run_name);
    create_private_dir_all(&run_artifacts)?;
    let state_root = isolation_root.join(format!("{run_name}-state"));
    fs::create_dir(&state_root).map_err(|source| io_error(&state_root, source))?;
    make_private_dir(&state_root)?;
    let arm_database = state_root.join("brain.sqlite");
    fs::copy(brain_baseline, &arm_database).map_err(|source| io_error(&arm_database, source))?;
    let arm_socket = state_root.join("lebd.sock");
    let proxy_port = reserve_loopback_port()?;
    let timeout = Duration::from_secs(
        task.limits
            .as_ref()
            .map_or(config.wall_seconds, |limits| limits.wall_seconds),
    );
    let started = Instant::now();
    let environment = vec![
        ("LEB_MODE", mode.as_str().to_owned()),
        ("LEB_SEED", seed.to_string()),
        ("LEB_TASK_ID", task.id.clone()),
        ("LEB_BENCHMARK_ID", config.benchmark_id.clone()),
        ("LEB_DB_PATH", arm_database.to_string_lossy().into_owned()),
        ("LEB_SOCKET_PATH", arm_socket.to_string_lossy().into_owned()),
        ("LEB_STATE_DIR", state_root.to_string_lossy().into_owned()),
        ("LEB_PROXY_PORT", proxy_port.to_string()),
        ("LEB_BRAIN_SNAPSHOT_HASH", brain_snapshot_hash.to_owned()),
    ];
    let setup = task
        .setup
        .as_ref()
        .map(|command| {
            run_command(
                &command.command,
                &worktree,
                &environment,
                remaining(timeout, started),
                &run_artifacts.join("setup.stdout"),
                &run_artifacts.join("setup.stderr"),
                &prompt_path,
            )
        })
        .transpose()?;
    let mut timed_out = setup.as_ref().is_some_and(|result| result.timed_out);
    let setup_ok = setup.as_ref().is_none_or(|result| result.status.success());
    let agent = if setup_ok && !timed_out {
        Some(run_command(
            &task.agent.command,
            &worktree,
            &environment,
            remaining(timeout, started),
            &run_artifacts.join("agent.stdout"),
            &run_artifacts.join("agent.stderr"),
            &prompt_path,
        )?)
    } else {
        None
    };
    timed_out |= agent.as_ref().is_some_and(|result| result.timed_out);
    let agent_ok = agent.as_ref().is_some_and(|result| result.status.success());
    let validation = if agent_ok && !timed_out {
        Some(run_command(
            &task.success.command,
            &worktree,
            &environment,
            remaining(timeout, started),
            &run_artifacts.join("validation.stdout"),
            &run_artifacts.join("validation.stderr"),
            &prompt_path,
        )?)
    } else {
        None
    };
    timed_out |= validation.as_ref().is_some_and(|result| result.timed_out);
    let success = !timed_out
        && setup_ok
        && agent_ok
        && validation
            .as_ref()
            .is_some_and(|result| result.status.success());
    let metrics = task
        .metrics_file
        .as_ref()
        .map(|path| read_metrics(&worktree, path))
        .transpose()?
        .unwrap_or(Value::Null);
    Ok(EvaluationRecord {
        run_id: EvaluationRunId::new(),
        benchmark_id: config.benchmark_id.clone(),
        task_id: task.id.clone(),
        mode,
        seed,
        ordinal,
        model_hash: config.model_hash.clone(),
        config_hash: config_hash.to_owned(),
        brain_snapshot_hash: brain_snapshot_hash.to_owned(),
        success,
        timed_out,
        setup_exit_code: setup.as_ref().and_then(|result| result.status.code()),
        agent_exit_code: agent.as_ref().and_then(|result| result.status.code()),
        validation_exit_code: validation.as_ref().and_then(|result| result.status.code()),
        wall_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        metrics,
        artifact_path: run_artifacts,
        created_at_ms: unix_time_ms(),
    })
}

struct CommandResult {
    status: ExitStatus,
    timed_out: bool,
}

#[allow(clippy::too_many_arguments)]
fn run_command(
    command: &str,
    cwd: &Path,
    environment: &[(impl AsRef<std::ffi::OsStr>, String)],
    timeout: Duration,
    stdout_path: &Path,
    stderr_path: &Path,
    prompt_path: &Path,
) -> Result<CommandResult> {
    if command.trim().is_empty() {
        return Err(Error::Config(
            "benchmark command cannot be empty".to_owned(),
        ));
    }
    let command = command.replace("{prompt_file}", &shell_quote(prompt_path));
    let stdout = create_private_file(stdout_path, true)?;
    let stderr = create_private_file(stderr_path, true)?;
    let mut command_builder = Command::new("/bin/sh");
    command_builder
        .args(["-c", &command])
        .current_dir(cwd)
        .env_clear()
        .envs(safe_environment())
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command_builder.process_group(0);
    }
    let mut process = command_builder
        .spawn()
        .map_err(|error| Error::Config(format!("benchmark command failed to start: {error}")))?;
    let process_group = process.id();
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = process
            .try_wait()
            .map_err(|error| Error::Config(format!("benchmark wait failed: {error}")))?
        {
            terminate_process_group(process_group, false);
            return Ok(CommandResult {
                status,
                timed_out: false,
            });
        }
        if Instant::now() >= deadline {
            terminate_process_group(process_group, true);
            let status = process
                .wait()
                .map_err(|error| Error::Config(format!("benchmark reap failed: {error}")))?;
            return Ok(CommandResult {
                status,
                timed_out: true,
            });
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn reserve_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|error| {
        Error::Config(format!("could not reserve evaluation proxy port: {error}"))
    })?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| Error::Config(format!("could not inspect evaluation proxy port: {error}")))
}

#[cfg(unix)]
fn terminate_process_group(process_group: u32, graceful_wait: bool) {
    let group = format!("-{process_group}");
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if graceful_wait {
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn terminate_process_group(_process_group: u32, _graceful_wait: bool) {}

fn compare_records(
    benchmark_id: &str,
    model_hash: &str,
    config_hash: &str,
    records: &[EvaluationRecord],
) -> Result<ComparisonReport> {
    let mut pairs = BTreeMap::<(&str, u64), BTreeMap<EvaluationMode, &EvaluationRecord>>::new();
    for record in records {
        pairs
            .entry((&record.task_id, record.seed))
            .or_default()
            .insert(record.mode, record);
    }
    if pairs.values().any(|pair| {
        !pair.contains_key(&EvaluationMode::Off) || !pair.contains_key(&EvaluationMode::Brain)
    }) {
        return Err(Error::Config(
            "evaluation report requires one off and one brain run per task/seed".to_owned(),
        ));
    }
    let off = summarize(EvaluationMode::Off, records);
    let brain = summarize(EvaluationMode::Brain, records);
    let wall_delta = (off.mean_wall_ms > 0.0)
        .then(|| ((brain.mean_wall_ms - off.mean_wall_ms) / off.mean_wall_ms) * 100.0);
    Ok(ComparisonReport {
        benchmark_id: benchmark_id.to_owned(),
        model_hash: model_hash.to_owned(),
        config_hash: config_hash.to_owned(),
        paired_runs: u64::try_from(pairs.len()).unwrap_or(u64::MAX),
        success_rate_delta: brain.success_rate - off.success_rate,
        mean_wall_time_delta_pct: wall_delta,
        off,
        brain,
        generated_at_ms: unix_time_ms(),
    })
}

#[allow(clippy::cast_precision_loss)]
fn summarize(mode: EvaluationMode, records: &[EvaluationRecord]) -> ModeSummary {
    let selected = records.iter().filter(|record| record.mode == mode);
    let (runs, successes, wall_ms) = selected.fold((0_u64, 0_u64, 0_u128), |acc, record| {
        (
            acc.0.saturating_add(1),
            acc.1.saturating_add(u64::from(record.success)),
            acc.2.saturating_add(u128::from(record.wall_ms)),
        )
    });
    ModeSummary {
        mode,
        runs,
        successes,
        success_rate: if runs == 0 {
            0.0
        } else {
            successes as f64 / runs as f64
        },
        mean_wall_ms: if runs == 0 {
            0.0
        } else {
            wall_ms as f64 / runs as f64
        },
    }
}

fn load_config(path: &Path) -> Result<(BenchmarkConfig, String)> {
    let metadata = fs::metadata(path).map_err(|source| io_error(path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(Error::Config(format!(
            "benchmark config must be a regular file no larger than {MAX_CONFIG_BYTES} bytes"
        )));
    }
    let bytes = fs::read(path).map_err(|source| io_error(path, source))?;
    let config = match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => serde_json::from_slice(&bytes)?,
        Some("toml") => toml::from_str(&String::from_utf8_lossy(&bytes))?,
        _ => yaml_serde::from_slice(&bytes).map_err(|error| Error::Yaml(error.to_string()))?,
    };
    Ok((config, sha256(&bytes)))
}

fn validate_config(config: &BenchmarkConfig) -> Result<()> {
    validate_id("benchmark", &config.benchmark_id)?;
    if config.model_hash.trim().is_empty() || config.model_hash.len() > 512 {
        return Err(Error::Config(
            "model_hash must be non-empty and at most 512 bytes".to_owned(),
        ));
    }
    if config.wall_seconds == 0 || config.tasks.is_empty() || config.seeds.is_empty() {
        return Err(Error::Config(
            "evaluation needs positive wall_seconds, tasks, and seeds".to_owned(),
        ));
    }
    let modes = config
        .modes
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if modes
        != [EvaluationMode::Off, EvaluationMode::Brain]
            .into_iter()
            .collect()
    {
        return Err(Error::Config(
            "paired evaluation modes must contain exactly `off` and `brain`".to_owned(),
        ));
    }
    for task in &config.tasks {
        validate_id("task", &task.id)?;
        if task
            .limits
            .as_ref()
            .is_some_and(|limits| limits.wall_seconds == 0)
        {
            return Err(Error::Config(
                "task wall_seconds must be greater than zero".to_owned(),
            ));
        }
        if let Some(path) = &task.metrics_file {
            validate_relative(path, "metrics_file")?;
        }
    }
    Ok(())
}

fn validate_id(kind: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(Error::Config(format!(
            "{kind} id must contain only ASCII letters, digits, `-`, or `_`"
        )));
    }
    Ok(())
}

fn copy_repository(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination).map_err(|source| io_error(destination, source))?;
    let mut files = 0_usize;
    let mut bytes = 0_u64;
    for entry in WalkBuilder::new(source)
        .hidden(false)
        .follow_links(false)
        .build()
    {
        let entry =
            entry.map_err(|error| Error::Config(format!("repository copy failed: {error}")))?;
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| Error::Config("repository copy escaped source".to_owned()))?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = destination.join(relative);
        let kind = entry
            .file_type()
            .ok_or_else(|| Error::Config("repository entry has unknown type".to_owned()))?;
        if kind.is_symlink() {
            return Err(Error::Config(format!(
                "benchmark repositories may not contain symlinks: {}",
                entry.path().display()
            )));
        }
        if kind.is_dir() {
            fs::create_dir_all(&target).map_err(|source| io_error(&target, source))?;
        } else if kind.is_file() {
            files = files.saturating_add(1);
            let size = entry
                .metadata()
                .map_err(|error| Error::Config(error.to_string()))?
                .len();
            bytes = bytes.saturating_add(size);
            if files > MAX_COPY_FILES || bytes > MAX_COPY_BYTES {
                return Err(Error::Config(
                    "benchmark repository exceeds isolation limits".to_owned(),
                ));
            }
            fs::copy(entry.path(), &target).map_err(|source| io_error(&target, source))?;
        }
    }
    Ok(())
}

fn read_metrics(root: &Path, relative: &Path) -> Result<Value> {
    validate_relative(relative, "metrics_file")?;
    let path = root.join(relative);
    if !path.exists() {
        return Ok(Value::Null);
    }
    let metadata = fs::metadata(&path).map_err(|source| io_error(&path, source))?;
    if !metadata.is_file() || metadata.len() > MAX_METRICS_BYTES {
        return Err(Error::Config(
            "benchmark metrics file is invalid or oversized".to_owned(),
        ));
    }
    serde_json::from_slice(&fs::read(&path).map_err(|source| io_error(&path, source))?)
        .map_err(Error::from)
}

fn canonical_required(base: &Path, value: &Path) -> Result<PathBuf> {
    let path = if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    };
    path.canonicalize()
        .map_err(|source| io_error(&path, source))
}

fn resolve_existing_or_relative(base: &Path, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        base.join(value)
    }
}

fn validate_relative(path: &Path, field: &str) -> Result<()> {
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(Error::Config(format!(
            "{field} must be a contained relative path"
        )));
    }
    Ok(())
}

fn safe_environment() -> Vec<(String, String)> {
    ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
        .collect()
}

fn prepare_artifact_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(Error::Config(format!(
                    "evaluation artifact path must be a real directory: {}",
                    path.display()
                )));
            }
            if fs::read_dir(path)
                .map_err(|source| io_error(path, source))?
                .next()
                .is_some()
            {
                return Err(Error::Config(format!(
                    "evaluation artifact directory is not empty; choose a new --output path: {}",
                    path.display()
                )));
            }
            make_private_dir(path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_private_dir_all(path),
        Err(source) => Err(io_error(path, source)),
    }
}

fn create_private_dir_all(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
    make_private_dir(path)
}

#[cfg(unix)]
fn make_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error(path, source))
}

#[cfg(not(unix))]
const fn make_private_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn create_private_file(path: &Path, create_new: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(create_new);
    if !create_new {
        options.create(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|source| io_error(path, source))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| io_error(path, source))?;
    }
    Ok(file)
}

fn remaining(limit: Duration, started: Instant) -> Duration {
    limit
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(1))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = create_private_file(&temporary, true)?;
    file.write_all(bytes)
        .map_err(|source| io_error(&temporary, source))?;
    file.sync_all()
        .map_err(|source| io_error(&temporary, source))?;
    fs::rename(&temporary, path).map_err(|source| io_error(path, source))
}

fn comparison_markdown(report: &ComparisonReport) -> String {
    format!(
        "# LEB paired evaluation: {}\n\nModel: `{}`  \nConfig: `{}`  \nPairs: {}\n\n| Mode | Runs | Successes | Success rate | Mean wall ms |\n|---|---:|---:|---:|---:|\n| off | {} | {} | {:.3} | {:.1} |\n| brain | {} | {} | {:.3} | {:.1} |\n\nSuccess-rate delta: **{:+.3}**  \nMean wall-time delta: **{}**\n",
        report.benchmark_id,
        report.model_hash,
        report.config_hash,
        report.paired_runs,
        report.off.runs,
        report.off.successes,
        report.off.success_rate,
        report.off.mean_wall_ms,
        report.brain.runs,
        report.brain.successes,
        report.brain.success_rate,
        report.brain.mean_wall_ms,
        report.success_rate_delta,
        report
            .mean_wall_time_delta_pct
            .map_or_else(|| "n/a".to_owned(), |value| format!("{value:+.2}%")),
    )
}

const fn default_wall_seconds() -> u64 {
    900
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1_024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_error(path, source))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    Ok(output)
}

fn unix_time_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn paired_runner_isolates_modes_and_emits_reports() {
        let temp = TempDir::new().expect("temp");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("repo");
        fs::write(repo.join("state"), "clean").expect("state");
        let prompt = temp.path().join("prompt.txt");
        fs::write(&prompt, "do the task").expect("prompt");
        let config_path = temp.path().join("bench.yaml");
        fs::write(
            &config_path,
            format!(
                "benchmark_id: smoke\nmodel_hash: model-1\nmodes: [off, brain]\nseeds: [1, 2]\nwall_seconds: 5\nartifact_dir: artifacts\ntasks:\n  - id: task_1\n    repo: {}\n    prompt_file: {}\n    agent:\n      command: \"printf '{{\\\"db\\\":\\\"%s\\\",\\\"socket\\\":\\\"%s\\\",\\\"snapshot\\\":\\\"%s\\\"}}' \\\"$LEB_DB_PATH\\\" \\\"$LEB_SOCKET_PATH\\\" \\\"$LEB_BRAIN_SNAPSHOT_HASH\\\" > metrics.json; printf '%s' \\\"$LEB_MODE\\\" > mode\"\n    success:\n      command: \"test -s mode && test \\\"$(cat state)\\\" = clean\"\n    metrics_file: metrics.json\n",
                repo.display(),
                prompt.display()
            ),
        )
        .expect("config");
        let artifacts = EvaluationRunner::run(&config_path, None).expect("run");
        assert_eq!(artifacts.records.len(), 4);
        assert_eq!(artifacts.comparison.paired_runs, 2);
        assert!((artifacts.comparison.off.success_rate - 1.0).abs() < f64::EPSILON);
        assert!((artifacts.comparison.brain.success_rate - 1.0).abs() < f64::EPSILON);
        assert!(artifacts.jsonl_path.exists());
        assert!(artifacts.comparison_markdown_path.exists());
        let databases = artifacts
            .records
            .iter()
            .map(|record| record.metrics["db"].as_str().expect("db"))
            .collect::<std::collections::BTreeSet<_>>();
        let sockets = artifacts
            .records
            .iter()
            .map(|record| record.metrics["socket"].as_str().expect("socket"))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(databases.len(), 4);
        assert_eq!(sockets.len(), 4);
        assert!(artifacts.records.iter().all(|record| {
            record.metrics["snapshot"].as_str() == Some(&record.brain_snapshot_hash)
        }));
        let rerun = EvaluationRunner::run(&config_path, None)
            .expect_err("a rerun must not overwrite retained evidence");
        assert!(rerun.to_string().contains("not empty"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let artifact_mode = fs::metadata(temp.path().join("artifacts"))
                .expect("artifact metadata")
                .permissions()
                .mode()
                & 0o777;
            let jsonl_mode = fs::metadata(&artifacts.jsonl_path)
                .expect("jsonl metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(artifact_mode, 0o700);
            assert_eq!(jsonl_mode, 0o600);
        }
    }

    #[test]
    fn rejects_incomplete_control_modes_and_escaping_metrics() {
        let config = BenchmarkConfig {
            benchmark_id: "safe".to_owned(),
            model_hash: "model".to_owned(),
            modes: vec![EvaluationMode::Brain],
            seeds: vec![1],
            wall_seconds: 1,
            artifact_dir: PathBuf::from("out"),
            brain_snapshot: None,
            runtime_config: Value::Null,
            tasks: vec![BenchmarkTask {
                id: "task".to_owned(),
                repo: PathBuf::from("repo"),
                prompt_file: PathBuf::from("prompt"),
                setup: None,
                agent: CommandSpec {
                    command: "true".to_owned(),
                },
                success: CommandSpec {
                    command: "true".to_owned(),
                },
                metrics_file: Some(PathBuf::from("../outside")),
                limits: None,
            }],
        };
        assert!(validate_config(&config).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn command_cleanup_terminates_background_process_group() {
        let temp = TempDir::new().expect("temp");
        let prompt = temp.path().join("prompt");
        fs::write(&prompt, "prompt").expect("prompt");
        let result = run_command(
            "sleep 30 & echo $! > child.pid",
            temp.path(),
            &[] as &[(String, String)],
            Duration::from_secs(2),
            &temp.path().join("stdout"),
            &temp.path().join("stderr"),
            &prompt,
        )
        .expect("command");
        assert!(result.status.success());
        let pid = fs::read_to_string(temp.path().join("child.pid")).expect("pid");
        let alive = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("kill probe")
            .success();
        assert!(!alive, "background child survived benchmark command");
    }
}
