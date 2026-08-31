#!/usr/bin/env bash
# Proves slice 10: a bot key binds a client Bot to a coworker, durably and revocably.
#
# The gap this closes: runs from a client Bot arrived anonymous — no tools, no policy, the
# deployment's model — because access tokens die hourly and a Bot's vault holds one static
# header. The key is that header: it names the account AND the coworker, so a bare POST /ag-ui
# with nothing but the key runs as the coworker, on the coworker's own model.
#
# Usage:  OG_PORT=1447 scripts/slice14-botkey-smoke.sh   (against the gate's shared mock server)
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }
command -v jq >/dev/null || fail "jq is required"

echo "1. sign in, hire a coworker on a model of its own"
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=botkey-$(date +%s)@og.local" | jq -r '.accessToken')
cw=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Bound","model":"xai/grok-4.6"}' | jq -r '.id')
[ -n "$cw" ] && [ "$cw" != "null" ] || fail "no coworker"
ok "hired $cw on xai/grok-4.6"

echo "2. mint a key — shown once, recorded revocable"
minted=$(curl -fsS -X POST "$BASE/coworkers/$cw/keys" -H "authorization: Bearer $token")
key=$(echo "$minted" | jq -r '.key')
jti=$(echo "$minted" | jq -r '.jti')
[ -n "$key" ] && [ "$key" != "null" ] || fail "no key: $minted"
listed=$(curl -fsS "$BASE/coworkers/$cw/keys" -H "authorization: Bearer $token")
echo "$listed" | jq -e --arg jti "$jti" 'any(.[]; .jti == $jti and .revoked == false)' >/dev/null \
  || fail "the key is not listed live: $listed"
echo "$listed" | jq -e 'any(.[]; has("key")) | not' >/dev/null || fail "the token leaked into the list"
ok "minted, listed, and the token itself appears nowhere but the mint reply"

echo "3. a stranger cannot mint for somebody else's coworker"
other=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=other-bk-$(date +%s)@og.local" | jq -r '.accessToken')
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/coworkers/$cw/keys" -H "authorization: Bearer $other")
[ "$code" = "404" ] || fail "a stranger got $code, expected 404"
ok "404 — existence not confirmed"

echo "4. a bare run with only the key arrives AS the coworker"
T=$(date +%s)
frames=$(curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $key" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t-bk-$T\",\"runId\":\"r-bk-$T\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"who runs this\"}]}" \
  --max-time 20 | sed -n 's/^data: //p')
echo "$frames" | jq -r 'select(.delta != null) | .delta' | tr -d '\n' | grep -q "xai/grok-4.6" \
  || fail "the run did not think with the coworker's model"
ok "no forwardedProps, and still the coworker's own model"

echo "5. the run is owned — the account's token can replay it, anonymous cannot"
replay=$(curl -fsS "$BASE/ag-ui/runs/r-bk-$T" -H "authorization: Bearer $token")
echo "$replay" | jq -e '.status == "finished"' >/dev/null || fail "replay: $replay"
anon=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ag-ui/runs/r-bk-$T")
[ "$anon" = "404" ] || fail "anonymous replay answered $anon"
ok "owned by the minting account, invisible to strangers"

echo "6. revocation is real, and refuses rather than downgrading"
curl -fsS -X DELETE "$BASE/coworkers/$cw/keys/$jti" -H "authorization: Bearer $token" -o /dev/null
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/ag-ui" -H "authorization: Bearer $key" \
  -H 'content-type: application/json' \
  -d "{\"threadId\":\"t-bk2-$T\",\"runId\":\"r-bk2-$T\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hi\"}]}")
[ "$code" = "401" ] || fail "a revoked key answered $code — a silent anonymous downgrade would hide the revocation"
ok "401 after revoke; the key does not quietly become anonymous"

echo
echo "PASS — slice 14 smoke: one durable header binds a Bot to a coworker, until somebody says otherwise."
