# Lean task-status behavioral grader

Run with Python 3.11 or newer. Only the standard library is needed; do not run
the repository's parent `conftest.py`, load its `.env`, or install dependencies.

```sh
python3 -B grade.py --repo /path/to/disposable/candidate --output /tmp/result.json
```

Exit codes: `0` accepted, `1` behavior rejected, `2` infrastructure failure.
The JSON contains each check and its outcome. Import or syntax errors in candidate
modules reject the candidate. The grader does not edit the candidate repository.

The grader imports and executes the actual status handler and session service.
Only external service modules are stubbed. An instrumented Redis fake executes
the real hash decoding, count, history decoding, and response code. Lean polls
raise if any full history payload is loaded. The real user-control polling
consumer is also executed against the candidate handler. Environment variables
are cleared before imports; outbound sockets and subprocesses are denied.

The 23 checks cover lean summaries, integer counts including empty history,
boolean/string opt-in, history field preservation, cursor filtering and total
counts, non-coercible cursor values, missing tasks and user isolation, terminal results
and errors, the public tool schema, historical v1 bytes, and the consumer that
needs step content. New v2 fixtures are checked if added, but are not mandatory.
Byte expectations come from the immutable external `v1_sha256.json` manifest.
History rows and their six expected fields are grader-owned dictionaries; neither
Redis fixtures nor assertions derive expectations from candidate serialization.
A separate runtime serializer check preserves populated and null field values.

Calibration uses Git archives of the base and reference fix, plus a reference
mutation that returns lean JSON but still loads all Redis history, a serializer
that drops a wire field, and coordinated serializer/handler payload erasure.
The latter catches a moving oracle that would accept the same wrong values on
both sides of an assertion. Calibration never
checks out, modifies, or runs the developer's working tree:

```sh
python3 -B preflight.py --source-repo /path/to/agents --output preflight.json
```

`preflight.json` retains all check results, provenance, interpreter version, and
fixture/log locations. Synthetic response-byte measurements describe this
product API payload, not coding-agent token savings. The historical reference
coerces numeric cursor strings, fractions, and booleans; grading covers the
specified ordinary integer/string cursors and
unambiguously invalid text/list/object values.

For the coding agent's environment preflight, a safe baseline command is:

```sh
python3 -m py_compile src/agency/api/services/mcp_tool_handlers.py src/agency/api/services/mcp_session_service.py scripts/mcp_user_control_integration_test.py
```

This is only a syntax smoke check, not the acceptance grader. An agent can add
isolated mock tests without importing development-service fixtures. Keep this
grader, its manifest, and reference fixtures outside the coding agent's sandbox.
