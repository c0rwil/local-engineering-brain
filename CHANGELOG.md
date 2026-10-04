## Engram rename (2026-09-04)

The product is now **Engram** (an engram is the physical trace a memory
leaves in a brain). Binaries: `engram`, `engramd`, `engram-proxy`,
`engram-mcp`; state `~/.engram`; config `~/.config/engram/config.toml`;
env vars `ENGRAM_*`; proxy headers `x-engram-*`; MCP tools `engram_*`.
Compatibility: the installer keeps `leb`/`lebd`/`leb-proxy`/`leb-mcp`
symlinks, `~/.leb` and `~/.config/leb` migrate automatically on first
run, repositories carrying a legacy `.leb/` metadata dir keep it (identity
never fractures), and `hooks install` / `agent-md` refresh pre-rename
entries in place.

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Evidence

- First end-to-end validation of the central claim on a frontier model:
  with pinned project identity and real injection, all three brain-arm
  compositions pass 18/18 tribal-knowledge fidelity probes that the
  memoryless baseline fails 4/6 of; the review-standards rendering is the
  fastest and cheapest arm. Earlier paired matrices are retroactively
  annotated: their brain arms ran with prefix-only injection due to the
  clone-identity fracture (see perf-evidence 2026-08-31).

### Security

- Prompt-injection screen for lesson proposals and stored memories. Flagged
  proposals are held for human review and never sent to the judge model;
  accepting flagged text needs an explicit human acknowledgment, which MCP
  agents cannot give, and is recorded as a human override. Evidence-gated
  promotion refuses flagged text. `engram doctor` reports flagged validated
  memories without changing them. Rendered context escapes hidden characters
  and framing markup instead of deleting them. See `docs/SECURITY.md`.
- The judge screens task prompts too. Agent-host `<system-reminder>` notices
  are removed before screening and before the model reads the prompt; prompts
  without a session (MCP, CLI) get every rule, session-captured prompts the
  judge-steering rules.
- On a loopback bind, the workspace browser answers only loopback `Host`
  names with its own port and refuses state-changing requests from another
  `Origin` (DNS-rebinding protection). Non-loopback binds are unchanged.
- `engram-proxy` applies the same shared guard. Model clients that send no
  `Origin` are unaffected; the brain-off path is unchanged apart from the
  transport check.

Everything below is the current state of the unreleased **0.1.0** pre-release.
Nothing has shipped under a version tag yet, so there is no comparison base:
the entries describe the shape of the product as it stands and the changes made
since the initial implementation landed. On-disk formats and CLI surfaces are
not yet stable, and the schema version is 7.

### Added

- `engram project merge --into <keep> --from <retire>` merges a repository
  registered as two projects (`engram doctor` reports them). It refuses
  unless live checkouts of both report the same canonical remote and root
  commit, prints a deterministic plan (`--dry-run`, `--json`) and changes
  nothing without `--yes`. Execution takes a consistent backup, then one
  transaction re-scopes every project-keyed table by an explicit role that
  is checked against the schema (a new project-keyed table without a role
  makes merges refuse, and a test fails): rows move, the retired structural
  index is dropped, events stay as history, collisions follow recorded
  rules, and a `PROJECTS_MERGED` event plus a tombstone naming the survivor
  make replay reproduce the result. The CLI rewrites the retired checkouts'
  pins, re-indexes the survivor once from its primary checkout (scanning
  other trees would quarantine memories through drift detection) and
  finalizes: memory anchors are re-attached to active symbols with the same
  stable identity or their memories are marked for revalidation, and rows
  queued under the retired id are moved, each run audited in a
  `PROJECT_MERGE_FINALIZED` event. Requests and pins that still carry the
  retired id resolve to the survivor; only typed project references are
  rewritten, never prompts, commands, artifacts or other text, and the
  redirect is installed by the merge job itself, so a disconnected client
  cannot skip it. A repeated merge skips completed steps. Migration 0021
  adds the tombstone column, waiting anchors and the indexes that make
  dropping nodes fast.
- The V1 product: a local-first, evidence-grounded engineering world model for
  coding agents. A per-user Rust daemon (`lebd`) owns all mutation ordering
  behind a single bounded priority writer with a bounded reader pool, keeping
  canonical state in one SQLite database in WAL mode. Around it sit a CLI
  (`leb`), an MCP server (`leb-mcp`), and an OpenAI/Anthropic proxy
  (`leb-proxy`) that can compile a small version-aware context for each model
  request — and, critically, can serve the same model and runtime with the
  brain switched off, so any claimed benefit is measured against an exact
  control path rather than a remembered baseline.
- Stable project identity, Git and tree-sitter structural ingestion, Graphify
  import, typed episodes and evidence, evidence-gated memory promotion,
  bitemporal validity filtering, hybrid retrieval, bounded context compilation
  with deterministic token budgets, CLI inspection and purge, backup/export,
  and paired benchmark evaluation.
- Hard bounds as a design rule rather than a tuning knob: queue depths, read
  pool size, request sizes, connection counts, scan and import volume, Git
  history depth, traversal, and context tokens are all capped, and no
  repository graph or full memory corpus is ever held in process memory.
  Durable authored text is conservatively secret-redacted; evidence artifacts
  store digests only; sockets, database files, WAL sidecars, backups, and
  exports default to user-only permissions.
- Continuous integration and release-engineering scaffolding: GitHub Actions
  workflows reproducing the four release gates (fmt, clippy with warnings as
  errors, tests on Linux and macOS, and a `--locked` release build), a weekly
  `cargo-deny` audit covering advisories, licenses, bans, and sources, a
  toolchain pin shared by contributors and CI, Dependabot for both Cargo and
  Actions, and this changelog. The latency and RSS assertions in
  `tests/performance.rs` run in a separate non-blocking job, because their
  bounds are meaningful on the Apple Silicon target and not on a shared runner.
- Hybrid retrieval's semantic stage (V1 goal): `EmbeddingProvider` and
  `VectorIndex` ports with a deterministic, fully offline default — a seeded
  hashed n-gram embedder into 256 normalized dimensions and a rebuildable
  SQLite vector sidecar with bounded brute-force cosine search. Disabled by
  default (`context.semantic_retrieval`), so ablation against the purely
  lexical/structural ranker stays one config flag away; purge removes vectors,
  and `leb semantic rebuild` reconstructs the sidecar from canonical memories.
- A content-addressed blob store for large evidence artifacts: redacted
  payloads above `storage.blob_threshold_bytes` (16 KiB default) land under
  `~/.leb/blobs/` sharded by SHA-256 digest with refcounted dedupe, purge
  integration that deletes files at zero references, transparent read-back,
  self-contained exports, and orphan reporting in `leb doctor`.
- A daemon-owned durable background job system over the previously unused
  `jobs` table: idempotent enqueue by `(type, dedupe_key)`, retries with
  exponential backoff, crash recovery that resets `running` jobs on start,
  RSS-pressure-aware execution under the existing background concurrency
  budget, and four job types — FTS/database integrity sweeps, semantic index
  rebuilds, bounded consolidation GC with blob-orphan cleanup, and an
  evidence staleness sweep that deliberately never re-executes stored
  commands (spec 26.4). Surfaced as `POST /api/v1/jobs`, `leb jobs
  list/enqueue/status`, and daily self-enqueued maintenance.
- Process-lifetime observability (spec 25.1): an in-daemon registry of
  fixed-bucket latency histograms (compile, retrieval, per-endpoint-group
  request, index scan) with interpolated percentiles, counters for compiles
  by mode, injected items/tokens, retrieval candidates, memory promotions and
  purges, evidence records, and a real lower-bound stale-injection signal
  wired to drift quarantine. Served by `GET /api/v1/metrics`, `leb metrics`,
  and a single-shot `leb dashboard`; the proxy exposes its own outcome
  counters at `GET /leb/metrics` on its loopback listener.
- A property-based test layer (spec 32.3) with proptest: fifteen seeded,
  shrinking properties over the event store, memory state machine, Git
  porcelain parsing, context budgeting, retrieval scoring, the byte-weighted
  cache, redaction, and typed IDs, with failure seeds persisted in-repo.
- Episode-to-procedure synthesis (spec 16.3–16.5): a bounded pass over recent
  successful episodes creates a typed `procedure` memory when at least
  `synthesis.min_supporting_episodes` (default 3) evidence-verified episodes
  share one normalized strategy signature with overlapping touched files.
  Drafts start unvalidated at the agent-inference confidence cap and never
  auto-promote; re-synthesis is idempotent and supersets extend provenance.
  Surfaced as `POST /api/v1/synthesis/run`, `leb synthesize <path>`, and an
  opt-in `synthesis.auto` pass after successful task finishes. Context
  compilation can now declare the task it serves, recording truthful
  usage-outcome linkage in `usage_events`; a validated procedure with
  `synthesis.min_distinct_uses` distinct successful uses can be promoted to
  the `skill` kind. Cross-project transfer of validated procedures as
  explicitly marked hypothesis memories is gated behind
  `synthesis.cross_project_hypotheses` (default off).
- Capture-health diagnostics in `engram doctor`: `checkout_access` probes
  every registered checkout from inside the daemon and fails with the denied
  paths and remediation when the daemon may not read them; `project_identity`
  warns when live checkouts with one remote and root commit belong to
  different project ids; `codex_hooks` (run in the CLI process) reports
  Engram hooks that Codex has not trusted, using Codex's `hooks.state` trust
  hashes. `engram daemon restart` replaces the daemon with one started from
  the calling terminal; health and `engram daemon status` report the daemon
  pid and the process chain that auto-started it.
- Projects registered without Git identity record their root commit and,
  when no other active project could be the same repository, their canonical
  remote once Git is readable (`PROJECT_IDENTITY_OBSERVED`); ids never change.
- Status notes. When the shell hides a recognized verification's exit status,
  the post-tool hook says so in its own output: a pipe without `pipefail`,
  `;` or a newline after it, `||`, a background run, an unmodeled `cd` or
  unread syntax. The note shows the same command in a checked recordable
  form. It goes at most once per conversation and cause, never with
  `ENGRAM_MODE=off`, through Claude Code's and Codex's
  `hookSpecificOutput.additionalContext` and Cursor's `additional_context`.
  The effectiveness ledger counts every recognized run (schema 20,
  `verification_observations`: kind, cause and note only). `engram
  effectiveness` reports the recordable share overall and before and after
  each conversation's first note. The browser's Effectiveness tab shows the
  same ledger per repository under "Recordable verification runs".

### Changed

- The bounded byte-weighted cache is now live instead of decorative: world
  search and graph-neighborhood reads consult per-project, generation-scoped
  partitions, and every graph mutation advances the generation before its
  response returns, making a stale cached read impossible by construction.
  Cache configuration takes real byte budgets and is enabled by default;
  health reports partition stats and hit/miss counters.

### Fixed

- Hooks resolve paths anywhere inside a registered folder that is not a Git
  repository. Before, only paths at the folder's root resolved: reads, edits
  and verifications in its subdirectories were dropped, and `engram index` or
  `--path` on a subdirectory registered it as a separate project with its own
  pin. A path now belongs to the nearest folder above it that holds a project
  pin, as a subdirectory belongs to its repository, and configuration loads
  from that folder. The search stops at any `.git` entry and at the home
  directory, so a registered home directory owns only itself.
- The user guide said Codex reports exit codes to hooks. Codex 0.155 does not:
  every shell command, also one run from a code-mode `exec` cell, reaches
  hooks as a `Bash` call whose `tool_response` is the command's output text.
  Engram reads no status from output text, so Codex verification runs are not
  recorded from hooks; status notes still apply. The `exec` cell itself runs
  no hook, and its JavaScript is never read as shell text.
- An interrupted backup (`engram backup create`, daily and pre-merge
  backups) no longer leaves a partial, world-readable file that looks like a
  backup: the copy is written to a hidden 0600 file and renamed when
  complete.
- Automatic vault refreshes recur. The `vault_export` job was keyed per
  project only, so the first export, or a dead one, blocked every later
  refresh. A graph change now schedules one export per repository per
  `[vault] refresh_interval_secs` window (default 300 seconds), run when the
  window closes. Changes in the same window share it. Shadow origin views
  and hidden projects (`internal.hidden`) no longer export a vault folder of
  their own, matching the full vault build. A project hidden after its
  refresh was queued is skipped when the job runs; a requested export still
  runs.
- Job history no longer grows without bound. The daily `consolidation_gc`
  job deletes succeeded and dead job rows that finished more than
  `[jobs] job_retention_days` ago (default 30) in bounded batches and reports
  `job_rows_deleted`. Pending, running and failed rows stay. No table
  references job rows, and a test fails if one starts to.
- The judge's live source checks run Git locally only: no lazy fetch from a
  partial clone's remote (`GIT_NO_LAZY_FETCH=1`), no credential prompt and
  no optional index lock. Project-state and commit-ancestry checks use the
  same environment. Git work for one review has a five-second limit. When Git
  exceeds it, the judge kills the process and defers the review with a reason
  that says so, and the next pass checks it again.
- A second daemon could start while the first was still draining: the first
  closed its listener at once, a hook auto-started another daemon, and both
  held the database (a new build migrated under the old one, and job recovery
  requeued the old daemon's running job). `engramd` now holds an exclusive
  lock on `<database>.daemon.lock` for its whole life, refuses to start while
  a daemon from a build without the lock still has the database open, and
  keeps its socket open while it drains, answering `draining` until requests,
  the background job, a judge call and queued writes have finished (bounded;
  writes still queued at the bound are logged as dropped); SIGTERM drains the
  same way. Clients wait for a draining daemon instead of starting another.
  `engram daemon stop` waits for the process to exit (`--wait-secs`),
  `engram daemon restart` follows the old daemon by pid even when it does not
  report one, and `scripts/install.sh` stops the previous daemon with the new
  client after installing and waits for it to exit.
- An auto-started daemon no longer inherits descriptors of the process that
  started it. A pipe another thread had just created (not yet close-on-exec
  on macOS) could stay open in the daemon, so a hook waiting for EOF on it
  hung.
- A checkout the process may not read (for example a macOS privacy denial in
  an auto-started daemon) is no longer treated as a folder without Git: it
  fails registration, lookup and Git probing with a `permission_denied` error
  that names the path, the process and the fix, and the session-start hook
  prints that line instead of staying silent.
- Git porcelain parsing no longer lets `./`-prefixed paths bypass the
  `.git`/`.leb` reserved-directory filter — found by the new property suite,
  with the shrunken failing seed persisted as a regression.
- Rescanning no longer loses track of files that were reverted while other
  files in the same project stayed dirty. Scan dirty sets are now persisted,
  so a partially reverted working tree converges to the correct state instead
  of retaining stale structure from the earlier scan.

### Removed

- The FP16 MTPLX launcher. It exists to help M1 and M2 hardware and offers no
  gain on M3 and later, so shipping it only added a third launcher to install,
  checksum, and explain. The remaining launchers are installed atomically with
  the binaries.
