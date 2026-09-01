#!/usr/bin/env bash
# Everything CI runs, run here.
#
# This exists because CI is not always available — a billing lapse, an outage, a plane — and a
# project whose only gate is a hosted runner has no gate on those days. It is deliberately the SAME
# commands in the SAME order as .github/workflows/ci.yml: if the two drift, the local ritual stops
# predicting the remote one and people stop trusting either.
#
# Usage:
#   scripts/gate.sh              # checks and tests only
#   scripts/gate.sh --smoke      # also stands the server up and runs the smoke scripts
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n=== %s\n' "$*"; }
fail() { echo "GATE FAILED: $*" >&2; exit 1; }

step "cargo fmt --all --check"
cargo fmt --all --check || fail "formatting (run: cargo fmt --all)"

step "cargo check --workspace"
cargo check --workspace || fail "check"

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings || fail "clippy"

step "cargo test --workspace"
cargo test --workspace || fail "tests"

if [ "${1:-}" != "--smoke" ]; then
  echo
  echo "GATE PASSED (checks and tests). Add --smoke to also run the smoke scripts."
  exit 0
fi

: "${OG_DATABASE_URL:?--smoke needs OG_DATABASE_URL, e.g. postgres://oag:oag@127.0.0.1:5452/opengrok}"

# THE GATE OWNS ITS DATABASE. The autonomy sweeps claim work with `for update skip locked`, so a
# second opengrok ON THE SAME DATABASE will legitimately RACE the smoke servers for schedule and
# monitor firings — and fire them with its own model door. Learned when a dev server with the real
# door won a monitor firing and the smoke read back a 403 instead of the mock's echo.
#
# The guard is DATABASE-SCOPED, not "any opengrok": a server on a different database (e.g. a live
# verification server, or another checkout) shares no rows and cannot race, so it is left alone.
for pid in $(pgrep -f "target/debug/opengrok" 2>/dev/null || true); do
  if ps eww -p "$pid" 2>/dev/null | tr ' ' '\n' | grep -qxF "OG_DATABASE_URL=$OG_DATABASE_URL"; then
    fail "another opengrok is running on $OG_DATABASE_URL; it would race the smokes — use a separate database"
  fi
done
# Not 1337: grok-bot's local-docker box binds that port, and a clash here looks like a broken
# server rather than a taken port.
PORT="${OG_PORT:-1447}"
BASE="http://127.0.0.1:$PORT"

# Claim the port rather than assume it: a server left over from an earlier run would answer every
# health check and quietly make the smoke tests test somebody else's process.
if lsof -ti:"$PORT" >/dev/null 2>&1; then
  echo "note: freeing port $PORT, something was already listening"
  lsof -ti:"$PORT" | xargs kill -9 2>/dev/null || true
  sleep 1
fi

step "starting a server on $PORT with the mock door"
OG_BIND="127.0.0.1:$PORT" \
OG_DATABASE_URL="$OG_DATABASE_URL" \
OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
OG_MODEL_DOOR=mock \
RUST_LOG=warn \
./target/debug/opengrok >/dev/null 2>&1 &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true' EXIT

for _ in $(seq 1 30); do
  curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 || fail "the server did not come up"

for script in slice1-auth slice2-agui slice3-harness slice5-roster slice7-policy slice11-gateway slice12-conversation slice14-botkey slice15-lifecycle; do
  step "scripts/$script-smoke.sh"
  OG_BASE="$BASE" OG_PORT="$PORT" "scripts/$script-smoke.sh" >/dev/null || fail "$script"
  echo "  passed"
done

# The tool path needs a door that actually reaches for a tool; the echoing one never does, so these
# two run against their own server. Without this they would exercise talking and never doing.
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
for _ in $(seq 1 20); do
  curl -fsS --max-time 1 "$BASE/health" >/dev/null 2>&1 || break
  sleep 1
done

step "starting a server with the tool-asking door"
OG_BIND="127.0.0.1:$PORT" \
OG_DATABASE_URL="$OG_DATABASE_URL" \
OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
OG_MODEL_DOOR=mock-tools \
RUST_LOG=warn \
./target/debug/opengrok >/dev/null 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 30); do
  curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && break
  sleep 1
done

for script in slice6-computer slice8-approval slice20-mcp-door; do
  step "scripts/$script-smoke.sh (tool door)"
  OG_BASE="$BASE" OG_PORT="$PORT" OG_MODEL_DOOR=mock-tools "scripts/$script-smoke.sh" >/dev/null \
    || fail "$script"
  echo "  passed"
done

# This one starts and kills its own servers, so the shared one must be out of the way first — and
# actually gone, not merely signalled: shutdown is graceful, so the port outlives the kill by a
# moment and the durability script would find a server it did not start.
kill "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
for _ in $(seq 1 20); do
  curl -fsS --max-time 1 "$BASE/health" >/dev/null 2>&1 || break
  sleep 1
done
curl -fsS --max-time 1 "$BASE/health" >/dev/null 2>&1 && fail "the shared server would not stop"
step "scripts/slice4-durability-smoke.sh"
OG_PORT="$PORT" scripts/slice4-durability-smoke.sh >/dev/null || fail "durability"
echo "  passed"

# Also starts and kills its own servers, and plants rows directly.
step "scripts/slice9-recovery-smoke.sh"
OG_PORT="$PORT" scripts/slice9-recovery-smoke.sh >/dev/null || fail "recovery"
echo "  passed"

# Also starts and kills its own servers — the SIGKILL mid-schedule is the point of it.
step "scripts/slice10-autonomy-smoke.sh"
OG_PORT="$PORT" scripts/slice10-autonomy-smoke.sh >/dev/null || fail "autonomy"
echo "  passed"

# Starts its own server too: it needs OG_GATEWAY_BEARER and OG_PUBLIC_GATEWAY_URL in its env.
step "scripts/slice13-seamb-smoke.sh"
OG_PORT="$PORT" scripts/slice13-seamb-smoke.sh >/dev/null || fail "seamb"
echo "  passed"

# Also its own server: it configures OG_PUBLIC_GATEWAY_URL + OG_GATEWAY_BEARER internally, and
# proves the browser login leg AND that the blind LAN token-mint hole is closed.
step "scripts/slice16-browser-login-smoke.sh"
OG_PORT="$((PORT + 3))" scripts/slice16-browser-login-smoke.sh >/dev/null || fail "browser-login"
echo "  passed"

# Own server + fresh DB: bootstraps an org via the CLI, then walks the full signup/login chain.
step "scripts/slice17-identity-smoke.sh"
OG_PORT="$((PORT + 4))" OG_DATABASE_URL="${OG_DATABASE_URL%/*}/opengrok_s17_gate" \
  scripts/slice17-identity-smoke.sh >/dev/null || fail "identity"
echo "  passed"

step "scripts/slice18-account-admin-smoke.sh"
OG_PORT="$((PORT + 5))" OG_DATABASE_URL="${OG_DATABASE_URL%/*}/opengrok_s18_gate" \
  scripts/slice18-account-admin-smoke.sh >/dev/null || fail "account-admin"
echo "  passed"

step "scripts/slice19-web-console-smoke.sh"
OG_PORT="$((PORT + 6))" OG_DATABASE_URL="${OG_DATABASE_URL%/*}/opengrok_s19_gate" \
  scripts/slice19-web-console-smoke.sh >/dev/null || fail "web-console"
echo "  passed"

echo
echo "GATE PASSED (checks, tests and every smoke script)."
