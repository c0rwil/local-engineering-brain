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
`engram daemon status` / `engram daemon stop` exist when you need them.
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

While the daemon runs, http://127.0.0.1:11436 serves the workspace browser:

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

Use your existing Claude Code or Codex client and its chosen model/provider.
Engram attaches through native hooks and MCP. No replacement launcher or local
model is required. `claude-mtplx` is only the optional MTPLX/local-model path.

```bash
engram hooks install --global   # Claude Code: ~/.claude/settings.json
engram agent-md --global        # the agent tool loop: ~/.claude/CLAUDE.md
claude mcp add --scope user engram -- "$HOME/.local/bin/engram-mcp"
codex mcp add engram -- "$HOME/.local/bin/engram-mcp"
engram hooks codex --install    # Codex native hooks; review them with /hooks
```

These commands assume the default Engram install location. Keep an existing
working `leb-mcp` connection rather than adding a duplicate server after a rename.
Codex's hook installer writes `$CODEX_HOME/hooks.json` (normally
`~/.codex/hooks.json`), repairs the old flat layout, preserves other hooks, and
uses an absolute executable path for GUI sessions. Open `/hooks` to review and
trust the installed definitions; newly installed or changed hooks do not run
until trusted. Start a new local session after connecting MCP.

| Client | Native connection |
|---|---|
| Claude Code CLI, IDE and local Code sessions in Claude Desktop | Shared Claude Code hooks and MCP settings |
| Codex CLI, desktop app and IDE on the same host | Shared Codex hooks and MCP configuration, subject to hook trust |
| Other GUI clients with local stdio MCP support | Register the absolute `engram-mcp` executable; tools are available, but MCP alone does not guarantee automatic capture |
| Remote/cloud agent sessions | Configure Engram on the execution host; local machine settings and localhost do not automatically follow |

Claude Desktop's ordinary Chat tab is separate from its Code sessions. Configure
its supported MCP integration separately; do not assume it runs coding hooks.
See the official [Claude Code desktop configuration](https://code.claude.com/docs/en/desktop#coming-from-the-cli),
[Codex hooks](https://learn.chatgpt.com/docs/hooks) and
[shared Codex MCP configuration](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

What the hooks do, invisibly, in every session — five injection moments:

| Hook | When | Effect |
|---|---|---|
| session-start | session opens | standing brief: open episodes from earlier sessions (resume or close), last completed outcome. In an untracked git repo: a one-line enrollment hint, nothing else |
| prompt | first prompt of a session | compiles task context and retains a redacted prompt; the first relevant tool use opens the episode |
| pretool | before file tools, Codex patches or Codex shell calls | repo-scoped task context; file graph shapes when explicit paths are available. Multi-file patches route to their edited repositories |
| bash | after each Bash tool call | recognizable verifications (test/build/lint/typecheck) recorded as deterministic evidence with exit codes — failures included |
| session-end | session closes | episode auto-closed conservatively (success only if the last verification passed) |

The global hooks are **safe by design**: they engage only in repositories
already registered in the brain and silently no-op everywhere else — a
repo joins the brain only when you `engram index` it. Every hook fails open;
a broken brain can never break a coding session.

MCP exposes the episode lifecycle to agents; clients without native hooks must
call those tools to capture it. Optional local-model proxy injection runs through
`engram-proxy` on 127.0.0.1:11435 (`x-engram-mode: off` is a true control).

## 6. Day to day: what's automatic vs. what you do

**Automatic with active native hooks:** context injection, evidence capture and
episode bookkeeping, identifiable completed tool calls and available provider usage.
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
| Stop everything | `engram daemon stop` |

Never copy `brain.sqlite` with `cp` — WAL contents are lost. Backups only.

## 11. Troubleshooting

- **Hook prints nothing in a repo** — the repo isn't registered
  (`engram index <repo>`), or the daemon is down (`engram doctor`).
- **Stale responses after upgrading** — restart the daemon
  (`engram daemon stop`; the next CLI call auto-starts the new binary).
- **Encyclopedia looks stale in the browser** — the page is no-cache, but
  a long-lived tab may need a reload; the data underneath refreshes on the
  watcher cadence.
- **A memory looks wrong** — Forget it in the browser or via MCP
  `engram_forget`; if it was drift (the code changed under it), it is already
  quarantined automatically.

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
**Effectiveness**. Human review stays available. See [engineering memory and measured
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
