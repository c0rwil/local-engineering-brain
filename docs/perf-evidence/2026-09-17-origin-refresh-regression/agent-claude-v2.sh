#!/bin/zsh
# Instrumented local benchmark agent (v2): Claude Code headless against the
# local MTPLX runtime through the per-arm LEB proxy. Differences from v1:
# stream-json transcripts (full tool/turn event log) written to stdout for
# the harness to archive, and the arm proxy's /leb/metrics snapshot saved
# next to the worktree metrics file. Harness supplies ENGRAM_DB_PATH,
# ENGRAM_SOCKET_PATH, ENGRAM_STATE_DIR, ENGRAM_PROXY_PORT, ENGRAM_MODE.
set -uo pipefail
prompt_file="$1"
export MTPLX_CLAUDE_MODEL_ID="${MTPLX_CLAUDE_MODEL_ID:-mtplx-qwen38-27b-optimized-speed}"
# Optional injection-selectivity arm: write the cap into this worktree's
# project-scoped LEB config so only this run's compiles are affected.
if [[ -n "${ENGRAM_MAX_MEMORIES:-}" ]]; then
  [[ "${ENGRAM_MAX_MEMORIES}" == <-> ]] || { print -u2 "ENGRAM_MAX_MEMORIES must be an integer"; exit 64 }
  mkdir -p .leb
  printf '[context]\nmax_injected_memories = %s\n' "${ENGRAM_MAX_MEMORIES}" > .leb/config.toml
fi
# This family measures the proxy alone in both arms; global hooks/MCP must not leak in.
mkdir -p .claude
printf '{"disableAllHooks":true}\n' > .claude/settings.json
prompt="$(cat "$prompt_file")"
/Users/corwil/.local/bin/claude-mtplx --strict-mcp-config --mcp-config '{"mcpServers":{}}' --dangerously-skip-permissions --max-turns 60 \
  --output-format stream-json --verbose -p "$prompt"
rc=$?
metrics_rc=0
curl --fail --silent --show-error --max-time 3 "http://127.0.0.1:${ENGRAM_PROXY_PORT}/engram/metrics" > .leb-proxy-metrics.json || metrics_rc=$?
/Users/corwil/.local/bin/claude-mtplx --stop-brain >/dev/null 2>&1 || true
[[ "$metrics_rc" -eq 0 ]] || exit 70
exit $rc
