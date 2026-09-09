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
(declaring another repo's hostname in your code creates a `calls-api-of`
link with file evidence), and render in the encyclopedia.

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

An Obsidian vault mirror (notes + mermaid diagrams) exports continuously to
`~/.engram/vault`.

## 5. Connect your agents (once, machine-wide)

```bash
engram hooks install --global   # Claude Code: ~/.claude/settings.json
engram agent-md --global        # the agent tool loop: ~/.claude/CLAUDE.md
engram hooks codex              # Codex: MCP server + AGENTS.md instructions
```

Codex gets the same automatic loop: `engram hooks codex --install` writes
`~/.codex/hooks.json`, where context returns through
`hookSpecificOutput.additionalContext`.

What the hooks do, invisibly, in every session — five injection moments:

| Hook | When | Effect |
|---|---|---|
| session-start | session opens | standing brief: open episodes from earlier sessions (resume or close), last completed outcome. In an untracked git repo: a one-line enrollment hint, nothing else |
| prompt | first prompt of a session | opens a task episode, compiles brain context for that exact task, injects it |
| pretool | before Read/Edit/Write | the file's graph shape — symbols it defines, who calls into it from elsewhere, imports — lands before the content does; once per file per session |
| bash | after each Bash tool call | recognizable verifications (test/build/lint/typecheck) recorded as deterministic evidence with exit codes — failures included |
| session-end | session closes | episode auto-closed conservatively (success only if the last verification passed) |

The global hooks are **safe by design**: they engage only in repositories
already registered in the brain and silently no-op everywhere else — a
repo joins the brain only when you `engram index` it. Every hook fails open;
a broken brain can never break a coding session.

Other front ends: GUI clients get the full episode lifecycle over MCP
(`engram-mcp`); local models get transparent injection through the proxy
(`engram-proxy` on 127.0.0.1:11435, `x-engram-mode: off` for a true control).

## 6. Day to day: what's automatic vs. what you do

**Automatic:** context injection, evidence capture, episode bookkeeping,
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
