#!/usr/bin/env bash
# Run one LEB benchmark arm through stock Claude Code with the HOOK pipeline
# (all five injection moments) instead of the proxy. Restricted to the
# synthetic conveyor fixture.
#
# brain arm: the arm repo's .leb/config.toml points storage/daemon at the
#            arm's isolated db+socket, so the machine's user-level LEB hooks
#            (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse,
#            SessionEnd) deliver the arm snapshot's knowledge — exactly the
#            production pipeline, isolated per arm.
# off arm:   .claude/settings.json sets disableAllHooks, so the control is
#            stock Claude Code with zero LEB involvement (spec P10).

set -Eeuo pipefail
umask 077

readonly ADAPTER_NAME="claude-code-hooks"
readonly DEFAULT_MODEL="claude-opus-4-8"
readonly EXPECTED_FIXTURE_HEAD="7311490905964b95284a2eed41bbb160b7bb0dec"
readonly EXPECTED_FIXTURE_AUTHOR="bench@example.invalid"
readonly SYNTHETIC_ACK="conveyor-tribal-v1"

claude_pid=""
watchdog_pid=""
timed_out=0
adapter_status="error"
claude_exit_code=2
raw_output=""
metrics_file="${ENGRAM_FRONTIER_METRICS_FILE:-.leb-bench-metrics.json}"
model="${ENGRAM_FRONTIER_MODEL:-$DEFAULT_MODEL}"
metrics_written=0
arm_daemon_started=0

say_error() { printf '%s: %s\n' "$ADAPTER_NAME" "$*" >&2; }
die() { say_error "$*"; exit 2; }
pid_is_live() { [[ "$1" =~ ^[1-9][0-9]*$ ]] && kill -0 "$1" 2>/dev/null; }

terminate_tree() {
  local root_pid="$1" signal_name="$2" child=""
  pid_is_live "$root_pid" || return 0
  for child in $(pgrep -P "$root_pid" 2>/dev/null || true); do
    terminate_tree "$child" "$signal_name"
  done
  kill "-$signal_name" "$root_pid" 2>/dev/null || true
}

write_metrics() {
  local final_exit_code="$1" tmp="${metrics_file}.tmp.$$" timeout_json=false
  [[ "$metrics_written" -eq 0 ]] || return 0
  [[ -n "$raw_output" ]] || return 0
  [[ -f "$raw_output" ]] || : > "$raw_output"
  [[ "$timed_out" -eq 1 ]] && timeout_json=true
  local injection_json="null"
  if [[ "$ENGRAM_MODE" == "brain" && -f "${ENGRAM_STATE_DIR}/leb-metrics.json" ]]; then
    injection_json="$(cat "${ENGRAM_STATE_DIR}/leb-metrics.json")"
  fi
  if jq -Rn \
    --arg adapter "$ADAPTER_NAME" \
    --arg status "$adapter_status" \
    --argjson exit_code "$final_exit_code" \
    --argjson timed_out "$timeout_json" \
    --arg model "$model" \
    --argjson leb "$injection_json" '
      [inputs | fromjson?] as $events
      | ($events | map(select(.type == "result")) | last // {}) as $result
      | {
          adapter: $adapter, status: $status, exit_code: $exit_code,
          timed_out: $timed_out, model: $model,
          reported: {
            duration_ms: ($result.duration_ms // null),
            num_turns: ($result.num_turns // null),
            total_cost_usd: ($result.total_cost_usd // null)
          },
          leb: $leb
        }
    ' < "$raw_output" > "$tmp"; then
    mv -f -- "$tmp" "$metrics_file"
    metrics_written=1
  else
    rm -f -- "$tmp"
  fi
}

cleanup() {
  local final_exit_code=$?
  trap - EXIT INT TERM
  if [[ -n "$watchdog_pid" ]] && pid_is_live "$watchdog_pid"; then
    kill "$watchdog_pid" 2>/dev/null || true
    wait "$watchdog_pid" 2>/dev/null || true
  fi
  if [[ -n "$claude_pid" ]] && pid_is_live "$claude_pid"; then
    terminate_tree "$claude_pid" TERM
  fi
  write_metrics "$final_exit_code" || true
  if [[ "$arm_daemon_started" -eq 1 ]]; then
    "$ENGRAM_CMD" --db "$ENGRAM_DB_PATH" --socket "$ENGRAM_SOCKET_PATH" daemon stop >/dev/null 2>&1 || true
  fi
  exit "$final_exit_code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

[[ $# -eq 1 ]] || die "usage: $0 PROMPT_FILE"
prompt_file="$1"
[[ -f "$prompt_file" && -s "$prompt_file" && ! -L "$prompt_file" ]] || die "bad prompt file"

for required_name in ENGRAM_DB_PATH ENGRAM_SOCKET_PATH ENGRAM_STATE_DIR ENGRAM_MODE ENGRAM_TASK_ID ENGRAM_BENCHMARK_ID; do
  [[ -n "${!required_name:-}" ]] || die "$required_name is required from the paired harness"
done
[[ "$ENGRAM_MODE" == "off" || "$ENGRAM_MODE" == "brain" ]] || die "ENGRAM_MODE must be off|brain"
case "$ENGRAM_BENCHMARK_ID" in
  conveyor-g2-opus-*) ;;
  *) die "safety guard refused an unrecognized benchmark id" ;;
esac
ack_file="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/.synthetic-ack"
[[ "$(cat "$ack_file" 2>/dev/null)" == "$SYNTHETIC_ACK" ]] || \
  die "reviewed-fixture ack file missing beside the adapter"
case "$ENGRAM_TASK_ID" in
  t2[0-4]_*) ;;
  *) die "safety guard permits only the reviewed t20-t24 synthetic tasks" ;;
esac

fixture_root="$(git rev-parse --show-toplevel 2>/dev/null || true)"
[[ -n "$fixture_root" && "$PWD" == "$fixture_root" ]] || die "agent must run at the fixture root"
[[ "$(git rev-parse HEAD)" == "$EXPECTED_FIXTURE_HEAD" ]] || die "unrecognized fixture commit"
[[ "$(git log --format='%ae' | LC_ALL=C sort -u)" == "$EXPECTED_FIXTURE_AUTHOR" ]] || \
  die "unrecognized fixture author"
grep -Eq '^name[[:space:]]*=[[:space:]]*"conveyor"[[:space:]]*$' Cargo.toml || die "not the conveyor fixture"

run_timeout_seconds="${ENGRAM_FRONTIER_TIMEOUT_SECONDS:-840}"
max_budget_usd="${ENGRAM_FRONTIER_MAX_BUDGET_USD:-5.00}"
ENGRAM_CMD="$(command -v leb || true)"
[[ -n "$ENGRAM_CMD" ]] || die "leb binary not found"
command -v jq >/dev/null || die "jq required"
CLAUDE_CMD="${ENGRAM_FRONTIER_CLAUDE_BIN:-$(command -v claude || true)}"
[[ -n "$CLAUDE_CMD" ]] || die "claude binary not found"

raw_output="${ENGRAM_STATE_DIR}/claude-hooks-output.jsonl"
timeout_marker="${ENGRAM_STATE_DIR}/claude-hooks-timeout"
rm -f -- "$raw_output" "$timeout_marker"

mkdir -p .claude
if [[ "$ENGRAM_MODE" == "off" ]]; then
  # Pure control: no hooks at all — including the machine's user-level LEB
  # hooks. Stock Claude Code, zero injection, zero capture.
  printf '{\n  "disableAllHooks": true\n}\n' > .claude/settings.json
  rm -rf .leb/config.toml
else
  # Point every LEB invocation from inside this repo (the user-level hooks
  # included) at the arm's isolated brain. The repo-scoped config override
  # is the production mechanism; nothing bench-specific runs in-band.
  mkdir -p .leb
  {
    printf '[storage]\ndb_path = "%s"\n\n' "$ENGRAM_DB_PATH"
    printf '[daemon]\nsocket_path = "%s"\n\n' "$ENGRAM_SOCKET_PATH"
    printf '[watch]\nenabled = false\n\n[web]\nenabled = false\n\n[vault]\nauto_refresh = false\n'
    if [[ -n "${ENGRAM_REVIEW_SECTION:-}" ]]; then
      printf '\n[context]\nreview_standards_section = true\n'
    fi
  } > .leb/config.toml
  printf '{}\n' > .claude/settings.json
  # Boot the arm daemon and build the arm world model so PreToolUse file
  # shapes resolve; the snapshot already carries the pinned project id and
  # its validated preference memories.
  "$ENGRAM_CMD" --db "$ENGRAM_DB_PATH" --socket "$ENGRAM_SOCKET_PATH" daemon status --json >/dev/null 2>&1 || \
    die "arm daemon did not start"
  arm_daemon_started=1
  "$ENGRAM_CMD" --db "$ENGRAM_DB_PATH" --socket "$ENGRAM_SOCKET_PATH" index . --json >/dev/null 2>&1 || \
    die "arm index failed"
fi


claude_arguments=(
  --strict-mcp-config
  --mcp-config '{"mcpServers":{}}'
  -p
  --model "$model"
  --effort "${ENGRAM_FRONTIER_EFFORT:-high}"
  --output-format stream-json
  --verbose
  --no-session-persistence
  --no-chrome
  --dangerously-skip-permissions
  --max-budget-usd "$max_budget_usd"
)

# Clean environment: no inherited ANTHROPIC_*/CLAUDE_* vars can leak into
# the arm (verified: nested markers and base-url overrides otherwise pass
# straight through). Stored login only needs HOME.
env -i HOME="$HOME" PATH="$PATH" USER="$USER" TERM="${TERM:-xterm}" \
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  "$CLAUDE_CMD" "${claude_arguments[@]}" < "$prompt_file" > "$raw_output" &
claude_pid=$!
(
  sleep "$run_timeout_seconds"
  if pid_is_live "$claude_pid"; then
    : > "$timeout_marker"
    terminate_tree "$claude_pid" TERM
    sleep 5
    terminate_tree "$claude_pid" KILL
  fi
) &
watchdog_pid=$!

set +e
wait "$claude_pid"
claude_exit_code=$?
set -e
claude_pid=""
if pid_is_live "$watchdog_pid"; then kill "$watchdog_pid" 2>/dev/null || true; fi
wait "$watchdog_pid" 2>/dev/null || true
watchdog_pid=""

# Injection telemetry from the arm daemon (brain arm only).
if [[ "$ENGRAM_MODE" == "brain" ]]; then
  "$ENGRAM_CMD" --db "$ENGRAM_DB_PATH" --socket "$ENGRAM_SOCKET_PATH" metrics --json \
    > "${ENGRAM_STATE_DIR}/leb-metrics.json" 2>/dev/null || printf 'null\n' > "${ENGRAM_STATE_DIR}/leb-metrics.json"
fi

[[ -f "$timeout_marker" ]] && timed_out=1
cat "$raw_output"
if [[ "$timed_out" -eq 1 ]]; then
  adapter_status="timeout"
  claude_exit_code=124
elif [[ "$claude_exit_code" -eq 0 ]]; then
  adapter_status="success"
else
  adapter_status="claude_error"
fi
write_metrics "$claude_exit_code"
exit "$claude_exit_code"
