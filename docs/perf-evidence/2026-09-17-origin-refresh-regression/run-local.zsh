#!/bin/zsh
set -euo pipefail
umask 077
export MTPLX_CLAUDE_PORT=8013
export MTPLX_LOG_DIR=/private/tmp/engram-refresh-regression-20260917/model-logs
export TMPDIR=/private/tmp/egrefresh17-local
mkdir -p "$TMPDIR"
cd /private/tmp/engram-refresh-regression-20260917
mkdir -p warmup-state warmup-repo/.claude
print '{"disableAllHooks":true}' > warmup-repo/.claude/settings.json
print 'fn main() { println!("warmup"); }' > warmup-repo/main.rs
cd warmup-repo
export ENGRAM_MODE=off
export ENGRAM_STATE_DIR=/private/tmp/engram-refresh-regression-20260917/warmup-state
export ENGRAM_DB_PATH=$ENGRAM_STATE_DIR/brain.sqlite
export ENGRAM_SOCKET_PATH=$ENGRAM_STATE_DIR/engramd.sock
export ENGRAM_PROXY_PORT=11439
claude-mtplx --stop-server > ../local-restart.log 2>&1 || true
claude-mtplx --strict-mcp-config --mcp-config '{"mcpServers":{}}' --dangerously-skip-permissions --no-session-persistence --max-turns 4 -p 'Read main.rs and report exactly what it prints. Do not edit files.' > ../local-warmup.log 2>&1
print ready > ../local.warmed
claude-mtplx --stop-brain >> ../local-restart.log 2>&1 || true
unset ENGRAM_MODE ENGRAM_STATE_DIR ENGRAM_DB_PATH ENGRAM_SOCKET_PATH ENGRAM_PROXY_PORT
cd ..
engram eval run local.yaml --output /private/tmp/engram-refresh-regression-20260917/local --json > local-result.json
print complete > local.complete
