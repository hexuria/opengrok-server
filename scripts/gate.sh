#!/usr/bin/env bash
# Everything CI runs, run here.
#
# This exists because CI is not always available — a billing lapse, an outage, a plane — and a
# project whose only gate is a hosted runner has no gate on those days. It is deliberately the SAME
# commands in the SAME order as .github/workflows/ci.yml: if the two drift, the local ritual stops
# predicting the remote one and people stop trusting either.
#
# Usage:
#   scripts/gate.sh --checks     # checks only: fmt, sizes, architecture, deny, formal, clippy
#   scripts/gate.sh              # checks and tests
#   scripts/gate.sh --smoke      # also stands the server up and runs the smoke scripts
set -euo pipefail

cd "$(dirname "$0")/.."

step() { printf '\n=== %s\n' "$*"; }
fail() { echo "GATE FAILED: $*" >&2; exit 1; }

# THE DATABASE HAS TO EXIST, and this check is here because its absence LIES. A run against a
# database that is not there does not fail with a word about databases — it fails deep in the
# integration tests with four red test names, so the tail reads "GATE FAILED: tests" and the next
# person spends their time reading four tests that are perfectly fine. Cost two full runs to learn.
# It sits ABOVE `cargo test` for that reason; the side databases further down are created by the
# smokes themselves, and this one is the caller's. psql is not required to run the gate, so a
# machine without it skips the check rather than failing on it.
if [ -n "${OG_DATABASE_URL:-}" ] && command -v psql >/dev/null 2>&1; then
  if ! psql "$OG_DATABASE_URL" -c 'select 1' >/dev/null 2>&1; then
    fail "cannot reach the database ${OG_DATABASE_URL##*/} — create it first (createdb ${OG_DATABASE_URL##*/}) or point OG_DATABASE_URL at one that exists. This is the environment, not the code."
  fi
fi

step "cargo fmt --all --check"
cargo fmt --all --check || fail "formatting (run: cargo fmt --all)"

step "scripts/crate-size.sh"
scripts/crate-size.sh || fail "crate size (a crate grew past its ceiling)"

step "scripts/check-architecture.sh"
scripts/check-architecture.sh || fail "architecture (a crate edge scripts/architecture.txt does not allow)"

# cargo-deny is CI's `supply-chain` job. It is optional here because it reads the RustSec
# advisory feed over the network; scripts/install-ci-tools.sh installs the pinned binary.
if command -v cargo-deny >/dev/null 2>&1; then
  step "cargo deny check"
  cargo deny check --hide-inclusion-graph || fail "cargo deny (see deny.toml for how an ignore is justified)"
else
  step "cargo deny check: skipped, cargo-deny is not installed (scripts/install-ci-tools.sh)"
fi

# CI runs this as its own `formal` job; here it runs when the pinned tools are installed
# (scripts/install-tla.sh, scripts/install-lean.sh, and a JVM), and says so when they are not.
if [ -f "$HOME/.local/tla/tla2tools.jar" ] && command -v java >/dev/null 2>&1; then
  step "scripts/formal.sh"
  scripts/formal.sh || fail "formal models (formal/README.md says what each configuration shows)"
else
  step "scripts/formal.sh: skipped, TLC is not installed (scripts/install-tla.sh)"
fi

step "cargo check --workspace --all-targets"
cargo check --workspace --all-targets || fail "check"

# CLAUDE.md names this build as one that must stay clean (one reqwest, one hyper), and no gate
# ran it, so it could break unseen.
step "cargo check -p opengrok --no-default-features"
cargo check -p opengrok --no-default-features || fail "check without default features"

step "cargo clippy --workspace --all-targets -- -D warnings"
cargo clippy --workspace --all-targets -- -D warnings || fail "clippy"

# CI's `checks` suite, which is what a push to main runs: its pull request already ran the tests
# on this same merge result, and the nightly run repeats them (docs/setup/gate.md, "CI suites").
if [ "${1:-}" = "--checks" ]; then
  echo
  echo "GATE PASSED (checks only). Drop --checks to also run the tests."
  exit 0
fi

# nextest runs every test binary at once instead of one after another, which is most of what the
# test step spent. It does not run doctests, so those keep cargo test. Without nextest installed
# the gate falls back to cargo test, which runs the same tests.
if command -v cargo-nextest >/dev/null 2>&1; then
  step "cargo nextest run --workspace"
  cargo nextest run --workspace --no-fail-fast || fail "tests"
  step "cargo test --workspace --doc"
  cargo test --workspace --doc || fail "doctests"
else
  step "cargo test --workspace"
  cargo test --workspace || fail "tests"
fi

if [ "${1:-}" != "--smoke" ]; then
  echo
  echo "GATE PASSED (checks and tests). Add --smoke to also run the smoke scripts."
  exit 0
fi

# THE SMOKES RUN ./target/debug/opengrok, AND NOTHING ABOVE BUILDS IT. `cargo test --workspace`
# compiles the crates' test harnesses, not this package's binary, so a local gate used to smoke
# whatever binary the developer last built — found 2 Sep 2026 when a box carried no run tag
# because the server was 40 minutes older than the code. CI always built first (ci.yml); now
# the script does too, so the two agree.
step "cargo build -p opengrok"
cargo build -p opengrok || fail "build"

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
# Every Docker box a gate server creates carries this run's tag, and the trap below removes them
# all on exit — passing or failing. Learned from 192 orphaned `sleep infinity` containers on the
# dev Mac (2 Sep 2026): the tool-door smokes remove their own boxes, but every other hire on a
# Docker deployment made one too, and nothing ever collected them.
export OG_BOX_RUN_TAG="gate-$$"
cleanup_boxes() {
  if command -v docker >/dev/null 2>&1; then
    ids=$(docker ps -aq --filter "label=dev.opengrok.run=$OG_BOX_RUN_TAG" 2>/dev/null || true)
    if [ -n "$ids" ]; then
      echo "=== removing $(echo "$ids" | wc -l | tr -d ' ') box(es) this gate created"
      # shellcheck disable=SC2086
      docker rm -f $ids >/dev/null 2>&1 || true
    fi
  fi
}

# Every smoke signs in through the password-free dev route, which a server mints from only when
# told to (OG_DEV_SIGN_IN, docs/setup/environment.md). Exported so the smokes that start their
# own servers inherit it; each of those also sets it on its own launch line to run standalone.
export OG_DEV_SIGN_IN=1

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
# The gate names its own deployment route rather than inheriting the code's. slice5, slice14 and
# slice22 hire on xai/grok-4.6 and prove a run with no coworker did NOT borrow that pin — which
# nothing can observe once the deployment default is the same route, as the code's now is.
OG_BIND="127.0.0.1:$PORT" \
OG_DATABASE_URL="$OG_DATABASE_URL" \
OG_TOKEN_SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}" \
OG_MODEL_DOOR=mock \
OG_MODEL=gate/deployment-default \
OG_DEV_SIGN_IN=1 \
RUST_LOG=warn \
./target/debug/opengrok >/dev/null 2>&1 &
SERVER_PID=$!
trap 'kill "$SERVER_PID" 2>/dev/null || true; cleanup_boxes' EXIT

for _ in $(seq 1 30); do
  curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 || fail "the server did not come up"

for script in slice1-auth slice2-agui slice3-harness slice5-roster slice7-policy slice14-botkey slice22-model-pins; do
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
OG_DEV_SIGN_IN=1 \
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

# Also its own server: it configures OG_PUBLIC_GATEWAY_URL internally, and
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

# Own server + fresh DB + its own stand-in gateway: the org-key surface boots with a gateway admin
# connection, which the shared server above deliberately has not got.
step "scripts/slice21-org-keys-smoke.sh"
OG_PORT="$((PORT + 7))" OG_DATABASE_URL="${OG_DATABASE_URL%/*}/opengrok_s21_gate" \
  scripts/slice21-org-keys-smoke.sh >/dev/null || fail "org-keys"
echo "  passed"

echo
echo "GATE PASSED (checks, tests and every smoke script)."
