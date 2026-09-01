#!/usr/bin/env bash
# One identity, two doors: an org admin mints a member a gateway key from the console's API, and
# only the admin can. Runs against a STAND-IN gateway admin API (python http.server) so the smoke
# needs no open-ai-gateway — the real gateway is exercised by its own repo's tests and by the live
# end-to-end run recorded in docs/verification/one-identity/.
#
# Usage:  OG_PORT=1449 scripts/slice21-org-keys-smoke.sh
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
command -v python3 >/dev/null || fail "python3 is required"

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
: "${OG_DATABASE_URL:?this smoke needs OG_DATABASE_URL}"

STAND_IN_PORT=$(( PORT + 40 ))
STAND_IN_PID=""
SERVER_PID=""
WORK="$(mktemp -d)"
cleanup() {
  [ -n "$STAND_IN_PID" ] && kill "$STAND_IN_PID" 2>/dev/null || true
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "1. a stand-in for the gateway's admin API"
cat > "$WORK/stand_in.py" <<'PY'
import json, sys, uuid
from http.server import BaseHTTPRequestHandler, HTTPServer

class Handler(BaseHTTPRequestHandler):
    def _reply(self, body, status=200):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _guard(self):
        # The stand-in insists on a bearer, so a smoke that forgot to configure one fails here
        # rather than passing against an unauthenticated door.
        if not self.headers.get("authorization", "").startswith("Bearer "):
            self._reply({"error": "an admin API key is required"}, 401)
            return False
        return True

    def do_POST(self):
        if not self._guard():
            return
        if self.path == "/admin/api/principals":
            self._reply({"id": "principal-1", "email": "org"})
        elif self.path == "/admin/api/keys":
            # A FRESH id per mint, like the real gateway: a fixed one collides with the
            # gateway_key_view primary key the second time this smoke runs on a database that
            # kept its rows, which is exactly what the gate does.
            self._reply({
                "id": f"key-smoke-{uuid.uuid4()}",
                "key_prefix": "oag_live_smoke01",
                "key": "oag_live_smoke0123456789_shown_once",
            })
        elif self.path.endswith("/revoke"):
            self._reply({"id": self.path.split("/")[-2], "active": False})
        else:
            self._reply({"error": "no such path"}, 404)

    def do_PATCH(self):
        if not self._guard():
            return
        self._reply({"ok": True})

    def do_GET(self):
        if not self._guard():
            return
        if self.path.endswith("/usage"):
            self._reply({
                "id": "principal-1", "email": "org",
                "monthly_budget_usd": "50.000000",
                "month_to_date_usd": "2.500000",
                "requests": 3,
            })
        else:
            self._reply({"error": "no such path"}, 404)

    def log_message(self, *args):
        pass

HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
PY
python3 "$WORK/stand_in.py" "$STAND_IN_PORT" &
STAND_IN_PID=$!
for _ in $(seq 1 20); do
  curl -s -o /dev/null "http://127.0.0.1:$STAND_IN_PORT/admin/api/principals" && break
  sleep 0.5
done
ok "stand-in gateway admin on :$STAND_IN_PORT"

echo "2. a server wired to it"
# The gateway admin connection is read at boot, so this server is started for this smoke.
OG_BIND="127.0.0.1:$PORT" \
OG_DATABASE_URL="$OG_DATABASE_URL" \
OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
OG_MODEL_DOOR=mock \
OG_GATEWAY_ADMIN_URL="http://127.0.0.1:$STAND_IN_PORT" \
OG_GATEWAY_ADMIN_TOKEN="oag_live_smoke_admin" \
RUST_LOG=warn \
./target/debug/opengrok >/dev/null 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 30); do
  curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 || fail "the server did not come up"
ok "server on :$PORT with the admin door wired"

echo "3. bootstrap an org with an admin and a member"
tag="$(date +%s)$RANDOM"
domain="orgkeys${tag}.test"
./target/debug/opengrok admin org create --admin-email "admin@$domain" --name "OrgKeys$tag" \
  --domain "$domain" --password "password1" >"$WORK/org.txt" 2>&1 || fail "org create: $(cat "$WORK/org.txt")"
./target/debug/opengrok admin account create --email "member@$domain" --name "A Member" \
  --password "password1" --org "$(grep -oE 'org_[a-z0-9-]+' "$WORK/org.txt" | head -1)" \
  >"$WORK/member.txt" 2>&1 || fail "member create: $(cat "$WORK/member.txt")"
admin_token=$(curl -fsS -X POST "$BASE/auth/login" -H 'content-type: application/json' \
  -d "{\"email\":\"admin@$domain\",\"password\":\"password1\"}" -c "$WORK/admin.jar" \
  >/dev/null 2>&1 && echo cookie || fail "admin login")
member_login=$(curl -fsS -X POST "$BASE/auth/login" -H 'content-type: application/json' \
  -d "{\"email\":\"member@$domain\",\"password\":\"password1\"}" -c "$WORK/member.jar")
member_id=$(echo "$member_login" | jq -r '.id // empty')
[ -n "$member_id" ] || member_id=$(curl -fsS "$BASE/account" -b "$WORK/member.jar" | jq -r '.id')
[ -n "$member_id" ] || fail "no member id"
ok "org on $domain, member $member_id"

echo "4. the admin mints the member a key — shown once"
minted=$(curl -fsS -X POST "$BASE/admin/gateway/keys" -b "$WORK/admin.jar" \
  -H 'content-type: application/json' -d "{\"memberId\":\"$member_id\",\"quotaUsd\":\"5.00\"}")
echo "$minted" | jq -e '.key | startswith("oag_live_")' >/dev/null || fail "no key in the mint reply: $minted"
key_id=$(echo "$minted" | jq -r '.id')
ok "minted $key_id for the member"

echo "5. the listing has the key and NOT the secret"
listed=$(curl -fsS "$BASE/admin/gateway/keys" -b "$WORK/admin.jar")
echo "$listed" | jq -e --arg id "$key_id" '.keys | map(.id) | index($id)' >/dev/null \
  || fail "the key is not listed: $listed"
echo "$listed" | grep -q "shown_once" && fail "the listing leaked the secret"
ok "listed by prefix only; the secret appears nowhere"

echo "6. usage and budget round-trip"
curl -fsS -X PUT "$BASE/admin/gateway/budget" -b "$WORK/admin.jar" \
  -H 'content-type: application/json' -d '{"monthlyBudgetUsd":"50.00"}' >/dev/null || fail "set budget"
usage=$(curl -fsS "$BASE/admin/gateway/usage" -b "$WORK/admin.jar")
echo "$usage" | jq -e '.monthToDateUsd == "2.500000" and .provisioned == true' >/dev/null \
  || fail "usage did not come from the gateway: $usage"
ok "the org's spend is read live from the gateway"

echo "7. a MEMBER cannot mint, budget, or even look"
for path in "/admin/gateway/keys" "/admin/gateway/usage"; do
  code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE$path" -b "$WORK/member.jar")
  [ "$code" = "403" ] || fail "a member reached $path (got $code)"
done
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/admin/gateway/keys" -b "$WORK/member.jar" \
  -H 'content-type: application/json' -d "{\"memberId\":\"$member_id\"}")
[ "$code" = "403" ] || fail "a member minted a key (got $code)"
ok "every member request is refused 403"

echo "8. revoke, and the row mirrors it"
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/admin/gateway/keys/$key_id" -b "$WORK/admin.jar")
[ "$code" = "204" ] || fail "revoke returned $code"
listed=$(curl -fsS "$BASE/admin/gateway/keys" -b "$WORK/admin.jar")
echo "$listed" | jq -e --arg id "$key_id" '.keys[] | select(.id == $id) | .revoked == true' >/dev/null \
  || fail "the row does not show revoked: $listed"
ok "revoked in the gateway and mirrored locally"

echo "9. an unknown key id is 404, not 403"
code=$(curl -s -o /dev/null -w '%{http_code}' -X DELETE "$BASE/admin/gateway/keys/key-not-ours" -b "$WORK/admin.jar")
[ "$code" = "404" ] || fail "an unknown key id answered $code"
ok "a key that is not this org's simply does not exist"

echo
echo "SLICE 21 SMOKE PASSED — one org, one principal, per-member keys, admin-only"
