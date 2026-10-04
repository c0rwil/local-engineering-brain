# Lean task-status behavioral grader, suite v2

This suite fixes the frozen pilot's fixture-byte overconstraint and ambiguous
cursor wording. It does not alter `real-repo-v1`, its results, or adjudications.

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

The 33 checks cover lean summaries, integer counts including empty history,
boolean/string opt-in, history field preservation, cursor filtering and total
counts, explicit cursor edge cases, missing tasks and user isolation, terminal
results and errors, the public tool schema, legacy fixture compatibility, and
the consumer that needs step content. New v2 fixtures are checked if added, but
are not mandatory. Their filenames do not imply lean versus explicit-history
requests. Status examples retain the five summary fields; optional history rows
retain all six step wire fields, and the integer total cannot be smaller than the
returned history length. A filtered example can have a larger total.

Tool-schema inspection accepts simple types, type arrays, `anyOf`/`oneOf`, and
local `$ref` definitions, including nullable or coercible input alternatives.
It verifies that boolean flags and integer-compatible cursors are advertised;
the runtime checks determine defaults, conversions, and invalid-parameter
behavior. This bounded inspection is not a general JSON Schema validator and
does not fetch external references.

Compatibility preserves every existing JSON object key, scalar type/value, and
array length/order. Extra optional object fields are allowed, including
`step_count` in legacy status examples; JSON formatting and object-key order
are irrelevant. `v1_semantic_manifest.json` records required structure and
hashes of parsed scalar values from the pinned base. It contains no raw private
fixture scalar values. `v1_sha256.json` records baseline provenance only; the
grader does not compare candidate fixture bytes with those hashes. Preflight
independently verifies the semantic manifest against immutable base Git objects.
History rows and their six expected fields are grader-owned dictionaries; neither
Redis fixtures nor assertions derive expectations from candidate serialization.
A separate runtime serializer check preserves populated and null field values.

Calibration has 33 controls: the reference, ten valid alternatives, and 22
rejected cases including the base. Valid alternatives add status or device
fields, reformat/reorder object keys, use string `"true"` for consumer history
opt-in, declare equivalent schema unions or local references, show explicit or
filtered history in ordinary status filenames, or omit optional v2 fixtures.
Mutants load history unnecessarily, erase runtime payload fields, remove/change
existing fixture content, substitute a boolean for an integer, change array length/order,
disable false cursors, reject historical cursor types, round numeric cursors,
omit the consumer's history opt-in, omit/mistype schema inputs or required task
IDs, create a schema-reference cycle, undercount history, or drop required example
fields. Runtime serializer expectations remain
independent of candidate serialization. Calibration never checks out, modifies,
or runs the developer's working tree:

```sh
python3 -B preflight.py --source-repo /path/to/agents --output preflight.json
```

`preflight.json` publishes control verdicts, check counts, named failed checks,
input hashes and interpreter version. Exported private source, full grader
results and logs remain in a private temporary directory outside the repository;
the command prints that location. Synthetic response-byte measurements describe
this product API payload, not coding-agent token savings.

The false-value flag rule applies only to `include_steps`. A missing/null
`since_step` is absent. Other cursor values follow historical Python `int`
conversion: booleans become 0/1, finite fractions truncate toward zero, and
integer strings (including whitespace/signs) convert. Negative cursors are
allowed. A converted cursor always implies history, including false converted
to zero. String `"false"`, fractional strings, empty strings, arrays and objects
return invalid params. These cases are explicitly scored in v2; no strict
integer-only behavior is silently imposed on the historical reference.

For the coding agent's environment preflight, a safe baseline command is:

```sh
python3 -m py_compile src/agency/api/services/mcp_tool_handlers.py src/agency/api/services/mcp_session_service.py scripts/mcp_user_control_integration_test.py
```

This is only a syntax smoke check, not the acceptance grader. An agent can add
isolated mock tests without importing development-service fixtures. Keep this
grader, its manifest, and reference fixtures outside the coding agent's sandbox.
