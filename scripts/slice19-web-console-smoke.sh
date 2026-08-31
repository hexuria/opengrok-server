#!/usr/bin/env bash
# Proves the web console's cookie login leg and the SPA static mount — the server-side half of the
# Account/Admin dashboards. A browser signs in at POST /auth/login, the session comes back as
# httpOnly cookies, and a cookie-only request (no Authorization header) reaches GET /account. The
# desktop client's Bearer path is untouched. Also proves /console serves the built SPA and
# deep-links fall back to index.html.
#
# Usage:  OG_PORT=1474 OG_DATABASE_URL=… scripts/slice19-web-console-smoke.sh
set -euo pipefail
PORT="${OG_PORT:-1337}"; BASE="${OG_BASE:-http://127.0.0.1:$PORT}"; BIN="${OG_BIN:-./target/debug/opengrok}"
fail() { echo "FAIL: $*" >&2; exit 1; }; ok() { echo "  ok: $*"; }
command -v jq >/dev/null || fail "jq is required"; : "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"
SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}"

WORK="$(mktemp -d)"; JAR="$WORK/jar"; JAR2="$WORK/jar2"
CONSOLE="$WORK/console"; mkdir -p "$CONSOLE"
MARKER="OPEN-GROK-CONSOLE-INDEX-$RANDOM"
printf '<!doctype html><title>console</title><div id=app>%s</div>' "$MARKER" > "$CONSOLE/index.html"
cleanup() { kill "${SERVER_PID:-0}" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

admin_cli() { OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" "$BIN" admin "$@"; }
start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" \
  OG_MODEL_DOOR=mock OG_PUBLIC_GATEWAY_URL="http://opengrok.lan:$PORT" OG_GATEWAY_BEARER=s19 \
  OG_WEB_CONSOLE_DIR="$CONSOLE" \
  RUST_LOG=warn "$BIN" >/dev/null 2>&1 & SERVER_PID=$!
  for _ in $(seq 1 30); do curl -fsS --max-time 2 "$BASE/health" -H 'authorization: Bearer s19' >/dev/null 2>&1 && return 0; sleep 1; done
  fail "server did not come up"
}

echo "1. bootstrap an org + admin, start the server (with a console dir)"
STAMP=$(date +%s)-$$; DOMAIN="co$STAMP.com"; ADMIN="admin@$DOMAIN"
ORG=$(admin_cli org create --name "Co" --admin-email "$ADMIN" --domain "$DOMAIN" --password "adminpass1" | awk '/org id:/{print $3}')
[ -n "$ORG" ] || fail "could not read the org id from org create"
admin_cli account create --email "mel@$DOMAIN" --org "$ORG" --name "Mel Ber" --password "memberpass1" >/dev/null
start_server
ok "server up on $PORT (org $ORG, admin + member seeded)"

echo "2. cookie login: POST /auth/login sets httpOnly cookies, body carries the email not a token"
body=$(curl -sS -c "$JAR" -X POST "$BASE/auth/login" -H 'content-type: application/json' \
  -d "{\"email\":\"$ADMIN\",\"password\":\"adminpass1\"}")
echo "$body" | jq -e ".email == \"$ADMIN\"" >/dev/null || fail "login body wrong: $body"
echo "$body" | jq -e 'has("accessToken") | not' >/dev/null || fail "login body leaked a token"
grep -q "og_access" "$JAR" || fail "og_access cookie not set"
grep -q "og_refresh" "$JAR" || fail "og_refresh cookie not set"
grep -qi "HttpOnly" "$JAR" || fail "cookies are not HttpOnly"
ok "cookies set (HttpOnly), no token in the body"

echo "3. a cookie-only request (NO Authorization header) reaches /account"
me=$(curl -sS -b "$JAR" "$BASE/account")
echo "$me" | jq -e ".email == \"$ADMIN\"" >/dev/null || fail "cookie did not authenticate /account: $me"
# And nothing at all → 401.
anon=$(curl -sS -o /dev/null -w '%{http_code}' "$BASE/account")
[ "$anon" = "401" ] || fail "no cookie should be 401, got $anon"
ok "cookie authenticates /account; anonymous is 401"

echo "4. a bad password sets no cookie and says which"
bad=$(curl -sS -c "$JAR2" -o "$WORK/bad.json" -w '%{http_code}' -X POST "$BASE/auth/login" \
  -H 'content-type: application/json' -d "{\"email\":\"$ADMIN\",\"password\":\"WRONG\"}")
[ "$bad" = "401" ] || fail "bad login should be 401, got $bad"
grep -q "og_access" "$JAR2" && fail "a failed login must not set og_access"
jq -e '.error == "Wrong email or password."' "$WORK/bad.json" >/dev/null || fail "bad login message wrong"
ok "wrong password: 401, no cookie, ambiguous message"

echo "5. refresh rotates the session cookies"
rot=$(curl -sS -b "$JAR" -c "$JAR" -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/refresh")
[ "$rot" = "200" ] || fail "refresh should be 200, got $rot"
grep -q "og_access" "$JAR" || fail "refresh should re-set og_access"
# The rotated cookie still authenticates.
curl -sS -b "$JAR" "$BASE/account" | jq -e ".email == \"$ADMIN\"" >/dev/null || fail "rotated cookie does not authenticate"
ok "refresh rotated; the new cookie still works"

echo "6. logout clears the cookies; the session no longer authenticates"
out=$(curl -sS -b "$JAR" -c "$JAR" -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/logout")
[ "$out" = "200" ] || fail "logout should be 200, got $out"
after=$(curl -sS -b "$JAR" -o /dev/null -w '%{http_code}' "$BASE/account")
[ "$after" = "401" ] || fail "after logout /account should be 401, got $after"
ok "logout cleared the session (now 401)"

echo "7. the console is served, and SPA deep-links fall back to index.html"
curl -sS "$BASE/console/" | grep -q "$MARKER" || fail "/console/ did not serve index.html"
deep_code=$(curl -sS -o "$WORK/deep.html" -w '%{http_code}' "$BASE/console/account")
[ "$deep_code" = "200" ] || fail "/console/account should be 200 (SPA route), got $deep_code"
grep -q "$MARKER" "$WORK/deep.html" || fail "/console/account did not fall back to index.html"
ok "static console served; deep-link is a 200 SPA page"

echo "8. isAdmin is reported, the user list is org-scoped and survives logins, self-disable is refused"
AJAR="$WORK/ajar"; MJAR="$WORK/mjar"
curl -sS -c "$AJAR" -X POST "$BASE/auth/login" -H 'content-type: application/json' -d "{\"email\":\"$ADMIN\",\"password\":\"adminpass1\"}" -o /dev/null
curl -sS -c "$MJAR" -X POST "$BASE/auth/login" -H 'content-type: application/json' -d "{\"email\":\"mel@$DOMAIN\",\"password\":\"memberpass1\"}" -o /dev/null
curl -sS -b "$AJAR" "$BASE/account" | jq -e '.isAdmin == true'  >/dev/null || fail "admin should report isAdmin:true"
curl -sS -b "$MJAR" "$BASE/account" | jq -e '.isAdmin == false' >/dev/null || fail "member should report isAdmin:false"
users=$(curl -sS -b "$AJAR" "$BASE/admin/users")
echo "$users" | jq -e '.users | map(.email) | contains(["'"$ADMIN"'","mel@'"$DOMAIN"'"])' >/dev/null || fail "user list missing org accounts: $users"
# The two logins just above would clobber the projection under the old bug; assert it did not.
echo "$users" | jq -e '.users[] | select(.email=="mel@'"$DOMAIN"'") | (.enabled==true) and (.firstName=="Mel") and (.lastName=="Ber")' >/dev/null || fail "member projection was clobbered by login: $users"
ok "isAdmin correct; org-scoped user list with intact profiles after logins"
admin_id=$(echo "$users" | jq -r '.users[] | select(.email=="'"$ADMIN"'") | .id')
mel_id=$(echo "$users" | jq -r '.users[] | select(.email=="mel@'"$DOMAIN"'") | .id')
sd=$(curl -sS -b "$AJAR" -o /dev/null -w '%{http_code}' -X POST "$BASE/admin/users/$admin_id/disable")
[ "$sd" = "409" ] || fail "admin self-disable must be refused (409), got $sd"
curl -sS -b "$AJAR" -X POST "$BASE/admin/users/$mel_id/disable" | jq -e '.enabled==false' >/dev/null || fail "disable member failed"
curl -sS -b "$AJAR" -X POST "$BASE/admin/users/$mel_id/enable"  | jq -e '.enabled==true'  >/dev/null || fail "enable member failed"
ok "self-disable refused (409); member disable/enable round-trips"

echo
echo "PASS — slice 19: web console cookie login (httpOnly, no token in JS) and the /console SPA mount."
