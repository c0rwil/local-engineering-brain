# r06 private behavioral grader

Run only after the coding agent has finished, in a disposable grading copy:

```sh
ENGRAM_PLATFORM_NODE_MODULES=/tmp/engram-platform-preflight-20260924/node_modules \
  python3 benchmarks/real-repo-v1/grading/r06_onboarding_outage/grade.py \
  --repo /absolute/path/to/disposable-platform-copy \
  --output /absolute/path/to/r06-grade.json
```

`--node-modules PATH` overrides the environment variable. Existing dependencies
in the grading copy are used if present. The grader never installs packages or
calls a live Clerk/backend service. Exit 0 means pass, 1 means a behavioral or
candidate-source failure, and 2 means missing tooling or grader infrastructure.
The JSON report contains `status`, named `checks`, durations, dependency versions,
hashes, and paths to the raw Vitest JSON and log.

The grader injects its private test and a private Vitest configuration, then
removes them. It mocks authentication, redirect control flow, and telemetry.
Node fetch is intercepted with MSW; unhandled requests fail. The actual candidate
actions, endpoint constants, and return-path sanitizer execute. The product's
test setup and reference test files are not used. Never expose this directory,
the reference export, mutation exports, or grading output to the coding agent or
to the Engram index used by that agent.

The 26 checks cover backend 500/unreachable/non-JSON responses, opposite gate
and welcome behavior, completed/incomplete users, safe invite/CLI destinations,
five unsafe return URLs, unauthenticated short-circuiting, server-side bearer
headers, and no-store fetching. The deadline check runs three actual hung
fetches concurrently through the profile action, gate, and welcome function. It
requires the 503 result, correct redirects, and cancellation of all fetches by
five seconds plus 500 ms scheduling tolerance. The entire process group has a
45-second outer limit.

## Preflight

Source exports came from the read-only repository
`/Users/corwil/PycharmProjects/platform` using `git archive`:

- Base: `4a2668b9bb198ae7d9108f2cebefb84fa9244b24`
- Reference: `60cfcd4eda9d776f6cdac8af88f853c9276cb30e`
- Welcome-loop mutation: the reference, with an added redirect to `/me` on a
  failed profile inside `redirectIfOnboardingComplete`.
- No-deadline mutation: the reference, with the fetch `signal` removed.

`preflight.json` records the observed results and hashes. Full raw logs/reports
and source exports remain under `/tmp/engram-platform-preflight-20260924`.

## Prepared dependency strategy

The preflight cloned the existing 1.8 GiB platform `node_modules` tree into the
private temporary directory using macOS copy-on-write `cp -cR`. It did not run a
package manager in the original repository. Runtime: Node v26.5.1, Vitest 4.1.5,
MSW 2.13.5. Both historical commits have the same checked-in lockfile.

The prepared dependency tree's lockfile differs from that historical lockfile,
including a few transitive versions/overrides and an extra `fflate` dependency.
Therefore this preflight establishes behavior under a frozen shared local
runtime, not a lockfile-exact historical reinstall. Use this identical prepared
runtime for both experiment arms, or prepare an exact historical installation
and rerun the four preflight controls before starting all measured runs. Do not
silently mix dependency environments between trials. The grader records both
lockfile hashes.

For agent workspaces, clone the dependency tree into each workspace before the
timed run. A symlink is sufficient for post-run grading, where the private Vite
cache is directed into the disposable copy. The dependency preparation and
grading time are outside the coding-agent token/tool measurement window.
