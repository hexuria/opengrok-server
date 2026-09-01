# Evidence: the PolicyApproval card, on the packaged app

Slice 16.later Part A (PR #17). Captured 2 Sep 2026 against the packaged
`/Applications/Open Grok.app` on the dev server (`main`), driven over CDP.

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

The coworker used for this capture was pinned to `xai/grok-4.6`. The default
`gpt-5.6-luna` route does not emit tool calls through the gateway (it answers a
shell request from a text message with fabricated output; zero `TOOL_*` events
across its runs), so the policy gate — which is only reached on a real
`RunTool` call — is never exercised with it. The card path is otherwise proven
by `tests/against_the_mcp_door.rs` and the `cards` unit tests.
