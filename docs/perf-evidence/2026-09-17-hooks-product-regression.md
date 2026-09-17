# Final product-build hook regression — 2026-09-17 UTC

Installed implementation: `040b666`, source hash
`5b77dd18e9454212d34f3254e0ecf56c28c78284adc264298ad0a7112fc41417`.
This reruns the existing five-task hook family, paired off/brain, repetitions
1–2, with the reviewed synthetic conveyor fixture and frozen preferences.
No private repository entered a cloud arm.

| Measure | Control | Engram |
|---|---:|---:|
| Functional + convention success | 7/10 | 10/10 |
| Mean wall | 73.620 s | 71.546 s |
| Median wall | 66.034 s | 69.327 s |
| Timeouts / agent errors | 0 / 0 | 0 / 0 |
| Compiled-context bytes, total | 0 | 22,220 |
| Compiled-context bytes, per trial | 0 | 2,222 |
| MCP / explicit brain CLI tool calls | 0 / 0 | 0 / 0 |

Compiled-context bytes sum stored stable-prefix and dynamic-context UTF-8 bytes.
Additional file-shape, session-brief and relay payloads are outside this metric;
total hook payload bytes were not measured. Controls disable all hooks.

All three control misses passed functional verification. One omitted the
terminal-transition metrics counter (`t20`, repetition 2); two omitted an
in-file test for the new grammar (`t21`, repetitions 1 and 2). All ten Engram
trials passed both stages. No trial was removed from the denominator.

The mean wall is 2.8% lower and the median is higher with Engram. This small
matrix supports preservation of convention fidelity, not a general speedup or
an attribution of differences from earlier dates to the new capture features.
The earlier September 16 matrix was 8/10 versus 10/10; both are retained.

## Isolation and provenance

Every fixture carries the pinned project identity. The control disables hooks
and MCP, while the brain arm uses only the hook pipeline. The audit rejects
missing cells, zero-context brain arms, nonzero control injection and MCP or
explicit brain CLI leakage. All 20 cells satisfy those checks. Launches were
detached with `nohup` + `caffeinate -dims`.

The YAML sampling fields are descriptive metadata, not enforced by this
harness. The adapter does not forward repetition IDs as model RNG seeds.
These are repeated pairs, not a claim of deterministic seeded decoding.

Raw structured evidence and artifact hashes:

- [Per-trial records](2026-09-17-product-regression/hooks-runs.jsonl)
- [Comparison](2026-09-17-product-regression/hooks-comparison.json)
- [Isolation and artifact audit](2026-09-17-product-regression/hooks-audit.json)
- [Context telemetry](2026-09-17-product-regression/hooks-telemetry.json)
- [Exact configuration](2026-09-17-product-regression/hooks.yaml)
- [Input hashes](2026-09-17-product-regression/input-hashes.json)
- [Runtime identity](2026-09-17-product-regression/runtime.json)

Earlier preflight rejections and the superseded partial build are recorded in
[discarded-runs.json](2026-09-17-product/discarded-runs.json); they are not
represented as final-build measurements. The local family's independent
runtime startup problem does not change the hook matrix denominator.
