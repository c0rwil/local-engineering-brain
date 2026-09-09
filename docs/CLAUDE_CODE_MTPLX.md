# Claude Code + MTPLX + Engram

## Installed topology

```text
Claude Code
  ├─ stdio MCP → engram-mcp → engramd
  └─ Anthropic /v1/messages → engram-proxy :11435 → MTPLX :8010
```

`engram-mcp` and `engram-proxy` auto-start `engramd`. MTPLX remains runtime-owned and can be stopped without
touching the durable brain.

## Claude MCP option

```bash
claude mcp add --scope user engram -- "$HOME/.local/bin/engram-mcp"
claude mcp get engram
```

Remove it with `claude mcp remove --scope user engram`.

## Optimized Speed launcher

The local launcher `~/.local/bin/claude-mtplx` starts
`Youssofal/Qwen3.8-27B-MTPLX-Optimized-Speed`, then starts the Engram proxy with MTPLX as its runtime,
and launches Claude Code with:

```text
ANTHROPIC_BASE_URL=http://127.0.0.1:11435
ANTHROPIC_CUSTOM_HEADERS=x-engram-project-path: <launch working directory>
```

Run it from the repository whose brain should be active:

```bash
cd /path/to/repo
claude-mtplx
```

The Q8/Optimized Quality launcher uses the same safe topology:

```bash
claude-mtplx --stop-server       # required before changing a running model
claude-mtplx-quality
```

Useful controls:

```bash
claude-mtplx --server-status
claude-mtplx --stop-brain
claude-mtplx --stop-server
engram daemon status
engram daemon stop
```

`--help` and `--version` never start MTPLX. The launcher resolves its dependencies instead of
assuming Homebrew paths; overrides include `MTPLX_BIN`, `CLAUDE_BIN`, `ENGRAM_BIN_DIR`, and
`ENGRAM_STATE_DIR`. Runtime startup uses filesystem locks and exact validated PID files.

Set `ENGRAM_MODE=off` before either launcher to route the same Claude Code/MTPLX configuration through
Engram's zero-injection control path. `ENGRAM_TOKEN_BUDGET` is optional; when omitted, the live
project/user configuration supplies the budget. The wrapper converts explicit values into request
headers.

## Q8 / Optimized Quality

The downloaded Q8 model is `Youssofal/Qwen3.8-27B-MTPLX-Optimized-Quality`. It is approximately
29 GiB of model shards plus the shared vision/MTP assets, while Optimized Speed is approximately
20 GiB including its selected assets. On a 48 GiB Mac, close other memory-heavy applications before
using Quality.
Use `claude-mtplx-quality` to run it; `claude-mtplx` remains the faster default.

The upstream `Youssofal/Qwen3.8-27B-MTPLX-Optimized-Quality-FP16` sibling exists for M1/M2 Macs,
whose hardware does not run BF16 well; despite the repository suffix, its weight matrices remain
8-bit. On M3 and newer Macs, including M5, it offers no quality gain over
`claude-mtplx-quality`, so no launcher is shipped for it. If it is ever needed, pull it with
`mtplx pull` and run it through `mtplx serve` directly.

## Privacy boundary

Engram and MTPLX bind only to loopback/Unix sockets. Claude Code nonessential traffic is disabled by
the launcher. Do not change `proxy.bind` to a non-loopback address; configuration validation rejects
it. Project source is sent to the selected local inference runtime, not stored as raw transcripts by
Engram.
