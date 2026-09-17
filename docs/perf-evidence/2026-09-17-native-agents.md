# Native coding-agent integration — 2026-09-17

Implementation: `280ef5c`. Installed build:
`ce72844791f6e70d8a42558d67fc9f63f176421654f333c9f943ec4628a81048`.

Engram connects to ordinary Claude Code and Codex through hooks and MCP, using
the client's existing model and provider. The workspace UI previously foregrounded
`claude-mtplx`, an optional local-model launcher. Its onboarding copy now explains
the native path and distinguishes local coding sessions from generic MCP clients.

The installed Codex hooks file had a flat event layout, and Codex had no Engram
MCP registration. The installer now repairs the root `hooks` object, honors
`CODEX_HOME`, preserves foreign handlers even in mixed groups, rejects malformed
settings without replacing them, and uses an absolute executable path. Claude's
installer uses the same preservation logic. Codex SessionEnd uses the supported
three-second timeout. Native patches resolve each edited repository; Bash hooks
use the actual tool directory. Context admission respects the aggregate budget
without losing later repositories' episode capture or suppressing omitted briefs.

## Verification

- Format check and Clippy with all targets/features and warnings denied passed.
- All **270 tests** passed, including native Codex payload routing, explicit
  verification results, unknown acceptance, multi-repository budget exhaustion,
  omitted-brief retry, installer preservation and MCP initialization instructions.
- After build, install and daemon restart, the installed Codex client
  (`0.154.0-alpha.6.2`) parsed all **five** Engram hooks through `hooks/list` with
  **zero errors**. All five remain **untrusted** pending the operator's `/hooks`
  review. No trust state or approval policy was changed.
- Codex's shared MCP configuration now points to the installed `engram-mcp`.
  A real stdio handshake returned **23 tools**, agent instructions and the live
  product snapshot. This checks connectivity, not an agent's decision to use it.
- The installed browser UI visibly shows the native-agent instructions.
- Existing Claude hooks and its legacy MCP connection remain installed. The
  operator must run `engram hooks install --global` to refresh global Claude
  definitions, as required by the operator's local permission constraint.

Sanitized [verification](2026-09-17-native-agents/verification.json),
[Clippy output](2026-09-17-native-agents/clippy.txt) and
[test summary](2026-09-17-native-agents/tests.txt) accompany this report.
There is no measured improvement in real-session capture or onboarding speed yet.
Live Codex automatic capture is not claimed before hook trust and a new session.

## Injection regression status

The new-build hook family (seeds 1–2, 20 cells) is running in
`/private/tmp/engram-native-regression-20260917`. The new-build local family
(seeds 2–3, 20 cells, 2700-second walls) is queued after the existing local matrix;
its driver restarts and warms the dedicated model before scoring. Both launch
detached with `nohup`, `caffeinate -dims` and `disown`. Only the reviewed synthetic
fixture enters cloud arms; project identity is pinned and controls disable hooks
and MCP. Executables are pinned to this release. See the
[runtime manifest](2026-09-17-native-agents/regression-runtime.json).

These matrices are incomplete and have **no new published score**. Existing
headline results remain associated with their earlier source builds.

Before installing this release, the ongoing origin-refresh local matrix was
pinned to its original release. Only executable paths and `ENGRAM_BIN_DIR` in its
adapter changed, with the original adapter and input hashes retained. Three cells
had completed and the fourth was active at that point. The change and both
adapter hashes are recorded in the [pin manifest](2026-09-17-native-agents/prior-local-binary-pin.json).
The old matrix must not be labeled as testing this new build. Its timing also
overlaps development/build activity, so it cannot establish a clean speedup.
