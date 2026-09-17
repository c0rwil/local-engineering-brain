# Living product knowledge and capture — 2026-09-17 UTC

Implementation: `040b666` (product catalog `879b5a5`, impact correction
`8ff20d8`, internal-synthesis isolation `040b666`). Installed source hash:
`5b77dd18e9454212d34f3254e0ecf56c28c78284adc264298ad0a7112fc41417`.
Schema 12 was installed after a WAL-safe snapshot. Doctor reports integrity,
foreign keys and WAL healthy. The source was reviewed independently and the
reported correctness issues were fixed before this build.

## What was measured

The reproducible [product evaluator](../../scripts/product_eval.py) starts an
isolated daemon with vault export, watchers and jobs disabled. Its two synthetic
repositories exercise product intent, a producer/queue/consumer contract, a
separate unanchored consumer, an unrelated environment, source changes and the
lesson lifecycle. All **21/21 checks pass** on the installed binary. The
[JSON result](2026-09-17-product-eval.json) records the binary and evaluator hashes.

This is a correctness evaluation. It does not establish faster human onboarding,
better model task success, or deployment compatibility. The recorded bundle
latency is one diagnostic sample, not a latency distribution or performance claim.

The [real-flow check](2026-09-17-product/real-flow-check.json) uses four existing
local product repositories. The local brain now contains 23 catalog records plus one imported legacy guide,
two ordered flows, ten distinct dependency contracts, four generated onboarding
sections awaiting review, and 57 source stamps. A task bundle uses 3,992 of its
4,000 estimated-token budget. Changing an API source reports ten directly
intersecting records and seven possible impacts, including all three relevant
consuming services. Private source, topology and generated prose remain in the
local brain; only aggregate verification counts are published here.

The real walkthrough exposed an impact-display bug: endpoints seeded from a
directly changed contract were traversed but omitted from the possible-impact
list. The corrected evaluator covers both this case and unanchored multi-hop
consumer traversal. Inferred records also show `NeedsReview` when a previously
known source changes; inference no longer hides source drift.

Browser checks covered product/flow/section editing, source inspection, history,
bounded task bundles, accepting a synthetic observation, and change impact.
The desktop and 375×812 views were inspected; the narrow view had no horizontal
page overflow. The installed real-product flow and navigation were verified.
No browser errors appeared during the final review/impact checks. The Rust
suite passes **263 tests**, all targets and features; strict clippy and formatting
pass. Details are in [verification.json](2026-09-17-product/verification.json).

## Capture and historical replay

[Capture metrics](2026-09-17-capture-product.json) preserve all historical tasks:
161 total, 159 closed, 52 successful, 107 abandoned. Acceptance is known on
28/159 closed tasks; 24/159 are explicitly accepted. Of the 52 successful tasks,
26 have unknown acceptance. There are 50 memories, three validated (6%).
Per-session conversion and first-validation latency remain in the JSON report.
No unknown acceptance was inferred from a commit, merge, silence or lesson review.

The unchanged six-case replay still delivers **1/6** expected lessons, the
previously recovered traffic-pin lesson. The other five are retained as misses.
This implementation does not claim that shipping a review UI retroactively
validates historical knowledge. One existing guide-flags candidate, with its two
original qualifying verifications, was surfaced in the review inbox and remains
pending. No new verification or user acceptance was invented.

The growing abandoned-task count exposed a distinct capture defect. **50**
historical abandoned prompts match the internal architecture-synthesis template,
including all **37** new abandonments since the prior capture snapshot.
The subprocess inherited global hooks and MCP tools, so internal prose generation
opened engineering episodes. The new invocation disables hooks, tools, MCP and
session persistence. A hermetic subprocess test verifies the actual arguments.
The [audit](2026-09-17-product/internal-synthesis-audit.json) contains counts and
timestamps only; historical tasks remain intact. This explains those matching
rows, not every abandonment or the original twelve scheduler failures.

Brief relevance is still unscored without labeled emitted items. Cumulative
abandonment and acceptance percentages are not prospective improvement claims.
Future real sessions must demonstrate better capture conversion and fewer false
brief items. The evaluator deliberately reports missing measurements as null.

## Regression discipline

The existing hook and wall-calibrated local families are rerun against the
installed build with unchanged prompts, validators, seeded memories and pinned
fixture identity. Cloud arms use only the reviewed synthetic conveyor fixture.
Control arms disable hooks/MCP as appropriate and must inject zero bytes. The
local model uses a dedicated port, a fresh server and a disposable warmup before
scoring; local walls remain 2,700 seconds. Every launch is detached under
`nohup` + `caffeinate -dims`, with per-arm context telemetry retained.

An initial adapter preflight rejected all hook launches because the copied
adapter lacked its adjacent fixture acknowledgment; no model call was made.
The original acknowledgment was restored after checking the exact fixture HEAD
and author. A subsequent partial matrix was stopped when the real-flow bug was
found. Both sets of artifacts are retained separately, excluded from the final
build matrix, and must not be represented as completed final-build measurements.
Completed and failed model trials in the final matrix remain in its denominators.

The first final-build local attempt targeted the default model port after the
harness cleared inherited environment variables. It was stopped before any
trial completed and retained as an invalid setup attempt. The corrected matrix
sets the dedicated model port explicitly in every scored command and repeats
warmup before scoring. No result from that attempt enters the final denominator.

The benchmark YAML records sampling metadata but the harness does not enforce
it. The adapters also do not pass repetition IDs as model RNG seeds. Runs use
the recorded runtime/client behavior; these repeated pairs are not claimed to be
bitwise reproducible seeded decoding. Runtime defaults are archived separately.

The completed [final hook matrix](2026-09-17-hooks-product-regression.md) scores
7/10 control versus 10/10 Engram. Every control has zero compiled-context bytes and every
brain trial compiled 2,222 bytes; no MCP or explicit brain CLI leakage was found.
Other hook payloads, such as file-shape text, are outside this byte metric.
Mean and median walls move in opposite directions, so no speedup is claimed.

The [final local rerun](2026-09-17-local-product-runtime-blocked.md) is blocked
before warmup: a dedicated runtime never became ready, and two process samples
show the same Metal/MLX waits. It has zero completed valid trials and no score.
The earlier 2/10 versus 2/10 local matrix remains historical evidence only.
