# Engram user guide

Everything you need to run the Engram day to day, from a
fresh machine to a fully automatic loop. For the team/company rollout path,
see [COMPANY_ROLLOUT.md](COMPANY_ROLLOUT.md); for central-server mechanics,
see [CENTRAL_SERVER.md](CENTRAL_SERVER.md).

## 1. What Engram is, in one paragraph

Engram captures what happens while you (and your agents) work — every test,
build, and lint result, every attempted approach, every accepted outcome —
and turns it into typed, evidence-gated memory. An agent's claim is never
trusted on its own: it caps at 0.50 confidence until deterministic evidence
(a passing test tied to a successful, accepted episode) promotes it.
Validated knowledge is compiled into every future session's context under a
hard token budget, so session 50 starts where session 49 finished instead of
rediscovering everything. Everything runs locally; nothing leaves the
machine in the default path.

## 2. Install and first run

```bash
./scripts/install.sh            # builds engram, engramd, engram-proxy, engram-mcp
export PATH="$HOME/.local/bin:$PATH"
engram doctor                      # every check should PASS
```

The daemon (`engramd`) auto-starts on the first CLI call and owns the canonical
database at `~/.engram/brain.sqlite`. You never manage it directly;
`engram daemon status` / `engram daemon stop` / `engram daemon restart` exist
when you need them. `engram daemon status` shows the daemon's process id and,
when an Engram client auto-started it, the process chain that started it.
Configuration lives at `~/.config/engram/config.toml`
(see `config/engram.example.toml` for every section).

## 3. Bring your repositories in

```bash
engram onboard ~/code              # sweeps a folder: register + index + auto-link + vault
# or one repo at a time:
engram index ~/code/my-repo
```

Indexing builds the world model — a provenance-bearing code graph
(tree-sitter for Rust, Python, JS/TS/TSX, Go, Java, Kotlin, C, C++, C#,
Ruby, PHP, Bash, and Scala — 16 languages — plus markdown headings as
searchable Section nodes and doc links as graph edges, so documentation is
part of the graph) — and
detects the technology stack from manifests and infra files. Definitive cross-repo dependencies (path deps,
`file:` packages, submodules) auto-link on every index; `engram link scan`
proposes fuzzier matches, `engram link add` records what you know by hand.

Teach Engram the facts a codebase can't tell it about itself:

```bash
engram fact set hosting.base_url api.example.com --path ~/code/my-api
engram fact set hosting.provider gcp --path ~/code/my-api
engram fact set role "public REST API for the mobile app" --path ~/code/my-api
```

Facts lead every compiled context, drive hostname-based link inference
(a hostname match proposes a `calls-api-of`
link for review; it is not enough to auto-apply a dependency), and render in the encyclopedia.

## 4. The living encyclopedia

While the daemon runs, http://127.0.0.1:11436 serves the workspace browser. Use
`127.0.0.1` or `localhost`: other host names are refused with HTTP 421, which
protects the loopback listener from DNS rebinding.

- **Service map** — every tracked repo, its links, edge labels.
- **Service pages** — click any card: synthesized architecture guide
  section, facts, technology chips, module maps with the functions each
  module actually *uses*, and the memories attached to that repo (with ROI
  and Validate/Forget curation controls).
- **Views** — the encyclopedia renders **origin/main by default** (the
  shared truth), with a header toggle for your local checkout. Agent
  context always uses the local checkout — your working tree is the source
  of truth for agentic work.
- **Architecture guide** — `engram architect --claude` synthesizes a
  CLAUDE.md-style guide; the daemon tracks a content digest and
  re-synthesizes automatically when the workspace actually drifts
  (`[architect] auto = "claude"` in config).

An optional Obsidian vault mirror exports to `~/.engram/vault` when enabled.
The workspace UI and canonical documentation do not depend on it.

## 5. Connect your agents (once, machine-wide)

Use your existing Claude Code, Codex or Cursor client and its chosen
model/provider. Engram attaches through native hooks and MCP. No replacement
launcher or local model is required. `claude-mtplx` is only the optional
MTPLX/local-model path.

```bash
engram hooks install --global   # Claude Code: ~/.claude/settings.json
engram agent-md --global        # the agent tool loop: ~/.claude/CLAUDE.md
claude mcp add --scope user engram -- "$HOME/.local/bin/engram-mcp"
codex mcp add engram -- "$HOME/.local/bin/engram-mcp"
engram hooks codex --install    # Codex native hooks; review them with /hooks
engram hooks cursor --install   # Cursor agent hooks: ~/.cursor/hooks.json
engram hooks cursor --mcp       # optional: engram-mcp in ~/.cursor/mcp.json
```

These commands assume the default Engram install location. Keep an existing
working `leb-mcp` connection rather than adding a duplicate server after a rename.
Codex's hook installer writes `$CODEX_HOME/hooks.json` (normally
`~/.codex/hooks.json`), repairs the old flat layout, preserves other hooks, and
uses an absolute executable path for GUI sessions. Open `/hooks` to review and
trust the installed definitions; newly installed or changed hooks do not run
until trusted. Start a new local session after connecting MCP.

`engram doctor` reports Engram hooks that Codex will not run (`codex_hooks`):
never trusted, changed since they were trusted, or disabled. It compares the
trust hashes Codex keeps in `$CODEX_HOME/config.toml` with the installed
definitions, using the rule Codex CLI 0.155.0-alpha.2.6 applies; `/hooks` in
Codex stays the authority. A reinstall that changes a definition (for example
a different executable path) needs trust again.

Cursor's installer merges Engram's entries into `hooks.json` in the Cursor
configuration directory: `--cursor-home DIR`, else `$ENGRAM_CURSOR_HOME`, else
`~/.cursor`. It preserves other hooks, refuses a file it cannot parse, and
leaves `mcp.json` alone unless you pass `--mcp`. `--mcp` registers the absolute
`engram-mcp` path as the `engram` server; it keeps an existing server that
already launches `engram-mcp` or `leb-mcp` and refuses to replace a foreign
`engram` entry. `--print` shows the entries without writing anything.
`--uninstall` removes only Engram's hook entries; add `--mcp` to also remove
Engram's server entry. Cursor reloads `hooks.json` on save; the Hooks tab under
Customize and the Hooks output channel show what ran. The entries point at
`~/.local/bin/engram` when it exists. An `engram` without the Cursor adapter
rejects `--cursor` with exit code 2, which Cursor treats as a block, so
`--install` first runs the target binary's adapter and refuses to write when
it fails. Install the Engram build before `--install`, and run `--uninstall`
before downgrading to a build without Cursor support.

| Client | Native connection |
|---|---|
| Claude Code CLI, IDE and local Code sessions in Claude Desktop | Shared Claude Code hooks and MCP settings |
| Codex CLI, desktop app and IDE on the same host | Shared Codex hooks and MCP configuration, subject to hook trust |
| Cursor desktop agent on the same host | Cursor user hooks (`engram hooks cursor --install`); MCP is a separate opt-in (`--mcp`) |
| Other GUI clients with local stdio MCP support | Register the absolute `engram-mcp` executable; tools are available, but MCP alone does not guarantee automatic capture |
| Remote/cloud agent sessions | Configure Engram on the execution host; local machine settings and localhost do not automatically follow |

Claude Desktop's ordinary Chat tab is separate from its Code sessions. Configure
its supported MCP integration separately; do not assume it runs coding hooks.
See the official [Claude Code desktop configuration](https://code.claude.com/docs/en/desktop#coming-from-the-cli),
[Codex hooks](https://learn.chatgpt.com/docs/hooks),
[shared Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp?surface=cli),
[Cursor hooks](https://cursor.com/docs/hooks),
[Cursor third-party hooks](https://cursor.com/docs/reference/third-party-hooks) and
[Cursor MCP](https://cursor.com/docs/mcp).

What the hooks do, invisibly, in every session — five injection moments:

| Hook | When | Effect |
|---|---|---|
| session-start | session opens | standing brief: open episodes from earlier sessions (resume or close), last completed outcome. In an untracked git repo: a one-line enrollment hint, nothing else |
| prompt | first prompt of a session | compiles task context and retains a redacted prompt; the first relevant tool use opens the episode |
| pretool | before file tools, Codex patches or Codex shell calls | repo-scoped task context; file graph shapes when explicit paths are available. Multi-file patches route to their edited repositories |
| bash | after each Bash tool call | recognizable verifications (test/build/lint/typecheck) recorded as deterministic evidence with the exit status the client reported — failures included. Runs whose status a pipe or `||` hides are not recorded (see [Verification evidence from hooks](#verification-evidence-from-hooks)) |
| session-end | session closes | episode auto-closed conservatively (success only if the last verification passed) |

The global hooks are **safe by design**: they engage only in repositories
already registered in the brain and silently no-op everywhere else — a
repo joins the brain only when you `engram index` it. Every hook fails open;
a broken brain can never break a coding session.

**Task source map.** Task context ends with up to 12 indexed source pointers,
one per file, ranked by task identifiers, paths and terms. They are reference
data, not verified lessons. A definition pointer names its lines as
`path:start-end` (1-based, inclusive): the daemon reads the file in the
request's checkout and shows the range only when the file still has the
digest recorded at indexing. A file changed since then, a linked repository,
or an unreadable file gets a file-level pointer instead; a refresh restores
the range. Markdown sections span to the next heading of the same or a higher
level, and definition ranges include attributes, decorators and comments
directly above them.

After every direct pointer, up to six `Related use:` lines may follow, only
within the remaining source budget. They come from the definitions the task
names (an identifier or an explicit path). A caller is listed only when its
call text names that definition exactly. From another file, the name must also
be unique in the repository and imported by the calling file (Kotlin, Java,
Scala, C# and Go files in the same directory need no import). Method calls on
values, ambiguous or unimported names and calls across languages are never
treated as dependencies. A shared use is another definition in the same file
that makes the same specific call, for example taking the same lock; it is not
a dependency between the two. The native indexer does not record Python call
sites or calls written inside macro arguments, so those produce no related
uses. Ranges and related uses do not change which direct pointers are chosen.
A pack is not delivered again just because a range moved or fell back to the
file; new or changed sources are. `context.source_line_ranges` and
`context.related_use_cues` turn each part off. Their effect on agent cost or
correctness has not been measured in a paired agent run; see
[perf evidence](perf-evidence/2026-10-04-line-ranges/README.md).

**Codex subagents.** Codex runs subagents in their own threads, but their
hooks carry the parent conversation's session id. Engram counts a spawned
worker's tool calls and verifications toward the parent's episode; the
worker's prompt, written by the parent agent, does not replace the task
prompt, drain the parent's relay inbox or receive prompt-time context.
Internal sub-sessions, such as the guardian that reviews approval requests,
are recognized from the `session_meta` record of their transcript and stay
silent: no context, prompt capture, episode, usage or evidence, and they never
close the parent's episode. Codex 0.155 turns hooks off inside guardian
sessions unless managed requirements pin `features.hooks = true`.

**Cursor sessions.** The Cursor entries handle `sessionStart`,
`beforeSubmitPrompt`, `postToolUse`, `postToolUseFailure`, `stop` and
`sessionEnd`, and answer with documented output fields only. Differences from
Claude Code and Codex:

- Cursor's documented prompt hook output has no context field, so Engram
  records the redacted prompt and delivers task context and relay messages
  with the first file (`Read`, `Write`, `Delete`) or shell result of a turn. A
  turn without such a tool receives no Engram context. Each response carries
  at most 8,000 bytes of context and 1,000 bytes of relay messages; the shipped
  Cursor 3.17 client discards `additional_context` above 10,000 characters.
- Permission hooks (`preToolUse`, `beforeReadFile`, `beforeShellExecution`)
  are not used: Cursor blocks the action when such a hook returns invalid
  output.
- A shell verification becomes evidence only when its tool result carries an
  exit code. The shipped Cursor 3.17 shell reports a failing command through
  `postToolUseFailure` without one: it counts as a tool call, not evidence,
  and it clears the session's last recorded result, so the episode cannot
  auto-close as a success on an earlier pass.
- Cursor 3.17 transcripts carry no token counters that Engram's transcript
  reader recognizes, so Cursor sessions report tool calls but no model token
  usage. Usage rows are labeled `cursor` and evidence `cursor-hook`.
- Cursor also runs Claude Code hooks from `~/.claude/settings.json` by default
  (Settings, Agents, Third-Party Imports). Engram's Claude Code entries
  recognize Cursor's input and stay silent there, so Cursor sessions are
  captured only through the Cursor entries, once.
- Session-level events use the first workspace root; tool events route by
  the tool's own paths. Cursor cloud agents do not read user-level hooks.

MCP exposes the episode lifecycle to agents; clients without native hooks must
call those tools to capture it. Optional local-model proxy injection runs through
`engram-proxy` on 127.0.0.1:11435 (`x-engram-mode: off` is a true control).

### Verification evidence from hooks

The `bash` hook records a test, build, lint or typecheck run only when the
coding client reported its exit status:

- Claude Code reports a non-zero exit through `PostToolUseFailure` ("Exit code
  N"). A completed foreground `PostToolUse` Bash call therefore has status 0.
  Interrupted, backgrounded or timed-out calls, and calls whose non-zero status
  Claude Code reinterpreted (for example `grep` finding nothing), are not recorded.
- Codex supplies an explicit exit code in its tool result.

The status must also belong to every verification in the command. Engram
records `cargo test`, `cd repo && cargo test` and `cargo fmt --check && cargo
test`. It does not record `cargo test 2>&1 | tail -20`, `cargo test || true`,
`cargo test; echo done`, `cargo test; cargo clippy` or background jobs, because
the shell reports another command's status. After the first verification, only
`&&` may join further commands. To keep output short and still record the
result, enable `pipefail` first in its own statement:
`set -o pipefail; cargo test 2>&1 | tail -40`. A `pipefail` set after `&&` or
`||`, or switched off again (`set +o pipefail`, zsh `setopt no_pipe_fail`), does
not count. Runner wrappers that return the tool's status are recognized
(`uv run`, `poetry run`, `npx`, `uvx`, `timeout N`, `python3 -m pytest`).
Listing or help invocations such as `cargo test -- --list`, formatters without
a check flag (`cargo fmt`, `terraform fmt`, `ruff format`), `--fix` runs and
`make` targets other than `test`, `lint`, `check` and `build` are not
verification. A client's `is_error` flag alone is not an exit status.

The directory comes from the same reading of the command: static `cd` or
`pushd` targets joined to the verification by `&&` (`cd ../two && cargo test`,
`export A=1; cd ../two && cargo test`). A change that might not take effect
(`cd ../two; cargo test`, a `cd` with variables, `cd -`, `popd`, sourcing a
script other than a virtualenv `activate`) or a change after the first
verification makes the run unrecorded, so it is never attributed to the wrong
repository.

Each episode keeps the latest result per verification: its directory and
commands without wrappers or redirections, so `cargo test 2>&1 | tail` and
`cargo test` share one result while `cargo test -p a` and `cargo test -p b` do
not. Before contacting the daemon, the hook marks a recognized verification's
result unknown; only a recorded observation replaces it. A hidden status, an
interrupted run, a failure without an exit code, or a record the daemon refuses
therefore leaves the result unknown. At session end the episode closes as
`failure` if any latest result failed, as `success` only if every latest result
passed and at least one ran tests, and as `abandoned` otherwise. A passing
lint, format, check or build never hides a failed test.

Each record keeps the last 4 KiB of output, the Git HEAD and SHA-256 digests of
the files the session touched or left modified (up to 64 files, 512 KiB each and
4 MiB in total; files beyond these bounds get no digest). If the agent declared
an Engram task in the same session through Engram's own MCP server (`engram`, or
the legacy `leb`, for example `mcp__engram__engram_task_start`, or a task ID
passed to its attempt or evidence tools), new records go to that task while the
daemon reports it open, so a later `engram_complete_task` can attach them to its
lessons. A task completed through MCP, or closed elsewhere (CLI, web UI), stops
receiving records; they return to the session's own episode. Evidence from runs
before the declaration stays on the session episode. One tool call is one
observation: a hook fired twice for the same call records once, and a new call
with identical output is recorded again.

The evidence judge accepts a hook record for a touched file that was re-indexed
after the run (for example by the commit watcher) only when the recorded digest
equals the indexed digest: the run observed exactly those bytes. Evidence that
agents report through MCP or completions has no such digests and keeps the
stricter rule: it must be newer than the indexed version of every touched file.
Only the hook path can attach a hook label and digests: the daemon removes them
from metadata that the CLI, MCP or other clients supply. Only new runs carry
digests; existing evidence is not changed.

## 6. Day to day: what's automatic vs. what you do

**Automatic with active native hooks:** context injection, evidence capture and
episode bookkeeping, identifiable completed tool calls, the repository files they read
or edit (paths and Read sizes, never content) and available provider usage.
The **Effectiveness** tab shows measurements and gaps. An enabled evidence judge
reviews pending lessons automatically. The daemon also handles
re-indexing on commits and branch switches, origin refresh, guide regen,
vault export, daily backups (7 kept), link discovery, drift quarantine.

**Worth doing deliberately:**

- Record durable lessons: through MCP, `engram_learn` with a claim phrased for
  a future engineer, linked to the task and its passing evidence.
  Candidates never inject until promoted, so err on the side of recording.
- Curate in the browser: Validate a good candidate, Forget a bad memory.
  Forgotten memories stay forgotten (local forgets win over sync).
- Add facts and links when you learn something structural.
- `engram context "your task" --path <repo> --budget 2000` any time you want
  to see exactly what an agent would receive, with a why for every item.

## 7. Freshness: never a full rediscovery

Engram's watcher polls each tracked repo's **committed HEAD every 30 s** (and
origin/main every 15 min). When it changes — a commit, a branch switch, a
pull — re-indexing is **incremental by diff**: Engram computes the changed
paths between the previously indexed HEAD and the new one (plus any files
that were locally dirty at either scan) and rescans only those files. A
full rescan happens only when incremental is unsafe: unknown prior state,
pruned/unrelated revisions, or a first index.

That means switching branches costs a per-commit delta, not a repo
rediscovery — and uncommitted edits never churn the graph (dirty files
influence retrieval bias, not indexing).

## 8. Skill packs

```bash
engram skills add https://github.com/DietrichGebert/ponytail
engram skills update      # pull every pack, re-install everywhere
```

Packs of agent-native skills (`<name>/SKILL.md`) install into every
detected agent (`~/.claude/skills`, `~/.codex/skills`), where Claude Code
and Codex trigger them natively when relevant. Engram marks what it installs
and never clobbers a hand-written skill of the same name.

## 9. Sync across machines

Point any machine at a central Engram (see the rollout guide):

```toml
# ~/.config/engram/config.toml
[sync]
central_url = "https://engram.internal.example.com"
token = "…"
```

The daemon then syncs automatically (default every 15 min; `engram sync` runs
one cycle by hand): central **validated** knowledge, facts, and links
replicate down; your locally-validated lessons are offered **up as
candidates** for curation. Your forgets always win locally, and offered
knowledge never ping-pongs back.

## 10. Maintenance

| Task | Command |
|---|---|
| Health check | `engram doctor` |
| Ask the workspace a question | `engram ask "…" --path <repo> --claude` (cited answer from memories, facts, code symbols, and doc sections; `--dry-run` shows the evidence) |
| Message another live session | `engram relay send <session\|all> "…"` (`engram relay board` lists them; delivered at the next prompt) |
| Update Engram in place (pull + atomic rebuild) | `engram update` (`--check` to only report) |
| Backup now (transactional; daily automatic, 7 kept) | `engram backup create` |
| Verify a backup | `engram backup verify` |
| See what would inject and why | `engram context "…" --path <repo> --json` |
| Regenerate the architecture guide | `engram architect --claude` |
| Restart the daemon from this terminal | `engram daemon restart` (waits up to `--wait-secs`, default 120, for in-flight work) |
| Stop everything | `engram daemon stop` |

Never copy `brain.sqlite` with `cp` — WAL contents are lost. Backups only.

## 11. Troubleshooting

- **Hook prints nothing in a repo** — the repo isn't registered
  (`engram index <repo>`), or the daemon is down (`engram doctor`).
- **No Engram context in Cursor** — Cursor receives context after the first
  file or shell tool of a turn, not at the prompt. Check the entries under
  Customize, Hooks, and that `engram hooks cursor --install` targeted the
  directory Cursor reads.
- **Stale responses after upgrading** — restart the daemon
  (`engram daemon restart`, or `engram daemon stop` and let the next CLI call
  auto-start the new binary).
- **Encyclopedia looks stale in the browser** — the page is no-cache, but
  a long-lived tab may need a reload; the data underneath refreshes on the
  watcher cadence.
- **A memory looks wrong** — Forget it in the browser or via MCP
  `engram_forget`; if it was drift (the code changed under it), it is already
  quarantined automatically.
- **`checkout_access` fails, or a hook or tool reports "macOS privacy
  permission denied for … in engramd"** — the daemon cannot read that
  repository. On macOS a process inherits privacy (TCC) folder access from
  the app that started it, and an agent hook can auto-start `engramd` from an
  app that may not read `~/Documents`, `~/Desktop` or `~/Downloads`. Indexing,
  identity, Git state and evidence-review source checks then fail for those
  repositories. `engram doctor` names the denied paths and the process chain
  that started the daemon. Fix it with `engram daemon restart` from a terminal
  that can read the folders, or grant `engramd` access in System Settings >
  Privacy & Security (Full Disk Access or Files and Folders). Each reinstall
  puts `engramd` at a new path under `~/.local/share/engram/releases/`, so such
  a grant likely does not survive reinstalling. Engram does not restart the
  daemon on its own: a restart interrupts every connected session, and two
  apps with different folder grants would keep replacing each other's daemon.
- **`codex_hooks` warns** — Codex has not trusted Engram's hooks, so Codex
  sessions run without context injection or capture. Open Codex, run `/hooks`
  and trust the Engram entries.
- **`project_identity` warns** — see
  [Duplicate project registrations](#duplicate-project-registrations).

### Duplicate project registrations

`engram doctor` warns when live checkouts of one repository (same canonical
`origin` remote and root commit) belong to different project ids. This
happens when a project was registered without its Git identity (for example
while the daemon could not read Git), so a later clone could not be matched
to it, or when separate clones carry different `.engram/project.toml` pins.
Knowledge recorded under one id is not visible from the other.

Engram never merges or re-identifies projects automatically. A project
registered without Git identity gets its root commit and canonical remote
recorded once Git is readable; its id does not change. The remote is recorded
only when no other active project could be the same repository, so with
duplicates neither claims it, and a later clone without metadata registers as
a new project instead of silently joining one of them. Until a merge command
exists:

- Work in one checkout. `engram memory list --path <checkout>` shows the
  memories each id holds.
- Do not point one checkout's `project.toml` at the other id. Each recorded
  checkout path belongs to exactly one project, so that checkout would stop
  resolving.
- Deleting a clone you no longer use clears the warning; its records stay
  under its id.

A safe merge command would need: an explicit choice of surviving and retired
id; evidence that both are the same repository (the doctor's remote and root
commit match); a backup and a dry run that counts the rows to move; one
single-writer transaction that re-scopes project-keyed rows (memories, tasks,
attempts, evidence, context runs, facts, links and checkout paths) with
explicit rules for key collisions; an auditable merge event plus a tombstone
for the retired id so replay reproduces the result; rewritten pins for the
retired checkouts; and a re-index instead of moving derived indexes.

## Capturing operations lessons

Operations checks are evidence too. For `gcloud ... describe`, log queries, curl
probes, and runtime measurements, record `runtime_observation`, `trace`, or
`metric` with `strength=observational` and the actual bounded output. Use the
same project and task for evidence and learning. A successful process can use
`exit_code=0`; observations without a process may omit it. Do not label an ops
observation `test_result` or `build_result`: those kinds require deterministic
verification.

The shortest explicit MCP sequence after resolving the edited repository is:

1. `engram_task_start` with a short task summary.
2. `engram_record_evidence` with that task ID and the observed output.
3. `engram_mark_outcome` with `outcome=success` when the task succeeded.
4. `engram_learn` with that task ID and the returned evidence ID.

An attempt is optional. Acceptance may remain unknown; it is separate from
successful verification and remains a separate ROI signal. Learning returns a
candidate at confidence 0.50 plus the next review step. Review it in the local
web UI and click **Promote** to record human validation, or use the existing
stepwise, evidence-gated `engram memory promote` commands. Candidates never
inject automatically. Failure messages identify the missing step, wrong task
scope, or invalid evidence classification.

Hooks retain a bounded redacted prompt until a file tool or recognized
verification first touches a registered repository. Each repository touched
gets its own task, and SessionEnd closes all of them from their last recorded
verification. A prompt-only session creates no episode. Passing verification
does not imply user acceptance. Abandoned episodes are excluded from the
"last completed episode" brief.

Linked Git worktrees share the primary checkout's project identity. Registration
stores metadata at the shared primary checkout rather than creating a new
`.engram/` directory in the linked checkout. A worktree does not replace the
workspace's primary path. MCP project status refreshes the requested checkout;
`index_matches_checkout` reports whether its shared structural index matches
that checkout's HEAD and working-tree fingerprint.

`engram index --origin` uses the same separate origin project as the browser. It
does not replace the local checkout's graph or invalidate that checkout's lessons.
Relative paths resolve in the CLI's working directory. `--origin` cannot be
combined with `--changed` or `--graphify`; its JSON result contains `reference`.

If a wrong source view previously caused a symbol-drift quarantine, reindex the
intended local checkout and inspect the memory's provenance. An operator can run
`engram memory restore-drift MEMORY_ID --event EVENT_SEQUENCE --json`. This only
restores the exact latest `referenced_symbol_changed` quarantine when every
original source digest matches again. It rejects missing anchors, other causes,
stale event IDs and archived memories. Validation state and confidence do not
change; this cannot validate a candidate or remove a real source mismatch.

## Removing a captured prompt

Task prompts, attempt summaries, evidence text, and authored memories pass
through credential redaction before storage. This covers Stripe test/live keys,
GitHub tokens, AWS access keys, Bearer credentials, DSN passwords, and credential
query parameters, plus configured custom patterns. Review what you paste even
when redaction is enabled: arbitrary secrets cannot be recognized by shape.

```sh
engram task purge-prompt tsk_<uuid> --json
```

This removes the canonical prompt and its pending hook copy, keeps task outcome
and evidence provenance, records a purge event, and truncates obsolete WAL
frames. If a reader pins the WAL, the command reports the incomplete checkpoint;
retry after that reader finishes. Existing backups, exports, and independently
captured copies in other tasks are separate and are not rewritten. Use
`engram backup create` for a consistent post-purge snapshot.

## Measuring capture and replay

A repository refresh updates the code graph and source freshness. It does not
recover lessons from old conversations or validate incident claims. `engram ask`
can find indexed symbols and documentation even when few memories are validated;
historical lesson replay specifically checks which lessons the context compiler
would inject. Candidates awaiting review, stale memories and uncaptured lessons
are therefore replay misses. Review the supporting evidence in **Knowledge** or
record a new verification before making those lessons eligible.

`scripts/capture_eval.py` reports per-session candidate-to-validated conversion,
time to first validated memory, acceptance coverage, and abandoned-task rate.
Session conversion uses the original distillation's source tasks; later reuse
does not count as creation, even when sessions overlap. Memories lacking that
creation provenance are reported separately. Multi-session source attribution
must not be summed to obtain a unique-memory total.
Tasks without session IDs are reported as individual episodes. Brief false
positives require an explicit JSON array of `{ "relevant": true|false }` labels
via `--brief-labels`; absent labels produce **unknown**, never a fabricated zero.

```sh
engram backup create /tmp/capture-baseline.sqlite
python3 scripts/capture_eval.py --db /tmp/capture-baseline.sqlite --output /tmp/capture.json
python3 scripts/capture_eval.py --db ~/.engram/brain.sqlite --socket ~/.engram/engramd.sock \
  --replay fixtures/capture-replay.json --project backend=<registered-name> \
  --project agents=<registered-name> --output /tmp/replay.json
```

Replay uses the production context compiler with the actual checkout HEAD and
counts a lesson only if its expected content appears in delivered text. It
separately records matching memories retrieved before compact truncation. The
replay database must match the daemon's database; for a frozen replay, start an
isolated daemon serving the snapshot. Reports omit raw prompts, evidence
artifacts, and repository source, and make no model or cloud calls.

## Optional local embedding model (off by default, unmeasured)

Semantic retrieval is off by default (`[context] semantic_retrieval = false`).
When it is on, the default embedder is a hashed n-gram projection: a lexical
approximation, not a learned model. Engram can instead call a local embedding
model through an `OpenAI`-compatible `POST /v1/embeddings` endpoint, such as
Ollama's. No benchmark has shown that either embedder improves task outcomes.
The 2026-09-25 real-repository follow-up attributed Engram's added cost to the
context agents carried after following source pointers, not to the injected
text, and it did not measure memory retrieval. Treat the local model as an
experiment: measure it before you enable it for real work.

To try it, pull an embedding model. Chat models such as `qwen3:8b` are not
embedding models. Sizes are from ollama.com/library on 2026-10-04:

```sh
ollama pull nomic-embed-text     # 274 MB, 768 dimensions
ollama pull embeddinggemma       # 622 MB, 768 dimensions
```

Configure it in `~/.config/engram/config.toml` (or a workspace profile). A
repository's `.engram/config.toml` can turn `semantic_retrieval` on or off and
set the two bounds, but it cannot set provider settings.

```toml
[context]
semantic_retrieval = true

[semantic]
provider = "local"
model = "nomic-embed-text"
query_prefix = "search_query: "        # model-specific; see the example config
document_prefix = "search_document: "
```

Restart the daemon (`engram daemon stop`; the next CLI call starts it), then
embed the memories of each repository:

```sh
engram semantic rebuild <repo>
```

The rebuild reports how many vectors it embedded, kept unchanged, and removed.
It embeds outside the canonical writer in pages and skips memories whose text
has not changed. Each stored vector records its model identity
(`local:<model>`, plus a digest of the document prefix) and its dimension.
Retrieval compares only vectors of the configured identity and dimension.
After a change of model, prefix, or dimension, the previous vectors are not
searched. That repository gets lexical and structural retrieval only until a
rebuild has run. Retrieval also requests that rebuild: with background jobs
enabled, the daemon queues a `semantic_rebuild` job, at most one per repository
per hour. The rebuild deletes the other model's vectors for that repository.
The hashed vectors are always kept up to date.

If the endpoint is down, slow (over `query_timeout_ms`, default 2 s) or returns
an error, that request uses the hashed vectors instead. The endpoint is then
skipped for 30 seconds. The `why` note of every injected memory names the
embedder that served it, any fallback and its reason, and any coverage gap. To
see these notes, use `engram context "…" --path <repo> --json`. By default
Ollama unloads a model after five idle minutes (`OLLAMA_KEEP_ALIVE`). A query
that waits for the model to load again can exceed `query_timeout_ms` and fall
back.

Safety limits:

- The endpoint must be loopback (`localhost`, `127.0.0.0/8` or `[::1]`), also
  when `network.local_only = false`. `localhost` is pinned to 127.0.0.1.
- Proxies and redirects are refused.
- Each text is cut at 8 KiB, and a request holds at most 64 texts.
- Response sizes are capped in proportion to the vectors requested.
- Every request has a time limit.

Memory text and task text go to the local endpoint only.

### Measuring hashed against model embeddings

`scripts/eval_semantic_retrieval.py` compares the arms `off` (lexical and
structural), `hashed`, and `local` on one frozen snapshot with the same cases.
Each arm runs an isolated daemon on its own copy of the snapshot and rebuilds its
own vectors. The report contains hit@1/3/5 and MRR for the first relevant
injected memory and the irrelevant memories injected per case. It also contains
injections on negative cases, the embedder that served each trial, median
semantic scores of relevant and other memories, and latency. It never contains
memory text. A `local` run is invalid (exit 1) when any trial was served by a
fallback.

Plumbing check with the committed synthetic fixture (it is too small to
measure quality):

```sh
python3 scripts/eval_semantic_retrieval.py --daemon ~/.local/bin/engramd \
  --synthetic fixtures/semantic-retrieval-synthetic.json \
  --model nomic-embed-text --query-prefix "search_query: " \
  --document-prefix "search_document: " --output /tmp/esr-synthetic.json
```

Comparison on your own frozen snapshot. Cases use the replay format
(`fixtures/capture-replay.json`): a case counts as relevant only when a
validated memory's claim matches all of its `expected_patterns`. Mark a case
that should inject nothing with `"negative": true`.

```sh
caffeinate -dims python3 scripts/eval_semantic_retrieval.py \
  --daemon ~/.local/bin/engramd \
  --freeze-from ~/.engram/brain.sqlite --snapshot /tmp/esr-snapshot.sqlite \
  --cases <cases.json> --project <alias>=<registered-name> \
  --model nomic-embed-text --query-prefix "search_query: " \
  --document-prefix "search_document: " --output /tmp/esr-nomic.json
```

Run it once with `--min-similarity 0` first. Learned models often give
unrelated text a positive cosine similarity, so too many memories inject. Pick
a floor from the median semantic scores for relevant and other memories, then
repeat the run with `--min-similarity`. Good retrieval diagnostics alone do not
justify turning the stage on. They justify a paired task benchmark
(`docs/BENCHMARKS.md`) with the brain-off control unchanged.

## Product knowledge and living documentation

The browser now opens on **Product**. Add the product's purpose, intended behavior,
constraints and owner, then connect capabilities, services, repositories and
contracts. **Flows** records ordered steps through the system; attach a handler,
contract, test or observation to each step. **Repositories** retains the existing
origin/main and local-checkout views.

**Changes** accepts repository-relative changed paths or a unified diff. It shows
records whose sources intersect the change separately from possible downstream
consumers. The interaction map filters by capability, environment and search text.
A missing edge means coverage is unknown, not that the change is isolated.

Each documentation section separates human explanation from generated reference
material. Add sources using the repository, file path, symbol ID or evidence ID.
An optional revision assertion must match the indexed/evidence revision. Check
the review checkbox only after reading those sources. “Sources unchanged” refers
to the last indexed bytes, not live production. Reindexing updates affected source
inventories and requests review without replacing human explanations. Every save
has a revision; History lets you inspect previous text and source stamps.

**Start working on this** builds a cited context for the selected product, flow,
component or section. It uses the same compiler as the agent tool and CLI:

```sh
engram product show
engram product schema
engram product bundle ENTITY_ID "Describe the intended change" --budget 2000
engram product impact PROJECT_ID --file src/api.rs --diff change.patch
engram product history RECORD_ID
```

Use IDs returned by `product show`. Bundles label declared intent, inferred claims,
source freshness, prior verification revisions and coverage gaps. The copied text
fits the requested estimated-token budget; metadata returned by the CLI/browser
is separate. MCP returns only the budgeted text. This explicit context does not
change the normal compact injection prefix or the brain-off control.

Catalog edits can also be applied as JSON with `engram product apply change.json`.
For example, a new product declaration uses version 0:

```json
{
  "operation": "save",
  "id": "engineering-product",
  "expected_version": 0,
  "item": {
    "type": "entity",
    "value": {
      "kind": "product",
      "name": "Engineering product",
      "intent": "Help engineers understand and change the system safely",
      "basis": "declared"
    }
  }
}
```

Read `product schema` for complete entity, dependency, section and deployment
fields. Updating or archiving requires the current version. A stale edit fails
without overwriting another editor. Archive preserves revision history. To restore
an archived item, save its historical item with the current archived version.

Architecture guides now live in SQLite. Existing `ARCHITECTURE.md` vault exports
are imported once. `engram architect` saves to the catalog; `--out` also writes a
Markdown file. If `vault.enabled` is true, the existing Obsidian export still runs.
The product UI and guide no longer depend on enabling it.

### Finish work and review lessons

An optional evidence judge can process pending lesson proposals automatically with
a local model or TypeSafe Jev. Its citations, source checks and usage appear in
**Effectiveness**. An unchanged proposal is judged once; new evidence or a revised
proposal starts another review. When the judge finds a conflict with an existing
memory, it marks that memory suspect and leaves the proposal for your review in
**Knowledge**. Human review stays available. See [engineering memory and measured
effectiveness](SECOND_BRAIN.md) for setup, usage capture and isolated workspaces.

Agents can replace the end-of-task sequence with `engram_complete_task`: provide
the existing task ID, observed outcome, touched files, actual verification output
and up to five proposed lessons. A completion summary becomes one proposal when
no lessons are supplied. Use deterministic test/build/lint/typecheck results with
command and exit code, or observational runtime output, traces or metrics. Keep
failed verification in the record; it is not automatically attached as supporting
evidence. The call is retryable and reuses identical observations and proposals.

**Knowledge** is the consolidated review inbox. Edit a claim into a reusable rule,
inspect its evidence and any similar memories, then accept or reject it. Acceptance
uses the normal evidence gates and caps confidence at 0.50. Unsupported proposals
remain pending; attach actual supporting evidence through a completion retry.
Similarity alone does not establish a contradiction. Explicitly identifying a
contradiction quarantines the old claim. Accepted lessons do not mark the task
accepted by the user.

A proposal whose claim, rationale or evidence matches a prompt-injection pattern
shows a **Possible prompt injection** notice with the matched rules. The judge does
not review it. Edit the flagged text out, or tick the acknowledgment and accept it
as written; that acceptance is recorded as a human override. `engram doctor`
reports validated memories that match these patterns without changing them. See
[prompt injection and memory poisoning](SECURITY.md).

A proposal can include `expires_at_ms` for temporary operational knowledge.
Expired memories stop injecting. Reverification can renew that expiry through a
new successful task and a new review; rejected or quarantined duplicates require
an explicit correction rather than silent reactivation.

```sh
engram product show --reviews
engram product lesson completion.json
engram product signals TASK_ID
```

The lesson JSON has `operation: "complete"`, `"review"` or `"signal"`. Completion
fields mirror `engram_complete_task`; review fields mirror `engram_review_lesson`.
Delivery signals cite actual evidence and distinguish `verification`,
`commit_landed`, `pr_merged` and `user_acceptance`. Never set `accepted_by_user`
from a passing test, merge, landed commit or silence. Omit it when unknown.

For an isolated daemon, `engramd --config /absolute/path/config.toml` replaces the
user configuration. Give it separate database, socket, vault and web-port paths;
this is useful for local evaluation without touching the main workspace.

Internal architecture synthesis runs with hooks, tools and MCP disabled. It does
not open engineering episodes or store an agent session transcript. Explicit
`--claude` and configured Claude auto-generation still use your opted-in cloud
provider; local-runtime generation remains available.
