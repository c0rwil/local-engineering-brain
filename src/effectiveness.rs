//! Honest resource accounting and coverage for the engineering brain.
//!
//! Context tokens are estimates; provider usage and costs are observations.
//! These descriptive reports never infer savings from unrelated daily tasks.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{ContextRunId, ProjectId, TaskId};
use crate::storage::Database;

const WEEK_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageMode {
    Off,
    Repo,
    Brain,
}

impl UsageMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Repo => "repo",
            Self::Brain => "brain",
        }
    }
}

/// Additive observations in the provider's token categories. Missing is unknown,
/// not zero. Cache categories are kept separate because providers differ in
/// whether their input-token count includes cached tokens.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageMetrics {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub tool_calls: Option<i64>,
    pub context_bytes: Option<i64>,
    pub context_tokens: Option<i64>,
    pub cost_usd: Option<f64>,
}

/// One immutable additive usage sample (for example, one provider response).
/// Never submit cumulative session totals alongside their constituent calls.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRecordRequest {
    pub sample_id: String,
    pub project_id: ProjectId,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub context_run_id: Option<ContextRunId>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Producer and purpose, for example `codex` or `internal_judge`.
    pub source: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub mode: Option<UsageMode>,
    pub metrics: UsageMetrics,
    /// Observation time, not ingestion time. Omit to use ingestion time.
    #[serde(default)]
    pub observed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageRecordResult {
    pub id: String,
    pub inserted: bool,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectivenessRequest {
    /// None includes all active projects; an empty list includes none.
    pub project_ids: Option<Vec<ProjectId>>,
    /// Inclusive lower bound. Defaults to seven days before the upper bound.
    pub since_ms: Option<i64>,
    /// Exclusive upper bound. Defaults to the current time plus one millisecond.
    pub until_ms: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UsageSummary {
    pub samples: u64,
    pub tasks_with_samples: u64,
    pub contexts_with_samples: u64,
    pub totals: UsageMetrics,
    /// Number of samples reporting each field. A partial total is not a full
    /// session cost; clients must display coverage alongside the total.
    pub reported_samples: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UsageBreakdown {
    pub source: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub mode: Option<UsageMode>,
    pub usage: UsageSummary,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct SourceDelivery {
    pub items: u64,
    pub runs: u64,
    pub estimated_tokens: u64,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ContextCoverage {
    pub runs: u64,
    pub runs_by_mode: BTreeMap<String, u64>,
    pub runs_with_memories: u64,
    pub task_linked_runs: u64,
    /// Contexts compiled in this period with usage observed in this period.
    pub usage_linked_runs: u64,
    pub candidate_count: u64,
    pub selected_items: u64,
    pub estimated_selected_tokens: u64,
    pub compiled_bytes: u64,
    /// Selected items only. Historical storage does not retain candidate kinds.
    pub selected_by_source: BTreeMap<String, SourceDelivery>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct MemoryCoverage {
    pub total: u64,
    pub active_validated: u64,
    pub by_state: BTreeMap<String, u64>,
    pub by_status: BTreeMap<String, u64>,
    pub pending_reviews: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProjectEffectiveness {
    pub project_id: ProjectId,
    pub project_name: String,
    /// Tasks started in the report period, grouped by their current outcome.
    pub tasks_by_status: BTreeMap<String, u64>,
    pub contexts: ContextCoverage,
    /// Current snapshot, independent of the usage report's time window.
    pub memories: MemoryCoverage,
    pub usage: UsageSummary,
    pub usage_by_source: Vec<UsageBreakdown>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct EffectivenessReport {
    pub since_ms: i64,
    pub until_ms: i64,
    pub generated_at_ms: i64,
    pub usage: UsageSummary,
    pub projects: Vec<ProjectEffectiveness>,
    /// Global benchmark ledger; omitted for project/workspace-filtered requests
    /// because legacy evaluation rows do not identify their indexed project.
    pub controlled_comparisons: Vec<crate::evaluation::ControlledUsageComparison>,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct EffectivenessStore {
    database: Database,
}

impl EffectivenessStore {
    #[must_use]
    pub const fn new(database: Database) -> Self {
        Self { database }
    }

    /// Record a usage delta through the daemon's mutation scheduler.
    ///
    /// # Errors
    /// Rejects invalid measurements, unknown or inconsistent references, and
    /// attempts to reuse a sample key with different content.
    pub fn record(&self, request: &UsageRecordRequest) -> Result<UsageRecordResult> {
        validate_request(request)?;
        let request_json = serde_json::to_string(request)?;
        let now = unix_time_ms();
        let observed_at_ms = request.observed_at_ms.unwrap_or(now);
        self.database.write_immediate(|tx| {
            let previous = tx
                .query_row(
                    "SELECT id, request_json, observed_at_ms FROM resource_usage
                 WHERE project_id = ?1 AND source = ?2 AND sample_id = ?3",
                    params![
                        request.project_id.as_str(),
                        request.source,
                        request.sample_id
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((id, original, observed_at_ms)) = previous {
                if original != request_json {
                    return Err(Error::Config(
                        "usage sample key already exists with different content".to_owned(),
                    ));
                }
                return Ok(UsageRecordResult {
                    id,
                    inserted: false,
                    observed_at_ms,
                });
            }
            validate_scope(tx, request)?;
            let id = format!("usage_{}", uuid::Uuid::new_v4());
            let metrics = &request.metrics;
            tx.execute(
                "INSERT INTO resource_usage
                 (id, project_id, sample_id, source, task_id, context_run_id, session_id,
                  provider, model, mode, input_tokens, output_tokens, cache_read_tokens,
                  cache_write_tokens, tool_calls, context_bytes, context_tokens, cost_usd,
                  request_json, observed_at_ms, recorded_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
                params![
                    id,
                    request.project_id.as_str(),
                    request.sample_id,
                    request.source,
                    request.task_id.as_ref().map(TaskId::as_str),
                    request.context_run_id.as_ref().map(ContextRunId::as_str),
                    request.session_id,
                    request.provider,
                    request.model,
                    request.mode.map(UsageMode::as_str),
                    metrics.input_tokens,
                    metrics.output_tokens,
                    metrics.cache_read_tokens,
                    metrics.cache_write_tokens,
                    metrics.tool_calls,
                    metrics.context_bytes,
                    metrics.context_tokens,
                    metrics.cost_usd,
                    request_json,
                    observed_at_ms,
                    now
                ],
            )?;
            Ok(UsageRecordResult {
                id,
                inserted: true,
                observed_at_ms,
            })
        })
    }

    /// Summarize measured usage and context delivery in a bounded time window.
    ///
    /// # Errors
    /// Returns validation, corrupt stored data, overflow, or storage errors.
    pub fn report(&self, request: &EffectivenessRequest) -> Result<EffectivenessReport> {
        let now = unix_time_ms();
        let until = request.until_ms.unwrap_or(now.saturating_add(1));
        let since = request
            .since_ms
            .unwrap_or_else(|| until.saturating_sub(WEEK_MS).max(0));
        if since < 0 || until <= since {
            return Err(Error::Config(
                "usage report requires 0 <= since_ms < until_ms".to_owned(),
            ));
        }
        self.database.with_reader(|connection| {
            // All sections share one snapshot, including current memory state.
            let tx = connection.unchecked_transaction()?;
            let mut statement = tx.prepare("SELECT id, name FROM projects WHERE tombstoned_at_ms IS NULL ORDER BY name, id")?;
            let projects = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut reports = Vec::new();
            let mut usage = UsageSummary::default();
            for (id, name) in projects {
                let id = ProjectId::parse(&id)?;
                if request.project_ids.as_ref().is_some_and(|ids| !ids.contains(&id)) { continue; }
                let report = project_report(&tx, &id, name, since, until)?;
                merge_usage(&mut usage, &report.usage)?;
                reports.push(report);
            }
            let controlled_comparisons = if request.project_ids.is_none() {
                let mut evaluations = tx.prepare("SELECT metrics_json FROM eval_runs WHERE created_at_ms >= ?1 AND created_at_ms < ?2 ORDER BY created_at_ms")?;
                let records = evaluations.query_map(params![since, until], |row| row.get::<_, String>(0))?
                    .map(|row| Ok(serde_json::from_str::<crate::evaluation::EvaluationRecord>(&row?)?))
                    .collect::<Result<Vec<_>>>()?;
                crate::evaluation::compare_usage_records(&records)
            } else { Vec::new() };
            Ok(EffectivenessReport {
                since_ms: since, until_ms: until, generated_at_ms: now, usage, projects: reports, controlled_comparisons,
                limitations: vec![
                    "Daily usage is observational. Savings require controlled trials with matched tasks, model, configuration and correctness checks.".to_owned(),
                    "Missing measurements remain null. Totals cover reported samples only; per-field coverage is included. Samples must be additive deltas, not cumulative session totals.".to_owned(),
                    "Compiled context tokens are estimates, not billed tokens. Compilation does not prove the agent read or applied the context. Candidate source kinds and hook text outside context_runs were not historically retained.".to_owned(),
                    "Cache tokens retain the provider's categories and are not added to input tokens. Costs are reported USD observations, never inferred from token estimates. Source breakdowns include internal judgment overhead when reported.".to_owned(),
                    "Controlled comparisons use matching stored benchmark/model/configuration/snapshot hashes and unique task/seed pairs. Savings on completed work use only pairs where both attempts pass and both measurements are complete. All-attempt totals retain failures, unscorable outcomes and reported partial usage. Missing and partial coverage is explicit. Legacy benchmark rows lack project identity, so comparisons appear only in unfiltered reports.".to_owned(),
                ],
            })
        })
    }
}

fn validate_request(request: &UsageRecordRequest) -> Result<()> {
    for (label, value, limit) in [
        ("sample_id", Some(request.sample_id.as_str()), 256),
        ("source", Some(request.source.as_str()), 128),
        ("session_id", request.session_id.as_deref(), 256),
        ("provider", request.provider.as_deref(), 128),
        ("model", request.model.as_deref(), 256),
    ] {
        if value.is_some_and(|v| {
            v.trim().is_empty() || v.len() > limit || v.chars().any(char::is_control)
        }) {
            return Err(Error::Config(format!(
                "usage {label} must be nonempty, bounded text without control characters"
            )));
        }
    }
    for (label, value) in integer_metrics(&request.metrics) {
        if value.is_some_and(|value| value < 0) {
            return Err(Error::Config(format!("usage {label} cannot be negative")));
        }
    }
    if request
        .metrics
        .cost_usd
        .is_some_and(|cost| !cost.is_finite() || cost < 0.0)
    {
        return Err(Error::Config(
            "usage cost_usd must be finite and nonnegative".to_owned(),
        ));
    }
    if request.observed_at_ms.is_some_and(|value| value < 0) {
        return Err(Error::Config(
            "usage observed_at_ms cannot be negative".to_owned(),
        ));
    }
    if integer_metrics(&request.metrics)
        .iter()
        .all(|(_, v)| v.is_none())
        && request.metrics.cost_usd.is_none()
    {
        return Err(Error::Config(
            "usage sample must contain at least one observed metric".to_owned(),
        ));
    }
    Ok(())
}

fn validate_scope(connection: &Connection, request: &UsageRecordRequest) -> Result<()> {
    let project_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM projects WHERE id = ?1 AND tombstoned_at_ms IS NULL)",
        [request.project_id.as_str()],
        |row| row.get(0),
    )?;
    if !project_exists {
        return Err(Error::Config(
            "usage project is unknown or tombstoned".to_owned(),
        ));
    }
    if let Some(task_id) = &request.task_id {
        let task: Option<(String, Option<String>)> = connection
            .query_row(
                "SELECT project_id, session_id FROM tasks WHERE id = ?1",
                [task_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, session)) = task else {
            return Err(Error::Config("usage task is unknown".to_owned()));
        };
        if project != request.project_id.as_str()
            || request
                .session_id
                .as_ref()
                .zip(session.as_ref())
                .is_some_and(|(a, b)| a != b)
        {
            return Err(Error::Config(
                "usage task does not match the project or session".to_owned(),
            ));
        }
    }
    if let Some(context_id) = &request.context_run_id {
        let context: Option<(String, String)> = connection
            .query_row(
                "SELECT project_id, mode FROM context_runs WHERE id = ?1",
                [context_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((project, mode)) = context else {
            return Err(Error::Config("usage context is unknown".to_owned()));
        };
        if project != request.project_id.as_str()
            || request.mode.is_some_and(|value| value.as_str() != mode)
        {
            return Err(Error::Config(
                "usage context does not match the project or mode".to_owned(),
            ));
        }
        if let Some(task_id) = &request.task_id {
            let mismatched: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM usage_events WHERE context_run_id = ?1
                 AND source_type = 'task' AND usage_kind = 'context_served' AND source_id <> ?2)",
                params![context_id.as_str(), task_id.as_str()],
                |row| row.get(0),
            )?;
            if mismatched {
                return Err(Error::Config(
                    "usage context is linked to a different task".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn integer_metrics(metrics: &UsageMetrics) -> [(&'static str, Option<i64>); 7] {
    [
        ("input_tokens", metrics.input_tokens),
        ("output_tokens", metrics.output_tokens),
        ("cache_read_tokens", metrics.cache_read_tokens),
        ("cache_write_tokens", metrics.cache_write_tokens),
        ("tool_calls", metrics.tool_calls),
        ("context_bytes", metrics.context_bytes),
        ("context_tokens", metrics.context_tokens),
    ]
}

fn project_report(
    connection: &Connection,
    id: &ProjectId,
    name: String,
    since: i64,
    until: i64,
) -> Result<ProjectEffectiveness> {
    let scope = params![id.as_str(), since, until];
    let tasks_by_status = counts(
        connection,
        "SELECT status, count(*) FROM tasks WHERE project_id = ?1 AND started_at_ms >= ?2 AND started_at_ms < ?3 GROUP BY status",
        scope,
    )?;
    let mut contexts = connection.query_row(
        "SELECT count(*), coalesce(sum(candidate_count),0), coalesce(sum(selected_tokens),0),
                coalesce(sum(length(CAST(stable_prefix AS BLOB)) + length(CAST(dynamic_context AS BLOB))),0),
                count(CASE WHEN EXISTS(SELECT 1 FROM context_items i WHERE i.context_run_id = c.id AND i.source_type IN ('memory','linked_memory')) THEN 1 END),
                count(CASE WHEN EXISTS(SELECT 1 FROM usage_events u WHERE u.context_run_id = c.id AND u.source_type = 'task' AND u.usage_kind = 'context_served') THEN 1 END),
                count(CASE WHEN EXISTS(SELECT 1 FROM resource_usage u WHERE u.context_run_id = c.id AND u.observed_at_ms >= ?2 AND u.observed_at_ms < ?3) THEN 1 END)
         FROM context_runs c WHERE project_id = ?1 AND created_at_ms >= ?2 AND created_at_ms < ?3", scope,
        |row| Ok(ContextCoverage { runs: read_count(row, 0)?, candidate_count: read_count(row, 1)?, estimated_selected_tokens: read_count(row, 2)?,
            compiled_bytes: read_count(row, 3)?, runs_with_memories: read_count(row, 4)?, task_linked_runs: read_count(row, 5)?, usage_linked_runs: read_count(row, 6)?, ..ContextCoverage::default() }),
    )?;
    contexts.runs_by_mode = counts(
        connection,
        "SELECT mode, count(*) FROM context_runs WHERE project_id = ?1 AND created_at_ms >= ?2 AND created_at_ms < ?3 GROUP BY mode",
        scope,
    )?;
    let mut source_statement = connection.prepare(
        "SELECT i.source_type, count(*), count(DISTINCT c.id), sum(i.tokens)
         FROM context_items i JOIN context_runs c ON c.id = i.context_run_id
         WHERE c.project_id = ?1 AND c.created_at_ms >= ?2 AND c.created_at_ms < ?3 GROUP BY i.source_type")?;
    contexts.selected_by_source = source_statement
        .query_map(scope, |row| {
            Ok((
                row.get(0)?,
                SourceDelivery {
                    items: read_count(row, 1)?,
                    runs: read_count(row, 2)?,
                    estimated_tokens: read_count(row, 3)?,
                },
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    contexts.selected_items = contexts
        .selected_by_source
        .values()
        .map(|source| source.items)
        .sum();
    let memories = memory_coverage(connection, id)?;
    let usage = read_usage(connection, id, since, until, None)?;
    let mut groups_statement = connection.prepare("SELECT DISTINCT source, provider, model, mode FROM resource_usage WHERE project_id = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3 ORDER BY source, provider, model, mode")?;
    let groups = groups_statement
        .query_map(scope, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut usage_by_source = Vec::new();
    for (source, provider, model, mode) in groups {
        let usage = read_usage(
            connection,
            id,
            since,
            until,
            Some((
                &source,
                provider.as_deref(),
                model.as_deref(),
                mode.as_deref(),
            )),
        )?;
        let mode = mode
            .map(|value| match value.as_str() {
                "off" => Ok(UsageMode::Off),
                "repo" => Ok(UsageMode::Repo),
                "brain" => Ok(UsageMode::Brain),
                _ => Err(Error::Config("invalid stored usage mode".to_owned())),
            })
            .transpose()?;
        usage_by_source.push(UsageBreakdown {
            source,
            provider,
            model,
            mode,
            usage,
        });
    }
    Ok(ProjectEffectiveness {
        project_id: id.clone(),
        project_name: name,
        tasks_by_status,
        contexts,
        memories,
        usage,
        usage_by_source,
    })
}

fn memory_coverage(connection: &Connection, id: &ProjectId) -> Result<MemoryCoverage> {
    let mut memories = connection.query_row(
        "SELECT count(*), count(CASE WHEN status = 'active' AND epistemic_state = 'validated' THEN 1 END)
         FROM memories WHERE project_id = ?1", [id.as_str()],
        |row| Ok(MemoryCoverage { total: read_count(row, 0)?, active_validated: read_count(row, 1)?, ..MemoryCoverage::default() }),
    )?;
    memories.by_state = counts(
        connection,
        "SELECT epistemic_state, count(*) FROM memories WHERE project_id = ?1 GROUP BY epistemic_state",
        [id.as_str()],
    )?;
    memories.by_status = counts(
        connection,
        "SELECT status, count(*) FROM memories WHERE project_id = ?1 GROUP BY status",
        [id.as_str()],
    )?;
    memories.pending_reviews = connection.query_row(
        "SELECT count(*) FROM lesson_reviews WHERE project_id = ?1 AND status = 'pending'",
        [id.as_str()],
        |row| read_count(row, 0),
    )?;
    Ok(memories)
}

fn counts(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<BTreeMap<String, u64>> {
    Ok(connection
        .prepare(sql)?
        .query_map(params, |row| Ok((row.get(0)?, read_count(row, 1)?)))?
        .collect::<std::result::Result<_, _>>()?)
}

fn read_count(row: &rusqlite::Row<'_>, column: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(column)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(column, value))
}

type UsageGroup<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);

fn read_usage(
    connection: &Connection,
    id: &ProjectId,
    since: i64,
    until: i64,
    group: Option<UsageGroup<'_>>,
) -> Result<UsageSummary> {
    let (source, provider, model, mode) = group.unwrap_or(("", None, None, None));
    let sql = "SELECT count(*), count(DISTINCT task_id), count(DISTINCT context_run_id),
        sum(input_tokens), sum(output_tokens), sum(cache_read_tokens), sum(cache_write_tokens),
        sum(tool_calls), sum(context_bytes), sum(context_tokens), sum(cost_usd),
        count(input_tokens), count(output_tokens), count(cache_read_tokens), count(cache_write_tokens),
        count(tool_calls), count(context_bytes), count(context_tokens), count(cost_usd)
        FROM resource_usage WHERE project_id = ?1 AND observed_at_ms >= ?2 AND observed_at_ms < ?3
        AND (?4 = 0 OR (source = ?5 AND provider IS ?6 AND model IS ?7 AND mode IS ?8))";
    Ok(connection.query_row(
        sql,
        params![
            id.as_str(),
            since,
            until,
            group.is_some(),
            source,
            provider,
            model,
            mode
        ],
        |row| {
            let totals = UsageMetrics {
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                cache_write_tokens: row.get(6)?,
                tool_calls: row.get(7)?,
                context_bytes: row.get(8)?,
                context_tokens: row.get(9)?,
                cost_usd: row.get(10)?,
            };
            let names = [
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "tool_calls",
                "context_bytes",
                "context_tokens",
                "cost_usd",
            ];
            let reported_samples = names
                .iter()
                .enumerate()
                .map(|(i, name)| Ok(((*name).to_owned(), read_count(row, i + 11)?)))
                .collect::<std::result::Result<_, rusqlite::Error>>()?;
            Ok(UsageSummary {
                samples: read_count(row, 0)?,
                tasks_with_samples: read_count(row, 1)?,
                contexts_with_samples: read_count(row, 2)?,
                totals,
                reported_samples,
            })
        },
    )?)
}

fn merge_usage(into: &mut UsageSummary, other: &UsageSummary) -> Result<()> {
    into.samples += other.samples;
    into.tasks_with_samples += other.tasks_with_samples;
    into.contexts_with_samples += other.contexts_with_samples;
    for (name, count) in &other.reported_samples {
        *into.reported_samples.entry(name.clone()).or_default() += count;
    }
    macro_rules! add {
        ($($field:ident),*) => { $(if let Some(value) = other.totals.$field {
            into.totals.$field = Some(into.totals.$field.unwrap_or(0).checked_add(value).ok_or_else(|| Error::Config("usage totals exceed integer range".to_owned()))?);
        })* };
    }
    add!(
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        tool_calls,
        context_bytes,
        context_tokens
    );
    if let Some(value) = other.totals.cost_usd {
        let total = into.totals.cost_usd.unwrap_or(0.0) + value;
        if !total.is_finite() {
            return Err(Error::Config(
                "usage cost totals exceed numeric range".to_owned(),
            ));
        }
        into.totals.cost_usd = Some(total);
    }
    Ok(())
}

fn unix_time_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Database, ProjectId, TaskId, ContextRunId) {
        let temp = tempfile::tempdir().expect("tempdir");
        let database = Database::open(temp.path().join("brain.sqlite")).expect("database");
        let project = ProjectId::new();
        let task = TaskId::new();
        let context = ContextRunId::new();
        database.write_immediate(|tx| {
            tx.execute("INSERT INTO projects (id,name,created_at_ms) VALUES (?1,'test',100)", [project.as_str()])?;
            tx.execute("INSERT INTO tasks (id,project_id,session_id,prompt,prompt_digest,status,started_at_ms) VALUES (?1,?2,'session','task','digest','success',100)", params![task.as_str(),project.as_str()])?;
            tx.execute("INSERT INTO context_runs (id,request_id,project_id,request_fingerprint,mode,task_text_digest,token_budget,selected_tokens,candidate_count,stable_prefix_hash,stable_prefix,dynamic_context,compile_ms,created_at_ms) VALUES (?1,'request',?2,'fingerprint','brain','digest',1000,25,3,'hash','facts','memory',1,100)", params![context.as_str(),project.as_str()])?;
            tx.execute("INSERT INTO context_items (context_run_id,ordinal,source_type,source_id,score,tokens,why_json) VALUES (?1,0,'memory','mem_fixture',1.0,10,'{}')", [context.as_str()])?;
            tx.execute("INSERT INTO usage_events (id,source_type,source_id,context_run_id,usage_kind,created_at_ms) VALUES ('use_fixture','task',?1,?2,'context_served',100)",params![task.as_str(),context.as_str()])?;
            Ok(())
        }).expect("fixtures");
        (temp, database, project, task, context)
    }

    fn request(project: &ProjectId, task: &TaskId, context: &ContextRunId) -> UsageRecordRequest {
        UsageRecordRequest {
            sample_id: "call-1".to_owned(),
            project_id: project.clone(),
            task_id: Some(task.clone()),
            context_run_id: Some(context.clone()),
            session_id: Some("session".to_owned()),
            source: "codex".to_owned(),
            provider: Some("openai".to_owned()),
            model: Some("model".to_owned()),
            mode: Some(UsageMode::Brain),
            metrics: UsageMetrics {
                input_tokens: Some(100),
                cost_usd: Some(0.2),
                ..UsageMetrics::default()
            },
            observed_at_ms: Some(110),
        }
    }

    fn period() -> EffectivenessRequest {
        EffectivenessRequest {
            since_ms: Some(100),
            until_ms: Some(200),
            ..EffectivenessRequest::default()
        }
    }

    #[test]
    fn usage_is_idempotent_without_inflating_totals_and_conflicting_retries_fail() {
        let (_temp, database, project, task, context) = fixture();
        let store = EffectivenessStore::new(database);
        let mut request = request(&project, &task, &context);
        // An omitted observed_at remains idempotent even when clock time changes.
        request.observed_at_ms = None;
        let first = store.record(&request).expect("first");
        let second = store.record(&request).expect("retry");
        assert!(first.inserted);
        assert!(!second.inserted);
        assert_eq!(first.id, second.id);
        assert_eq!(first.observed_at_ms, second.observed_at_ms);
        request.metrics.input_tokens = Some(101);
        assert!(store.record(&request).is_err());
        let report = store
            .report(&EffectivenessRequest::default())
            .expect("report");
        assert_eq!(report.usage.samples, 1);
        assert_eq!(report.usage.totals.input_tokens, Some(100));
    }

    #[test]
    fn partial_measurements_preserve_unknowns_and_count_field_coverage() {
        let (_temp, database, project, task, context) = fixture();
        let store = EffectivenessStore::new(database);
        let mut request = request(&project, &task, &context);
        store.record(&request).expect("first");
        request.sample_id = "tool-1".to_owned();
        request.metrics = UsageMetrics {
            tool_calls: Some(2),
            output_tokens: Some(0),
            ..UsageMetrics::default()
        };
        store.record(&request).expect("second");
        request.sample_id = "judge-1".to_owned();
        request.source = "internal_judge".to_owned();
        request.metrics = UsageMetrics {
            input_tokens: Some(20),
            cost_usd: Some(0.1),
            ..UsageMetrics::default()
        };
        store.record(&request).expect("judge");
        let report = store.report(&period()).expect("report");
        assert_eq!(report.usage.samples, 3);
        assert_eq!(report.usage.totals.input_tokens, Some(120));
        assert_eq!(report.usage.totals.output_tokens, Some(0));
        assert_eq!(report.usage.totals.cache_read_tokens, None);
        assert_eq!(report.usage.totals.tool_calls, Some(2));
        assert!((report.usage.totals.cost_usd.expect("cost") - 0.3).abs() < 1e-10);
        assert_eq!(report.usage.reported_samples["input_tokens"], 2);
        assert_eq!(report.usage.reported_samples["output_tokens"], 1);
        assert_eq!(report.usage.reported_samples["cache_read_tokens"], 0);
        assert_eq!(report.usage.tasks_with_samples, 1);
        assert_eq!(report.usage.contexts_with_samples, 1);
        let project = &report.projects[0];
        assert_eq!(project.usage_by_source.len(), 2);
        assert_eq!(project.contexts.runs, 1);
        assert_eq!(project.contexts.runs_with_memories, 1);
        assert_eq!(project.contexts.task_linked_runs, 1);
        assert_eq!(project.contexts.usage_linked_runs, 1);
        assert_eq!(project.contexts.estimated_selected_tokens, 25);
        assert_eq!(
            project.contexts.selected_by_source["memory"].estimated_tokens,
            10
        );
        assert_eq!(project.contexts.candidate_count, 3);
    }

    #[test]
    fn foreign_task_context_session_and_mode_are_rejected() {
        let (_temp, database, project, task, context) = fixture();
        let foreign = ProjectId::new();
        let another_task = TaskId::new();
        database.write_immediate(|tx| {
            tx.execute("INSERT INTO projects (id,name,created_at_ms) VALUES (?1,'foreign',100)",[foreign.as_str()])?;
            tx.execute("INSERT INTO tasks (id,project_id,prompt,prompt_digest,status,started_at_ms) VALUES (?1,?2,'other','digest','open',100)",params![another_task.as_str(),project.as_str()])?;
            Ok(())
        }).expect("foreign fixture");
        let store = EffectivenessStore::new(database);
        let original = request(&project, &task, &context);
        let mut bad = original.clone();
        bad.project_id = foreign.clone();
        bad.context_run_id = None;
        assert!(store.record(&bad).is_err());
        bad = original.clone();
        bad.project_id = foreign;
        bad.task_id = None;
        assert!(store.record(&bad).is_err());
        bad = original.clone();
        bad.session_id = Some("wrong-session".to_owned());
        assert!(store.record(&bad).is_err());
        bad = original.clone();
        bad.task_id = Some(another_task);
        assert!(store.record(&bad).is_err());
        bad = original;
        bad.mode = Some(UsageMode::Off);
        assert!(store.record(&bad).is_err());
        assert_eq!(store.report(&period()).expect("report").usage.samples, 0);
    }

    #[test]
    fn malformed_observations_and_empty_samples_fail() {
        let (_temp, database, project, task, context) = fixture();
        let store = EffectivenessStore::new(database);
        let mut request = request(&project, &task, &context);
        for bad_cost in [f64::NAN, f64::INFINITY, -0.1] {
            request.metrics.cost_usd = Some(bad_cost);
            assert!(store.record(&request).is_err());
        }
        request.metrics = UsageMetrics {
            tool_calls: Some(-1),
            ..UsageMetrics::default()
        };
        assert!(store.record(&request).is_err());
        request.metrics = UsageMetrics::default();
        assert!(store.record(&request).is_err());
    }

    #[test]
    fn time_windows_and_empty_workspace_do_not_leak_usage() {
        let (_temp, database, project, task, context) = fixture();
        let store = EffectivenessStore::new(database);
        store
            .record(&request(&project, &task, &context))
            .expect("record");
        let mut period = period();
        period.until_ms = Some(110);
        let report = store.report(&period).expect("exclusive upper bound");
        assert_eq!(report.usage.samples, 0);
        assert_eq!(report.usage.totals.input_tokens, None);
        period.until_ms = Some(200);
        period.project_ids = Some(Vec::new());
        let report = store.report(&period).expect("empty workspace");
        assert!(report.projects.is_empty());
        assert_eq!(report.usage.samples, 0);
        assert_eq!(report.usage.totals.cost_usd, None);
        period.since_ms = Some(200);
        assert!(store.report(&period).is_err());
    }

    #[test]
    fn stored_benchmarks_are_not_attributed_to_project_scoped_reports() {
        use crate::evaluation::{EvaluationMode, EvaluationRecord, EvaluationStore};
        use crate::ids::EvaluationRunId;
        let (_temp, database, project, _task, _context) = fixture();
        let evaluations = EvaluationStore::new(database.clone());
        for mode in [EvaluationMode::Off, EvaluationMode::Brain] {
            evaluations
                .record(&EvaluationRecord {
                    run_id: EvaluationRunId::new(),
                    benchmark_id: "paired".to_owned(),
                    task_id: "fixture".to_owned(),
                    mode,
                    seed: 1,
                    ordinal: 1,
                    model_hash: "model".to_owned(),
                    config_hash: "config".to_owned(),
                    brain_snapshot_hash: "snapshot".to_owned(),
                    success: true,
                    timed_out: false,
                    setup_exit_code: None,
                    agent_exit_code: Some(0),
                    validation_exit_code: Some(0),
                    wall_ms: 1,
                    metrics: serde_json::json!({"reported":{"total_cost_usd":1.0}}),
                    artifact_path: "fixture".into(),
                    created_at_ms: 110,
                })
                .expect("record evaluation");
        }
        let store = EffectivenessStore::new(database);
        assert_eq!(
            store
                .report(&period())
                .expect("global report")
                .controlled_comparisons
                .len(),
            1
        );
        let mut scoped = period();
        scoped.project_ids = Some(vec![project]);
        assert!(
            store
                .report(&scoped)
                .expect("project report")
                .controlled_comparisons
                .is_empty()
        );
    }
}
