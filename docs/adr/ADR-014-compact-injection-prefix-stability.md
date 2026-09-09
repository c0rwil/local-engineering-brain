# ADR-014: Derived compact injection and byte-stable context

- Status: accepted
- Date: 2026-08-29

## Context

Canonical memories are deliberately human-readable and provenance-bearing, but
injecting each full memory ID and rationale repeats audit-oriented text on every
model turn. The first paired local-model benchmark measured a material wall-time
cost for injection. Runtime prefix caches also lose value when an otherwise
unchanged LEB block contains per-request data or unstable ordering.

## Decision

Migration 8 adds `memory_compact_injections`, a rebuildable projection keyed by
canonical memory ID. Renderer version 1 is deterministic Rust code with no model
call:

```text
- RULE: <claim>
- PROC: <claim>
- SKILL: <claim>
- PREF: <claim>
```

Unknown kinds use `MEM`. The renderer collapses whitespace, replaces control
characters, preserves case, and bounds the claim to 512 UTF-8 bytes without
splitting a code point. It omits rationale boilerplate and IDs. Each row stores
the exact compact bytes, their conservative `ceil(bytes / 4)` token estimate,
renderer version, and a digest of the complete canonical `kind` and `claim`.

Backfill runs in the migration transaction with the same renderer used by later
writes. Memory creation and every canonical `kind`/`claim` mutation regenerate
the row in the enclosing transaction. A full rebuild deletes and re-renders the
sidecar in one immediate transaction. Compact compilation rejects missing or
stale projections instead of silently mixing renderer versions.

`context.compact_injection` selects the projection and defaults to `false`.
Verbose mode therefore remains the compatibility/control arm, and one flag is
the complete verbose-versus-compact ablation. Brain-off mode does not inspect
the flag and still injects zero bytes.

Canonical `memories.claim`, `memories.rationale`, evidence/source/node links,
and context `source_id` plus feature-level `why` remain unchanged. Explain and
audit flows resolve the full canonical memory; the compact text never becomes
truth.

Context rendering contains no request ID, context-run ID, timestamps, or usage
counters. Architecture facts have explicit SQL ordering; retrieved memories
have deterministic score ordering with memory ID as the final tie-break. Thus,
for the same eligible memory set, configuration, and session/repository state,
the concatenated `stable_prefix + dynamic_context` bytes and digest are equal
across turns.

## Consequences

- Compact form has a hard size and token bound and is fully rebuildable.
- Canonical prose can remain explanatory without paying its full injection cost.
- Runtime consumers can append the LEB block without introducing volatile text.
- Changing the renderer requires a new renderer version and a transactional
  rebuild/migration; in-place semantic drift is not allowed.
