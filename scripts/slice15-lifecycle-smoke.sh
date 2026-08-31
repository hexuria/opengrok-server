#!/usr/bin/env bash
# Proves slice 11: the agent lifecycle tier (P5), entry mutation (P6), and automations-as-schedules
# (P9) — the breadth below the conversation milestone, each answer in the container type the
# renderer validates.
#
# Usage:  OG_PORT=1447 scripts/slice15-lifecycle-smoke.sh   (against the gate's shared mock server)
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }
command -v jq >/dev/null || fail "jq is required"
api() { curl -sS --max-time 10 -X POST "$BASE/api/$1" -H 'content-type: application/json' -d "${2:-}"; }

echo "1. createAgent hires, and the clientNonce dedupes"
NONCE="ca-$(date +%s)-$$"
made=$(api createAgent "{\"name\":\"Lifecycle\",\"description\":\"born in P5\",\"clientNonce\":\"$NONCE\"}")
aid=$(echo "$made" | jq -r '.agent.id')
[ -n "$aid" ] && [ "$aid" != "null" ] || fail "no agent: $made"
echo "$made" | jq -e '.transcript | type == "array"' >/dev/null || fail "no transcript array"
again=$(api createAgent "{\"name\":\"Lifecycle\",\"description\":\"born in P5\",\"clientNonce\":\"$NONCE\"}")
aid2=$(echo "$again" | jq -r '.agent.id')
[ "$aid2" = "$aid" ] || fail "the retried create made a twin ($aid vs $aid2)"
ok "one coworker from two creates"

echo "2. updateAgent round-trips, and a stranger's id answers null"
up=$(api updateAgent "{\"id\":\"$aid\",\"profile\":{\"name\":\"Lifecycle II\",\"description\":\"renamed\"}}")
echo "$up" | jq -e '.name == "Lifecycle II" and .description == "renamed"' >/dev/null || fail "update: $up"
nul=$(api updateAgent '{"id":"cw_nobody"}')
[ "$nul" = "null" ] || fail "a stranger's update answered: $nul"
ok "summary back, null for a stranger"

echo "3. avatars: bytes in, dataUrl out, and /avatars serves it"
PNG="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
setav=$(api setAgentAvatarBytes "{\"id\":\"$aid\",\"pngBase64\":\"$PNG\"}")
echo "$setav" | jq -e '.avatarVersion != null' >/dev/null || fail "no avatarVersion: $setav"
got=$(api getAgentAvatar "{\"id\":\"$aid\"}")
echo "$got" | jq -e '.dataUrl | startswith("data:image/png;base64,")' >/dev/null || fail "avatar: $got"
ok "avatar round-trips with a version"

echo "4. searchAgents filters; searchMedia stays an array"
hits=$(api searchAgents '{"query":"lifecycle"}')
echo "$hits" | jq -e --arg id "$aid" 'any(.[]; .id == $id)' >/dev/null || fail "search missed it"
none=$(api searchAgents '{"query":"zzz-not-a-name"}')
echo "$none" | jq -e 'length == 0' >/dev/null || fail "phantom hit"
api searchMedia '{"query":"x"}' | jq -e 'type == "array"' >/dev/null || fail "searchMedia container"
ok "search finds by name, media is an empty array"

echo "5. duplicateAgent makes a copy with the profile"
dup=$(api duplicateAgent "{\"id\":\"$aid\"}")
echo "$dup" | jq -e '.agent.name == "Lifecycle II copy"' >/dev/null || fail "duplicate: $dup"
did=$(echo "$dup" | jq -r '.agent.id')
ok "a copy exists ($did)"

echo "6. reactions and deletion mutate entries and tell the stream"
SSE=$(mktemp); curl -sN --max-time 15 "$BASE/events?channels=transcript" > "$SSE" & SSE_PID=$!
sleep 1
PN="pn-$(date +%s)"
curl -sS --max-time 5 -X POST "$BASE/api/sendPrompt" -H 'content-type: application/json' \
  -d "{\"agentId\":\"$aid\",\"prompt\":\"react to me\",\"clientNonce\":\"$PN\"}" >/dev/null
sleep 2
eid=$(api getAgentTranscriptTail "{\"agentId\":\"$aid\",\"limit\":10}" | jq -r '[.entries[] | select(.kind=="message")][0].id')
[ -n "$eid" ] && [ "$eid" != "null" ] || fail "no message entry to react to"
reacted=$(api reactToMessage "{\"agentId\":\"$aid\",\"entryId\":\"$eid\",\"emoji\":\"🔥\"}")
echo "$reacted" | jq -e '.reactions | any(.[]; .emoji == "🔥")' >/dev/null || fail "reaction: $reacted"
deleted=$(api deleteTranscriptEntries "{\"agentId\":\"$aid\",\"ids\":[\"$eid\"]}")
echo "$deleted" | jq -e '.deleted == 1' >/dev/null || fail "delete: $deleted"
sleep 1; kill $SSE_PID 2>/dev/null || true; wait $SSE_PID 2>/dev/null || true
frames=$(sed -n 's/^data: //p' "$SSE"); rm -f "$SSE"
echo "$frames" | jq -e -s '[.[] | select(.channel=="transcript") | .payload | select(.type=="updated") | .entry.reactions] | any(. != null)' >/dev/null \
  || fail "no updated frame carried the reaction"
echo "$frames" | jq -e -s --arg id "$eid" '[.[] | select(.channel=="transcript") | .payload | select(.type=="removed")] | any(.[]; .id == $id)' >/dev/null \
  || fail "no removed frame for the deleted entry"
ok "updated and removed frames both told the stream"

echo "7. automations are schedules wearing client names"
created=$(api createAgentAutomation "{\"agentId\":\"$aid\",\"cron\":\"0 9 * * 1\",\"instruction\":\"weekly report\"}")
echo "$created" | jq -e 'type == "array" and length >= 1' >/dev/null || fail "create returned: $created"
auto_id=$(echo "$created" | jq -r '.[0].id')
echo "$created" | jq -e '.[0].enabled == true and .[0].instruction == "weekly report"' >/dev/null || fail "shape: $created"
listed=$(api getAgentAutomations "{\"id\":\"$aid\"}")
echo "$listed" | jq -e --arg id "$auto_id" 'any(.[]; .id == $id)' >/dev/null || fail "not listed"
disabled=$(api setAgentAutomationEnabled "{\"id\":\"$auto_id\",\"enabled\":false}")
echo "$disabled" | jq -e --arg id "$auto_id" '[.[] | select(.id == $id)][0].enabled == false' >/dev/null \
  || fail "disable did not stick: $disabled"
schedules_see_it=$(curl -sS "$BASE/schedules" -H "authorization: Bearer $(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=host@opengrok.local" | jq -r '.accessToken')")
echo "$schedules_see_it" | jq -e --arg id "$auto_id" 'any(.[]; .id == $id and .active == false)' >/dev/null \
  || fail "the /schedules view disagrees — two vocabularies, two truths: $schedules_see_it"
api deleteAgentAutomation "{\"id\":\"$auto_id\"}" >/dev/null
after=$(api listAllAutomations '{}')
echo "$after" | jq -e --arg id "$auto_id" 'any(.[]; .id == $id) | not' >/dev/null || fail "delete did not stick"
ok "create, list, disable (visible under /schedules too), delete — one scheduler"

echo "8. the honest empties hold their container types"
api getSharingState '{}' | jq -e 'type == "object" and (.rooms | type == "array")' >/dev/null || fail "sharing"
api getAgentChannels "{\"id\":\"$aid\"}" | jq -e '.channels | type == "array"' >/dev/null || fail "channels"
api listBoxMcpServers '{}' | jq -e '.servers | type == "array"' >/dev/null || fail "mcp servers"
api getSubagents "{\"id\":\"$aid\"}" | jq -e 'type == "array"' >/dev/null || fail "subagents"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/createGroup" -H 'content-type: application/json' -d '{"name":"g"}')
[ "$code" = "400" ] || fail "groups answered $code, expected a readable 400"
ok "records and arrays where validated, a readable refusal for groups"

echo "9. deleteAgents retires both test coworkers"
gone=$(api deleteAgents "{\"ids\":[\"$aid\",\"$did\"]}")
echo "$gone" | jq -e '.deleted == 2' >/dev/null || fail "delete: $gone"
still=$(api listAgents)
echo "$still" | jq -e --arg id "$aid" 'any(.[]; .id == $id) | not' >/dev/null || fail "still listed"
ok "retired and off the roster"

echo "10. /avatars serves the bytes a slim roster points at"
made2=$(api createAgent '{"name":"Avatarian"}')
avid=$(echo "$made2" | jq -r '.agent.id')
api setAgentAvatarBytes "{\"id\":\"$avid\",\"pngBase64\":\"$PNG\"}" >/dev/null
ctype=$(curl -s -o /dev/null -w '%{content_type}' "$BASE/avatars/$avid")
case "$ctype" in image/png*) ;; *) fail "avatar content-type was $ctype" ;; esac
size=$(curl -s "$BASE/avatars/$avid" | wc -c | tr -d ' ')
[ "$size" -gt 30 ] || fail "avatar bytes too small: $size"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/avatars/cw_nobody")
[ "$code" = "404" ] || fail "a missing avatar answered $code"
slim=$(curl -sS --max-time 5 -X POST "$BASE/api/listAgents" -H 'x-sand-slim-avatars: 1')
echo "$slim" | jq -e --arg id "$avid" '[.[] | select(.id == $id)][0] | .avatarDataUrl == null and .avatarVersion != null' >/dev/null \
  || fail "slim mode must null the dataUrl and keep the version"
api deleteAgents "{\"ids\":[\"$avid\"]}" >/dev/null

echo "11. the skills catalogue is the plugin catalogue; attachments refuse readably"
api skillsCatalog '{}' | jq -e 'type == "array"' >/dev/null || fail "skillsCatalog container"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/api/uploadAttachment" -H 'content-type: application/json' -d '{}')
[ "$code" = "400" ] || fail "uploadAttachment answered $code, expected a readable 400"
box=$(api getBoxStoreStatus '{}')
[ "$box" = "null" ] || fail "box store status should be null with no store: $box"
ok "P7/P8/P10 surfaces hold"

echo
echo "PASS — slice 15 smoke: the lifecycle tier answers, honestly, in the shapes the renderer checks."
