# Auto-review — end-to-end verification (server half)

Driven against the shipped path: the packaged desktop client (the peer's CDP session, screenshots
in the client repo's `docs/consent-model-B5-acceptance.md`) talking to this server on `:1447`,
the real gateway door and the real judge route (`OG_AUTO_REVIEW_MODEL` = the deployment's
`OG_MODEL`), Postgres `opengrok_web_verify`. The rows below are what the server recorded; the
client's transcript and screenshots are the other half of each pair.

Account `acct_01a0551e-29ad-74b3-b1d6-236a8122d6d8`, coworker "New Bot"
`cw_01a0562a-5e9c-76f1-9421-11d2d60e3b1d`. Design: `docs/AUTO-REVIEW.md`.

## 1. A block instruction refuses a matching command, naming the rule — REAL JUDGE

Client side: per-agent policy set through the agent-settings section — `enabled: true`,
`blockInstructions: "anything that installs software or changes system settings"`;
`GET /auto-review/effective?coworkerId=…` read back `decidedBy.enabled = coworker`,
`decidedBy.blockInstructions = coworker`, `allowInstructions` from `default`. Prompt at 21:09
Manila: *"On your computer, run: brew install jq"*.

Server side (`events`, stream `run/run_01a057f0-3770-7772-89d0-58c2ca26f4d8`, `stream_seq 6`,
`occurred_at 2026-08-31 13:09:21 UTC`):

```json
{"type":"TOOL_CALL_RESULT","ok":false,"toolCallId":"call_rJ8D7XofHSv9IOf9qfmVSXQw",
 "content":"refused: auto-review blocked this — your block instructions say: \"anything that installs software or changes system settings\""}
```

No `run-suspended` event on that stream — nothing was dispatched and no card was raised.
`gateway_entry` for the coworker: `seq 75` the prompt, `seq 76`
(`e_01a057f0-3742-75c2-a993-f3df83e0346e`) the bot's reply:

> I couldn't run `brew install jq` on your Mac because the local-execution policy blocked
> software installation.

One transcript row after the prompt, the rule named, the run finished normally. The policy row
was deleted afterwards through `DELETE /auto-review/policy {scopeKind:"coworker", …}` and
`auto_review_policy` reads empty for the account — the reset landed.

## 2. An ask raises exactly one card; the answer flips it in place; "Always" writes the coworker tier

Driven in a deterministic window: `:1447` relaunched with `OG_MODEL_DOOR=mock-tools` (every turn
asks for one `shell` call on the bot's box) and `OG_AUTO_REVIEW_MOCK_VERDICT=ask`, the policy on
with one instruction so the judge is not short-circuited.

Client side: one prompt → **exactly one** `auto-review-approval` card (one "Allow once" button
counted): `surface: box_shell`, `command: echo opengrok-tool-ran > /tmp/opengrok-tool-ran`,
`reason: "Your auto-review instructions did not clearly allow this, so it is being asked rather
than allowed."`, buttons Allow once / Always allow / Deny. Allow once → the card settled as
"Allowed once" on the same entry, the run resumed, the mock reported the command ran. Screenshot
in the client repo (`docs/consent-model-evidence/b4-autoreview-card.png`).

Server side, first card (`events`, stream `run/run_01a057f5-0f95-7a12-8952-be2080a0a93d`):

| stream_seq | event | occurred_at (UTC) | payload |
|---|---|---|---|
| 7 | run-emitted | 13:14:33.585 | `TOOL_CALL_ARGS {"command":"echo opengrok-tool-ran > /tmp/opengrok-tool-ran"}` `toolCallId: mock-call-1` |
| 10 | run-emitted | 13:14:33.657 | `CUSTOM run-awaiting-approval` `callId: mock-call-1` `reason: auto-review` |
| 11 | run-suspended | 13:14:33.657 | `reason: "auto-review"` `call_id: mock-call-1` |
| 12 | run-answered | 13:15:16.833 | `approved: true` `by: acct_01a0551e-…` |

`gateway_entry` seq 79, `e_01a057f5-1175-70b2-b6d3-cc45ff351cae`: `approval.requestId
mock-call-1`, `surface box_shell`, `status` now `approved` — the same entry id the pending card was
appended under, flipped in place (`set_gateway_approval_status`, status-only `jsonb_set`).

Second card, the "Always allow" press: stream `run/run_01a057f6-1fe8-7d10-a5c8-ba68454d16d2`,
`run-suspended` 13:15:43.325 → `run-answered approved` 13:15:46.742; `gateway_entry` seq 83
`e_01a057f6-217b-7e13-b938-db162b7c808e` → `approved`. The verb saw `approved` (the wire has no
"always"; the client's Always is its own append, then `approved`) — as transcribed.

### Finding: the pinned client's "Always allow" lands on the global tier

After the Always press, `GET /auto-review/effective?coworkerId=…` read
`decidedBy.allowInstructions = global`, and the coworker row stayed null. Cause is client-side:
the shipped 0.18 renderer's auto-review card has **no `proposedRule` handling** (zero occurrences
in the bundle) — its "always" appends to the local `autoReviewInstructions` store, which the
General-tab mirror writes to the **global** tier. The server sends `proposedRule` exactly as
transcribed; the pinned client ignores it. The per-agent widget on the agent-settings screen
writes the coworker tier correctly (verified). Rerouting the card's Always needs a surgical patch
of the minified card, which the peer declined to attempt blind. **Open product decision for the
user:** accept global-scoped card-Always for v1 (the widget covers per-agent), or schedule the
card patch. Nothing on the server changes either way.

The window was closed afterwards: `:1447` relaunched on the real judge; global auto-review reset
to off/empty and the coworker row deleted.

## What was NOT proven by this run

- The real judge answering `ask` (item 2 used the canned verdict). The parsing and fail-closed
  ladder are unit-tested (`opengrok-harness/src/review.rs`, `opengrok-tools/src/review.rs`); a
  real "ask" is the same code path with a different one-word input.
- A card pressed after a server restart. Covered by `tests/against_auto_review_gate.rs`'s
  "answered exactly once" test, which resolves a run that exists only in Postgres, never in
  process memory — the same situation a restart leaves.
