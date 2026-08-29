#!/usr/bin/env bash
# Proves a run abandoned by a restart is picked up rather than left running forever.
#
# Durable is not the same as continuing. A run whose process died has every event safely in the log
# and nobody doing anything about it — so the client waits on an ending that will never come. This
# checks that a new process ends it, and says why.
#
# Usage:  OG_PORT=1447 OG_DATABASE_URL=… scripts/slice9-recovery-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
BIN="${OG_BIN:-./target/debug/opengrok}"
SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
: "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"
PSQL=(docker exec oag-dev-postgres-1 psql -U oag -d opengrok -tAc)
"${PSQL[@]}" "select 1" >/dev/null 2>&1 || fail "cannot reach Postgres to plant an abandoned run"

start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" \
  OG_MODEL_DOOR=mock RUST_LOG=warn "$BIN" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server did not come up"
}
stop_server() { kill "${SERVER_PID:-0}" 2>/dev/null || true; wait "${SERVER_PID:-0}" 2>/dev/null || true; }
trap stop_server EXIT

echo "1. a run exists and its process dies mid-flight"
start_server
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=rec-$(date +%s)@og.local" | jq -r '.accessToken')
account=$(python3 -c "
import base64,json,sys
p=sys.argv[1].split('.')[1]; p+='='*(-len(p)%4)
print(json.loads(base64.urlsafe_b64decode(p))['sub'])" "$token")
RUN="run-abandoned-$(date +%s)"

# Planted directly, because a real mid-flight kill is a race and this needs to be deterministic.
# The shape is exactly what a SIGKILL leaves behind: status running, a tool call with no result,
# and a lease that has long since expired.
"${PSQL[@]}" "insert into run_view (id, thread_id, status, event_count, updated_at_ms, account_id, leased_until_ms)
              values ('$RUN', 't-abandoned', 'running', 2, 1, '$account', 1)" >/dev/null
"${PSQL[@]}" "insert into events (stream_id, stream_seq, event_type, payload) values
   ('run/$RUN', 1, 'run-started', '{\"type\":\"started\",\"thread_id\":\"t-abandoned\",\"coworker_id\":null,\"at_ms\":1}'),
   ('run/$RUN', 2, 'run-emitted', '{\"type\":\"emitted\",\"seq\":0,\"at_ms\":1,\"payload\":{\"type\":\"RUN_STARTED\"}}'),
   ('run/$RUN', 3, 'run-emitted', '{\"type\":\"emitted\",\"seq\":1,\"at_ms\":1,\"payload\":{\"type\":\"TOOL_CALL_START\",\"toolCallId\":\"c1\",\"toolCallName\":\"shell\"}}')
   " >/dev/null
ok "planted $RUN: running, a shell call in flight, lease expired"

echo "2. it is genuinely stuck before anybody looks"
status=$(curl -fsS "$BASE/ag-ui/runs/$RUN" -H "authorization: Bearer $token" | jq -r '.status')
[ "$status" = "running" ] || fail "expected running, got $status"
ok "the client would wait on this forever"

echo "3. a new process picks it up"
stop_server
start_server
resolved=""
for _ in $(seq 1 40); do
  status=$(curl -fsS "$BASE/ag-ui/runs/$RUN" -H "authorization: Bearer $token" | jq -r '.status')
  if [ "$status" != "running" ]; then resolved="$status"; break; fi
  sleep 1
done
[ -n "$resolved" ] || fail "the abandoned run was never picked up"
ok "resolved to: $resolved"

echo "4. and it says what happened, including what it would not guess about"
# The command may have run, may have half-run, may never have started. Re-running it would repeat
# whatever it did, so the run says so instead of pretending to know.
reason=$(curl -fsS "$BASE/ag-ui/runs/$RUN" -H "authorization: Bearer $token" | jq -r '.failure')
echo "$reason" | grep -q "interrupted by a restart" || fail "the reason does not say why: $reason"
echo "$reason" | grep -q "shell" || fail "the reason does not name the call in flight: $reason"
echo "$reason" | grep -q "unknown" || fail "the reason should not claim to know if it ran: $reason"
ok "$(echo "$reason" | head -c 90)…"

echo "5. a run being served right now is NOT reclaimed"
# The lease is what tells a restart from a run that is simply still going. A live run must survive
# a sweep, or recovery would kill the work it exists to protect.
live="run-live-$(date +%s)"
future=$(python3 -c 'import time; print(int(time.time()*1000) + 600000)')
"${PSQL[@]}" "insert into run_view (id, thread_id, status, event_count, updated_at_ms, account_id, leased_until_ms)
              values ('$live', 't-live', 'running', 0, 1, '$account', $future)" >/dev/null
sleep 3
still=$("${PSQL[@]}" "select status from run_view where id = '$live'" | tr -d ' ')
[ "$still" = "running" ] || fail "a leased run was reclaimed: $still"
ok "a run with a live lease was left alone"

echo
echo "PASS — slice 9: a run abandoned by a restart is ended, and one still held is not."
