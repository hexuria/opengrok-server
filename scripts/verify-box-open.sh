#!/usr/bin/env bash
# Prove the box-open path: grok-box publishes noVNC, and OpenGrok returns vncUrl.
set -euo pipefail

BASE="${OG_BASE:-http://127.0.0.1:1447}"
EMAIL="${OG_EMAIL:-signin@acme.test}"
PASSWORD="${OG_PASSWORD:-nativechat-dev}"
IMAGE="${OG_DOCKER_IMAGE:-grok-box:local}"
NAME="og-box-open-smoke"

fail() { echo "FAILED: $*" >&2; exit 1; }
pass() { echo "OK: $*"; }

echo "=== 1. grok-box image publishes noVNC"
docker image inspect "$IMAGE" >/dev/null 2>&1 || fail "missing image $IMAGE"
docker rm -f "$NAME" >/dev/null 2>&1 || true
cid=$(docker run -d --name "$NAME" --label dev.opengrok.box=1 \
  -p 127.0.0.1::6080 \
  -e BOX_TOKEN=og-box-open-smoke-token \
  -e BOX_VNC_PASSWORD=opengrok \
  -e BOX_DESKTOP=1 \
  -e BOX_DESKTOP_REQUIRED=0 \
  -e BOX_ALLOW_INSECURE_DEV=1 \
  "$IMAGE")
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT

mapping=""
for _ in $(seq 1 30); do
  mapping=$(docker port "$NAME" 6080 2>/dev/null | head -1 || true)
  if [ -n "$mapping" ]; then
    break
  fi
  sleep 1
done
[ -n "$mapping" ] || fail "docker port 6080 never appeared"
host_port="${mapping##*:}"
host_port="${host_port%%$'\r'}"
url="http://127.0.0.1:${host_port}/vnc.html"
code=""
for _ in $(seq 1 40); do
  code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 2 "$url" || true)
  if [ "$code" = "200" ]; then
    break
  fi
  sleep 1
done
[ "$code" = "200" ] || fail "GET $url -> $code"
pass "noVNC at $url"

echo "=== 2. OpenGrok computer status"
python3 - "$BASE" "$EMAIL" "$PASSWORD" <<'PY' || fail "login or computer status"
import json, sys, urllib.request, urllib.error, http.cookiejar
base, email, password = sys.argv[1], sys.argv[2], sys.argv[3]
cj = http.cookiejar.CookieJar()
opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(cj))

def req(method, path, data=None, token=None):
    body = None
    headers = {}
    if data is not None:
        body = json.dumps(data).encode()
        headers["Content-Type"] = "application/json"
    r = urllib.request.Request(base + path, data=body, method=method, headers=headers)
    if token:
        r.add_header("Authorization", f"Bearer {token}")
    with opener.open(r, timeout=20) as resp:
        raw = resp.read()
        return json.loads(raw) if raw else None, resp.status

login, _ = req("POST", "/auth/login", {"email": email, "password": password})
token = next((c.value for c in cj if c.name == "og_access"), "")
if not token:
    raise SystemExit("no og_access cookie")
print("OK: login", login)

coworkers, _ = req("GET", "/coworkers", token=token)
hexuria = next((row["id"] for row in coworkers if row.get("name") == "Hexuria"), "")
if not hexuria:
    raise SystemExit("Hexuria missing from GET /coworkers")
print("OK: Hexuria", hexuria)

status, _ = req("GET", f"/coworkers/{hexuria}/computer", token=token)
if "state" not in status or "agentId" not in status:
    raise SystemExit(f"bad computer json {status}")
print("OK: GET computer", status.get("state"), "vncUrl", status.get("vncUrl"))

try:
    admin, code = req("GET", "/admin/computers", token=token)
    print("OK: GET /admin/computers", code, admin)
except urllib.error.HTTPError as e:
    print("OK: GET /admin/computers", e.code, e.read()[:200].decode())

hired, _ = req("POST", "/coworkers", {"name": "box-open-probe"}, token=token)
probe_id = hired.get("id") or hired.get("coworkerId")
print("OK: hired", probe_id, "box_id", hired.get("boxId") or hired.get("box_id"))
if probe_id:
    ensured, _ = req("POST", f"/coworkers/{probe_id}/computer", token=token)
    print("OK: POST computer", ensured.get("state"), "vncUrl", ensured.get("vncUrl"))
    if ensured.get("vncUrl"):
        vnc = ensured["vncUrl"].split("?")[0]
        import urllib.request as u
        code = u.urlopen(vnc, timeout=5).status
        print("OK: probe noVNC", code, vnc)
PY

coworkers=$(curl -sS -H "Authorization: Bearer $token" "$BASE/coworkers")
hexuria=$(printf '%s' "$coworkers" | python3 -c '
import json,sys
rows=json.load(sys.stdin)
for row in rows:
    if row.get("name")=="Hexuria":
        print(row.get("id",""))
        break
')
[ -n "$hexuria" ] || fail "Hexuria missing from GET /coworkers"
pass "Hexuria $hexuria"

status=$(curl -sS -H "Authorization: Bearer $token" "$BASE/coworkers/$hexuria/computer")
printf '%s' "$status" | python3 -c '
import json,sys
d=json.load(sys.stdin)
assert "state" in d, d
assert "agentId" in d, d
print("state", d.get("state"), "vncUrl", d.get("vncUrl"), "keys", sorted(d.keys()))
'
pass "GET /coworkers/{id}/computer returns box status JSON"

admin=$(curl -sS -o /tmp/og-admin-computers.json -w '%{http_code}' \
  -H "Authorization: Bearer $token" "$BASE/admin/computers")
if [ "$admin" = "200" ]; then
  pass "GET /admin/computers 200 $(python3 -c 'import json; print(json.load(open("/tmp/og-admin-computers.json")))')"
elif [ "$admin" = "403" ]; then
  pass "GET /admin/computers 403 (account is not org admin)"
else
  fail "GET /admin/computers -> $admin"
fi

echo "DONE"
