#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

VLLM_HEALTH_URL="${VLLM_HEALTH_URL:-http://127.0.0.1:8000/v1/models}"
VLLM_LOG="${VLLM_LOG:-/tmp/career-tools-vllm.log}"
VLLM_PID_FILE="${VLLM_PID_FILE:-/tmp/career-tools-vllm.pid}"
VLLM_LOCK_DIR="${VLLM_LOCK_DIR:-/tmp/career-tools-vllm.lock}"
VLLM_STARTUP_TIMEOUT="${VLLM_STARTUP_TIMEOUT:-900}"

healthy() {
  curl --silent --fail --max-time 2 "$VLLM_HEALTH_URL" >/dev/null
}

if healthy; then
  echo "vLLM already healthy at $VLLM_HEALTH_URL"
  exit 0
fi

if ! mkdir "$VLLM_LOCK_DIR" 2>/dev/null; then
  echo "another vLLM startup is in progress; waiting for health"
else
  trap 'rmdir "$VLLM_LOCK_DIR"' EXIT

  if [[ -f "$VLLM_PID_FILE" ]] && kill -0 "$(cat "$VLLM_PID_FILE")" 2>/dev/null; then
    echo "vLLM process $(cat "$VLLM_PID_FILE") is running but not healthy yet"
  else
    echo "starting vLLM; logs: $VLLM_LOG"
    nohup scripts/serve-qwen-vllm.sh >>"$VLLM_LOG" 2>&1 &
    echo "$!" >"$VLLM_PID_FILE"
  fi
fi

deadline=$((SECONDS + VLLM_STARTUP_TIMEOUT))
while (( SECONDS < deadline )); do
  if healthy; then
    echo "vLLM healthy at $VLLM_HEALTH_URL"
    exit 0
  fi
  sleep 5
done

echo "vLLM did not become healthy within ${VLLM_STARTUP_TIMEOUT}s; see $VLLM_LOG" >&2
exit 1
