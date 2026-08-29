#!/usr/bin/env bash
# Proves the permission layer over HTTP: naming somebody else's coworker is not a way to use it.
#
# This is the "what's the status of order 8891?" attack in its simplest form — a well-formed
# request about somebody else's thing. It must be refused by a rule, not by luck.
#
# Usage:  OG_PORT=1447 scripts/slice7-policy-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"

echo "1. two people sign in, and one hires a coworker"
mine=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=owner-$(date +%s)@og.local" | jq -r '.accessToken')
theirs=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=other-$(date +%s)@og.local" | jq -r '.accessToken')
[ -n "$mine" ] && [ -n "$theirs" ] || fail "sign-in failed"

coworker=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $mine" \
  -H 'content-type: application/json' -d '{"name":"Ada"}' | jq -r '.id')
[ -n "$coworker" ] && [ "$coworker" != "null" ] || fail "hire failed"
ok "hired $coworker"

echo "2. the owner may use it"
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/ag-ui" \
  -H "authorization: Bearer $mine" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t\",\"runId\":\"r-own-$(date +%s)\",\"forwardedProps\":{\"coworkerId\":\"$coworker\"},\"messages\":[{\"id\":\"m\",\"role\":\"user\",\"content\":\"hello\"}]}" \
  --max-time 30)
[ "$status" = "200" ] || fail "the owner was refused with $status"
ok "the owner's run is accepted"

echo "3. somebody else naming that coworker is refused by a rule"
# The whole point. A well-formed request about another account's coworker.
body=$(curl -s -w '\n%{http_code}' -X POST "$BASE/ag-ui" \
  -H "authorization: Bearer $theirs" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t\",\"runId\":\"r-theft-$(date +%s)\",\"forwardedProps\":{\"coworkerId\":\"$coworker\"},\"messages\":[{\"id\":\"m\",\"role\":\"user\",\"content\":\"use their coworker\"}]}" \
  --max-time 30)
status=$(echo "$body" | tail -1)
reason=$(echo "$body" | sed '$d')
[ "$status" = "403" ] || fail "expected 403, got $status"
echo "$reason" | grep -qi "grant" || fail "the refusal should name the missing grant: $reason"
ok "403, and the reason names the rule: $(echo "$reason" | head -c 60)…"

echo "4. the refusal is a rule, not an accident of the id being unknown"
# An id that does not exist at all must also be refused — same answer, so a probe learns nothing
# about which coworkers exist.
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/ag-ui" \
  -H "authorization: Bearer $theirs" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t\",\"runId\":\"r-ghost-$(date +%s)\",\"forwardedProps\":{\"coworkerId\":\"cw_does_not_exist\"},\"messages\":[{\"id\":\"m\",\"role\":\"user\",\"content\":\"x\"}]}" \
  --max-time 30)
[ "$status" = "403" ] || fail "an unknown coworker returned $status, expected 403"
ok "an unknown coworker is refused identically — a probe learns nothing"

echo
echo "PASS — slice 7: permission is a rule, checked every turn."
