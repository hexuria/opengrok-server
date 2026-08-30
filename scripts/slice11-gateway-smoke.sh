#!/usr/bin/env bash
# Proves slice 7 of the port ladder: the gateway wire contract the desktop client boots against.
#
# Every assertion is a rule from client-grok-bot.md §2.0/§9 that fails SILENTLY in the real app —
# a missing `retry:` line or a wrong container type is a reconnect loop or a renderer throw, not
# an error message. So the smoke checks the wire, and the live client checks the experience.
#
# Usage:  OG_PORT=1447 scripts/slice11-gateway-smoke.sh   (against the gate's shared server)
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

echo "1. /health answers the supervisor's shape"
health=$(curl -fsS --max-time 2 "$BASE/health")
echo "$health" | jq -e '.ok == true' >/dev/null || fail "ok !== true: $health"
echo "$health" | jq -e '.isBusy == false or .isBusy == true' >/dev/null || fail "no isBusy: $health"
echo "$health" | jq -e 'has("activeAgentId") and has("startedAt") and has("lastBusyAtMs")' >/dev/null \
  || fail "missing supervisor fields: $health"
ok "ok, pid, isBusy, activeAgentId, startedAt, lastBusyAtMs"

echo "2. /events opens as an event stream with the mandatory framing"
frames=$(curl -sN --max-time 2 "$BASE/events?channels=agents,transcript" -H 'accept: text/event-stream' || true)
case "$frames" in
  "retry: 1000"*) ok "retry: 1000 is the first line" ;;
  *) fail "the stream did not open with retry: 1000: $(echo "$frames" | head -2)" ;;
esac
ctype=$(curl -sN -o /dev/null --max-time 2 -w '%{content_type}' "$BASE/events" || true)
case "$ctype" in
  text/event-stream*) ok "content-type is text/event-stream" ;;
  *) fail "content-type was $ctype" ;;
esac

echo "3. the heartbeat arrives well inside the 35 s watchdog"
pinged=$(curl -sN --max-time 12 "$BASE/events" || true)
echo "$pinged" | grep -q ":ping" || fail "no :ping within 12s — the client's watchdog would abort"
ok ":ping within 12 s"

echo "4. commands answer with the reply mechanics the client parses"
resp_headers=$(curl -fsS -D - -o /dev/null -X POST "$BASE/api/getTrays" -H 'content-type: application/json')
echo "$resp_headers" | grep -qi "x-sand-mint-dedupe: 1" || fail "no x-sand-mint-dedupe header"
echo "$resp_headers" | grep -qi "content-type: application/json" || fail "not application/json"
ok "x-sand-mint-dedupe: 1 on a JSON reply"

echo "5. container-type discipline — the renderer throws on these"
trays=$(curl -fsS -X POST "$BASE/api/getTrays")
echo "$trays" | jq -e 'type == "array"' >/dev/null || fail "getTrays is not an array: $trays"
agents=$(curl -fsS -X POST "$BASE/api/listAgents")
echo "$agents" | jq -e 'type == "array"' >/dev/null || fail "listAgents is not an array: $agents"
count=$(curl -fsS -X POST "$BASE/api/countAgents")
echo "$count" | jq -e 'type == "number"' >/dev/null || fail "countAgents is not a number: $count"
box=$(curl -fsS -X POST "$BASE/api/getForeverBoxStatus")
[ "$box" = "null" ] || fail "getForeverBoxStatus should be null with no box: $box"
net=$(curl -fsS -X POST "$BASE/api/isAgentNetworkEnabled")
echo "$net" | jq -e 'type == "boolean"' >/dev/null || fail "isAgentNetworkEnabled is not a boolean"
ok "array, array, number, null, boolean"

echo "6. an empty body parses as {} — the client sends none for no-arg commands"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/countAgents")
[ "$code" = "200" ] || fail "an empty body answered $code"
ok "no body, still 200"

echo "7. setHostSettings echoes the FULL record back — the resync chain reads it"
echoed=$(curl -fsS -X POST "$BASE/api/setHostSettings" -H 'content-type: application/json' \
  -d '{"userTimeZone":"Asia/Manila"}')
echo "$echoed" | jq -e '.userTimeZone == "Asia/Manila"' >/dev/null || fail "the set value did not echo"
echo "$echoed" | jq -e 'has("notifications") and has("mcpCustomInstructions") and has("sidebarSections")' >/dev/null \
  || fail "the echo is not the whole record: $echoed"
after=$(curl -fsS -X POST "$BASE/api/getHostSettings")
echo "$after" | jq -e '.userTimeZone == "Asia/Manila"' >/dev/null || fail "the set did not stick"
ok "merged, echoed whole, and readable back"

echo "8. an unknown method is a 404 with the shipped wording"
missing=$(curl -s -w '\n%{http_code}' -X POST "$BASE/api/noSuchThing")
code=$(echo "$missing" | tail -1)
[ "$code" = "404" ] || fail "unknown method answered $code"
echo "$missing" | head -1 | jq -e '.error | test("unknown gateway method")' >/dev/null \
  || fail "wrong error wording: $(echo "$missing" | head -1)"
ok "404, unknown gateway method: noSuchThing"

echo "9. a browser Origin is refused everywhere"
for path in /health /events /api/listAgents; do
  method=$([ "$path" = "/api/listAgents" ] && echo POST || echo GET)
  code=$(curl -s -o /dev/null -w '%{http_code}' -X "$method" "$BASE$path" -H 'origin: https://evil.example')
  [ "$code" = "403" ] || fail "$path with an Origin answered $code"
done
ok "403 on /health, /events, and /api with an Origin header"

echo "10. a hired coworker appears as a well-formed roster row"
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=host@opengrok.local" | jq -r '.accessToken')
curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Gateway Smoke","model":"oag/cheap"}' >/dev/null
rows=$(curl -fsS -X POST "$BASE/api/listAgents")
echo "$rows" | jq -e 'any(.[]; .name == "Gateway Smoke")' >/dev/null || fail "the coworker is not on the roster"
row=$(echo "$rows" | jq '[.[] | select(.name == "Gateway Smoke")][0]')
echo "$row" | jq -e '.updatedAt != null and (.memberIds | type == "array") and (.hasUnread | type == "boolean")' \
  >/dev/null || fail "malformed row: $row"
count=$(curl -fsS -X POST "$BASE/api/countAgents")
[ "$count" -ge 1 ] || fail "countAgents did not count it"
ok "a §8.1 row, and countAgents agrees"

echo
echo "PASS — slice 11 smoke: the gateway speaks the desktop client's wire contract."
