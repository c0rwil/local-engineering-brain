# Engram — operator guide

Single-crate Rust project. Four binaries from one lib: `engram` (CLI), `engramd`
(daemon), `engram-proxy` (model proxy), `engram-mcp` (MCP stdio adapter). The
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
| Install everything | `sh scripts/install.sh` (versioned releases under `~/.local/share/engram`, symlinks in `~/.local/bin`) |
| Daemon status / stop / restart | `engram daemon status --json` / `engram daemon stop` (waits for the process to exit, `--wait-secs`) / `engram daemon restart` (new daemon inherits this terminal's env and macOS folder access) |
| Diagnostics | `engram doctor --json` (daemon-side `checkout_access` and `project_identity`; CLI-side `codex_hooks` trust) |
| Index a repo | `engram index <path> --json` (local tree; add `--origin` for fetched origin/main via throwaway worktree) |
| Compile context | `engram context "<query>" --path <repo> --budget N --json` |
| Auto-freshness | daemon watcher re-indexes on commits/branch switches (`[watch]`, local 30s / origin 900s; committed HEADs only) |
| Graph views | web defaults to fetched origin/main (shadow projects, `derived_view` metadata); local-checkout view via header toggle; agent context always local |
| Architecture facts (hosting, storage, role) | `engram fact set hosting.base_url api.example.com --path <repo>` / `engram fact list` — facts lead the stable prefix, feed hostname link inference, render in the vault |
| Workspace encyclopedia (browser UI) | always on at http://127.0.0.1:11436 while the daemon runs (`[web]` config); `engram web` for a custom port. Refresh buttons index origin/main by default |
| Living product and flows | `engram product show` / `schema` / `bundle ENTITY TASK` / `impact PROJECT --file PATH`; browser Product, Flows, Changes and Knowledge tabs |
| Architecture guide (LLM synthesis) | `engram architect --claude` (opus via claude CLI) or `--model <m>` (local runtime); SQLite-backed guide; staleness includes indexed source-content digests |
| Link repos (cross-repo context) | `engram link add <target> --path <repo> --relation uses` / `engram link list` / auto-discovery: `engram link scan [--apply]` (definitive manifest deps auto-link on every `engram index`) |
| Ask the workspace | `engram ask "<question>" --path <repo> --claude` (retrieval over memories/facts/symbols/doc sections, cited synthesis; `--dry-run` prints the evidence pack) |
| Session relay | `engram relay board` / `engram relay send <session\|all> "msg"` — delivery at recipient's next prompt via hooks |
| Self-update | `engram update` (pulls the recorded source checkout, atomic reinstall; `--check` reports only) |
| Native coding agents | Use ordinary Claude Code, Codex (CLI, local desktop, IDE) or Cursor with hooks + `engram-mcp`; no local model or launcher required. Cursor: `engram hooks cursor --install` merges `~/.cursor/hooks.json` (`--cursor-home DIR` / `ENGRAM_CURSOR_HOME` override; `--print`, `--uninstall`; MCP opt-in `--mcp`). Codex subagent hooks attribute to the parent session; guardian sub-sessions stay silent |
| Codex automatic loop | `engram hooks codex --install` writes `$CODEX_HOME/hooks.json` (default `~/.codex`); review/trust definitions with `/hooks`. Register MCP separately with `codex mcp add engram -- /absolute/path/to/engram-mcp` |
| Skill packs (agent-native skills) | `engram skills add <git-url>` fetches into `~/.engram/skillpacks/` and installs each `SKILL.md` skill into every detected agent (`~/.claude/skills`, `~/.codex/skills`); `list` / `update` / `install` / `remove`. Engram-installed skills carry a `.engram-skillpack` marker — hand-written skills are never clobbered |

Tests are hermetic: no network, no live model, no pre-existing daemon needed.
Integration tests spawn their own daemons on temp sockets.

## Architecture in one paragraph

`engramd` owns the canonical SQLite database (WAL) at `~/.engram/brain.sqlite` and is
the ONLY writer: every mutation flows through a prioritized single-writer
scheduler (`src/scheduler.rs`); reads use a bounded read-only pool. CLI, proxy,
and MCP talk to it over a Unix socket (`~/.engram/engramd.sock`, mode 0600). The
world model (`src/world/`) holds a provenance-bearing code graph built by
tree-sitter (Rust/Python/JS/TS) or an external Graphify export. Memories are
typed and evidence-gated (`src/memory.rs`, `src/experience.rs`): agent claims
cap at 0.50 confidence and only deterministic/observational evidence tied to a
successful episode can promote them. The context compiler (`src/context.rs`)
selects under a hard token budget and explains every injected item. The proxy
(`src/proxy.rs`) fronts an OpenAI/Anthropic-compatible local runtime (MTPLX)
and injects compiled context; `x-engram-mode: off` is a true zero-injection
control path used as the benchmark control.

## Optional local-model launchers (installed)

- `claude-mtplx` — Claude Code against MTPLX Optimized-Speed (Q4) through the
  Engram proxy. `ENGRAM_MODE=off` disables injection; `ENGRAM_TOKEN_BUDGET` overrides.
- `claude-mtplx-quality` — same, Q8 Optimized-Quality (best local coder on
  this M5; ~32.7 GB peak, close heavy apps first).
- MTPLX serves on `127.0.0.1:8010`; the Engram proxy on `127.0.0.1:11435`.
  `claude-mtplx --server-status` / `--stop-server` manage them.

## Migration rules

Migrations are sequential numbered SQL files in `src/storage/migrations/`,
registered in `MIGRATIONS` in `src/storage/mod.rs` with
`LATEST_MIGRATION_VERSION` bumped to match. Never edit an applied migration;
never skip a number. Databases newer than the binary are refused (downgrade
protection), so bump-and-reinstall order matters: build, install, restart
`engramd`.

## Gotchas (verified)

- `status` is a READ-ONLY special variable in zsh. The launcher scripts in
  `scripts/` are zsh; assigning `status=` crashes them. This already broke
  `--server-status` once.
- Remotes: `origin` is the company repo (SSH), `mirror` is the personal
  mirror (HTTPS). Push `main` to both. Account/credential specifics are
  machine-local (gh + gitconfig), never repo docs.
- A running `engramd` predating a schema bump keeps serving old response shapes.
  After `cargo install`, restart it (`engram daemon stop`; next CLI call
  auto-starts the sibling binary) or clients may see deserialization gaps.
- One daemon per database (`src/lifecycle.rs`): `engramd` holds an exclusive
  `flock` on `<db>.daemon.lock` from before migrations and job recovery until
  the process exits, and refuses to start while a daemon from a build without
  the lock still has the database open (found with `lsof`/`/proc`). Stopping
  keeps the listener open and answers `draining` (503) while it finishes
  requests, the job, a judge call and queued writes (bounded 120 s + 10 s);
  closing the listener first let a hook start a second daemon that migrated
  and requeued jobs under the first. A daemon spawn closes every inherited
  descriptor except stdio (`pre_exec`, the crate's only `unsafe`): macOS std
  pipes get close-on-exec only after `pipe()`, and a daemon that inherited a
  hook's stdin pipe kept that hook from ever seeing EOF.
- Structural freshness needs the PRIOR scan's dirty-path set to catch files
  whose local edits were reverted while other files stayed dirty. It is
  persisted in `structural_scans.changed_paths_json` (NULL ⇒ conservative full
  rescan). Do not "optimize" the plan back to fingerprint-only comparison.
- Registration writes `.engram/project.toml` into the primary checkout, which
  dirties its working tree; linked worktrees share that identity without new
  metadata directories. First-registration Git state is recomputed after the
  write on purpose. `.engram`/`.git` paths are excluded from changed-path
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
- An auto-started `engramd` inherits macOS privacy (TCC) access from the app
  whose hook or client started it (`engram daemon status` shows the chain).
  Without access to `~/Documents` it could stat but not read checkouts there,
  and Git failures read as "no Git", so registrations made in that state
  recorded no remote or root commit (both projects registered for this
  repository lack them). Unreadable checkouts are now `AccessDenied`
  errors (`permission_denied` API code) and `checkout_access` fails in
  `engram doctor`; `engram daemon restart` from a terminal fixes it. Missing
  remote/root identity is backfilled once Git is readable (never the remote
  while another project could be the same repository).
- Codex runs `$CODEX_HOME/hooks.json` handlers only after `/hooks` trust,
  stored as `[hooks.state."<canonical CODEX_HOME>/hooks.json:<event_snake>:<group>:<handler>"]
  trusted_hash = "sha256:…"` (rule in `src/codex_trust.rs`, verified against
  Codex CLI 0.155.0-alpha.2.6 via its app-server `hooks/list`).
- `project_aliases.path` is UNIQUE across projects: re-pinning a registered
  checkout's `project.toml` to another existing id makes that checkout fail to
  resolve. There is no merge command yet (USER_GUIDE "Duplicate project
  registrations").
- Project identity: repos with only LOCAL-path remotes mint a NEW project id
  per clone unless `.engram/project.toml` travels with the checkout (an
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
(spec P10). The evaluation harness (`src/evaluation.rs`, `engram eval run`)
executes paired off/brain trials in isolated repo copies with per-arm
databases and sockets. Published results and limitations are in `docs/BENCHMARKS.md`. Commit new evidence
under `docs/perf-evidence/` with dated inputs, isolation checks and failed trials
retained in the denominators.
