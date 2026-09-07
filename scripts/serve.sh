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

# A MOCK TURN THAT TAKES OBSERVABLE TIME. Every mock door replays a script already in memory, so
# without this a turn starts and finishes inside one millisecond: `isRunning` flips true then false
# with no roster frame in between, the client's green "working" dot is never drawn, and an answer
# cannot be watched arriving. Exported rather than defaulted in the binary because the tests and
# the smokes drive these same doors hundreds of times and pacing them would buy no assertion.
# Set OG_MOCK_DELTA_MS=0 to turn it off for a run that wants the old instant behaviour.
export OG_MOCK_DELTA_MS="${OG_MOCK_DELTA_MS:-90}"
# And a floor under every mock model call: per-delta pacing cannot make a one-line fixture answer
# visible, because it is one delta. Measured on the packaged app — the row was complete before the
# first sample and the working dot never painted. A fixture turn is two visible calls, so 1.2 s
# gives the two-to-three seconds of "thinking" that reads as reassuring rather than as slow; 0 off.
export OG_MOCK_MIN_TURN_MS="${OG_MOCK_MIN_TURN_MS:-1200}"
# And a ceiling on what the pacing may ADD, per model call. The catalogue answer is chunked
# word-per-delta, so 90 ms makes a 4,632-character help text take 79 s — measured, and reported as
# a stuck turn, because that is what it looks like from outside. The pause is shared out rather
# than lowered: min(90ms, ceiling/deltas), so a one-liner still types and a long answer scrolls.
export OG_MOCK_MAX_TURN_MS="${OG_MOCK_MAX_TURN_MS:-6000}"
echo "=== mock doors: ${OG_MOCK_DELTA_MS}ms/delta, floor ${OG_MOCK_MIN_TURN_MS}ms, ceiling ${OG_MOCK_MAX_TURN_MS}ms per call"

# WITH the mock catalogue. It is off by default so it cannot ship (see opengrok-server's
# Cargo.toml), but a dev server is exactly where it is wanted — and `OG_MODEL_DOOR=mock-cards`
# refuses to start without it, so a plain `cargo build -p opengrok` would leave you with a binary
# that will not boot from this .env.
echo "=== cargo build -p opengrok --features mock-fixtures"
cargo build -p opengrok --features mock-fixtures

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
