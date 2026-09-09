# V1 Definition-of-Done validation — 2026-08-29

Machine: Apple M5 Pro, 48 GB, macOS 26.4. Binaries: install-manifest build
`0e19a4e39b077e05454c38578e066c7de4c92a41a447e461fdceb23c902b4d3b` (repo HEAD at
validation time). Fresh state root (`~/.leb-e2e`), fresh fixture repo. Model:
`Youssofal/Qwen3.8-27B-MTPLX-Optimized-Quality` via MTPLX on 127.0.0.1:8010.

| # | DoD item (spec §48) | Result | Evidence |
|---|---|---|---|
| 1 | Install without cloud account | PASS | `scripts/install.sh`, no network beyond crates already vendored in lockfile builds |
| 2 | Register a Git repository | PASS | `leb status` → stable `prj_…` id |
| 3 | Build structural world model | PASS | `leb index` 1 file → 3 nodes; `leb graph Reactor` → Type node |
| 4 | OpenAI-compatible local model through proxy | PASS | `/v1/messages` brain + off modes against live Q8; proxy counters `{forwarded:1, injected:2}` |
| 5 | Capture an engineering episode | PASS | task → attempt → finish via CLI |
| 6 | Link passing/failing test to attempt | PASS | deterministic `test_result` evidence, exit 0 and exit 1 |
| 7 | Evidence-gated promotion | PASS | candidate → observed → supported → validated |
| 8 | Invalidate on implementation change | PASS (after fix) | First run exposed a real gap: CLI-learned lessons had no symbol anchors, `invalidated_memories: 0`. Fixed (distillation derives node links from touched files); re-run: `invalidated_memories: 1`, memory `suspect` |
| 9 | Retrieve + compile valid context later | PASS | memory injected at 80/800 tokens |
| 10 | Explain why each item appeared | PASS | per-item why: confidence/exact/lexical/semantic/structural/utility/validity |
| 11 | Purge memory + derived material | PASS | list empty, zero injected memories afterward |
| 12 | Same benchmark brain OFF and ON | PASS | `leb eval run bench.smoke.yaml`, paired counterbalanced runs |
| 13 | Machine- and human-readable reports | PASS | `comparison.json`, `comparison.md`, `runs.jsonl` |
| 14 | Required test suite | PASS | 173 unit/integration + 15 property tests, fmt/clippy clean |
| 15 | Local latency/RAM/token budgets | PASS | daemon RSS 13–27 MiB vs 600 MiB soft limit; compile ms sub-millisecond warm; token selection ≤ budget |

Also exercised incidentally: stale-socket recovery, daemon build-identity
mismatch refusal (old daemon rejected by newer client with explicit
remediation), socket/database ownership pairing check, and the macOS Unix
socket path-length limit (state roots must keep socket paths short).

Fixes that came out of this validation, all landed on main the same day:
distilled/synthesized memories are anchored to touched-file symbols;
`leb-proxy` gained `--db` to pair with `--socket`.

The deterministic smoke benchmark proves harness isolation only. No
model-quality claim is made here; the real paired benchmark against the
`conveyor` fixture suite is the next recorded artifact.
