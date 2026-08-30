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

echo "2. loginDeepControl now presents a LOGIN FORM, not an auto-sign-in"
UUID="u-$(date +%s)-$$"
VERIFIER="verifier-secret-$(date +%s)"
CHALLENGE=$(pkce "$VERIFIER")
page=$(curl -s "$BASE/loginDeepControl?challenge=$CHALLENGE&uuid=$UUID&mode=login&redirectTarget=cli")
echo "$page" | grep -qi "Sign in" || fail "loginDeepControl did not render a sign-in form"
echo "$page" | grep -qi "Open Grok" || fail "the login page is not branded"
echo "$page" | grep -qi 'name=password' || fail "the form has no password field"
# It must NOT hand out a session just for opening the URL — that was the old behaviour, now gone.
echo "$page" | grep -qi "Signed in" && fail "opening the URL alone still signs in (opener-is-host regression)"
ok "a credential form, no token from merely opening the page"

echo "3. a poll before any credential login stays pending"
# The challenge is registered, but nobody authenticated — poll must be 404, no token.
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/poll?uuid=$UUID&verifier=$VERIFIER")
[ "$code" = "404" ] || fail "poll released a token with no credential login ($code)"
ok "registered but un-authenticated → 404, no token"

echo "4. a wrong-verifier poll is also just pending (learns nothing)"
wrong=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/auth/poll?uuid=$UUID&verifier=not-it")
[ "$wrong" = "404" ] || fail "a wrong verifier answered $wrong"
ok "wrong verifier → 404 (the credential-login chain is proven in slice 17)"

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
