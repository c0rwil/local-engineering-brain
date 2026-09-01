use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use leb::Result;
use leb::config::{Config, ConfigOverrides};
use leb::context::{ContextCompileRequest, ContextMode};
use leb::daemon::{
    AttemptCreateRequest, EvidenceCreateRequest, JobEnqueueRequest, JobListRequest,
    MemoryCreateRequest, MemoryListRequest, MemoryPromoteRequest, ProjectExportRequest,
    SemanticRebuildRequest, SkillPromoteRequest, SynthesisRunRequest, TaskFinishRequest,
    TaskStartRequest, WorldSearchRequest,
};
use leb::daemon::{DaemonClient, ProjectStatus};
use leb::doctor::DoctorReport;
use leb::evaluation::EvaluationRunner;
use leb::events::EventStore;
use leb::experience::{AttemptOutcome, EvidenceStrength, TaskOutcome};
use leb::ids::{AttemptId, EvidenceId, JobId, MemoryId, TaskId};
use leb::jobs::{JobRecord, JobStatus, JobType};
use leb::learning::DistillationRequest;
use leb::memory::{EpistemicState, MemoryDraft};
use leb::projects::GitState;
use leb::storage::Database;

#[derive(Debug, Parser)]
#[command(
    name = "leb",
    version,
    about = "Local Engineering Brain",
    long_about = "Local-first engineering memory, world-model, and runtime tooling"
)]
struct Cli {
    /// Override the canonical `SQLite` database path.
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Override the local daemon Unix socket path.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Check configuration, canonical storage, and local-only safety defaults.
    Doctor {
        #[arg(long)]
        json: bool,
    },

    /// Resolve/register a project and show its current Git and brain state.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,

        #[arg(long)]
        json: bool,
    },

    /// Inspect or control the local daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },

    /// Show process-lifetime daemon metrics (reset on daemon restart).
    Metrics {
        #[arg(long)]
        json: bool,
    },

    /// Render a single-shot plain-text observability dashboard.
    Dashboard,

    /// Build or refresh the structural world model.
    Index {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_name = "PATH")]
        changed: Vec<PathBuf>,
        /// Import a bounded Graphify JSON export instead of native tree-sitter.
        #[arg(long, value_name = "JSON")]
        graphify: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },

    /// Search active structural nodes.
    Graph {
        query: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },

    /// Preview the exact bounded context that the proxy would inject.
    Context {
        task: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        budget: Option<u32>,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },

    /// Inspect and manage durable memory.
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },

    /// Manage the derived semantic vector sidecar.
    Semantic {
        #[command(subcommand)]
        action: SemanticAction,
    },

    /// Inspect and enqueue durable daemon background jobs.
    Jobs {
        #[command(subcommand)]
        action: JobsAction,
    },

    /// Run a reproducible paired brain-off/brain-on benchmark.
    Eval {
        #[command(subcommand)]
        action: EvalAction,
    },

    /// Capture task episodes, attempts, evidence, and observed outcomes.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },

    /// Distill a successful evidence-linked task into candidate memory.
    Learn {
        claim: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        task: Vec<String>,
        #[arg(long)]
        evidence: Vec<String>,
        #[arg(long, default_value = "lesson")]
        kind: String,
        #[arg(long, default_value = "Evidence-linked task distillation")]
        rationale: String,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        json: bool,
    },

    /// Synthesize procedures from repeated evidence-backed successful episodes.
    Synthesize {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Promote this validated procedure memory to the `skill` kind
        /// instead of running synthesis (requires the configured number of
        /// recorded distinct successful uses).
        #[arg(long)]
        promote: Option<String>,
    },

    /// Link repositories so tasks in one see validated knowledge from the
    /// others (multi-repo/microservice workspaces).
    Link {
        #[command(subcommand)]
        action: LinkAction,
    },

    /// Create or verify transactionally consistent canonical backups.
    Backup {
        #[command(subcommand)]
        action: BackupAction,
    },

    /// Export one project's canonical state as bounded-memory NDJSON.
    Export {
        destination: PathBuf,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },

    /// Internal durability-test helper. This command never commits or returns.
    #[command(hide = true)]
    InternalCrashTransaction {
        #[arg(long)]
        ready: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum LinkAction {
    /// Link another repository's brain into this project's context.
    Add {
        /// Path to the repository to link (registered if new).
        target: PathBuf,
        /// Source project path whose context gains the link.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// How this project relates to the target (e.g. `uses`,
        /// `interface`, `sibling-service`).
        #[arg(long, default_value = "uses")]
        relation: String,
        /// Optional free-form note stored with the link.
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove a repository link.
    Remove {
        /// Path to the linked repository.
        target: PathBuf,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List this project's outgoing links.
    List {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum DaemonAction {
    /// Report daemon liveness and bounded-resource utilization.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Ask the daemon to finish in-flight requests and stop.
    Stop {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryAction {
    /// List bounded project memories without changing retrieval utility.
    List {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Add a scoped candidate memory. It is not auto-injected until promoted.
    Add {
        claim: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "Explicit developer-provided memory")]
        rationale: String,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long, default_value_t = 0.5)]
        confidence: f64,
        #[arg(long, default_value_t = 0.5)]
        importance: f64,
        #[arg(long)]
        json: bool,
    },
    /// Show one memory.
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Advance memory state; skipping states requires `--human-override`.
    Promote {
        id: String,
        #[arg(long)]
        to: EpistemicState,
        #[arg(long)]
        human_override: bool,
        #[arg(long)]
        json: bool,
    },
    /// Logically forget a memory while retaining its audit record.
    Forget {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Irreversibly purge content and all derived retrieval material.
    Purge {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SemanticAction {
    /// Rebuild the rebuildable vector sidecar from canonical memories.
    Rebuild {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum JobsAction {
    /// List newest jobs, optionally filtered by status.
    List {
        /// pending | running | succeeded | failed | dead
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Enqueue one allowlisted job type: `fts_integrity`, `semantic_rebuild`,
    /// `consolidation_gc`, or `evidence_reverify`. Enqueue is idempotent by
    /// (type, dedupe key).
    Enqueue {
        job_type: String,
        /// Project scope; required by `semantic_rebuild` and `evidence_reverify`.
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        dedupe_key: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one job.
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum EvalAction {
    /// Run a benchmark config in isolated repository copies.
    Run {
        config: PathBuf,
        #[arg(long, value_name = "DIR")]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum TaskAction {
    Start {
        prompt: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    Attempt {
        task_id: String,
        strategy: String,
        #[arg(long, value_enum, default_value_t = AttemptOutcome::Unknown)]
        outcome: AttemptOutcome,
        #[arg(long)]
        file: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Evidence {
        kind: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        task_id: Option<String>,
        #[arg(long)]
        attempt_id: Option<String>,
        #[arg(long, value_enum)]
        strength: EvidenceStrength,
        #[arg(long)]
        command: Option<String>,
        #[arg(long)]
        exit_code: Option<i32>,
        #[arg(long)]
        artifact: Option<PathBuf>,
        #[arg(long, default_value = "{}")]
        metadata: String,
        #[arg(long)]
        json: bool,
    },
    Finish {
        task_id: String,
        #[arg(long, value_enum)]
        outcome: TaskOutcome,
        #[arg(long)]
        accepted_by_user: Option<bool>,
        #[arg(long)]
        final_commit: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum BackupAction {
    Create {
        destination: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Verify {
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(success) => {
            if success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(error) => {
            eprintln!("leb: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> Result<bool> {
    let overrides = ConfigOverrides {
        db_path: cli.db,
        local_only: None,
    };
    let socket_override = cli.socket;
    match cli.command {
        Commands::Doctor { json } => run_doctor(&overrides, socket_override.as_deref(), json),
        Commands::Status { path, json } => {
            run_status(&overrides, socket_override.as_deref(), &path, json)
        }
        Commands::Daemon { action } => {
            run_daemon_action(&overrides, socket_override.as_deref(), &action)
        }
        Commands::Link { action } => run_link(&overrides, socket_override.as_deref(), &action),
        Commands::Metrics { json } => run_metrics(&overrides, socket_override.as_deref(), json),
        Commands::Dashboard => run_dashboard(&overrides, socket_override.as_deref()),
        Commands::Index {
            path,
            changed,
            graphify,
            json,
        } => run_index(
            &overrides,
            socket_override.as_deref(),
            &path,
            &changed,
            graphify.as_deref(),
            json,
        ),
        Commands::Graph {
            query,
            path,
            limit,
            json,
        } => run_graph(
            &overrides,
            socket_override.as_deref(),
            &path,
            &query,
            limit,
            json,
        ),
        Commands::Context {
            task,
            path,
            budget,
            off,
            json,
        } => run_context(
            &overrides,
            socket_override.as_deref(),
            &path,
            &task,
            budget,
            off,
            json,
        ),
        Commands::Memory { action } => run_memory(&overrides, socket_override.as_deref(), &action),
        Commands::Semantic { action } => {
            run_semantic(&overrides, socket_override.as_deref(), &action)
        }
        Commands::Jobs { action } => run_jobs(&overrides, socket_override.as_deref(), &action),
        Commands::Eval { action } => run_eval(&overrides, socket_override.as_deref(), &action),
        Commands::Task { action } => run_task(&overrides, socket_override.as_deref(), &action),
        Commands::Learn {
            claim,
            path,
            task,
            evidence,
            kind,
            rationale,
            tag,
            json,
        } => run_learn(
            &overrides,
            socket_override.as_deref(),
            &path,
            &claim,
            &task,
            &evidence,
            &kind,
            &rationale,
            &tag,
            json,
        ),
        Commands::Synthesize {
            path,
            json,
            promote,
        } => run_synthesize(
            &overrides,
            socket_override.as_deref(),
            &path,
            json,
            promote.as_deref(),
        ),
        Commands::Backup { action } => run_backup(&overrides, socket_override.as_deref(), &action),
        Commands::Export {
            destination,
            path,
            json,
        } => run_export(
            &overrides,
            socket_override.as_deref(),
            &path,
            &destination,
            json,
        ),
        Commands::InternalCrashTransaction { ready } => {
            let config = Config::load_for(None, &overrides)?;
            let database = Database::open(&config.storage.db_path)?;
            EventStore::new(database).hold_uncommitted_test_append(&ready)?;
            Ok(false)
        }
    }
}

fn run_backup(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &BackupAction,
) -> Result<bool> {
    let config = load_daemon_config(overrides, socket, None)?;
    let client = client(&config);
    match action {
        BackupAction::Create { destination, json } => {
            block_on(client.create_backup(destination))?;
            if *json {
                println!(
                    "{}",
                    serde_json::json!({
                        "created": true,
                        "destination": destination,
                    })
                );
            } else {
                println!("Backup created: {}", destination.display());
            }
        }
        BackupAction::Verify { path, json } => {
            let report = block_on(client.verify_backup(path))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Backup {}: integrity={}, journal={}, schema={}",
                    path.display(),
                    report.integrity,
                    report.journal_mode,
                    report.migration_version
                );
            }
            if report.integrity != "ok" {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn run_export(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    destination: &Path,
    json: bool,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let client = client(&config);
    let project = block_on(client.resolve_project(path))?;
    let report = block_on(client.export_project(&ProjectExportRequest {
        project_id: project.resolved.project.id,
        destination: destination.to_path_buf(),
    }))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Exported {} records ({} bytes) to {}",
            report.records,
            report.bytes,
            report.destination.display()
        );
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn run_learn(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    claim: &str,
    tasks: &[String],
    evidence: &[String],
    kind: &str,
    rationale: &str,
    tags: &[String],
    json: bool,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let client = client(&config);
    let status = block_on(client.resolve_project(path))?;
    let project_id = status.resolved.project.id;
    let result = block_on(
        client.distill(&DistillationRequest {
            project_id: project_id.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: project_id.to_string(),
                kind: kind.to_owned(),
                claim: claim.to_owned(),
                rationale: rationale.to_owned(),
                confidence: 0.5,
                importance: 0.7,
                tags: tags.to_vec(),
                valid_from_commit: status.resolved.git.and_then(|git| git.head),
                valid_to_commit: None,
                trust_source: "agent_inference".to_owned(),
                evidence_ids: evidence
                    .iter()
                    .map(|id| EvidenceId::parse(id))
                    .collect::<Result<_>>()?,
                source_task_ids: tasks
                    .iter()
                    .map(|id| TaskId::parse(id))
                    .collect::<Result<_>>()?,
                node_ids: Vec::new(),
            },
        }),
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{} {} ({})",
            if result.reused_existing {
                "Linked existing"
            } else {
                "Created candidate"
            },
            result.memory.id,
            result.memory.claim
        );
    }
    Ok(true)
}

fn run_synthesize(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    json: bool,
    promote: Option<&str>,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let client = client(&config);
    if let Some(raw_id) = promote {
        let memory_id = MemoryId::parse(raw_id)?;
        let skill = block_on(client.promote_skill(&SkillPromoteRequest {
            path: path.to_path_buf(),
            memory_id,
        }))?;
        if json {
            println!("{}", serde_json::to_string_pretty(&skill)?);
        } else {
            println!("Promoted {} to kind `{}`", skill.id, skill.kind);
        }
        return Ok(true);
    }
    let report = block_on(client.run_synthesis(&SynthesisRunRequest {
        path: path.to_path_buf(),
    }))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Examined {} successful evidence-backed episodes in {}",
            report.examined_episodes, report.project_id
        );
        if report.procedures.is_empty() {
            println!("No procedure met the synthesis threshold.");
        }
        for procedure in &report.procedures {
            println!(
                "{} {} [{} episodes{}] {}",
                if procedure.reused_existing {
                    "Extended"
                } else if procedure.cross_project_hypothesis {
                    "Hypothesis"
                } else {
                    "Created"
                },
                procedure.memory.id,
                procedure.supporting_episodes.len(),
                if procedure.cross_project_hypothesis {
                    ", cross-project"
                } else {
                    ""
                },
                procedure.memory.claim
            );
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_lines)]
fn run_task(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &TaskAction,
) -> Result<bool> {
    let project_root = match action {
        TaskAction::Start { path, .. } | TaskAction::Evidence { path, .. } => {
            Some(discover_project_root(path)?)
        }
        TaskAction::Attempt { .. } | TaskAction::Finish { .. } => None,
    };
    let config = load_daemon_config(overrides, socket, project_root.as_deref())?;
    let client = client(&config);
    let value = match action {
        TaskAction::Start {
            prompt,
            path,
            session,
            ..
        } => {
            let project = block_on(client.resolve_project(path))?;
            serde_json::to_value(block_on(client.start_task(&TaskStartRequest {
                project_id: project.resolved.project.id,
                prompt: prompt.clone(),
                session_id: session.clone(),
            }))?)?
        }
        TaskAction::Attempt {
            task_id,
            strategy,
            outcome,
            file,
            ..
        } => serde_json::to_value(block_on(client.record_attempt(
            &TaskId::parse(task_id)?,
            &AttemptCreateRequest {
                strategy_summary: strategy.clone(),
                outcome: *outcome,
                files: file.clone(),
            },
        ))?)?,
        TaskAction::Evidence {
            kind,
            path,
            task_id,
            attempt_id,
            strength,
            command,
            exit_code,
            artifact,
            metadata,
            ..
        } => {
            let project = block_on(client.resolve_project(path))?;
            let artifact_text = if let Some(path) = artifact {
                let artifact_metadata = std::fs::metadata(path)
                    .map_err(|source| leb::error::io_error_for_cli(path, source))?;
                if artifact_metadata.len() > u64::from(config.daemon.max_request_bytes / 2) {
                    return Err(leb::Error::Config(
                        "evidence artifact exceeds half the daemon request limit".to_owned(),
                    ));
                }
                std::fs::read_to_string(path)
                    .map_err(|source| leb::error::io_error_for_cli(path, source))?
            } else {
                String::new()
            };
            serde_json::to_value(block_on(client.record_evidence(&EvidenceCreateRequest {
                project_id: project.resolved.project.id,
                task_id: task_id.as_deref().map(TaskId::parse).transpose()?,
                attempt_id: attempt_id.as_deref().map(AttemptId::parse).transpose()?,
                kind: kind.clone(),
                strength: *strength,
                commit_hash: project.resolved.git.and_then(|git| git.head),
                command: command.clone(),
                exit_code: *exit_code,
                artifact_text,
                metadata: serde_json::from_str(metadata)?,
            }))?)?
        }
        TaskAction::Finish {
            task_id,
            outcome,
            accepted_by_user,
            final_commit,
            ..
        } => serde_json::to_value(block_on(client.finish_task(
            &TaskId::parse(task_id)?,
            &TaskFinishRequest {
                outcome: *outcome,
                accepted_by_user: *accepted_by_user,
                final_commit: final_commit.clone(),
            },
        ))?)?,
    };
    let json = match action {
        TaskAction::Start { json, .. }
        | TaskAction::Attempt { json, .. }
        | TaskAction::Evidence { json, .. }
        | TaskAction::Finish { json, .. } => *json,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("{}", serde_json::to_string(&value)?);
    }
    Ok(true)
}

fn run_eval(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &EvalAction,
) -> Result<bool> {
    let config = load_daemon_config(overrides, socket, None)?;
    let client = client(&config);
    match action {
        EvalAction::Run {
            config: benchmark,
            output,
            json,
        } => {
            let artifacts = EvaluationRunner::run(benchmark, output.as_deref())?;
            for record in &artifacts.records {
                block_on(client.record_evaluation(record))?;
            }
            if *json {
                println!("{}", serde_json::to_string_pretty(&artifacts.comparison)?);
            } else {
                println!(
                    "Paired runs: {}\nSuccess: off {:.1}% / brain {:.1}% ({})\nReport: {}",
                    artifacts.comparison.paired_runs,
                    artifacts.comparison.off.success_rate * 100.0,
                    artifacts.comparison.brain.success_rate * 100.0,
                    artifacts.comparison.success_rate_delta.map_or_else(
                        || "single-arm".to_owned(),
                        |delta| format!("{:+.1} pp", delta * 100.0)
                    ),
                    artifacts.comparison_markdown_path.display()
                );
            }
        }
    }
    Ok(true)
}

fn load_daemon_config(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    project_root: Option<&Path>,
) -> Result<Config> {
    let mut config = Config::load_for(project_root, overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    Ok(config)
}

fn run_index(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    changed: &[PathBuf],
    graphify: Option<&Path>,
    json: bool,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let changed = (!changed.is_empty()).then(|| changed.to_vec());
    let response = block_on(client(&config).index_project_with_provider(
        path,
        changed,
        graphify.map(|_| "graphify".to_owned()),
        graphify.map(Path::to_path_buf),
    ))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("Project:   {}", response.resolved.project.id);
        println!("Provider:  {}", response.index.provider);
        println!("Files:     {} discovered", response.index.discovered_files);
        println!("Parsed:    {}", response.index.parsed_files);
        println!("Unchanged: {}", response.index.unchanged_files);
        println!("Removed:   {}", response.index.removed_files);
        println!(
            "Graph:     {} nodes, {} edges",
            response.index.nodes_written, response.index.edges_written
        );
        println!(
            "Invalid:   {} drifted memories",
            response.invalidated_memories
        );
        if let Some(git) = response.git {
            println!(
                "Git:       {} commits{}",
                git.commits_upserted,
                if git.truncated { " (bounded)" } else { "" }
            );
        }
    }
    Ok(true)
}

fn run_graph(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    query: &str,
    limit: usize,
    json: bool,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let client = client(&config);
    let status = block_on(client.resolve_project(path))?;
    let nodes = block_on(client.search_world(&WorldSearchRequest {
        project_id: status.resolved.project.id,
        query: query.to_owned(),
        limit,
    }))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
    } else {
        for node in &nodes {
            println!(
                "{}\t{}\t{}\t{}",
                node.id,
                node.kind,
                node.label,
                node.file_path.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn run_context(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    task: &str,
    budget: Option<u32>,
    off: bool,
    json: bool,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let client = client(&config);
    let status = block_on(client.resolve_project(path))?;
    let context = block_on(client.compile_context(&ContextCompileRequest {
        request_id: format!("cli_{}", uuid::Uuid::new_v4()),
        project_id: status.resolved.project.id,
        task_text: task.to_owned(),
        current_paths: Vec::new(),
        token_budget: budget.unwrap_or(config.context.max_injected_tokens),
        mode: if off {
            ContextMode::Off
        } else {
            ContextMode::Brain
        },
        repository_root: Some(status.resolved.root),
        head: status.resolved.git.and_then(|git| git.head),
        task_id: None,
        framing: None,
    }))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&context)?);
    } else {
        print!("{}{}", context.stable_prefix, context.dynamic_context);
        println!(
            "\n[LEB: {}/{} tokens, {} items, {} ms]",
            context.metrics.selected_tokens,
            context.metrics.token_budget,
            context.items.len(),
            context.metrics.compile_ms
        );
    }
    Ok(true)
}

fn run_semantic(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &SemanticAction,
) -> Result<bool> {
    match action {
        SemanticAction::Rebuild { path, json } => {
            let root = discover_project_root(path)?;
            let config = load_daemon_config(overrides, socket, Some(&root))?;
            let client = client(&config);
            let status = block_on(client.resolve_project(path))?;
            let response = block_on(client.rebuild_semantic_index(&SemanticRebuildRequest {
                project_id: status.resolved.project.id,
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                println!(
                    "Rebuilt {} semantic vectors for {}",
                    response.vectors, response.project_id
                );
            }
        }
    }
    Ok(true)
}

fn run_jobs(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &JobsAction,
) -> Result<bool> {
    let project_root = match action {
        JobsAction::Enqueue { path, .. } => Some(discover_project_root(path)?),
        JobsAction::List { .. } | JobsAction::Status { .. } => None,
    };
    let config = load_daemon_config(overrides, socket, project_root.as_deref())?;
    let client = client(&config);
    match action {
        JobsAction::List {
            status,
            limit,
            json,
        } => {
            let status = status.as_deref().map(JobStatus::parse).transpose()?;
            let jobs = block_on(client.list_jobs(&JobListRequest {
                status,
                limit: *limit,
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&jobs)?);
            } else {
                for job in &jobs {
                    print_job_line(job);
                }
            }
        }
        JobsAction::Enqueue {
            job_type,
            path,
            dedupe_key,
            json,
        } => {
            let job_type = JobType::parse(job_type)?;
            let project_id = if job_type.requires_project() {
                Some(block_on(client.resolve_project(path))?.resolved.project.id)
            } else {
                None
            };
            let job = block_on(client.enqueue_job(&JobEnqueueRequest {
                job_type,
                project_id,
                payload: None,
                dedupe_key: dedupe_key.clone(),
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                println!(
                    "Enqueued {} ({}), status {}",
                    job.id,
                    job.job_type.as_str(),
                    job.status.as_str()
                );
            }
        }
        JobsAction::Status { id, json } => {
            let id = JobId::parse(id)?;
            let job = block_on(client.get_job(&id))?
                .ok_or_else(|| leb::Error::Config(format!("unknown job `{id}`")))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&job)?);
            } else {
                print_job_line(&job);
                if let Some(error) = &job.last_error {
                    println!("Error:  {error}");
                }
                if let Some(result) = &job.result {
                    println!("Result: {result}");
                }
            }
        }
    }
    Ok(true)
}

fn print_job_line(job: &JobRecord) {
    println!(
        "{}\t{}\t{}\tattempts {}/{}\t{}",
        job.id,
        job.job_type.as_str(),
        job.status.as_str(),
        job.attempts,
        job.max_attempts,
        job.project_id
            .as_ref()
            .map_or_else(|| "-".to_owned(), std::string::ToString::to_string)
    );
}

fn run_memory(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &MemoryAction,
) -> Result<bool> {
    let project_root = match action {
        MemoryAction::Add { path, .. } | MemoryAction::List { path, .. } => {
            Some(discover_project_root(path)?)
        }
        _ => None,
    };
    let config = load_daemon_config(overrides, socket, project_root.as_deref())?;
    let client = client(&config);
    match action {
        MemoryAction::List {
            path,
            kind,
            limit,
            json,
        } => {
            let status = block_on(client.resolve_project(path))?;
            let memories = block_on(client.list_memories(&MemoryListRequest {
                project_id: status.resolved.project.id,
                kind: kind.clone(),
                limit: *limit,
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&memories)?);
            } else {
                for memory in memories {
                    println!(
                        "{}\t{:?}\t{:?}\t{}",
                        memory.id, memory.epistemic_state, memory.status, memory.claim
                    );
                }
            }
        }
        MemoryAction::Add { .. } => run_memory_add(&client, action)?,
        MemoryAction::Show { id, json } => {
            let id = MemoryId::parse(id)?;
            let memory = block_on(client.get_memory(&id))?
                .ok_or_else(|| leb::Error::Config(format!("unknown memory `{id}`")))?;
            print_memory(&memory, *json)?;
        }
        MemoryAction::Promote {
            id,
            to,
            human_override,
            json,
        } => {
            let id = MemoryId::parse(id)?;
            let memory = block_on(client.promote_memory(
                &id,
                &MemoryPromoteRequest {
                    target: *to,
                    human_override: *human_override,
                },
            ))?;
            print_memory(&memory, *json)?;
        }
        MemoryAction::Forget { id, json } => {
            let id = MemoryId::parse(id)?;
            let memory = block_on(client.forget_memory(&id))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&memory)?);
            } else {
                println!("Forgot {id}");
            }
        }
        MemoryAction::Purge { id, json } => {
            let id = MemoryId::parse(id)?;
            block_on(client.purge_memory(&id))?;
            if *json {
                println!(
                    "{}",
                    serde_json::json!({
                        "purged": true,
                        "memory_id": id,
                        "backup_limitation": "existing backup copies are not modified by canonical purge"
                    })
                );
            } else {
                println!("Purged {id}");
                println!("Note: existing backup copies are not modified by canonical purge.");
            }
        }
    }
    Ok(true)
}

fn run_memory_add(client: &DaemonClient, action: &MemoryAction) -> Result<()> {
    let MemoryAction::Add {
        claim,
        path,
        rationale,
        tag,
        confidence,
        importance,
        json,
    } = action
    else {
        return Err(leb::Error::Config(
            "internal memory command dispatch mismatch".to_owned(),
        ));
    };
    let status = block_on(client.resolve_project(path))?;
    let project_id = status.resolved.project.id;
    let memory = block_on(
        client.create_memory(&MemoryCreateRequest {
            project_id: project_id.clone(),
            memory: MemoryDraft {
                scope_type: "project".to_owned(),
                scope_id: project_id.to_string(),
                kind: "lesson".to_owned(),
                claim: claim.clone(),
                rationale: rationale.clone(),
                confidence: *confidence,
                importance: *importance,
                tags: tag.clone(),
                valid_from_commit: status
                    .resolved
                    .git
                    .as_ref()
                    .and_then(|git| git.head.clone()),
                valid_to_commit: None,
                trust_source: "human_instruction".to_owned(),
                evidence_ids: Vec::new(),
                source_task_ids: Vec::new(),
                node_ids: Vec::new(),
            },
        }),
    )?;
    print_memory(&memory, *json)
}

fn print_memory(memory: &leb::memory::MemoryRecord, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(memory)?);
    } else {
        println!("Memory:    {}", memory.id);
        println!(
            "State:     {:?} / {:?}",
            memory.epistemic_state, memory.status
        );
        println!("Claim:     {}", memory.claim);
        println!("Rationale: {}", memory.rationale);
    }
    Ok(())
}

fn run_doctor(overrides: &ConfigOverrides, socket: Option<&Path>, json: bool) -> Result<bool> {
    let mut config = Config::load_for(None, overrides)?;
    apply_socket_override(&mut config, socket)?;
    let report = if config.daemon.socket_path.exists() {
        require_daemon(&config)?;
        block_on(client(&config).doctor())?
    } else {
        DoctorReport::run(&config)
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Local Engineering Brain diagnostics");
        for check in &report.checks {
            println!(
                "  {:<8} {:<24} {}",
                status_label(check.status),
                check.name,
                check.detail
            );
        }
        println!(
            "\nResult: {}",
            if report.ok {
                "healthy"
            } else {
                "problems found"
            }
        );
    }
    Ok(report.ok)
}

fn run_status(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    json: bool,
) -> Result<bool> {
    let config_root = discover_project_root(path)?;
    let mut config = Config::load_for(Some(&config_root), overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let output = block_on(client(&config).resolve_project(path))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_status(&output);
    }
    Ok(true)
}

fn run_link(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &LinkAction,
) -> Result<bool> {
    let source_path = match action {
        LinkAction::Add { path, .. }
        | LinkAction::Remove { path, .. }
        | LinkAction::List { path, .. } => path,
    };
    let config_root = discover_project_root(source_path)?;
    let mut config = Config::load_for(Some(&config_root), overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let client = client(&config);
    let source = block_on(client.resolve_project(source_path))?
        .resolved
        .project
        .id;
    match action {
        LinkAction::Add {
            target,
            relation,
            note,
            json,
            ..
        } => {
            let target_project = block_on(client.resolve_project(target))?.resolved.project;
            let link = block_on(client.link_project(&leb::daemon::ProjectLinkRequest {
                project_id: source,
                linked_project_id: target_project.id,
                relation: relation.clone(),
                note: note.clone(),
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&link)?);
            } else {
                println!(
                    "Linked `{}` ({}) into this project's context.",
                    link.name, link.relation
                );
            }
        }
        LinkAction::Remove { target, json, .. } => {
            let target_project = block_on(client.resolve_project(target))?.resolved.project;
            let removed = block_on(client.unlink_project(&leb::daemon::ProjectUnlinkRequest {
                project_id: source,
                linked_project_id: target_project.id,
            }))?
            .removed;
            if *json {
                println!("{}", serde_json::json!({ "removed": removed }));
            } else if removed {
                println!("Unlinked `{}`.", target_project.name);
            } else {
                println!("No link to `{}` existed.", target_project.name);
            }
        }
        LinkAction::List { json, .. } => {
            let links = block_on(
                client.project_links(&leb::daemon::ProjectLinksRequest { project_id: source }),
            )?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&links)?);
            } else if links.is_empty() {
                println!("No linked projects.");
            } else {
                for link in links {
                    let note = link
                        .note
                        .map(|note| format!(" - {note}"))
                        .unwrap_or_default();
                    println!(
                        "{}  ({}){}  [{}]",
                        link.name, link.relation, note, link.project_id
                    );
                }
            }
        }
    }
    Ok(true)
}

fn run_daemon_action(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &DaemonAction,
) -> Result<bool> {
    let mut config = Config::load_for(None, overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let client = client(&config);
    match action {
        DaemonAction::Status { json } => {
            let health = block_on(client.health())?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&health)?);
            } else {
                println!("Daemon:    {}", health.status);
                println!("Version:   {}", health.version);
                println!("Build:     {}", health.build_id);
                println!("Database:  {}", health.database.display());
                println!("Uptime:    {} ms", health.uptime_ms);
                println!(
                    "Readers:   {}/{} in use",
                    health.read_pool.in_use, health.read_pool.capacity
                );
                println!(
                    "Writes:    {}/{} queued",
                    health.scheduler.queued, health.scheduler.queue_capacity
                );
                if let Some(rss_bytes) = health.rss_bytes {
                    let mebibyte = 1_024_u64 * 1_024;
                    let whole = rss_bytes / mebibyte;
                    let tenths = (rss_bytes % mebibyte) * 10 / mebibyte;
                    println!(
                        "RSS:       {whole}.{tenths} MiB ({:?})",
                        health.memory_pressure
                    );
                }
            }
            Ok(true)
        }
        DaemonAction::Stop { json } => {
            block_on(client.shutdown())?;
            if *json {
                println!("{}", serde_json::json!({"shutdown_requested": true}));
            } else {
                println!("Daemon shutdown requested");
            }
            Ok(true)
        }
    }
}

fn run_metrics(overrides: &ConfigOverrides, socket: Option<&Path>, json: bool) -> Result<bool> {
    let config = load_daemon_config(overrides, socket, None)?;
    let response = block_on(client(&config).metrics())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
        return Ok(true);
    }
    println!("Daemon metrics (process-lifetime; reset on daemon restart)");
    println!("Uptime:    {:.1} s", ms_to_seconds(response.uptime_ms));
    let counters = &response.metrics.counters;
    println!("\nCounters");
    for (name, value) in [
        ("context_compiles_brain", counters.context_compiles_brain),
        ("context_compiles_off", counters.context_compiles_off),
        ("injected_items", counters.injected_items),
        ("injected_tokens", counters.injected_tokens),
        (
            "candidates_considered",
            counters.retrieval_candidates_considered,
        ),
        (
            "candidates_selected",
            counters.retrieval_candidates_selected,
        ),
        ("memory_promotions", counters.memory_promotions),
        ("memory_rejections", counters.memory_rejections),
        ("memory_purges", counters.memory_purges),
        ("memories_invalidated", counters.memories_invalidated),
        ("stale_injections", counters.stale_injections),
        ("evidence_records", counters.evidence_records),
    ] {
        println!("  {name:<24} {value}");
    }
    print_latency_table(&response.metrics);
    if let Some(cache) = &response.cache {
        println!("\nCache");
        println!("  hits/misses            {}/{}", cache.hits, cache.misses);
        println!(
            "  search entries/bytes   {}/{}",
            cache.search.entries, cache.search.weighted_bytes
        );
        println!(
            "  neighbor entries/bytes {}/{}",
            cache.neighbors.entries, cache.neighbors.weighted_bytes
        );
    }
    Ok(true)
}

fn print_latency_table(metrics: &leb::metrics::MetricsSnapshot) {
    println!("\nLatency (ms)               count      p50      p90      p95      p99");
    print_latency_row("context_compile", &metrics.context_compile_ms);
    print_latency_row("retrieval", &metrics.retrieval_ms);
    print_latency_row("index_scan", &metrics.index_scan_ms);
    for (group, histogram) in &metrics.requests_ms {
        if histogram.count > 0 {
            print_latency_row(&format!("request:{group}"), histogram);
        }
    }
}

fn print_latency_row(name: &str, histogram: &leb::metrics::HistogramSnapshot) {
    println!(
        "  {name:<24} {:>7} {:>8.1} {:>8.1} {:>8.1} {:>8.1}",
        histogram.count, histogram.p50_ms, histogram.p90_ms, histogram.p95_ms, histogram.p99_ms
    );
}

#[allow(clippy::cast_precision_loss)]
fn ms_to_seconds(ms: u64) -> f64 {
    ms as f64 / 1_000.0
}

fn run_dashboard(overrides: &ConfigOverrides, socket: Option<&Path>) -> Result<bool> {
    let config = load_daemon_config(overrides, socket, None)?;
    let client = client(&config);
    let health = block_on(client.health())?;
    let response = block_on(client.metrics())?;
    let memory = block_on(client.memory_stats())?;

    println!("Local Engineering Brain dashboard (single snapshot)");
    println!("\n== Daemon ==");
    println!(
        "Status:    {} (v{}, {})",
        health.status, health.version, health.build_id
    );
    println!("Database:  {}", health.database.display());
    println!("Uptime:    {:.1} s", ms_to_seconds(health.uptime_ms));
    println!(
        "Network:   {}",
        if health.local_only {
            "local-only"
        } else {
            "enabled"
        }
    );
    if let Some(rss_bytes) = health.rss_bytes {
        let mebibyte = 1_024_u64 * 1_024;
        println!(
            "RSS:       {}.{} MiB ({:?} pressure)",
            rss_bytes / mebibyte,
            (rss_bytes % mebibyte) * 10 / mebibyte,
            health.memory_pressure
        );
    }
    println!(
        "Scheduler: {}/{} queued, {} completed, {} failed, {} rejected",
        health.scheduler.queued,
        health.scheduler.queue_capacity,
        health.scheduler.completed,
        health.scheduler.failed,
        health.scheduler.rejected
    );
    println!(
        "Readers:   {}/{} in use",
        health.read_pool.in_use, health.read_pool.capacity
    );

    println!("\n== Cache ==");
    if let Some(cache) = &health.cache {
        println!("Hits:      {}", cache.hits);
        println!("Misses:    {}", cache.misses);
        println!(
            "Entries:   {} search, {} neighbors",
            cache.search.entries, cache.neighbors.entries
        );
        println!(
            "Bytes:     {} / {} budget",
            cache.search.weighted_bytes + cache.neighbors.weighted_bytes,
            cache.search.total_budget_bytes + cache.neighbors.total_budget_bytes
        );
    } else {
        println!("Disabled");
    }

    println!("\n== Latency ==");
    print_latency_table(&response.metrics);

    let counters = &response.metrics.counters;
    println!("\n== Context compiles ==");
    println!(
        "Compiles:  {} brain / {} off",
        counters.context_compiles_brain, counters.context_compiles_off
    );
    println!(
        "Injected:  {} items, {} tokens",
        counters.injected_items, counters.injected_tokens
    );
    println!(
        "Candidates: {} considered, {} selected",
        counters.retrieval_candidates_considered, counters.retrieval_candidates_selected
    );
    println!(
        "Stale:     {} invalidated ({} previously injected)",
        counters.memories_invalidated, counters.stale_injections
    );

    println!("\n== Memory ==");
    println!("Total:     {}", memory.total);
    for (status, count) in &memory.by_status {
        println!("  status {status:<14} {count}");
    }
    for (epistemic_state, count) in &memory.by_epistemic_state {
        println!("  state  {epistemic_state:<14} {count}");
    }

    println!("\n== Learning ==");
    println!(
        "Promotions: {} / rejections: {} / purges: {}",
        counters.memory_promotions, counters.memory_rejections, counters.memory_purges
    );
    println!("Evidence:   {} records", counters.evidence_records);
    Ok(true)
}

fn client(config: &Config) -> DaemonClient {
    DaemonClient::new(
        &config.daemon.socket_path,
        Duration::from_millis(config.daemon.request_timeout_ms),
    )
}

fn discover_project_root(path: &Path) -> Result<PathBuf> {
    let existing = path
        .canonicalize()
        .map_err(|source| leb::error::io_error_for_cli(path, source))?;
    let directory = if existing.is_file() {
        existing
            .parent()
            .map_or_else(|| existing.clone(), Path::to_path_buf)
    } else {
        existing
    };
    Ok(GitState::discover(&directory)?.map_or(directory, |git| git.repository_root))
}

fn print_status(status: &ProjectStatus) {
    println!(
        "Project:   {} ({})",
        status.resolved.project.name, status.resolved.project.id
    );
    println!("Root:      {}", status.resolved.root.display());
    if let Some(component) = &status.resolved.component {
        println!("Component: {} ({})", component.name, component.id);
    }
    if let Some(git) = &status.resolved.git {
        println!("HEAD:      {}", git.head.as_deref().unwrap_or("unborn"));
        println!("Branch:    {}", git.branch.as_deref().unwrap_or("detached"));
        println!("Dirty:     {}", git.dirty);
        println!("Diff:      {}", git.diff_fingerprint);
    } else {
        println!("Git:       not a repository");
    }
    println!("Database:  {}", status.database.display());
    println!("Integrity: {}", status.database_integrity);
    println!("Events:    {}", status.project_event_count);
    println!(
        "Network:   {}",
        if status.local_only {
            "local-only"
        } else {
            "enabled"
        }
    );
}

fn apply_socket_override(config: &mut Config, socket: Option<&Path>) -> Result<()> {
    if let Some(socket) = socket {
        config.daemon.socket_path = socket.to_path_buf();
    }
    config.validate()
}

fn require_daemon(config: &Config) -> Result<()> {
    block_on(leb::autostart::ensure_daemon(config))
}

fn block_on<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| leb::Error::Daemon(format!("failed to start client runtime: {error}")))?
        .block_on(future)
}

const fn status_label(status: leb::doctor::CheckStatus) -> &'static str {
    match status {
        leb::doctor::CheckStatus::Pass => "PASS",
        leb::doctor::CheckStatus::Warning => "WARN",
        leb::doctor::CheckStatus::Fail => "FAIL",
    }
}
