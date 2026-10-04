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

Everything below is the current state of the unreleased **0.1.0** pre-release.
Nothing has shipped under a version tag yet, so there is no comparison base:
the entries describe the shape of the product as it stands and the changes made
since the initial implementation landed. On-disk formats and CLI surfaces are
not yet stable, and the schema version is 7.

### Added

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

### Changed

- The bounded byte-weighted cache is now live instead of decorative: world
  search and graph-neighborhood reads consult per-project, generation-scoped
  partitions, and every graph mutation advances the generation before its
  response returns, making a stale cached read impossible by construction.
  Cache configuration takes real byte budgets and is enabled by default;
  health reports partition stats and hit/miss counters.

### Fixed

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
