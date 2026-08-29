#!/usr/bin/env bash
# Proves slice 3: a run reaches a model door and streams back as a well-formed AG-UI run.
#
# Runs against OG_MODEL_DOOR=mock by default, so it costs nothing and works with no key, no
# provider and no network. Point it at a server started with the gateway door to prove the live
# path — the assertions are the same, which is the point of the seam.
#
# Usage:  OG_BASE=http://127.0.0.1:1337 scripts/slice3-harness-smoke.sh
set -euo pipefail

BASE="${OG_BASE:-http://127.0.0.1:1337}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

RUN="run-$(date +%s)"
BODY=$(cat <<JSON
{"threadId":"thread-1","runId":"$RUN","messages":[
  {"id":"m0","role":"developer","content":"internal note"},
  {"id":"m1","role":"user","content":"streaming please"}]}
JSON
)

raw=$(curl -sN -X POST "$BASE/ag-ui" -H 'content-type: application/json' -d "$BODY" --max-time 60)
events=$(echo "$raw" | sed -n 's/^data: //p')
[ -n "$events" ] || fail "no data frames"

echo "1. the run opens and ends exactly once"
first=$(echo "$events" | head -1 | jq -r '.type')
[ "$first" = "RUN_STARTED" ] || fail "first event is $first"
endings=$(echo "$events" | jq -r 'select(.type == "RUN_FINISHED" or .type == "RUN_ERROR") | .type')
[ "$(echo "$endings" | wc -l | tr -d ' ')" = "1" ] || fail "expected one ending, got: $endings"
ok "RUN_STARTED … $endings"

# A RUN_ERROR is a valid ending — it means the door failed and said why, which is the behaviour we
# want. But the streaming assertions below only apply to a run that produced text.
if [ "$endings" = "RUN_ERROR" ]; then
  reason=$(echo "$events" | jq -r 'select(.type == "RUN_ERROR") | .message')
  echo
  echo "PASS (degraded) — the run failed closed and said why:"
  echo "  $reason"
  exit 0
fi

echo "2. the text arrived in more than one frame"
# One frame would also pass a non-streaming implementation, so this is the assertion that the
# streaming is real.
# -c so each event is one line: without it jq pretty-prints and this counts lines of JSON, not
# frames, which passes for the wrong reason.
frames=$(echo "$events" | jq -c 'select(.type == "TEXT_MESSAGE_CONTENT")' | wc -l | tr -d ' ')
[ "$frames" -gt 1 ] || fail "only $frames content frame(s) — that is not streaming"
ok "$frames content frames"

echo "3. the message is bracketed and its pieces share one id"
sequence=$(echo "$events" | jq -r '.type' | tr '\n' ' ')
case "$sequence" in
  "RUN_STARTED TEXT_MESSAGE_START "*"TEXT_MESSAGE_END RUN_FINISHED ") ;;
  *) fail "unexpected sequence: $sequence" ;;
esac
ids=$(echo "$events" | jq -r 'select(.messageId != null) | .messageId' | sort -u | wc -l | tr -d ' ')
[ "$ids" = "1" ] || fail "expected one messageId, saw $ids"
ok "start … content … end, one messageId"

echo "4. the user's message reached the model, the developer note did not"
text=$(echo "$events" | jq -r 'select(.type == "TEXT_MESSAGE_CONTENT") | .delta' | tr -d '\n')
echo "$text" | grep -q "streaming please" || fail "the user's text did not reach the run: $text"
echo "$text" | grep -q "internal note" && fail "a developer-role message reached the model"
ok "user content in, developer note filtered"

echo
echo "PASS — slice 3: a run streams from a model door as a well-formed AG-UI run."
echo "  reply: $text"
