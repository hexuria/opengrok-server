# PRD: Auto-steer (visible workstreams + harness delivery)

Status: draft for review  
Branch: `auto-steer`  
Repos: `hexuria/opengrok-server` (contracts + this PRD) · `hexuria/nativechat` (UI/UX implementation)  
Audience: implementers and reviewing agents  
Related research: conversation with Prime (2026-10-05) on queue / steer / interrupt; Cursor Agent docs; OpenGrok stop + pending paths

---

## 1. Problem

NativeChat today collapses the conversation into **one busy bit**.

- While a turn runs, the send control becomes stop, or the next message is queued / force-steered.
- Mid-turn “steer” in OpenGrok is often **stop run R1 + start run R2** plus a `STEER_CONTINUATION` splice, not inject-into-the-same-run.
- When the user starts job X then job Y, then says “fix X” or “stop Y”, the UI does not show **which** job a bubble belongs to, and Kill is not reachable without scrolling to the start of X.

Grok Bot’s useful pattern is different: the main chat stays free; work runs on named workers; each follow-up is **routed** (queue / steer / interrupt) to the right worker.

We want that UX on NativeChat, with OpenGrok owning durable run identity and stop, without pretending every model has a native steer API.

## 2. Goals

1. **Uninterrupted chat.** The composer stays usable while work runs. Send is not Kill.
2. **Visible workstreams.** Every in-flight job is labeled and distinguishable in the UI.
3. **Per-workstream Kill.** Always reachable without scrolling the transcript (sticky rail + live reply).
4. **Routed follow-ups.** Reply-to (or explicit target) decides which workstream gets the message.
5. **Honest delivery words.** Document queue / steer / interrupt as **harness policies**, not model features.
6. **Phased delivery.** Ship UI routing on today’s stop+queue first; add same-run inject later.

## 3. Non-goals (v1)

- Full Grok Bot multi-executor fan-out inside OpenGrok (many child processes per conversation).
- Claiming Cursor-identical same-run steer on day one.
- Aborting in-flight box recipes that the pinned exec path cannot cancel (do not lie in `takesEffect`).
- Putting Kill only on the original user bubble that started the job.
- Renaming today’s stop+restart path to “steer” in user-facing copy without changing harness semantics.

## 4. Terms (one meaning each)

| Term | Meaning |
|---|---|
| **Conversation** | OpenGrok thread / NativeChat chat. |
| **Workstream** | One user-visible job inside a conversation. v1 maps 1:1 to an OpenGrok **run** (`run_id`). |
| **Delivery** | What happens to a message aimed at a busy workstream: **queue**, **steer**, or **interrupt**. |
| **Queue** | Hold the message until that workstream (or conversation policy) is idle; then start the next turn. |
| **Steer** | Keep direction for the *same* job. **Target (later):** inject at next safe tool/model boundary without ending the run. **Today’s OpenGrok stand-in:** stop R1 + new R2 + splice (must be labeled “coarse steer” in eng docs until inject ships). |
| **Interrupt / stop** | End this workstream’s run via durable stop. In-flight tool may finish (`takesEffect: next-step`). |
| **Reply thread** | UI fork under a bubble used for routing. Not the same word as “workstream.” |

## 5. User stories

1. As a user, I start “do X” then “do Y” without waiting; both show as active workstreams.
2. As a user, I reply under X’s spine with “update X”; only X receives it.
3. As a user, I tap Kill on Y in the sticky rail while scrolled to the bottom; only Y stops.
4. As a user, I keep typing in the main composer; nothing becomes a global stop button.
5. As a reviewer, I can tell which assistant bubbles belong to X vs Y by label (and optional color).

## 6. UX design (NativeChat owns this)

### 6.1 Sticky workstream rail

- Show every **active** workstream: short label, status (`running` / `waiting for you` / `stopping`), **Kill**.
- Rail stays visible while any workstream is active (top or side; implementation choice).
- Tapping a chip scrolls to / focuses that workstream’s latest bubble.
- Kill calls existing stop for that `run_id` (see server).

### 6.2 Labels on bubbles

- Every user and assistant message in a workstream shows the same workstream label (e.g. `X`).
- Optional color accent; label text is required (color alone is not enough).
- Streaming indicator only on the workstream currently producing tokens.

### 6.3 Kill placement (all target the same `run_id`)

1. Sticky rail chip (primary; solves scroll-away).
2. Latest assistant bubble for that workstream while it is live.
3. Optional header on the reply-thread spine.

Do **not** require Kill only on the original “do X” user message.

### 6.4 Routing

| User action | Target |
|---|---|
| Plain send, no reply target | Main conversation: new workstream **or** idle small-talk policy (product choice; default = new workstream when intent is a task). |
| Reply under workstream X | Delivery aimed at X only. |
| Explicit “stop Y” / Kill on Y | Interrupt Y only. |
| Text “update X” without reply | Best-effort: if exactly one active workstream matches, route there; else ask (widget) or treat as main. v1 may require reply-to for safety. |

### 6.5 Composer

- Send never morphs into Stop.
- Optional per-message delivery override later (queue vs steer), similar to Cursor New Messages + opposite chord. v1 can hardcode: reply-to busy workstream → steer-policy; plain follow-up while one run busy → queue on that run or conversation pending (match current pending API until per-run queue exists).

### 6.6 Example timeline

| Message | Workstream | UI |
|---|---|---|
| A: do X | starts **X** | rail `X · running [Kill]`; bubbles tagged X |
| B: do Y | starts **Y** | rail `Y · running [Kill]`; bubbles tagged Y |
| C: update X (reply under X) | routes to **X** | Kill still on X chip |
| D: stop Y / Kill Y | ends **Y** | Y chip clears; X continues |

## 7. Server design (opengrok-server)

### 7.1 Keep (v1)

- Durable `POST /ag-ui/runs/{id}/stop` → `RunEvent::Stopped`; harness checks `journal.stopped` at step boundaries; honest `takesEffect`.
- Conversation-level pending user messages (`agui/pending.rs`) until per-workstream queue exists.
- Parked HITL: new message supersedes parked run without `/stop` first (`interrupt_parked_hitl`).
- Do not kill Tokio/SSE task as stop.

### 7.2 Add (phased)

**Phase A — visibility contracts (unblock NativeChat UI)**

- Ensure every streamed event / transcript row that clients need for labeling exposes stable `runId` (already true for AG-UI runs; audit NativeChat paint paths).
- Document that a conversation may have **multiple non-terminal runs** over time; UI may show more than one “active” if we allow parallel runs.

**Phase B — parallel workstreams (product decision)**

- Today a conversation often has one live run. To match “do X and do Y”:
  - **Option B1 (preferred for PRD):** allow multiple concurrent runs per conversation (or per coworker), each a workstream. Define cap and fairness.
  - **Option B2:** multiplex in the client only (serialize on server, fake parallelism). Reject for this PRD; it breaks Kill and steer semantics.

**Phase C — same-run steer (true harness steer)**

- Durable **pending-steer** buffer on the run (append-only).
- Harness: after tool results (and/or before next model call), drain pending-steer into the message list **without** `Ending::Stop`.
- Keep Stop as a separate control.
- Models do not need a steer API; the harness fakes steer by message injection timing.

**Phase D — optional hard interrupt of box work**

- Only if product accepts partial tool outcomes: wire researched `POST /boxes/{id}/interrupt` (or equivalent) into stop path carefully with `open_tools` accounting. Not required for UI v1.

### 7.3 API sketch (Phase C; refine in implementation)

```text
POST /ag-ui/runs/{run_id}/steer
  body: { content, clientMessageId? }
  effect: append pending-steer; 202 { accepted: true, deliversAt: "next-boundary" }

POST /ag-ui/runs/{run_id}/stop
  (existing)

GET /ag-ui/threads/{id}/workstreams  (optional)
  → [{ runId, label?, status, startedAtMs }]
```

Exact paths may follow existing AG-UI style; this is intent, not frozen URL law.

## 8. NativeChat implementation notes

Primary code today:

- `src/send_policy.rs` — `OnSend`, `SendPlan`, `plan_send`
- `src/state.rs` — `take_running_turn_for_steer`, stop-before-post, `drain_queued_send`, `settle_parked_cards`

Changes (direction):

1. Introduce `Workstream` client model keyed by `run_id` (+ display label).
2. Rail + bubble chrome bind to workstreams.
3. Composer: remove send↔stop morph for multi-workstream mode.
4. Reply-to sets `target_run_id` on the outbound turn.
5. Kill → `stop_run(run_id)` only.
6. Keep conversation pending queue for untargeted messages until Phase C.

Sibling PR in `hexuria/nativechat` should reference this PRD.

## 9. Phased rollout

| Phase | Ship | Success check |
|---|---|---|
| **A** | PRD + rail/labels/Kill on single live run (no send morph) | User can Kill without send-as-stop; labels match `run_id` |
| **B** | Concurrent runs per conversation + routing by reply-to | X and Y both active; fix X / stop Y isolated |
| **C** | Pending-steer inject same run | Mid-run correction does not require new `run_id`; eng docs drop “coarse steer” alias |
| **D** | Optional box interrupt | Documented `takesEffect` for hard abort cases |

## 10. Risks

| Risk | Mitigation |
|---|---|
| Calling stop+restart “steer” forever | Eng + UI copy: “coarse steer” until Phase C |
| Kill on scrolled-away user bubble | Sticky rail is mandatory |
| Color-only distinction | Require text label |
| Steer cannot undo finished tools | Same as industry; document “Accepted ≠ Applied” |
| Parallel runs explode cost | Cap concurrent workstreams per conversation |
| Mis-routed plain “fix X” | v1 prefer required reply-to when >1 active |

## 11. Validation plan (other agents)

Reviewers should answer:

1. Does Phase A work with **today’s** OpenGrok stop + single run?
2. Is Option B1 (real concurrent runs) acceptable for metering and store invariants?
3. Is pending-steer (Phase C) the right inject point (post-tool vs pre-model)?
4. Any conflict with parked HITL / `interrupt_parked_hitl`?
5. NativeChat: can reply-threading carry `target_run_id` without breaking local persistence?

Suggested reviewers: OpenGrok maintainer agent, NativeChat maintainer agent, adversarial pass on naming (steer vs coarse steer).

## 12. Open questions

1. Default when plain send arrives with two active workstreams: ask, queue on conversation, or refuse?
2. Workstream labels: auto (`1`, `2`) vs model-summarized titles?
3. Should “Waiting for you” cards appear in the rail as workstreams?
4. Do we expose delivery mode (queue/steer) in UI in Phase A or only after Phase C?

## 13. Citations (current code / PRs)

- OpenGrok stop: PR [#131](https://github.com/hexuria/opengrok-server/pull/131); `agui/routes.rs` `stop_run`, `takesEffect`
- Pending queue: PR [#171](https://github.com/hexuria/opengrok-server/pull/171); `agui/pending.rs`
- Steer continuation splice: `STEER_CONTINUATION` in `agui/routes.rs` / `agui/history.rs`
- Interrupted run resume: [#253](https://github.com/hexuria/opengrok-server/pull/253) / [#254](https://github.com/hexuria/opengrok-server/pull/254)
- NativeChat policy: `send_policy.rs`; stop-before-steer in `state.rs`
- Cursor Agent queue/steer: https://cursor.com/docs/agent/overview ; Settings → Agents → Conversation → New Messages

---

## 14. Decision log

| Date | Decision |
|---|---|
| 2026-10-05 | Kill follows the live workstream (rail + latest reply), not the original user bubble. |
| 2026-10-05 | Steer is harness delivery; models generally do not implement steer. |
| 2026-10-05 | UI ships before same-run inject; do not fake Cursor steer in docs until Phase C. |
| 2026-10-05 | Composer stays free; Kill is per-workstream. |