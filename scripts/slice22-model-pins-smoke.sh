#!/usr/bin/env bash
# A coworker's model is the coworker's — and it can be CHANGED.
#
# slice5 proved a pin reaches the door; this proves the pin can move. The mock door names the model
# it was asked for, which is the only place that answer is observable without spending anything.
#
# Usage:  OG_PORT=1449 scripts/slice22-model-pins-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

echo "1. somebody signs in and hires a coworker on a specific route"
T=$(date +%s)$RANDOM
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=pins-$T@og.local" | jq -r '.accessToken')
[ -n "$token" ] || fail "no access token"
hired=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Pinned","model":"xai/grok-4.6"}')
id=$(echo "$hired" | jq -r '.id')
[ -n "$id" ] && [ "$id" != "null" ] || fail "no coworker id: $hired"
echo "$hired" | jq -e '.model == "xai/grok-4.6"' >/dev/null || fail "hire ignored the pin: $hired"
ok "hired $id on xai/grok-4.6"

answered_on() { # $1 coworker id, $2 tag — echoes the model the door was asked for
  curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token" -H 'content-type: application/json' \
    -d "{\"threadId\":\"t-$2\",\"runId\":\"r-$2\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hello\"}],\"forwardedProps\":{\"coworkerId\":\"$1\"}}" \
    --max-time 60 | sed -n 's/^data: //p' | jq -r 'select(.type=="TEXT_MESSAGE_CONTENT") | .delta' 2>/dev/null | tr -d '\n'
}

echo "2. the turn is answered with the coworker's own route"
first=$(answered_on "$id" "a$T")
case "$first" in
  *"xai/grok-4.6"*) ok "the door was asked for xai/grok-4.6" ;;
  *) fail "the pin did not reach the door: $first" ;;
esac

echo "3. the pin is CHANGED — the point of the slice"
repinned=$(curl -fsS -X PATCH "$BASE/coworkers/$id" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"model":"openai/gpt-5.5"}')
echo "$repinned" | jq -e '.model == "openai/gpt-5.5"' >/dev/null || fail "repin did not take: $repinned"
listed=$(curl -fsS "$BASE/coworkers" -H "authorization: Bearer $token")
echo "$listed" | jq -e --arg id "$id" 'map(select(.id == $id))[0] | .model == "openai/gpt-5.5" and .name == "Pinned"' >/dev/null \
  || fail "the roster did not follow the repin (or renamed it): $listed"
ok "repinned to openai/gpt-5.5, name intact"

echo "4. the NEXT turn is answered on the new route"
second=$(answered_on "$id" "b$T")
case "$second" in
  *"openai/gpt-5.5"*) ok "the door was asked for the new pin" ;;
  *"xai/grok-4.6"*) fail "the run used the OLD pin after a repin: $second" ;;
  *) fail "the new pin did not reach the door: $second" ;;
esac

echo "5. a blank pin is refused rather than stored"
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$BASE/coworkers/$id" \
  -H "authorization: Bearer $token" -H 'content-type: application/json' -d '{"model":"   "}')
[ "$code" = "400" ] || fail "a blank repin answered $code"
still=$(curl -fsS "$BASE/coworkers" -H "authorization: Bearer $token" | jq -r --arg id "$id" 'map(select(.id == $id))[0].model')
[ "$still" = "openai/gpt-5.5" ] || fail "the refused repin still changed the pin: $still"
ok "refused 400; the old pin survived"

echo "6. another account cannot repin it, and cannot learn it exists"
other=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=other-$T@og.local" | jq -r '.accessToken')
code=$(curl -s -o /dev/null -w '%{http_code}' -X PATCH "$BASE/coworkers/$id" \
  -H "authorization: Bearer $other" -H 'content-type: application/json' -d '{"model":"oag/cheap"}')
[ "$code" = "404" ] || fail "another account's repin answered $code, not 404"
ok "404 — not 403, which would confirm it exists"

echo "7. the deployment's own model still answers a run with no coworker"
# slice5 step 8's invariant: this path must not start requiring a coworker.
plain=$(curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t-plain-$T\",\"runId\":\"r-plain-$T\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hello\"}]}" \
  --max-time 60 | sed -n 's/^data: //p' | jq -r 'select(.type=="TEXT_MESSAGE_CONTENT") | .delta' 2>/dev/null | tr -d '\n')
case "$plain" in
  *"openai/gpt-5.5"*|*"xai/grok-4.6"*) fail "a run with no coworker borrowed a coworker's pin: $plain" ;;
  "") fail "no answer without a coworker" ;;
  *) ok "answered on the deployment's own model" ;;
esac

echo "8. the catalogue never carries the gateway's key"
models=$(curl -fsS "$BASE/models" -H "authorization: Bearer $token")
echo "$models" | jq -e 'has("models")' >/dev/null || fail "no models key: $models"
echo "$models" | grep -q "oag_live_" && fail "the gateway key reached the client"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/models")
[ "$code" = "401" ] || fail "the catalogue answered $code without a token"
ok "listed without leaking a key; 401 without a token"

echo
echo "SLICE 22 SMOKE PASSED — a pin is the coworker's, changeable, and never blank"
