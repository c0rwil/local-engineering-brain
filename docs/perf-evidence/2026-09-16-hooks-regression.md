# Hook regression after capture changes — 2026-09-16 UTC

The post-rename hook-pipeline matrix passes all ten brain trials. The control
passes eight, failing both seeds of t21 (the in-file unit-test convention).

| Arm | Successes | Mean wall | Compiled context bytes |
|---|---:|---:|---:|
| Off | 8/10 | 86.474 s | 0 |
| Brain | 10/10 | 81.022 s | 26,664 total |

The paired mean-wall delta is **−6.30%**, calculated over all ten trials in each
arm, including failed validators. There were no timeouts. These are ten small
synthetic pairs, not a general estimate of real engineering productivity.
Fidelity remains 10/10 versus the September 3 hook result. The changed control
score and elapsed time do not establish a causal improvement across dates.

## Reproduction and isolation

- Implementation commit `581ca21`; installed runtime build
  `af5c488300e1ca0d5c5b0808d410d3780743364acdb5fd44ccf7f1ce4aab14e6`.
  The reviewed Rust source was unchanged between that build and the commit.
- Stock Claude Code, `claude-opus-4-8`, original t20–t24 prompts and validators,
  seeds 1–2, 900-second walls. The public conveyor fixture is pinned at
  `7311490905964b95284a2eed41bbb160b7bb0dec`, with `.engram/project.toml`
  carrying the snapshot project identity in every copy.
- Frozen snapshot SHA-256:
  `04ce3a872fa11468d20d56eb61e38ac0f3522ceb4ccac39d4caff072edd7448d`.
  Both arms use isolated databases and sockets. The detached launcher uses
  `nohup`, `disown`, and `caffeinate -dims` in a separate process session.
- Both arms explicitly disable globally configured MCP servers. Only the brain
  arm enables hooks. The control's task and validation surface is unchanged.
  Transcript audits found no MCP calls or Engram CLI calls in any scored cell.
- Read-only collection from each arm's context records found zero bytes in all
  controls and positive delivery in every brain cell: 2,222–4,444 bytes per
  trial, 556 tokens per compile. Twelve compiles occurred across ten brain
  trials. Existing compact-injection, budget, and prefix-stability tests pass.

Raw [runs](2026-09-16-regression/hooks-runs.jsonl),
[comparison](2026-09-16-regression/hooks-comparison.json),
[cell audit](2026-09-16-regression/hooks-audit.json),
[byte telemetry](2026-09-16-regression/hooks-telemetry.json),
[configuration](2026-09-16-regression/hooks.yaml), and
[input hashes](2026-09-16-regression/input-hashes.json) are archived together.
Full public-fixture transcripts and validator output remain under
`/private/tmp/engram-regression-20260916/hooks/`; their hashes are in the cell audit.

## Discarded launch and final registration

An initial launch revealed that disabling hooks did not disable global MCP.
Its first control called Engram recall and received 454 bytes (114 tokens).
That launch is excluded in full, including successful cells. Its
[audit](2026-09-16-regression/discarded-run-audit.json) records the MCP calls and
transcript hashes. Three synthetic episodes that it wrote into the live brain
were removed after a canonical backup, preserving audit history and real work.
The [adapter patch](2026-09-16-benchmark-isolation.patch) records the correction.

After all twenty valid cells and their comparison were written, the CLI failed
to register the results because the live daemon socket refused the connection.
The daemon was restarted in a detached process with a retained log; the same
twenty records were then registered through the production evaluation API.
No scores or walls were reconstructed and no scored trial was rerun for this
registration repair. The original
[registration error](2026-09-16-regression/hooks-final-registration-error.txt)
is preserved.

The older hook transcript archive is unavailable at its documented local path,
so this work cannot retrospectively verify its global-MCP isolation. Historical
headline results remain dated separately from this audited rerun.
