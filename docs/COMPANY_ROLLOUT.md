# From one machine to a company-wide brain

The path from today's single-workstation Engram to a team deployment where
nobody ever pays for a rediscovery someone else already made. Server
mechanics live in [CENTRAL_SERVER.md](CENTRAL_SERVER.md); this document is
the rollout sequence and the operating model.

## The end state

```
                     merges to main (any tracked repo)
        CI webhook ────────────────────────────────┐
                                                   ▼
                                        ┌─────────────────────┐
   engineer laptops                     │   CENTRAL Engram        │
  ┌──────────────┐   pull validated     │  · origin/main views │
  │ local brain  │◄─────────────────────│  · validated KB      │
  │ (per person) │   offer candidates   │  · per-repo facts,   │
  └──────────────┘─────────────────────►│    links, guides     │
        │                               └─────────────────────┘
        │ local checkouts (any branch):          ▲
        │ incremental diff re-index,             │ curator validates
        │ agent context from working tree        │ offered candidates
        ▼                                        │
   agents (claude / codex / GUI / local models)
```

Central holds the **source of truth**: the knowledge bases for every repo
the company tracks, kept current from origin/main by CI. Engineers' local
instances **pull** that truth instead of rediscovering it, and **push**
their own accepted lessons up as candidates. Local checkouts of feature
branches stay first-class: agent context always compiles from the working
tree, refreshed **incrementally by diff** — never a whole-repo rescan.

## Phase 1 — stand up central, seeded with today's brain

The default knowledge base is the one that already exists: the currently
tracked repos (agi-backend-rust, agents, platform, mobile) with their
validated memories, facts, links, and architecture guides. Seed central by
restoring this machine's backup — central starts life as a copy of the
best brain we have:

```bash
# on the workstation that has the knowledge today
engram backup create                      # transactional snapshot

# on the central host
./scripts/install.sh
cp <backup>.sqlite ~/.engram/brain.sqlite # BEFORE first daemon start
# clone every tracked repo (read-only checkouts), then:
engram index <each-repo> --origin         # seed the shared origin/main views
```

Bind the browser/API beyond loopback **only** with a bearer token
(`[web] bind` + `token` — the daemon refuses a non-loopback bind without
one). Everyone can now browse the encyclopedia — service map, per-repo
docs, guides — with zero setup.

## Phase 2 — auto-update on every merge to main

Add the workflow from CENTRAL_SERVER.md to each tracked repo. On every
merge to main, CI POSTs `/webhook/repo-updated`; central fetches
origin/main and re-indexes **just the diff** since the last indexed
commit. Between webhooks, the watcher's 15-minute origin poll is the
safety net. Result: central is never more than one merge behind reality,
and no human ever refreshes anything.

Adding a repo to the company KB later is the same two steps: clone +
`engram index` on central, add the workflow. Its knowledge base starts from
structure (graph, tech stack, auto-links) and grows as people work in it.

## Phase 3 — roll out to engineers

Each engineer, once:

```bash
./scripts/install.sh
engram hooks install --global && engram agent-md --global   # every agent, every repo
engram index <the repos they work in>                    # opt-in per repo
```

and in `~/.config/engram/config.toml`:

```toml
[sync]
central_url = "https://engram.internal.example.com"
token = "…"          # per-user or shared bearer
```

From then on the daemon syncs automatically (15-minute default):

- **Down:** central's validated memories, facts, and links replicate into
  the local brain. Day-one engineers get the company's entire proven
  knowledge base before their first prompt — nobody burns their own
  subscription rediscovering what the team already validated.
- **Up:** lessons an engineer's sessions validate locally are offered to
  central as **candidates** (`synced_hypothesis` trust — they never inject
  for anyone else until a curator clicks Validate in the central browser).
  Curation is the only human step in the loop, by design: the company KB
  grows from real work, but only through a deliberate gate.
- Local forgets always win, and offered knowledge never ping-pongs back.

## Local branches: the incremental contract

The reason engineers never fall back to "huge rediscovery":

- **Reading the shared truth** costs nothing: the encyclopedia and the
  synced KB describe origin/main, maintained centrally by CI.
- **Working on a branch** costs a diff: the local watcher notices every
  commit and branch switch (30 s poll of the committed HEAD) and re-indexes
  only the paths changed between the previously indexed commit and the new
  one, plus files that were dirty at either scan. Full rescans occur only
  when incremental would be unsafe (first index, pruned/unrelated
  revisions, unknown prior state).
- **Uncommitted edits** never trigger indexing at all — dirty files bias
  retrieval toward the code being touched, but the graph updates on
  commits, keeping it deterministic.
- **Agent context is always the working tree.** The origin/main view is
  for shared documentation; the code an agent reasons about is the code
  the engineer actually has checked out.

## Operating notes

- One curator (or a rotation) reviews the offered-candidates queue in the
  central browser; cadence of minutes per week, not hours.
- Central's daily transactional backups (7 kept) are the disaster story;
  `engram backup verify` in a cron proves them restorable.
- Private repos stay private: sync moves *knowledge* (memories, facts,
  links), never source code. Engineers without access to a repo simply
  have nothing local for its knowledge to attach to.
- Scale ceiling is generous: a single daemon is one SQLite writer with a
  bounded read pool — hundreds of engineers polling sync every 15 minutes
  is idle load. Shard by workspace only if you track thousands of repos.
