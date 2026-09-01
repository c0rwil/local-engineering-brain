# Local Engineering Brain (LEB) — operator guide

Single-crate Rust project. Four binaries from one lib: `leb` (CLI), `lebd`
(daemon), `leb-proxy` (model proxy), `leb-mcp` (MCP stdio adapter). The
authoritative product spec is `LOCAL_ENGINEERING_BRAIN_SPEC.md`; ADRs live in
`docs/adr/`. Everything is local-first: no cloud dependency in the default
path, ever.

## Commands

| Task | Command |
|---|---|
| Format check | `cargo fmt --all -- --check` |
| Lint (warnings are hard errors) | `cargo clippy --all-targets --all-features -- -D warnings` |
| Full test suite | `cargo test --all-targets --all-features` |
| Release build | `cargo build --release --locked` |
| Install everything | `sh scripts/install.sh` (versioned releases under `~/.local/share/leb`, symlinks in `~/.local/bin`) |
| Daemon status / stop | `leb daemon status --json` / `leb daemon stop` |
| Diagnostics | `leb doctor --json` |
| Index a repo | `leb index <path> --json` (local tree; add `--origin` for fetched origin/main via throwaway worktree) |
| Compile context | `leb context "<query>" --path <repo> --budget N --json` |
| Architecture facts (hosting, storage, role) | `leb fact set hosting.base_url api.example.com --path <repo>` / `leb fact list` — facts lead the stable prefix, feed hostname link inference, render in the vault |
| Workspace encyclopedia (browser UI) | always on at http://127.0.0.1:11436 while the daemon runs (`[web]` config); `leb web` for a custom port. Refresh buttons index origin/main by default |
| Architecture guide (LLM synthesis) | `leb architect --claude` (opus via claude CLI) or `--model <m>` (local runtime); guide staleness tracked by dossier digest |
| Link repos (cross-repo context) | `leb link add <target> --path <repo> --relation uses` / `leb link list` / auto-discovery: `leb link scan [--apply]` (definitive manifest deps auto-link on every `leb index`) |

Tests are hermetic: no network, no live model, no pre-existing daemon needed.
Integration tests spawn their own daemons on temp sockets.

## Architecture in one paragraph

`lebd` owns the canonical SQLite database (WAL) at `~/.leb/brain.sqlite` and is
the ONLY writer: every mutation flows through a prioritized single-writer
scheduler (`src/scheduler.rs`); reads use a bounded read-only pool. CLI, proxy,
and MCP talk to it over a Unix socket (`~/.leb/lebd.sock`, mode 0600). The
world model (`src/world/`) holds a provenance-bearing code graph built by
tree-sitter (Rust/Python/JS/TS) or an external Graphify export. Memories are
typed and evidence-gated (`src/memory.rs`, `src/experience.rs`): agent claims
cap at 0.50 confidence and only deterministic/observational evidence tied to a
successful episode can promote them. The context compiler (`src/context.rs`)
selects under a hard token budget and explains every injected item. The proxy
(`src/proxy.rs`) fronts an OpenAI/Anthropic-compatible local runtime (MTPLX)
and injects compiled context; `x-leb-mode: off` is a true zero-injection
control path used as the benchmark control.

## Model launchers (installed)

- `claude-mtplx` — Claude Code against MTPLX Optimized-Speed (Q4) through the
  LEB proxy. `LEB_MODE=off` disables injection; `LEB_TOKEN_BUDGET` overrides.
- `claude-mtplx-quality` — same, Q8 Optimized-Quality (best local coder on
  this M5; ~32.7 GB peak, close heavy apps first).
- MTPLX serves on `127.0.0.1:8010`; the LEB proxy on `127.0.0.1:11435`.
  `claude-mtplx --server-status` / `--stop-server` manage them.

## Migration rules

Migrations are sequential numbered SQL files in `src/storage/migrations/`,
registered in `MIGRATIONS` in `src/storage/mod.rs` with
`LATEST_MIGRATION_VERSION` bumped to match. Never edit an applied migration;
never skip a number. Databases newer than the binary are refused (downgrade
protection), so bump-and-reinstall order matters: build, install, restart
`lebd`.

## Gotchas (verified)

- `status` is a READ-ONLY special variable in zsh. The launcher scripts in
  `scripts/` are zsh; assigning `status=` crashes them. This already broke
  `--server-status` once.
- The user's global gitconfig rewrites `https://github.com/` remotes to SSH
  (`url.…insteadOf`). This repo carries a local self-mapping override so
  pushes to `github.com/c0rwil/local-engineering-brain` go over HTTPS with the
  `c0rwil` gh account (the SSH key belongs to a different account and gets
  "Repository not found"). Repo work happens under the `c0rwil` GitHub
  account, NOT `corwil-agi`.
- A running `lebd` predating a schema bump keeps serving old response shapes.
  After `cargo install`, restart it (`leb daemon stop`; next CLI call
  auto-starts the sibling binary) or clients may see deserialization gaps.
- Structural freshness needs the PRIOR scan's dirty-path set to catch files
  whose local edits were reverted while other files stayed dirty. It is
  persisted in `structural_scans.changed_paths_json` (NULL ⇒ conservative full
  rescan). Do not "optimize" the plan back to fingerprint-only comparison.
- Registration writes `.leb/project.toml` into the target repo, which dirties
  the working tree; first-registration Git state is recomputed after that
  write on purpose. `.leb`/`.git` paths are excluded from changed-path
  planning but DO affect the porcelain-based diff fingerprint.
- `wait_for_mutation` responses must stay after cache-generation bumps in
  `src/daemon.rs` — a graph mutation bumps the world-read cache generation
  BEFORE its response returns, which is what makes cached reads safe.
- The daemon test `stale_socket_is_replaced_but_regular_file_is_preserved`
  used to flake under parallel load: macOS can keep a closed Unix listener
  connectable for a moment while the kernel tears it down. The test now waits
  for teardown (the same connect probe `bind_socket` uses) before asserting
  replacement; keep that wait if you touch the test.
- `mtplx` subcommands: `pull`/`models` accept `--cache-dir`; `inspect` does
  not (it reads the default cache). `mtplx validate` does not exist.
- Project identity: repos with only LOCAL-path remotes mint a NEW project id
  per clone unless `.leb/project.toml` travels with the checkout (an
  independent template repo and a clone are indistinguishable by content, so
  no fallback heuristic is safe). Benchmark fixtures and seed scripts must
  pin identity via that file — a brain snapshot is useless to an arm that
  resolves a different project id. This silently blanked EVERY brain arm's
  injection across the early paired matrices; see perf-evidence 2026-08-31.

## Benchmarking discipline

Launch every benchmark under `caffeinate -dims` (or attach with
`caffeinate -dims -w <pid>`) so system sleep cannot stall a matrix; sleep
pauses macOS monotonic clocks, but idle-sleep mid-run stalls the model
server and agents. Restart the MTPLX server before any matrix longer than
~10 runs — its RAM session bank grows per run and under memory pressure it
defers generation past Claude Code's stream-idle timeout (this burned five
runs once; see perf-evidence 2026-08-29). After any server (re)start, warm
it with a disposable headless task before the first scored run — cold-start
runs flame out with zero edits (~500 output tokens, observed 3×). Local
fidelity runs need walls ≥2700 s; shorter walls time out legitimate work
and score it as failure (wall-calibration evidence 2026-08-31).

Brain-off mode is the control group and must remain semantically untouched
(spec P10). The evaluation harness (`src/evaluation.rs`, `leb eval run`)
executes paired off/brain trials in isolated repo copies with per-arm
databases and sockets. Only fixture smoke runs exist so far — no real-model
benchmark has been recorded yet. When one is, commit the evidence under
`docs/perf-evidence/` with date-and-commit filenames.
