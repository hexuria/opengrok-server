#!/usr/bin/env bash
# Proves slice 2: openbot can add OpenGrok as a Bot and get a well-formed run back.
#
# Every assertion is something an AG-UI consumer depends on. The event names and the envelope come
# from `@ag-ui/core` 0.0.57 — the version pinned in hexuria/openbot's app/package.json.
#
# Usage:  OG_BASE=http://127.0.0.1:1337 scripts/slice2-agui-smoke.sh
set -euo pipefail

BASE="${OG_BASE:-http://127.0.0.1:1337}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

THREAD="thread-$(date +%s)"
RUN="run-$(date +%s)"
BODY=$(cat <<JSON
{"threadId":"$THREAD","runId":"$RUN","messages":[{"id":"m1","role":"user","content":"ping"}]}
JSON
)

echo "1. the endpoint streams server-sent events"
headers=$(curl -sS -o /dev/null -D - -X POST "$BASE/ag-ui" \
  -H 'content-type: application/json' -d "$BODY" --max-time 10)
echo "$headers" | grep -qi "content-type: text/event-stream" \
  || fail "not an event stream: $(echo "$headers" | head -5)"
ok "content-type is text/event-stream"

raw=$(curl -sN -X POST "$BASE/ag-ui" -H 'content-type: application/json' -d "$BODY" --max-time 10)
events=$(echo "$raw" | sed -n 's/^data: //p')
[ -n "$events" ] || fail "no data frames in the response"

echo "2. the run opens and closes"
# A consumer holds its spinner open until the closing event; a run that never finishes hangs the UI.
first=$(echo "$events" | head -1 | jq -r '.type')
last=$(echo "$events" | tail -1 | jq -r '.type')
[ "$first" = "RUN_STARTED" ]  || fail "first event is $first, expected RUN_STARTED"
[ "$last"  = "RUN_FINISHED" ] || fail "last event is $last, expected RUN_FINISHED"
ok "RUN_STARTED … RUN_FINISHED"

echo "3. the message events are properly nested"
sequence=$(echo "$events" | jq -r '.type' | tr '\n' ' ')
expected="RUN_STARTED TEXT_MESSAGE_START TEXT_MESSAGE_CONTENT TEXT_MESSAGE_END RUN_FINISHED "
[ "$sequence" = "$expected" ] || fail "sequence was: $sequence"
ok "start, content, end — in order"

echo "4. our ids come back, not ids the server invented"
# openbot correlates its own UI against these; minting our own would orphan the reply.
got_thread=$(echo "$events" | head -1 | jq -r '.threadId')
got_run=$(echo "$events" | head -1 | jq -r '.runId')
[ "$got_thread" = "$THREAD" ] || fail "threadId came back as $got_thread"
[ "$got_run" = "$RUN" ]       || fail "runId came back as $got_run"
ok "threadId and runId echoed"

echo "5. one message id ties the pieces together"
# Different ids per event would scatter one reply across several bubbles.
ids=$(echo "$events" | jq -r 'select(.messageId != null) | .messageId' | sort -u | wc -l | tr -d ' ')
[ "$ids" = "1" ] || fail "expected one messageId across the message events, saw $ids"
ok "a single messageId across start/content/end"

echo "6. the request body actually reached the run"
echo "$events" | jq -r 'select(.delta != null) | .delta' | grep -q "ping" \
  || fail "the user's message did not reach the run"
ok "the user's text came back through the run"

echo
echo "PASS — slice 2: a well-formed AG-UI run streams from OpenGrok."
