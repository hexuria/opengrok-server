#!/usr/bin/env bash
# Proves slice 9 of the port ladder: Seam B — the ConnectRPC backend, at the mock's minimum.
#
# Connect unary is POST /aiserver.v1.<Service>/<Method> with JSON. Every shape asserted here is
# transcribed from the client's own mock and generated types: proto3 JSON int64s as strings,
# enums by name, the default-empty-reply leniency that lets the app boot without all 46 methods,
# and EnsureSandBox — the mint — handing out OUR gateway's address and bearer.
#
# Usage:  OG_PORT=1466 OG_DATABASE_URL=… scripts/slice13-seamb-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
BIN="${OG_BIN:-./target/debug/opengrok}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
: "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"

GW_BEARER="seamb-smoke-bearer-$(date +%s)"
PUBLIC_URL="http://opengrok.lan:$PORT"

start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" \
  OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
  OG_MODEL_DOOR=mock OG_GATEWAY_BEARER="$GW_BEARER" OG_PUBLIC_GATEWAY_URL="$PUBLIC_URL" \
  RUST_LOG=warn "$BIN" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS --max-time 2 "$BASE/health" -H "authorization: Bearer $GW_BEARER" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server did not come up"
}
stop_server() { kill "${SERVER_PID:-0}" 2>/dev/null || true; wait "${SERVER_PID:-0}" 2>/dev/null || true; }
trap stop_server EXIT

rpc() { # rpc <service> <method> <token> [json]
  local body="${4-}"
  [ -n "$body" ] || body='{}'
  curl -sS --max-time 10 -X POST "$BASE/aiserver.v1.$1/$2" \
    -H "authorization: Bearer $3" -H 'content-type: application/json' -d "$body"
}

echo "1. the server is up and /auth/poll mints a token pair"
start_server
pair=$(curl -fsS "$BASE/auth/poll?email=seamb-$(date +%s)@og.local")
tok=$(echo "$pair" | jq -r '.accessToken')
[ -n "$tok" ] && [ "$tok" != "null" ] || fail "no accessToken: $pair"
echo "$pair" | jq -e '.refreshToken | length > 0' >/dev/null || fail "no refreshToken"
ok "accessToken + refreshToken, the mock's own shape"

echo "2. no bearer means unauthenticated, in Connect's vocabulary"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/aiserver.v1.DashboardService/GetMe" -d '{}')
[ "$code" = "401" ] || fail "expected 401, got $code"
body=$(curl -s -X POST "$BASE/aiserver.v1.DashboardService/GetMe" -d '{}')
echo "$body" | jq -e '.code == "unauthenticated"' >/dev/null || fail "wrong error code: $body"
ok "401 {code: unauthenticated}"

echo "3. DashboardService answers the boot set"
me=$(rpc DashboardService GetMe "$tok")
echo "$me" | jq -e '.authId | length > 0' >/dev/null || fail "GetMe has no authId: $me"
echo "$me" | jq -e '.email | contains("@")' >/dev/null || fail "GetMe has no email"
teams=$(rpc DashboardService GetTeams "$tok")
echo "$teams" | jq -e '.teams | type == "array" and length == 1' >/dev/null || fail "GetTeams: $teams"
echo "$teams" | jq -e '.teams[0].role == "TEAM_ROLE_OWNER"' >/dev/null || fail "enum not by name: $teams"
privacy=$(rpc DashboardService GetUserPrivacyMode "$tok")
echo "$privacy" | jq -e '.privacyMode == "PRIVACY_MODE_NO_TRAINING"' >/dev/null || fail "privacy: $privacy"
admin=$(rpc DashboardService GetTeamAdminSettingsOrEmptyIfNotInTeam "$tok")
echo "$admin" | jq -e '.localToolControls.permissionCeiling == "LOCAL_TOOL_PERMISSION_CEILING_ALWAYS"' >/dev/null \
  || fail "admin settings: $admin"
ok "GetMe, GetTeams, privacy, admin settings — enums by name"

echo "4. an unmodelled method answers an empty message — the leniency the boot leans on"
extra=$(rpc DashboardService GetUsageBasedPremiumRequests "$tok")
[ "$extra" = "{}" ] || fail "expected {}, got: $extra"
extra2=$(rpc GrokBotService ListGrokBotTemplates "$tok")
[ "$extra2" = "{}" ] || fail "expected {}, got: $extra2"
ok "{} for the methods the mock does not model either"

echo "5. an agent round-trips: create, list, update, delete"
created=$(rpc GrokBotService CreateGrokBotAgent "$tok" \
  '{"name":"Backhand","description":"seam B born","title":"Analyst","avatarShape":"squircle","avatarColor":"teal"}')
aid=$(echo "$created" | jq -r '.agent.id')
[ -n "$aid" ] && [ "$aid" != "null" ] || fail "no agent id: $created"
echo "$created" | jq -e '.harness == "GROK_BOT_AGENT_HARNESS_KIND_BOX"' >/dev/null || fail "harness enum: $created"
echo "$created" | jq -e '.agent.createdAtMs | type == "string"' >/dev/null || fail "int64 not a string: $created"
listed=$(rpc GrokBotService ListGrokBotAgents "$tok")
echo "$listed" | jq -e --arg id "$aid" '.agents | any(.[]; .id == $id and .description == "seam B born" and .title == "Analyst")' >/dev/null \
  || fail "the profile did not round-trip: $listed"
updated=$(rpc GrokBotService UpdateGrokBotAgent "$tok" "{\"id\":\"$aid\",\"name\":\"Backhand II\",\"description\":\"renamed\"}")
echo "$updated" | jq -e '.agent.name == "Backhand II" and .agent.description == "renamed"' >/dev/null \
  || fail "update did not round-trip: $updated"
ok "create → list → update, profile intact, int64s as strings"

echo "6. a send is dispatched, idempotently, and status is queryable"
MSG="msg-$(date +%s)"
sent=$(rpc GrokBotService SendGrokBotUserMessage "$tok" \
  "{\"agentId\":\"$aid\",\"messageId\":\"$MSG\",\"text\":\"hello seam B\",\"sentAtMs\":\"$(date +%s000)\"}")
echo "$sent" | jq -e '.dispatched == true and .delivery == "GROK_BOT_USER_MESSAGE_DELIVERY_ACCEPTED_BOX"' >/dev/null \
  || fail "send not dispatched: $sent"
again=$(rpc GrokBotService SendGrokBotUserMessage "$tok" \
  "{\"agentId\":\"$aid\",\"messageId\":\"$MSG\",\"text\":\"hello seam B\",\"sentAtMs\":\"$(date +%s000)\"}")
echo "$again" | jq -e '.dispatched == true' >/dev/null || fail "retry refused: $again"
status=$(rpc GrokBotService GetGrokBotSendStatus "$tok" "{\"agentId\":\"$aid\",\"messageId\":\"$MSG\"}")
echo "$status" | jq -e '.status == "GROK_BOT_SEND_STATUS_ACCEPTED" and .echoEntryId == "'"$MSG"'"' >/dev/null \
  || fail "send status: $status"
none=$(rpc GrokBotService GetGrokBotSendStatus "$tok" "{\"agentId\":\"$aid\",\"messageId\":\"never\"}")
echo "$none" | jq -e '.status == "GROK_BOT_SEND_STATUS_NOT_FOUND"' >/dev/null || fail "phantom status: $none"
ok "ACCEPTED_BOX, retry-safe, status found and not-found"

echo "7. the transcript lists with base64 bodies and string seqs — and the turn answered"
# The placeholder exists before the turn ends; wait for it to GAIN its content.
for _ in $(seq 1 15); do
  entries=$(rpc GrokBotService ListGrokBotTranscriptEntries "$tok" "{\"agentId\":\"$aid\",\"limit\":50}")
  done_yet=$(echo "$entries" | jq -r '[.entries[] | select(.entryKind == "send-message") | .body] | last // empty' | base64 -d 2>/dev/null | jq -r '.message.content // "" | length' 2>/dev/null || echo 0)
  [ "${done_yet:-0}" -gt 0 ] && break
  sleep 1
done
echo "$entries" | jq -e '.generation == 1' >/dev/null || fail "no generation: $entries"
echo "$entries" | jq -e '.entries[0].seq | type == "string"' >/dev/null || fail "seq not a string"
decoded=$(echo "$entries" | jq -r '.entries[0].body' | base64 -d)
echo "$decoded" | jq -e '.kind == "message" and .content == "hello seam B"' >/dev/null \
  || fail "the body does not decode to the user's message: $decoded"
# The idempotent retry must not have produced a second user message.
users=$(echo "$entries" | jq '[.entries[] | select(.entryKind == "message")] | length')
answer=$(echo "$entries" | jq -r '[.entries[] | select(.entryKind == "send-message")][0].body' | base64 -d)
echo "$answer" | jq -e '.message.content | length > 0' >/dev/null || fail "no answer content: $answer"
[ "$users" = "1" ] || fail "the retried send duplicated the user message ($users)"
ok "one user message, one answered turn, bodies decode"

echo "8. commit accepts entries the client already shaped"
BODY=$(printf '{"kind":"notice","id":"n1","text":"committed from the client","timestampMs":1}' | base64)
committed=$(rpc GrokBotService CommitGrokBotTranscriptEntries "$tok" \
  "{\"agentId\":\"$aid\",\"generation\":1,\"entries\":[{\"seq\":\"0\",\"entryKind\":\"notice\",\"body\":\"$BODY\"}]}")
echo "$committed" | jq -e '.committedCount == 1' >/dev/null || fail "commit: $committed"
ok "committedCount: 1"

echo "9. EnsureSandBox mints OUR gateway"
mint=$(rpc GrokBotService EnsureSandBox "$tok")
echo "$mint" | jq -e '.gatewayUrl == "'"$PUBLIC_URL"'"' >/dev/null || fail "wrong gatewayUrl: $mint"
echo "$mint" | jq -e '.gatewayToken == "'"$GW_BEARER"'"' >/dev/null || fail "wrong gatewayToken"
case "$(echo "$mint" | jq -r '.gatewayUrl')" in
  *127.0.0.1*|*localhost*) fail "the mint handed out a loopback gateway — the client refuses it" ;;
esac
ok "non-loopback gatewayUrl + the gateway bearer: the seams meet"

echo "10. delete retires, and the roster forgets"
rpc GrokBotService DeleteGrokBotAgent "$tok" "{\"id\":\"$aid\"}" >/dev/null
after=$(rpc GrokBotService ListGrokBotAgents "$tok")
echo "$after" | jq -e --arg id "$aid" '.agents | any(.[]; .id == $id) | not' >/dev/null \
  || fail "the deleted agent is still listed"
ok "gone from the list, history kept in the log"

echo
echo "PASS — slice 13 smoke: Seam B speaks Connect, at the mock's minimum, and the mint points home."
