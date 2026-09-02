# Running LEB as a central source of truth

One always-on machine holds the canonical brain, built from `origin/main` of
every tracked repository; teammates browse it, curate it, and their CI keeps
it fresh the moment anything merges. Local developer brains stay untouched —
their agents keep following the local checkout, which is correct for coding.

## Server setup

```bash
sh scripts/install.sh
export PATH="$HOME/.local/bin:$PATH"

# Clone every tracked repository once (read-only checkouts are fine).
leb onboard /srv/repos            # registers + indexes + auto-links + vault

# Seed the shared-truth views (afterwards the watcher maintains them).
# For each repo: click "Refresh from origin/main" in the browser, or POST
# the webhook below once per repo.
```

`~/.config/leb/config.toml` on the server:

```toml
[web]
bind = "0.0.0.0"          # LAN-visible; loopback is the default otherwise
port = 11436
token = "<long-random-token>"   # REQUIRED for any non-loopback bind

[watch]
enabled = true
local_poll_secs = 300      # server checkouts rarely move locally
origin_poll_secs = 900     # background safety net behind the webhook

[vault]
enabled = true

[architect]
auto = "claude"            # or "local" with a runtime, or "off"
```

Anything other than a loopback bind is refused without a token. The token
gates every `/api` and `/webhook` request; the page itself is static HTML.
Teammates open `http://server:11436/?token=<token>` — the token sticks for
the browser session. For anything beyond a trusted LAN, front it with TLS
(Caddy/nginx) — the token is a bearer credential.

## Instant freshness from CI

Add this job to each tracked repository so a merge to main updates the
central brain immediately (the origin poll remains as backstop):

```yaml
# .github/workflows/leb-central.yml
name: Update central LEB
on:
  push:
    branches: [main]
jobs:
  notify:
    runs-on: ubuntu-latest
    steps:
      - run: |
          curl -fsS -X POST "$LEB_CENTRAL_URL/webhook/repo-updated" \
            -H "authorization: Bearer $LEB_CENTRAL_TOKEN" \
            -H "content-type: application/json" \
            -d "{\"repo\": \"${{ github.event.repository.name }}\"}"
        env:
          LEB_CENTRAL_URL: ${{ vars.LEB_CENTRAL_URL }}
          LEB_CENTRAL_TOKEN: ${{ secrets.LEB_CENTRAL_TOKEN }}
```

The webhook fetches `origin/<default>`, refreshes that repo's shared-truth
view in a throwaway worktree, and every derived surface — diagrams, vault,
links, the architecture guide's staleness check — follows automatically.

## What flows where (today)

- **Central → humans**: the browser (map, service pages, memories with ROI,
  curation, symbol search) and the Obsidian vault on the server.
- **Repos → central**: webhook + origin polling; `leb fact set` and curation
  happen against the central brain (point the CLI at it from the server, or
  curate in the browser).
- **Developer laptops**: run their own LEB for agent context (local
  checkouts are the right source for coding). Branch→main propagation is
  natural: when a branch merges, the webhook updates central, so main's
  documentation is already current the moment the merge lands.

## Brain synchronization

Laptops sync with `leb sync` (or automatically: set `[sync]` locally):

```toml
[sync]
central_url = "http://leb.internal:11436"
token = "<the central token>"
every_secs = 900        # automatic cycles in the daemon; 0 = manual only
push = true             # also offer local validated memories upstream
```

- **Down**: central's validated memories, curated facts, and links replicate
  into the laptop and inject immediately (same memory identity everywhere;
  re-syncs converge; a locally forgotten memory stays forgotten).
- **Up**: locally validated memories arrive on central as CANDIDATES tagged
  `offered` — the browser's Validate button is the team curation queue.
  Replicas never echo back, so knowledge cannot ping-pong.
- Node anchors stay instance-local by design; commit-validity bounds travel
  because git history is shared.
