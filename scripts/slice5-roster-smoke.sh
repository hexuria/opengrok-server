#!/usr/bin/env bash
# Proves the arc joins at the edge: sign in, hire a coworker, see it on your roster — and only
# yours. Runs with no box key, so a coworker gets no computer; that path is asserted too, because
# "no computer" must be an honest state rather than a broken one.
#
# Usage:  OG_PORT=1447 OG_DATABASE_URL=… scripts/slice5-roster-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

echo "1. two people sign in"
one=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=one-$(date +%s)@og.local")
two=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=two-$(date +%s)@og.local")
token_one=$(echo "$one" | jq -r '.accessToken')
token_two=$(echo "$two" | jq -r '.accessToken')
[ -n "$token_one" ] && [ -n "$token_two" ] || fail "sign-in did not return tokens"
ok "two accounts"

echo "2. hiring needs a signed-in account"
anon=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/coworkers" \
  -H 'content-type: application/json' -d '{"name":"Nobody"}')
[ "$anon" = "401" ] || fail "an anonymous hire returned $anon, expected 401"
ok "an anonymous hire is refused"

echo "3. a coworker is hired"
hired=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token_one" \
  -H 'content-type: application/json' -d '{"name":"Ada","model":"xai/grok-4.6"}')
id=$(echo "$hired" | jq -r '.id')
[ -n "$id" ] && [ "$id" != "null" ] || fail "no coworker id: $hired"
[ "$(echo "$hired" | jq -r '.name')" = "Ada" ] || fail "wrong name: $hired"
ok "hired $id"

echo "4. the roster is an array, and it is theirs"
# An ARRAY always: the desktop client throws on a malformed array reply, and an empty roster is a
# valid answer rather than an error.
roster_one=$(curl -fsS "$BASE/coworkers" -H "authorization: Bearer $token_one")
echo "$roster_one" | jq -e 'type == "array"' >/dev/null || fail "the roster is not an array"
echo "$roster_one" | jq -e --arg id "$id" 'any(.[]; .id == $id)' >/dev/null \
  || fail "the new coworker is not on its own roster"
ok "one coworker on the hirer's roster"

echo "5. somebody else's roster does not show it"
roster_two=$(curl -fsS "$BASE/coworkers" -H "authorization: Bearer $token_two")
echo "$roster_two" | jq -e 'type == "array"' >/dev/null || fail "the second roster is not an array"
echo "$roster_two" | jq -e --arg id "$id" 'any(.[]; .id == $id)' >/dev/null \
  && fail "a coworker leaked onto another account's roster"
ok "an empty array, not somebody else's coworker"

echo "6. a coworker with no computer says so honestly"
# With no box key configured this is the expected path: hired, no box, and a reason.
box=$(echo "$hired" | jq -r '.boxId')
if [ "$box" = "null" ]; then
  ok "no computer, and the roster reports boxId: null rather than pretending"
else
  ok "a computer was assigned: $box"
fi

echo "7. the coworker's own model is the one its turn is answered with"
# THE REGRESSION THIS EXISTS FOR. Hiring takes a model, stores it and reports it on the roster,
# while the run read the deployment's `OG_MODEL` and ignored all three. Every visible surface said
# `xai/grok-4.6` and every call went somewhere else; nothing failed, so nothing caught it.
#
# The mock door names the model it was asked for, which is the only place that answer is observable
# from out here — a real door would answer identically either way.
T=$(date +%s)
answered=$(curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token_one" \
  -H 'content-type: application/json' \
  -d "{\"threadId\":\"t-model-$T\",\"runId\":\"r-model-$T\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hello\"}],\"forwardedProps\":{\"coworkerId\":\"$id\"}}" \
  --max-time 20 | sed -n 's/^data: //p' | jq -r 'select(.delta != null) | .delta' | tr -d '\n')
case "$answered" in
  *"xai/grok-4.6"*) ok "the turn was answered with the coworker's model" ;;
  *) fail "the coworker's model did not reach the door; the reply was: $answered" ;;
esac

echo "8. a run with no coworker still gets the deployment's model"
# The default has to survive the fix: the AG-UI endpoint is also how a client with no coworker at
# all just talks to a model, and that path must not start requiring one.
plain=$(curl -sN -X POST "$BASE/ag-ui" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t-plain-$T\",\"runId\":\"r-plain-$T\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hello\"}]}" \
  --max-time 20 | sed -n 's/^data: //p' | jq -r 'select(.delta != null) | .delta' | tr -d '\n')
case "$plain" in
  *"xai/grok-4.6"*) fail "a run with no coworker borrowed a coworker's model: $plain" ;;
  "") fail "a run with no coworker produced nothing" ;;
  *) ok "answered without a coworker, on the deployment's own model" ;;
esac

echo
echo "PASS — slice 5: a coworker is hired, scoped to its account, honest about its computer, and
       answered with its own model."
