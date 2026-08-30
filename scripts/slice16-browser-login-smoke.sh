#!/usr/bin/env bash
# Proves roadmap 9.1b: the PKCE browser login leg — and that it CLOSES the unauthenticated
# LAN token-mint hole a blind /auth/poll left open.
#
# The client's own flow (source/packages/cursor-config/auth/login.ts): verifier = base64url(rand 32),
# challenge = base64url(sha256(verifier)); the browser opens /loginDeepControl?challenge=&uuid=;
# the client polls /auth/poll?uuid=&verifier= and treats 404 as "keep waiting", 200 {accessToken,
# refreshToken} as done. This asserts exactly those states.
#
# Usage:  OG_PORT=1447 scripts/slice16-browser-login-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
BIN="${OG_BIN:-./target/debug/opengrok}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }
command -v jq >/dev/null || fail "jq is required"
command -v python3 >/dev/null || fail "python3 is required"
: "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"

# Its own server: step 5 needs a configured mint address, and the whole point is a fresh listener
# where the login store starts empty.
start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" \
  OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
  OG_MODEL_DOOR=mock OG_PUBLIC_GATEWAY_URL="http://opengrok.lan:$PORT" OG_GATEWAY_BEARER=slice16-bearer \
  RUST_LOG=warn "$BIN" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS --max-time 2 "$BASE/health" -H 'authorization: Bearer slice16-bearer' >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server did not come up"
}
stop_server() { kill "${SERVER_PID:-0}" 2>/dev/null || true; wait "${SERVER_PID:-0}" 2>/dev/null || true; }
trap stop_server EXIT
start_server

# The PKCE pair the client would compute.
pkce() { python3 - "$1" <<'PY'
import base64, hashlib, sys
verifier = sys.argv[1]
challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
print(challenge)
PY
}

echo "1. a blind poll never yields a token (the hole, closed)"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/poll?uuid=probe&verifier=probe")
[ "$code" = "404" ] || fail "a blind poll answered $code, expected 404 (pending forever)"
ok "uuid=probe&verifier=probe → 404, no token"

echo "2. a registered uuid still refuses a WRONG verifier"
UUID="u-$(date +%s)-$$"
VERIFIER="verifier-secret-$(date +%s)"
CHALLENGE=$(pkce "$VERIFIER")
page=$(curl -s -w '\n%{http_code}' "$BASE/loginDeepControl?challenge=$CHALLENGE&uuid=$UUID&mode=login&redirectTarget=cli")
pcode=$(echo "$page" | tail -1)
[ "$pcode" = "200" ] || fail "loginDeepControl answered $pcode"
echo "$page" | head -1 | grep -qi "signed in" || fail "the login page did not confirm sign-in"
wrong=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/poll?uuid=$UUID&verifier=not-the-verifier")
[ "$wrong" = "404" ] || fail "a wrong verifier answered $wrong, expected 404 (pending, learns nothing)"
ok "challenge registered; wrong verifier still 404"

echo "3. the matching verifier completes exactly once"
tokens=$(curl -s "$BASE/auth/poll?uuid=$UUID&verifier=$VERIFIER")
echo "$tokens" | jq -e '.accessToken | length > 0 and (. | contains("."))' >/dev/null || fail "no accessToken: $tokens"
echo "$tokens" | jq -e '.refreshToken | length > 0' >/dev/null || fail "no refreshToken: $tokens"
tok=$(echo "$tokens" | jq -r '.accessToken')
# the token is a real account token — it opens an owner-only endpoint
who=$(python3 - "$tok" <<'PY'
import base64, json, sys
p = sys.argv[1].split('.')[1]; p += '='*(-len(p)%4)
print(json.loads(base64.urlsafe_b64decode(p))['email'])
PY
)
[ -n "$who" ] || fail "the minted token has no email claim"
ok "matching verifier → {accessToken, refreshToken} for $who"

echo "4. the challenge is consumed — a replay gets nothing"
replay=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/poll?uuid=$UUID&verifier=$VERIFIER")
[ "$replay" = "404" ] || fail "a consumed challenge replayed as $replay, expected 404"
ok "one challenge, one token — replay is 404"

echo "5. that token really is an account token: it opens seam B, and the bearer no longer leaks blindly"
mint=$(curl -s -X POST "$BASE/aiserver.v1.GrokBotService/EnsureSandBox" -H "authorization: Bearer $tok")
echo "$mint" | jq -e '.gatewayUrl | length > 0' >/dev/null || fail "EnsureSandBox refused a real token: $mint"
# and the pre-login hole is gone: no token, no seam B
anon=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/aiserver.v1.GrokBotService/EnsureSandBox")
[ "$anon" = "401" ] || fail "EnsureSandBox served an anonymous caller ($anon)"
ok "a browser-earned token opens the mint; an anonymous one is 401"

echo "6. the dev token is loopback-only now"
# This smoke runs on 127.0.0.1, so dev sign-in still works for the OTHER smokes.
loop=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/cursor_dev_session_token?plan=pro&email=x@og.local")
[ "$loop" = "200" ] || fail "dev token refused a loopback caller ($loop) — would break every other smoke"
# A forged non-loopback Host is refused (proxy the check without needing a second interface).
forged=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/cursor_dev_session_token?plan=pro" -H 'host: 192.168.1.9:1447')
[ "$forged" = "401" ] || fail "dev token served a non-loopback Host ($forged) — the LAN mint is still open"
ok "loopback dev sign-in works; a LAN Host is 401"

echo
echo "PASS — slice 16: browser login binds a token to a human step, and the blind LAN mint is closed."
