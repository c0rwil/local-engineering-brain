# Final product-build local regression: runtime blocked — 2026-09-17 UTC

The final installed implementation is `040b666`, source hash
`5b77dd18e9454212d34f3254e0ecf56c28c78284adc264298ad0a7112fc41417`.
**There is no final-build local score.** The full planned matrix has 20 cells
(five tasks, two modes, repetitions 2–3), with 2,700-second limits per cell.
The corrected matrix never reached scoring because its required local model
could not become ready and complete warmup.

## Attempts and diagnosis

The first launch warmed a dedicated model on port 8013, but the evaluation
harness clears the inherited environment. The scored adapter therefore used
its default port 8010 and started another model. That setup was invalid. It
was stopped before any trial completed, and its transcript, telemetry and
input hashes are retained in the discarded-run audit. No result from it enters
a quality or latency denominator.

The corrected configuration places `MTPLX_CLAUDE_PORT=8013` and the dedicated
log directory explicitly in every scored command. A fresh dedicated process
exceeded the wrapper's initial 90-second readiness window. A detached resume
then waited up to 180 health checks, separated by five seconds, before warmup.
It exhausted that window without a healthy listener; no corrected trial began.

A process sample during the wait showed MLX evaluation waiting for a Metal
shared event and a Metal command queue waiting in an IOGPU submission call.
This establishes where the process was waiting, not a proven driver root cause.
The prior dedicated model processes had not fully exited after termination.
Restarting the benchmark model did not restore readiness. Raw process samples
and logs remain local; the published diagnostic artifact contains hashes and
selected observations only. No unrelated application or system service was
terminated, and no reboot was attempted.

The runtime must recover before the unchanged full matrix can be completed.
A restart of the host may be needed; this report does not claim one is sufficient.
No cloud model was substituted for this local axis.

## What remains valid

The earlier [September 16 local matrix](2026-09-16-local-regression.md) completed
20 trials with 2/10 success in each arm, dominated by runtime errors. It used an
earlier build and remains historical evidence, not a substitute final-build score.
The [final hook regression](2026-09-17-hooks-product-regression.md) is complete:
7/10 control versus 10/10 Engram, with audited zero-context controls.

Configuration, adapters, model-file hashes and runtime identity are retained in
[the regression directory](2026-09-17-product-regression/). YAML sampling values
are metadata; the harness does not enforce them, and these adapters do not
forward repetition IDs as model RNG seeds. Neither deterministic decoding nor
local speed or quality improvement is claimed.
