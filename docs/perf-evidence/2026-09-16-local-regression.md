# Local regression: runtime failures dominate — 2026-09-16 UTC

The wall-calibrated rerun completed all twenty cells but achieved only 2/10
successes in each arm. It does not reproduce the September 1 task-success gain.
Twelve trials ended in runtime/API errors before validation, and four reached
the configured wall. These results must not be presented as an injection-quality
win or a clean comparison of the capture changes.

| Outcome | Off | Brain |
|---|---:|---:|
| Passed agent and validator | 2 | 2 |
| Agent/API error before validator | 5 | 7 |
| 2,700-second wall timeout | 3 | 1 |

Across both arms, eight API errors explicitly report cancellation through
`POST /v1/mtplx/cancel`; four report a stream-idle timeout. The transcripts do
not identify the actor issuing cancellation or establish the cause of idle
timeouts. The raw mean walls are 1,448.579 s off and 907.844 s brain. Earlier
failure terminates a trial sooner, so the computed −37.33% wall delta is **not
a supported speedup claim**. No failed trial was dropped from these totals.

## Method and recovery

- Implementation `581ca21`, unchanged Rust runtime build
  `af5c488300e1ca0d5c5b0808d410d3780743364acdb5fd44ccf7f1ce4aab14e6`.
- MTPLX 2.9.2 / Qwen3.8 27B Optimized-Speed Q4, the original public conveyor
  t20–t24 prompts and validators, seeds 2–3, all walls 2,700 seconds.
- Server restarted and a disposable agent task completed before scoring.
  The matrix ran detached under `nohup`, `disown`, and `caffeinate -dims`.
- Pinned fixture identity, per-arm databases/sockets, and the same frozen
  snapshot as the hook matrix. Both arms disable global MCP and hooks; this
  family measures proxy delivery only.
- All controls had zero compiled context bytes; all brain trials had positive
  context delivery (22,220 bytes total). No MCP or Engram CLI tool calls were
  found by the transcript audit. Delivery alone does not establish utility.
- A preceding attempt passed its first control, then aborted before recording
  that cell because the adapter queried the old `/leb/metrics` endpoint and
  received an empty body. Its artifacts are retained under
  `/private/tmp/engram-regression-20260916/local-metrics-endpoint-failure`.
  The adapter now uses `/engram/metrics`, checks HTTP errors, and still cleans
  up on metrics failure. A direct endpoint preflight passed, then the entire
  matrix restarted with a fresh warmup. That earlier cell is not counted.

## Evidence and unresolved gate

[Runs](2026-09-16-regression/local-runs.jsonl),
[comparison](2026-09-16-regression/local-comparison.json),
[cell audit](2026-09-16-regression/local-audit.json),
[failure classifications](2026-09-16-regression/local-failure-classification.json),
[telemetry](2026-09-16-regression/local-telemetry.json),
[configuration](2026-09-16-regression/local.yaml),
[adapter input hashes](2026-09-16-regression/local-input-hashes.json), and
[warmup](2026-09-16-regression/local-warmup.txt) are retained. Full transcripts
and validator output remain in `/private/tmp/engram-regression-20260916/local`;
the cell audit records their hashes.

The completed hook regression and hermetic gates remain green. A reliable
local-model regression comparison remains unresolved: identify cancellation and
stream-idle causes, establish an isolated and stable runtime, then repeat the
paired matrix under the same task/validator/wall conditions. Preserve this run
as observed reliability evidence; do not substitute historical success for it.
