# Evidence: the PolicyApproval card, on the packaged app

Slice 16.later Part A (PR #17). Captured 2 Sep 2026 against the packaged
`/Applications/Open Grok.app` on the dev server (`main`), driven over CDP. That client was removed
on 20 Sep 2026; the card and the `xai/grok-4.6` model note below still describe the server.

## What was done

1. Grant `shell` as needs-a-human-yes on a coworker:
   `POST /coworkers/{id}/approvals {"tools":["shell"]}` → `grant_view.needs_approval = {"only":["shell"]}`.
2. Ask the coworker in chat to run a shell command on its box.
3. The run suspends with `SuspendReason::PolicyApproval`; the transcript gets an
   `auto-review-approval` card.
4. Press **Allow once**; `resolveAutoReviewApproval` returns 200; the run resumes
   and the command executes.

## What the card shows (`card-pending.png`)

> **Approval needed** — Runs on Grok Bot's computer
> Command on the agent's own box: `date +%s%N; hostname`
> **running shell on coworker cw_… needs a human yes**
> [ Show the command ] [ Allow once ] [ Always allow ] [ Deny ]

The bold line is the grant's own reason (`Decision::reason()`), rendered by the
client's `auto-review-approval` view under the summary
(`frontend/src/recovered/features/conversation/cards/transcript-card/views/auto-review-approval.tsx`
renders `approval.reason` as a paragraph). No proposed rule is offered, so
"Always allow" is a plain approve — a policy grant is widened in policy, never
from a card.

`card-approved.png` is the same conversation after Allow once, the run resumed.

## Model note

The coworker used for this capture was pinned to `xai/grok-4.6` — which is why
that route became the shipped default on 25 Sep 2026 (#197). The then-default
`gpt-5.6-luna` route does not emit tool calls through the gateway (it answers a
shell request from a text message with fabricated output; zero `TOOL_*` events
across its runs), so the policy gate — which is only reached on a real
`RunTool` call — is never exercised with it. The card path is otherwise proven
by `tests/against_the_mcp_door.rs` and the `cards` unit tests.

## What this evidence does not show (25 Sep 2026)

It stops at "the command executes". Until #187 the continuation after
**Allow once** was rebuilt from the run's emitted frames alone: the model was
handed the system message and a bare tool output, with neither the person's
request nor the call it had allowed, so the answer after the yes was not
grounded in anything. The rebuild now opens with the journaled request and
names the allowed call before its result (`agui/history.rs`
`conversation_from`); `tests/against_a_no_on_the_desktop_card.rs`
`an_allowed_card_continues_with_the_request_it_was_asked` asserts it on the
mock door's captured request. **Still owed:** a re-capture on a live door
showing the grounded answer after Allow once (CLAUDE.md #10).
