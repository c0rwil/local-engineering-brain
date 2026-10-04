# r06 v2 behavioral grader

This version makes the deadline contract explicit in the v2 prompt. The complete
backend operation, including response-body consumption, must settle within five
seconds. Header/request timeouts accept 503 or 504; an interrupted body accepts
502, 503, or 504. Existing ordinary HTTP, unreachable-backend, and malformed-JSON
result statuses remain unchanged. No abort API, abort count, or cancellation
strategy is prescribed.

The prompt also states the existing `safeNextPath` return-path policy. Both the
historical base and reference reject a pathname containing `://` under an
invalid-route rule, and the base's sanitizer tests explicitly cover that case.
For example, `/https://example.com/path` is a same-origin local route; its
rejection is a preserved application policy, not an open-redirect claim.

Run only in a disposable grading export after the coding agent finishes:

```sh
python3 benchmarks/real-repo-v2/grading/r06_onboarding_outage/grade.py \
  --repo /absolute/path/to/disposable-platform-copy \
  --node-modules /tmp/engram-platform-preflight-20260924/node_modules \
  --output /absolute/private/path/r06-grade.json
```

The grader injects and then removes its private test/configuration. It installs
nothing and calls no live authentication/backend service. Exit 0 means pass,
1 means candidate failure, and 2 means missing tooling or grader infrastructure.
The 31 checks retain the existing outage, return-path, authentication, token, and
cache checks, and replace the old combined deadline assertion with six independent
checks: profile action, gate, and welcome, each under stalled headers and a
stalled body. Every deadline has 500 ms scheduling tolerance; the test-process
group has a 45-second outer limit. A profile status failure cannot hide the gate
or welcome result.

The body fixture delays headers for two seconds, then sends an incomplete JSON
body that does not finish before the total deadline. It mirrors transport aborts
by interrupting the stream when the request signal aborts, but never requires an
abort event. A complete-operation Promise.race implementation without fetch
cancellation is an explicit passing control. The header-only-deadline mutation
is the distinct failing body control.

## Frozen preflight controls

Reproduce fresh exports and private logs with:

```sh
python3 benchmarks/real-repo-v2/grading/r06_onboarding_outage/preflight.py \
  --source-repo /Users/corwil/PycharmProjects/platform \
  --node-modules /tmp/engram-platform-preflight-20260924/node_modules \
  --report benchmarks/real-repo-v2/grading/r06_onboarding_outage/preflight.json
```

The script uses read-only git archive exports from base
`4a2668b9bb198ae7d9108f2cebefb84fa9244b24` and reference
`60cfcd4eda9d776f6cdac8af88f853c9276cb30e`, then modifies only fresh private copies.
The original reference's fetch timeout also governs the body; its body JSON
catch reports 502 when aborted. The v2 interrupted-body status contract accepts
that observable behavior. All mutations are generated in preflight.py, and the
report records prompt, grader, private-test, preflight-script, dependency-lock,
and control-source hashes.

| Control | Required outcome | Observed checks passed | Purpose |
| --- | --- | --- | --- |
| Historical base | Fail | 14/31 | Untreated behavior remains detectable. |
| Reference, timeout 503 | Pass | 31/31 | Existing legitimate implementation. |
| Reference, timeout 504 | Pass | 31/31 | Equally permitted timeout status. |
| Complete-operation race, no abort | Pass | 31/31 | Cancellation mechanism is unconstrained. |
| No deadline | Fail | 25/31 | All three public operations must settle. |
| Deadline cleared after headers | Fail body checks | 28/31 | Body is inside the same total deadline. |
| Welcome redirects on failure | Fail | 25/31 | Welcome remains usable without a loop. |
| Timeout status 599 | Fail action check | 30/31 | Gate/welcome checks still run independently. |

Observed counts and individual deadline results are in preflight.json. Complete
candidate exports, Vitest reports, and logs remain in the private temporary
workspace named by that report. Do not expose this grading directory, controls,
reports, or private workspace to a coding agent or its Engram index. The v1
prompt, grader, and outcomes are unchanged.

Calibration requires every control to collect all 31 unique behavioral checks,
return its expected process/status outcome, and fail exactly its declared check
names. Source-load failures, partial suites, unrelated failures, and missing
intended failures cannot certify a mutant. The report includes each expected
failure set and any calibration errors.

## Runtime boundary

Both experiment arms must use the same prepared runtime: Node v26.5.1, Vitest
4.1.5, MSW 2.13.5. The shared prepared dependency lock differs from the historical
repository lock, so this establishes behavior under the recorded common local
runtime, not an exact historical dependency reinstall. The grader records both
lock hashes. Dependency preparation and post-run grading are outside measured
coding-agent work.
