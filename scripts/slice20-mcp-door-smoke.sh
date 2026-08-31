#!/usr/bin/env bash
# Door 1's tool half, end to end: an MCP client (a hand-written one — curl) presents a bot key
# at /mcp, sees exactly the coworker's toolbox, and a tools/call lands on that coworker's OWN
# computer. Also the two refusals that make the door safe: a tool outside the grant is refused
# with the reason, and an identity argument in the request is overwritten, not honoured.
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

echo "3. a call runs on the coworker's own computer — and an identity argument is overwritten"
marker="/tmp/mcp-door-ran-$(date +%s)"
# The foreign coworkerId in the arguments is the slice-7 attack, replayed through MCP: the
# executor must overwrite it from the session, so the marker lands on OUR box regardless.
call=$(mcp "$key" 3 tools/call "{\"name\":\"shell\",\"arguments\":{\"command\":\"echo through-the-door > $marker\",\"coworkerId\":\"cw_somebody_else\"}}")
echo "$call" | jq -e '.result.isError != true' >/dev/null || fail "the call was refused: $call"
docker exec "$BOX_ID" cat "$marker" | grep -q "through-the-door" \
  || fail "the marker is not on the coworker's box"
ok "the command ran on $BOX_ID, identity argument ignored"

echo "4. a tool outside the grant is refused with the reason, never silently dropped"
denied=$(mcp "$key" 4 tools/call '{"name":"gmail.workspace.send","arguments":{}}')
echo "$denied" | jq -e '.result.isError == true' >/dev/null || fail "an ungranted tool did not refuse: $denied"
text=$(echo "$denied" | jq -r '.result.content[0].text')
case "$text" in
  *refused*|*unknown*) ok "refused with a reason: $text" ;;
  *) fail "the refusal does not say why: $text" ;;
esac

echo "5. an account token is a person, not a coworker"
person=$(mcp "$token" 5 tools/list '{}' || true)
echo "$person" | jq -e '.error.message | test("bot key")' >/dev/null \
  || fail "a person's token was not pointed at the mint: $person"
ok "a person is told to mint a bot key"

echo
echo "SLICE 20 SMOKE PASSED — the MCP door serves the coworker's toolbox, and only that"
