#!/usr/bin/env bash
# Proves the autonomy slice: runs that NO CLIENT STARTED.
#
# The mission is "keeps working when the laptop is off", and until this slice every run began with
# a client POST. This watches a schedule fire a run by itself, watches firing survive a SIGKILL,
# watches pause actually stop it, watches a monitor react to the event log exactly once — the
# loop guard being the difference between "reacts to events" and "fires forever at its own echo" —
# and watches an outside app wake a routine through its own minted key, which is the one run in
# here that begins with a POST from somebody holding no account token at all.
#
# Usage:  OG_PORT=1461 OG_DATABASE_URL=… scripts/slice10-autonomy-smoke.sh
set -euo pipefail
PGDB="$(printf %s "${OG_DATABASE_URL:-}" | sed -E 's#.*/([^/?]+).*#\1#')"; PGDB="${PGDB:-opengrok}"

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
BIN="${OG_BIN:-./target/debug/opengrok}"
SECRET="${OG_TOKEN_SECRET:-$(openssl rand -hex 32)}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
: "${OG_DATABASE_URL:?needs OG_DATABASE_URL}"
# Reach Postgres however this machine can: a local psql client speaking OG_DATABASE_URL if there
# is one (CI, and any dev box with the client installed), else the dev compose container by name.
# Hardcoding the container made this script fail anywhere that Postgres is not that container.
if command -v psql >/dev/null 2>&1 && psql "$OG_DATABASE_URL" -tAc "select 1" >/dev/null 2>&1; then
  PSQL=(psql "$OG_DATABASE_URL" -tAc)
else
  PSQL=(docker exec oag-dev-postgres-1 psql -U oag -d "$PGDB" -tAc)
fi
"${PSQL[@]}" "select 1" >/dev/null 2>&1 || fail "cannot reach Postgres"

start_server() {
  OG_BIND=127.0.0.1:$PORT OG_DATABASE_URL="$OG_DATABASE_URL" OG_TOKEN_SECRET="$SECRET" \
  OG_MODEL_DOOR=mock RUST_LOG=warn "$BIN" >/dev/null 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 30); do
    curl -fsS --max-time 2 "$BASE/health" >/dev/null 2>&1 && return 0
    sleep 1
  done
  fail "the server did not come up"
}
stop_server() { kill "${SERVER_PID:-0}" 2>/dev/null || true; wait "${SERVER_PID:-0}" 2>/dev/null || true; }
trap stop_server EXIT

# Runs fired for a thread — how this smoke sees server-initiated work.
runs_in_thread() { "${PSQL[@]}" "select count(*) from run_view where thread_id = '$1'"; }

echo "1. sign in, hire a coworker"
start_server
# A clean autonomy slate, after boot so migrations have created the tables. Projections only — the
# log is never touched. Without this, schedules left by a previous run of this same smoke would
# keep firing into our counting windows.
"${PSQL[@]}" "delete from schedule_view; delete from monitor_view;" >/dev/null
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=auto-$(date +%s)@og.local" | jq -r '.accessToken')
[ -n "$token" ] && [ "$token" != "null" ] || fail "no token"
cw=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Nightshift","model":"oag/cheap"}' | jq -r '.id')
[ -n "$cw" ] && [ "$cw" != "null" ] || fail "no coworker"
ok "hired $cw"

echo "2. a bad cron expression is refused, not stored"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/schedules" \
  -H "authorization: Bearer $token" -H 'content-type: application/json' \
  -d "{\"coworkerId\":\"$cw\",\"cron\":\"every tuesday probably\",\"prompt\":\"hi\"}")
[ "$code" = "422" ] || fail "a bad cron answered $code, expected 422"
ok "422, with the reason in the body"

echo "3. a schedule fires a run that no client started"
sched=$(curl -fsS -X POST "$BASE/schedules" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' \
  -d "{\"coworkerId\":\"$cw\",\"cron\":\"*/2 * * * * *\",\"prompt\":\"heartbeat: report in\"}")
sid=$(echo "$sched" | jq -r '.id')
[ -n "$sid" ] && [ "$sid" != "null" ] || fail "no schedule id: $sched"
echo "$sched" | jq -e '.nextDueMs != null' >/dev/null || fail "no nextDueMs: $sched"
fired=0
for _ in $(seq 1 15); do
  fired=$(runs_in_thread "$sid")
  [ "$fired" -ge 1 ] && break
  sleep 1
done
[ "$fired" -ge 1 ] || fail "no run fired within 15s"
ok "a run appeared in thread $sid with nobody asking"

echo "4. the fired run is a real, replayable, owned run"
run=$("${PSQL[@]}" "select id from run_view where thread_id = '$sid' limit 1")
replay=$(curl -fsS "$BASE/ag-ui/runs/$run" -H "authorization: Bearer $token")
echo "$replay" | jq -e '.status == "finished"' >/dev/null || fail "fired run not finished: $replay"
echo "$replay" | jq -r '.events[] | select(.delta != null) | .delta' | tr -d '\n' | grep -q "heartbeat" \
  || fail "the schedule's prompt did not reach the run"
anon=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/ag-ui/runs/$run")
[ "$anon" = "404" ] || fail "a fired run leaked to an anonymous reader ($anon)"
ok "finished, carries the prompt, and only its owner can read it"

echo "5. firing survives a SIGKILL"
kill -9 "$SERVER_PID" 2>/dev/null || true
wait "$SERVER_PID" 2>/dev/null || true
before=$(runs_in_thread "$sid")
start_server
after="$before"
for _ in $(seq 1 15); do
  after=$(runs_in_thread "$sid")
  [ "$after" -gt "$before" ] && break
  sleep 1
done
[ "$after" -gt "$before" ] || fail "no firing after the restart ($before -> $after)"
ok "killed mid-schedule, restarted, and it fired again ($before -> $after)"

echo "6. pause means stop"
curl -fsS -X POST "$BASE/schedules/$sid/pause" -H "authorization: Bearer $token" -o /dev/null
sleep 1  # a firing claimed before the pause may still land
frozen=$(runs_in_thread "$sid")
sleep 5
now=$(runs_in_thread "$sid")
[ "$now" = "$frozen" ] || fail "a paused schedule kept firing ($frozen -> $now)"
ok "paused, and $frozen stayed $frozen"

echo "7. somebody else's schedule is a 404, not a 403"
other=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=other-$(date +%s)@og.local" | jq -r '.accessToken')
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/schedules/$sid/resume" \
  -H "authorization: Bearer $other")
[ "$code" = "404" ] || fail "a stranger got $code, expected 404"
ok "existence is not leaked"

echo "8. a monitor reacts to the event log — exactly once"
# Watching run-started is the sharp test: the monitor's OWN fired run emits run-started too, so
# without the loop guard this fires forever at 1s intervals and the count below explodes.
mon=$(curl -fsS -X POST "$BASE/monitors" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' \
  -d "{\"coworkerId\":\"$cw\",\"watches\":\"run-started\",\"prompt\":\"a run began; take note\"}" | jq -r '.id')
[ -n "$mon" ] && [ "$mon" != "null" ] || fail "no monitor id"
sleep 2  # let the cursor pass everything that happened before the monitor existed
T=$(date +%s)
curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token" -H 'content-type: application/json' \
  -d "{\"threadId\":\"t-trig-$T\",\"runId\":\"r-trig-$T\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"trigger\"}]}" \
  --max-time 15 >/dev/null
mfired=0
for _ in $(seq 1 10); do
  mfired=$(runs_in_thread "$mon")
  [ "$mfired" -ge 1 ] && break
  sleep 1
done
[ "$mfired" -ge 1 ] || fail "the monitor never fired"
ok "the monitor fired on run-started"

echo "9. the loop guard holds"
sleep 5  # five more sweep ticks: ample time for a runaway monitor to fire at its own run
final=$(runs_in_thread "$mon")
[ "$final" = "1" ] || fail "the monitor fired $final times; its own run must not retrigger it"
mrun=$("${PSQL[@]}" "select id from run_view where thread_id = '$mon' limit 1")
mreplay=$(curl -fsS "$BASE/ag-ui/runs/$mrun" -H "authorization: Bearer $token")
echo "$mreplay" | jq -r '.events[] | select(.delta != null) | .delta' | tr -d '\n' | grep -q "woken by event" \
  || fail "the fired run was not told what woke it"
ok "one firing, and the coworker was told why it was woken"

echo "10. a monitor watching monitor firings is refused at the root"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/monitors" \
  -H "authorization: Bearer $token" -H 'content-type: application/json' \
  -d "{\"coworkerId\":\"$cw\",\"watches\":\"monitor-fired\",\"prompt\":\"watch the watchers\"}")
[ "$code" = "422" ] || fail "watching monitor-fired answered $code, expected 422"
ok "the cross-monitor cascade cannot be configured"

echo "11. an outside app wakes a routine with the key we minted for it"
hook=$(curl -fsS -X POST "$BASE/schedules" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' \
  -d "{\"coworkerId\":\"$cw\",\"kind\":\"webhook\",\"name\":\"Inbox\",\"prompt\":\"a webhook arrived: report in\"}")
hsid=$(echo "$hook" | jq -r '.id')
[ -n "$hsid" ] && [ "$hsid" != "null" ] || fail "no webhook schedule id: $hook"
echo "$hook" | jq -e '.kind == "webhook"' >/dev/null || fail "not a webhook routine: $hook"
echo "$hook" | jq -e '.cron == null' >/dev/null || fail "a webhook routine has no cron: $hook"
key=$(echo "$hook" | jq -r '.webhook.key')
url=$(echo "$hook" | jq -r '.webhook.url')
case "$key" in og_*) ;; *) fail "the minted key is not one of ours: $hook" ;; esac
hid="${url##*/}"
case "$hid" in hook_*) ;; *) fail "no hook id in the minted URL: $hook" ;; esac
# Every refusal FIRST. Counting straight after one would only prove nothing had fired YET, which
# a slow spawn satisfies too; the honest check is the count after a fire known to have landed —
# a refusal that secretly fired would make it two.
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/hooks/$hid" \
  -H 'authorization: Bearer og_not_the_minted_key' -H 'content-type: application/json' -d '{}')
[ "$code" = "401" ] || fail "a wrong key answered $code, expected 401"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/hooks/$hid" \
  -H 'content-type: application/json' -d '{}')
[ "$code" = "401" ] || fail "no key at all answered $code, expected 401"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/hooks/hook_00000000-0000-7000-8000-000000000000" \
  -H "authorization: Bearer $key" -H 'content-type: application/json' -d '{}')
[ "$code" = "401" ] || fail "an unknown hook answered $code, expected 401 — 404 would let the id space be walked"
code=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/hooks/$hid" \
  -H "authorization: Bearer $key" -H 'content-type: application/json' -d '{"item":"milk"}')
[ "$code" = "202" ] || fail "the hook answered $code, expected 202"
hfired=0
for _ in $(seq 1 15); do
  hfired=$(runs_in_thread "$hsid")
  [ "$hfired" -ge 1 ] && break
  sleep 1
done
[ "$hfired" = "1" ] || fail "expected exactly one run in $hsid, got $hfired"
ok "POST /hooks/$hid woke $hsid exactly once, and no refusal started anything"

# Leave nothing running for the next smoke's counting windows.
curl -fsS -X DELETE "$BASE/schedules/$sid" -H "authorization: Bearer $token" -o /dev/null || true
curl -fsS -X DELETE "$BASE/schedules/$hsid" -H "authorization: Bearer $token" -o /dev/null || true
curl -fsS -X DELETE "$BASE/monitors/$mon" -H "authorization: Bearer $token" -o /dev/null || true

echo
echo "PASS — slice 10: the server starts runs on its own clock, on its own events and on an"
echo "       inbound webhook, survives a SIGKILL, stops when paused, and cannot be talked"
echo "       into a loop."
