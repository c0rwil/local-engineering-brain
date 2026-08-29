#!/bin/sh

set -eu
umask 077

prompt_file=${1:?prompt file is required}
test -s "${prompt_file}"
case "${LEB_MODE:?LEB_MODE is required}" in
  off|brain) ;;
  *) echo "unexpected LEB_MODE: ${LEB_MODE}" >&2; exit 2 ;;
esac
test -n "${LEB_DB_PATH:?isolated LEB_DB_PATH is required}"
test -n "${LEB_SOCKET_PATH:?isolated LEB_SOCKET_PATH is required}"
test -n "${LEB_STATE_DIR:?isolated LEB_STATE_DIR is required}"
test -n "${LEB_PROXY_PORT:?isolated LEB_PROXY_PORT is required}"

{
  printf 'mode=%s\n' "${LEB_MODE}"
  sed -n '1p' "${prompt_file}"
} > answer.txt

printf '%s\n' '{"success":true,"prompt_tokens":8,"completion_tokens":4,"tool_calls":0,"leb_compile_ms":0}' \
  > .leb-bench-metrics.json
