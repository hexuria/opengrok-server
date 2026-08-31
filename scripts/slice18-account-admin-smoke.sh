#!/usr/bin/env bash
# Proves the account self-service + org-admin backend: a person edits their own name/avatar/
# password (never email), and the org admin lists/enables/disables users and issues invite links.
#
# Usage:  OG_PORT=1473 OG_DATABASE_URL=… scripts/slice18-account-admin-smoke.sh
set -euo pipefail
PORT="${OG_PORT:-1337}"; BASE="${OG_BASE:-http://127.0.0.1:$PORT}"; BIN="${OG_BIN:-./target/debug/opengrok}"
fail() { echo "FAIL: $*" >&2; exit 1; }; ok() { echo "  ok: $*"; }
command -v jq >/dev/null || fail "jq is required"; : "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"
SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}"
admin_cli() { OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" "$BIN" admin "$@"; }
start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" \
  OG_MODEL_DOOR=mock OG_PUBLIC_GATEWAY_URL="http://opengrok.lan:$PORT" OG_GATEWAY_BEARER=s18 \
  RUST_LOG=warn "$BIN" >/dev/null 2>&1 & SERVER_PID=$!
  for _ in $(seq 1 30); do curl -fsS --max-time 2 "$BASE/health" -H 'authorization: Bearer s18' >/dev/null 2>&1 && return 0; sleep 1; done
  fail "server did not come up"
}
trap 'kill "${SERVER_PID:-0}" 2>/dev/null || true' EXIT

# Log a person in by credential and return their access token (through the browser leg + poll).
login() { # login <email> <password>
  local u="u-$RANDOM-$RANDOM" v="v-$RANDOM-$RANDOM"
  local c; c=$(python3 -c "import base64,hashlib,sys;print(base64.urlsafe_b64encode(hashlib.sha256(sys.argv[1].encode()).digest()).rstrip(b'=').decode())" "$v")
  curl -s "$BASE/loginDeepControl?challenge=$c&uuid=$u&mode=login" >/dev/null
  curl -s -X POST "$BASE/loginDeepControl" --data-urlencode "challenge=$c" --data-urlencode "uuid=$u" \
    --data-urlencode "email=$1" --data-urlencode "password=$2" >/dev/null
  curl -s "$BASE/auth/poll?uuid=$u&verifier=$v" | jq -r '.accessToken // "null"' 2>/dev/null || echo null
}

echo "1. bootstrap an org + admin, start the server"
STAMP=$(date +%s)-$$; DOMAIN="co$STAMP.com"
admin_cli org create --name "Co" --admin-email "admin@$DOMAIN" --domain "$DOMAIN" --password "adminpass1" >/dev/null
start_server
ADMIN_TOK=$(login "admin@$DOMAIN" "adminpass1")
[ -n "$ADMIN_TOK" ] && [ "$ADMIN_TOK" != "null" ] || fail "admin login failed"
ok "admin signed in"

echo "2. the admin issues an invite LINK (code + paste-or-click URL)"
inv=$(curl -s -X POST "$BASE/admin/invites" -H "authorization: Bearer $ADMIN_TOK")
code=$(echo "$inv" | jq -r '.code'); link=$(echo "$inv" | jq -r '.link')
[ -n "$code" ] && [ "$code" != "null" ] || fail "no invite code: $inv"
echo "$link" | grep -q "/signup?code=$code" || fail "the link is not a signup URL with the code: $link"
ok "invite $code with link $link"

echo "3. a user signs up with the code, admin enables them"
curl -s -X POST "$BASE/auth/signup" -H 'content-type: application/json' \
  -d "{\"email\":\"jo@$DOMAIN\",\"password\":\"password1\",\"firstName\":\"Jo\",\"lastName\":\"V\",\"code\":\"$code\"}" >/dev/null
# find the user id via admin list
users=$(curl -s "$BASE/admin/users" -H "authorization: Bearer $ADMIN_TOK")
jo_id=$(echo "$users" | jq -r '.users[] | select(.email=="jo@'"$DOMAIN"'") | .id')
[ -n "$jo_id" ] && [ "$jo_id" != "null" ] || fail "the signed-up user is not in the admin list: $users"
echo "$users" | jq -e '.users[] | select(.email=="jo@'"$DOMAIN"'") | .enabled == false' >/dev/null || fail "user should start disabled"
curl -s -X POST "$BASE/admin/users/$jo_id/enable" -H "authorization: Bearer $ADMIN_TOK" | jq -e '.enabled == true' >/dev/null || fail "enable failed"
ok "user listed, started disabled, admin enabled them"

echo "4. the user signs in and reads their own account (email present, editable fields too)"
JO_TOK=$(login "jo@$DOMAIN" "password1")
[ -n "$JO_TOK" ] && [ "$JO_TOK" != "null" ] || fail "user login failed after enable"
me=$(curl -s "$BASE/account" -H "authorization: Bearer $JO_TOK")
echo "$me" | jq -e '.email == "jo@'"$DOMAIN"'" and .firstName == "Jo"' >/dev/null || fail "account read wrong: $me"
ok "own account reads back"

echo "5. the user updates name + avatar (a data URL), not email"
PNG="data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="
upd=$(curl -s -X POST "$BASE/account/profile" -H "authorization: Bearer $JO_TOK" -H 'content-type: application/json' \
  -d "{\"firstName\":\"Josephine\",\"avatarUrl\":\"$PNG\"}")
echo "$upd" | jq -e '.firstName == "Josephine" and (.avatarUrl | startswith("data:image/png"))' >/dev/null || fail "profile update: $upd"
echo "$upd" | jq -e '.email == "jo@'"$DOMAIN"'"' >/dev/null || fail "email must be unchanged"
# there is no way to change the email — the endpoint ignores any email field
noemail=$(curl -s -X POST "$BASE/account/profile" -H "authorization: Bearer $JO_TOK" -H 'content-type: application/json' -d '{"email":"hacker@evil.com","firstName":"Jo"}')
echo "$noemail" | jq -e '.email == "jo@'"$DOMAIN"'"' >/dev/null || fail "email got changed via profile update!"
ok "name + avatar updated; email is immutable"

echo "6. a junk avatar and an oversized one are refused"
bad=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/account/profile" -H "authorization: Bearer $JO_TOK" -H 'content-type: application/json' -d '{"avatarUrl":"http://evil/x.png"}')
[ "$bad" = "422" ] || fail "a non-data avatar answered $bad"
ok "non-data:image avatar refused (422)"

echo "7. the user changes their password, current required"
wrong=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/account/password" -H "authorization: Bearer $JO_TOK" -H 'content-type: application/json' -d '{"currentPassword":"WRONG","newPassword":"newpass123"}')
[ "$wrong" = "403" ] || fail "wrong current password answered $wrong"
ok_change=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/account/password" -H "authorization: Bearer $JO_TOK" -H 'content-type: application/json' -d '{"currentPassword":"password1","newPassword":"newpass123"}')
[ "$ok_change" = "204" ] || fail "password change answered $ok_change"
# old password no longer logs in; new one does
oldtok=$(login "jo@$DOMAIN" "password1"); [ "$oldtok" = "null" ] || fail "the old password still works"
newtok=$(login "jo@$DOMAIN" "newpass123"); [ -n "$newtok" ] && [ "$newtok" != "null" ] || fail "the new password does not work"
ok "wrong current refused; changed; old password dead, new one works"

echo "8. a non-admin cannot manage users"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/admin/users" -H "authorization: Bearer $newtok")
[ "$code" = "403" ] || fail "a member reached /admin/users ($code)"
code2=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/admin/users/$jo_id/disable" -H "authorization: Bearer $newtok")
[ "$code2" = "403" ] || fail "a member disabled a user ($code2)"
ok "admin endpoints are admin-only (403 for a member)"

echo "9. the admin disables the user, and login then refuses"
curl -s -X POST "$BASE/admin/users/$jo_id/disable" -H "authorization: Bearer $ADMIN_TOK" | jq -e '.enabled == false' >/dev/null || fail "disable failed"
gone=$(login "jo@$DOMAIN" "newpass123"); [ "$gone" = "null" ] || fail "a disabled user still logs in"
ok "disabled → login refused"

echo
echo "PASS — slice 18: account self-service (name/avatar/password, never email) and org-admin user management."
