#!/usr/bin/env bash
# Door 1's tool half, end to end: an MCP client (a hand-written one — curl) presents a bot key
# at /mcp, sees exactly the coworker's toolbox, and a tools/call lands on that coworker's OWN
# computer. Also the two refusals that make the door safe: a tool outside the grant is refused
# with the reason, and an identity argument in the request is overwritten, not honoured.
#
# And the third answer a call can get, which is neither yes nor no but "ask somebody": a tool the
# policy says needs a human yes raises a real card, the PERSON answers it in OpenGrok (the MCP
# client cannot answer its own card), and the client's retry is let through.
#
# Needs a Docker daemon. Skips rather than fails without one.
#
# Usage:  OG_PORT=1447 scripts/slice20-mcp-door-smoke.sh
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

# One JSON-RPC call to the door. No MCP SDK on purpose: the door's own tests already use rmcp's
# types on the server side, and a second SDK here would only prove a shared interpretation.
mcp() { # $1 bearer, $2 id, $3 method, $4 params-json
  curl -fsS -X POST "$BASE/mcp" \
    -H "authorization: Bearer $1" \
    -H 'accept: application/json, text/event-stream' \
    -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":$2,\"method\":\"$3\",\"params\":$4}"
}

require_shell() { # $1 tools/list json — the box tools must be present
  echo "$1" | jq -e '.result.tools | map(.name) | index("shell")' >/dev/null
}

echo "1. hire a coworker with a computer, and mint its bot key"
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=mcp-$(date +%s)@og.local" | jq -r '.accessToken')
[ -n "$token" ] || fail "no access token"
hired=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Doorman","with_computer":true}')
coworker=$(echo "$hired" | jq -r '.id')
BOX_ID=$(echo "$hired" | jq -r '.boxId')
[ "$BOX_ID" != "null" ] && [ -n "$BOX_ID" ] || fail "no computer was assigned: $hired"
key=$(curl -fsS -X POST "$BASE/coworkers/$coworker/keys" -H "authorization: Bearer $token" | jq -r '.key')
[ -n "$key" ] && [ "$key" != "null" ] || fail "no bot key"
ok "coworker $coworker on box $BOX_ID, key minted"

echo "2. the handshake answers, and the toolbox is the coworker's own"
init=$(mcp "$key" 1 initialize '{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}')
echo "$init" | jq -e '.result.capabilities.tools' >/dev/null || fail "no tools capability: $init"
tools=$(mcp "$key" 2 tools/list '{}')
echo "$tools" | jq -e '.result.tools | map(.name) | index("shell")' >/dev/null \
  || fail "shell is not offered: $tools"
ok "initialize + tools/list offer the box tools"

echo "3. a call runs on the coworker's own computer, with a positive success assertion"
marker="/tmp/mcp-door-ran-$(date +%s)"
# `.result.content` present AND isError not set: a vacuous check (`isError != true` on a
# JSON-RPC error, which has no .result) would pass on a refusal too, so assert the shape that
# only a real success produces, then confirm the side effect on the box.
call=$(mcp "$key" 3 tools/call "{\"name\":\"shell\",\"arguments\":{\"command\":\"echo through-the-door > $marker\"}}")
echo "$call" | jq -e '.result.content[0].text != null and (.result.isError // false) == false' >/dev/null \
  || fail "the call did not succeed cleanly: $call"
docker exec "$BOX_ID" cat "$marker" | grep -q "through-the-door" \
  || fail "the marker is not on the coworker's box"
ok "the command ran on $BOX_ID"

echo "4. a foreign identity argument is overwritten, not honoured (the slice-7 attack via MCP)"
marker2="/tmp/mcp-door-identity-$(date +%s)"
# Both spellings of a foreign coworker id, alongside a real command. overwrite_identity strips
# every alias before dispatch, and dispatch binds the box from the SESSION — so the marker lands
# on OUR box no matter what the arguments claimed.
attack=$(mcp "$key" 4 tools/call "{\"name\":\"shell\",\"arguments\":{\"command\":\"echo owned > $marker2\",\"coworkerId\":\"cw_somebody_else\",\"coworker_id\":\"cw_somebody_else\",\"boxId\":\"box_elsewhere\"}}")
echo "$attack" | jq -e '(.result.isError // false) == false' >/dev/null || fail "the call was refused: $attack"
docker exec "$BOX_ID" cat "$marker2" | grep -q "owned" \
  || fail "the identity-injection marker is not on the coworker's own box"
ok "the foreign coworkerId/boxId were ignored; the command ran on $BOX_ID"

echo "5. a tool outside the grant is refused, NAMING the rule"
denied=$(mcp "$key" 5 tools/call '{"name":"gmail.workspace.send","arguments":{}}')
echo "$denied" | jq -e '.result.isError == true' >/dev/null || fail "an ungranted tool did not refuse: $denied"
text=$(echo "$denied" | jq -r '.result.content[0].text')
case "$text" in
  *"may never run"*|*"unknown tool"*) ok "refused naming the rule: $text" ;;
  *) fail "the refusal does not name the rule: $text" ;;
esac

echo "6. reverse-exec is not carried over MCP"
rx=$(mcp "$key" 6 tools/call '{"name":"user_machine_shell","arguments":{"command":"echo hi"}}')
echo "$rx" | jq -e '.result.isError == true and (.result.content[0].text | test("not available over MCP"))' >/dev/null \
  || fail "user_machine_shell was not refused over MCP: $rx"
# And it is absent from the listing.
mcp "$key" 7 tools/list '{}' | jq -e '.result.tools | map(.name) | index("user_machine_shell") | not' >/dev/null \
  || fail "user_machine_shell appears in the MCP toolbox"
ok "the reverse-exec channel is neither listed nor callable over MCP"

echo "7. an account token is a person, not a coworker — 401 at the edge"
# The guard refuses before rmcp, so this is a real HTTP 401, not a JSON-RPC error.
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/mcp" \
  -H "authorization: Bearer $token" -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":8,"method":"tools/list","params":{}}')
[ "$code" = "401" ] || fail "a person's token was not refused with 401 (got $code)"
ok "a person's access token is refused 401 with mint guidance"

echo "8. a call that needs a person's yes is held, and the MCP client is given a requestId"
# The grant stays; `shell` is now marked as needing a human yes. That is the POLICY's ask — the
# same card the auto-review judge raises, carrying the grant's reason instead of the judge's.
marked=$(curl -fsS -X POST "$BASE/coworkers/$coworker/approvals" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"tools":["shell"]}')
echo "$marked" | jq -e '.needsApproval | index("shell")' >/dev/null || fail "shell was not marked: $marked"

marker3="/tmp/mcp-door-after-the-yes-$(date +%s)"
args="{\"command\":\"echo after-the-yes > $marker3\"}"
asked=$(mcp "$key" 9 tools/call "{\"name\":\"shell\",\"arguments\":$args}")
echo "$asked" | jq -e '.result.isError == true' >/dev/null || fail "a call needing a person ran anyway: $asked"
text=$(echo "$asked" | jq -r '.result.content[0].text')
request_id=$(printf '%s' "$text" | sed -n 's/.*requestId: \([^)]*\)).*/\1/p')
[ -n "$request_id" ] || fail "the client was not told where to answer: $text"
# The waiting call must not have run. A pending approval that already happened is the one failure
# mode this whole layer exists to prevent.
docker exec "$BOX_ID" sh -c "[ ! -e $marker3 ]" || fail "the command ran before anybody said yes"
ok "held on requestId $request_id, and nothing ran"

echo "9. the card is in the person's queue, and says WHICH question is being asked"
queue=$(curl -fsS "$BASE/ag-ui/approvals" -H "authorization: Bearer $token")
entry=$(echo "$queue" | jq -c --arg c "$request_id" '.[] | select(.callId == $c)')
[ -n "$entry" ] || fail "the MCP card is not in the approvals queue: $queue"
echo "$entry" | jq -e '.reason == "policy-approval"' >/dev/null \
  || fail "the queue does not name the ask: $entry"
echo "$entry" | jq -e '.why | type == "string" and length > 0' >/dev/null \
  || fail "the queue does not say why: $entry"
echo "$entry" | jq -e '.threadId | startswith("mcp-")' >/dev/null \
  || fail "an MCP card is not on a conversation thread: $entry"
run_id=$(echo "$entry" | jq -r '.runId')
ok "queued as $(echo "$entry" | jq -r '.reason'), because: $(echo "$entry" | jq -r '.why' | cut -c1-60)"

echo "10. the person says yes, and the run FINISHES rather than resuming as a turn"
# An MCP run is one call's audit row, not a conversation. Resuming it would run the tool here
# while the client is being told to retry — the same call, twice.
answered=$(curl -fsS -X POST "$BASE/ag-ui/runs/$run_id/answer" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d "{\"call_id\":\"$request_id\",\"approved\":true}")
echo "$answered" | jq -e '.finished == true and .continuing == false' >/dev/null \
  || fail "the MCP run was not settled on its card: $answered"
state=$(curl -fsS "$BASE/ag-ui/runs/$run_id" -H "authorization: Bearer $token" | jq -r '.status')
[ "$state" = "finished" ] || fail "the answered MCP run is $state, not finished"
ok "answered, and its run is finished"

echo "11. the retry spends the yes, and the approved command runs"
retry=$(mcp "$key" 10 tools/call "{\"name\":\"shell\",\"arguments\":$args}")
echo "$retry" | jq -e '.result.content[0].text != null and (.result.isError // false) == false' >/dev/null \
  || fail "the retry after the yes was not let through: $retry"
docker exec "$BOX_ID" cat "$marker3" | grep -q "after-the-yes" \
  || fail "the approved command never ran on the coworker's box"
ok "the approved call ran, after the yes and not before"

echo
echo "SLICE 20 SMOKE PASSED — the MCP door serves the coworker's toolbox, and only that"
