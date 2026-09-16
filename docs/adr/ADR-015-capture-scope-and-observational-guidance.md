# ADR-015: Capture scope, observational lessons, and explicit acceptance

- Status: accepted
- Date: 2026-09-16

## Decision

Keep the existing evidence policy from ADR-004. The verified-supporting-evidence
view already accepts successful task-linked `runtime_observation`, `trace`,
`metric`, and `user_acceptance` observations. The capture failure was misleading
MCP guidance that advertised only deterministic kinds, combined with terse
scope/sequence errors. Expose the existing path and prescribe corrective steps.
Do not add an alternative trust tier or weaken promotion and injection gates.
MCP learns candidates at confidence 0.50. Human validation remains available in
the web UI; automated promotion still traverses the evidence gate.

Delay hook episodes until tool use. Keep prompt injection at UserPromptSubmit
and attach its context-served provenance atomically when the episode opens.
Track one episode per session/project, derive project scope from tool paths or
explicit shell working directories, and preserve the original session ID for
relay addressing. Cross-project context uses the target checkout root and HEAD.
SessionEnd records success only from a passing last verification, failure from a
failing one, and abandoned when no verification is available. Acceptance stays
unknown absent explicit evidence; silence or a local commit is not user approval.

Use Git's common directory to find a linked worktree's primary identity store.
Keep normal primary checkout aliases ahead of linked worktrees in stable
registration order. Bare Git directories may store shared identity but are never
checkout aliases. Derived origin views retain their deliberately separate IDs.

Redact before capture, including before pending-prompt truncation. Write hook
sidecars atomically with private permissions. A prompt purge removes the row's
text and pending copy, emits a content-free audit event, and truncates old WAL
frames. Immutable events contain prompt digests, not prompt text. Backups and
independent copies require separate handling.

## Validation

Hermetic regressions cover punctuation/header credentials, canonical/WAL/pending
purge, ordinary and bare-parent worktrees, MCP observational learning with
unknown acceptance, and actual multi-project hook inputs/output. Existing
compact-byte stability and zero-injection control tests remain unchanged.
Capture metrics and lesson replay are separate from injection benchmarks;
missing relevance labels or incomplete benchmark runs are reported as such.
