# Local Engineering Brain

LEB is a local-first, evidence-grounded engineering world model for coding agents. It keeps
canonical state in a per-user Rust daemon, compiles a small version-aware context for each model
request, and provides the same model/runtime an exact brain-off control path.

The implementation is V1-capable: stable project identity, Git and structural ingestion,
Graphify import, typed episodes/evidence, evidence-gated memory, validity filtering, hybrid
retrieval, bounded context compilation, OpenAI/Anthropic proxying, MCP, CLI inspection and purge,
backup/export, and paired evaluation. The detailed contract is in
[`LOCAL_ENGINEERING_BRAIN_SPEC.md`](LOCAL_ENGINEERING_BRAIN_SPEC.md).

Beyond the core, LEB is a **living engineering encyclopedia** for multi-repo workspaces:

- **Workspace browser** at `http://127.0.0.1:11436` whenever the daemon runs: an SVG service
  map of repositories and their declared relations, clickable service pages (facts, links,
  validated memories, per-module dependency diagrams), an activity feed from the event log,
  and a synthesized architecture guide that flags itself when the workspace drifts past it.
- **Two graph views that never overwrite each other**: documentation defaults to fetched
  `origin/main` (indexed in throwaway worktrees — no checkout is ever touched), toggleable to
  the local branch; agent-facing context always follows the local tree being edited.
- **Commit-driven freshness**: a daemon watcher re-indexes on commits and branch switches
  (never on uncommitted edits) and periodically refreshes the shared-truth views, so diagrams,
  links, and the Obsidian vault export update without any manual step.
- **Automatic cross-repo linking**: manifest path deps, submodules, and references to other
  services' declared hostnames (`leb fact set hosting.base_url …`) create evidence-stamped
  links; linked repos' validated knowledge then injects into each other's compiled context.

## Architecture

```text
Claude Code / agent / OpenAI client
          | MCP + loopback HTTP
          v
  leb-mcp / leb-proxy
          |
          v
        lebd  -- one bounded priority writer, bounded reader pool
          |
          +-- SQLite WAL/FULL (canonical events, graph, evidence, memory, metrics)
          +-- rebuildable indexes and byte-bounded, generation-invalidated read caches
          +-- Git, tree-sitter, optional Graphify export
          |
          v
 Anthropic/OpenAI runtime (local by default; remote only by explicit opt-in)
```

SQLite is deliberate for the single-user local product: one daemon owns mutation ordering, WAL
supports concurrent readers, transactions keep events and projections coherent, and there is no
second database service consuming RAM. PostgreSQL is the planned team/distributed boundary, not a
faster default for a Mac-local workload. Redis is unnecessary because canonical state and request
projections are already local to one daemon; the daemon's own byte-bounded read caches are
invalidated by canonical storage generations. See the ADRs in [`docs/adr`](docs/adr).

## Install

Requirements: macOS or Linux, Rust 1.85+, Git, and `shasum`.

```bash
./scripts/install.sh
export PATH="$HOME/.local/bin:$PATH"
leb doctor

# One command from empty machine to living encyclopedia:
leb onboard ~/code                 # registers, indexes, auto-links, vault
open http://127.0.0.1:11436        # served by the daemon whenever it runs

# Make stock `claude` sessions draw from and feed the brain — once, machine-wide:
leb hooks install --global         # user-level hooks: session brief, context
                                   # injection, pre-read file shapes, evidence
                                   # capture; silent no-op in untracked repos
leb agent-md --global              # the agent tool loop, in ~/.claude/CLAUDE.md
leb hooks codex --install          # Codex: the same automatic hook loop
# (or per repo: leb hooks install <repo> / leb agent-md --install <repo>)

# Distribute agent-native skills to every agent on the machine:
leb skills add https://github.com/DietrichGebert/ponytail
# installs each SKILL.md into ~/.claude/skills and ~/.codex/skills, where
# Claude Code and Codex trigger them natively when relevant to the task;
# `leb skills update` pulls and re-installs every pack.

# Day-to-day guide: docs/USER_GUIDE.md
# Team/company rollout: docs/COMPANY_ROLLOUT.md (central source of truth
# from origin/main, CI webhooks, cross-instance sync, incremental freshness).
```

The atomic installer builds all four binaries (`leb`, `lebd`, `leb-proxy`, and `leb-mcp`), installs
all three MTPLX launchers, and writes checksums plus a source identity to
`~/.local/share/leb/install-manifest.json`. CLI commands that
need the canonical writer and both edge binaries health-check and auto-start their sibling daemon.
Defaults are `~/.leb/brain.sqlite`, `~/.leb/lebd.sock`, and a loopback-only proxy at
`127.0.0.1:11435`. Evidence artifacts larger than `storage.blob_threshold_bytes` (16 KiB by
default) are redacted and then stored content-addressed under `~/.leb/blobs/`; reads, exports,
and purges resolve them transparently, and `leb doctor` reports blob-store health.

## Core workflow

```bash
# Register and index native structure plus bounded Git history.
leb status /path/to/repo
leb index /path/to/repo

# Or import a bounded Graphify JSON snapshot.
leb index /path/to/repo --graphify /path/to/graphify-export.json

# Inspect graph/context/memory.
leb graph Reactor --path /path/to/repo
leb context "fix abort cleanup" --path /path/to/repo
leb memory list --path /path/to/repo

# Capture an evidence-grounded episode.
leb task start "fix abort cleanup" --path /path/to/repo --json
leb task attempt tsk_... "move cleanup into child lifecycle" --outcome success --file src/reactor.rs
leb task evidence test_result --path /path/to/repo --task-id tsk_... \
  --strength deterministic --command "cargo test cleanup" --exit-code 0
leb task finish tsk_... --outcome success --accepted-by-user true
leb learn "Cleanup runs in the child lifecycle" --path /path/to/repo \
  --task tsk_... --evidence evd_...

# Candidates never auto-inject until evidence-gated promotion or explicit human override.
leb memory promote mem_... --to observed
leb memory promote mem_... --to supported
leb memory promote mem_... --to validated
```

Every command intended for automation has `--json`. Use `leb --help` for the complete surface.

## Runtime proxy and brain-off control

```bash
leb-proxy --runtime http://127.0.0.1:8000 --bind 127.0.0.1:11435
```

Point an Anthropic client at `http://127.0.0.1:11435` and send
`x-leb-project-path: /absolute/repo`. Use `x-leb-mode: off` for the paired control; without a
project header, off mode bypasses the daemon and forwards the semantic request unchanged. OpenAI
clients use `/v1/chat/completions`; Anthropic clients use `/v1/messages`. Streaming and tool schemas
pass through end-to-end. If the brain daemon is temporarily unavailable, the default fail-open path
forwards unchanged and adds `x-leb-degraded: context_unavailable`; set
`proxy.failure_mode = "fail_closed"` for policy-sensitive or controlled evaluation use.

When a brain-mode request carries Claude Code's `x-claude-code-session-id`, the proxy pins the
first compiled LEB context block for that session, resolved project, and effective token budget in
an 8 MiB byte-bounded process-local cache. Later turns reuse those exact UTF-8 bytes; off mode never
reads or populates this cache. Each injected response reports `x-leb-injection-sha256`,
`x-leb-injection-bytes`, and `x-leb-session-cache` (`hit`, `miss`, or `disabled`), while
`GET /leb/metrics` exposes aggregate injected bytes and session-cache hit/miss counts. No context
content is logged by this telemetry.

This stability intentionally snapshots LEB guidance for the life of the Claude Code session.
Project resolution and freshness checks still run, but new or invalidated memories and changed
source-derived guidance do not alter the already-pinned injection. Start a new Claude Code session
after material source, configuration, or memory changes when freshness is more important than
within-session prefix identity.

MTPLX/Claude Code setup is documented in [`docs/CLAUDE_CODE_MTPLX.md`](docs/CLAUDE_CODE_MTPLX.md).
The installed launchers are `claude-mtplx` for Optimized Speed and `claude-mtplx-quality` for the
Q8/Optimized Quality variant recommended on M3-and-newer Macs.

### Explicit remote-runtime opt-in

Remote runtimes are disabled by default. Enabling one requires two independent settings in the
user or project configuration; turning off the broad local-only policy by itself is not enough:

```toml
[network]
local_only = false

[proxy]
runtime_url = "https://api.anthropic.com"
allow_remote_runtime = true
```

The proxy still binds only to a loopback address. A remote runtime URL must use HTTPS and cannot
contain URL credentials, a query, or a fragment. Supply credentials through the normal client
headers instead: `Authorization`, `x-api-key`, `anthropic-version`, and other provider headers are
forwarded unchanged. LEB strips its private `x-leb-*` control headers and hop-by-hop transport
headers before forwarding. Request tools and streaming response content pass through without
translation; brain-off mode also preserves the request's model, messages, system content, tool
schemas, streaming flag, and other runtime parameters semantically unchanged.

Remote use sends the model request, including any prompt or injected repository context, to that
provider. Enable it only for repositories and data that are approved for that provider. Put the
settings in `~/.config/leb/config.toml`, or opt in for one process without changing global config:

```bash
leb-proxy --runtime https://api.anthropic.com \
  --allow-remote-runtime --no-local-only \
  --bind 127.0.0.1:11435
```

The two flags are intentionally independent and must both be present for a remote CLI-selected
runtime. Keep secrets out of the URL.

## MCP

```bash
claude mcp add --scope user leb -- "$HOME/.local/bin/leb-mcp"
claude mcp get leb
```

The stdio adapter is stateless and requires explicit project/task/context IDs for stateful calls.
It auto-starts `lebd` and exposes project status, world query/impact, recall/explain, memory,
failures, skills/invariants, outcome marking, human decisions, and logical forget.
`leb_forget` requires both `project_id` and `memory_id`, and verifies ownership before mutation.

## Backup, export, and purge

```bash
leb backup create "$HOME/.leb/backups/brain-$(date +%Y%m%d).sqlite"
leb backup verify "$HOME/.leb/backups/brain-20260828.sqlite"
leb export ./project.ndjson --path /path/to/repo
leb memory forget mem_...
leb memory purge mem_...
```

Backups use SQLite's consistent snapshot operation. Exports stream versioned NDJSON rather than
loading a large brain into RAM. Forget is a non-destructive logical archive excluded from retrieval;
purge irreversibly removes content, FTS entries, links, and derived context references while
retaining a content-free immutable audit event. Purge cannot rewrite pre-existing backup copies;
the CLI reports that limitation explicitly.

## Measured results

Every claim below is backed by a dated evidence file in
[`docs/perf-evidence/`](docs/perf-evidence/) with pinned model identity,
verified injection telemetry, and honest controls (brain-off is the same
model, runtime, and harness with zero injection).

- **Frontier tier (Claude Code + Opus 4.8 through the LEB proxy):** on
  tribal-knowledge fidelity probes — unwritten team conventions enforced by
  mechanical style verifiers — the memoryless control passed 5/12 across
  four seeds while brain-arm compositions passed 24/24, at 36-100 s per
  task. Failure-stage
  replay shows all control failures are pure style failures: fast models
  solve the ticket and skip the unwritten rules (2026-08-31).
- **Local tier (Qwen 27B on this machine):** on capability probes with
  seeded episode lessons, brain 12/16 vs control 10/16 with 14% lower mean
  wall time; wins align with the seeded lesson families and the brain arm
  was never worse on any task. On fidelity probes the local bottleneck is
  wall clock, not compliance: replay shows most timed-out runs had already
  produced compliant patches, and the oriented brain arm finished in time
  3/5 vs 1/5 (2026-08-31). At calibrated 2700 s walls across seeds 2-3
  (all cells warmed and activity-verified), **brain passed 10/10 vs
  control 7/10 with 33% lower mean wall time** — injected tribal knowledge
  prevents the exploratory spirals that burn the clock (2026-09-01).
- **Compact injection:** the deterministic compact rendering held success
  parity at 33% fewer injected bytes on frontier A/B; it stays opt-in
  until a fidelity-family A/B (2026-09-01).
- **Learning the engineer:** preferences distilled purely from episodes
  (fail-then-pass verification + agreeing attempt summaries; no operator
  claim, no LLM) pass the same promotion gates and reproduce the seeded
  effect: distilled-brain 4/4 vs control 3/4 at 41% lower wall on the
  convention probes (2026-09-02).
- **Real-repository recall:** seeded a production Rust backend's 11
  operator-guide gotchas through the real learning pathway and probed with
  natural, non-verbatim developer questions: 11/11 recall@1, fully local
  (2026-09-01).
- **Prompt-cache stability:** injected context is byte-stable per session
  (session-pinned), measured at 11-24 consecutive cache hits per agentic
  session against both local and Anthropic runtimes.
- Null and superseded results are recorded with the same prominence,
  including a retroactive annotation of every pre-identity-pinning brain
  arm (prefix-only injection; see 2026-08-31 integrity note).

## Paired evaluation

Copy [`bench.example.yaml`](bench.example.yaml), pin the model/runtime identity, and run:

```bash
leb eval run ./bench.yaml
```

To verify the evaluation machinery itself on a clean machine, run the bundled deterministic smoke
fixture first:

```bash
leb eval run ./bench.smoke.yaml
```

The smoke fixture proves isolation and artifact/report production; it does not make a model-quality
claim. Use a pinned real-agent command in `bench.example.yaml` for efficacy measurements.

Set `brain_snapshot` to a standalone file created by `leb backup create` when evaluating learned
state. Each task/seed runs once with `LEB_MODE=off` and once with `LEB_MODE=brain` in fresh
repository copies. Every arm gets a byte-identical baseline copied to a distinct database, Unix
socket, state root, and proxy port; process-group cleanup prevents child agents/servers from leaking
into another arm. Order alternates. The snapshot hash is folded into the configuration hash and
recorded on every run. The runner emits `runs.jsonl`, `comparison.json`, and `comparison.md`; it
never claims improvement unless the measured report does.

For an approved synthetic frontier-model fixture, a benchmark config may request credential
passthrough by name:

```yaml
pass_env: [ANTHROPIC_API_KEY]
```

The default is empty. Only `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are accepted, a requested
value must exist in the runner's environment, and the value is supplied identically to the agent
phase of both arms without being written to configuration hashes or reports. `pass_env` gives the
benchmarked agent—and processes it launches—access to that secret. Never enable it for untrusted
code or private repositories; keep cloud evaluation confined to an approved synthetic fixture.

## Configuration and resource model

Priority: request/CLI override → project `.leb/config.toml` → user
`~/.config/leb/config.toml` → defaults. See [`config/leb.example.toml`](config/leb.example.toml).

`context.compact_injection = true` selects deterministic, bounded memory blocks that omit full IDs
and rationale boilerplate while retaining canonical source IDs and `why` records for audit. It is
off by default for an exact verbose/compact ablation. With unchanged eligible memory and repository
state, rendered LEB context is byte-stable across turn/request IDs and contains no volatile metadata.

- canonical writer queue, read pool, request sizes, connections, scans, Graphify imports, Git
  history, traversal, context tokens, and prepared data are hard-bounded;
- no repository graph or full memory corpus is retained in process memory;
- expensive parsing/retrieval runs outside the writer critical section;
- semantic/vector/remote embeddings, automatic learning, raw transcript retention, and derived
  caches are explicitly rejected in V1 rather than silently accepting inert settings;
- durable authored text is conservatively secret-redacted, including configurable bounded regexes;
  evidence artifacts store digests only;
- Unix socket, database, WAL/SHM sidecars, backups, and exports default to user-only.

## Release gates

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
cargo build --release --locked
```

The tests include process-kill rollback, migration/downgrade protection, bounded concurrency and
backpressure, Git fixtures, native and Graphify graph contracts, poisoning/promotion/invalidation,
deterministic context budgets, real proxy/runtime forwarding, real MCP stdio plus daemon autostart,
paired benchmark isolation, backup/export, redaction, and CLI process integration.
