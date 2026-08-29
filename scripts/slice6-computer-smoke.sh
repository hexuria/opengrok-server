#!/usr/bin/env bash
# The goal, end to end: hire a coworker, it gets a computer of its own, and a model's tool call
# runs on that computer.
#
# Needs a Docker daemon. Skips rather than fails without one — a test that fails on a machine with
# no daemon gets deleted, and then nothing exercises this at all.
#
# Usage:  OG_PORT=1447 OG_DATABASE_URL=… scripts/slice6-computer-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
if ! docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
  echo "SKIPPED — no Docker daemon, so no local computers"
  exit 0
fi

BOX_ID=""
cleanup() { [ -n "$BOX_ID" ] && docker rm -f "$BOX_ID" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "1. somebody signs in and hires a coworker with a computer"
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=box-$(date +%s)@og.local" | jq -r '.accessToken')
[ -n "$token" ] || fail "no access token"

hired=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' \
  -d '{"name":"Ada","with_computer":true}')
coworker=$(echo "$hired" | jq -r '.id')
BOX_ID=$(echo "$hired" | jq -r '.boxId')
err=$(echo "$hired" | jq -r '.computerError // empty')
[ -z "$err" ] || fail "the computer could not be created: $err"
[ "$BOX_ID" != "null" ] && [ -n "$BOX_ID" ] || fail "no computer was assigned: $hired"
ok "hired $coworker with computer $BOX_ID"

echo "2. the computer is a real, running container"
state=$(docker inspect -f '{{.State.Running}}' "$BOX_ID" 2>/dev/null || echo "missing")
[ "$state" = "true" ] || fail "the box is not a running container: $state"
ok "container $BOX_ID is running"

echo "3. a run reaches the model, and its tool call lands on that computer"
# The mock door is scripted to ask for a shell command, so this exercises the whole chain without
# spending anything: model → reassembly → executor → the coworker's own box → result event.
run_id="run-box-$(date +%s)"
events=$(curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token" -H 'content-type: application/json' -d "$(cat <<JSON
{"threadId":"t-box","runId":"$run_id",
 "forwardedProps":{"coworkerId":"$coworker"},
 "messages":[{"id":"m1","role":"user","content":"run a command"}]}
JSON
)" --max-time 60 | sed -n 's/^data: //p')
[ -n "$events" ] || fail "no events"

last=$(echo "$events" | tail -1 | jq -r '.type')
[ "$last" = "RUN_FINISHED" ] || fail "the run ended as $last"
ok "the run completed"

echo "4. proof the work happened INSIDE the coworker's own computer"
# The file is written by the model's own tool call, not by this script. That is the difference
# between "the run finished" and "the work happened" — and the earlier version of this step wrote
# the marker itself, which proved only that the box was reachable.
if [ "${OG_MODEL_DOOR:-}" = "mock-tools" ] || echo "$events" | grep -q TOOL_CALL_RESULT; then
  seen=$(docker exec "$BOX_ID" cat /tmp/opengrok-tool-ran 2>/dev/null | tr -d '\n')
  [ "$seen" = "opengrok-tool-ran" ] \
    || fail "the tool did not run on the coworker's box (marker: '$seen')"
  ok "a file written by the model's tool call is on the coworker's box"
else
  # The echoing door never reaches for a tool; assert what is actually true instead of pretending.
  docker exec "$BOX_ID" sh -c 'echo reachable > /tmp/reachable' >/dev/null 2>&1 \
    || fail "could not reach the coworker's computer"
  ok "no tool was called by this door; the box is reachable and keeps files"
fi

echo "5. the run replays from the log after the fact"
# A run is readable only by whoever started it, so the owner's token is required here.
replay=$(curl -fsS "$BASE/ag-ui/runs/$run_id" -H "authorization: Bearer $token")
[ "$(echo "$replay" | jq -r '.status')" = "finished" ] || fail "the run did not persist"
ok "$(echo "$replay" | jq -r '.events | length') events replayed from Postgres"

echo
echo "PASS — slice 6: a coworker was hired, given a computer of its own, and used it."
