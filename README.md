# Local Engineering Brain

LEB is a local-first, evidence-grounded engineering world model for coding agents. It keeps
canonical state in a per-user Rust daemon, compiles a small version-aware context for each model
request, and provides the same model/runtime an exact brain-off control path.

The implementation is V1-capable: stable project identity, Git and structural ingestion,
Graphify import, typed episodes/evidence, evidence-gated memory, validity filtering, hybrid
retrieval, bounded context compilation, OpenAI/Anthropic proxying, MCP, CLI inspection and purge,
backup/export, and paired evaluation. The detailed contract is in
[`LOCAL_ENGINEERING_BRAIN_SPEC.md`](LOCAL_ENGINEERING_BRAIN_SPEC.md).

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
 local Anthropic/OpenAI runtime (MTPLX, Ollama, llama.cpp, vLLM, MLX)
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

MTPLX/Claude Code setup is documented in [`docs/CLAUDE_CODE_MTPLX.md`](docs/CLAUDE_CODE_MTPLX.md).
The installed launchers are `claude-mtplx` for Optimized Speed and `claude-mtplx-quality` for the
Q8/Optimized Quality variant recommended on M3-and-newer Macs.

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

## Configuration and resource model

Priority: request/CLI override → project `.leb/config.toml` → user
`~/.config/leb/config.toml` → defaults. See [`config/leb.example.toml`](config/leb.example.toml).

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
