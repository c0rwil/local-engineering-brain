# Line-ranged source pointers and related-use cues — 2026-10-04

The task source map now names a definition's lines as `path:start-end` when the
request's checkout file still has the indexed digest, and it can list a few other
users of the definitions a task names. On the four historical pilot tasks, every
emitted pointer carried a checked range, and required components anywhere in the
pack rose from 13/35 to 19/35. All six added components come from the
environment-isolation task. The added text costs 12.8% more estimated pack tokens.
Direct pointer selection did not change in any trial. These are retrieval
diagnostics. **Agent-level token or Read savings are not established until a
paired agent run.**

The 2026-09-25 follow-up found agents following file-level pointers with
whole-file Reads: 74% more Read text and 17.2% more provider tokens on both-pass
pairs, with no correctness gain. Ranges target that mechanism. The related-use
cue targets the environment-task miss, where an attempt fixed a shared helper but
not two other users of the same lock in the same test file. Whether agents read
less, or cover more sites, is untested here.

## Changes evaluated

- **Line ranges.** A definition pointer gets `path:start-end` (1-based,
  inclusive) only if the daemon reads the file under the request's repository
  root and its SHA-256 equals the digest recorded at indexing. A changed file,
  an unreadable or symlinked file, a linked repository, an imported graph, or a
  request without a root keeps the file-level pointer. Markdown sections run to
  the next heading of equal or higher level. Definitions include attributes,
  decorators and comments directly above them. Ranges are added after selection
  and only within the remaining budget, so they never change which sources fit.
- **Delivery suppression.** The delivery signature uses the range-free form of
  the source map. A range that moves, or falls back to a file-level pointer,
  does not resend a pack. New sources and changed source content still do.
- **Related uses.** Anchors are up to three direct definitions in the request's
  repository that a task identifier or explicit path names. A caller is listed
  when its call text equals the definition's name and no other definition in
  the anchor's file has that name. From another file, the name must also be
  unique in the repository, in the same language family, and imported by the
  calling file. Kotlin, Java, Scala, C# and Go files in the same
  directory need no import. An import longer than the indexer's 200-character
  label counts as possible evidence. A shared use is another definition in the
  anchor's file that makes the same specific call: a free function, a path, a
  static or type receiver, or a `self`/`this` member. Shared use is not a
  dependency. Callers rank by a task identifier in their name, then task terms
  in their path, then terms in their name and graph specificity. At most four
  per anchor and six in total follow every direct pointer, within the
  remaining budget.
- **Configuration.** `context.source_line_ranges` and `context.related_use_cues`
  default to on and switch each part off for ablation.

## Measurements

Before is the installed release `58c2d018…`. After is a release build of this
branch's commit `0a08a9a`, before any merge with other workstreams. Both used
identical frozen inputs and cases. The navigation suite alternated the two
binaries over three rounds. Selections and token counts below were identical in
both measurement sessions; see [Latency](#latency).

### Navigation probes, no checkout root

Eighteen authored probes over five real indexed repositories, three repeats each,
54 Brain compilations per round. Without a request root, no range can be checked.

| Measurement | Before | After |
| --- | ---: | ---: |
| Intended file in first five pointers | 45/48 trials | 45/48 trials |
| Expected definition in first five pointers | 39/48 trials | 39/48 trials |
| Negative probes with no source filler | 6/6 trials, 0 filler | 6/6 trials, 0 filler |
| Direct pointer selections | — | identical in 54/54 trials |
| Estimated pack tokens, 54 Brain compilations | 11,067 | 13,047 (+17.9%) |
| Related-use lines (estimated tokens) | 0 | 48 (1,998), in 6/18 cases |
| Brain requests within budget; Off controls with zero injection | 54/54; 54/54 | 54/54; 54/54 |

### Navigation probes, live checkout roots

The same probes, each project's primary local checkout as request root, one round
per binary. The daemon read those files only to compare digests.

| Measurement | Before | After |
| --- | ---: | ---: |
| Direct pointers with a line range | 0/252 | 231/252 |
| Estimated tokens of direct pointers | 6,312 | 6,723 (≈1.8 per range) |
| Related-use lines (estimated tokens) | 0 | 48 (2,076) |
| Estimated pack tokens | 12,291 | 14,766 (+20.1%) |
| Intended file / expected definition in first five | 45/48; 39/48 | 45/48; 39/48 |

The 231 ranged pointers are every emitted local definition pointer. An independent
recomputation of the checkout digests found 231 current files and none changed.
The other 21 direct pointers were whole-file or linked-repository pointers. This
run therefore did not exercise the stale-file fallback; the tests below do.

### Historical pilot tasks, reindexed clone as request root

Twelve cases from the four pilot tasks, including reordered requirements and
field, method and absent-field probes. Each binary reindexed a disposable clone
of the frozen fixture at its recorded HEAD. The clone was the request root.
Selections were identical across the three repeats, so per-repeat counts follow.

| Measurement | Before | After |
| --- | ---: | ---: |
| Required components anywhere in the pack | 13/35 | 19/35 |
| … of which from related-use lines | 0 | 6 |
| Required components in first five pointers | 7/35 | 7/35 |
| Required files anywhere in the pack | 15/25 | 17/25 |
| Required files in first five pointers | 7/25 | 7/25 |
| Pointers with a line range | 0/99 | 106/106 |
| Direct pointer selections | — | identical in 36/36 trials |
| Estimated pack tokens, 36 Brain compilations | 10,296 | 11,610 (+12.8%) |
| Related-use lines (estimated tokens), 36 compilations | 0 | 21 (777) |
| Absent-field negative probe | no source | no source |

| Case (one repeat) | Before components | After components | Before tokens | After tokens |
| --- | ---: | ---: | ---: | ---: |
| Environment isolation, full task | 1/4 | 4/4 | 428 | 563 |
| Environment isolation, reordered | 1/4 | 4/4 | 428 | 563 |
| Environment isolation, qualified method | 1/1 | 1/1 | 110 | 148 |
| Guide hot path, full and reordered | 2/5 each | 2/5 each | 374 each | 398 each |
| Lean status polling, full and reordered | 0/3 each | 0/3 each | 408 each | 430 each |
| Onboarding outage, full and reordered | 2/4 each | 2/4 each | 316 each | 333 each |

In the environment task, the task text names `Settings::from_env`. The after pack
lists three of its callers in the HTTP contract test file, each with a range: the
settings helper and both tests that take the shared lock. The two tests are the
sites the first pilot attempt missed, and none of the three appeared in any pilot
pack. In that attempt, a whole-file Read exceeded the size limit and narrower
reads did not reach the missed sites.

The cue added nothing to the other tasks. The guide task names types and fields;
the index records no calls to them. The status-polling repository is Python, and
the native indexer records no Python call sites. The onboarding task names no
identifier, so it has no anchor.

### Latency

The machine was shared with other compile jobs, so latency is indicative only.
Two sessions ran the same alternating plan. Session 1 used a build that differs
from the commit by one two-line comment edit. Session 2 used the commit's build.

| Median API latency | Session 1 (load ≈7–14) | Session 2 (load ≈17–20) |
| --- | ---: | ---: |
| Navigation, no root, per round: before | 5.65, 4.89, 5.50 ms | 11.93, 11.49, 11.51 ms |
| Navigation, no root, per round: after | 5.28, 5.64, 7.43 ms | 6.33, 21.45, 14.00 ms |
| Navigation, live roots: before → after | 14.5 → 18.5 ms | 24.4 → 29.7 ms |
| Historical tasks: before → after | 10.4 → 15.3 ms | 16.7 → 24.6 ms |

With a checkout root, the after daemon was 4–8 ms slower per request in both
sessions. That covers file reads, digests and use lookups. Without a root, the
difference was within round-to-round variation.

## Correctness checks

Hermetic tests cover range rendering and line arithmetic, Markdown section
extents, and the stale, symlinked, path-escape, imported-graph and wrong-project
fallbacks. Others cover misaligned CRLF Markdown offsets, related-use resolution
(ambiguous, unimported, cross-language and generic calls), and budget sweeps
showing that direct selection is unchanged and every budget holds. A daemon and
CLI test shows a range delivered, an edit suppressed without resending the pack,
and a changed definition delivered with its new range. The delivery-signature
test covers moved ranges, fallbacks and newly added uses.

## Limitations

- These pilot tasks informed this change, so they are not held-out evidence.
  The related-use ranking was adjusted while inspecting the environment task.
- No coding agent was run. Lower Read volume, provider tokens or better site
  coverage are hypotheses for a paired agent run that reports every token
  category. The cue adds 37–43 estimated tokens per line.
- Live-root results depend on the checkouts at run time. None of the emitted
  files had changed since indexing.
- Latency was measured locally on a machine shared with compile jobs
  (one-minute load average about 7–20). Treat it as indicative only.
- Two navigation cases now expect `Intervention::cancelled`. The current native
  extractor qualifies impl methods, so the old label `cancelled` is absent from
  today's index. Both binaries used the updated case file.
- Linked-repository pointers stay file-level. Python call sites and calls inside
  Rust macro arguments are not indexed, so they cannot become related uses.

## Reproduction and provenance

- Frozen navigation snapshot: SHA-256 `9501c83e33d3dafa…`, schema 16, frozen
  with `--freeze-from` from the live database; unchanged after every run.
- Before daemon SHA-256 `c595276569ebf8a6…` (release `58c2d0182e653f23…`);
  after daemon SHA-256 `3e2cbe4ef9f75dff…` (session 2, built from `0a08a9a`)
  and `1b2320edbe0225a9…` (session 1).
- Harness SHA-256: navigation `df3c2293372c57a7…`, tasks `ae727153eb2cf4e9…`,
  matching the committed scripts.
- Navigation cases SHA-256 `0abdb2d0ab649052…`; task cases `76c5cd20a1bc4fa2…`.
  Pilot fixture snapshots were hash-checked before and after each task run.
- Raw reports include private repository paths and identifiers. They stay in the
  local working evidence directory; only these aggregates are committed.

```sh
caffeinate -dims python3 -B scripts/eval_context_quality.py \
  --daemon /path/to/engramd --snapshot /private/snapshot.sqlite \
  --repeats 3 --output /tmp/navigation.json \
  [--repository-root PROJECT=/absolute/checkout ...]
caffeinate -dims python3 -B scripts/eval_task_context.py \
  --daemon /path/to/engramd --fixtures /private/pilot/fixtures \
  --reindex --live-checkout --repeats 3 --output /tmp/tasks.json
```
