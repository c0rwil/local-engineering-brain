# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Everything below is the current state of the unreleased **0.1.0** pre-release.
Nothing has shipped under a version tag yet, so there is no comparison base:
the entries describe the shape of the product as it stands and the changes made
since the initial implementation landed. On-disk formats and CLI surfaces are
not yet stable, and the schema version is 4.

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

### Changed

- Cache configuration now takes real byte budgets instead of the reserved
  placeholder values it previously accepted. The bounded cache primitive stays
  disabled in V1 — generation-based invalidation is not wired end to end yet —
  but the settings are now honest about what they will mean when it is
  enabled, rather than silently inert.

### Fixed

- Rescanning no longer loses track of files that were reverted while other
  files in the same project stayed dirty. Scan dirty sets are now persisted,
  so a partially reverted working tree converges to the correct state instead
  of retaining stale structure from the earlier scan.

### Removed

- The FP16 MTPLX launcher. It exists to help M1 and M2 hardware and offers no
  gain on M3 and later, so shipping it only added a third launcher to install,
  checksum, and explain. The remaining launchers are installed atomically with
  the binaries.
