#!/usr/bin/env bash
# Proves slice 1: the desktop client can sign in against us instead of Cursor.
#
# Every assertion here is something the CLIENT does, not something we wish were true — each is
# annotated with the file and line that makes it load-bearing. "200 accepted" is not "honoured"
# (CLAUDE.md #10), so the shapes are checked, not just the status codes.
#
# Usage:  OG_BASE=http://127.0.0.1:1337 scripts/slice1-auth-smoke.sh
set -euo pipefail

BASE="${OG_BASE:-http://127.0.0.1:1337}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

need() { command -v "$1" >/dev/null || fail "$1 is required"; }
need curl
need jq
need python3

echo "1. health answers inside the client's 1500 ms deadline"
# The supervisor discards the connection and re-resolves forever if this misses (RUNBOOK §4).
start=$(python3 -c 'import time; print(int(time.time()*1000))')
health=$(curl -fsS --max-time 2 "$BASE/health") || fail "health did not answer"
elapsed=$(( $(python3 -c 'import time; print(int(time.time()*1000))') - start ))
[ "$(echo "$health" | jq -r '.ok')" = "true" ] || fail "health.ok is not true: $health"
[ "$elapsed" -lt 1500 ] || fail "health took ${elapsed}ms, over the client's 1500ms deadline"
ok "health ok=true in ${elapsed}ms"

echo "2. dev sign-in returns camelCase tokens"
# cursor-auth.ts:315-316 reads body.accessToken / body.refreshToken. snake_case here throws
# SandDevLoginError("dev session response did not include an accessToken").
EMAIL="smoke-$(date +%s)@opengrok.local"
login=$(curl -fsS -H 'accept: application/json' \
  "$BASE/auth/cursor_dev_session_token?plan=pro&trial=true&email=$EMAIL") || fail "dev login failed"
access=$(echo "$login" | jq -r '.accessToken // empty')
refresh=$(echo "$login" | jq -r '.refreshToken // empty')
[ -n "$access" ]  || fail "no accessToken in reply: $login"
[ -n "$refresh" ] || fail "no refreshToken in reply: $login"
# We always send a refresh token rather than letting the client fall back to the access token.
[ "$access" != "$refresh" ] || fail "refreshToken must not be the accessToken"
ok "accessToken and refreshToken present and distinct"

echo "3. the access token is a JWT the client can read unaided"
# cursor-token.ts:9-22 base64url-decodes the payload itself; cursor-auth.ts:67-73 builds the whole
# logged-in status from sub/email/exp. An opaque token parses to null and the app shows logged-out.
claims=$(python3 - "$access" <<'PY'
import base64, json, sys
payload = sys.argv[1].split(".")[1]
payload += "=" * (-len(payload) % 4)
print(json.dumps(json.loads(base64.urlsafe_b64decode(payload))))
PY
) || fail "access token is not a decodable JWT"
sub=$(echo "$claims" | jq -r '.sub // empty')
claim_email=$(echo "$claims" | jq -r '.email // empty')
exp=$(echo "$claims" | jq -r '.exp // empty')
[ -n "$sub" ] || fail "no sub claim (client uses it as authId): $claims"
[ "$claim_email" = "$EMAIL" ] || fail "email claim is '$claim_email', expected '$EMAIL'"
[ -n "$exp" ] || fail "no exp claim (client uses it as expiresAt)"
# Seconds, not milliseconds — cursor-auth.ts:71 multiplies by 1000. A millisecond exp would put
# expiry ~50,000 years out and the client would never refresh.
now=$(date +%s)
[ "$exp" -gt "$now" ] || fail "exp $exp is already past"
[ "$exp" -lt $(( now + 86400 * 365 )) ] || fail "exp $exp looks like milliseconds, not seconds"
ok "sub=$sub email=$claim_email exp in $(( (exp - now) / 60 )) min"

echo "4. refresh returns snake_case tokens and rotates them"
# parseOAuthTokenBody (cursor-auth.ts:160-166) rejects the body unless access_token /
# refresh_token are strings. camelCase here throws SandAuthSignInExpiredError.
refreshed=$(curl -fsS -X POST "$BASE/oauth/token" \
  -H 'content-type: application/json' \
  -d "{\"client_id\":\"OzaBXLClY5CAGxNzUhQ2vlknpi07tGuE\",\"grant_type\":\"refresh_token\",\"refresh_token\":\"$refresh\"}") \
  || fail "refresh failed"
new_access=$(echo "$refreshed" | jq -r '.access_token // empty')
new_refresh=$(echo "$refreshed" | jq -r '.refresh_token // empty')
[ -n "$new_access" ]  || fail "no access_token (snake_case) in reply: $refreshed"
[ -n "$new_refresh" ] || fail "no refresh_token (snake_case) in reply: $refreshed"
[ "$new_refresh" != "$refresh" ] || fail "refresh token was not rotated"
ok "rotated: new access_token and refresh_token issued"

echo "5. the old refresh token stops working"
# Without this, a leaked refresh token is immortal.
old_status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/oauth/token" \
  -H 'content-type: application/json' \
  -d "{\"grant_type\":\"refresh_token\",\"refresh_token\":\"$refresh\"}")
[ "$old_status" = "401" ] || fail "reusing the old refresh token returned $old_status, expected 401"
ok "the rotated-away token is refused with 401"

echo "6. the same person signing in twice is one account, not two"
# The client dev-logs-in on every launch. Two accounts would split somebody's coworkers in half.
curl -fsS -H 'accept: application/json' \
  "$BASE/auth/cursor_dev_session_token?plan=pro&trial=true&email=$EMAIL" >/dev/null
count=$(docker exec oag-dev-postgres-1 psql -U oag -d opengrok -tAc \
  "select count(*) from account_view where email = '$EMAIL'" 2>/dev/null || echo "skip")
if [ "$count" = "skip" ]; then
  echo "  (skipped: no direct database access)"
else
  [ "$count" = "1" ] || fail "expected 1 account row for $EMAIL, found $count"
  ok "exactly one account row after two sign-ins"
fi

echo "7. the account came from Postgres, not from a constant"
if [ "$count" != "skip" ]; then
  events=$(docker exec oag-dev-postgres-1 psql -U oag -d opengrok -tAc \
    "select count(*) from events where payload->>'email' = '$EMAIL'")
  [ "$events" -ge 1 ] || fail "no events were written for $EMAIL"
  ok "$events event(s) in the log for this account"
fi

echo
echo "PASS — slice 1: a client can sign in, stay signed in, and cannot reuse a spent token."
