#!/usr/bin/env bash
# Proves slice 8 of the port ladder: one conversation, the way the desktop client holds one.
#
# The choreography under test is client-grok-bot.md §2.4: sendPrompt answers {accepted:true}
# immediately; the user's message comes back over SSE carrying the clientNonce (that echo settles
# the optimistic bubble); the answer appears as a send-message entry that ends un-streaming; and
# agent-upserted frames pulse isRunning around the turn. Plus the two contracts that fail silently
# in the app: window replies MUST carry threadCounts, and a reused nonce with different input MUST
# be refused rather than absorbed.
#
# Usage:  OG_PORT=1447 scripts/slice12-conversation-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

echo "1. a coworker exists on the gateway account"
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=host@opengrok.local" | jq -r '.accessToken')
cw=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Chatter","model":"oag/cheap"}' | jq -r '.id')
[ -n "$cw" ] && [ "$cw" != "null" ] || fail "no coworker"
ok "hired $cw"

echo "2. the SSE stream is listening before the send"
SSE=$(mktemp)
curl -sN --max-time 20 "$BASE/events?channels=transcript,agent-upserted" > "$SSE" &
SSE_PID=$!
sleep 1

echo "3. sendPrompt answers accepted:true, immediately"
NONCE="nonce-$(date +%s)-$$"
send=$(curl -fsS --max-time 5 -X POST "$BASE/api/sendPrompt" -H 'content-type: application/json' \
  -d "{\"agentId\":\"$cw\",\"prompt\":\"hello from the smoke\",\"clientNonce\":\"$NONCE\"}")
echo "$send" | jq -e '.accepted == true' >/dev/null || fail "not accepted: $send"
ok "{accepted:true}"

echo "4. the same nonce again is accepted without a duplicate send"
again=$(curl -fsS --max-time 5 -X POST "$BASE/api/sendPrompt" -H 'content-type: application/json' \
  -d "{\"agentId\":\"$cw\",\"prompt\":\"hello from the smoke\",\"clientNonce\":\"$NONCE\"}")
echo "$again" | jq -e '.accepted == true' >/dev/null || fail "retry not accepted: $again"
ok "idempotent"

echo "5. the same nonce with DIFFERENT input is refused loudly"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/sendPrompt" -H 'content-type: application/json' \
  -d "{\"agentId\":\"$cw\",\"prompt\":\"a rewritten message\",\"clientNonce\":\"$NONCE\"}")
[ "$code" = "409" ] || fail "digest mismatch answered $code, expected 409"
ok "409 NONCE_DIGEST_MISMATCH"

echo "6. promptAcceptanceStatus finds the nonce"
status=$(curl -fsS -X POST "$BASE/api/promptAcceptanceStatus" -H 'content-type: application/json' \
  -d "{\"clientNonce\":\"$NONCE\"}")
echo "$status" | jq -e '.outcome == "found" and .record.clientNonce == "'"$NONCE"'"' >/dev/null \
  || fail "acceptance not found: $status"
missing=$(curl -fsS -X POST "$BASE/api/promptAcceptanceStatus" -H 'content-type: application/json' \
  -d '{"clientNonce":"never-sent"}')
echo "$missing" | jq -e '.outcome == "not-found"' >/dev/null || fail "phantom acceptance: $missing"
ok "found, and not-found for a stranger"

echo "7. the SSE stream carried the whole choreography"
# Wait for the turn's LAST frame — the roster pulse that says the coworker stopped running — not
# for the final transcript update. The pulse follows the update by a few milliseconds, and a
# capture stopped on the update lost the pulse on a slow CI runner (#15's CI run 33569452117).
for _ in $(seq 1 20); do
  grep -q '"channel":"agent-upserted".*"isRunning":false' "$SSE" 2>/dev/null && break
  sleep 1
done
kill $SSE_PID 2>/dev/null || true; wait $SSE_PID 2>/dev/null || true
frames=$(sed -n 's/^data: //p' "$SSE")
echo "$frames" | jq -c 'select(.channel=="transcript") | .payload | select(.type=="appended") | .entry | select(.kind=="message")' | grep -q "$NONCE" \
  || fail "the user echo did not carry the clientNonce"
ok "user message echoed with clientNonce"
echo "$frames" | jq -e -s '[.[] | select(.channel=="transcript") | .payload | select(.type=="appended") | .entry | select(.kind=="send-message" and .streaming==true)] | length >= 1' >/dev/null \
  || fail "no streaming placeholder appeared"
ok "streaming placeholder appended"
final=$(echo "$frames" | jq -c 'select(.channel=="transcript") | .payload | select(.type=="updated") | .entry' | tail -1)
echo "$final" | jq -e '.message.content | length > 0' >/dev/null || fail "the final update carries no content: $final"
echo "$final" | jq -e 'has("streaming") | not' >/dev/null || fail "the final update still says streaming: $final"
ok "final update with content, streaming over"
echo "$frames" | jq -e -s '[.[] | select(.channel=="agent-upserted") | .payload.agent.isRunning] | index(true) != null' >/dev/null \
  || fail "no isRunning:true pulse"
echo "$frames" | jq -e -s '[.[] | select(.channel=="agent-upserted") | .payload.agent.isRunning] | last == false' >/dev/null \
  || fail "the last pulse still says running"
ok "agent-upserted pulsed running -> not running"
echo "$frames" | jq -e -s '[.[] | select(.channel=="transcript") | .payload.ordered.sequence] | . == (. | sort) and length > 0' >/dev/null \
  || fail "transcript ordered sequences are not monotonic"
ok "ordered stamps are monotonic"

echo "8. the tail reads back, and a window carries threadCounts"
tail_reply=$(curl -fsS -X POST "$BASE/api/openAgentTail" -H 'content-type: application/json' \
  -d "{\"id\":\"$cw\",\"agentId\":\"$cw\",\"limit\":200}")
echo "$tail_reply" | jq -e '.entries | type == "array" and length >= 2' >/dev/null || fail "tail too short: $tail_reply"
echo "$tail_reply" | jq -e '[.entries[] | select(.kind=="message" and .role=="user")] | length >= 1' >/dev/null \
  || fail "the user message is not durable"
window=$(curl -fsS -X POST "$BASE/api/getAgentTranscriptWindow" -H 'content-type: application/json' \
  -d "{\"id\":\"$cw\",\"agentId\":\"$cw\",\"limit\":10}")
echo "$window" | jq -e 'has("threadCounts") and (.threadCounts | type == "object")' >/dev/null \
  || fail "the window has no threadCounts — the validated reply rejects: $window"
ok "durable tail, and threadCounts present"

echo "9. paging walks backwards"
one=$(curl -fsS -X POST "$BASE/api/getAgentTranscriptTail" -H 'content-type: application/json' \
  -d "{\"id\":\"$cw\",\"agentId\":\"$cw\",\"limit\":1}")
echo "$one" | jq -e '.entries | length == 1' >/dev/null || fail "limit ignored: $one"
next=$(echo "$one" | jq -r '.nextBeforeSeq // empty')
[ -n "$next" ] || fail "no nextBeforeSeq on a truncated tail"
older=$(curl -fsS -X POST "$BASE/api/getAgentTranscriptTail" -H 'content-type: application/json' \
  -d "{\"id\":\"$cw\",\"agentId\":\"$cw\",\"beforeSeq\":$next,\"limit\":200}")
echo "$older" | jq -e '.entries | length >= 1' >/dev/null || fail "the older page is empty"
ok "nextBeforeSeq pages into the past"

echo "10. the empty shapes hold"
thread=$(curl -fsS -X POST "$BASE/api/getAgentThread" -H 'content-type: application/json' -d "{\"id\":\"$cw\",\"rootId\":\"x\"}")
echo "$thread" | jq -e '.entries | type == "array"' >/dev/null || fail "getAgentThread malformed: $thread"
outline=$(curl -fsS -X POST "$BASE/api/getConversationOutline" )
echo "$outline" | jq -e 'type == "array"' >/dev/null || fail "outline not an array"
ok "thread and outline are well-formed empties"

rm -f "$SSE"
echo
echo "PASS — slice 12 smoke: a conversation holds, idempotently, with the client's own choreography."
