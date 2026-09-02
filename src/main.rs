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
        /// Index fetched `origin/<default>` (shared truth) in a throwaway
        /// worktree instead of this checkout. Documentation surfaces default
        /// to this; agent context always uses the local tree.
        #[arg(long)]
        origin: bool,
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

    /// Synthesize a CLAUDE.md-style workspace architecture guide with the
    /// configured model (derived output; never evidence-gated memory).
    Architect {
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Model name for the configured runtime (overrides `architect.model`).
        #[arg(long)]
        model: Option<String>,
        /// Print the deterministic dossier and prompt without calling a model.
        #[arg(long)]
        dry_run: bool,
        /// Synthesize with the local `claude` CLI (your Claude subscription)
        /// instead of the configured HTTP runtime. The dossier is sent
        /// through your authenticated Claude session.
        #[arg(long)]
        claude: bool,
        /// Write here instead of `<vault root>/ARCHITECTURE.md`.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },

    /// Curated per-project architecture facts (hosting, storage, role).
    Fact {
        #[command(subcommand)]
        action: FactAction,
    },

    /// Hook runtime for stock coding agents (invoked by Claude Code hooks;
    /// reads the hook JSON on stdin and always exits 0 - fail open).
    #[command(hide = true)]
    Hook {
        /// `prompt` (`UserPromptSubmit`) or `bash` (`PostToolUse` for Bash).
        event: String,
    },

    /// Install or inspect the stock-agent integration (hooks + MCP).
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },

    /// Print the CLAUDE.md snippet that teaches a coding agent the LEB
    /// episode loop (recall, record, verify, learn) via the MCP tools.
    AgentMd {
        /// Append the snippet to this repo's agent file (idempotent: skipped
        /// when the section is already present). Repeatable.
        #[arg(long, value_name = "REPO")]
        install: Vec<PathBuf>,
        /// Agent file name to install into (CLAUDE.md for Claude Code,
        /// AGENTS.md for Codex).
        #[arg(long, default_value = "CLAUDE.md")]
        file: String,
    },

    /// Sweep a folder of repositories into the brain: register, index,
    /// auto-link, and export the vault for every git repo found.
    Onboard {
        /// Folder containing repositories (each with a .git).
        path: PathBuf,
        /// Also synthesize the architecture guide via the claude CLI.
        #[arg(long)]
        claude: bool,
        #[arg(long)]
        json: bool,
    },

    /// Synchronize knowledge with the configured central instance
    /// (`[sync] central_url`): pull validated knowledge down, offer local
    /// validated memories up as candidates.
    Sync {
        /// Central instance URL (overrides `[sync] central_url`).
        #[arg(long)]
        central: Option<String>,
        /// Bearer token (overrides `[sync] token`).
        #[arg(long)]
        token: Option<String>,
        /// Only pull central knowledge.
        #[arg(long)]
        pull_only: bool,
        /// Only offer local knowledge upstream.
        #[arg(long)]
        push_only: bool,
        #[arg(long)]
        json: bool,
    },

    /// Serve the local workspace browser (facts, links, architecture).
    Web {
        /// Loopback port to bind.
        #[arg(long, default_value_t = 11436)]
        port: u16,
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },

    /// Export the derived Obsidian vault (notes + mermaid diagrams).
    Vault {
        #[command(subcommand)]
        action: VaultAction,
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
    /// Discover links from the repo's own manifests (path deps, submodules).
    Scan {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Apply definitive candidates (name matches always stay proposals).
        #[arg(long)]
        apply: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum FactAction {
    /// Set or update one fact (e.g. `hosting.base_url api.example.com`).
    Set {
        key: String,
        value: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove one fact.
    Unset {
        key: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List this project's facts.
    List {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HooksAction {
    /// Merge LEB's hooks into a repo's .claude/settings.json (idempotent)
    /// and register the `leb` MCP server for it.
    Install {
        #[arg(default_value = ".")]
        repo: PathBuf,
    },
    /// Print the hooks JSON without writing anything.
    Show,
    /// Print Codex setup: AGENTS.md snippet target and MCP configuration.
    Codex,
}

#[derive(Debug, Subcommand)]
enum VaultAction {
    /// Refresh the structural index, then export vault notes and diagrams.
    Build {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Export every registered project instead of just this one.
        #[arg(long)]
        all: bool,
        /// `git pull --ff-only` first — only when the working tree is clean.
        #[arg(long)]
        pull: bool,
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
        Commands::Architect {
            path,
            model,
            dry_run,
            claude,
            out,
            json,
        } => run_architect(
            &overrides,
            socket_override.as_deref(),
            &path,
            model.as_deref(),
            dry_run,
            claude,
            out.as_deref(),
            json,
        ),
        Commands::Fact { action } => run_fact(&overrides, socket_override.as_deref(), &action),
        Commands::Hook { event } => run_hook_event(&overrides, socket_override.as_deref(), &event),
        Commands::Hooks { action } => run_hooks_action(&action),
        Commands::AgentMd { install, file } => {
            if install.is_empty() {
                println!("{AGENT_MD_SNIPPET}");
            }
            for repo in &install {
                let target = repo.join(&file);
                let existing = std::fs::read_to_string(&target).unwrap_or_default();
                if existing.contains("## Engineering memory (LEB)") {
                    println!("{}: already installed", target.display());
                    continue;
                }
                let separator = if existing.is_empty() || existing.ends_with("\n\n") {
                    ""
                } else if existing.ends_with('\n') {
                    "\n"
                } else {
                    "\n\n"
                };
                std::fs::write(&target, format!("{existing}{separator}{AGENT_MD_SNIPPET}"))
                    .map_err(|error| {
                        leb::Error::Config(format!("{}: {error}", target.display()))
                    })?;
                println!("{}: installed", target.display());
            }
            Ok(true)
        }
        Commands::Onboard { path, claude, json } => {
            run_onboard(&overrides, socket_override.as_deref(), &path, claude, json)
        }
        Commands::Sync {
            central,
            token,
            pull_only,
            push_only,
            json,
        } => run_sync(
            &overrides,
            socket_override.as_deref(),
            central.as_deref(),
            token.as_deref(),
            pull_only,
            push_only,
            json,
        ),
        Commands::Web { port, path } => {
            run_web(&overrides, socket_override.as_deref(), &path, port)
        }
        Commands::Vault { action } => run_vault(&overrides, socket_override.as_deref(), &action),
        Commands::Metrics { json } => run_metrics(&overrides, socket_override.as_deref(), json),
        Commands::Dashboard => run_dashboard(&overrides, socket_override.as_deref()),
        Commands::Index {
            path,
            changed,
            graphify,
            origin,
            json,
        } => {
            if origin {
                let config_root = discover_project_root(&path)?;
                let config = Config::load_for(Some(&config_root), &overrides)?;
                let state_dir = config
                    .storage
                    .db_path
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join("worktrees");
                let worktree = leb::gitref::checkout_origin_default(&path, &state_dir)?;
                println!("Indexing {} (shared truth) …", worktree.reference);
                let result = run_index(
                    &overrides,
                    socket_override.as_deref(),
                    worktree.path(),
                    &changed,
                    graphify.as_deref(),
                    json,
                );
                drop(worktree);
                result
            } else {
                run_index(
                    &overrides,
                    socket_override.as_deref(),
                    &path,
                    &changed,
                    graphify.as_deref(),
                    json,
                )
            }
        }
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
        if report.procedures.is_empty() && report.preferences.is_empty() {
            println!("Nothing met the synthesis thresholds.");
        }
        for preference in &report.preferences {
            println!(
                "{} preference {} [{} episodes, corrected by `{}`] {}",
                if preference.reused_existing {
                    "Extended"
                } else {
                    "Distilled"
                },
                preference.memory.id,
                preference.supporting_episodes.len(),
                preference.correction_command,
                preference.memory.claim
            );
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

#[allow(clippy::too_many_lines)]
fn run_link(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &LinkAction,
) -> Result<bool> {
    let source_path = match action {
        LinkAction::Add { path, .. }
        | LinkAction::Remove { path, .. }
        | LinkAction::List { path, .. }
        | LinkAction::Scan { path, .. } => path,
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
        LinkAction::Scan { path, apply, json } => {
            let report = block_on(client.scan_links(&leb::daemon::LinkScanRequest {
                path: path.clone(),
                apply: *apply,
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.candidates.is_empty() && report.applied.is_empty() {
                println!("No link candidates found in this repo's manifests.");
            } else {
                for link in &report.applied {
                    println!("LINKED  {}  ({})", link.name, link.relation);
                }
                for candidate in &report.candidates {
                    if candidate.definitive && *apply {
                        continue;
                    }
                    let status = if candidate.definitive {
                        "definitive"
                    } else {
                        "proposal"
                    };
                    println!(
                        "{status}  {}  ({})  — {}",
                        candidate.target_name, candidate.relation, candidate.evidence
                    );
                }
                if !*apply
                    && report
                        .candidates
                        .iter()
                        .any(|candidate| candidate.definitive)
                {
                    println!("Run `leb link scan --apply` to create the definitive links.");
                }
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

#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn run_architect(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    model: Option<&str>,
    dry_run: bool,
    claude: bool,
    out: Option<&Path>,
    json: bool,
) -> Result<bool> {
    let config_root = discover_project_root(path)?;
    let mut config = Config::load_for(Some(&config_root), overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let dossier = block_on(client(&config).architect_dossier())?;
    let prompt = leb::architect::synthesis_prompt(&dossier);
    if dry_run {
        println!("{prompt}");
        return Ok(true);
    }
    let synthesized;
    let model = if claude {
        let model = model.unwrap_or("claude-opus-4-8").to_owned();
        println!("Synthesizing with `{model}` via the claude CLI …");
        synthesized = leb::architect::synthesize_with_claude_cli(&model, &prompt)?;
        model
    } else {
        let model = model
            .map(str::to_owned)
            .or_else(|| {
                (!config.architect.model.is_empty()).then(|| config.architect.model.clone())
            })
            .ok_or_else(|| {
                leb::Error::Config(
                    "no model configured: pass --model, --claude, or set architect.model"
                        .to_owned(),
                )
            })?;
        leb::architect::enforce_local_only(
            &config.architect.runtime_url,
            config.network.local_only,
        )?;
        println!(
            "Synthesizing with `{model}` via {} …",
            config.architect.runtime_url
        );
        synthesized = block_on(leb::architect::synthesize(
            &config.architect.runtime_url,
            &model,
            &prompt,
        ))?;
        model
    };
    let destination = out.map_or_else(
        || config.vault.root.join("ARCHITECTURE.md"),
        Path::to_path_buf,
    );
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| leb::Error::Config(format!("architecture output dir: {error}")))?;
    }
    let document = leb::architect::guide_document(&model, &dossier, &synthesized);
    std::fs::write(&destination, &document)
        .map_err(|error| leb::Error::Config(format!("architecture output: {error}")))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&leb::architect::ArchitectReport {
                model,
                output_path: destination.display().to_string(),
                dossier_bytes: dossier.len(),
                synthesized_bytes: synthesized.len(),
            })?
        );
    } else {
        println!(
            "Wrote {} ({} bytes).",
            destination.display(),
            document.len()
        );
    }
    Ok(true)
}

fn run_fact(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &FactAction,
) -> Result<bool> {
    let source_path = match action {
        FactAction::Set { path, .. }
        | FactAction::Unset { path, .. }
        | FactAction::List { path, .. } => path,
    };
    let config_root = discover_project_root(source_path)?;
    let mut config = Config::load_for(Some(&config_root), overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let client = client(&config);
    let project_id = block_on(client.resolve_project(source_path))?
        .resolved
        .project
        .id;
    match action {
        FactAction::Set {
            key,
            value,
            note,
            json,
            ..
        } => {
            let fact = block_on(client.set_fact(&leb::daemon::FactSetRequest {
                project_id,
                key: key.clone(),
                value: value.clone(),
                note: note.clone(),
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&fact)?);
            } else {
                println!("Set {} = {}", fact.key, fact.value);
            }
        }
        FactAction::Unset { key, json, .. } => {
            let removed = block_on(client.unset_fact(&leb::daemon::FactUnsetRequest {
                project_id,
                key: key.clone(),
            }))?
            .removed;
            if *json {
                println!("{}", serde_json::json!({ "removed": removed }));
            } else if removed {
                println!("Removed `{key}`.");
            } else {
                println!("No fact `{key}` existed.");
            }
        }
        FactAction::List { json, .. } => {
            let facts = block_on(client.list_facts(&leb::daemon::FactListRequest { project_id }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&facts)?);
            } else if facts.is_empty() {
                println!("No facts recorded.");
            } else {
                for fact in facts {
                    let note = fact
                        .note
                        .map(|note| format!("  — {note}"))
                        .unwrap_or_default();
                    println!("{} = {}{note}", fact.key, fact.value);
                }
            }
        }
    }
    Ok(true)
}

const AGENT_MD_SNIPPET: &str = r"## Engineering memory (LEB)

This repository is tracked by LEB, a local evidence-gated memory system. The
`leb` MCP server is available; use it so finished work compounds instead of
evaporating.

At task start:
- Call `leb_project_status` once to resolve the project id, then `leb_recall`
  with the task text (pass `task_id` when you have one) - it returns validated
  team knowledge with a why for every item. Treat `- MUST:` review standards
  as hard requirements.
- For cross-repo work, `leb_workspace` shows who calls whom, hosting, and
  each repo's technology stack; `leb_symbol_search` finds where a symbol is
  defined across every repo.

While working (substantive tasks, not trivial edits):
1. `leb_task_start` with the user's request.
2. After each meaningful approach, `leb_record_attempt` (summary phrased as
   the reusable takeaway, outcome success/failure).
3. When tests/builds/lints run, `leb_record_evidence` with the command and
   exit code (kind `test_result`/`lint_result`..., strength `deterministic`).
   Record FAILING runs too - fail-then-pass pairs teach conventions.
4. On completion, `leb_mark_outcome` (outcome + whether the user accepted).

After accepted work that surfaced a durable rule:
- `leb_learn` with a claim phrased for a future engineer, linking the task
  and its passing evidence. Candidates never inject until promoted, so err on
  the side of recording.
- If a failed approach was instructive, it is already captured by the failed
  attempt; `leb_get_past_failures` replays those to future sessions.
";

/// Hook runtime. EVERY failure path prints nothing and returns Ok: a broken
/// or absent brain must never break the user's coding session.
#[allow(clippy::too_many_lines, clippy::unnecessary_wraps)]
fn run_hook_event(overrides: &ConfigOverrides, socket: Option<&Path>, event: &str) -> Result<bool> {
    use std::io::Read as _;
    let mut stdin = String::new();
    if std::io::stdin().read_to_string(&mut stdin).is_err() {
        return Ok(true);
    }
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(&stdin) else {
        return Ok(true);
    };
    let session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown-session")
        .to_owned();
    let cwd = payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let Ok(mut config) = Config::load_for(discover_project_root(&cwd).ok().as_deref(), overrides)
    else {
        return Ok(true);
    };
    if apply_socket_override(&mut config, socket).is_err() || require_daemon(&config).is_err() {
        return Ok(true);
    }
    let state_dir = config
        .storage
        .db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let client = client(&config);
    match event {
        "prompt" => {
            if leb::agenthooks::load_session(&state_dir, &session_id).is_some() {
                return Ok(true); // session already primed (pinned semantics)
            }
            let prompt = payload
                .get("user_prompt")
                .or_else(|| payload.get("prompt"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_owned();
            if prompt.is_empty() {
                return Ok(true);
            }
            let Ok(status) = block_on(client.resolve_project(&cwd)) else {
                return Ok(true);
            };
            let project_id = status.resolved.project.id.clone();
            let task = block_on(client.start_task(&TaskStartRequest {
                project_id: project_id.clone(),
                prompt: prompt.clone(),
                session_id: Some(session_id.clone()),
            }))
            .ok();
            let Ok(context) = block_on(client.compile_context(&ContextCompileRequest {
                request_id: format!("hook_{session_id}"),
                project_id: project_id.clone(),
                task_text: prompt,
                current_paths: Vec::new(),
                token_budget: config.context.max_injected_tokens,
                mode: ContextMode::Brain,
                repository_root: Some(status.resolved.root),
                head: status.resolved.git.and_then(|git| git.head),
                task_id: task.as_ref().map(|task| task.id.clone()),
                framing: None,
            })) else {
                return Ok(true);
            };
            if let Some(task) = task {
                let _ = leb::agenthooks::save_session(
                    &state_dir,
                    &session_id,
                    &leb::agenthooks::HookSession {
                        task_id: task.id,
                        project_id,
                        last_verification_exit: None,
                    },
                );
            }
            let injected = format!("{}{}", context.stable_prefix, context.dynamic_context);
            if !injected.trim().is_empty() {
                println!("{injected}");
            }
        }
        "session-end" => {
            let Some(session) = leb::agenthooks::load_session(&state_dir, &session_id) else {
                return Ok(true);
            };
            // Conservative auto-close: success only when the last recorded
            // verification passed; acceptance stays unknown, so this counts
            // toward memory ROI but never feeds distillation. If the agent
            // already closed the task via leb_mark_outcome this finish fails
            // harmlessly (fail-open).
            let outcome = match leb::agenthooks::auto_close_outcome(session.last_verification_exit)
            {
                "success" => leb::experience::TaskOutcome::Success,
                _ => leb::experience::TaskOutcome::Abandoned,
            };
            let _ = block_on(client.finish_task(
                &session.task_id,
                &leb::daemon::TaskFinishRequest {
                    outcome,
                    accepted_by_user: None,
                    final_commit: None,
                },
            ));
            let _ =
                std::fs::remove_file(leb::agenthooks::session_state_path(&state_dir, &session_id));
        }
        "bash" => {
            let command = payload
                .get("tool_input")
                .and_then(|input| input.get("command"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let Some(kind) = leb::agenthooks::detect_verification(command) else {
                return Ok(true);
            };
            let Some(session) = leb::agenthooks::load_session(&state_dir, &session_id) else {
                return Ok(true);
            };
            let empty = serde_json::Value::Null;
            let tool_result = payload
                .get("tool_result")
                .or_else(|| payload.get("tool_response"))
                .unwrap_or(&empty);
            let Some(exit_code) = leb::agenthooks::extract_exit_code(tool_result) else {
                return Ok(true);
            };
            let _ = block_on(client.record_evidence(&EvidenceCreateRequest {
                project_id: session.project_id.clone(),
                task_id: Some(session.task_id.clone()),
                attempt_id: None,
                kind: kind.to_owned(),
                strength: EvidenceStrength::Deterministic,
                commit_hash: None,
                command: Some(command.chars().take(500).collect()),
                exit_code: Some(exit_code),
                artifact_text: leb::agenthooks::extract_artifact(tool_result),
                metadata: serde_json::json!({"source": "claude-code-hook"}),
            }));
            let _ = leb::agenthooks::save_session(
                &state_dir,
                &session_id,
                &leb::agenthooks::HookSession {
                    last_verification_exit: Some(exit_code),
                    ..session
                },
            );
        }
        _ => {}
    }
    Ok(true)
}

fn run_hooks_action(action: &HooksAction) -> Result<bool> {
    let hooks = leb::agenthooks::claude_hooks_json("leb");
    match action {
        HooksAction::Show => {
            println!("{}", serde_json::to_string_pretty(&hooks)?);
        }
        HooksAction::Install { repo } => {
            let settings_dir = repo.join(".claude");
            std::fs::create_dir_all(&settings_dir)
                .map_err(|error| leb::Error::Config(format!("settings dir: {error}")))?;
            let settings_path = settings_dir.join("settings.json");
            let mut settings: serde_json::Value = std::fs::read_to_string(&settings_path)
                .ok()
                .and_then(|content| serde_json::from_str(&content).ok())
                .unwrap_or_else(|| serde_json::json!({}));
            let existing = settings
                .get("hooks")
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            if existing.contains("leb hook") {
                println!("{}: hooks already installed", settings_path.display());
            } else {
                let hooks_slot = settings
                    .as_object_mut()
                    .ok_or_else(|| {
                        leb::Error::Config("settings.json is not a JSON object".to_owned())
                    })?
                    .entry("hooks")
                    .or_insert_with(|| serde_json::json!({}));
                let hooks_object = hooks_slot.as_object_mut().ok_or_else(|| {
                    leb::Error::Config("settings.json hooks is not an object".to_owned())
                })?;
                for (event, entries) in hooks.as_object().into_iter().flatten() {
                    let slot = hooks_object
                        .entry(event.clone())
                        .or_insert_with(|| serde_json::json!([]));
                    if let (Some(target), Some(additions)) =
                        (slot.as_array_mut(), entries.as_array())
                    {
                        target.extend(additions.iter().cloned());
                    }
                }
                std::fs::write(
                    &settings_path,
                    serde_json::to_string_pretty(&settings)
                        .map_err(|error| leb::Error::Config(error.to_string()))?,
                )
                .map_err(|error| leb::Error::Config(format!("settings write: {error}")))?;
                println!("{}: hooks installed", settings_path.display());
            }
            println!(
                "Context injection + automatic evidence capture are active for stock \
                 `claude` sessions in this repo. Also run `leb agent-md --install {}` \
                 so the agent knows the wider tool loop.",
                repo.display()
            );
        }
        HooksAction::Codex => {
            println!(
                "Codex setup:\n\n1. MCP server (once, in ~/.codex/config.toml):\n\n\
                 [mcp_servers.leb]\ncommand = \"leb-mcp\"\n\n\
                 2. Per repo, install the agent loop instructions into AGENTS.md:\n\n\
                 leb agent-md --install <repo> --file AGENTS.md\n\n\
                 Codex reads AGENTS.md automatically and reaches the brain through MCP\n\
                 (recall, workspace map, symbol search, episode recording)."
            );
        }
    }
    Ok(true)
}

fn run_onboard(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    claude: bool,
    json: bool,
) -> Result<bool> {
    const MAX_REPOS: usize = 50;
    let root = path
        .canonicalize()
        .map_err(|source| leb::error::io_error_for_cli(path, source))?;
    let mut repos = Vec::new();
    if root.join(".git").exists() {
        repos.push(root.clone());
    } else {
        let mut entries = std::fs::read_dir(&root)
            .map_err(|source| leb::error::io_error_for_cli(&root, source))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| candidate.is_dir() && candidate.join(".git").exists())
            .collect::<Vec<_>>();
        entries.sort();
        repos.extend(entries.into_iter().take(MAX_REPOS));
    }
    if repos.is_empty() {
        return Err(leb::Error::Config(format!(
            "no git repositories found under {}",
            root.display()
        )));
    }
    let mut config = Config::load_for(None, overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let patient = DaemonClient::new(
        &config.daemon.socket_path,
        Duration::from_millis(config.daemon.request_timeout_ms.max(600_000)),
    );
    println!("Onboarding {} repositories…", repos.len());
    for repo in &repos {
        let name = repo.file_name().map_or_else(
            || repo.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        match block_on(patient.refresh_index(repo)) {
            Ok(response) => println!(
                "  ✓ {name}: {} nodes, {} edges",
                response.index.nodes_written, response.index.edges_written
            ),
            Err(error) => println!("  ✗ {name}: {error}"),
        }
    }
    println!("Building the vault…");
    let report =
        block_on(patient.build_vault(&leb::daemon::VaultBuildRequest { project_id: None }))?;
    if claude {
        println!("Synthesizing the architecture guide via claude…");
        let dossier = block_on(patient.architect_dossier())?;
        let prompt = leb::architect::synthesis_prompt(&dossier);
        let synthesized = leb::architect::synthesize_with_claude_cli("claude-opus-4-8", &prompt)?;
        let document = leb::architect::guide_document("claude-opus-4-8", &dossier, &synthesized);
        std::fs::write(config.vault.root.join("ARCHITECTURE.md"), document)
            .map_err(|error| leb::Error::Config(format!("guide write: {error}")))?;
    }
    let snapshot = block_on(patient.workspace())?;
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        println!("\nWorkspace ({} repositories):", snapshot.projects.len());
        for project in &snapshot.projects {
            println!(
                "  {} — {} nodes · {} links · {} technologies",
                project.name,
                project.nodes,
                project.links.len(),
                project.technologies.len()
            );
        }
        println!(
            "\nVault: {}\nBrowser: http://127.0.0.1:{}",
            report.root, config.web.port
        );
    }
    Ok(true)
}

#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
fn run_sync(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    central: Option<&str>,
    token: Option<&str>,
    pull_only: bool,
    push_only: bool,
    json: bool,
) -> Result<bool> {
    let mut config = Config::load_for(None, overrides)?;
    apply_socket_override(&mut config, socket)?;
    let central_url = central.map_or_else(|| config.sync.central_url.clone(), str::to_owned);
    let bearer = token
        .map(str::to_owned)
        .or_else(|| config.sync.token.clone());
    if central_url.is_empty() {
        return Err(leb::Error::Config(
            "no central instance configured: set [sync] central_url or pass --central".to_owned(),
        ));
    }
    require_daemon(&config)?;
    let local = client(&config);
    let report = block_on(leb::sync::run_sync_cycle(
        &local,
        &central_url,
        bearer.as_deref(),
        !push_only,
        !pull_only && config.sync.push || push_only,
    ))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if let Some(pulled) = &report.pulled {
            println!(
                "Pulled: {} project(s) matched · {} memories added · {} updated · {} facts · {} links",
                pulled.projects_matched,
                pulled.memories_added,
                pulled.memories_updated,
                pulled.facts_applied,
                pulled.links_applied
            );
        }
        if let Some(offered) = &report.offered {
            println!(
                "Offered upstream: {} accepted as candidates · {} already known",
                offered.accepted_as_candidates, offered.deduplicated
            );
        }
        if report.pulled.is_none() && report.offered.is_none() {
            println!("Nothing to do.");
        }
    }
    Ok(true)
}

fn run_web(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    port: u16,
) -> Result<bool> {
    let config_root = discover_project_root(path)?;
    let mut config = Config::load_for(Some(&config_root), overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    println!("LEB workspace browser: http://127.0.0.1:{port}  (Ctrl-C to stop)");
    block_on(leb::web::serve(config, port))?;
    Ok(true)
}

fn run_vault(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &VaultAction,
) -> Result<bool> {
    let VaultAction::Build {
        path,
        all,
        pull,
        json,
    } = action;
    let config_root = discover_project_root(path)?;
    let mut config = Config::load_for(Some(&config_root), overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let client = client(&config);
    if *pull {
        pull_ff_only_if_clean(path)?;
    }
    let resolved = if *all {
        None
    } else {
        let response = block_on(client.refresh_index(path))?;
        Some(response.resolved.project.id)
    };
    // Vault exports run the expensive cross-module edge-resolution query per
    // project; a full-workspace build legitimately exceeds the default
    // request timeout, so this call gets a patient client.
    let patient = DaemonClient::new(
        &config.daemon.socket_path,
        Duration::from_millis(config.daemon.request_timeout_ms.max(600_000)),
    );
    let report = block_on(patient.build_vault(&leb::daemon::VaultBuildRequest {
        project_id: resolved,
    }))?;
    if *json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Vault at {}: {} project(s), {} file(s) written, {} unchanged.",
            report.root,
            report.projects.len(),
            report.files_written,
            report.files_unchanged
        );
        println!("Open the vault root in Obsidian to browse notes and diagrams.");
    }
    Ok(true)
}

/// Fast-forward the repo only when the working tree is clean; skipping
/// (with a notice) beats surprising a dirty checkout from a tool.
fn pull_ff_only_if_clean(path: &Path) -> Result<()> {
    let porcelain = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| leb::Error::Config(format!("git status: {error}")))?;
    if !porcelain.status.success() {
        return Err(leb::Error::Config(
            "git status failed; is this a repository?".to_owned(),
        ));
    }
    if !porcelain.stdout.is_empty() {
        println!("Skipping --pull: working tree has local changes.");
        return Ok(());
    }
    let pull = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["pull", "--ff-only"])
        .output()
        .map_err(|error| leb::Error::Config(format!("git pull: {error}")))?;
    if pull.status.success() {
        println!("Pulled (fast-forward only).");
    } else {
        println!(
            "Pull skipped: {}",
            String::from_utf8_lossy(&pull.stderr).trim()
        );
    }
    Ok(())
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
