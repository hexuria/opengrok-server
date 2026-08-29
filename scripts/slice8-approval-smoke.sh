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

echo "5. the run is open in the log, and says what it is waiting for"
# `awaiting-approval` rather than `running`: a suspended run must be distinguishable from one that
# is still working, or nobody can tell which runs need a person.
replay=$(curl -fsS "$BASE/ag-ui/runs/$run_id" -H "authorization: Bearer $token")
status=$(echo "$replay" | jq -r '.status')
[ "$status" = "awaiting-approval" ] || fail "expected awaiting-approval, got $status"
echo "$replay" | jq -e '.pending.tool == "shell"' >/dev/null || fail "no pending call: $replay"
ok "the log says awaiting-approval, and names the call"

echo "6. the waiting call is findable, and says what it is asking for"
# A suspended run nobody can find is a run nobody will answer, which is the same as a lost one.
queue=$(curl -fsS "$BASE/ag-ui/approvals" -H "authorization: Bearer $token")
echo "$queue" | jq -e 'type == "array"' >/dev/null || fail "the queue is not an array"
entry=$(echo "$queue" | jq -c --arg r "$run_id" '.[] | select(.runId == $r)')
[ -n "$entry" ] || fail "the suspended run is not in the queue: $queue"
call_id=$(echo "$entry" | jq -r '.callId')
[ -n "$call_id" ] && [ "$call_id" != "null" ] || fail "no callId: $entry"
# A person asked to approve "shell" without seeing the command is approving nothing.
echo "$entry" | jq -e '.arguments.command | test("opengrok-tool-ran")' >/dev/null \
  || fail "the queue does not show what is being approved: $entry"
ok "queued: $(echo "$entry" | jq -r '.tool') with its arguments visible"

echo "7. answering twice runs the tool once"
# THE EXACTLY-ONCE PROPERTY. A retried request, a double-clicked button, two devices — all must
# converge on one answer.
first=$(curl -fsS -X POST "$BASE/ag-ui/runs/$run_id/answer" -H "authorization: Bearer $token" \
  -H 'content-type: application/json' -d "{\"call_id\":\"$call_id\",\"approved\":true}")
echo "$first" | jq -e '.alreadyAnswered == false' >/dev/null || fail "first answer: $first"

for _ in 1 2 3; do
  again=$(curl -fsS -X POST "$BASE/ag-ui/runs/$run_id/answer" -H "authorization: Bearer $token" \
    -H 'content-type: application/json' -d "{\"call_id\":\"$call_id\",\"approved\":true}")
  # A retry is not an error — it is the same answer arriving again, and it must be safe to send.
  echo "$again" | jq -e '.alreadyAnswered == true' >/dev/null \
    || fail "a repeated answer should report the settled state: $again"
done
ok "answered once; three retries reported the settled state instead of answering again"

echo "8. the server carries the run on by itself"
# NOBODY ASKS IT TO. No further /ag-ui request is sent below — the run finishes because the server
# picked it back up after the answer, which is the difference between a suspended run and a lost one.
finished=""
for _ in $(seq 1 40); do
  state=$(curl -fsS "$BASE/ag-ui/runs/$run_id" -H "authorization: Bearer $token" | jq -r '.status')
  if [ "$state" = "finished" ] || [ "$state" = "failed" ]; then finished="$state"; break; fi
  sleep 1
done
[ "$finished" = "finished" ] || fail "the answered run did not continue on its own (last: $state)"
ok "the run finished with no further request"

echo "8b. and the approved command actually ran on the coworker's box"
# The marker is written by the tool the person approved. Its absence would mean the run "continued"
# without doing the thing that was waiting.
seen=$(docker exec "$BOX_ID" cat /tmp/opengrok-tool-ran 2>/dev/null | tr -d '\n')
[ "$seen" = "opengrok-tool-ran" ] || fail "the approved command never ran (marker: '$seen')"
ok "the approved command ran, after the yes and not before"

echo "9. somebody else cannot answer this run"
other=$(curl -fsS "$BASE/auth/cursor_dev_session_token?plan=pro&email=nosy-$(date +%s)@og.local" | jq -r '.accessToken')
status=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/ag-ui/runs/$run_id/answer" \
  -H "authorization: Bearer $other" -H 'content-type: application/json' \
  -d "{\"call_id\":\"$call_id\",\"approved\":true}")
[ "$status" = "404" ] || fail "another account answered the run: $status"
ok "another account gets 404 — a run id is not a way in"

echo "10. approval is withdrawn, and the same tool runs"
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
echo "PASS — slice 8: a call that needs a person suspends the run, and one answer settles it."
