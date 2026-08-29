#!/usr/bin/env bash
# Proves PLAN §4.5 layer 5: some calls need a human yes, and the run SUSPENDS until it arrives.
#
# The distinction being tested is the whole point. A refusal ends a turn; an approval pauses one
# that can still be finished. So a suspended run must be neither finished nor failed, and the tool
# must not have run.
#
# Usage:  OG_PORT=1447 scripts/slice8-approval-smoke.sh
set -euo pipefail

PORT="${OG_PORT:-1337}"
BASE="${OG_BASE:-http://127.0.0.1:$PORT}"
fail() { echo "FAIL: $*" >&2; exit 1; }
ok()   { echo "  ok: $*"; }

command -v jq >/dev/null || fail "jq is required"
if ! docker version --format '{{.Server.Version}}' >/dev/null 2>&1; then
  echo "SKIPPED — no Docker daemon, so no computers and no tools to approve"
  exit 0
fi

BOX_ID=""
cleanup() { [ -n "$BOX_ID" ] && docker rm -f "$BOX_ID" >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "1. a coworker with a computer"
token=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=appr-$(date +%s)@og.local" | jq -r '.accessToken')
hired=$(curl -fsS -X POST "$BASE/coworkers" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"name":"Ada","with_computer":true}')
coworker=$(echo "$hired" | jq -r '.id')
BOX_ID=$(echo "$hired" | jq -r '.boxId')
[ "$BOX_ID" != "null" ] || fail "no computer: $hired"
ok "hired $coworker on $BOX_ID"

echo "2. shell is marked as needing a human yes"
set=$(curl -fsS -X POST "$BASE/coworkers/$coworker/approvals" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"tools":["shell"]}')
echo "$set" | jq -e '.needsApproval | index("shell")' >/dev/null || fail "not set: $set"
ok "shell now needs approval"

echo "3. a run that reaches for shell suspends instead of running it"
run_id="run-appr-$(date +%s)"
events=$(curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d "$(cat <<JSON
{"threadId":"t-appr","runId":"$run_id",
 "forwardedProps":{"coworkerId":"$coworker"},
 "messages":[{"id":"m1","role":"user","content":"run a command"}]}
JSON
)" --max-time 60 | sed -n 's/^data: //p')
[ -n "$events" ] || fail "no events"

last=$(echo "$events" | tail -1 | jq -r '.type')
name=$(echo "$events" | tail -1 | jq -r '.name // empty')
# NOT finished and NOT failed: the run is waiting, and the client is told so in a way it can render.
[ "$last" = "CUSTOM" ] && [ "$name" = "run-awaiting-approval" ] \
  || fail "expected a suspended run, got $last ($name)"
ok "the run suspended: $name"

echo "4. the tool result says waiting, not success"
# A pending approval that read as success is how a model concludes its command already worked.
result=$(echo "$events" | jq -c 'select(.type == "TOOL_CALL_RESULT")' | head -1)
[ -n "$result" ] || fail "no tool result"
echo "$result" | jq -e '.ok == false' >/dev/null || fail "a pending approval must not read as ok: $result"
echo "$result" | jq -e '.content | test("waiting for approval")' >/dev/null || fail "$result"
ok "the model was told it is waiting, not that it succeeded"

echo "5. the run is still open in the log, not finished"
replay=$(curl -fsS "$BASE/ag-ui/runs/$run_id" -H "authorization: Bearer $token")
status=$(echo "$replay" | jq -r '.status')
[ "$status" = "running" ] || fail "a suspended run should still be running, got $status"
ok "the log says running — it can still be picked up"

echo "6. approval is withdrawn, and the same tool runs"
curl -fsS -X POST "$BASE/coworkers/$coworker/approvals" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"tools":[]}' >/dev/null
run2="run-appr2-$(date +%s)"
events2=$(curl -sN -X POST "$BASE/ag-ui" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d "$(cat <<JSON
{"threadId":"t-appr","runId":"$run2",
 "forwardedProps":{"coworkerId":"$coworker"},
 "messages":[{"id":"m1","role":"user","content":"run a command"}]}
JSON
)" --max-time 60 | sed -n 's/^data: //p')
last2=$(echo "$events2" | tail -1 | jq -r '.type')
[ "$last2" = "RUN_FINISHED" ] || fail "after approval the run should finish, got $last2"
ok "with approval no longer required, the run completes"

echo
echo "PASS — slice 8: a call that needs a person suspends the run rather than ending it."
