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
- Linked-repository pointers stay file-level. The build measured above did not
  index Python call sites or calls inside Rust macro arguments; the
  [addendum](#addendum-python-call-sites) measures the extractor that does.

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

## Addendum: Python call sites

Measured later on 2026-10-04 on branch `roadmap/python-calls`. The native
extractor, now `tree-sitter-v3`, records Python call sites, Python import
bindings and calls inside Rust macro arguments. On the pilot tasks, the
Python status-polling task gained three related-use lines per pack, but none
names a required component. Required components anywhere in the pack stayed
at 19/35, and direct selections were identical in 36/36 trials. A capacity
review on a copy of the live index found two problems in three existing
queries that match calls to definitions by name. They became several times
slower, up to 85 s for one query, and most of their new Python matches were
coincidental. The queries were rewritten, and a name-only match now needs a
name that exactly one file defines. **Agent-level effects remain
unmeasured.**

### Changes evaluated

- **Extraction.** A Python call is recorded the way a Rust call is: one
  reference per calling file and callee text, with a `CALLS` edge from the
  enclosing definition. Each name a Python import binds gets a binding node:
  bound name to absolute dotted target, with relative imports resolved
  against the file's directory. The extractor keeps at most 512 bindings per
  file, and they carry no edges. Rust macro arguments are re-parsed as call
  arguments, up to 4,096 argument lists, 1 MiB and three nesting levels per
  file. A reference that already exists keeps its first span outside a macro.
- **One-time re-extraction.** The extractor version bump makes the next index
  re-parse every natively indexed file once. A hermetic test simulates the
  upgrade. It checks that node ids are unchanged, that no lesson anchor is
  invalidated and that the following index parses nothing.
- **Related uses.** A Python caller in another file resolves through an
  import of the definition's module or name. The import's dotted target must
  end with the defining file's module path: at least two segments of it, or
  one when both files share a directory. Within the definition's file,
  `self.m()` and `cls.m()` in Python, `self.m()` and `Self::m()` in Rust, and
  `this.m()` in TypeScript resolve when they come from another member of the
  same owner. Python `Owner.m()` also resolves within that file. A Python
  method call counts as a shared call only on `self`, `cls`, a capitalized
  name or an import-bound name, and a recursive call is never a shared use.
  Identifier and path matches now rank across anchors before anchor order.
- **Name-matching queries.** File shapes (the pretool brief), module symbols
  and the vault's module map match a call reference to a definition by label.
  The file-shape and module-symbol queries now start from the file's or
  module's own labels, and all three pin their indexes. A match counts only
  when exactly one file defines the name, the rule that related uses already
  apply across files.

### Capacity review

Each daemon re-indexed a disposable clone of the status-polling repository's
local checkout into its own copy of `pre-roadmap-20261004.sqlite`. The
installed release (`82b5df1d…`) and the branch build both re-extracted all
1,033 files; the release was forced to by clearing its file digests. Three
runs used three branch builds: one before the query rewrite, `c83adde` and
the final `c080e05`. Index counts and sizes were identical across the runs.

| Measurement | Installed release, `tree-sitter-v2` | Branch, `tree-sitter-v3` |
| --- | ---: | ---: |
| Index time, full re-extraction (three runs) | 2.7–3.0 s | 5.3–6.1 s |
| Active nodes | 16,234 ¹ | 44,046 |
| Active edges | 15,383 ¹ | 49,614 |
| `CALLS` edges | 509 | 34,786 |
| Call references (Python) | 479 (0) | 21,170 (20,691) |
| Import bindings | — | 7,171 |
| Database size after `VACUUM` | 967.8 MB | 1,016.5 MB (+48.7 MB, +5.0%) |
| Definitions with unchanged id and digest | — | 10,355/10,355 |
| Lesson anchors: total, on removed nodes, digest changed | 128, 0, 2 | 128, 0, 2 |

¹ Includes 50 definitions from four files deleted since the backup. Clearing
the digests kept the release from tombstoning them.

For the related-use queries, the capacity script compiled one task per
worst-case anchor: the five Python functions with the most calling files,
plus the two functions the status-polling fix needed. Each task ran five
times on the final build. Related uses stayed within their limits, at 0–6
lines per pack. Median compile time rose by 1–7 ms; the slowest single
compiles took 63 ms and 173 ms.

| Anchor | Median compile, v2 → v3 | Related-use lines, v3 |
| --- | ---: | ---: |
| Most-called function 1 | 6 → 12 ms | 0 |
| Most-called function 2 | 33 → 38 ms | 1 |
| Most-called function 3 | 19 → 23 ms | 6 |
| Most-called function 4 | 3 → 4 ms | 3 |
| Most-called function 5 | 6 → 7 ms | 4 |
| Status-polling session lookup | 37 → 44 ms | 6 |
| Status-polling handler | 7 → 9 ms | 4 |

The name-matching endpoints were timed through each daemon: file shapes for
880 Python files, module symbols for the eight largest groups, and the module
map once. The middle column replays the unchanged queries on the v3 index
with Python's SQLite 3.53.4; the daemon bundles 3.53.2.

| Endpoint | Installed release, v2 index | Unchanged queries, v3 index | Final build, v3 index |
| --- | ---: | ---: | ---: |
| File shape p50 / p90 / max | 12.9 / 33.3 / 364.7 ms | 28 / 133 / 2,100 ms, query only | 8.4 / 8.9 / 39.4 ms |
| Module symbols median / max | 10.2 / 12.3 s | 4.1–84.7 s per direction query | 137 / 186 ms |
| Module map | 11.5 s | 78–93 s, coupling query | 0.78 s |

Before the name rule, the new query forms returned the old rows. Module-symbol
directions were compared without limits on three groups of the v3 index and
on 26 groups across four other repositories on the v2 index. File shapes
matched on all 880 files of the v3 index. The pinned coupling query returned
the original rows on the v3 index, and the regrouped form returned the pinned
rows and weights on both indexes.

The name rule's effect, from replays of the same queries:

| Measurement | Before the rule | After the rule |
| --- | ---: | ---: |
| File-shape caller rows, Python repository, v3, before the 25-row cap | 41,026 | 2,308 |
| Files with any file-shape caller, same | 543/1,033 | 253/1,033 |
| Name-matched coupling weight, same | 41,532 | 2,492 |
| File-shape caller rows, four other repositories, v2 | 562–6,817 | 386–4,368 (−31% to −56%) |
| Name-matched coupling weight, same four | 606–6,976 | 418–4,956 (−29% to −52%) |

Names defined in several files include a `main` per script and private
helpers duplicated across modules. Through the endpoint, the final build's
file shapes listed 1,395 callers across the 880 files, against 7,588 for
`c83adde` and 15 for the installed release.

### Pilot tasks

The task harness and fixtures were the same as above. Before is the
installed release `82b5df1d…`; after is the build of `c83adde`. The final
build `c080e05` produced identical selections and token counts in all 72
task trials and all 108 navigation trials.

| Measurement | Before | After |
| --- | ---: | ---: |
| Required components anywhere in the pack | 19/35 | 19/35 |
| … of which from related-use lines | 6 | 6 |
| Required components in first five pointers | 7/35 | 7/35 |
| Required files anywhere / in first five | 17/25; 7/25 | 17/25; 7/25 |
| Direct pointer selections | — | identical in 36/36 trials |
| Pointers with a line range | 106/106 | 113/113 |
| Related-use lines (estimated tokens), 36 compilations | 21 (777) | 42 (1,716) |
| Estimated pack tokens, 36 compilations | 11,610 | 12,543 (+8.0%) |
| Median task latency | 12.4 ms | 13.9 ms |

| Case (one repeat) | Before | After |
| --- | --- | --- |
| Lean status polling, full and reordered | 0/3 components, no related use, 430 tokens | 0/3 components, 3 related uses, 559 tokens |
| Environment isolation, qualified method | 1/1, 1 related use, 148 tokens | 1/1, 2 related uses, 201 tokens |
| All other cases | unchanged | unchanged |

In the status-polling task, the anchors are two step-count helpers that match
identifiers in the task text. The after pack lists three callers, each
another method of the anchor's class in the same file. None is a required
component. The status handler, session lookup and polling helper that the fix
needed are still missing, and only one of their three files is in the pack;
that gap is in direct selection. In the environment probe, the added line is
a test that makes the anchor's specific helper call inside `assert_eq!`.

On the frozen navigation snapshot, which is not re-extracted, only the
resolution and ranking changes apply. Direct selections were identical in
54/54 trials. The same 48 related-use lines and 13,047 tokens appeared, in a
different order in 2 of the 18 cases.

### Limitations

- The pilot tasks shaped the original cue, and r02 prompted this change, so
  they are not held-out evidence.
- Python resolution does not follow re-exports through a package
  `__init__`, attribute chains such as `self.repo.get()`, instance variables,
  dynamic imports or star imports. Module-level lowercase globals do not
  count as shared targets.
- The name rule also drops real matches when several files define a name.
  Related uses can still resolve such calls through imports.
- Capacity figures come from one repository on a machine shared with other
  jobs. Treat latency as indicative.
- No coding agent was run.

### Reproduction and provenance

- Before daemon SHA-256 `cfd24bf78f4d0cf9…` (release `82b5df1d…`). After
  daemon `9e98e467cc7dfa53…`, built from `c83adde`. Final daemon
  `5182e0c891d61dec…`, built from `c080e05`. A rebuild of either commit
  reproduced its hash.
- Harness and case hashes match the first measurement. The navigation
  snapshot SHA-256 `9501c83e33d3dafa…` was unchanged after every run.
- The capacity review used read-only copies of the backup, a disposable clone
  and local scripts that print aggregates. Raw reports with private paths and
  labels stay in the local working evidence directory.

## Addendum: ranged file briefs

The pre-tool file brief now gives each listed definition its lines when the
checkout's copy of the file still has the indexed digest, the same check as
task source map ranges. A file of at least `context.large_file_bytes`
(64 KiB by default) gets one line with its size. Measured later on 2026-10-04
on branch `roadmap/ranged-briefs`, against the installed release `71df87f2…`,
on a synthetic fixture.

The fixture has three Rust files of 960 KiB (1,700 functions), 101 KiB (180
functions) and 3.4 KiB (6 functions), plus one caller file. Each binary
indexed it into its own home, database and socket. For Claude Code, Codex and
Cursor, a prompt hook was followed by a Read hook on each file, repeated once
in the same conversation. The large file was then edited by one prepended line
and read again in new conversations.

| File | Installed release | Branch, unchanged file | Branch, file edited after indexing |
| --- | ---: | ---: | ---: |
| 960 KiB, 20 of 1,700 definitions listed | 809 B | 1,031 B: spans and size line | 828 B: size line, no spans |
| 101 KiB | 765 B | 987 B: spans and size line | — |
| 3.4 KiB | 352 B | 400 B: spans, no size line | — |

- The brief was the same size for all three clients, and each fit its limit.
- The repeated call in the same conversation added no brief in either build.
- The spans locate only the first 20 definitions in file order. A definition
  that the task names keeps its range in the task source map.

The installed index was copied this morning and frozen. In it, 15 of the 131
files of the Rust backend repository are 64 KiB or larger. Four are over
256 KiB, and the largest is 806 KiB. In the Python repository, 4 of 1,037 files
are 64 KiB or larger, none is over 256 KiB, and the largest is 96 KiB.

No coding agent was run, so the effect on whole-file reads is not measured.
The installed daemon SHA-256 is `cac553904322a1c8…`, and the branch daemon is
`f456a6846366e14b…`.

## Addendum: task-ranked file briefs

A file brief now lists the definitions that the task source map selected or the
task names first. Definitions whose names contain a task identifier, share two
of its words or contain words of the prompt come next, and the rest follow in
file order. Measured later on 2026-10-04 on branch `roadmap/brief-relevance`.
The baseline is a build of the integration head `eabcd54`, whose briefs keep
file order.

**Synthetic large file.** This is the 960 KiB fixture above, with a task that
names `engine_step_1500`, the 1,501st of its 1,700 functions. Under Claude
Code, Codex and Cursor alike, the baseline brief listed the first 20 functions
and not the named one. The ranked brief listed it first with its span,
`engine_step_1500 (Function) 19504-19515`, under the heading
`Defines (line spans, task matches first)`. That brief is 1,055 bytes, 24 more
than the baseline. After an edit to the file, the named function stayed first
without a span. A repeated read in the same conversation added no brief.

**Pilot tasks.** `scripts/eval_task_context.py --file-briefs` asks the daemon
for the brief of each required file. It uses the case's task text and the run
just compiled, and records where each required definition falls in the first
20 entries. Twelve cases ran on reindexed disposable clones of the frozen pilot
fixtures, three repeats each, so the figures below count 105 required
definitions in 36 compilations.

| Required definitions in their file's brief | Baseline | Ranked |
| --- | ---: | ---: |
| Listed in the first 20, all with spans | 69 | 99 |
| In the first five | 45 | 87 |
| First | 6 | 51 |
| Named in the task text: listed | 15 of 15 | 15 of 15, all first |

Source map selections and tokens were identical in both builds. One r02
definition is still not listed: neither the source map nor the task text
matches it, and it lies past the first 20 definitions of its file. No coding
agent was run. The baseline daemon SHA-256 is `c944043384408843…`, the
candidate is `20cc0bfd761f8a61…`, and the harness SHA-256 is
`9fa5416ba65f66ba…`.

## Addendum: file briefs per task

A conversation now gets one brief of a file per task. After a new prompt, the
file is briefed again only if its brief would list other sources than the one
delivered. Order and line spans alone do not count, as with the delivery rule
for task source maps. A context run compiled for an earlier task no longer
ranks a later task's brief.

The measurement used one conversation, the 960 KiB fixture above, and six
reads of it under five tasks. Step 1 named `engine_step_1500`, and step 2
repeated it. Step 3 named `engine_step_0003`, a function near the top of the
file. Step 4 named `engine_step_0007`, so it listed the same 20 functions in
another order. Step 5 named two functions near the end, and step 6 named none.

| Client | Baseline (`79e6635`): briefs, bytes | Per task: briefs, bytes |
| --- | ---: | ---: |
| Claude Code | 1, 1,055 B | 4, 4,220 B |
| Codex | 1, 1,055 B | 4, 4,218 B |
| Cursor | 1, 1,055 B | 4, 4,220 B |

The repeated task (step 2) and the reordered list (step 4) added no brief. The
re-briefs opened with the named function: `engine_step_0003` at step 3 and
`engine_step_1200` at step 5. At step 6 the prompt named no function, but the
task source map still selected one, and it differed between clients. That
pick led the re-brief, so a vague task re-briefs whenever its source map picks
a definition outside the delivered list. No coding agent was run.
