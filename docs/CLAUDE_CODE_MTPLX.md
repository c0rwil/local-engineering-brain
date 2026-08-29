# Claude Code + MTPLX + LEB

## Installed topology

```text
Claude Code
  ├─ stdio MCP → leb-mcp → lebd
  └─ Anthropic /v1/messages → leb-proxy :11435 → MTPLX :8010
```

`leb-mcp` and `leb-proxy` auto-start `lebd`. MTPLX remains runtime-owned and can be stopped without
touching the durable brain.

## Claude MCP option

```bash
claude mcp add --scope user leb -- "$HOME/.local/bin/leb-mcp"
claude mcp get leb
```

Remove it with `claude mcp remove --scope user leb`.

## Optimized Speed launcher

The local launcher `~/.local/bin/claude-mtplx` starts
`Youssofal/Qwen3.8-27B-MTPLX-Optimized-Speed`, then starts the LEB proxy with MTPLX as its runtime,
and launches Claude Code with:

```text
ANTHROPIC_BASE_URL=http://127.0.0.1:11435
ANTHROPIC_CUSTOM_HEADERS=x-leb-project-path: <launch working directory>
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

The exact FP16-tensor sibling is an explicit third option:

```bash
claude-mtplx --stop-server       # required before changing a running model
claude-mtplx-quality-fp16
```

Useful controls:

```bash
claude-mtplx --server-status
claude-mtplx --stop-brain
claude-mtplx --stop-server
leb daemon status
leb daemon stop
```

`--help` and `--version` never start MTPLX. The launcher resolves its dependencies instead of
assuming Homebrew paths; overrides include `MTPLX_BIN`, `CLAUDE_BIN`, `LEB_BIN_DIR`, and
`LEB_STATE_DIR`. Runtime startup uses filesystem locks and exact validated PID files.

Set `LEB_MODE=off` before either launcher to route the same Claude Code/MTPLX configuration through
LEB's zero-injection control path. `LEB_TOKEN_BUDGET` is optional; when omitted, the live
project/user configuration supplies the budget. The wrapper converts explicit values into request
headers.

## Q8 / Optimized Quality

The downloaded Q8 model is `Youssofal/Qwen3.8-27B-MTPLX-Optimized-Quality`. It is approximately
29 GiB of model shards plus the shared vision/MTP assets, while Optimized Speed is approximately
20 GiB including its selected assets. On a 48 GiB Mac, close other memory-heavy applications before
using Quality.
Use `claude-mtplx-quality` to run it; `claude-mtplx` remains the faster default.

## Optimized Quality FP16-tensor sibling

`claude-mtplx-quality-fp16` selects
`Youssofal/Qwen3.8-27B-MTPLX-Optimized-Quality-FP16` exactly. Despite the repository suffix, its
weight matrices remain dynamically quantized at 8-bit; FP16 is used for the remaining floating
tensors. The artifact is approximately 29.4 GB and its published parent-model peak is 32.7 GB, so
it fits a 48 GB machine with reasonable application headroom.

This sibling exists for M1/M2 Macs, whose hardware does not run BF16 well. On M3 and newer Macs,
including M5, use `claude-mtplx-quality` for the native parent unless you specifically need to test
the FP16-tensor conversion. The launcher remains available for reproducibility and never starts on
`--help` or `--version`.

## Privacy boundary

LEB and MTPLX bind only to loopback/Unix sockets. Claude Code nonessential traffic is disabled by
the launcher. Do not change `proxy.bind` to a non-loopback address; configuration validation rejects
it. Project source is sent to the selected local inference runtime, not stored as raw transcripts by
LEB.
