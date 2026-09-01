# Preference distillation: learning the engineer — 2026-09-02

The last unproven spec axis (§16): can LEB DERIVE a team convention from
episodes, rather than storing one it was told? Injection efficacy of
preferences was already proven (2026-08-31 flagship, 24/24); this
benchmark isolates derivation.

## Mechanism (deterministic, no model in the loop)

`distill_preferences` mines successful, user-accepted episodes in which a
recorded verification command FAILED and later PASSED — the fix between
those runs is what the convention demanded. When ≥ N distinct episodes
share the same correcting command AND the same successful-attempt summary,
that summary becomes a candidate `preference`: evidence-linked, capped at
synthesized confidence (0.50), deduplicated by exact claim, and subject to
the same promotion gates as operator-written memories. Unit-tested for
agreement thresholds, idempotency, disagreement, and pass-only commands.

## Setup

Three correcting episodes were seeded into a fresh brain through the real
CLI pathway (task → fail evidence → pass evidence → accepted finish); the
G6 terminal-counters convention appeared ONLY as those attempts'
summaries — `leb learn` was never invoked. `leb synthesize` distilled one
candidate preference; it passed observed → supported → validated on its
episode evidence; `leb backup create` froze the brain as
`distilled-snapshot.sqlite`. (A raw `cp` snapshot failed backup
verification downstream — WAL not checkpointed — which is itself a
correct-behavior observation: use `leb backup create`.)

Benchmark: claude-opus-4-8 via the LEB proxy, pinned identity, tasks
t20 + t24 (both enforce G6 via mechanical style verifiers), seeds 1-2,
paired off vs brain, where the brain arm injects ONLY the distilled
memory.

## Result

| arm | fidelity passes | mean wall |
|---|---|---|
| off | 3/4 (t20 seed 2 failed) | 71 s |
| brain (distilled only) | **4/4** | **42 s (−41%)** |

Per-pair walls (off → brain): t20s1 67→43, t20s2 53(fail)→50,
t24s1 103→39, t24s2 60→36.

## Reading

The derived preference behaves exactly like a seeded one: perfect
fidelity where the control slips, and consistently faster walls because
the agent stops re-deriving the convention from the codebase. Four pairs
is a small sample; the strong claim rests on composition — derivation
correctness is unit-proven and the injected artifact is the same kind of
validated preference the 24/24 flagship measured at scale. The learning
loop is closed end to end: episodes → distillation → evidence gates →
injection → measured effect.
