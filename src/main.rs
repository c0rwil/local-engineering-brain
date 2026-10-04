use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use engram::Result;
use engram::config::{Config, ConfigOverrides};
use engram::context::{ContextCompileRequest, ContextMode};
use engram::daemon::{
    AttemptCreateRequest, EvidenceCreateRequest, JobEnqueueRequest, JobListRequest,
    MemoryCreateRequest, MemoryListRequest, MemoryPromoteRequest, ProjectExportRequest,
    SemanticRebuildRequest, SkillPromoteRequest, SynthesisRunRequest, TaskFinishRequest,
    TaskStartRequest, WorldSearchRequest,
};
use engram::daemon::{DaemonClient, ProjectStatus};
use engram::doctor::DoctorReport;
use engram::evaluation::EvaluationRunner;
use engram::events::EventStore;
use engram::experience::{AttemptOutcome, EvidenceStrength, TaskOutcome};
use engram::ids::{AttemptId, EvidenceId, JobId, MemoryId, TaskId};
use engram::jobs::{JobRecord, JobStatus, JobType};
use engram::learning::DistillationRequest;
use engram::memory::{EpistemicState, MemoryDraft};
use engram::projects::GitState;
use engram::storage::Database;

#[derive(Debug, Parser)]
#[command(
    name = "engram",
    version,
    about = "Engram",
    long_about = "Local-first engineering memory, world-model, and runtime tooling"
)]
struct Cli {
    /// Isolated engineering workspace. Defaults to `ENGRAM_WORKSPACE` or default.
    #[arg(long, global = true, value_name = "NAME")]
    workspace: Option<String>,
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
enum WorkspaceAction {
    List,
    Show,
}

#[derive(Debug, Subcommand)]
enum JudgeAction {
    Status,
    Run,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Inspect durable usage, context delivery, and learning coverage.
    Effectiveness {
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=3650))]
        days: u32,
        #[arg(long)]
        json: bool,
    },
    /// Record provider-reported usage from a JSON file (additive, idempotent samples).
    Usage {
        #[arg(value_name = "JSON_FILE")]
        file: PathBuf,
    },
    /// Inspect workspace profiles. Select one with --workspace NAME.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceAction,
    },
    /// Inspect or run automatic evidence review using the configured model.
    Judge {
        #[command(subcommand)]
        action: JudgeAction,
    },
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
        #[arg(long, value_name = "PATH", conflicts_with = "origin")]
        changed: Vec<PathBuf>,
        /// Import a bounded Graphify JSON export instead of native tree-sitter.
        #[arg(long, value_name = "JSON", conflicts_with = "origin")]
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
        /// Also export the SQLite-backed guide to this Markdown file.
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },

    /// Product catalog, living documentation, task bundles and lesson review.
    Product {
        #[command(subcommand)]
        action: ProductAction,
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
        /// `session-start`, `prompt` (`UserPromptSubmit`), `pretool`
        /// (`PreToolUse` for Read/Edit/Write), `bash` (`PostToolUse` for all
        /// tools; verification stays shell-only), `stop`, or `session-end`.
        event: String,
        /// Emit context as `hookSpecificOutput.additionalContext` JSON (the
        /// Codex CLI contract) instead of plain stdout.
        #[arg(long)]
        codex: bool,
        /// Read Cursor hook input and answer with one JSON object of Cursor's
        /// documented output fields. Takes precedence over `--codex`; no
        /// argument error is raised, because exit code 2 blocks in Cursor.
        #[arg(long)]
        cursor: bool,
    },

    /// Install or inspect the stock-agent integration (hooks + MCP).
    Hooks {
        #[command(subcommand)]
        action: HooksAction,
    },

    /// Print the CLAUDE.md snippet that teaches a coding agent the Engram
    /// episode loop (recall, record, verify, learn) via the MCP tools.
    AgentMd {
        /// Append the snippet to this repo's agent file (idempotent: skipped
        /// when the section is already present). Repeatable.
        #[arg(long, value_name = "REPO")]
        install: Vec<PathBuf>,
        /// Install at the USER level (~/.claude/CLAUDE.md): the agent loop
        /// applies universally instead of per repository.
        #[arg(long)]
        global: bool,
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

    /// Answer a question from the workspace's own knowledge: validated
    /// memories, facts, links, code symbols, and documentation sections
    /// retrieved for the question, then synthesized by a model (or printed
    /// as an evidence pack with --dry-run).
    Ask {
        /// The question.
        question: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Synthesize via the `claude` CLI (frontier quality).
        #[arg(long)]
        claude: bool,
        /// Synthesize via the configured local runtime with this model.
        #[arg(long)]
        model: Option<String>,
        /// Print the retrieved evidence pack without calling any model.
        #[arg(long)]
        dry_run: bool,
        /// Token budget for the compiled-context portion.
        #[arg(long, default_value_t = 2_000)]
        budget: u32,
    },

    /// Coordinate live agent sessions: send messages between them and see
    /// who is active. Delivery is at the recipient's next prompt, through
    /// its own hooks — no daemon required.
    Relay {
        #[command(subcommand)]
        action: RelayAction,
    },

    /// Update Engram in place: pull the recorded source checkout and rebuild
    /// through the atomic installer. The daemon restarts on the next CLI
    /// call, so a running session keeps the binary it started with.
    Update {
        /// Only report whether an update is available; change nothing.
        #[arg(long)]
        check: bool,
    },

    /// Manage skill packs: agent-native skills (`<name>/SKILL.md`)
    /// distributed through Engram into every detected agent's user-level
    /// skills directory (Claude Code, Codex), where they trigger natively
    /// when relevant to the task at hand.
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
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

    /// Manage registered projects (merge duplicate registrations).
    Project {
        #[command(subcommand)]
        action: ProjectAction,
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
enum ProjectAction {
    /// Merge a project into another registration of the same repository.
    /// Prints the plan and changes nothing unless --yes is given. With
    /// --yes: backup, one merge transaction, pin rewrites, re-index.
    Merge {
        /// The surviving project: id, name, or checkout path (a path
        /// contains `/` or starts with `.` or `~`).
        #[arg(long, value_name = "PROJECT")]
        into: String,
        /// The project to retire into the survivor: id, name, or checkout path.
        #[arg(long, value_name = "PROJECT")]
        from: String,
        /// Print the plan only.
        #[arg(long, conflicts_with = "yes")]
        dry_run: bool,
        /// Execute the merge.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
enum ProductAction {
    /// Print the typed catalog editing schema.
    Schema,
    /// Read verification, delivery and acceptance signals for a task.
    Signals { task_id: String },
    /// Show the catalog or lesson inbox as JSON.
    Show {
        #[arg(long)]
        reviews: bool,
    },
    /// Apply a versioned catalog mutation from a JSON file.
    Apply { file: PathBuf },
    /// Complete a task, review a lesson or record a delivery signal from JSON.
    Lesson { file: PathBuf },
    /// List immutable revisions of one record.
    History { id: String },
    /// Compile a bounded context bundle for a product entity or flow.
    Bundle {
        entity: String,
        task: String,
        #[arg(long, default_value_t = 2000)]
        budget: u32,
        #[arg(long)]
        environment: Option<String>,
    },
    /// Analyze changed paths or a unified diff without executing it.
    Impact {
        project: String,
        #[arg(long)]
        file: Vec<String>,
        #[arg(long)]
        diff: Option<PathBuf>,
        #[arg(long)]
        environment: Option<String>,
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
enum RelayAction {
    /// Send a message to one session (`engram relay board` lists ids) or to
    /// every active session with `all`.
    Send {
        /// Recipient session id, or `all`.
        to: String,
        message: String,
        /// Sender label shown to the recipient (defaults to `operator`).
        #[arg(long, default_value = "operator")]
        from: String,
    },
    /// List active sessions: id, project, open task, last activity.
    Board,
}

#[derive(Debug, Subcommand)]
enum SkillsAction {
    /// Fetch a pack (git URL or local git repo) and install its skills
    /// into every detected agent (Claude Code, Codex).
    Add {
        /// Git URL or local repository path.
        source: String,
        /// Pack name (defaults to the source's last path segment).
        #[arg(long)]
        name: Option<String>,
    },
    /// List fetched packs and the skills they carry.
    List,
    /// `git pull` every pack and re-install its skills everywhere.
    Update,
    /// Re-install all packs' skills into detected agents (e.g. after a new
    /// agent is installed on the machine).
    Install,
    /// Uninstall a pack's skills from every agent and delete the pack.
    Remove { name: String },
}

#[derive(Debug, Subcommand)]
enum HooksAction {
    /// Merge Engram's hooks into a repo's .claude/settings.json (idempotent)
    /// and register the `engram` MCP server for it.
    Install {
        #[arg(default_value = ".")]
        repo: PathBuf,
        /// Install at the USER level (~/.claude/settings.json) so every
        /// Claude session in every registered repo uses Engram. Unregistered
        /// repos are untouched: the hooks stay silent there.
        #[arg(long)]
        global: bool,
    },
    /// Print the hooks JSON without writing anything.
    Show,
    /// Print Codex setup: AGENTS.md snippet target and MCP configuration.
    Codex {
        /// Also write the automatic hooks into ~/.codex/hooks.json
        /// (session brief, context injection, file shapes, evidence
        /// capture — the same loop stock `claude` gets).
        #[arg(long)]
        install: bool,
    },
    /// Cursor agent hooks (hooks.json) and optional MCP registration
    /// (mcp.json). Without flags, prints the setup steps.
    Cursor(CursorSetupArgs),
}

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
struct CursorSetupArgs {
    /// Merge Engram's hooks into hooks.json (idempotent; other hooks are
    /// preserved).
    #[arg(long, conflicts_with_all = ["uninstall", "print"])]
    install: bool,
    /// Remove only Engram's hook entries.
    #[arg(long, conflicts_with = "print")]
    uninstall: bool,
    /// Print the entries Engram would merge, keyed by file; write nothing.
    #[arg(long)]
    print: bool,
    /// Register the engram-mcp server in mcp.json. Alone: MCP only. With
    /// --install, --uninstall or --print: hooks and MCP together.
    #[arg(long)]
    mcp: bool,
    /// Cursor's user configuration directory (default: `ENGRAM_CURSOR_HOME`,
    /// then ~/.cursor).
    #[arg(long, value_name = "DIR")]
    cursor_home: Option<PathBuf>,
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
    /// Ask the daemon to finish in-flight requests and stop, then wait for
    /// its process to exit.
    Stop {
        /// Seconds to wait for the daemon process to exit (0: do not wait).
        #[arg(long, default_value_t = 150)]
        wait_secs: u64,
        #[arg(long)]
        json: bool,
    },
    /// Stop the daemon gracefully and start a new one from this terminal, so
    /// it inherits this terminal's environment and macOS folder access.
    Restart {
        /// Seconds to wait for the old daemon to finish in-flight work.
        #[arg(long, default_value_t = 150)]
        wait_secs: u64,
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
    /// Undo a specific symbol-drift quarantine only if every source digest matches again.
    RestoreDrift {
        id: String,
        /// Sequence of the last `MEMORY_STATE_CHANGED` event that quarantined this memory.
        #[arg(long)]
        event: u64,
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
    /// Permanently remove a captured prompt; existing exports/backups are separate.
    PurgePrompt {
        task_id: String,
        #[arg(long)]
        json: bool,
    },
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
            eprintln!("engram: {error}");
            ExitCode::FAILURE
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run(cli: Cli) -> Result<bool> {
    let overrides = ConfigOverrides {
        workspace: cli.workspace,
        db_path: cli.db,
        local_only: None,
    };
    let socket_override = cli.socket;
    match cli.command {
        Commands::Effectiveness { days, json } => {
            run_effectiveness(&overrides, socket_override.as_deref(), days, json)
        }
        Commands::Usage { file } => {
            let request: engram::effectiveness::UsageRecordRequest = serde_json::from_reader(
                std::fs::File::open(&file).map_err(|e| engram::Error::Config(e.to_string()))?,
            )?;
            let config = load_daemon_config(&overrides, socket_override.as_deref(), None)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&block_on(client(&config).record_usage(&request))?)?
            );
            Ok(true)
        }
        Commands::Workspace { action } => run_workspace(&overrides, &action),
        Commands::Judge { action } => {
            let config = load_daemon_config(&overrides, socket_override.as_deref(), None)?;
            let client = client(&config);
            let value = match action {
                JudgeAction::Status => block_on(client.judge_status())?,
                JudgeAction::Run => block_on(client.judge_run())?,
            };
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(true)
        }
        Commands::Product { action } => {
            run_product(&overrides, socket_override.as_deref(), &action)
        }
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
        Commands::Hook {
            event,
            codex,
            cursor,
        } => run_hook_event(
            &overrides,
            socket_override.as_deref(),
            &event,
            codex,
            cursor,
        ),
        Commands::Hooks { action } => run_hooks_action(&action),
        Commands::Skills { action } => run_skills_action(&action),
        Commands::Update { check } => run_update(check),
        Commands::Relay { action } => run_relay_action(&overrides, &action),
        Commands::Ask {
            question,
            path,
            claude,
            model,
            dry_run,
            budget,
        } => run_ask(
            &overrides,
            socket_override.as_deref(),
            &question,
            &path,
            claude,
            model.as_deref(),
            dry_run,
            budget,
        ),
        Commands::AgentMd {
            install,
            global,
            file,
        } => {
            let mut install = install.clone();
            if global {
                install.push(dirs_home().join(".claude"));
            }
            if install.is_empty() {
                println!("{AGENT_MD_SNIPPET}");
            }
            for repo in &install {
                let target = repo.join(&file);
                let existing = std::fs::read_to_string(&target).unwrap_or_default();
                // Refresh in place: an existing section (current or legacy
                // LEB header) is replaced so tool names and guidance update.
                let header = [
                    "## Engineering memory (Engram)",
                    "## Engineering memory (LEB)",
                ]
                .into_iter()
                .find_map(|marker| existing.find(marker));
                if let Some(start) = header {
                    let rest = &existing[start..];
                    let end = rest[1..]
                        .find("\n## ")
                        .map_or(existing.len(), |offset| start + 1 + offset + 1);
                    let refreshed = format!(
                        "{}{}{}",
                        &existing[..start],
                        AGENT_MD_SNIPPET,
                        &existing[end..]
                    );
                    std::fs::write(&target, refreshed).map_err(|error| {
                        engram::Error::Config(format!("{}: {error}", target.display()))
                    })?;
                    println!("{}: refreshed", target.display());
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
                        engram::Error::Config(format!("{}: {error}", target.display()))
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
                run_origin_index(&overrides, socket_override.as_deref(), &path, json)
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
        Commands::Project { action } => {
            run_project(&overrides, socket_override.as_deref(), &action)
        }
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
            let config = Config::load_for_current_dir(&overrides)?;
            let database = Database::open(&config.storage.db_path)?;
            EventStore::new(database).hold_uncommitted_test_append(&ready)?;
            Ok(false)
        }
    }
}

fn run_project(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &ProjectAction,
) -> Result<bool> {
    let ProjectAction::Merge {
        into,
        from,
        dry_run,
        yes,
        json,
    } = action;
    let config = load_daemon_config(overrides, socket, None)?;
    let client = client(&config);
    let request = engram::daemon::ProjectMergeRequest {
        into: merge_selector(into)?,
        from: merge_selector(from)?,
        dry_run: !*yes,
    };
    let response = block_on(client.merge_projects(&request))?;
    if !*yes {
        return print_merge_preview(&response, *dry_run, *json);
    }
    let (outcome, already) = match response {
        engram::daemon::ProjectMergeResponse::Merged { outcome } => (outcome, false),
        engram::daemon::ProjectMergeResponse::AlreadyMerged { outcome } => (outcome, true),
        engram::daemon::ProjectMergeResponse::Planned { .. } => {
            return Err(engram::error::Error::Daemon(
                "the daemon planned instead of merging".to_owned(),
            ));
        }
    };
    if !*json {
        match (&outcome.reindexed, &outcome.reindex_path) {
            (Some(_), _) => eprintln!("Merge recorded. Checking pins..."),
            (None, Some(path)) => {
                eprintln!("Merge recorded. Rewriting pins and re-indexing {path}...");
            }
            (None, None) => eprintln!("Merge recorded. Rewriting pins..."),
        }
    }
    let follow_up = block_on(engram::merge::client::follow_up(&client, &outcome))?;
    if *json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "status": if already { "already_merged" } else { "merged" },
                "outcome": outcome,
                "follow_up": follow_up,
            }))?
        );
    } else {
        print!(
            "{}",
            engram::merge::render::outcome_text(&outcome, already, &follow_up)
        );
    }
    Ok(follow_up.complete)
}

/// Print a merge plan (or an earlier merge) without changing anything.
fn print_merge_preview(
    response: &engram::daemon::ProjectMergeResponse,
    dry_run: bool,
    json: bool,
) -> Result<bool> {
    use engram::daemon::ProjectMergeResponse;
    use engram::projects::PinState;
    match response {
        ProjectMergeResponse::Planned { plan } => {
            let pins = engram::merge::client::rewrite_pins(
                &plan.checkouts_to_repin,
                &plan.retired.id,
                &plan.survivor.id,
                &plan.survivor.name,
                true,
            );
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "planned",
                        "plan": plan,
                        "pins": pins,
                    }))?
                );
            } else {
                print!("{}", engram::merge::render::plan_text(plan, &pins));
            }
            if plan.identity.refusal.is_some() {
                return Ok(false);
            }
            if !dry_run {
                eprintln!("Nothing was changed. Run the same command with --yes to merge.");
            }
            Ok(dry_run)
        }
        ProjectMergeResponse::AlreadyMerged { outcome } => {
            let pins = engram::merge::client::rewrite_pins(
                &outcome.checkouts_to_repin,
                &outcome.retired,
                &outcome.survivor,
                &outcome.survivor_name,
                true,
            );
            let remaining = outcome.pending_anchors > 0
                || outcome.reindexed.is_none()
                || pins.iter().any(|pin| {
                    matches!(
                        pin.state,
                        PinState::WouldRewrite | PinState::Denied | PinState::Failed
                    )
                });
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "status": "already_merged",
                        "outcome": outcome,
                        "pins": pins,
                        "remaining_steps": remaining,
                    }))?
                );
            } else {
                println!(
                    "Already merged: {} ({}) into {} ({}).",
                    outcome.retired_name, outcome.retired, outcome.survivor_name, outcome.survivor
                );
                if remaining {
                    println!(
                        "Pins, the re-index or memory anchors still wait; run the same command \
                         with --yes to finish them."
                    );
                }
            }
            Ok(!remaining || dry_run)
        }
        ProjectMergeResponse::Merged { .. } => Err(engram::error::Error::Daemon(
            "the daemon merged during a dry run".to_owned(),
        )),
    }
}

/// A `--into`/`--from` value as the daemon expects it: ids and names pass
/// through; a checkout path (it contains `/` or starts with `.` or `~`) is
/// made absolute, since the daemon's working directory is not this one.
fn merge_selector(selector: &str) -> Result<String> {
    let selector = selector.trim();
    if !(selector.contains('/') || selector.starts_with('.') || selector.starts_with('~')) {
        return Ok(selector.to_owned());
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    let expanded = match selector.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home()
            .ok_or_else(|| engram::error::Error::Config("HOME is not set".to_owned()))?
            .join(rest.trim_start_matches('/')),
        _ => PathBuf::from(selector),
    };
    let absolute = std::path::absolute(&expanded)
        .map_err(|source| engram::error::io_error_for_cli(&expanded, source))?;
    Ok(absolute
        .canonicalize()
        .unwrap_or(absolute)
        .display()
        .to_string())
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
        TaskAction::Attempt { .. } | TaskAction::Finish { .. } | TaskAction::PurgePrompt { .. } => {
            None
        }
    };
    let config = load_daemon_config(overrides, socket, project_root.as_deref())?;
    let client = client(&config);
    let value = match action {
        TaskAction::PurgePrompt { task_id, .. } => {
            block_on(client.purge_task_prompt(&TaskId::parse(task_id)?))?;
            serde_json::json!({"purged": true, "scope": "task_prompt",
                "backup_limitation": "existing backups and exports are not modified"})
        }
        TaskAction::Start {
            prompt,
            path,
            session,
            ..
        } => {
            let project = block_on(client.resolve_project(path))?;
            serde_json::to_value(block_on(client.start_task(&TaskStartRequest {
                context_request_id: None,
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
                    .map_err(|source| engram::error::io_error_for_cli(path, source))?;
                if artifact_metadata.len() > u64::from(config.daemon.max_request_bytes / 2) {
                    return Err(engram::Error::Config(
                        "evidence artifact exceeds half the daemon request limit".to_owned(),
                    ));
                }
                std::fs::read_to_string(path)
                    .map_err(|source| engram::error::io_error_for_cli(path, source))?
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
        TaskAction::PurgePrompt { json, .. }
        | TaskAction::Start { json, .. }
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
    let mut config = if let Some(root) = project_root {
        Config::load_for(Some(root), overrides)?
    } else {
        Config::load_for_current_dir(overrides)?
    };
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

fn run_origin_index(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    path: &Path,
    json: bool,
) -> Result<bool> {
    let root = discover_project_root(path)?;
    let config = load_daemon_config(overrides, socket, Some(&root))?;
    let client = DaemonClient::new(&config.daemon.socket_path, Duration::from_secs(600));
    let project = block_on(client.resolve_project(&root))?.resolved.project;
    let response = block_on(
        client.origin_refresh(&engram::daemon::OriginRefreshRequest {
            project_id: project.id,
        }),
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("Indexed {} in the separate origin view", response.reference);
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
            "\n[Engram: {}/{} tokens, {} items, {} ms]",
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
            // A local embedding model rebuild may outlive the default
            // deadline; the daemon allows it at least ten minutes.
            let patient = DaemonClient::new(
                &config.daemon.socket_path,
                Duration::from_millis(config.daemon.request_timeout_ms)
                    .max(Duration::from_secs(660)),
            );
            let response = block_on(patient.rebuild_semantic_index(&SemanticRebuildRequest {
                project_id: status.resolved.project.id,
            }))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                println!(
                    "Rebuilt {} semantic vectors for {}",
                    response.vectors, response.project_id
                );
                if let Some(model) = &response.model {
                    println!(
                        "{}: {} embedded, {} unchanged, {} removed, {} changed during the run \
                         ({} active memories, dimension {}, {} requests, {} ms){}",
                        model.embedder,
                        model.embedded,
                        model.unchanged,
                        model.removed,
                        model.skipped_changed,
                        model.active_memories,
                        model.dimension,
                        model.requests,
                        model.duration_ms,
                        if model.complete {
                            ""
                        } else {
                            "; stopped at the deadline, rerun to continue"
                        }
                    );
                }
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
                .ok_or_else(|| engram::Error::Config(format!("unknown job `{id}`")))?;
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
                .ok_or_else(|| engram::Error::Config(format!("unknown memory `{id}`")))?;
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
        MemoryAction::RestoreDrift { id, event, json } => {
            let memory = block_on(client.restore_memory_drift(
                &MemoryId::parse(id)?,
                &engram::daemon::MemoryRestoreDriftRequest {
                    expected_event_sequence: *event,
                },
            ))?;
            print_memory(&memory, *json)?;
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
        return Err(engram::Error::Config(
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

fn print_memory(memory: &engram::memory::MemoryRecord, json: bool) -> Result<()> {
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
    let mut config = Config::load_for_current_dir(overrides)?;
    apply_socket_override(&mut config, socket)?;
    let report = if config.daemon.socket_path.exists() {
        require_daemon(&config)?;
        block_on(client(&config).doctor())?
    } else {
        DoctorReport::run(&config)
    }
    .with_agent_checks();
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Engram diagnostics");
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
            let link = block_on(client.link_project(&engram::daemon::ProjectLinkRequest {
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
            let removed = block_on(
                client.unlink_project(&engram::daemon::ProjectUnlinkRequest {
                    project_id: source,
                    linked_project_id: target_project.id,
                }),
            )?
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
            let report = block_on(client.scan_links(&engram::daemon::LinkScanRequest {
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
                    println!("Run `engram link scan --apply` to create the definitive links.");
                }
            }
        }
        LinkAction::List { json, .. } => {
            let links = block_on(
                client.project_links(&engram::daemon::ProjectLinksRequest { project_id: source }),
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
    let prompt = engram::architect::synthesis_prompt(&dossier);
    if dry_run {
        println!("{prompt}");
        return Ok(true);
    }
    let synthesized;
    let model = if claude {
        let model = model.unwrap_or("claude-opus-4-8").to_owned();
        println!("Synthesizing with `{model}` via the claude CLI …");
        synthesized = engram::architect::synthesize_with_claude_cli(&model, &prompt)?;
        model
    } else {
        let model = model
            .map(str::to_owned)
            .or_else(|| {
                (!config.architect.model.is_empty()).then(|| config.architect.model.clone())
            })
            .ok_or_else(|| {
                engram::Error::Config(
                    "no model configured: pass --model, --claude, or set architect.model"
                        .to_owned(),
                )
            })?;
        engram::architect::enforce_local_only(
            &config.architect.runtime_url,
            config.network.local_only,
        )?;
        println!(
            "Synthesizing with `{model}` via {} …",
            config.architect.runtime_url
        );
        synthesized = block_on(engram::architect::synthesize(
            &config.architect.runtime_url,
            &model,
            &prompt,
        ))?;
        model
    };
    let document = engram::architect::guide_document(&model, &dossier, &synthesized);
    save_product_guide(&client(&config), &model, &dossier, &document)?;
    let destination = out.map_or_else(|| config.storage.db_path.clone(), Path::to_path_buf);
    if let Some(out) = out {
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| engram::Error::Config(e.to_string()))?;
        }
        std::fs::write(out, &document).map_err(|e| engram::Error::Config(e.to_string()))?;
    }
    if config.vault.enabled {
        std::fs::create_dir_all(&config.vault.root)
            .map_err(|e| engram::Error::Config(e.to_string()))?;
        std::fs::write(config.vault.root.join("ARCHITECTURE.md"), &document)
            .map_err(|e| engram::Error::Config(e.to_string()))?;
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&engram::architect::ArchitectReport {
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
            let fact = block_on(client.set_fact(&engram::daemon::FactSetRequest {
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
            let removed = block_on(client.unset_fact(&engram::daemon::FactUnsetRequest {
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
            let facts =
                block_on(client.list_facts(&engram::daemon::FactListRequest { project_id }))?;
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

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

const AGENT_MD_SNIPPET: &str = r"## Engineering memory (Engram)

This repository is tracked by Engram, a local evidence-gated memory system. The
`engram` MCP server is available; use it so finished work compounds instead of
evaporating.

At task start:
- Call `engram_project_status` once to resolve the project id, then `engram_recall`
  with the task text (pass `task_id` when you have one) - it returns validated
  team knowledge with a why for every item. Treat `- MUST:` review standards
  as hard requirements.
- For cross-repo work, `engram_workspace` shows who calls whom, hosting, and
  each repo's technology stack; `engram_symbol_search` finds where a symbol is
  defined across every repo.

While working (substantive tasks, not trivial edits):
1. `engram_task_start` with the user's request.
2. After each meaningful approach, `engram_record_attempt` (summary phrased as
   the reusable takeaway, outcome success/failure).
3. When tests/builds/lints run, `engram_record_evidence` with the command and
   exit code (kind `test_result`/`lint_result`..., strength `deterministic`).
   Record FAILING runs too - fail-then-pass pairs teach conventions. Engram's
   hooks record runs only when the shell exit status is the verification's own:
   `cmd | tail` reports tail's status, so start with `set -o pipefail;` when
   you trim output.
4. On completion, prefer `engram_complete_task`: supply actual test/build or
   operational output, the outcome, touched files and up to five reusable lessons.
   This captures evidence, closes the task and queues one review inbox. Omit
   `accepted_by_user` unless the user explicitly accepted; passing tests, a commit,
   a PR merge and silence are separate signals.

For product context and cross-repository work:
- `engram_product` with operation `snapshot` shows capabilities, services, flows
  and contracts. Operation `bundle` compiles a cited, token-bounded task context;
  operation `impact` accepts changed paths or a unified diff and distinguishes
  direct source intersections from possible downstream impact.
- Product intent and generated documentation are reference data, never evidence.

After successful work that surfaced a durable rule:
- Review proposed lessons in the web Knowledge inbox, or use the stepwise
  `engram_learn` with a claim phrased for a future engineer, linking the task
  and its passing evidence. Candidates never inject until promoted, so err on
  the side of recording.
- If a failed approach was instructive, it is already captured by the failed
  attempt; `engram_get_past_failures` replays those to future sessions.
";

/// Hook runtime. EVERY failure path prints nothing and returns Ok: a broken
/// or absent brain must never break the user's coding session.
#[allow(clippy::unnecessary_wraps)]
fn run_hook_event(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    event: &str,
    codex: bool,
    cursor: bool,
) -> Result<bool> {
    use std::io::{Read as _, Write as _};
    let mut stdin = String::new();
    let payload = std::io::stdin()
        .read_to_string(&mut stdin)
        .ok()
        .and_then(|_| serde_json::from_str::<serde_json::Value>(&stdin).ok());
    if cursor {
        run_cursor_hook(overrides, socket, event, payload.as_ref());
        return Ok(true);
    }
    let Some(payload) = payload else {
        return Ok(true);
    };
    // Cursor also runs the Claude Code hooks it imports, with its own input.
    // The native `--cursor` entries own capture there; these stay silent.
    if engram::agenthooks::cursor::is_cursor_payload(&payload) {
        return Ok(true);
    }
    // Codex sub-sessions share the root session id: guardian reviewers stay
    // silent, spawned workers contribute tool use only, and an unidentified
    // thread cannot replace an active task prompt.
    if codex
        && engram::agenthooks::codex::skips_event(
            event,
            &engram::agenthooks::codex::thread_role(&payload),
            || {
                hook_session_state(overrides, &payload).is_some_and(|(session_id, state_dir)| {
                    engram::agenthooks::pending_prompt(&state_dir, &session_id).is_some()
                        || !engram::agenthooks::project_sessions(&state_dir, &session_id).is_empty()
                })
            },
        )
    {
        return Ok(true);
    }
    // One emitter for every context-bearing event: plain stdout for Claude
    // Code (its native contract for SessionStart/UserPromptSubmit), the
    // additionalContext envelope for Codex — and for tool events everywhere
    // (PreToolUse, PostToolUse, PostToolUseFailure), where plain stdout is
    // not read back into context.
    let emit = |event_name: &str, context: &str| {
        let output = if codex
            || matches!(
                event_name,
                "PreToolUse" | "PostToolUse" | "PostToolUseFailure"
            ) {
            engram::agenthooks::additional_context_output(event_name, context)
        } else {
            context.to_owned()
        };
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{output}")
            .and_then(|()| stdout.flush())
            .is_ok()
    };
    let agent = if codex {
        engram::agenthooks::HookClient::Codex
    } else {
        engram::agenthooks::HookClient::ClaudeCode
    };
    let _ = run_hook_payload(overrides, socket, event, agent, &payload, &emit);
    Ok(true)
}

/// Cursor adapter: translate Cursor's hook input, run the shared handlers, and
/// answer with exactly one JSON object of Cursor's documented output fields.
/// A file or shell tool result also delivers repository context and relayed
/// messages, because Cursor reads context only after tool results.
fn run_cursor_hook(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    event: &str,
    raw: Option<&serde_json::Value>,
) {
    use engram::agenthooks::cursor;
    use std::io::Write as _;
    let write = |response: &serde_json::Value| {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{response}")
            .and_then(|()| stdout.flush())
            .is_ok()
    };
    let Some(payload) = raw.map(cursor::normalize_payload) else {
        write(&cursor::response(event, None));
        return;
    };
    let steps = match event {
        "bash" if cursor::delivers_context(&payload) => vec!["bash", "pretool"],
        "session-start" | "prompt" | "bash" | "stop" | "session-end" => vec![event],
        _ => Vec::new(),
    };
    let relay = if steps.contains(&"pretool") {
        cursor_relay(overrides, &payload)
    } else {
        String::new()
    };
    // A status note from the shell step waits for the context the pre-tool
    // step may add, so the call still gets exactly one response. Relay
    // messages, repository context and a note together stay within the
    // response size Cursor accepts (checked in `agenthooks`).
    let defer_notes = steps.contains(&"pretool");
    let note = std::cell::RefCell::new(String::new());
    let printed = std::cell::Cell::new(false);
    let emit = |event_name: &str, context: &str| {
        if defer_notes && matches!(event_name, "PostToolUse" | "PostToolUseFailure") {
            note.borrow_mut().push_str(context);
            return true;
        }
        if printed.replace(true) {
            return false;
        }
        write(&cursor::response(
            event,
            Some(&format!("{relay}{context}{}", note.borrow())),
        ))
    };
    for step in steps {
        let daemon_available = run_hook_payload(
            overrides,
            socket,
            step,
            engram::agenthooks::HookClient::Cursor,
            &payload,
            &emit,
        );
        // Without the daemon a later step would only wait for it again.
        if matches!(daemon_available, Ok(false)) {
            break;
        }
    }
    if raw.is_some_and(cursor::failed_verification) {
        forget_last_verification(overrides, &payload);
    }
    if !printed.get() {
        write(&cursor::response(
            event,
            Some(&format!("{relay}{}", note.borrow())),
        ));
    }
}

/// Session id and hook state directory for a hook payload in the shared shape.
fn hook_session_state(
    overrides: &ConfigOverrides,
    payload: &serde_json::Value,
) -> Option<(String, PathBuf)> {
    let session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)?;
    let cwd = payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let config = Config::load_for(discover_project_root(&cwd).ok().as_deref(), overrides).ok()?;
    let state_dir = config.storage.db_path.parent()?.to_path_buf();
    Some((session_id.to_owned(), state_dir))
}

/// Drain the Cursor session's relay inbox into a bounded block.
fn cursor_relay(overrides: &ConfigOverrides, payload: &serde_json::Value) -> String {
    hook_session_state(overrides, payload).map_or_else(String::new, |(session_id, state_dir)| {
        engram::agenthooks::cursor::relay_block(&engram::agenthooks::relay_drain(
            &state_dir,
            &session_id,
        ))
    })
}

/// A verification failed without an observed exit code, so the latest result
/// is unknown. Forget the session's last recorded result, so its episodes
/// cannot auto-close as a success on an older pass. Conservatively, every
/// project episode of the session is reset, without a daemon lookup.
fn forget_last_verification(overrides: &ConfigOverrides, payload: &serde_json::Value) {
    let Some((session_id, state_dir)) = hook_session_state(overrides, payload) else {
        return;
    };
    for (scope, mut session) in engram::agenthooks::project_sessions(&state_dir, &session_id) {
        if session.last_verification_exit.is_some() || session.last_evidence_fingerprint.is_some() {
            session.forget_last_result();
            let _ = engram::agenthooks::save_session(&state_dir, &scope, &session);
        }
    }
}

/// Shared hook handlers for every client. `emit` returns whether the context
/// reached the agent; only then are delivery receipts recorded. Returns
/// `Ok(false)` only when the daemon could not be reached.
#[allow(clippy::too_many_lines, clippy::unnecessary_wraps)]
fn run_hook_payload(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    event: &str,
    agent: engram::agenthooks::HookClient,
    payload: &serde_json::Value,
    emit: &dyn Fn(&str, &str) -> bool,
) -> Result<bool> {
    let session_id = payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown-session")
        .to_owned();
    let cwd = payload
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let tool_input = payload.get("tool_input").cloned().unwrap_or_default();
    let tool_paths = engram::agenthooks::resolve_tool_paths(&cwd, &tool_input);
    let target_dir = tool_paths.first().map_or_else(
        || engram::agenthooks::tool_directory(&cwd, &tool_input),
        |path| engram::agenthooks::tool_path_directory(path),
    );
    let config_dir = if matches!(event, "pretool" | "bash") {
        &target_dir
    } else {
        &cwd
    };
    let Ok(mut config) =
        Config::load_for(discover_project_root(config_dir).ok().as_deref(), overrides)
    else {
        return Ok(true);
    };
    if apply_socket_override(&mut config, socket).is_err() {
        return Ok(true);
    }
    let state_dir = config
        .storage
        .db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    if event == "prompt" {
        let prompt = payload
            .get("user_prompt")
            .or_else(|| payload.get("prompt"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if !prompt.is_empty() {
            let Ok(redactor) =
                engram::security::Redactor::new(&config.security.custom_redaction_patterns)
            else {
                return Ok(true);
            };
            if engram::agenthooks::save_pending_prompt(&state_dir, &session_id, prompt, &redactor)
                .is_err()
            {
                return Ok(true);
            }
        }
    }
    // A client that cannot receive prompt-time context keeps only the intent.
    if event == "prompt" && !agent.prompt_context() {
        return Ok(true);
    }
    if require_daemon(&config).is_err() {
        return Ok(false);
    }
    let client = client(&config);
    match event {
        "prompt" => {
            // Relay delivery point: every prompt drains this session's inbox,
            // pinned or not — a message queued mid-session still arrives.
            let relayed = engram::agenthooks::relay_drain(&state_dir, &session_id);
            let relay_block = if relayed.is_empty() {
                String::new()
            } else {
                format!(
                    "# Messages from other sessions (engram relay)\n{}\n",
                    relayed
                        .iter()
                        .map(|message| format!("- {message}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            };
            // A prompt refreshes intent without opening another episode. Only a
            // successful emission replaces the last delivered context receipt.
            let delivered = (|| {
                let prompt = engram::agenthooks::pending_prompt(&state_dir, &session_id)?;
                let lookup = block_on(client.lookup_project(&cwd)).ok()?;
                lookup.project?; // Never auto-enroll an untracked repository.
                let status = block_on(client.resolve_project(&cwd)).ok()?;
                let project_id = status.resolved.project.id.clone();
                let scope = engram::agenthooks::scoped_session_id(&session_id, &project_id);
                let current_paths = engram::agenthooks::recent_paths(&state_dir, &scope);
                let claim = engram::agenthooks::claim_context_delivery(
                    &state_dir,
                    &scope,
                    &prompt,
                    &current_paths,
                    config.context.max_injected_tokens,
                    status.resolved.git.as_ref(),
                )?;
                let request_id = format!("hook_{}", uuid::Uuid::new_v4());
                let context = block_on(client.compile_context(&ContextCompileRequest {
                    request_id: request_id.clone(),
                    project_id,
                    task_text: prompt,
                    current_paths,
                    token_budget: config.context.max_injected_tokens,
                    mode: ContextMode::Brain,
                    repository_root: Some(status.resolved.root),
                    head: status.resolved.git.and_then(|git| git.head),
                    task_id: None,
                    framing: None,
                }))
                .ok()?;
                let text = format!("{}{}", context.stable_prefix, context.dynamic_context);
                if text.trim().is_empty() {
                    let _ = claim.invalidate_empty();
                    return None;
                }
                let digest = context.delivery_signature();
                if !claim.should_emit_digest(&digest).unwrap_or(false) {
                    return None;
                }
                let task_id = engram::agenthooks::load_session(&state_dir, &scope)
                    .map(|session| session.task_id);
                Some((claim, request_id, context.id, text, task_id, digest))
            })();
            let injected = format!(
                "{relay_block}{}",
                delivered
                    .as_ref()
                    .map_or("", |delivery| delivery.3.as_str())
            );
            if !injected.trim().is_empty() && emit("UserPromptSubmit", &injected) {
                if let Some((claim, request_id, run_id, _, task_id, digest)) = delivered {
                    let _ = claim.delivered_digest(&request_id, &run_id, &digest);
                    if let Some(task_id) = task_id {
                        let _ = block_on(client.context_delivered(
                            &engram::daemon::ContextDeliveryRequest { run_id, task_id },
                        ));
                    }
                }
            }
        }
        "session-start" => {
            initialize_hook_usage_baseline(&state_dir, &session_id, payload);
            if !engram::agenthooks::claim_once(&state_dir, &session_id, "brief") {
                return Ok(true); // a concurrent hook (user + project level) already briefed
            }
            let lookup = match block_on(client.lookup_project(&cwd)) {
                Ok(lookup) => lookup,
                Err(error) => {
                    // The one failure worth a line: the daemon cannot read
                    // this repository, so every other hook stays silent here.
                    // Like the enrollment hint, a folder this hook can see is
                    // not in a Git repository stays silent.
                    let plain_folder = std::fs::read_dir(&cwd).is_ok()
                        && !cwd.ancestors().any(|dir| dir.join(".git").exists());
                    if engram::error::is_access_denied(&error) && !plain_folder {
                        emit(
                            "SessionStart",
                            &engram::error::access_denied_hook_notice(&error),
                        );
                    }
                    return Ok(true);
                }
            };
            let Some(project) = lookup.project else {
                // Untracked repo: one opt-in line (never auto-enroll). Only
                // for actual git repositories — a bare folder stays silent.
                if cwd.join(".git").exists() {
                    emit(
                        "SessionStart",
                        "Engram: this repository is not tracked by the engineering brain. \
                         Run `engram index .` once to enroll it (context injection, evidence \
                         capture, and the workspace encyclopedia activate automatically).",
                    );
                }
                return Ok(true);
            };
            let Ok(brief) = block_on(client.tasks_brief(&project.id)) else {
                return Ok(true);
            };
            let open: Vec<(String, String, i64)> = brief
                .open
                .iter()
                .map(|task| (task.id.clone(), task.prompt.clone(), task.started_at_ms))
                .collect();
            let last_completed = brief
                .last_completed
                .as_ref()
                .map(|task| (task.status.as_str(), task.prompt.as_str()));
            let now_ms = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or_default(),
            )
            .unwrap_or(i64::MAX);
            if let Some(text) = engram::agenthooks::render_session_brief(
                &project.name,
                &open,
                last_completed,
                now_ms,
            ) {
                emit("SessionStart", &text);
            }
        }
        "pretool" => {
            let mut pretool_context = String::new();
            let mut deliveries = Vec::new();
            let byte_limit = if agent.bounded_context() {
                usize::try_from(config.context.max_injected_tokens.saturating_mul(4))
                    .unwrap_or(usize::MAX)
                    .min(agent.context_byte_cap())
            } else {
                usize::MAX
            };
            for (resolved, observed_paths) in hook_tool_projects(&client, &target_dir, &tool_paths)
            {
                let project_id = resolved.resolved.project.id.clone();
                let checkout_root = resolved.resolved.root.clone();
                let Some((scope, session)) =
                    hook_episode(&client, &state_dir, &session_id, &project_id)
                else {
                    continue;
                };
                // Observe the touch before any context it triggers is compiled.
                record_hook_tool_observation(
                    &client,
                    &state_dir,
                    &scope,
                    &session,
                    payload,
                    &observed_paths,
                    engram::effectiveness::ToolPhase::Pre,
                    agent,
                );
                let _ = engram::agenthooks::remember_paths(&state_dir, &scope, &observed_paths);
                let current_paths = engram::agenthooks::recent_paths(&state_dir, &scope);
                // Receipts and briefs follow the conversation that saw them.
                let delivery_scope = engram::agenthooks::thread_scope(&scope, payload);
                // Capture each touched repository even if an earlier block used
                // the output budget. Undelivered context remains retryable.
                let remaining_tokens = if agent.bounded_context() {
                    u32::try_from(byte_limit.saturating_sub(pretool_context.len()) / 4)
                        .unwrap_or(u32::MAX)
                } else {
                    config.context.max_injected_tokens
                };
                let prompt =
                    engram::agenthooks::pending_prompt(&state_dir, &session_id).unwrap_or_default();
                // The run whose source map ranks the file briefs below: this
                // call's compilation, else the last one delivered here.
                let mut source_run = None;
                if let Some(claim) = (remaining_tokens > 0)
                    .then(|| {
                        engram::agenthooks::claim_context_delivery(
                            &state_dir,
                            &delivery_scope,
                            &prompt,
                            &current_paths,
                            remaining_tokens,
                            resolved.resolved.git.as_ref(),
                        )
                    })
                    .flatten()
                {
                    let request_id = format!("hook_{}", uuid::Uuid::new_v4());
                    if let Ok(context) = block_on(client.compile_context(&ContextCompileRequest {
                        request_id: request_id.clone(),
                        project_id: project_id.clone(),
                        task_text: prompt.clone(),
                        current_paths,
                        token_budget: remaining_tokens,
                        mode: ContextMode::Brain,
                        repository_root: Some(resolved.resolved.root),
                        head: resolved.resolved.git.and_then(|git| git.head),
                        task_id: None,
                        framing: None,
                    })) {
                        let text = format!("{}{}", context.stable_prefix, context.dynamic_context);
                        let digest = context.delivery_signature();
                        source_run = Some(context.id.clone());
                        if text.trim().is_empty() {
                            let _ = claim.invalidate_empty();
                        } else if text.len() <= byte_limit.saturating_sub(pretool_context.len())
                            && claim.should_emit_digest(&digest).unwrap_or(false)
                        {
                            pretool_context.push_str(&text);
                            deliveries.push((
                                claim,
                                request_id,
                                context.id,
                                digest,
                                session.task_id,
                            ));
                        }
                    }
                }
                let source_run = source_run
                    .or_else(|| engram::agenthooks::context_run_id(&state_dir, &delivery_scope));
                for relative in observed_paths {
                    let Ok(shape) =
                        block_on(client.file_shape(&engram::daemon::FileShapeRequest {
                            project_id: project_id.clone(),
                            file_path: relative.clone(),
                            repository_root: Some(checkout_root.clone()),
                            task_text: (!prompt.is_empty()).then(|| prompt.clone()),
                            context_run_id: source_run.clone(),
                        }))
                    else {
                        continue;
                    };
                    let symbols: Vec<_> = shape
                        .symbols
                        .iter()
                        .map(|symbol| engram::agenthooks::BriefSymbol {
                            kind: symbol.kind.clone(),
                            label: symbol.label.clone(),
                            lines: symbol.lines,
                            task_match: symbol.task_match,
                        })
                        .collect();
                    let callers: Vec<(String, String)> = shape
                        .callers
                        .iter()
                        .map(|caller| (caller.label.clone(), caller.file_path.clone()))
                        .collect();
                    // Bounded by the client's whole-message cap, so a brief
                    // that misses the remaining budget can still fit later.
                    if let Some(text) = engram::agenthooks::render_file_shape(
                        &engram::agenthooks::FileBrief {
                            file_path: &relative,
                            symbols: &symbols,
                            callers: &callers,
                            imports: &shape.imports,
                            size_bytes: shape.size_bytes,
                        },
                        config.context.large_file_bytes,
                        byte_limit,
                    ) {
                        if text.len() <= byte_limit.saturating_sub(pretool_context.len())
                            && engram::agenthooks::mark_file_briefed(
                                &state_dir,
                                &delivery_scope,
                                &relative,
                            )
                        {
                            pretool_context.push_str(&text);
                        }
                    }
                }
            }
            if !pretool_context.trim().is_empty() && emit("PreToolUse", &pretool_context) {
                for (claim, request_id, run_id, digest, task_id) in deliveries {
                    let _ = claim.delivered_digest(&request_id, &run_id, &digest);
                    let _ = block_on(client.context_delivered(
                        &engram::daemon::ContextDeliveryRequest { run_id, task_id },
                    ));
                }
            }
        }
        "stop" => {
            capture_hook_transcript_usage(&client, &state_dir, &session_id, payload, agent);
        }
        "session-end" => {
            capture_hook_transcript_usage(&client, &state_dir, &session_id, payload, agent);
            for (scope, session) in engram::agenthooks::project_sessions(&state_dir, &session_id) {
                let outcome =
                    match engram::agenthooks::auto_close_outcome(&session.verification_results) {
                        "success" => engram::experience::TaskOutcome::Success,
                        "failure" => engram::experience::TaskOutcome::Failure,
                        _ => engram::experience::TaskOutcome::Abandoned,
                    };
                let _ = block_on(client.finish_task(
                    &session.task_id,
                    &engram::daemon::TaskFinishRequest {
                        outcome,
                        accepted_by_user: None,
                        final_commit: None,
                    },
                ));
                let _ = std::fs::remove_file(engram::agenthooks::session_state_path(
                    &state_dir, &scope,
                ));
                let _ = std::fs::remove_file(engram::agenthooks::session_files_path(
                    &state_dir, &scope,
                ));
                let _ = std::fs::remove_file(
                    engram::agenthooks::session_state_path(&state_dir, &scope)
                        .with_extension("context-run"),
                );
                for extension in ["context-delivery", "recent-paths"] {
                    let _ = std::fs::remove_file(
                        engram::agenthooks::session_state_path(&state_dir, &scope)
                            .with_extension(extension),
                    );
                }
                engram::agenthooks::clear_claims(&state_dir, &scope);
                engram::agenthooks::clear_thread_scopes(&state_dir, &scope);
            }
            let _ = std::fs::remove_file(
                engram::agenthooks::session_state_path(&state_dir, &session_id)
                    .with_extension("pending"),
            );
            engram::agenthooks::clear_claims(&state_dir, &session_id);
            engram::agenthooks::clear_task_declarations(&state_dir, &session_id);
        }
        "bash" => {
            // Write-ahead before any daemon call can fail or time out: a
            // recognized verification's result is unknown until its evidence
            // is recorded, so an older pass cannot close the episode.
            // When user- and project-level hooks both handle this call, only the
            // one that claims it touches its verification state.
            let pending = engram::agenthooks::pending_verification(payload, &cwd).filter(|_| {
                engram::agenthooks::claim_verification(&state_dir, &session_id, payload)
            });
            if let Some(pending) = &pending {
                engram::agenthooks::mark_verification_unknown(&state_dir, &session_id, pending);
            }
            let mut primary = None;
            for (resolved, paths) in hook_tool_projects(&client, &target_dir, &tool_paths) {
                let Some((scope, session)) = hook_episode(
                    &client,
                    &state_dir,
                    &session_id,
                    &resolved.resolved.project.id,
                ) else {
                    continue;
                };
                // PostToolUse can be the only observed event for an edit.
                let _ = engram::agenthooks::remember_paths(&state_dir, &scope, &paths);
                record_hook_tool_observation(
                    &client,
                    &state_dir,
                    &scope,
                    &session,
                    payload,
                    &paths,
                    engram::effectiveness::post_tool_phase(payload),
                    agent,
                );
                if primary.is_none() {
                    primary = Some((scope, session, resolved));
                }
            }
            let Some((scope, session, resolved)) = primary else {
                return Ok(true);
            };
            // Declarations and evidence come before the usage write, which
            // waits behind every higher-priority write and may outlast the hook.
            if let Some(declaration) = engram::agenthooks::task_declaration(payload) {
                // Engram MCP task calls name the task later verification belongs to.
                let _ = engram::agenthooks::record_task_declaration(
                    &state_dir,
                    &session_id,
                    &scope,
                    &declaration,
                );
            } else if let Some(pending) = &pending {
                // Only shell verification results can become deterministic evidence.
                record_hook_verification(
                    &client,
                    &state_dir,
                    &scope,
                    session.clone(),
                    &resolved,
                    payload,
                    agent,
                    pending,
                );
                // A hidden exit status recorded nothing: say so once, then
                // count the run for the effectiveness report.
                let noted = deliver_status_note(&state_dir, &session_id, payload, pending, emit);
                record_hook_verification_observation(
                    &client, &session, payload, agent, pending, noted,
                );
            }
            // All tools contribute actual call counts. A multi-repository patch
            // is still one tool call: attribute its receipt once to the first
            // tracked target, never once per file.
            record_hook_tool_usage(&client, &state_dir, &scope, &session, payload, agent);
        }
        _ => {}
    }
    Ok(true)
}

/// Record a shell verification on the declared task or the session's episode,
/// then set its result. The write-ahead already marked the result unknown, so
/// an unobserved status or a failed record leaves it unknown.
#[allow(clippy::too_many_arguments)]
fn record_hook_verification(
    client: &engram::daemon::DaemonClient,
    state_dir: &Path,
    scope: &str,
    mut session: engram::agenthooks::HookSession,
    resolved: &ProjectStatus,
    payload: &serde_json::Value,
    agent: engram::agenthooks::HookClient,
    pending: &engram::agenthooks::PendingVerification,
) {
    session.root = Some(resolved.resolved.root.clone());
    // Only a harness-reported status that belongs to the verification itself
    // is recorded; pipes and `||` hide it. The record carries digests of the
    // session's touched and dirty files.
    let session_paths = engram::agenthooks::recent_paths(state_dir, scope);
    let Some(observed) = engram::agenthooks::observe_verification(
        payload,
        agent.evidence_source(),
        &resolved.resolved.root,
        resolved.resolved.git.as_ref(),
        &session_paths,
    ) else {
        // Hidden by the shell, interrupted, or failed without an exit code:
        // the result is unknown and neither an older pass nor the duplicate
        // check may stand in for it.
        session.set_verification_result(&pending.key, pending.tests, None);
        session.forget_last_result();
        let _ = engram::agenthooks::save_session(state_dir, scope, &session);
        return;
    };
    // A hook fired twice for one tool call restores the recorded result.
    if session.last_evidence_fingerprint.as_deref() != Some(observed.fingerprint.as_str()) {
        let record = |task_id: &engram::ids::TaskId| {
            block_on(
                client.record_hook_evidence(&engram::daemon::HookEvidenceRequest {
                    evidence: EvidenceCreateRequest {
                        project_id: session.project_id.clone(),
                        task_id: Some(task_id.clone()),
                        attempt_id: None,
                        kind: observed.kind.to_owned(),
                        strength: EvidenceStrength::Deterministic,
                        commit_hash: None,
                        command: Some(observed.command.clone()),
                        exit_code: Some(observed.exit_code),
                        artifact_text: observed.artifact.clone(),
                        metadata: observed.metadata.clone(),
                    },
                    provenance: observed.provenance.clone(),
                }),
            )
            .is_ok()
        };
        // Prefer the open task the agent declared in this session.
        let declared = open_declared_task(client, state_dir, scope, &session);
        if !declared.as_ref().is_some_and(&record) {
            if !record(&session.task_id) {
                return;
            }
            // A declared task that refuses evidence this project's episode
            // accepts (another project, or gone) is not this scope's task.
            if let Some(declared) = &declared {
                engram::agenthooks::drop_task_declaration(state_dir, scope, declared);
            }
        }
    }
    session.set_verification_result(&pending.key, pending.tests, Some(observed.exit_code));
    session.last_verification_exit = Some(observed.exit_code);
    session.last_evidence_fingerprint = Some(observed.fingerprint);
    let _ = engram::agenthooks::save_session(state_dir, scope, &session);
}

/// Coach the agent when the shell hid a recognized verification's exit
/// status: one short reference-data note in this hook's own output, at most
/// once per conversation (the session, or a subagent thread in it) and cause,
/// never in brain-off mode. Returns whether the note was delivered; an
/// undelivered note stays available for a later run.
fn deliver_status_note(
    state_dir: &Path,
    session_id: &str,
    payload: &serde_json::Value,
    pending: &engram::agenthooks::PendingVerification,
    emit: &dyn Fn(&str, &str) -> bool,
) -> bool {
    let Some(reason) = pending.hidden else {
        return false;
    };
    if !engram::agenthooks::status_notes_enabled() || session_id == "unknown-session" {
        return false;
    }
    let conversation = engram::agenthooks::thread_scope(session_id, payload);
    if !engram::agenthooks::claim_status_note(state_dir, &conversation, reason) {
        return false;
    }
    let input = payload
        .get("tool_input")
        .unwrap_or(&serde_json::Value::Null);
    let note = engram::agenthooks::status_note(engram::agenthooks::tool_command(input), reason);
    // The note answers the event that reported the call.
    let event = match payload
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
    {
        Some("PostToolUseFailure") => "PostToolUseFailure",
        _ => "PostToolUse",
    };
    let delivered = emit(event, &note);
    if !delivered {
        engram::agenthooks::release_status_note(state_dir, &conversation, reason);
    }
    delivered
}

/// Count one recognized verification run for `engram effectiveness`: its
/// kind, whether its exit status could be recorded, the cause when not, and
/// whether a note went out. Never the command or its output. Calls without
/// agent-reported session and tool-use IDs are not counted.
fn record_hook_verification_observation(
    client: &engram::daemon::DaemonClient,
    session: &engram::agenthooks::HookSession,
    payload: &serde_json::Value,
    agent: engram::agenthooks::HookClient,
    pending: &engram::agenthooks::PendingVerification,
    note_delivered: bool,
) {
    // The daemon acknowledges once the write is queued; bound the wait anyway
    // so measurement never holds up the agent. A dropped row fails open.
    const OBSERVATION_WAIT: Duration = Duration::from_millis(500);
    let Some(tool_use_id) = payload
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(session_id) = session
        .session_id
        .as_ref()
        .filter(|id| id.as_str() != "unknown-session")
    else {
        return;
    };
    let request = engram::effectiveness::VerificationObservationRequest {
        project_id: session.project_id.clone(),
        session_id: session_id.clone(),
        tool_use_id: tool_use_id.to_owned(),
        conversation_id: engram::agenthooks::thread_scope(session_id, payload),
        task_id: Some(session.task_id.clone()),
        source: agent.usage_source().to_owned(),
        kind: pending.kind.to_owned(),
        hidden: pending.hidden,
        note_delivered,
    };
    let _ = block_on(async {
        tokio::time::timeout(
            OBSERVATION_WAIT,
            client.record_verification_observation(&request),
        )
        .await
        .map_err(|_| engram::Error::Daemon("verification observation timed out".to_owned()))?
    });
}

/// The task the agent declared in this session for this scope, while the
/// daemon reports it open. A declaration whose task was closed elsewhere (CLI,
/// web UI) is dropped; an unreadable status routes to the session's episode.
fn open_declared_task(
    client: &engram::daemon::DaemonClient,
    state_dir: &Path,
    scope: &str,
    session: &engram::agenthooks::HookSession,
) -> Option<engram::ids::TaskId> {
    let declared = engram::agenthooks::declared_task(state_dir, scope)
        .filter(|declared| *declared != session.task_id)?;
    let signals = block_on(client.product_query(&engram::product::Query::TaskSignals {
        task_id: declared.clone(),
    }))
    .ok()?;
    match signals.get("status").and_then(serde_json::Value::as_str) {
        Some("open") => Some(declared),
        Some(_) => {
            engram::agenthooks::drop_task_declaration(state_dir, scope, &declared);
            None
        }
        None => None,
    }
}

/// Route already resolved tool paths once and retain each repository's actual paths.
fn hook_tool_projects(
    client: &engram::daemon::DaemonClient,
    target_dir: &Path,
    tool_paths: &[PathBuf],
) -> Vec<(ProjectStatus, Vec<String>)> {
    let mut projects: Vec<(ProjectStatus, Vec<String>)> = Vec::new();
    let targets = if tool_paths.is_empty() {
        vec![(target_dir.to_path_buf(), None)]
    } else {
        tool_paths
            .iter()
            .map(|path| (engram::agenthooks::tool_path_directory(path), Some(path)))
            .collect()
    };
    for (directory, path) in targets {
        let Ok(lookup) = block_on(client.lookup_project(&directory)) else {
            continue;
        };
        let Some(project) = lookup.project else {
            continue;
        };
        let index = if let Some(index) = projects
            .iter()
            .position(|(resolved, _)| resolved.resolved.project.id == project.id)
        {
            index
        } else {
            let Ok(resolved) = block_on(client.resolve_project(&directory)) else {
                continue;
            };
            projects.push((resolved, Vec::new()));
            projects.len() - 1
        };
        let (resolved, paths) = &mut projects[index];
        if let Some(relative) =
            path.and_then(|path| path.strip_prefix(&resolved.resolved.root).ok())
        {
            let relative = relative.to_string_lossy().into_owned();
            if !paths.contains(&relative) {
                paths.push(relative);
            }
        }
    }
    projects
}

/// Tool call identity must come from the agent. Commands and timestamps are
/// not unique call ids and cannot support truthful, idempotent call counts.
fn record_hook_tool_usage(
    client: &engram::daemon::DaemonClient,
    state_dir: &Path,
    scope: &str,
    session: &engram::agenthooks::HookSession,
    payload: &serde_json::Value,
    agent: engram::agenthooks::HookClient,
) {
    let Some(tool_id) = payload
        .get("tool_use_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(session_id) = session
        .session_id
        .as_ref()
        .filter(|id| id.as_str() != "unknown-session")
    else {
        return;
    };
    let context_run_id = engram::agenthooks::context_run_id(state_dir, scope);
    let request = engram::effectiveness::UsageRecordRequest {
        sample_id: engram::agenthooks::tool_usage_sample_id(session_id, tool_id),
        project_id: session.project_id.clone(),
        task_id: Some(session.task_id.clone()),
        mode: context_run_id
            .as_ref()
            .map(|_| engram::effectiveness::UsageMode::Brain),
        context_run_id,
        session_id: Some(session_id.clone()),
        source: agent.usage_source().to_owned(),
        provider: None,
        model: None,
        metrics: engram::effectiveness::UsageMetrics {
            tool_calls: Some(1),
            ..engram::effectiveness::UsageMetrics::default()
        },
        observed_at_ms: None,
    };
    // The analytics write waits behind every higher-priority write. Bound the
    // wait so the hook stays within its client time limit; usage is best effort.
    let _ = block_on(async {
        tokio::time::timeout(HOOK_USAGE_WAIT, client.record_usage(&request))
            .await
            .map_err(|_| engram::Error::Daemon("tool usage write timed out".to_owned()))?
    });
}

/// Longest wait for a hook's best-effort tool-usage write.
const HOOK_USAGE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// Observe one tool call for context-use attribution: action, repository-relative
/// paths and Read flags/sizes only, never content, commands or error text.
#[allow(clippy::too_many_arguments)]
fn record_hook_tool_observation(
    client: &engram::daemon::DaemonClient,
    state_dir: &Path,
    scope: &str,
    session: &engram::agenthooks::HookSession,
    payload: &serde_json::Value,
    paths: &[String],
    phase: engram::effectiveness::ToolPhase,
    agent: engram::agenthooks::HookClient,
) {
    // The daemon acknowledges once the write is queued. Bound the wait anyway:
    // the pre-tool hook compiles context after this call, and measurement must
    // never cost the agent its context. A dropped observation fails open.
    const OBSERVATION_WAIT: Duration = Duration::from_millis(500);
    let context_run_id = engram::agenthooks::context_run_id(state_dir, scope);
    if let Some(request) = engram::effectiveness::hook_tool_observation(
        payload,
        session,
        context_run_id,
        paths,
        phase,
        agent,
    ) {
        let _ = block_on(async {
            tokio::time::timeout(OBSERVATION_WAIT, client.record_tool_observation(&request))
                .await
                .map_err(|_| engram::Error::Daemon("tool observation timed out".to_owned()))?
        });
    }
}

/// A resumed session starts with existing transcript samples already consumed.
/// Missing transcript paths remain uninitialized until the first usable Stop;
/// that Stop establishes a baseline and does not guess historical attribution.
fn initialize_hook_usage_baseline(state_dir: &Path, session_id: &str, payload: &serde_json::Value) {
    if session_id == "unknown-session" {
        return;
    }
    let Some(path) = payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let samples = match engram::usage_capture::read_transcript(Path::new(path)) {
        Ok(samples) => samples,
        Err(engram::Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
            Vec::new()
        }
        Err(_) => return,
    };
    let scope = engram::agenthooks::usage_session_scope(session_id);
    let Ok(receipts) = engram::agenthooks::usage_receipts(state_dir, &scope) else {
        return;
    };
    let mut receipts = receipts.unwrap_or_default();
    receipts.extend(samples.into_iter().map(|sample| sample.sample_id));
    let _ = engram::agenthooks::save_usage_receipts(state_dir, &scope, &receipts);
}

/// Attribute only usage observed after this workspace's session baseline.
/// Ambiguous project intervals are consumed without inventing an allocation.
fn capture_hook_transcript_usage(
    client: &engram::daemon::DaemonClient,
    state_dir: &Path,
    session_id: &str,
    payload: &serde_json::Value,
    agent: engram::agenthooks::HookClient,
) {
    if session_id == "unknown-session" {
        return;
    }
    let Some(path) = payload
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let Ok(samples) = engram::usage_capture::read_transcript(Path::new(path)) else {
        return;
    };
    let usage_scope = engram::agenthooks::usage_session_scope(session_id);
    let Ok(previous) = engram::agenthooks::usage_receipts(state_dir, &usage_scope) else {
        return;
    };
    let sessions = engram::agenthooks::project_sessions(state_dir, session_id);
    let initialized = previous.is_some();
    let mut receipts = previous.unwrap_or_default();
    if !initialized || sessions.len() != 1 {
        receipts.extend(samples.into_iter().map(|sample| sample.sample_id));
        let _ = engram::agenthooks::save_usage_receipts(state_dir, &usage_scope, &receipts);
        return;
    }
    let session = &sessions[0].1;
    // Keep the hook bounded if the daemon becomes unavailable. Successful
    // receipts persist so a later Stop/SessionEnd resumes outstanding samples.
    let _ = block_on(async {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            for sample in samples {
                if receipts.contains(&sample.sample_id) {
                    continue;
                }
                let sample_id = sample.sample_id.clone();
                if client
                    .record_usage(&engram::effectiveness::UsageRecordRequest {
                        sample_id: sample.sample_id,
                        project_id: session.project_id.clone(),
                        task_id: Some(session.task_id.clone()),
                        // The baseline proves when we began observing; it does
                        // not identify which injected context this model used.
                        mode: None,
                        context_run_id: None,
                        session_id: Some(session_id.to_owned()),
                        source: agent.usage_source().to_owned(),
                        provider: sample.provider,
                        model: sample.model,
                        metrics: sample.metrics,
                        observed_at_ms: None,
                    })
                    .await
                    .is_ok()
                {
                    receipts.insert(sample_id);
                }
            }
        })
        .await;
        Ok(())
    });
    let _ = engram::agenthooks::save_usage_receipts(state_dir, &usage_scope, &receipts);
}

/// Lazily open one episode per session/project, only after a tool touches it.
fn hook_episode(
    client: &engram::daemon::DaemonClient,
    state_dir: &Path,
    session_id: &str,
    project_id: &engram::ids::ProjectId,
) -> Option<(String, engram::agenthooks::HookSession)> {
    let scope = engram::agenthooks::scoped_session_id(session_id, project_id);
    if let Some(session) = engram::agenthooks::load_session(state_dir, &scope) {
        return Some((scope, session));
    }
    let prompt = engram::agenthooks::pending_prompt(state_dir, session_id)?;
    if !engram::agenthooks::claim_once(state_dir, &scope, "episode") {
        // A concurrent hook for this session (user- and project-level hooks
        // run together) is creating the episode. Wait briefly for it, so this
        // call's capture is not dropped; give up if the creator failed.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
            if let Some(session) = engram::agenthooks::load_session(state_dir, &scope) {
                return Some((scope, session));
            }
        }
        return None;
    }
    let Ok(task) = block_on(
        client.start_task(&TaskStartRequest {
            context_request_id: engram::agenthooks::context_request_id(state_dir, &scope)
                .or_else(|| Some(format!("hook_{session_id}"))),
            project_id: project_id.clone(),
            prompt,
            session_id: Some(session_id.to_owned()),
        }),
    ) else {
        engram::agenthooks::clear_claims(state_dir, &scope);
        return None;
    };
    let session = engram::agenthooks::HookSession {
        task_id: task.id,
        session_id: Some(session_id.to_owned()),
        project_id: project_id.clone(),
        last_verification_exit: None,
        last_evidence_fingerprint: None,
        root: None,
        verification_results: Vec::new(),
    };
    engram::agenthooks::save_session(state_dir, &scope, &session).ok()?;
    Some((scope, session))
}

/// Extract a section's text: from its heading until the next heading of the
/// same or higher level, bounded.
fn section_excerpt(content: &str, start_byte: usize) -> String {
    let Some(section) = content.get(start_byte..) else {
        return String::new();
    };
    let level = section.chars().take_while(|c| *c == '#').count().max(1);
    let mut end = section.len();
    let mut offset = 0_usize;
    for line in section.lines().skip(1) {
        offset += 1; // newline of the previous line
        if line.starts_with('#') && line.chars().take_while(|c| *c == '#').count() <= level {
            end = section[..end]
                .find(line)
                .map_or(end, |position| position.min(end));
            break;
        }
        offset += line.len();
        if offset > 1_500 {
            end = end.min(offset);
            break;
        }
    }
    let mut excerpt = section[..end.min(section.len())].to_owned();
    excerpt.truncate(1_500);
    excerpt
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn run_ask(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    question: &str,
    path: &Path,
    claude: bool,
    model: Option<&str>,
    dry_run: bool,
    budget: u32,
) -> Result<bool> {
    use std::fmt::Write as _;
    let config = load_daemon_config(overrides, socket, Some(path))?;
    let client = client(&config);
    let status = block_on(client.resolve_project(path))?;
    let project_id = status.resolved.project.id.clone();
    let root = status.resolved.root.clone();

    let context = block_on(
        client.compile_context(&ContextCompileRequest {
            request_id: format!("ask_{}", uuid::Uuid::new_v4()),
            project_id: project_id.clone(),
            task_text: question.to_owned(),
            current_paths: Vec::new(),
            token_budget: budget,
            mode: ContextMode::Brain,
            repository_root: Some(root.clone()),
            head: status
                .resolved
                .git
                .as_ref()
                .and_then(|git| git.head.clone()),
            task_id: None,
            framing: None,
        }),
    )?;

    // Retrieve graph matter for the question's salient words: code symbols
    // ground the answer in real definitions, Section hits pull in the
    // documentation text itself.
    let mut seen = std::collections::BTreeSet::new();
    let keywords: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.')
        .filter(|word| word.len() >= 4)
        .map(str::to_lowercase)
        .filter(|word| seen.insert(word.clone()))
        .take(6)
        .collect();
    let mut symbols = Vec::new();
    let mut sections = Vec::new();
    for keyword in &keywords {
        let Ok(nodes) = block_on(client.search_world(&engram::daemon::WorldSearchRequest {
            project_id: project_id.clone(),
            query: keyword.clone(),
            limit: 8,
        })) else {
            continue;
        };
        for node in nodes {
            if node.kind == "Section" {
                sections.push(node);
            } else if node.kind != "ExternalReference" && node.kind != "File" {
                // Production symbols answer questions; test fixtures bury them.
                let is_test = node.file_path.as_deref().is_some_and(|path| {
                    path.split('/').any(|segment| {
                        segment == "test" || segment == "tests" || segment.starts_with("test")
                    }) || path.contains("Test.")
                });
                if !is_test {
                    symbols.push(node);
                }
            }
        }
    }
    sections.truncate(5);
    symbols.truncate(12);

    let mut pack = format!(
        "# Question\n{question}\n\n# Workspace knowledge (validated)\n{}{}\n",
        context.stable_prefix, context.dynamic_context
    );
    if !symbols.is_empty() {
        pack.push_str("\n# Matching code symbols\n");
        for node in &symbols {
            let file = node.file_path.as_deref().unwrap_or("?");
            let _ = writeln!(pack, "- {} ({}) — {}", node.label, node.kind, file);
        }
    }
    if !sections.is_empty() {
        pack.push_str("\n# Documentation excerpts\n");
        for node in &sections {
            let Some(file) = node.file_path.as_deref() else {
                continue;
            };
            let Ok(document) = std::fs::read_to_string(root.join(file)) else {
                continue;
            };
            let start = usize::try_from(node.start_byte.unwrap_or(0)).unwrap_or(0);
            let excerpt = section_excerpt(&document, start);
            if !excerpt.trim().is_empty() {
                let _ = writeln!(pack, "\n## {file} § {}\n{excerpt}", node.label);
            }
        }
    }

    if dry_run || (!claude && model.is_none()) {
        println!("{pack}");
        if !dry_run {
            println!(
                "\n(no model selected — pass --claude or --model <m> for a synthesized answer)"
            );
        }
        return Ok(true);
    }
    let prompt = format!(
        "Answer the question strictly from the workspace evidence below. Cite the \
         file or memory each claim comes from. If the evidence does not answer the \
         question, say exactly what is missing — do not guess.\n\n{pack}"
    );
    let answer = if claude {
        let model = model.unwrap_or("claude-opus-4-8");
        engram::architect::synthesize_with_claude_cli(model, &prompt)?
    } else {
        let model = model.expect("checked above");
        engram::architect::enforce_local_only(
            &config.architect.runtime_url,
            config.network.local_only,
        )?;
        block_on(engram::architect::synthesize(
            &config.architect.runtime_url,
            model,
            &prompt,
        ))?
    };
    println!("{answer}");
    Ok(true)
}

#[allow(clippy::too_many_lines)]
fn run_relay_action(overrides: &ConfigOverrides, action: &RelayAction) -> Result<bool> {
    let config = Config::load_for_current_dir(overrides)?;
    let state_dir = config
        .storage
        .db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    match action {
        RelayAction::Send { to, message, from } => {
            let recipients: Vec<String> = if to == "all" {
                engram::agenthooks::active_sessions(&state_dir)
                    .into_iter()
                    .map(|(id, session, _)| session.session_id.unwrap_or(id))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect()
            } else {
                vec![to.clone()]
            };
            if recipients.is_empty() {
                println!("No active sessions.");
                return Ok(true);
            }
            for recipient in &recipients {
                engram::agenthooks::relay_send(&state_dir, recipient, from, message)?;
            }
            println!(
                "Queued for {} session(s); delivery at each recipient's next prompt.",
                recipients.len()
            );
        }
        RelayAction::Board => {
            let sessions = engram::agenthooks::active_sessions(&state_dir);
            if sessions.is_empty() {
                println!("No active sessions.");
            }
            for (id, session, modified) in sessions {
                let id = session.session_id.as_deref().unwrap_or(&id);
                let age_minutes = modified
                    .elapsed()
                    .map(|elapsed| elapsed.as_secs() / 60)
                    .unwrap_or_default();
                println!(
                    "{id}  project={}  task={}  active {age_minutes}m ago",
                    session.project_id, session.task_id
                );
            }
        }
    }
    Ok(true)
}

fn run_update(check: bool) -> Result<bool> {
    let manifest_path = dirs_home()
        .join(".local/share/engram")
        .join("install-manifest.json");
    let manifest = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .ok_or_else(|| {
            engram::error::Error::Config(format!(
                "no install manifest at {} — run scripts/install.sh once first",
                manifest_path.display()
            ))
        })?;
    let Some(source_dir) = manifest
        .get("source_dir")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .filter(|dir| dir.join("scripts/install.sh").is_file())
    else {
        return Err(engram::error::Error::Config(
            "install manifest predates `engram update` (no source_dir) — \
             re-run scripts/install.sh from the Engram checkout once, then \
             `engram update` works from anywhere"
                .to_owned(),
        ));
    };
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&source_dir)
            .args(args)
            .output()
            .map_err(|error| engram::error::Error::Config(format!("git: {error}")))
    };
    let fetch = git(&["fetch", "--quiet"])?;
    if !fetch.status.success() {
        return Err(engram::error::Error::Config(format!(
            "git fetch in {} failed: {}",
            source_dir.display(),
            String::from_utf8_lossy(&fetch.stderr).trim()
        )));
    }
    let behind = git(&["rev-list", "--count", "HEAD..@{upstream}"])?;
    let behind_count = String::from_utf8_lossy(&behind.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0);
    if check {
        if behind_count == 0 {
            println!("Up to date (source checkout matches upstream).");
        } else {
            println!("Update available: {behind_count} commit(s) behind. Run `engram update`.");
        }
        return Ok(true);
    }
    if behind_count > 0 {
        let pull = git(&["pull", "--ff-only", "--quiet"])?;
        if !pull.status.success() {
            return Err(engram::error::Error::Config(format!(
                "git pull --ff-only failed (local changes or divergence): {}",
                String::from_utf8_lossy(&pull.stderr).trim()
            )));
        }
        println!("Pulled {behind_count} commit(s).");
    } else {
        println!("Source already current; rebuilding to match.");
    }
    let status = std::process::Command::new("sh")
        .arg(source_dir.join("scripts/install.sh"))
        .status()
        .map_err(|error| engram::error::Error::Config(format!("installer: {error}")))?;
    if !status.success() {
        return Err(engram::error::Error::Config(
            "installer failed — the previous release remains active".to_owned(),
        ));
    }
    println!(
        "Updated. The installer stopped the previous daemon; the next CLI call starts \
         the new binary. Running sessions keep the binary they started with."
    );
    Ok(true)
}

fn install_pack_everywhere(pack: &engram::skillpacks::SkillPack, home: &Path) -> Result<()> {
    let targets = engram::skillpacks::detect_targets(home);
    if targets.is_empty() {
        println!("No agent installations detected (~/.claude or ~/.codex).");
        return Ok(());
    }
    for target in &targets {
        let outcomes = engram::skillpacks::install_into(pack, target)?;
        for (skill, outcome) in outcomes {
            let verb = match outcome {
                engram::skillpacks::InstallOutcome::Installed => "installed",
                engram::skillpacks::InstallOutcome::Updated => "updated",
                engram::skillpacks::InstallOutcome::SkippedForeign => {
                    "SKIPPED (a non-Engram skill of this name already exists)"
                }
            };
            println!("  {}: {skill} {verb}", target.label);
        }
    }
    Ok(())
}

fn run_skills_action(action: &SkillsAction) -> Result<bool> {
    let home = dirs_home();
    match action {
        SkillsAction::Add { source, name } => {
            let pack = engram::skillpacks::add(&home, source, name.as_deref())?;
            let skills = engram::skillpacks::discover_skills(&pack.path)?;
            println!(
                "Fetched pack '{}' ({} skill{}).",
                pack.name,
                skills.len(),
                if skills.len() == 1 { "" } else { "s" }
            );
            install_pack_everywhere(&pack, &home)?;
            println!(
                "Skills trigger natively when relevant; `engram skills update` refreshes them."
            );
        }
        SkillsAction::List => {
            let packs = engram::skillpacks::list(&home)?;
            if packs.is_empty() {
                println!("No skill packs. `engram skills add <git-url>` fetches one.");
            }
            for pack in packs {
                let source = pack.source.clone().unwrap_or_else(|| "local".to_owned());
                println!("{} ({source})", pack.name);
                for skill in engram::skillpacks::discover_skills(&pack.path)? {
                    let description: String = skill.description.chars().take(96).collect();
                    println!("  {} — {description}", skill.name);
                }
            }
        }
        SkillsAction::Update => {
            for pack in engram::skillpacks::list(&home)? {
                engram::skillpacks::update(&pack)?;
                println!("Pulled '{}'.", pack.name);
                install_pack_everywhere(&pack, &home)?;
            }
        }
        SkillsAction::Install => {
            for pack in engram::skillpacks::list(&home)? {
                println!("Pack '{}':", pack.name);
                install_pack_everywhere(&pack, &home)?;
            }
        }
        SkillsAction::Remove { name } => {
            let packs = engram::skillpacks::list(&home)?;
            let Some(pack) = packs.into_iter().find(|pack| &pack.name == name) else {
                return Err(engram::error::Error::Config(format!(
                    "no skill pack named '{name}'"
                )));
            };
            for target in engram::skillpacks::detect_targets(&home) {
                for skill in engram::skillpacks::uninstall_from(&pack.name, &target)? {
                    println!("  {}: {skill} removed", target.label);
                }
            }
            std::fs::remove_dir_all(&pack.path)
                .map_err(|error| engram::error::Error::Config(format!("remove pack: {error}")))?;
            println!("Removed pack '{name}'.");
        }
    }
    Ok(true)
}

fn native_hook_executable() -> Result<PathBuf> {
    let stable = dirs_home().join(".local/bin/engram");
    if stable.is_file() {
        Ok(stable)
    } else {
        std::env::current_exe().map_err(|e| engram::Error::Config(e.to_string()))
    }
}

fn native_hook_command() -> Result<String> {
    let executable = native_hook_executable()?;
    let binary = format!("'{}'", executable.to_string_lossy().replace('\'', "'\\''"));
    Ok(binary)
}

/// Refuse to point Cursor at a binary without the Cursor adapter. An older
/// engram rejects `--cursor` with exit code 2, which Cursor treats as a block.
/// With empty input the adapter only prints its default prompt response.
fn verify_cursor_adapter(executable: &Path) -> Result<()> {
    let output = std::process::Command::new(executable)
        .args(["hook", "prompt", "--cursor"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|error| engram::Error::Config(format!("run {}: {error}", executable.display())))?;
    let response = serde_json::from_slice::<serde_json::Value>(&output.stdout).ok();
    if output.status.success() && response == Some(serde_json::json!({"continue": true})) {
        return Ok(());
    }
    Err(engram::Error::Config(format!(
        "{} does not support Cursor hooks; install this Engram build first, then rerun \
         `engram hooks cursor --install`",
        executable.display()
    )))
}

fn install_codex_hooks() -> Result<()> {
    use std::io::Write as _;
    let config_dir =
        std::env::var_os("CODEX_HOME").map_or_else(|| dirs_home().join(".codex"), PathBuf::from);
    let hooks_path = config_dir.join("hooks.json");
    let document = match std::fs::read_to_string(&hooks_path) {
        Ok(content) => serde_json::from_str(&content)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(engram::Error::Config(format!("read Codex hooks: {error}"))),
    };
    let binary = native_hook_command()?;
    let document = engram::agenthooks::merge_codex_hooks(document, &binary)?;
    std::fs::create_dir_all(&config_dir).map_err(|e| engram::Error::Config(e.to_string()))?;
    let mut file = tempfile::NamedTempFile::new_in(&config_dir)
        .map_err(|e| engram::Error::Config(e.to_string()))?;
    file.write_all(serde_json::to_string_pretty(&document)?.as_bytes())
        .map_err(|e| engram::Error::Config(e.to_string()))?;
    file.persist(&hooks_path)
        .map_err(|e| engram::Error::Config(e.to_string()))?;
    println!(
        "Wrote {}. Open /hooks in Codex to review and trust these definitions, then start a new local session. Hooks do not change your model or provider.",
        hooks_path.display()
    );
    Ok(())
}

fn run_hooks_action(action: &HooksAction) -> Result<bool> {
    use std::io::Write as _;
    let binary = native_hook_command()?;
    let hooks = engram::agenthooks::claude_hooks_json(&binary);
    match action {
        HooksAction::Show => {
            println!("{}", serde_json::to_string_pretty(&hooks)?);
        }
        HooksAction::Install { repo, global } => {
            let settings_dir = if *global {
                dirs_home().join(".claude")
            } else {
                repo.join(".claude")
            };
            std::fs::create_dir_all(&settings_dir)
                .map_err(|error| engram::Error::Config(format!("settings dir: {error}")))?;
            let settings_path = settings_dir.join("settings.json");
            let settings = match std::fs::read_to_string(&settings_path) {
                Ok(content) => serde_json::from_str(&content)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
                Err(error) => return Err(engram::Error::Config(error.to_string())),
            };
            let settings = engram::agenthooks::merge_claude_hooks(settings, &binary)?;
            let mut file = tempfile::NamedTempFile::new_in(&settings_dir)
                .map_err(|e| engram::Error::Config(e.to_string()))?;
            file.write_all(serde_json::to_string_pretty(&settings)?.as_bytes())
                .map_err(|e| engram::Error::Config(e.to_string()))?;
            file.persist(&settings_path)
                .map_err(|e| engram::Error::Config(e.to_string()))?;
            println!(
                "{}: Engram hooks installed/refreshed; other hooks preserved",
                settings_path.display()
            );
            if *global {
                println!(
                    "Universal: every Claude session in every REGISTERED repo now gets \
                     context injection and automatic evidence capture (unregistered \
                     repos are untouched). Pair with `engram agent-md --global`."
                );
            } else {
                println!(
                    "Context injection + automatic evidence capture are active for stock \
                     `claude` sessions in this repo. Also run `engram agent-md --install {}` \
                     so the agent knows the wider tool loop.",
                    repo.display()
                );
            }
        }
        HooksAction::Codex { install } => {
            if *install {
                install_codex_hooks()?;
            }
            println!(
                "Codex setup:\n\n1. Connect MCP once for the local CLI, app and IDE:\n\n\
                 codex mcp add engram -- /absolute/path/to/engram-mcp\n\n\
                 2. Per repo, install the agent loop instructions into AGENTS.md:\n\n\
                 engram agent-md --install <repo> --file AGENTS.md\n\n\
                 3. Automatic hooks (recommended): engram hooks codex --install\n\n\
                 Codex reads AGENTS.md automatically and reaches the brain through MCP\n\
                 (recall, workspace map, symbol search, episode recording)."
            );
        }
        HooksAction::Cursor(args) => run_cursor_setup(args)?,
    }
    Ok(true)
}

const CURSOR_SETUP: &str = "Cursor setup:

1. Hooks: session brief, repository context after file and shell tools,
   evidence and episode capture in Cursor's agent:

   engram hooks cursor --install

2. Optional MCP tools (recall, workspace map, episode recording):

   engram hooks cursor --mcp

Preview without writing:  engram hooks cursor --print --mcp
Remove Engram's entries:  engram hooks cursor --uninstall --mcp
Files live in $ENGRAM_CURSOR_HOME or ~/.cursor; --cursor-home DIR overrides both.";

/// Cursor integration files: Engram's hooks in hooks.json and, as a separate
/// opt-in, its MCP server in mcp.json. Only Engram's entries are touched.
fn run_cursor_setup(args: &CursorSetupArgs) -> Result<()> {
    use engram::agenthooks::cursor;
    let CursorSetupArgs {
        install,
        uninstall,
        print,
        mcp,
        ref cursor_home,
    } = *args;
    let home = cursor::cursor_home(
        cursor_home.as_deref(),
        std::env::var_os("ENGRAM_CURSOR_HOME").as_deref(),
        &dirs_home(),
    );
    let hooks_path = home.join("hooks.json");
    let mcp_path = home.join("mcp.json");
    if print {
        let mut fragments = serde_json::Map::new();
        fragments.insert(
            hooks_path.display().to_string(),
            cursor::cursor_hooks_json(&native_hook_command()?),
        );
        if mcp {
            fragments.insert(
                mcp_path.display().to_string(),
                serde_json::json!({"mcpServers": {
                    (cursor::MCP_SERVER_NAME): cursor::mcp_server_entry(&native_mcp_command()?)
                }}),
            );
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(fragments))?
        );
        return Ok(());
    }
    if !install && !uninstall && !mcp {
        println!("{CURSOR_SETUP}");
        return Ok(());
    }
    if install {
        verify_cursor_adapter(&native_hook_executable()?)?;
        let document = cursor::read_document(&hooks_path, serde_json::json!({}))?;
        let document = cursor::merge_cursor_hooks(document, &native_hook_command()?)?;
        cursor::write_document(&hooks_path, &document)?;
        println!(
            "Wrote {}: Engram handles sessionStart, beforeSubmitPrompt, postToolUse, \
             postToolUseFailure, stop and sessionEnd; other hooks are preserved. Cursor \
             reloads hooks.json on save; confirm under Customize > Hooks.\n\
             Cursor also runs Claude Code hooks from ~/.claude/settings.json by default. \
             Engram's Claude Code entries recognize Cursor input and stay silent, so each \
             event is captured once.",
            hooks_path.display()
        );
    }
    if uninstall {
        let document = cursor::read_document(&hooks_path, serde_json::json!({}))?;
        let (document, removed) = cursor::remove_cursor_hooks(document)?;
        if removed > 0 {
            cursor::write_document(&hooks_path, &document)?;
        }
        println!(
            "Removed {removed} Engram hook entr{} from {}; other hooks are unchanged.",
            if removed == 1 { "y" } else { "ies" },
            hooks_path.display()
        );
    }
    if mcp && uninstall {
        let document = cursor::read_document(&mcp_path, serde_json::json!({}))?;
        let (document, removed) = cursor::remove_cursor_mcp(document)?;
        if removed {
            cursor::write_document(&mcp_path, &document)?;
            println!(
                "Removed the `engram` MCP server from {}.",
                mcp_path.display()
            );
        } else {
            println!("No Engram MCP server in {}.", mcp_path.display());
        }
    } else if mcp {
        let document = cursor::read_document(&mcp_path, serde_json::json!({}))?;
        let (document, outcome) = cursor::merge_cursor_mcp(document, &native_mcp_command()?)?;
        if let cursor::McpRegistration::AlreadyRegistered(name) = outcome {
            println!(
                "`{name}` in {} already launches Engram's MCP server; left unchanged.",
                mcp_path.display()
            );
        } else {
            cursor::write_document(&mcp_path, &document)?;
            println!(
                "Registered engram-mcp as `{}` in {}; other servers are unchanged. Check it \
                 under Customize in Cursor.",
                cursor::MCP_SERVER_NAME,
                mcp_path.display()
            );
        }
    }
    Ok(())
}

/// Absolute path of the MCP stdio server for GUI clients without a login PATH.
fn native_mcp_command() -> Result<String> {
    let stable = dirs_home().join(".local/bin/engram-mcp");
    let candidate = if stable.is_file() {
        stable
    } else {
        std::env::current_exe()
            .map_err(|e| engram::Error::Config(e.to_string()))?
            .with_file_name("engram-mcp")
    };
    if !candidate.is_file() {
        return Err(engram::Error::Config(format!(
            "engram-mcp was not found at {}; install Engram first",
            candidate.display()
        )));
    }
    Ok(candidate.to_string_lossy().into_owned())
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
        .map_err(|source| engram::error::io_error_for_cli(path, source))?;
    let mut repos = Vec::new();
    if root.join(".git").exists() {
        repos.push(root.clone());
    } else {
        let mut entries = std::fs::read_dir(&root)
            .map_err(|source| engram::error::io_error_for_cli(&root, source))?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| candidate.is_dir() && candidate.join(".git").exists())
            .collect::<Vec<_>>();
        entries.sort();
        repos.extend(entries.into_iter().take(MAX_REPOS));
    }
    if repos.is_empty() {
        return Err(engram::Error::Config(format!(
            "no git repositories found under {}",
            root.display()
        )));
    }
    let mut config = Config::load_for_current_dir(overrides)?;
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
    let report = if config.vault.enabled {
        Some(block_on(patient.build_vault(
            &engram::daemon::VaultBuildRequest { project_id: None },
        ))?)
    } else {
        None
    };
    if claude {
        println!("Synthesizing the architecture guide via claude…");
        let dossier = block_on(patient.architect_dossier())?;
        let prompt = engram::architect::synthesis_prompt(&dossier);
        let synthesized =
            engram::architect::synthesize_with_claude_cli("claude-opus-4-8", &prompt)?;
        let document = engram::architect::guide_document("claude-opus-4-8", &dossier, &synthesized);
        save_product_guide(&patient, "claude-opus-4-8", &dossier, &document)?;
        if config.vault.enabled {
            std::fs::write(config.vault.root.join("ARCHITECTURE.md"), document)
                .map_err(|error| engram::Error::Config(format!("guide export: {error}")))?;
        }
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
            report
                .as_ref()
                .map_or("disabled (optional export)", |r| r.root.as_str()),
            config.web.port
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
    let mut config = Config::load_for_current_dir(overrides)?;
    apply_socket_override(&mut config, socket)?;
    let central_url = central.map_or_else(|| config.sync.central_url.clone(), str::to_owned);
    let bearer = token
        .map(str::to_owned)
        .or_else(|| config.sync.token.clone());
    if central_url.is_empty() {
        return Err(engram::Error::Config(
            "no central instance configured: set [sync] central_url or pass --central".to_owned(),
        ));
    }
    require_daemon(&config)?;
    let local = client(&config);
    let report = block_on(engram::sync::run_sync_cycle(
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
    println!("Engram workspace browser: http://127.0.0.1:{port}  (Ctrl-C to stop)");
    block_on(engram::web::serve(config, port))?;
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
    let report = block_on(patient.build_vault(&engram::daemon::VaultBuildRequest {
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
        .map_err(|error| engram::Error::Config(format!("git status: {error}")))?;
    if !porcelain.status.success() {
        return Err(engram::Error::Config(
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
        .map_err(|error| engram::Error::Config(format!("git pull: {error}")))?;
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
    let mut config = Config::load_for_current_dir(overrides)?;
    apply_socket_override(&mut config, socket)?;
    // `stop` must not require a build-matched daemon: after an upgrade the
    // whole point of stopping is that the socket serves an OLD build, and
    // refusing creates a chicken-and-egg lockout (observed). Status and
    // stop talk to whatever answers the socket; auto-start still applies
    // to status so it can report a freshly started daemon. Restart replaces
    // whatever answers, so it must not auto-start one first either.
    if !matches!(
        action,
        DaemonAction::Stop { .. } | DaemonAction::Restart { .. }
    ) {
        require_daemon(&config)?;
    }
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
                if let Some(pid) = health.pid {
                    println!("PID:       {pid}");
                }
                if let Some(launcher) = &health.launcher {
                    println!("Started by: {launcher}");
                }
            }
            Ok(true)
        }
        DaemonAction::Restart { wait_secs, json } => {
            let report = block_on(engram::autostart::restart_daemon(
                &config,
                Duration::from_secs(*wait_secs),
            ))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_restart_report(&report);
            }
            Ok(report.started_by_this_process)
        }
        DaemonAction::Stop { wait_secs, json } => {
            let report = block_on(engram::autostart::stop_daemon(
                &config,
                Duration::from_secs(*wait_secs),
            ))?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print_stop_report(&report, *wait_secs);
            }
            Ok(report.exited != Some(false))
        }
    }
}

fn print_restart_report(report: &engram::autostart::RestartReport) {
    match (report.stopped_previous, report.previous_pid) {
        (true, Some(previous)) => println!("Stopped engramd pid {previous}"),
        (true, None) => println!("Stopped the previous engramd"),
        (false, _) => println!("No daemon was running"),
    }
    if let Some(pid) = report.pid {
        println!("Started engramd pid {pid}");
    }
    if let Some(launcher) = &report.launcher {
        println!("Started by: {launcher}");
    }
    if let Some(displaced) = report.displaced_pid {
        println!(
            "Replaced engramd pid {displaced}, which another client had started while the old \
             daemon was exiting"
        );
    }
    if !report.started_by_this_process {
        println!(
            "Warning: another client started the daemon during the restart; it runs with that \
             client's folder access. Run `engram daemon restart` again if `engram doctor` still \
             reports unreadable checkouts."
        );
    }
}

fn print_stop_report(report: &engram::autostart::StopReport, wait_secs: u64) {
    let process = report
        .pid
        .map_or_else(|| "engramd".to_owned(), |pid| format!("engramd pid {pid}"));
    match (report.stopped, report.exited) {
        (false, _) => println!("No daemon is answering"),
        (true, None) => println!("Asked {process} to stop"),
        (true, Some(true)) if report.replaced => {
            println!("{process} stopped listening, and another daemon now serves the socket");
        }
        (true, Some(true)) => println!("{process} stopped after {} ms", report.waited_ms),
        (true, Some(false)) => println!(
            "{process} is still finishing in-flight work after {wait_secs} s; it exits on its own"
        ),
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

fn run_effectiveness(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    days: u32,
    json: bool,
) -> Result<bool> {
    let config = load_daemon_config(overrides, socket, None)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let report = block_on(client(&config).effectiveness(
        &engram::effectiveness::EffectivenessRequest {
            project_ids: None,
            since_ms: Some(now - i64::from(days) * 86_400_000),
            until_ms: None,
        },
    ))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("Workspace: {} · last {days} days", config.workspace);
        println!("Observed usage samples: {}", report.usage.samples);
        println!(
            "Input tokens: {} · output tokens: {} · tool calls: {} · reported cost: {}",
            report
                .usage
                .totals
                .input_tokens
                .map_or_else(|| "unknown".into(), |n| n.to_string()),
            report
                .usage
                .totals
                .output_tokens
                .map_or_else(|| "unknown".into(), |n| n.to_string()),
            report
                .usage
                .totals
                .tool_calls
                .map_or_else(|| "unknown".into(), |n| n.to_string()),
            report
                .usage
                .totals
                .cost_usd
                .map_or_else(|| "unknown".into(), |n| format!("${n:.4}"))
        );
        for line in report.verification_status.summary_lines() {
            println!("{line}");
        }
        for project in &report.projects {
            println!(
                "{}: {} context runs, {} with lessons; {} active validated, {} pending reviews",
                project.project_name,
                project.contexts.runs,
                project.contexts.runs_with_memories,
                project.memories.active_validated,
                project.memories.pending_reviews
            );
            for line in project.context_use.summary_lines() {
                println!("{line}");
            }
        }
        println!(
            "Coverage is partial. Use --json for per-field reporting coverage and controlled comparisons. Daily totals alone do not establish savings."
        );
    }
    Ok(true)
}

fn run_workspace(overrides: &ConfigOverrides, action: &WorkspaceAction) -> Result<bool> {
    let config = Config::load_for_current_dir(overrides)?;
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    let profiles = engram::workspaces::list(&home, &config)?;
    match action {
        WorkspaceAction::List => println!("{}", serde_json::to_string_pretty(&profiles)?),
        WorkspaceAction::Show => println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"workspace":config.workspace,"database":config.storage.db_path,"socket":config.daemon.socket_path,"web_enabled":config.web.enabled,"web_port":config.web.port})
            )?
        ),
    }
    Ok(true)
}

fn print_latency_table(metrics: &engram::metrics::MetricsSnapshot) {
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

fn print_latency_row(name: &str, histogram: &engram::metrics::HistogramSnapshot) {
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

    println!("Engram dashboard (single snapshot)");
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
        .map_err(|source| engram::error::io_error_for_cli(path, source))?;
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
    block_on(engram::autostart::ensure_daemon(config))
}

fn block_on<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| engram::Error::Daemon(format!("failed to start client runtime: {error}")))?
        .block_on(future)
}

const fn status_label(status: engram::doctor::CheckStatus) -> &'static str {
    match status {
        engram::doctor::CheckStatus::Pass => "PASS",
        engram::doctor::CheckStatus::Warning => "WARN",
        engram::doctor::CheckStatus::Fail => "FAIL",
    }
}

fn save_product_guide(
    client: &DaemonClient,
    model: &str,
    dossier: &str,
    document: &str,
) -> Result<()> {
    let record: Option<engram::product::Record> = serde_json::from_value(block_on(
        client.product_query(&engram::product::Query::Guide),
    )?)?;
    block_on(client.product_mutate(&engram::product::Mutation::Save {
        id: Some("workspace-guide".to_owned()),
        expected_version: record.map_or(0, |r| r.version),
        item: Box::new(engram::product::Item::Guide(engram::product::Guide {
            content: document.to_owned(),
            model: model.to_owned(),
            dossier_digest: engram::architect::stable_digest(dossier),
        })),
    }))?;
    Ok(())
}

fn run_product(
    overrides: &ConfigOverrides,
    socket: Option<&Path>,
    action: &ProductAction,
) -> Result<bool> {
    use engram::product::{BundleRequest, ImpactRequest, Query};
    let mut config = Config::load_for_current_dir(overrides)?;
    apply_socket_override(&mut config, socket)?;
    require_daemon(&config)?;
    let client = client(&config);
    let read = |path: &Path| -> Result<String> {
        std::fs::read_to_string(path)
            .map_err(|e| engram::Error::Config(format!("{}: {e}", path.display())))
    };
    let result = match action {
        ProductAction::Apply { file } => serde_json::to_value(block_on(
            client.product_mutate(&serde_json::from_str(&read(file)?)?),
        )?)?,
        ProductAction::Lesson { file } => {
            block_on(client.lesson_mutate(&serde_json::from_str(&read(file)?)?))?
        }
        _ => {
            let query = match action {
                ProductAction::Schema => Query::Schema,
                ProductAction::Signals { task_id } => Query::TaskSignals {
                    task_id: TaskId::parse(task_id)?,
                },
                ProductAction::Show { reviews } => {
                    if *reviews {
                        Query::Reviews
                    } else {
                        Query::Snapshot
                    }
                }
                ProductAction::History { id } => Query::History { id: id.clone() },
                ProductAction::Bundle {
                    entity,
                    task,
                    budget,
                    environment,
                } => Query::Bundle(BundleRequest {
                    entity_id: entity.clone(),
                    task: task.clone(),
                    token_budget: *budget,
                    environment: environment.clone(),
                }),
                ProductAction::Impact {
                    project,
                    file,
                    diff,
                    environment,
                } => Query::Impact(ImpactRequest {
                    project_id: engram::ids::ProjectId::parse(project)?,
                    paths: file.clone(),
                    diff: diff.as_deref().map(read).transpose()?.unwrap_or_default(),
                    environment: environment.clone(),
                }),
                _ => unreachable!(),
            };
            block_on(client.product_query(&query))?
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(true)
}
