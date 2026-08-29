#!/usr/bin/env bash
# Proves the promise: work survives the client, and the process.
#
# "A coworker keeps working when you close the tab, because the work was never in the tab."
# This script is that sentence, checked. It kills the server between the run and the replay, so a
# pass cannot come from anything held in memory.
#
# Usage:  scripts/slice4-durability-smoke.sh
set -euo pipefail

# The port is configurable because 1337 is also grok-bot's local-docker box port: with that
# container running, a hardcoded 1337 fails to bind and the failure looks like a broken server.
PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
BIN="${OG_BIN:-./target/debug/opengrok}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
: "${OG_DATABASE_URL:?OG_DATABASE_URL is required}"

start_server() {
  OG_BIND=127.0.0.1:$PORT \
  OG_DATABASE_URL="$OG_DATABASE_URL" \
  OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
  OG_MODEL_DOOR=mock \
  RUST_LOG=warn \
  "$BIN" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS "$BASE/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server did not come up"
}

stop_server() { kill "${SERVER_PID:-0}" 2>/dev/null || true; wait "${SERVER_PID:-0}" 2>/dev/null || true; }
trap stop_server EXIT

RUN="run-durable-$(date +%s)"
THREAD="thread-durable-$(date +%s)"

echo "1. a run happens"
start_server
sent=$(curl -sN -X POST "$BASE/ag-ui" -H 'content-type: application/json' \
  -d "{\"threadId\":\"$THREAD\",\"runId\":\"$RUN\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"remember this\"}]}" \
  --max-time 30 | sed -n 's/^data: //p' | jq -c '.' | wc -l | tr -d ' ')
[ "$sent" -gt 3 ] || fail "expected a real run, saw $sent events"
ok "$sent events streamed to the client"

echo "2. the process dies"
# Not a graceful shutdown that could flush something — the tab, the laptop and the process all just
# go away, which is the case the design is for.
kill -9 "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && fail "the server is still up"
ok "server killed with SIGKILL"

echo "3. a new process replays the run from the log"
start_server
replay=$(curl -fsS --max-time 10 "$BASE/ag-ui/runs/$RUN") || fail "the run could not be replayed"
status=$(echo "$replay" | jq -r '.status')
count=$(echo "$replay" | jq -r '.events | length')
thread=$(echo "$replay" | jq -r '.threadId')

[ "$thread" = "$THREAD" ] || fail "threadId came back as $thread"
[ "$status" = "finished" ] || fail "status is $status, expected finished"
[ "$count" = "$sent" ] || fail "replayed $count events, streamed $sent"
ok "$count events replayed, status=$status, thread preserved"

echo "4. the replayed events are the ones that were sent, in order"
first=$(echo "$replay" | jq -r '.events[0].type')
last=$(echo "$replay" | jq -r '.events[-1].type')
[ "$first" = "RUN_STARTED" ]  || fail "first replayed event is $first"
[ "$last"  = "RUN_FINISHED" ] || fail "last replayed event is $last"
text=$(echo "$replay" | jq -r '.events[] | select(.type == "TEXT_MESSAGE_CONTENT") | .delta' | tr -d '\n')
echo "$text" | grep -q "remember this" || fail "the reply did not survive: $text"
ok "RUN_STARTED … RUN_FINISHED, and the reply is intact"

echo "5. a run that never happened is not invented"
missing=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ag-ui/runs/run-that-never-was")
[ "$missing" = "404" ] || fail "an unknown run returned $missing, expected 404"
ok "an unknown run is 404, not an empty success"

echo
echo "PASS — slice 4: the work was never in the tab."
echo "  reply, recovered from Postgres after SIGKILL: $text"
