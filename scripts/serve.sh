#!/usr/bin/env bash
# Build and (re)start the dev server from .env.
#
# Exists because restarting by hand has three sharp edges that each cost a session:
# pgrep -f matches your own shell wrapper (and you kill yourself), SIGTERM drops the
# listener but graceful shutdown holds open SSE connections so the process lingers,
# and the port outlives the kill by a moment so the next binary fails to bind.
# docs/setup/running.md is the prose version.
#
# Usage:
#   scripts/serve.sh                                        # the real window
#   OG_MODEL_DOOR=mock-tools OG_AUTO_REVIEW_MOCK_VERDICT=ask scripts/serve.sh   # deterministic cards
set -euo pipefail

cd "$(dirname "$0")/.."

[ -f .env ] || { echo "no .env — copy .env.example and fill it (docs/setup/environment.md)" >&2; exit 1; }
set -a
# shellcheck disable=SC1091
source ./.env
set +a

# The gate owns its database; a dev server there would race the smoke suite's sweeps.
case "${OG_DATABASE_URL:-}" in
  *_gate) echo "OG_DATABASE_URL points at a gate database; refusing (docs/setup/gate.md)" >&2; exit 1 ;;
esac

PORT="${OG_BIND##*:}"
PORT="${PORT:-1447}"

echo "=== cargo build -p opengrok"
cargo build -p opengrok

# -x, never -f: -f matches this script's own command line.
if pids=$(pgrep -x opengrok); then
  echo "=== stopping running server (pid $pids)"
  kill $pids 2>/dev/null || true
  for _ in $(seq 1 10); do
    pgrep -x opengrok >/dev/null || break
    sleep 1
  done
  # Graceful shutdown holds open SSE connections; don't wait on a drain that never ends.
  if pgrep -x opengrok >/dev/null; then
    echo "=== still draining after 10s; kill -9"
    pkill -9 -x opengrok || true
    sleep 1
  fi
fi

# The port can outlive the process by a moment.
for _ in $(seq 1 5); do
  curl -fsS --max-time 1 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1 || break
  sleep 1
done

echo "=== starting on ${OG_BIND:-0.0.0.0:$PORT}"
nohup ./target/debug/opengrok >> "${OG_SERVE_LOG:-/tmp/opengrok-serve.log}" 2>&1 &
disown

for _ in $(seq 1 20); do
  if curl -fsS --max-time 2 "http://127.0.0.1:$PORT/health" >/dev/null 2>&1; then
    echo "=== up: http://127.0.0.1:$PORT/health (log: ${OG_SERVE_LOG:-/tmp/opengrok-serve.log})"
    exit 0
  fi
  sleep 1
done
echo "the server did not come up — read ${OG_SERVE_LOG:-/tmp/opengrok-serve.log}" >&2
exit 1
