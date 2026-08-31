#!/usr/bin/env bash
# Proves the identity slice: org + invite + domain-gated signup + email verification + admin
# enablement + credential login. The whole chain a real person walks, and the gates that keep a
# stranger out.
#
# Usage:  OG_PORT=1471 OG_DATABASE_URL=… scripts/slice17-identity-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
BIN="${OG_BIN:-./target/debug/opengrok}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }
command -v jq >/dev/null || fail "jq is required"
command -v python3 >/dev/null || fail "python3 is required"
: "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"

SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}"
admin() { OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" "$BIN" admin "$@"; }

start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" \
  OG_MODEL_DOOR=mock OG_PUBLIC_GATEWAY_URL="http://opengrok.lan:$PORT" OG_GATEWAY_BEARER=s17 \
  RUST_LOG=warn "$BIN" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS --max-time 2 "$BASE/health" -H 'authorization: Bearer s17' >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server did not come up"
}
stop_server() { kill "${SERVER_PID:-0}" 2>/dev/null || true; wait "${SERVER_PID:-0}" 2>/dev/null || true; }
trap stop_server EXIT

pkce_challenge() { python3 -c "import base64,hashlib,sys;print(base64.urlsafe_b64encode(hashlib.sha256(sys.argv[1].encode()).digest()).rstrip(b'=').decode())" "$1"; }

echo "1. CLI bootstraps the first org + admin (no server needed)"
STAMP=$(date +%s)-$$
DOMAIN="acme$STAMP.com"
out=$(admin org create --name "Acme Inc" --admin-email "boss@$DOMAIN" --domain "$DOMAIN" --password "bosspass1")
org=$(echo "$out" | awk '/org id:/{print $3}')
[ -n "$org" ] || fail "no org id from bootstrap: $out"
ok "org $org, admin boss@$DOMAIN"

echo "2. the CLI issues an invite code"
code=$(admin invite --org "$org" | awk '/invite code:/{print $3}')
[ -n "$code" ] || fail "no invite code"
ok "invite $code"

start_server

echo "3. signup is refused without both gates"
# wrong domain, real code
r1=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/signup" -H 'content-type: application/json' \
  -d "{\"email\":\"jo@gmail.com\",\"password\":\"password1\",\"code\":\"$code\"}")
[ "$r1" = "403" ] || fail "a gmail with a real code got $r1, expected 403"
# right domain, bad code
r2=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/signup" -H 'content-type: application/json' \
  -d "{\"email\":\"jo@$DOMAIN\",\"password\":\"password1\",\"code\":\"nope\"}")
[ "$r2" = "422" ] || fail "an unknown code got $r2, expected 422"
# short password
r3=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/signup" -H 'content-type: application/json' \
  -d "{\"email\":\"jo@$DOMAIN\",\"password\":\"short\",\"code\":\"$code\"}")
[ "$r3" = "422" ] || fail "a short password got $r3"
ok "gmail refused (403), bad code refused (422), short password refused (422)"

echo "4. signup passes with code + org-domain email"
signup=$(curl -s -X POST "$BASE/auth/signup" -H 'content-type: application/json' \
  -d "{\"email\":\"jo@$DOMAIN\",\"password\":\"password1\",\"firstName\":\"Jo\",\"lastName\":\"Vale\",\"code\":\"$code\"}")
acct=$(echo "$signup" | jq -r '.account_id')
[ -n "$acct" ] && [ "$acct" != "null" ] || fail "no account: $signup"
# no Resend key configured ⇒ auto-verified true, email not sent
echo "$signup" | jq -e '.verified == true and .verification_email_sent == false' >/dev/null \
  || fail "expected auto-verified with no mailer: $signup"
ok "account $acct created, auto-verified (no mailer)"

echo "5. the invite is single-use — a second signup with it is spent"
r4=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/auth/signup" -H 'content-type: application/json' \
  -d "{\"email\":\"kai@$DOMAIN\",\"password\":\"password1\",\"code\":\"$code\"}")
[ "$r4" = "403" ] || fail "a reused invite got $r4, expected 403 (spent)"
ok "the code is spent"

echo "6. login refuses BEFORE the admin enables the account — distinguishably"
U="u-$STAMP"; V="verifier-$STAMP"; C=$(pkce_challenge "$V")
curl -s "$BASE/loginDeepControl?challenge=$C&uuid=$U&mode=login&redirectTarget=cli" >/dev/null
page=$(curl -s -X POST "$BASE/loginDeepControl" \
  --data-urlencode "challenge=$C" --data-urlencode "uuid=$U" \
  --data-urlencode "email=jo@$DOMAIN" --data-urlencode "password=password1")
echo "$page" | grep -qi "awaiting an administrator" || fail "expected the not-enabled message, got: $(echo "$page" | tr -d '\n' | head -c 200)"
# and poll must still be pending — no token slipped out
pcode=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/poll?uuid=$U&verifier=$V")
[ "$pcode" = "404" ] || fail "poll released a token for a not-enabled account ($pcode)"
ok "not enabled → readable refusal, poll still 404"

echo "7. the admin enables the account, then login succeeds"
admin account enable --email "jo@$DOMAIN" >/dev/null
U2="u2-$STAMP"; V2="verifier2-$STAMP"; C2=$(pkce_challenge "$V2")
curl -s "$BASE/loginDeepControl?challenge=$C2&uuid=$U2&mode=login&redirectTarget=cli" >/dev/null
page2=$(curl -s -X POST "$BASE/loginDeepControl" \
  --data-urlencode "challenge=$C2" --data-urlencode "uuid=$U2" \
  --data-urlencode "email=jo@$DOMAIN" --data-urlencode "password=password1")
echo "$page2" | grep -qi "signed in" || fail "enabled login did not succeed: $(echo "$page2" | tr -d '\n' | head -c 200)"
tokens=$(curl -s "$BASE/auth/poll?uuid=$U2&verifier=$V2")
tok=$(echo "$tokens" | jq -r '.accessToken')
[ -n "$tok" ] && [ "$tok" != "null" ] || fail "no token after enabled login: $tokens"
who=$(python3 -c "import base64,json,sys;p=sys.argv[1].split('.')[1];p+='='*(-len(p)%4);print(json.loads(base64.urlsafe_b64decode(p))['email'])" "$tok")
[ "$who" = "jo@$DOMAIN" ] || fail "the token is for $who, not the account that logged in"
ok "enabled → signed in → poll released a token for jo@$DOMAIN"

echo "8. a wrong password is refused, indistinguishably from a wrong email"
bad=$(curl -s -X POST "$BASE/loginDeepControl" \
  --data-urlencode "challenge=$C2" --data-urlencode "uuid=wrong-$STAMP" \
  --data-urlencode "email=jo@$DOMAIN" --data-urlencode "password=WRONG")
echo "$bad" | grep -qi "wrong email or password" || fail "wrong password leaked something specific"
nobody=$(curl -s -X POST "$BASE/loginDeepControl" \
  --data-urlencode "challenge=$C2" --data-urlencode "uuid=wrong2-$STAMP" \
  --data-urlencode "email=ghost@$DOMAIN" --data-urlencode "password=x")
echo "$nobody" | grep -qi "wrong email or password" || fail "a nonexistent email leaked that it does not exist"
ok "wrong password and unknown email give the same answer"

echo "9. the CLI mints a second test identity directly (Uriah's multi-account need)"
admin account create --email "tester@$DOMAIN" --org "$org" --name "Test User" --password "testpass1" >/dev/null
U3="u3-$STAMP"; V3="verifier3-$STAMP"; C3=$(pkce_challenge "$V3")
curl -s "$BASE/loginDeepControl?challenge=$C3&uuid=$U3&mode=login&redirectTarget=cli" >/dev/null
page3=$(curl -s -X POST "$BASE/loginDeepControl" \
  --data-urlencode "challenge=$C3" --data-urlencode "uuid=$U3" \
  --data-urlencode "email=tester@$DOMAIN" --data-urlencode "password=testpass1")
echo "$page3" | grep -qi "signed in" || fail "the CLI-minted account cannot log in"
ok "a second identity logs in — multi-account is real and testable"

echo
echo "PASS — slice 17: org + invite + domain-gated signup + verification + enablement + credential login."
