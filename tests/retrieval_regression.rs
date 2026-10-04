//! Hermetic retrieval regression gate.
//!
//! An isolated `engramd` indexes the committed synthetic repository in
//! `tests/fixtures/retrieval_repo` and compiles context for authored
//! source-navigation probes. The gate checks that the intended file is ranked
//! where expected and its definition is selected, that unmatched queries add no
//! filler, that every compilation stays within its token budget, and that off
//! mode injects nothing. Assertions resolve each selected item to its indexed
//! file path and symbol instead of parsing rendered pointer text, so the
//! pointer format (for example line ranges) can change without breaking the
//! gate.
//!
//! Run `cargo test --test retrieval_regression -- --nocapture` to print the
//! per-probe report.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use engram::context::{CompiledContext, ContextCompileRequest, ContextMode, estimate_tokens};
use engram::daemon::DaemonClient;
use engram::ids::ProjectId;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::json;
use tempfile::TempDir;

const FIXTURE: &str = "tests/fixtures/retrieval_repo";
/// Same default budget as the frozen-snapshot navigation harness.
const DEFAULT_BUDGET: u32 = 1_600;
/// Item types that point directly at, or next to, indexed source.
const SOURCE_TYPES: [&str; 2] = ["code", "code_dependency"];
/// Node kinds that count as a definition.
const DEFINITION_KINDS: [&str; 5] = ["Function", "Method", "Type", "Interface", "Module"];
/// Definition hits are scored within the first five source pointers.
const DEFINITION_WINDOW: usize = 5;
/// Natural-language probes must rank the intended file within this window.
const PROSE_WINDOW: usize = 2;
/// Direct pointers allowed for an exact identifier or path probe.
const EXACT_MAX_DIRECT: usize = 2;
/// Direct pointers allowed for a natural-language probe. The base selects at
/// most three; a loosened relevance filter returns most of the fixture.
const PROSE_MAX_DIRECT: usize = 6;
/// Non-Markdown source files in the fixture.
const FIXTURE_SOURCE_FILES: i64 = 12;

#[derive(Clone, Copy, Debug)]
enum Expect {
    /// Exact identifier or path: the intended file must be the first pointer.
    First(&'static str),
    /// Natural language: the intended file must be within `PROSE_WINDOW`.
    Prose(&'static str),
    /// Unmatched query: no items and no dynamic context.
    Nothing,
}

#[derive(Clone, Copy, Debug)]
enum Definition {
    /// This definition must be selected within `DEFINITION_WINDOW`.
    Required(&'static str),
    /// Reported only. The current compiler is known to miss it; promote it to
    /// `Required` once it passes.
    KnownGap(&'static str),
    NotScored,
}

struct Probe {
    id: &'static str,
    query: &'static str,
    current_paths: &'static [&'static str],
    expect: Expect,
    definition: Definition,
}

const PROBES: &[Probe] = &[
    Probe {
        id: "rust-exact-helper",
        query: "with_env_guard",
        current_paths: &[],
        expect: Expect::First("service/src/env_guard.rs"),
        definition: Definition::Required("with_env_guard"),
    },
    Probe {
        id: "rust-qualified-method",
        query: "Settings::load_for_mode",
        current_paths: &[],
        expect: Expect::First("service/src/settings.rs"),
        definition: Definition::Required("Settings::load_for_mode"),
    },
    Probe {
        id: "rust-prose-dotenv",
        query: "Test mode must not reload developer dotenv values",
        current_paths: &[],
        expect: Expect::Prose("service/src/settings.rs"),
        definition: Definition::Required("reload_dotenv_values"),
    },
    Probe {
        id: "rust-prose-shared-lock",
        query: "Recover the poisoned environment guard lock at every call site",
        current_paths: &[],
        expect: Expect::Prose("service/src/env_guard.rs"),
        definition: Definition::Required("recover_poisoned_guard"),
    },
    Probe {
        id: "python-exact-function",
        query: "compute_backoff_delay",
        current_paths: &[],
        expect: Expect::First("worker/retry_policy.py"),
        definition: Definition::Required("compute_backoff_delay"),
    },
    Probe {
        id: "python-exact-class",
        query: "RetryBudget",
        current_paths: &[],
        expect: Expect::First("worker/retry_policy.py"),
        definition: Definition::Required("RetryBudget"),
    },
    Probe {
        id: "python-prose-backoff",
        query: "exponential backoff delay for retried queue messages",
        current_paths: &[],
        expect: Expect::Prose("worker/retry_policy.py"),
        definition: Definition::Required("compute_backoff_delay"),
    },
    Probe {
        id: "python-unrelated-current-path",
        query: "compute_backoff_delay",
        current_paths: &["README.md"],
        expect: Expect::First("worker/retry_policy.py"),
        definition: Definition::Required("compute_backoff_delay"),
    },
    Probe {
        id: "ts-exact-function",
        query: "refreshSessionToken",
        current_paths: &[],
        expect: Expect::First("web/src/session/refreshToken.ts"),
        definition: Definition::Required("refreshSessionToken"),
    },
    Probe {
        id: "ts-exact-class",
        query: "ApiClient",
        current_paths: &[],
        expect: Expect::First("web/src/api/client.ts"),
        definition: Definition::Required("ApiClient"),
    },
    Probe {
        // Every symbol in the file matches these words through its path, and
        // full-text search keeps two candidates per file: the file node wins.
        id: "ts-prose-refresh",
        query: "refresh the session token before it expires",
        current_paths: &[],
        expect: Expect::Prose("web/src/session/refreshToken.ts"),
        definition: Definition::KnownGap("refreshSessionToken"),
    },
    Probe {
        // The prose words match the session module more strongly; the
        // explicit path must still win.
        id: "exact-path-over-prose",
        query: "Move the session refresh call out of web/src/api/client.ts",
        current_paths: &[],
        expect: Expect::First("web/src/api/client.ts"),
        definition: Definition::NotScored,
    },
    Probe {
        id: "negative-unmatched-token",
        query: "quuxzorbpf1049",
        current_paths: &[],
        expect: Expect::Nothing,
        definition: Definition::NotScored,
    },
    Probe {
        id: "negative-missing-path",
        query: "service/src/never_exists_probe.rs",
        current_paths: &[],
        expect: Expect::Nothing,
        definition: Definition::NotScored,
    },
    Probe {
        id: "negative-stop-words",
        query: "please help with this",
        current_paths: &[],
        expect: Expect::Nothing,
        definition: Definition::NotScored,
    },
    Probe {
        // Shares one component (`cache`) with indexed symbols and paths.
        id: "negative-partial-identifier",
        query: "frobnicate_widget_cache",
        current_paths: &[],
        expect: Expect::Nothing,
        definition: Definition::NotScored,
    },
];

/// Call sites of the shared environment guard. Reported, not gated: the base
/// compiler has no related-use cue and selects none of them.
const SHARED_LOCK_PROBE: &str = "rust-prose-shared-lock";
const SHARED_LOCK_CALL_SITES: [&str; 3] = [
    "service/src/broker_contract.rs",
    "service/src/cache_contract.rs",
    "service/src/config_harness.rs",
];

/// A query that matches most of the fixture, for the budget sweep.
const BROAD_QUERY: &str =
    "settings dotenv guard backoff retry session token client ledger currency";
const SWEEP_BUDGETS: [u32; 6] = [64, 96, 160, 256, 400, DEFAULT_BUDGET];

/// One selected source item, resolved through the index.
#[derive(Clone, Debug, PartialEq)]
struct Pointer {
    source_type: String,
    path: String,
    kind: String,
    label: String,
}

struct Daemon {
    child: Child,
    client: DaemonClient,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

struct Fixture {
    project: ProjectId,
    repository: PathBuf,
}

impl Fixture {
    fn request(
        &self,
        request_id: String,
        query: &str,
        current_paths: &[&str],
        budget: u32,
        mode: ContextMode,
    ) -> ContextCompileRequest {
        ContextCompileRequest {
            request_id,
            project_id: self.project.clone(),
            task_text: query.to_owned(),
            current_paths: current_paths
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
            token_budget: budget,
            mode,
            repository_root: Some(self.repository.clone()),
            head: None,
            task_id: None,
            framing: None,
        }
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create fixture directory");
    for entry in std::fs::read_dir(source).expect("read fixture directory") {
        let entry = entry.expect("fixture entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("fixture file type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy fixture file");
        }
    }
}

/// Start an isolated daemon. `HOME` points into the temporary directory, so a
/// developer's user configuration cannot change retrieval behavior.
fn start_daemon(root: &Path) -> Daemon {
    let socket = root.join("d.sock");
    let config = root.join("config.toml");
    let document = json!({
        "workspace": "default",
        "storage": {"db_path": root.join("brain.sqlite"), "vector_backend": "disabled"},
        "daemon": {"socket_path": socket},
        "web": {"enabled": false},
        "watch": {"enabled": false},
        "jobs": {"enabled": false},
        "vault": {"enabled": false, "auto_refresh": false, "root": root.join("vault")},
        "judge": {"enabled": false},
        "architect": {"auto": "off"},
        "synthesis": {"auto": false, "cross_project_hypotheses": false},
        "brain": {"auto_learn": false},
        "links": {"auto_scan": false},
        "context": {"semantic_retrieval": false, "include_linked_projects": false},
        "embeddings": {"provider": "disabled", "allow_remote": false},
        "sync": {"central_url": "", "every_secs": 0, "push": false},
        "network": {"local_only": true},
        "structural": {"provider": "tree_sitter"},
    });
    std::fs::write(&config, toml::to_string(&document).expect("config toml")).expect("config");
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("isolated home");
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("engramd"));
    command
        .arg("--config")
        .arg(&config)
        .current_dir(root)
        .env("HOME", &home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("ENGRAM_") {
            command.env_remove(key);
        }
    }
    let mut child = command.spawn().expect("spawn engramd");
    let deadline = Instant::now() + Duration::from_secs(20);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        if let Some(status) = child.try_wait().expect("poll engramd") {
            panic!("engramd exited before readiness: {status}");
        }
        assert!(Instant::now() < deadline, "engramd readiness timeout");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon {
        child,
        client: DaemonClient::new(&socket, Duration::from_secs(30)),
    }
}

async fn compile(client: &DaemonClient, request: ContextCompileRequest) -> CompiledContext {
    client
        .compile_context(&request)
        .await
        .unwrap_or_else(|error| panic!("compile {}: {error}", request.request_id))
}

/// Resolve source items, in emitted order, to their indexed path and symbol.
fn pointers(index: &Connection, context: &CompiledContext) -> Vec<Pointer> {
    context
        .items
        .iter()
        .filter(|item| SOURCE_TYPES.contains(&item.source_type.as_str()))
        .map(|item| {
            let (path, kind, label) = index
                .query_row(
                    "SELECT file_path, kind, label FROM nodes WHERE id = ?1",
                    [&item.source_id],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .expect("query indexed node")
                .unwrap_or_else(|| panic!("source item {} is not an indexed node", item.source_id));
            Pointer {
                source_type: item.source_type.clone(),
                path: path.unwrap_or_default(),
                kind,
                label,
            }
        })
        .collect()
}

fn is_definition(pointer: &Pointer, path: &str, label: &str) -> bool {
    pointer.path == path
        && DEFINITION_KINDS.contains(&pointer.kind.as_str())
        && (pointer.label == label || pointer.label.ends_with(&format!("::{label}")))
}

fn rendered(context: &CompiledContext) -> String {
    format!("{}{}", context.stable_prefix, context.dynamic_context)
}

/// Record a gate violation without stopping, so one run reports every miss.
fn check(failures: &mut Vec<String>, passed: bool, message: impl FnOnce() -> String) {
    if !passed {
        failures.push(message());
    }
}

/// Checks that apply to every probe, whatever its expectation.
fn check_invariants(
    failures: &mut Vec<String>,
    id: &str,
    brain: &CompiledContext,
    selected: &[Pointer],
    repeat: &[Pointer],
    off: &CompiledContext,
) {
    check(
        failures,
        brain.metrics.selected_tokens <= DEFAULT_BUDGET,
        || {
            format!(
                "{id}: selected {} tokens over budget",
                brain.metrics.selected_tokens
            )
        },
    );
    check(
        failures,
        estimate_tokens(&rendered(brain)) <= DEFAULT_BUDGET,
        || format!("{id}: rendered context exceeds the budget"),
    );
    check(failures, repeat == selected, || {
        format!("{id}: a repeated compilation selected different sources")
    });
    check(
        failures,
        off.mode == ContextMode::Off
            && off.stable_prefix.is_empty()
            && off.dynamic_context.is_empty()
            && off.items.is_empty()
            && off.metrics.selected_tokens == 0,
        || format!("{id}: off mode injected content"),
    );
}

fn check_expectation(
    failures: &mut Vec<String>,
    probe: &Probe,
    brain: &CompiledContext,
    selected: &[Pointer],
) {
    let id = probe.id;
    let paths: Vec<&str> = selected
        .iter()
        .map(|pointer| pointer.path.as_str())
        .collect();
    let direct = selected
        .iter()
        .filter(|pointer| pointer.source_type == "code")
        .count();
    let (path, window, max_direct) = match probe.expect {
        Expect::First(path) => (path, 1, EXACT_MAX_DIRECT),
        Expect::Prose(path) => (path, PROSE_WINDOW, PROSE_MAX_DIRECT),
        Expect::Nothing => {
            check(
                failures,
                brain.items.is_empty() && brain.dynamic_context.is_empty(),
                || format!("{id}: unmatched query added {paths:?}"),
            );
            return;
        }
    };
    check(failures, paths.contains(&path), || {
        format!("{id}: intended file {path} missing from the pack {paths:?}")
    });
    check(
        failures,
        paths.iter().take(window).any(|found| *found == path),
        || format!("{id}: intended file {path} not within the first {window}: {paths:?}"),
    );
    // Line-ranged pointers still name the file path.
    check(failures, rendered(brain).contains(path), || {
        format!("{id}: intended file {path} is not named in the context text")
    });
    check(failures, direct <= max_direct, || {
        format!("{id}: {direct} direct pointers exceed {max_direct}: {paths:?}")
    });
}

fn definition_hit(probe: &Probe, selected: &[Pointer], label: &str) -> bool {
    let (Expect::First(path) | Expect::Prose(path)) = probe.expect else {
        return false;
    };
    selected
        .iter()
        .take(DEFINITION_WINDOW)
        .any(|pointer| is_definition(pointer, path, label))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)] // One fixture run keeps the gate and its report together.
async fn synthetic_repository_retrieval_meets_regression_thresholds() {
    let temp = TempDir::new().expect("temp dir");
    let repository = temp.path().join("repo");
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE),
        &repository,
    );
    let daemon = start_daemon(temp.path());
    let indexed = daemon
        .client
        .index_project(&repository, None)
        .await
        .expect("index fixture repository");
    let fixture = Fixture {
        project: indexed.resolved.project.id.clone(),
        repository,
    };
    let index = Connection::open_with_flags(
        temp.path().join("brain.sqlite"),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only index");
    let source_files: i64 = index
        .query_row(
            "SELECT count(*) FROM indexed_files WHERE project_id = ?1 AND language <> 'markdown'",
            [fixture.project.as_str()],
            |row| row.get(0),
        )
        .expect("indexed file count");

    let mut failures = Vec::new();
    check(&mut failures, source_files == FIXTURE_SOURCE_FILES, || {
        format!("indexed {source_files} source files, expected {FIXTURE_SOURCE_FILES}")
    });
    let mut report = Vec::new();
    let (mut required_hits, mut required_total) = (0_usize, 0_usize);
    let mut gaps_closed = Vec::new();
    for (ordinal, probe) in PROBES.iter().enumerate() {
        let probe_request = |mode: ContextMode, repeat: usize| {
            fixture.request(
                format!("retrieval-regression-{ordinal}-{repeat}-{mode:?}"),
                probe.query,
                probe.current_paths,
                DEFAULT_BUDGET,
                mode,
            )
        };
        let brain = compile(&daemon.client, probe_request(ContextMode::Brain, 0)).await;
        let repeat = compile(&daemon.client, probe_request(ContextMode::Brain, 1)).await;
        let off = compile(&daemon.client, probe_request(ContextMode::Off, 0)).await;
        let selected = pointers(&index, &brain);
        check_invariants(
            &mut failures,
            probe.id,
            &brain,
            &selected,
            &pointers(&index, &repeat),
            &off,
        );
        check_expectation(&mut failures, probe, &brain, &selected);
        let definition = match probe.definition {
            Definition::Required(label) => {
                let hit = definition_hit(probe, &selected, label);
                required_total += 1;
                required_hits += usize::from(hit);
                check(&mut failures, hit, || {
                    format!(
                        "{}: definition {label} not within the first {DEFINITION_WINDOW}",
                        probe.id
                    )
                });
                Some(hit)
            }
            Definition::KnownGap(label) => {
                let hit = definition_hit(probe, &selected, label);
                if hit {
                    gaps_closed.push(probe.id);
                }
                Some(hit)
            }
            Definition::NotScored => None,
        };
        let call_sites = (probe.id == SHARED_LOCK_PROBE).then(|| {
            SHARED_LOCK_CALL_SITES
                .iter()
                .filter(|site| selected.iter().any(|pointer| pointer.path == **site))
                .count()
        });
        report.push(json!({
            "id": probe.id,
            "pointers": selected.iter().map(|pointer| format!(
                "{} {} {} {}", pointer.source_type, pointer.path, pointer.kind, pointer.label
            )).collect::<Vec<_>>(),
            "definition_hit": definition,
            "selected_tokens": brain.metrics.selected_tokens,
            "shared_lock_call_sites_selected": call_sites,
        }));
    }

    // Small budgets must still be honored, including the source-map heading.
    let mut sweep = Vec::new();
    for budget in SWEEP_BUDGETS {
        let context = compile(
            &daemon.client,
            fixture.request(
                format!("retrieval-regression-budget-{budget}"),
                BROAD_QUERY,
                &[],
                budget,
                ContextMode::Brain,
            ),
        )
        .await;
        check(
            &mut failures,
            context.metrics.selected_tokens <= budget
                && estimate_tokens(&rendered(&context)) <= budget,
            || {
                format!(
                    "budget {budget}: selected {} tokens",
                    context.metrics.selected_tokens
                )
            },
        );
        let files: BTreeSet<String> = pointers(&index, &context)
            .into_iter()
            .map(|pointer| pointer.path)
            .collect();
        sweep.push(json!({
            "budget": budget,
            "selected_tokens": context.metrics.selected_tokens,
            "files": files.len(),
        }));
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "probes": report,
            "required_definition_hits": format!("{required_hits}/{required_total}"),
            "known_gaps_now_passing": gaps_closed,
            "budget_sweep": sweep,
            "failures": failures,
        }))
        .expect("report json")
    );
    daemon.client.shutdown().await.expect("shutdown engramd");
    assert!(
        failures.is_empty(),
        "retrieval regression gate failed:\n{}",
        failures.join("\n")
    );
}
