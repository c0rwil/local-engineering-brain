# LEB — next-session handoff

Paste-ready context for continuing Local Engineering Brain. Refreshed
2026-08-31 after the re-validation program completed. Supersedes the
2026-08-29 version below where they conflict.

## Current state (2026-08-31)

- Frontier arm is LIVE and green-lit: Claude Code + claude-opus-4-8 through
  the LEB proxy (stored login; wrapper `agent-claude-frontier.sh`).
  Opus is the fast iteration loop; the local Speed model is the shipping
  runtime and re-validation target. Launch matrices with `caffeinate -dims`
  from BACKGROUND commands only (foreground timeouts kill the process group).
- FLAGSHIP: with pinned project identity, all Opus brain arms passed 18/18
  tribal-fidelity probes vs 2/6 memoryless; locally brain 12/16 vs 10/16
  and −14% wall on capability probes. Every pre-pinning brain-arm result is
  void (prefix-only injection) — see perf-evidence 2026-08-31 files.
- Identity contract: benchmark fixtures/seeds pin `.leb/project.toml`
  (clone-identity fracture is unfixable by heuristics; documented in
  CLAUDE.md, regression-tested).
- Defaults: `context.review_standards_section` now defaults ON (best
  measured composition). `max_injected_memories`, `compact_injection`,
  `semantic_retrieval` remain opt-in pending fair tests.
- In flight/queued: wall-calibrated local fidelity rerun (2700 s walls);
  Opus multi-seed confirmation (opus-g2-off-s34.yaml /
  opus-g2-review-s34.yaml in leb-bench-target) queued behind it.
- Check `gh auth status` shows active account c0rwil before every push —
  something external flips it to corwil-agi.

---

# Original 2026-08-29 handoff (historical)

## Where things stand

- `~/Documents/brain` — the LEB repo, pushed to github.com/c0rwil/local-engineering-brain
  (private; gh account must stay `c0rwil`, NOT `corwil-agi`; the repo has a local
  insteadOf override so pushes go HTTPS). ~188 tests, schema at migration 7,
  CI/cargo-deny in place, all gates green: `cargo fmt --all -- --check`,
  `cargo clippy --all-targets --all-features -- -D warnings`,
  `cargo test --all-targets --all-features`.
- Installed via `scripts/install.sh` → `~/.local/bin` (leb, lebd, leb-proxy,
  leb-mcp, claude-mtplx, claude-mtplx-quality). Live DB `~/.leb/brain.sqlite`.
- `~/Documents/leb-bench-target` — benchmark lab: `conveyor` fixture repo
  (14 validated tasks, 6 trap families + controls, GROUND_TRUTH.md scoring key),
  `agent-claude.sh` wrapper, `pilot.yaml` / `full.yaml` configs, seed snapshots.
  Seed brain DB at `~/.leb-seed` (6 validated lessons, one per family).
- Evidence discipline: every measurement lands in `docs/perf-evidence/` with
  date+config-hash. Round 1 (8 tasks × 2 arms, seed 1, local Speed model):
  aggregate delta 0.0; brain won t11 (the designed strongest memory probe,
  with the trap explicitly reasoned about in its output) and lost t10 to a
  timeout; injection cost +25% wall. Pilot + DoD validation docs are alongside.
- CLAUDE.md in the repo carries verified gotchas — read it first. Critical ones:
  Unix socket paths must stay under ~104 bytes (state dirs must be short);
  after reinstalling, restart daemons (build-identity check refuses mismatched
  clients — including `daemon stop` itself; pkill the old lebd if wedged);
  `status` is read-only in zsh launcher scripts.

## Mission (user's words, expanded)

Make LEB *really work*: not just capability/bug-avoidance, but **learning the
engineer and the codebase** — preferences respected instead of overwritten,
the model speaking/working the way the codebase was built, no degradation of
founding principles. Get real data with margin for improvement, faster.

## Work plan, in priority order

### 1. Frontier-model benchmark arm (speed up iteration)

Run the same paired harness with Claude Code on Opus (and optionally Sonnet)
via the normal Anthropic API instead of only the 25 tok/s local model — runs
should drop from ~15 min to ~1-3 min, making seeds cheap.

- The proxy already fronts `/v1/messages`; point `leb-proxy --runtime
  https://api.anthropic.com` — BUT `network.local_only` (default true)
  rejects non-loopback runtimes by design. Add an explicit, documented
  config opt-in (e.g. `proxy.allow_remote_runtime = true` requiring
  `network.local_only = false`), verify auth-header passthrough (the proxy
  must forward `authorization`/`x-api-key`/`anthropic-version` untouched and
  keep stripping only `x-leb-*`), and confirm streaming/tool passthrough
  against the real API with one manual request before benchmarking.
- New agent wrapper (`agent-claude-frontier.sh`): plain `claude` headless
  with `ANTHROPIC_BASE_URL` pointed at the per-arm LEB proxy, model pinned
  (`--model claude-opus-5` — verify current id), same LEB_MODE/env contract.
- Fairness cautions to write into the evidence doc: frontier models may
  ceiling the current traps (both arms pass everything) — if so, that is a
  finding about task calibration, and the harder discriminators become the
  preference/fidelity probes (section 3). Privacy: the conveyor fixture is
  synthetic and fine to send to the API; do NOT benchmark against
  agi-backend-rust or other private repos through the cloud arm.
- Cost/rate: 16-run matrix on Opus ≈ tens of dollars; note spend in evidence.

### 2. Wall-time optimization: compact injection + prefix stability

Round 1 showed injection costs ~25% wall on a local model. Two levers:

- **Compact rendering**: memories are stored human-readable (right for audit)
  but injected verbatim. Add a derived, rebuildable "compact injection form"
  per memory — terse, deduplicated, structured notation (drop rationale
  boilerplate, collapse to `RULE:`-style lines, strip IDs to short refs) —
  cached in a sidecar column/table, regenerated on memory change, and used
  by the context compiler when `context.compact_injection = true`. Must be
  deterministic (no LLM in the loop for V1), ablatable (A/B verbose vs
  compact is a one-flag benchmark), and the `why`/explain path must still
  reference the canonical text. Measure tokens-injected and outcome deltas.
- **Prefix-cache stability**: MTPLX caches by prompt prefix. Ensure the
  injected block is byte-stable across turns within a session (stable
  ordering, no per-request timestamps/ids in rendered context) so multi-turn
  agentic runs re-hit the runtime prefix cache instead of re-prefilling ~20k
  tokens per turn. Audit `context.rs` rendering + proxy injection for any
  per-request variance; this may be the single biggest local wall-time win.

### 3. "Learning the engineer" benchmark axes (the real differentiator)

Extend leb-bench-target with **family G: preference/fidelity probes** —
mechanically verifiable, so no LLM judging is needed:

- Seed preference memories through the real pathway (e.g. "no `unwrap()` in
  library code — use typed errors", "config structs use the builder pattern",
  "doc comments state the why, never restate the signature", "commits use
  conventional-commit scopes", "public APIs never expose tick counts, only
  Durations"). Then tasks whose *naive correct solution violates the
  preference*: acceptance = functional tests PLUS a style verifier script
  (grep/clippy-config/custom assert) that fails on violation. Off arm should
  pass function but fail style; brain arm should pass both. That turns
  "respecting how the codebase was written" into a scored, paired metric.
- Non-degradation probes: tasks where the trap is deleting/flattening
  existing structure (removing doc comments, collapsing an intentional
  abstraction). Verifier asserts the preserved structure.
- Voice probes are hardest to verify mechanically; start with objectively
  checkable style (naming conventions, error-message format, spelling
  variant) and defer subjective tone.
- Wire the same probes into the synthesis path later: can LEB *distill*
  preferences automatically from episodes (spec 16), not just store them
  when told?

### 4. Immediate real-data steps (cheap, do first in parallel)

- Study the t10 brain-arm transcript (`~/lebbench-full/000007-*brain*/`):
  was the timeout injection-distraction or task difficulty? Feeds lever 2.
- Run seeds 2-3 locally overnight for variance on the existing matrix:
  `cd ~/Documents/leb-bench-target && sed -i '' 's/seeds: \[1\]/seeds: [2, 3]/' full.yaml && nohup ~/.local/bin/leb eval run ./full.yaml --output ~/lebbench-full-r2 --json > ~/lebbench-full-r2.json 2>&1 &`
  (start MTPLX Speed on 8010 first: see CLAUDE.md launcher notes).
- Record every result in `docs/perf-evidence/` — null results included.

### 5. Standing discipline

Off-mode stays semantically untouched (spec P10). Any runtime-compat or
compact-injection change must apply identically to both arms or be
brain-arm-only by explicit design, stated in the evidence doc. All gates
green before every push; benchmark configs pin model identity in
`model_hash`; keep worktree-per-agent workflow for parallel feature work
(migration numbers: next free is 8).
