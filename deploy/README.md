# Engram deployment — AGI rollout kit

The concrete path from today's single workstation to the company brain.
Generic background: [../docs/COMPANY_ROLLOUT.md](../docs/COMPANY_ROLLOUT.md)
and [../docs/CENTRAL_SERVER.md](../docs/CENTRAL_SERVER.md). This file is the
AGI-specific runbook.

## What we're deploying

One **central Engram** holding the validated knowledge base for the four
tracked repos — `agents` (api.agi.tech, orchestration), `agi-backend-rust`
(androidagent.agi.tech, personal/company models, direct device↔agent),
`mobile` (Android client monorepo + eve-agent/hermes runtimes), `platform`
(platform.agi.tech, console/billing) — kept current from origin/main by CI,
with every engineer's local instance syncing validated truth down and
offering new lessons up for curation.

## 1. Central host

```bash
git clone git@github.com:corwil-agi/agi-engineering-brain.git && cd agi-engineering-brain
./scripts/install.sh && export PATH="$HOME/.local/bin:$PATH"

# Seed with the existing brain (Carlos's workstation is the source of truth
# today): on the workstation `engram backup create`, copy the backup here as
# ~/.engram/brain.sqlite BEFORE first daemon start. Never `cp` a live db.

# Clone the four product repos (read-only checkouts are fine), then:
for r in agents agi-backend-rust mobile platform; do engram index ~/repos/$r --origin; done
```

Central config — [`config.central.toml`](config.central.toml) → `~/.config/engram/config.toml`.
The daemon **refuses** a non-loopback bind without the bearer token; set a real
one. Front with your TLS of choice at e.g. `https://engram.internal.agi.tech`.

## 2. Instant freshness from CI

Drop [`engram-central.yml`](engram-central.yml) into each product repo's
`.github/workflows/`, set the two secrets (`LEB_CENTRAL_URL`,
`LEB_CENTRAL_TOKEN`). Every merge to main re-indexes just the diff on
central. The 15-minute origin poll is the safety net between webhooks.

## 3. Engineer machines (one time, ~2 minutes)

```bash
git clone git@github.com:corwil-agi/agi-engineering-brain.git && cd agi-engineering-brain
./scripts/install.sh && export PATH="$HOME/.local/bin:$PATH"

engram hooks install --global        # Claude Code: all five injection moments
engram agent-md --global             # the agent tool loop
engram hooks codex --install         # Codex: same automatic loop
engram index <each repo they work in>   # opt-in per repo — hooks stay silent elsewhere
```

Engineer config — [`config.engineer.toml`](config.engineer.toml) → append the
`[sync]` block to `~/.config/engram/config.toml`. From then on: validated company
knowledge replicates down before their first prompt; their locally-proven
lessons flow up as candidates. Nobody burns their own subscription
rediscovering what the team already validated.

## 4. Curation (the only human step)

Central's browser (`http://127.0.0.1:11436` on the host, or the fronted URL)
lists offered candidates per repo — Validate the good ones, Forget the bad.
Minutes per week. Nothing an agent claims injects for anyone else until a
human clicks Validate or deterministic evidence promotes it.

## Operating notes

- **Updates:** `engram update` on any machine (pulls this repo, atomic
  reinstall). Tag pushes build prebuilt bundles via `release.yml`.
- **Backups:** daily transactional backups are automatic (7 kept);
  `engram backup verify` in cron proves them restorable.
- **Privacy:** sync moves *knowledge* (memories/facts/links), never source.
  Local forgets always win; offered knowledge never ping-pongs.
- **Windows:** not supported (Unix-socket daemon); macOS + Linux.
- **Benchmarks backing all of this:** [../docs/BENCHMARKS.md](../docs/BENCHMARKS.md) —
  most recently the five-moment hook pipeline at 10/10 vs 6/10 control,
  −19% wall, on Opus (2026-09-03).
