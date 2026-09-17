# Hook regression after the origin-refresh fix — 2026-09-17

Installed implementation: `3b65868`, source hash
`79e96819a4f43b74bacd20c8b9be6e17405d8494688402db77d13676899e6b01`.
The unchanged five-task hook family ran with paired off/brain repetitions 1–2,
the reviewed synthetic conveyor fixture, and frozen validated preferences.
No private repository entered a cloud arm.

| Measure | Control | Engram |
|---|---:|---:|
| Functional + convention success | 8/10 | 10/10 |
| Mean wall | 104.657 s | 88.754 s |
| Median wall | 88.900 s | 89.230 s |
| Timeouts / agent errors | 0 / 0 | 0 / 0 |
| Compiled-context bytes, total | 0 | 22,220 |
| Compiled-context bytes, per trial | 0 | 2,222 |
| MCP / explicit brain CLI tool calls | 0 / 0 | 0 / 0 |

Both control misses passed functional verification but omitted an in-file unit
test for the new schedule grammar (`t21`, both repetitions). Every Engram trial
passed both stages. All 20 trials remain in the denominator.

The mean wall is lower with Engram and the median is slightly higher. The local
model regression was running concurrently and reported memory-pressure pauses.
This small matrix supports preserved convention fidelity; it does not establish
a general speedup or attribute changes across dates to the origin-refresh fix.
The preceding product-build matrix remains available at
[7/10 versus 10/10](2026-09-17-hooks-product-regression.md).

## Isolation and measurement scope

Every fixture carries its pinned project identity. Each arm has a separate
database and Unix socket. Controls disable all hooks and MCP; Engram trials use
the hook pipeline. The audit verifies all 20 unique task/mode/repetition cells,
final telemetry, zero control injection, positive brain injection and no MCP or
explicit brain CLI calls. The launch used detached `nohup`, `disown` and
`caffeinate -dims`, with 900-second harness limits.

Compiled-context bytes sum stored stable-prefix and dynamic-context UTF-8 bytes.
They exclude file-shape, session-brief and relay payloads. Total hook payload
bytes were not measured. YAML sampling settings are descriptive metadata, not
enforced by the harness. Repetition IDs are not forwarded as model RNG seeds;
these runs do not claim deterministic seeded decoding.

The local family has a separate 20-cell matrix, repetitions 2–3 and 2,700-second
limits. It completed warmup on the installed build and ran on dedicated port
8013. Its results are separate from the completed hook score above.

## Evidence

- [Per-trial records](2026-09-17-origin-refresh-regression/hooks-runs.jsonl)
- [Comparison](2026-09-17-origin-refresh-regression/hooks-comparison.json)
- [Isolation and artifact audit](2026-09-17-origin-refresh-regression/hooks-audit.json)
- [Compiled-context telemetry](2026-09-17-origin-refresh-regression/hooks-telemetry.json)
- [Exact configuration](2026-09-17-origin-refresh-regression/hooks.yaml)
- [Input hashes](2026-09-17-origin-refresh-regression/input-hashes.json)
- [Runtime identity](2026-09-17-origin-refresh-regression/runtime.json)

The [repository refresh report](2026-09-17-repository-refresh.md) covers the
separate live refresh, guarded drift recovery and unchanged 1/6 lesson replay.
