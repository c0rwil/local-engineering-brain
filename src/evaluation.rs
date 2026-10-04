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
const ALLOWED_PASSTHROUGH_ENV: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

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
    /// Optional immutable backup created by `engram backup create`. Every arm
    /// receives its own byte-identical copy.
    #[serde(default)]
    pub brain_snapshot: Option<PathBuf>,
    #[serde(default)]
    pub runtime_config: Value,
    /// Credential environment variable names copied into the agent command
    /// only. Values are resolved at launch and never enter config artifacts.
    #[serde(default)]
    pub pass_env: Vec<String>,
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
    pub success_rate_delta: Option<f64>,
    pub mean_wall_time_delta_pct: Option<f64>,
    pub generated_at_ms: i64,
}

/// Observed resource differences on matched off/brain trials. A metric uses
/// only pairs where both arms reported it; missing values are never zero.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PairedUsageMetric {
    pub measured_pairs: u64,
    pub off_total: Option<f64>,
    pub brain_total: Option<f64>,
    /// Positive means brain used more; negative means brain used less.
    pub brain_minus_off_pct: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationOutcome {
    Pass,
    Fail,
    InfrastructureError,
    Unknown,
}

impl EvaluationRecord {
    /// Classify completion separately from a saved behavioral verdict. Explicit
    /// outcomes cannot turn a known runtime or validation failure into a pass.
    #[must_use]
    pub fn outcome(&self) -> EvaluationOutcome {
        let explicit = self
            .metrics
            .get("outcome")
            .map(|outcome| match outcome.as_str() {
                Some("pass") => EvaluationOutcome::Pass,
                Some("fail") => EvaluationOutcome::Fail,
                Some("infrastructure_error") => EvaluationOutcome::InfrastructureError,
                _ => EvaluationOutcome::Unknown,
            });
        if let Some(
            outcome @ (EvaluationOutcome::InfrastructureError | EvaluationOutcome::Unknown),
        ) = explicit
        {
            return outcome;
        }
        let invalid_controls = metric_objects(&self.metrics).any(|metrics| {
            metrics
                .get("validity_issues")
                .and_then(Value::as_array)
                .is_some_and(|issues| {
                    issues.iter().any(|issue| {
                        matches!(issue.as_str(), Some("model_drift" | "no_delivery_receipt"))
                    })
                })
        });
        if self.setup_exit_code.is_some_and(|code| code != 0) || invalid_controls {
            return EvaluationOutcome::InfrastructureError;
        }
        let budget_exhausted = self.timed_out
            || metric_objects(&self.metrics).any(|metrics| {
                metrics.get("agent_timed_out").and_then(Value::as_bool) == Some(true)
                    || metrics
                        .get("result_subtype")
                        .and_then(Value::as_str)
                        .is_some_and(|subtype| subtype.starts_with("error_max_"))
            });
        if budget_exhausted {
            return EvaluationOutcome::Fail;
        }
        let runtime_error = self.agent_exit_code.is_some_and(|code| code != 0)
            || metric_objects(&self.metrics).any(|metrics| {
                metrics.get("is_error").and_then(Value::as_bool) == Some(true)
                    || metrics
                        .get("result_subtype")
                        .and_then(Value::as_str)
                        .is_some_and(|subtype| subtype.starts_with("error_"))
            });
        if runtime_error {
            return EvaluationOutcome::InfrastructureError;
        }
        if self.validation_exit_code.is_some_and(|code| code != 0) {
            return EvaluationOutcome::Fail;
        }
        explicit.unwrap_or(if self.success {
            EvaluationOutcome::Pass
        } else {
            EvaluationOutcome::Fail
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct OutcomeCounts {
    pub pass: u64,
    pub fail: u64,
    pub infrastructure_error: u64,
    pub unknown: u64,
}

impl OutcomeCounts {
    fn record(&mut self, outcome: EvaluationOutcome) {
        match outcome {
            EvaluationOutcome::Pass => self.pass += 1,
            EvaluationOutcome::Fail => self.fail += 1,
            EvaluationOutcome::InfrastructureError => self.infrastructure_error += 1,
            EvaluationOutcome::Unknown => self.unknown += 1,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct MeasurementCoverage {
    /// Attempts with an explicitly reported value, including partial values.
    pub available: u64,
    pub missing: u64,
    pub total: u64,
    /// Reported values known to be incomplete; always a subset of `available`.
    #[serde(default)]
    pub partial: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AttemptUsageSummary {
    pub attempts: u64,
    pub outcome_counts: OutcomeCounts,
    /// Sums of explicitly measured values, including failed, unpaired and
    /// infrastructure attempts. Null means no observations or an overflowing sum.
    pub totals_observed: BTreeMap<String, Option<f64>>,
    pub measurement_coverage: BTreeMap<String, MeasurementCoverage>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct AllAttemptUsage {
    pub off: AttemptUsageSummary,
    pub brain: AttemptUsageSummary,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct PairOutcomeCounts {
    pub both_pass: u64,
    pub off_only_pass: u64,
    pub brain_only_pass: u64,
    pub both_fail: u64,
    /// Neither arm can earn a correctness win when either outcome is unscorable.
    pub infrastructure_or_unknown: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ControlledUsageComparison {
    pub benchmark_id: String,
    pub model_hash: String,
    pub config_hash: String,
    pub brain_snapshot_hash: String,
    pub paired_runs: u64,
    pub excluded_runs: u64,
    pub off_successes: u64,
    pub brain_successes: u64,
    /// Legacy all-matched-pair measurements, including failed/infra attempts.
    pub metrics: BTreeMap<String, PairedUsageMetric>,
    #[serde(default)]
    pub all_attempts: AllAttemptUsage,
    #[serde(default)]
    pub pair_outcomes: PairOutcomeCounts,
    #[serde(default)]
    pub both_clean_pass_pairs: u64,
    #[serde(default)]
    pub both_clean_pass_metrics: BTreeMap<String, PairedUsageMetric>,
}

/// Compare resource measurements only within identical recorded benchmark,
/// model, configuration and brain snapshot controls, and unique task/seed pairs.
/// Unpaired or duplicate arms are excluded from paired metrics, but retained in
/// all-attempt totals. Groups without a complete pair remain visible. Failed
/// trials remain in outcome counts; infrastructure is not a correctness loss.
/// This describes benchmarks, not everyday savings.
#[must_use]
pub fn compare_usage_records(records: &[EvaluationRecord]) -> Vec<ControlledUsageComparison> {
    type Controls<'a> = (&'a str, &'a str, &'a str, &'a str);
    let mut groups = BTreeMap::<Controls<'_>, Vec<&EvaluationRecord>>::new();
    for record in records {
        if [
            &record.benchmark_id,
            &record.model_hash,
            &record.config_hash,
            &record.brain_snapshot_hash,
        ]
        .iter()
        .any(|value| value.trim().is_empty())
        {
            continue;
        }
        groups
            .entry((
                &record.benchmark_id,
                &record.model_hash,
                &record.config_hash,
                &record.brain_snapshot_hash,
            ))
            .or_default()
            .push(record);
    }
    let mut reports = Vec::new();
    for ((benchmark, model, config, snapshot), records) in groups {
        let mut pairs = BTreeMap::<(&str, u64), Vec<&EvaluationRecord>>::new();
        for record in &records {
            if record.task_id.trim().is_empty() {
                continue;
            }
            pairs
                .entry((&record.task_id, record.seed))
                .or_default()
                .push(record);
        }
        let mut report = ControlledUsageComparison {
            benchmark_id: benchmark.to_owned(),
            model_hash: model.to_owned(),
            config_hash: config.to_owned(),
            brain_snapshot_hash: snapshot.to_owned(),
            paired_runs: 0,
            excluded_runs: u64::try_from(records.len()).unwrap_or(u64::MAX),
            off_successes: 0,
            brain_successes: 0,
            metrics: empty_paired_metrics(),
            all_attempts: AllAttemptUsage {
                off: summarize_attempt_usage(&records, EvaluationMode::Off),
                brain: summarize_attempt_usage(&records, EvaluationMode::Brain),
            },
            pair_outcomes: PairOutcomeCounts::default(),
            both_clean_pass_pairs: 0,
            both_clean_pass_metrics: empty_paired_metrics(),
        };
        for pair in pairs.values() {
            if pair.len() != 2 || pair[0].mode == pair[1].mode {
                continue;
            }
            let (off, brain) = if pair[0].mode == EvaluationMode::Off {
                (pair[0], pair[1])
            } else {
                (pair[1], pair[0])
            };
            report.paired_runs += 1;
            report.excluded_runs -= 2;
            let outcomes = (off.outcome(), brain.outcome());
            report.off_successes += u64::from(outcomes.0 == EvaluationOutcome::Pass);
            report.brain_successes += u64::from(outcomes.1 == EvaluationOutcome::Pass);
            match outcomes {
                (EvaluationOutcome::Pass, EvaluationOutcome::Pass) => {
                    report.pair_outcomes.both_pass += 1;
                    report.both_clean_pass_pairs += 1;
                    accumulate_pair(&mut report.both_clean_pass_metrics, off, brain);
                }
                (EvaluationOutcome::Pass, EvaluationOutcome::Fail) => {
                    report.pair_outcomes.off_only_pass += 1;
                }
                (EvaluationOutcome::Fail, EvaluationOutcome::Pass) => {
                    report.pair_outcomes.brain_only_pass += 1;
                }
                (EvaluationOutcome::Fail, EvaluationOutcome::Fail) => {
                    report.pair_outcomes.both_fail += 1;
                }
                _ => report.pair_outcomes.infrastructure_or_unknown += 1,
            }
            accumulate_pair(&mut report.metrics, off, brain);
        }
        finish_paired_metrics(&mut report.metrics);
        finish_paired_metrics(&mut report.both_clean_pass_metrics);
        reports.push(report);
    }
    reports
}

const USAGE_METRICS: [&str; 10] = [
    "cost_usd",
    "turns",
    "tool_calls",
    "input_tokens",
    "output_tokens",
    "cache_read_tokens",
    "cache_write_tokens",
    "total_tokens",
    "processed_input_tokens",
    "agent_wall_seconds",
];

fn metric_objects(metrics: &Value) -> impl Iterator<Item = &Value> {
    [
        Some(metrics),
        metrics.get("reported"),
        metrics.get("usage"),
        metrics.pointer("/reported/usage"),
    ]
    .into_iter()
    .flatten()
}

fn empty_paired_metrics() -> BTreeMap<String, PairedUsageMetric> {
    USAGE_METRICS
        .into_iter()
        .map(|name| (name.to_owned(), PairedUsageMetric::default()))
        .collect()
}

fn accumulate_pair(
    metrics: &mut BTreeMap<String, PairedUsageMetric>,
    off: &EvaluationRecord,
    brain: &EvaluationRecord,
) {
    for (name, metric) in metrics {
        // Lower bounds belong in observed attempt totals, not paired reductions.
        if metric_is_partial(&off.metrics, name) || metric_is_partial(&brain.metrics, name) {
            continue;
        }
        if let (Some(off), Some(brain)) = (
            reported_metric(&off.metrics, name),
            reported_metric(&brain.metrics, name),
        ) {
            metric.measured_pairs += 1;
            metric.off_total = Some(metric.off_total.unwrap_or(0.0) + off);
            metric.brain_total = Some(metric.brain_total.unwrap_or(0.0) + brain);
        }
    }
}

fn finish_paired_metrics(metrics: &mut BTreeMap<String, PairedUsageMetric>) {
    for metric in metrics.values_mut() {
        // Keep measurement coverage even when an aggregate cannot be represented.
        metric.off_total = metric.off_total.filter(|value| value.is_finite());
        metric.brain_total = metric.brain_total.filter(|value| value.is_finite());
        metric.brain_minus_off_pct = metric
            .off_total
            .zip(metric.brain_total)
            .filter(|(off, _)| *off > 0.0)
            .map(|(off, brain)| (brain - off) / off * 100.0)
            .filter(|value| value.is_finite());
    }
}

fn summarize_attempt_usage(
    records: &[&EvaluationRecord],
    mode: EvaluationMode,
) -> AttemptUsageSummary {
    let mut summary = AttemptUsageSummary {
        totals_observed: USAGE_METRICS
            .into_iter()
            .map(|name| (name.to_owned(), None))
            .collect(),
        measurement_coverage: USAGE_METRICS
            .into_iter()
            .map(|name| (name.to_owned(), MeasurementCoverage::default()))
            .collect(),
        ..AttemptUsageSummary::default()
    };
    for record in records.iter().filter(|record| record.mode == mode) {
        summary.attempts += 1;
        summary.outcome_counts.record(record.outcome());
        for (name, total) in &mut summary.totals_observed {
            let coverage = summary
                .measurement_coverage
                .get_mut(name)
                .expect("known metric");
            coverage.total += 1;
            if let Some(value) = reported_metric(&record.metrics, name) {
                coverage.available += 1;
                coverage.partial += u64::from(metric_is_partial(&record.metrics, name));
                *total = Some(total.unwrap_or(0.0) + value);
            } else {
                coverage.missing += 1;
            }
        }
    }
    for total in summary.totals_observed.values_mut() {
        *total = total.filter(|value| value.is_finite());
    }
    summary
}

fn metric_is_partial(metrics: &Value, name: &str) -> bool {
    if name == "tool_calls" {
        return metric_objects(metrics).any(|metrics| {
            metrics.get("tool_calls_complete").and_then(Value::as_bool) == Some(false)
        });
    }
    matches!(
        name,
        "cost_usd"
            | "input_tokens"
            | "output_tokens"
            | "cache_read_tokens"
            | "cache_write_tokens"
            | "total_tokens"
            | "processed_input_tokens"
    ) && metric_objects(metrics)
        .any(|metrics| metrics.get("usage_may_be_partial").and_then(Value::as_bool) == Some(true))
}

fn reported_metric(metrics: &Value, name: &str) -> Option<f64> {
    // Only explicit provider/harness measurements. In particular, turns are
    // never substituted for tool calls, nor injected estimates for input tokens.
    // Observed partial values remain lower bounds in all-attempt totals.
    let field = match name {
        "cost_usd" => "total_cost_usd",
        "turns" => "num_turns",
        "cache_read_tokens" => "cache_read_input_tokens",
        "cache_write_tokens" => "cache_creation_input_tokens",
        other => other,
    };
    [
        format!("/reported/{field}"),
        format!("/reported/usage/{field}"),
        format!("/usage/{field}"),
        format!("/{field}"),
    ]
    .iter()
    .find_map(|path| {
        metrics
            .pointer(path)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
    })
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

    /// Persist one completed run's compact metrics. Retrying the same record is
    /// idempotent; reusing its ID for different content is rejected atomically.
    ///
    /// # Errors
    ///
    /// Returns an error if the record is invalid or storage fails.
    pub fn record(&self, record: &EvaluationRecord) -> Result<()> {
        let metrics = serde_json::to_string(record)?;
        self.database.write_immediate(|transaction| {
            let inserted = transaction.execute(
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
            if inserted == 0 {
                let existing: String = transaction.query_row(
                    "SELECT metrics_json FROM eval_runs WHERE id = ?1",
                    [record.run_id.as_str()],
                    |row| row.get(0),
                )?;
                if existing != metrics
                    && serde_json::from_str::<Value>(&existing).ok()
                        != Some(serde_json::to_value(record)?)
                {
                    return Err(Error::Config(format!(
                        "evaluation run `{}` already exists with different content",
                        record.run_id
                    )));
                }
            }
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
        run_with_environment_lookup(config_path, output_override, |name| {
            std::env::var(name).ok()
        })
    }
}

fn run_with_environment_lookup(
    config_path: &Path,
    output_override: Option<&Path>,
    environment_value: impl Fn(&str) -> Option<String>,
) -> Result<EvaluationArtifacts> {
    let (config, raw_config_hash) = load_config(config_path)?;
    validate_config(&config)?;
    let pass_environment = resolve_pass_environment_with(&config.pass_env, environment_value)?;
    let base = config_path.parent().unwrap_or_else(|| Path::new("."));
    let artifact_dir = output_override.map_or_else(
        || resolve_existing_or_relative(base, &config.artifact_dir),
        Path::to_path_buf,
    );
    prepare_artifact_dir(&artifact_dir)?;
    let jsonl_path = artifact_dir.join("runs.jsonl");
    let mut jsonl = create_private_file(&jsonl_path, true)?;
    let isolation = tempfile::Builder::new()
        .prefix("engram-eval-")
        .tempdir()
        .map_err(|source| io_error(std::env::temp_dir(), source))?;
    let (baseline, brain_snapshot_hash) = prepare_brain_baseline(&config, base, isolation.path())?;
    let config_hash =
        sha256(format!("{raw_config_hash}\0brain-snapshot\0{brain_snapshot_hash}").as_bytes());
    let mut records = Vec::new();
    let mut ordinal = 0_u64;

    for (task_index, task) in config.tasks.iter().enumerate() {
        for (seed_index, seed) in config.seeds.iter().copied().enumerate() {
            let paired = config.modes.len() == 2;
            let order: Vec<EvaluationMode> = if paired {
                if (task_index + seed_index) % 2 == 0 {
                    vec![EvaluationMode::Off, EvaluationMode::Brain]
                } else {
                    vec![EvaluationMode::Brain, EvaluationMode::Off]
                }
            } else {
                config.modes.clone()
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
                    &pass_environment,
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
                    "brain_snapshot must be a standalone `engram backup create` file; live sidecar exists: {}",
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
    pass_environment: &[(String, String)],
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
    // Deliberately short: `sun_path` is ~104 bytes on macOS, and the state
    // dir hosts the arm's Unix socket, so the descriptive run name must not
    // appear in this path (the worktree and artifact dirs carry it instead).
    let state_root = isolation_root.join(format!("s{ordinal:06}"));
    fs::create_dir(&state_root).map_err(|source| io_error(&state_root, source))?;
    make_private_dir(&state_root)?;
    let arm_database = state_root.join("brain.sqlite");
    fs::copy(brain_baseline, &arm_database).map_err(|source| io_error(&arm_database, source))?;
    let arm_socket = state_root.join("engramd.sock");
    if arm_socket.as_os_str().len() > 100 {
        return Err(Error::Config(format!(
            "evaluation socket path exceeds the platform Unix-socket limit ({} bytes): {}; \
             set TMPDIR to a shorter directory and rerun",
            arm_socket.as_os_str().len(),
            arm_socket.display()
        )));
    }
    let proxy_port = reserve_loopback_port()?;
    let timeout = Duration::from_secs(
        task.limits
            .as_ref()
            .map_or(config.wall_seconds, |limits| limits.wall_seconds),
    );
    let started = Instant::now();
    let environment = vec![
        // Every arm already has an isolated database/socket. A named workspace
        // inherited from the operator must not redirect or reject those paths.
        ("ENGRAM_WORKSPACE".to_owned(), "default".to_owned()),
        ("ENGRAM_MODE".to_owned(), mode.as_str().to_owned()),
        ("ENGRAM_SEED".to_owned(), seed.to_string()),
        ("ENGRAM_TASK_ID".to_owned(), task.id.clone()),
        (
            "ENGRAM_BENCHMARK_ID".to_owned(),
            config.benchmark_id.clone(),
        ),
        (
            "ENGRAM_DB_PATH".to_owned(),
            arm_database.to_string_lossy().into_owned(),
        ),
        (
            "ENGRAM_SOCKET_PATH".to_owned(),
            arm_socket.to_string_lossy().into_owned(),
        ),
        (
            "ENGRAM_STATE_DIR".to_owned(),
            state_root.to_string_lossy().into_owned(),
        ),
        ("ENGRAM_PROXY_PORT".to_owned(), proxy_port.to_string()),
        (
            "ENGRAM_BRAIN_SNAPSHOT_HASH".to_owned(),
            brain_snapshot_hash.to_owned(),
        ),
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
        let mut agent_environment = environment.clone();
        agent_environment.extend_from_slice(pass_environment);
        Some(run_command(
            &task.agent.command,
            &worktree,
            &agent_environment,
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
    environment: &[(String, String)],
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
    // `--` is required for the negative (process-group) pid to be treated as
    // an operand by procps kill on Linux; BSD kill accepts it too. Without it
    // the group kill silently fails on Linux and background children survive.
    let group = format!("-{process_group}");
    let _ = Command::new("/bin/kill")
        .args(["-TERM", "--", &group])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if graceful_wait {
        std::thread::sleep(Duration::from_millis(250));
    }
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &group])
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
    let seen_modes = records
        .iter()
        .map(|record| record.mode)
        .collect::<std::collections::BTreeSet<_>>();
    let paired_config = seen_modes.len() == 2;
    if paired_config
        && pairs.values().any(|pair| {
            !pair.contains_key(&EvaluationMode::Off) || !pair.contains_key(&EvaluationMode::Brain)
        })
    {
        return Err(Error::Config(
            "evaluation report requires one off and one brain run per task/seed".to_owned(),
        ));
    }
    let off = summarize(EvaluationMode::Off, records);
    let brain = summarize(EvaluationMode::Brain, records);
    // Deltas are meaningful only when both arms ran in THIS config; a
    // single-arm config compares against a control recorded elsewhere.
    let wall_delta = (paired_config && off.mean_wall_ms > 0.0)
        .then(|| ((brain.mean_wall_ms - off.mean_wall_ms) / off.mean_wall_ms) * 100.0);
    let success_delta = paired_config.then_some(brain.success_rate - off.success_rate);
    let paired_runs = if paired_config {
        u64::try_from(pairs.len()).unwrap_or(u64::MAX)
    } else {
        0
    };
    Ok(ComparisonReport {
        benchmark_id: benchmark_id.to_owned(),
        model_hash: model_hash.to_owned(),
        config_hash: config_hash.to_owned(),
        paired_runs,
        success_rate_delta: success_delta,
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
    let mut pass_env_names = std::collections::BTreeSet::new();
    for name in &config.pass_env {
        if !ALLOWED_PASSTHROUGH_ENV.contains(&name.as_str()) {
            return Err(Error::Config(format!(
                "pass_env may contain only {}",
                ALLOWED_PASSTHROUGH_ENV.join(" or ")
            )));
        }
        if !pass_env_names.insert(name) {
            return Err(Error::Config(format!(
                "pass_env contains duplicate variable name {name}"
            )));
        }
    }
    let modes = config
        .modes
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if modes.is_empty() || modes.len() != config.modes.len() {
        return Err(Error::Config(
            "evaluation modes must be non-empty and free of duplicates: use `[off, brain]` \
             for a paired run or a single mode for an unpaired arm (e.g. an A/B whose \
             control lives in another config)"
                .to_owned(),
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

fn resolve_pass_environment_with(
    names: &[String],
    environment_value: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>> {
    names
        .iter()
        .map(|name| {
            let value = environment_value(name)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    Error::Config(format!(
                        "pass_env requested {name}, but it is missing, empty, or not Unicode"
                    ))
                })?;
            Ok((name.clone(), value))
        })
        .collect()
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
    [
        "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "TERM", "USER", "LOGNAME",
    ]
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
        "# Engram paired evaluation: {}\n\nModel: `{}`  \nConfig: `{}`  \nPairs: {}\n\n| Mode | Runs | Successes | Success rate | Mean wall ms |\n|---|---:|---:|---:|---:|\n| off | {} | {} | {:.3} | {:.1} |\n| brain | {} | {} | {:.3} | {:.1} |\n\nSuccess-rate delta: **{}**  \nMean wall-time delta: **{}**\n",
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
        report.success_rate_delta.map_or_else(
            || "n/a (single-arm)".to_owned(),
            |value| format!("{value:+.3}")
        ),
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

    fn measured_record(mode: EvaluationMode, seed: u64, metrics: Value) -> EvaluationRecord {
        EvaluationRecord {
            run_id: EvaluationRunId::new(),
            benchmark_id: "bench".to_owned(),
            task_id: "task".to_owned(),
            mode,
            seed,
            ordinal: seed,
            model_hash: "model".to_owned(),
            config_hash: "config".to_owned(),
            brain_snapshot_hash: "snapshot".to_owned(),
            success: mode == EvaluationMode::Off,
            timed_out: false,
            setup_exit_code: None,
            agent_exit_code: Some(0),
            validation_exit_code: Some(0),
            wall_ms: 10,
            metrics,
            artifact_path: PathBuf::from("unused"),
            created_at_ms: 100,
        }
    }

    #[test]
    fn usage_comparison_retains_failures_and_only_compares_measured_pairs() {
        let records = vec![
            measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"reported":{"total_cost_usd":2.0,"num_turns":10,"usage":{"input_tokens":200}}}),
            ),
            measured_record(
                EvaluationMode::Brain,
                1,
                serde_json::json!({"reported":{"total_cost_usd":1.0,"num_turns":8,"usage":{"input_tokens":100}}}),
            ),
            measured_record(
                EvaluationMode::Off,
                2,
                serde_json::json!({"reported":{"total_cost_usd":20.0}}),
            ),
            measured_record(EvaluationMode::Brain, 2, Value::Null),
        ];
        let reports = compare_usage_records(&records);
        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        assert_eq!(report.paired_runs, 2);
        assert_eq!(report.off_successes, 2);
        assert_eq!(report.brain_successes, 0);
        let cost = &report.metrics["cost_usd"];
        assert_eq!(cost.measured_pairs, 1);
        assert_eq!(cost.off_total, Some(2.0));
        assert_eq!(cost.brain_total, Some(1.0));
        assert_eq!(cost.brain_minus_off_pct, Some(-50.0));
        assert_eq!(report.metrics["input_tokens"].measured_pairs, 1);
        assert_eq!(report.metrics["turns"].measured_pairs, 1);
        assert_eq!(report.metrics["tool_calls"].off_total, None);
        assert_eq!(report.metrics["output_tokens"].brain_minus_off_pct, None);
    }

    #[test]
    fn usage_comparison_rejects_mismatched_controls_and_duplicate_arms() {
        let off = measured_record(
            EvaluationMode::Off,
            1,
            serde_json::json!({"reported":{"total_cost_usd":2.0}}),
        );
        let brain = measured_record(
            EvaluationMode::Brain,
            1,
            serde_json::json!({"reported":{"total_cost_usd":1.0}}),
        );
        for field in ["model", "config", "snapshot", "task", "seed"] {
            let mut mismatched = brain.clone();
            match field {
                "model" => mismatched.model_hash = "other".to_owned(),
                "config" => mismatched.config_hash = "other".to_owned(),
                "snapshot" => mismatched.brain_snapshot_hash = "other".to_owned(),
                "task" => mismatched.task_id = "other".to_owned(),
                _ => mismatched.seed = 2,
            }
            let reports = compare_usage_records(&[off.clone(), mismatched]);
            assert!(reports.iter().all(|report| report.paired_runs == 0));
            assert_eq!(
                reports
                    .iter()
                    .map(|report| report.all_attempts.off.attempts
                        + report.all_attempts.brain.attempts)
                    .sum::<u64>(),
                2
            );
        }
        let mut duplicate_arm = off.clone();
        duplicate_arm.run_id = EvaluationRunId::new();
        let reports = compare_usage_records(&[off, brain.clone(), duplicate_arm]);
        assert_eq!(reports[0].paired_runs, 0);
        assert_eq!(reports[0].excluded_runs, 3);
        assert_eq!(reports[0].all_attempts.off.attempts, 2);
        assert_eq!(
            reports[0].all_attempts.off.totals_observed["cost_usd"],
            Some(4.0)
        );
        assert_eq!(reports[0].all_attempts.brain.attempts, 1);
        assert_eq!(reports[0].metrics["cost_usd"].measured_pairs, 0);
        let mut invalid = brain;
        invalid.config_hash.clear();
        assert!(compare_usage_records(&[invalid]).is_empty());
    }

    #[test]
    fn usage_comparison_has_no_percentage_for_zero_control() {
        let reports = compare_usage_records(&[
            measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"reported":{"total_cost_usd":0.0,"num_turns":-1}}),
            ),
            measured_record(
                EvaluationMode::Brain,
                1,
                serde_json::json!({"reported":{"total_cost_usd":1.0,"num_turns":2}}),
            ),
        ]);
        assert_eq!(reports[0].metrics["cost_usd"].off_total, Some(0.0));
        assert_eq!(reports[0].metrics["cost_usd"].brain_minus_off_pct, None);
        assert_eq!(reports[0].metrics["turns"].measured_pairs, 0);
    }

    #[test]
    fn usage_outcome_keeps_behavioral_pass_separate_from_runtime_failure() {
        for metrics in [
            serde_json::json!({"outcome":"infrastructure_error","behavioral_grade":{"status":"pass"},"total_tokens":10}),
            serde_json::json!({"outcome":"pass","behavioral_grade":{"status":"pass"},"is_error":true,"total_tokens":10}),
            serde_json::json!({"outcome":"pass","reported":{"is_error":true},"total_tokens":10}),
            serde_json::json!({"outcome":"pass","result_subtype":"error_during_execution","total_tokens":10}),
        ] {
            let mut off = measured_record(EvaluationMode::Off, 1, metrics);
            off.success = true;
            let brain = measured_record(
                EvaluationMode::Brain,
                1,
                serde_json::json!({"outcome":"pass","total_tokens":20}),
            );
            assert_eq!(off.outcome(), EvaluationOutcome::InfrastructureError);
            let reports = compare_usage_records(&[off, brain]);
            let report = &reports[0];
            assert_eq!(report.off_successes, 0);
            assert_eq!(report.brain_successes, 1);
            assert_eq!(report.pair_outcomes.infrastructure_or_unknown, 1);
            assert_eq!(report.pair_outcomes.brain_only_pass, 0);
            assert_eq!(report.both_clean_pass_pairs, 0);
            assert_eq!(report.metrics["total_tokens"].off_total, Some(10.0));
            assert_eq!(
                report.all_attempts.off.outcome_counts.infrastructure_error,
                1
            );
            assert_eq!(
                report.both_clean_pass_metrics["total_tokens"].measured_pairs,
                0
            );
        }
    }

    #[test]
    fn usage_outcome_budget_failure_does_not_override_invalid_infrastructure() {
        for subtype in ["error_max_turns", "error_max_budget_usd"] {
            let mut record = measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"outcome":"pass","result_subtype":subtype,"is_error":true}),
            );
            record.agent_exit_code = Some(1);
            assert_eq!(record.outcome(), EvaluationOutcome::Fail);
            for issue in ["model_drift", "no_delivery_receipt"] {
                record.metrics["validity_issues"] = serde_json::json!([issue]);
                assert_eq!(record.outcome(), EvaluationOutcome::InfrastructureError);
            }
            record.metrics["validity_issues"] = serde_json::json!([]);
            record.metrics["outcome"] = serde_json::json!("infrastructure_error");
            assert_eq!(record.outcome(), EvaluationOutcome::InfrastructureError);
        }
        let mut record = measured_record(
            EvaluationMode::Off,
            1,
            serde_json::json!({"outcome":"pass"}),
        );
        record.timed_out = true;
        assert_eq!(record.outcome(), EvaluationOutcome::Fail);
        record.setup_exit_code = Some(1);
        assert_eq!(record.outcome(), EvaluationOutcome::InfrastructureError);
        record.setup_exit_code = Some(0);
        record.timed_out = false;
        record.metrics["agent_timed_out"] = serde_json::json!(true);
        assert_eq!(record.outcome(), EvaluationOutcome::Fail);
    }

    #[test]
    fn usage_outcome_defends_explicit_pass_and_supports_legacy_records() {
        let mut record = measured_record(EvaluationMode::Off, 1, Value::Null);
        assert_eq!(record.outcome(), EvaluationOutcome::Pass);
        record.success = false;
        assert_eq!(record.outcome(), EvaluationOutcome::Fail);
        record.metrics = serde_json::json!({"outcome":"pass"});
        assert_eq!(record.outcome(), EvaluationOutcome::Pass);
        record.validation_exit_code = Some(1);
        assert_eq!(record.outcome(), EvaluationOutcome::Fail);
        record.validation_exit_code = Some(0);
        record.agent_exit_code = Some(1);
        assert_eq!(record.outcome(), EvaluationOutcome::InfrastructureError);
        record.agent_exit_code = Some(0);
        record.setup_exit_code = Some(2);
        assert_eq!(record.outcome(), EvaluationOutcome::InfrastructureError);
        record.setup_exit_code = None;
        record.success = true;
        for invalid in [
            Value::Null,
            serde_json::json!(true),
            serde_json::json!("success"),
            serde_json::json!("PASS"),
        ] {
            record.metrics["outcome"] = invalid;
            assert_eq!(record.outcome(), EvaluationOutcome::Unknown);
        }
    }

    #[test]
    fn usage_comparison_separates_cheap_failed_runs_from_both_pass_metrics() {
        let records = [
            measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"outcome":"pass","total_tokens":100,"agent_wall_seconds":10}),
            ),
            measured_record(
                EvaluationMode::Brain,
                1,
                serde_json::json!({"outcome":"pass","total_tokens":120,"agent_wall_seconds":12}),
            ),
            measured_record(
                EvaluationMode::Off,
                2,
                serde_json::json!({"outcome":"pass","total_tokens":100,"agent_wall_seconds":10}),
            ),
            measured_record(
                EvaluationMode::Brain,
                2,
                serde_json::json!({"outcome":"fail","total_tokens":1,"agent_wall_seconds":1}),
            ),
        ];
        let reports = compare_usage_records(&records);
        let report = &reports[0];
        assert_eq!(report.metrics["total_tokens"].measured_pairs, 2);
        assert_eq!(report.metrics["total_tokens"].brain_total, Some(121.0));
        assert_eq!(
            report.metrics["total_tokens"].brain_minus_off_pct,
            Some(-39.5)
        );
        assert_eq!(report.both_clean_pass_pairs, 1);
        assert_eq!(
            report.both_clean_pass_metrics["total_tokens"].brain_minus_off_pct,
            Some(20.0)
        );
        assert_eq!(
            report.both_clean_pass_metrics["agent_wall_seconds"].brain_total,
            Some(12.0)
        );
        assert_eq!(report.pair_outcomes.both_pass, 1);
        assert_eq!(report.pair_outcomes.off_only_pass, 1);
        assert_eq!(report.all_attempts.brain.attempts, 2);
        assert_eq!(report.all_attempts.brain.outcome_counts.fail, 1);
        assert_eq!(
            report.all_attempts.brain.totals_observed["total_tokens"],
            Some(121.0)
        );
    }

    #[test]
    fn usage_comparison_pair_contingency_excludes_unknown_and_infrastructure_wins() {
        let outcomes = [
            ("pass", "pass"),
            ("pass", "fail"),
            ("fail", "pass"),
            ("fail", "fail"),
            ("infrastructure_error", "pass"),
            ("pass", "unknown"),
        ];
        let mut records = Vec::new();
        for (index, (off, brain)) in outcomes.into_iter().enumerate() {
            let seed = u64::try_from(index).expect("small index");
            records.push(measured_record(
                EvaluationMode::Off,
                seed,
                serde_json::json!({"outcome":off}),
            ));
            records.push(measured_record(
                EvaluationMode::Brain,
                seed,
                serde_json::json!({"outcome":brain}),
            ));
        }
        let reports = compare_usage_records(&records);
        assert_eq!(reports[0].paired_runs, 6);
        assert_eq!(
            reports[0].pair_outcomes,
            PairOutcomeCounts {
                both_pass: 1,
                off_only_pass: 1,
                brain_only_pass: 1,
                both_fail: 1,
                infrastructure_or_unknown: 2
            }
        );
        assert_eq!(reports[0].all_attempts.brain.outcome_counts.unknown, 1);
        assert_eq!(reports[0].both_clean_pass_pairs, 1);
    }

    #[test]
    fn usage_comparison_keeps_unpaired_attempts_and_explicit_measurement_coverage() {
        let mut missing_task = measured_record(
            EvaluationMode::Brain,
            3,
            serde_json::json!({"outcome":"infrastructure_error","total_tokens":5}),
        );
        missing_task.task_id.clear();
        let records = [
            measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"total_tokens":100,"processed_input_tokens":90,"input_tokens":5,"cache_read_input_tokens":80,"cache_creation_input_tokens":5,"agent_wall_seconds":2.5}),
            ),
            measured_record(
                EvaluationMode::Off,
                2,
                serde_json::json!({"outcome":"fail","total_tokens":null,"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":2,"num_turns":4}),
            ),
            measured_record(
                EvaluationMode::Brain,
                1,
                serde_json::json!({"outcome":"pass","total_tokens":80,"tool_calls":0}),
            ),
            missing_task,
        ];
        let reports = compare_usage_records(&records);
        let report = &reports[0];
        assert_eq!(report.paired_runs, 1);
        assert_eq!(report.excluded_runs, 2);
        assert_eq!(report.all_attempts.off.attempts, 2);
        assert_eq!(report.all_attempts.brain.attempts, 2);
        assert_eq!(
            report.all_attempts.off.totals_observed["total_tokens"],
            Some(100.0)
        );
        assert_eq!(
            report.all_attempts.brain.totals_observed["total_tokens"],
            Some(85.0)
        );
        assert_eq!(
            report.all_attempts.off.measurement_coverage["total_tokens"],
            MeasurementCoverage {
                available: 1,
                missing: 1,
                total: 2,
                partial: 0,
            }
        );
        assert_eq!(
            report.all_attempts.off.totals_observed["processed_input_tokens"],
            Some(90.0)
        );
        assert_eq!(
            report.all_attempts.off.measurement_coverage["processed_input_tokens"].missing,
            1
        );
        assert_eq!(
            report.all_attempts.off.totals_observed["agent_wall_seconds"],
            Some(2.5)
        );
        assert_eq!(
            report.all_attempts.off.measurement_coverage["agent_wall_seconds"].missing,
            1
        );
        assert_eq!(
            report.all_attempts.off.totals_observed["cache_read_tokens"],
            Some(100.0)
        );
        assert_eq!(
            report.all_attempts.off.totals_observed["cache_write_tokens"],
            Some(10.0)
        );
        assert_eq!(report.all_attempts.off.totals_observed["tool_calls"], None);
        assert_eq!(
            report.all_attempts.brain.totals_observed["tool_calls"],
            Some(0.0)
        );
        assert_eq!(report.metrics["total_tokens"].brain_total, Some(80.0));
        assert_eq!(report.metrics["agent_wall_seconds"].measured_pairs, 0);
    }

    #[test]
    fn usage_comparison_does_not_infer_unknown_or_incomplete_metrics() {
        let records = [
            measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"outcome":"pass","total_tokens":-1,"processed_input_tokens":"12","agent_wall_seconds":true,"tool_calls":7,"tool_calls_complete":false,"num_turns":2}),
            ),
            measured_record(
                EvaluationMode::Brain,
                1,
                serde_json::json!({"outcome":"pass","total_tokens":null,"tool_calls":3,"tool_calls_complete":true,"estimated_tokens":99}),
            ),
        ];
        let reports = compare_usage_records(&records);
        for name in [
            "total_tokens",
            "processed_input_tokens",
            "agent_wall_seconds",
            "input_tokens",
        ] {
            assert_eq!(reports[0].metrics[name].measured_pairs, 0);
            assert_eq!(reports[0].all_attempts.off.totals_observed[name], None);
            assert_eq!(
                reports[0].all_attempts.off.measurement_coverage[name].missing,
                1
            );
            assert_eq!(
                reports[0].all_attempts.off.measurement_coverage[name].partial,
                0
            );
        }
        assert_eq!(
            reports[0].all_attempts.off.totals_observed["tool_calls"],
            Some(7.0)
        );
        assert_eq!(
            reports[0].all_attempts.off.measurement_coverage["tool_calls"],
            MeasurementCoverage {
                available: 1,
                missing: 0,
                total: 1,
                partial: 1
            }
        );
        assert_eq!(
            reports[0].all_attempts.brain.totals_observed["tool_calls"],
            Some(3.0)
        );
        assert_eq!(reports[0].metrics["tool_calls"].measured_pairs, 0);
        assert_eq!(reports[0].both_clean_pass_pairs, 1);
        assert_eq!(
            reports[0].both_clean_pass_metrics["tool_calls"].measured_pairs,
            0
        );
    }

    #[test]
    fn usage_comparison_marks_observed_partial_usage_without_inventing_values() {
        let records = [
            measured_record(
                EvaluationMode::Off,
                1,
                serde_json::json!({"outcome":"infrastructure_error","usage_may_be_partial":true,"total_cost_usd":0.5,"total_tokens":10,"processed_input_tokens":8,"input_tokens":2,"output_tokens":2,"cache_read_input_tokens":5,"cache_creation_input_tokens":1,"agent_wall_seconds":3}),
            ),
            measured_record(
                EvaluationMode::Off,
                2,
                serde_json::json!({"usage_may_be_partial":true,"tool_calls_complete":false,"total_tokens":null}),
            ),
        ];
        let reports = compare_usage_records(&records);
        let arm = &reports[0].all_attempts.off;
        for name in [
            "cost_usd",
            "total_tokens",
            "processed_input_tokens",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ] {
            assert_eq!(
                arm.measurement_coverage[name],
                MeasurementCoverage {
                    available: 1,
                    missing: 1,
                    total: 2,
                    partial: 1
                }
            );
        }
        assert_eq!(arm.totals_observed["total_tokens"], Some(10.0));
        assert_eq!(arm.totals_observed["cost_usd"], Some(0.5));
        assert_eq!(arm.totals_observed["tool_calls"], None);
        assert_eq!(arm.measurement_coverage["tool_calls"].partial, 0);
        assert_eq!(arm.measurement_coverage["tool_calls"].missing, 2);
        assert_eq!(arm.measurement_coverage["agent_wall_seconds"].partial, 0);
    }

    #[test]
    fn usage_comparison_excludes_partial_usage_even_when_both_outcomes_pass() {
        let off = measured_record(
            EvaluationMode::Off,
            1,
            serde_json::json!({"outcome":"pass","reported":{"total_cost_usd":0.5,"total_tokens":10,"processed_input_tokens":8,"input_tokens":2,"output_tokens":2,"cache_read_input_tokens":5,"cache_creation_input_tokens":1,"agent_wall_seconds":3}}),
        );
        let mut brain = off.clone();
        brain.run_id = EvaluationRunId::new();
        brain.mode = EvaluationMode::Brain;
        brain.metrics["usage_may_be_partial"] = serde_json::json!(true);
        let reports = compare_usage_records(&[off, brain]);
        let report = &reports[0];
        assert_eq!(report.both_clean_pass_pairs, 1);
        assert_eq!(report.pair_outcomes.both_pass, 1);
        for name in [
            "cost_usd",
            "total_tokens",
            "processed_input_tokens",
            "input_tokens",
            "output_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
        ] {
            assert_eq!(report.metrics[name].measured_pairs, 0);
            assert_eq!(report.metrics[name].brain_minus_off_pct, None);
            assert_eq!(report.both_clean_pass_metrics[name].measured_pairs, 0);
            assert_eq!(
                report.both_clean_pass_metrics[name].brain_minus_off_pct,
                None
            );
            assert!(report.all_attempts.brain.totals_observed[name].is_some());
            assert_eq!(
                report.all_attempts.brain.measurement_coverage[name].available,
                1
            );
            assert_eq!(
                report.all_attempts.brain.measurement_coverage[name].partial,
                1
            );
            assert_eq!(
                report.all_attempts.off.measurement_coverage[name].partial,
                0
            );
        }
        assert_eq!(
            report.all_attempts.brain.totals_observed["total_tokens"],
            Some(10.0)
        );
        assert_eq!(
            report.all_attempts.brain.totals_observed["cost_usd"],
            Some(0.5)
        );
        assert_eq!(report.metrics["agent_wall_seconds"].measured_pairs, 1);
        assert_eq!(
            report.both_clean_pass_metrics["agent_wall_seconds"].measured_pairs,
            1
        );
    }

    #[test]
    fn usage_comparison_overflow_keeps_coverage_without_numeric_delta() {
        let mut records = Vec::new();
        for seed in [1, 2] {
            records.push(measured_record(
                EvaluationMode::Off,
                seed,
                serde_json::json!({"total_tokens":1e308}),
            ));
            records.push(measured_record(
                EvaluationMode::Brain,
                seed,
                serde_json::json!({"total_tokens":1}),
            ));
        }
        let reports = compare_usage_records(&records);
        assert_eq!(reports[0].metrics["total_tokens"].measured_pairs, 2);
        assert_eq!(reports[0].metrics["total_tokens"].off_total, None);
        assert_eq!(reports[0].metrics["total_tokens"].brain_total, Some(2.0));
        assert_eq!(reports[0].metrics["total_tokens"].brain_minus_off_pct, None);
        assert_eq!(
            reports[0].all_attempts.off.totals_observed["total_tokens"],
            None
        );
        assert_eq!(
            reports[0].all_attempts.off.measurement_coverage["total_tokens"].available,
            2
        );
    }

    #[test]
    fn usage_comparison_serializes_zero_outcomes_and_reads_legacy_shape() {
        let reports =
            compare_usage_records(&[measured_record(EvaluationMode::Off, 1, Value::Null)]);
        let mut value = serde_json::to_value(&reports[0]).expect("serialize");
        assert_eq!(
            value.pointer("/all_attempts/brain/outcome_counts"),
            Some(&serde_json::json!({"pass":0,"fail":0,"infrastructure_error":0,"unknown":0}))
        );
        assert_eq!(
            value.pointer("/all_attempts/brain/measurement_coverage/total_tokens"),
            Some(&serde_json::json!({"available":0,"missing":0,"total":0,"partial":0}))
        );
        let legacy_coverage: MeasurementCoverage =
            serde_json::from_value(serde_json::json!({"available":1,"missing":0,"total":1}))
                .expect("legacy coverage");
        assert_eq!(legacy_coverage.partial, 0);
        for field in [
            "all_attempts",
            "pair_outcomes",
            "both_clean_pass_pairs",
            "both_clean_pass_metrics",
        ] {
            value.as_object_mut().expect("object").remove(field);
        }
        let legacy: ControlledUsageComparison =
            serde_json::from_value(value).expect("legacy comparison");
        assert_eq!(legacy.both_clean_pass_pairs, 0);
        assert_eq!(legacy.all_attempts, AllAttemptUsage::default());
        assert_eq!(legacy.pair_outcomes, PairOutcomeCounts::default());
        assert!(legacy.both_clean_pass_metrics.is_empty());
    }

    #[test]
    fn evaluation_store_retries_identical_records_and_rejects_conflicting_id_reuse() {
        let temp = TempDir::new().expect("temp");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let store = EvaluationStore::new(database.clone());
        let record = measured_record(
            EvaluationMode::Off,
            1,
            serde_json::json!({"outcome":"pass","total_tokens":42}),
        );
        store.record(&record).expect("initial record");
        store.record(&record).expect("byte-identical retry");
        let pretty = serde_json::to_string_pretty(&record).expect("pretty JSON");
        database
            .write_immediate(|tx| {
                tx.execute(
                    "UPDATE eval_runs SET metrics_json = ?1 WHERE id = ?2",
                    params![pretty, record.run_id.as_str()],
                )?;
                Ok(())
            })
            .expect("reformat stored JSON");
        store.record(&record).expect("semantically identical retry");
        for field in ["metrics", "success", "task", "controls", "timestamp"] {
            let mut conflicting = record.clone();
            match field {
                "metrics" => conflicting.metrics["total_tokens"] = serde_json::json!(43),
                "success" => conflicting.success = false,
                "task" => conflicting.task_id = "other".to_owned(),
                "controls" => conflicting.config_hash = "other".to_owned(),
                _ => conflicting.created_at_ms += 1,
            }
            let error = store.record(&conflicting).expect_err("conflicting ID");
            assert!(
                error
                    .to_string()
                    .contains("already exists with different content")
            );
        }
        database
            .with_reader(|connection| {
                let count: i64 =
                    connection.query_row("SELECT count(*) FROM eval_runs", [], |row| row.get(0))?;
                assert_eq!(count, 1);
                let stored: String = connection.query_row(
                    "SELECT metrics_json FROM eval_runs WHERE id = ?1",
                    [record.run_id.as_str()],
                    |row| row.get(0),
                )?;
                assert_eq!(serde_json::from_str::<EvaluationRecord>(&stored)?, record);
                Ok(())
            })
            .expect("original unchanged");
    }

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
                "benchmark_id: smoke\nmodel_hash: model-1\nmodes: [off, brain]\nseeds: [1, 2]\nwall_seconds: 5\nartifact_dir: artifacts\ntasks:\n  - id: task_1\n    repo: {}\n    prompt_file: {}\n    agent:\n      command: \"printf '{{\\\"db\\\":\\\"%s\\\",\\\"socket\\\":\\\"%s\\\",\\\"snapshot\\\":\\\"%s\\\"}}' \\\"$ENGRAM_DB_PATH\\\" \\\"$ENGRAM_SOCKET_PATH\\\" \\\"$ENGRAM_BRAIN_SNAPSHOT_HASH\\\" > metrics.json; printf '%s' \\\"$ENGRAM_MODE\\\" > mode\"\n    success:\n      command: \"test -s mode && test \\\"$(cat state)\\\" = clean\"\n    metrics_file: metrics.json\n",
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
    fn single_arm_runner_reports_without_pairing() {
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
                "benchmark_id: single\nmodel_hash: model-1\nmodes: [brain]\nseeds: [1, 2]\nwall_seconds: 5\nartifact_dir: artifacts\ntasks:\n  - id: task_1\n    repo: {}\n    prompt_file: {}\n    agent:\n      command: \"printf '%s' \\\"$ENGRAM_MODE\\\" > mode\"\n    success:\n      command: \"test \\\"$(cat mode)\\\" = brain\"\n",
                repo.display(),
                prompt.display()
            ),
        )
        .expect("config");
        let output = temp.path().join("out");
        let artifacts = run_with_environment_lookup(&config_path, Some(&output), |_| None)
            .expect("single-arm run");
        assert_eq!(artifacts.comparison.brain.runs, 2);
        assert_eq!(artifacts.comparison.brain.successes, 2);
        assert_eq!(artifacts.comparison.off.runs, 0);
        assert_eq!(artifacts.comparison.paired_runs, 0);
        assert!(artifacts.comparison.success_rate_delta.is_none());
        assert!(artifacts.comparison.mean_wall_time_delta_pct.is_none());
        let markdown = fs::read_to_string(&artifacts.comparison_markdown_path).expect("md");
        assert!(markdown.contains("n/a (single-arm)"));
    }

    #[test]
    fn rejects_incomplete_control_modes_escaping_metrics_and_unsafe_env() {
        let mut config = BenchmarkConfig {
            benchmark_id: "safe".to_owned(),
            model_hash: "model".to_owned(),
            modes: vec![EvaluationMode::Brain],
            seeds: vec![1],
            wall_seconds: 1,
            artifact_dir: PathBuf::from("out"),
            brain_snapshot: None,
            runtime_config: Value::Null,
            pass_env: Vec::new(),
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
        // Single-arm mode is legal; the escaping metrics path is what fails.
        let error = validate_config(&config).expect_err("escaping metrics path must fail");
        assert!(error.to_string().contains("metrics"), "{error}");

        config.tasks[0].metrics_file = None;
        config.modes = Vec::new();
        let error = validate_config(&config).expect_err("empty modes must fail");
        assert!(error.to_string().contains("non-empty"));
        config.modes = vec![EvaluationMode::Brain, EvaluationMode::Brain];
        let error = validate_config(&config).expect_err("duplicate modes must fail");
        assert!(error.to_string().contains("free of duplicates"));
        config.modes = vec![EvaluationMode::Brain];
        validate_config(&config).expect("single-arm config is legal");

        config.modes = vec![EvaluationMode::Off, EvaluationMode::Brain];
        config.pass_env = vec!["PATH".to_owned()];
        let error = validate_config(&config).expect_err("arbitrary environment access must fail");
        assert!(error.to_string().contains("pass_env may contain only"));

        config.pass_env = vec![
            "ANTHROPIC_API_KEY".to_owned(),
            "ANTHROPIC_API_KEY".to_owned(),
        ];
        let error = validate_config(&config).expect_err("duplicate names must fail");
        assert!(error.to_string().contains("duplicate variable name"));
    }

    #[test]
    fn pass_env_defaults_empty_and_requires_a_present_value() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("bench.smoke.yaml");
        let (config, _) = load_config(&example).expect("shipped benchmark config");
        assert!(config.pass_env.is_empty());

        let names = vec!["ANTHROPIC_AUTH_TOKEN".to_owned()];
        let error = resolve_pass_environment_with(&names, |_| None)
            .expect_err("requested credential must be present");
        assert!(error.to_string().contains("ANTHROPIC_AUTH_TOKEN"));

        let resolved = resolve_pass_environment_with(&names, |name| {
            (name == "ANTHROPIC_AUTH_TOKEN").then(|| "fixture-secret".to_owned())
        })
        .expect("allowed credential");
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "ANTHROPIC_AUTH_TOKEN");
        assert_eq!(resolved[0].1, "fixture-secret");
    }

    #[test]
    fn pass_env_reaches_both_agents_but_not_other_phases_or_artifacts() {
        let temp = TempDir::new().expect("temp");
        let repo = temp.path().join("repo");
        fs::create_dir(&repo).expect("repo");
        let prompt = temp.path().join("prompt.txt");
        fs::write(&prompt, "do the task").expect("prompt");
        let config_path = temp.path().join("bench.yaml");
        fs::write(
            &config_path,
            format!(
                "benchmark_id: pass-env\nmodel_hash: fixture\nmodes: [off, brain]\nseeds: [1]\nwall_seconds: 5\nartifact_dir: artifacts\npass_env: [ANTHROPIC_API_KEY]\ntasks:\n  - id: task\n    repo: {}\n    prompt_file: {}\n    setup:\n      command: 'test -z \"${{ANTHROPIC_API_KEY+x}}\"'\n    agent:\n      command: 'test -n \"$ANTHROPIC_API_KEY\" && printf ''{{\"secret_present\":true}}'' > metrics.json'\n    success:\n      command: 'test -z \"${{ANTHROPIC_API_KEY+x}}\" && test -s metrics.json'\n    metrics_file: metrics.json\n",
                repo.display(),
                prompt.display()
            ),
        )
        .expect("config");
        let secret = "frontier-fixture-secret";
        let artifacts = run_with_environment_lookup(&config_path, None, |name| {
            (name == "ANTHROPIC_API_KEY").then(|| secret.to_owned())
        })
        .expect("paired run");
        assert_eq!(artifacts.records.len(), 2);
        assert!(artifacts.records.iter().all(|record| record.success));
        assert!(
            artifacts
                .records
                .iter()
                .all(|record| { record.metrics["secret_present"].as_bool() == Some(true) })
        );

        for entry in WalkBuilder::new(temp.path().join("artifacts")).build() {
            let entry = entry.expect("artifact entry");
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                let bytes = fs::read(entry.path()).expect("artifact file");
                assert!(
                    !bytes
                        .windows(secret.len())
                        .any(|window| window == secret.as_bytes()),
                    "credential value leaked into {}",
                    entry.path().display()
                );
            }
        }
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
            &[],
            Duration::from_secs(2),
            &temp.path().join("stdout"),
            &temp.path().join("stderr"),
            &prompt,
        )
        .expect("command");
        assert!(result.status.success());
        let pid = fs::read_to_string(temp.path().join("child.pid")).expect("pid");
        // SIGKILL is delivered before run_command returns, but the orphaned
        // child may linger as a zombie until whatever it was reparented to
        // collects it — CI runners can act as slow subreapers, so `kill -0`
        // (which succeeds on zombies) is the wrong probe. A process that is
        // gone OR in state Z is terminated for our purposes; only a live,
        // runnable process is a cleanup failure.
        let mut alive = true;
        for _ in 0..100 {
            let output = Command::new("ps")
                .args(["-o", "state=", "-p", pid.trim()])
                .stderr(Stdio::null())
                .output()
                .expect("ps probe");
            let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
            alive = output.status.success() && !state.is_empty() && !state.starts_with('Z');
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        assert!(!alive, "background child survived benchmark command");
    }
}
