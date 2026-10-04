# Re-verification plan for the pending review backlog, 4 October 2026

Scope: `engram review reverify` (`src/reverify.rs`). This records how much of
the pending lesson-review backlog operator-initiated re-verification could
unblock. All numbers are aggregates. The database holds proprietary claims,
commands and paths; none of them is committed.

## Method

- Input: a copy of the newest backup, `pre-roadmap-20261004.sqlite` (written at
  07:04 local time), in a scratch directory. The live database and daemon were
  not used.
- An isolated daemon from this branch opened the copy through its own socket
  and a scratch `HOME` whose configuration disabled jobs, the watcher, the
  browser, the judge and the vault. Opening the copy migrated it.
- One plan: `engram review reverify --all --dry-run --json`. No verification
  command ran. The plan read the registered checkouts with local-only Git and
  bounded file reads, as the judge's live source check does.
- "If the commands pass" is the judge's deterministic gate with one
  hypothetical passing run attached. Its digests are the touched files' content
  at the primary checkout's HEAD.

## Results

The backup holds 135 pending reviews from 65 source tasks.

The judge's gate now (the same gate the live judge applies):

| Gate | Reviews |
| --- | ---: |
| No readable, independently verified evidence | 90 |
| Touched file without an index anchor and without a hook digest | 30 |
| Source task revision differs from the live checkout | 3 |
| Touched path outside every registered checkout | 3 |
| Touched file differs from the indexed source | 3 |
| Evidence revision differs from the live checkout | 2 |
| Touched file missing from the checkout | 2 |
| None: the judge would ask its model | 2 |

Recorded commands:

- 131 reviews (61 tasks) recorded at least one deterministic test, lint,
  type-check or build command; 4 recorded none.
- 194 distinct task and command pairs; 24 can run.
- 27 reviews (14 tasks) have at least one runnable command. They need 19
  distinct runs across 5 projects.
- 104 reviews have only refused commands.

Refusals of the 170 refused task and command pairs:

| Reason | Pairs |
| --- | ---: |
| No recognized verification (CI references, `gh`, scripts, prose) | 95 |
| Not plain shell (annotations in parentheses, here-documents) | 26 |
| Network tool or non-loopback URL | 12 |
| Exit status hidden (pipe, `;`, `||`) | 9 |
| Redacted text | 9 |
| Unsupported step | 7 |
| Variable or command expansion | 5 |
| Path outside the worktree | 5 |
| File redirection | 2 |

If the runnable commands pass, 5 of the 27 runnable reviews (one task) would
reach the judge's model. The other 22 would stay deferred:

| Remaining gate | Reviews |
| --- | ---: |
| No readable evidence: touched files are not at HEAD (uncommitted new files or deleted files), so the run cannot certify them | 7 |
| Touched file without an index anchor: its live content differs from its content at HEAD | 5 |
| Source task revision differs from the live checkout | 4 |
| Touched file differs from the indexed source | 4 |
| Touched file without an index anchor and without a digest at HEAD | 2 |

14 of the 27 runnable reviews touched files that are not at HEAD, and 3 have
touched files with uncommitted changes in the primary checkout.

## Reading

Re-verification removes the evidence gap only where the recorded command is a
plain verification and the task's files are the files at the primary
checkout's HEAD. In this backlog, most recorded commands are agent prose or
CI references rather than commands, and many tasks worked on files that are
not at the primary checkout's HEAD (feature branches in other worktrees, or
uncommitted files). A native hook run during the task, which records digests
of the live files, remains the main path to eligible evidence.
