# opengrok-server PR #139 (5e367df) — review findings

Reviewed against the author's stated intent (items 1–8). Scope: the AG-UI sink, user-form /
credential handling, HITL park + interrupt, auth rotate grace, spend-cap upsert, egress tunnel,
plus the smoke scripts and new tests.

Category summary is at the bottom, including the categories that turned up nothing.

---

## [SEVERITY: high] [CONFIDENCE: high] Live AG-UI deltas bypass the secret scrubber entirely, so a model-smuggled `values.password` on `request_user_form` is streamed verbatim to NativeChat

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-harness/src/lib.rs:495-502` (the per-delta emit), vs `crates/opengrok-harness/src/lib.rs:387-393` (`emit_live`, which does scrub)

**What's wrong:** Every model delta is pushed to the sink with a raw `sink.emit(&produced).await`
call that does **not** go through `emit_live`, and therefore never calls `scrub_event_secrets`:

```rust
let produced = projection.push(delta);
if let Some(sink) = sink && !produced.is_empty() {
    sink.emit(&produced).await;     // <-- no scrub_event_secrets
}
round_events.extend(produced);
```

Every *other* live emission in `converse` (`stop_here`, `fail`, `finish`, tool results, park) uses
`emit_live`, which maps `scrub_event_secrets` first. The delta path — the one that carries
`TOOL_CALL_ARGS` — is the single exception.

**Why it matters / how it fails:** `MockDoor::user_form_call` deliberately smuggles
`"values": {"password": "s3cret-should-never-land"}` into the `request_user_form` arguments, exactly
as a real model might after reading a password off a screenshot. That JSON is emitted as one
`ModelDelta::ToolCallArgs`, becomes a `TOOL_CALL_ARGS` frame, is held by `UserFormSseHold`, and is
then released onto the SSE with the secret intact. The CUSTOM frame is sanitised, the gateway card is
sanitised, the tool call handed to `execute` is sanitised (`tools.rs:collect_tool_calls`), and the
journal is scrubbed — but the live AG-UI stream NativeChat actually reads is not. The tests only
assert on the CUSTOM frame (`tests/against_user_form.rs:812-830`, `:1801-1804`); adding
`assert!(!sse.contains("s3cret-should-never-land"))` to
`two_same_completion_website_logins_live_tool_calls_carry_e_ids` would fail today.

**Suggested fix:** Replace the inline `sink.emit(&produced).await` with `emit_live(sink, &produced).await`
so the delta path gets the same scrub as every other path, and add an end-to-end assertion that the
whole SSE body never contains the smuggled token.

---

## [SEVERITY: high] [CONFIDENCE: high] `scrub_secret_keys` cannot scrub streamed tool-arg fragments, so with a real model the smuggled password also lands in the durable run log

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-tools/src/credential.rs:255-267`, reached from `crates/opengrok-harness/src/lib.rs:828-842` (`for_journal` / `record_round`)

**What's wrong:** The string branch of `scrub_secret_keys` only parses a value when it is a *complete*
JSON document:

```rust
Value::String(text) => {
    let trimmed = text.trim();
    if ((trimmed.starts_with('{') && trimmed.ends_with('}')) || ...)
        && let Ok(parsed) = serde_json::from_str::<Value>(trimmed) { ... }
    Value::String(text.clone())
}
```

A real provider streams function arguments in fragments — `crates/opengrok-harness/src/gateway.rs:205-210`
forwards each chunk's `function.arguments` verbatim as one `ModelDelta::ToolCallArgs`. Those
fragments (`{"title":"Sig`, `n in","values":{"pass`, `word":"s3cret"}}`) are not valid JSON, so the
scrubber returns them unchanged.

**Why it matters / how it fails:** `MockDoor` emits the whole argument object in one delta, which is
why `scrubbing_drops_password_keys_even_inside_a_delta_string`
(`crates/opengrok-tools/src/credential.rs:348`) and the integration tests pass. Against OpenAI/Grok
streaming, the same smuggled password is split across two or more `TOOL_CALL_ARGS` frames, survives
`for_journal`, and is persisted in `run.emitted` — i.e. in Postgres and in every subsequent
`GET /ag-ui/runs/{id}` / `GET /ag-ui/threads/{id}` reply. The PR's claim that a password cannot reach
"AG-UI frames, transcripts, or logs" is only true for complete-JSON deltas.

**Suggested fix:** Scrub at the point where the arguments are assembled, not per fragment. The
projection already knows which `toolCallId` maps to `request_user_form` / `credential.request`;
either buffer the fragments and re-emit one sanitised `TOOL_CALL_ARGS`, or suppress raw arg deltas
for those two tools entirely (NativeChat paints those cards from the CUSTOM + card anyway).

---

## [SEVERITY: high] [CONFIDENCE: high] `POST /ag-ui/runs/{id}/stop` on a parked form leaves the card unresolved forever, and the self-heal path is guarded off — every screen tool for that coworker is then refused permanently

> **Fixed (#186).** `stop_run` now lives on the host router and, once the Stop is in the log,
> settles every card no parked run waits on any more (`settle_dead_holds`: the stopped run's forms
> dismissed, a live handoff declined) before it answers; the note says the card is closed only when
> it was. `interrupt_parked_hitl` runs the same settle on every new turn, with or without a stop of
> its own, so a card an older stop or a dead process left open is closed before the turn builds its
> tools. There is no separate credential card left on main (the gateway's went with seam A). Tests:
> `stopping_a_run_parked_on_a_form_closes_its_card_and_frees_the_screen`,
> `stopping_a_run_whose_form_was_escalated_declines_its_handoff`,
> `a_card_a_stop_left_open_does_not_hold_the_screen_after_a_restart`.

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/agui/routes.rs:2995` (`stop_run`), and `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/conversation.rs:355-370` (`interrupt_parked_hitl`'s `if stopped > 0` guard)

**What's wrong:** `stop_run` stops the run aggregate only. Unlike `interrupt_agent_run` /
`sendPrompt` / `POST /ag-ui`, it never calls `dismiss_unresolved_on_interrupt`, so the gateway
user-form card keeps `formResolution` absent and any live `sand://box` sibling keeps `boxResolution`
absent. Worse, the recovery path is gated:

```rust
for run_id in run_ids { if stop_parked_run(...).await { stopped += 1; } }
if stopped > 0 { dismiss_unresolved_on_interrupt(...).await; ... }
```

Once the run is already `Stopped`, `awaiting_approval()` no longer returns it, so `stopped == 0` and
the dismissal never runs, on any later prompt.

**Why it matters / how it fails:** User asks a coworker to sign in → form card appears → user hits
`POST /ag-ui/runs/{runId}/stop` (item 3 of the PR contract) → run `Stopped`, card still unresolved.
From then on `tools_for_coworker` computes `transcript_hold = entries.iter().any(holds_the_screen)`
→ `true` (`crates/opengrok-server/src/agui/routes.rs:310-325`), so `computer`, `open_url`,
`run_recipe`, `request_user_form` and `credential.request` are all refused on every subsequent turn
for that coworker, and no later `sendPrompt` clears it. The only escape is the user manually pressing
Skip on stale chrome.

**Suggested fix:** Have `stop_run` call the same settle path (or move settling out of the
`stopped > 0` guard so `interrupt_parked_hitl` also cleans up orphan chrome when there is no parked
run left to stop).

---

## [SEVERITY: high] [CONFIDENCE: high] A retried submit of an already-settled stacked twin resumes the *other* card's parked call with the wrong tool result

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:848` (inside `heal_or_already`)

**What's wrong:** The normal submit path correctly passes the card's own `callId` so
`resume_settled` refuses to answer a different pending call
(`user_form.rs:891-896`). The already-settled path does not:

```rust
if resume_user_form(state, account_id, coworker_id, agent_id, content, None).await { ... }
```

`call_id: None` disables the `pending.call_id != want` guard, so it answers whatever call the run is
parked on.

**Why it matters / how it fails:** Two same-completion Website logins (`call-42628be6`,
`call-42628be6-1`). The run's `pending` is the **last** suspension (`RunEvent::Suspended` overwrites
`pending` — `crates/opengrok-core/src/run.rs:343-357`), i.e. card 2. NativeChat submits card 1 → it
settles, no resume (correct). NativeChat retries card 1 (double click, offline retry, KeepAlive
resend) → `is_unresolved` is now false → `heal_or_already` → resume with `call_id: None` → card 2's
parked call is answered with **card 1's** result text, while card 2 is still showing "Waiting for
you" and was never filled. The `first Continue must not consume the parked twin` property the PR
asserts holds only for the first attempt. The same `None` is passed by `abandon_escalated_form`
(`:774`), `resolve_box_handoff` (`:343`) and `timeout_unresolved_form` (`:457`).

**Suggested fix:** Pass `call_id_of(entry)` from `heal_or_already` (and from
`abandon_escalated_form`), and only fall back to `None` where the resume is genuinely card-agnostic
(the timeout sweep).

---

## [SEVERITY: medium] [CONFIDENCE: high] `pending_suspended` returns the first parked run, not the one whose `pending.call_id` matches — a submit in a room with two parked forms settles the card and never resumes its run

> **Fixed (#188).** `pending_suspended` takes the card's `callId` and returns the run whose parked
> calls (every unanswered `run-awaiting-approval` it raised, not only `pending`) include it; `None`
> is left only for cards written before cards carried a call. `resume_settled`,
> `journal_agui_custom` (so a settled frame and the save-login offer land on the card's own run)
> and the hand-back resume (a handoff card has no call; its escalated form's is used) all name it.
> Test: `submit_resumes_the_run_parked_on_its_own_call_not_the_oldest`.

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:951-985`

**What's wrong:** `pending_suspended` walks `awaiting_approval(account_id)` (ordered by
`updated_at_ms`, `crates/opengrok-store/src/postgres.rs:534-539`) and returns the **first** run that
belongs to the coworker with the right `SuspendReason`. `resume_settled` then compares that run's
`pending.call_id` to the submitted card's `callId` and gives up if they differ — it never looks at
the next candidate.

**Why it matters / how it fails:** `run_belongs_to` also matches any run whose thread is
`gateway-{agent}` (`gateway/conversation.rs:381-386`), so a group room can have several members'
runs parked on user-forms at once (`group.rs::pause_room` suspends each). Member B's card is
submitted → `pending_suspended` returns member A's older run → call_id mismatch → `false`. B's card
is stamped `submitted`, the values were already typed into the box, and B's run stays parked until
the 10-minute timeout. The card looks answered and the turn silently hangs.

**Suggested fix:** Give `pending_suspended` an optional `call_id` and return the run whose
`pending.call_id` matches, falling back to the first only when no `call_id` was supplied.

---

## [SEVERITY: medium] [CONFIDENCE: high] The interrupt is coworker-scoped, not thread-scoped: a new message in one thread stops the parked run and dismisses the card of a different thread

> **Fixed (#188).** `interrupt_parked_hitl` takes the incoming `threadId` and stops only that
> thread's parked runs. The coworker-wide dismissal is gone: `settle_dead_holds` settles only
> cards whose `callId` no parked run waits on any more. **The screen hold stays per computer**
> (a coworker has one box; another conversation clicking on the page a person is signing in on is
> the race the hold exists for), and the refusal now names the conversation holding it
> (`ToolContext::screen_held_in`). Test: `a_message_in_another_thread_leaves_this_threads_form_open`.

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/conversation.rs:346-371`, called from `crates/opengrok-server/src/agui/routes.rs:2107-2113`

**What's wrong:** `interrupt_parked_hitl` enumerates `awaiting_approval(account_id)` and stops every
run matching `run_belongs_to(&run, coworker_id)`. Nothing filters on `input.thread_id`, and
`dismiss_unresolved_on_interrupt` then settles every unresolved entry in the coworker's whole gateway
transcript.

**Why it matters / how it fails:** NativeChat opens thread A with coworker "Ada", Ada raises a login
form and parks. The user switches to thread B (same coworker) and types anything → `POST /ag-ui`
→ thread A's run is `Stopped` and its card goes `dismissed` with no fill. The user returns to thread
A to find the sign-in they were about to complete has silently been abandoned. Same for
`sendPrompt`. The stated contract is "a new prompt **for a coworker whose run is parked**" — thread
scope is not mentioned, but the observable behaviour will read as data loss.

**Suggested fix:** Scope the interrupt to runs whose `thread_id` matches the incoming turn (or at
least log/report which runs were interrupted so the client can show it).

---

## [SEVERITY: medium] [CONFIDENCE: high] A held `request_user_form` TOOL_CALL whose CUSTOM never arrives is flushed only at RUN_FINISHED — the card paints at the end of the turn, badly out of order

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:1208-1234` (`UserFormSseHold::push`) and `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/agui/routes.rs:3490-3520` (`AgUiSink::emit`)

**What's wrong:** `push` holds `TOOL_CALL_START/ARGS/END/RESULT` for any `request_user_form` call id.
They are released only by (a) a matching CUSTOM `run-awaiting-approval` / `user-form`, or (b)
`release_rest()` on `RUN_FINISHED` / `RUN_ERROR`. There is no release when the form call does **not**
park.

**Why it matters / how it fails:** `Executor::execute` refuses `request_user_form` outright when
`context.screen_hold` is true or the policy Denies it (`crates/opengrok-tools/src/lib.rs:1095-1106`).
No CUSTOM is emitted, so the START/ARGS/END **and the refusal TOOL_CALL_RESULT** sit in the buffer
while the model runs further rounds, streams its whole answer, and only at `RUN_FINISHED` do they
appear — after the assistant text, with the result arriving minutes after the start. A client that
pairs START/END within a bounded window shows a spinner for the whole turn and then paints a
"Website login" card the user can no longer act on. The same hold also silently delays the frames
across every intervening round (ordering relative to text and to other tools is not preserved).

There is also no release at all if the sink is dropped without a terminal frame (task panic), which
loses the frames entirely.

**Suggested fix:** Release a held call id as soon as its `TOOL_CALL_RESULT` shows it is not awaiting
(`awaiting_approval == false`), and release everything on any terminal condition including sink
drop. Add a unit test for "form call refused, no CUSTOM" and for the `RUN_ERROR` release.

---

## [SEVERITY: medium] [CONFIDENCE: high] The in-memory rotate grace short-circuits the store, so it bypasses every DB-side check on the refresh token — including session revocation

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/auth/routes.rs:780-784`

```rust
if let Some(slot) = state.refresh_grace.reuse(&presented_hash, at_ms) {
    return mint_access_for_slot(state, &slot, at_ms);   // no store read at all
}
```

**What's wrong:** Before this PR, every `/auth/refresh` and `/oauth/token` went through
`account_by_refresh_hash` + `Account::decide`, both of which consult the session's `revoked` flag
and the `session_view` row. The fast path now returns a freshly minted 1-hour access token
(`ACCESS_TOKEN_TTL_SECONDS = 3600`) plus the current refresh plaintext purely from process memory,
and access tokens are verified by signature only — `account_from_bearer`
(`crates/opengrok-server/src/agui/routes.rs:1433-1439`) never checks revocation.

**Why it matters / how it fails:** Today nothing emits `AccountCommand::SignOut` (I grepped: the only
`SessionRevoked` reference outside `account.rs` is the store projection, and `/auth/logout` just
clears cookies), so this is latent rather than live. The moment revocation is wired up — "sign out
all devices", key revocation, a compromised-token response — a stolen *previous* refresh token still
mints valid access tokens for up to 45 s after the last rotation, on a session the operator has just
killed, and the DB has no say. The matching slice1 smoke assertion was removed in this PR (see
below), so nothing would catch it.

**Suggested fix:** Have the grace path re-validate against the store (`match_refresh`/`session_id_for_refresh`
already exists and checks `revoked`) before minting, or drop the slot on any `SessionRevoked`.

---

## [SEVERITY: medium] [CONFIDENCE: medium] A late duplicate refresh can roll the client's cookie *backwards* to a superseded refresh token

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/auth/routes.rs:780-784` and `crates/opengrok-server/src/auth/refresh_grace.rs:54-71`

**What's wrong:** `remember(previous_hash → current_refresh)` slots are keyed by the hash rotated
away, and `reuse` returns the stashed plaintext for the full 45 s regardless of how many rotations
have happened since. Nothing invalidates `hash(R1) → R2` when `R2 → R3` later rotates.

**Why it matters / how it fails:** Client refreshes R1→R2 (slot: `hash(R1)→R2`), then quickly again
R2→R3 (slot: `hash(R2)→R3`). A retried/slow duplicate still carrying R1 arrives inside the 45 s
window → the server replies `Set-Cookie: og_refresh=R2` and the client's good R3 is overwritten with
R2 — a token that is now only the *previous* hash and dies 45 s after the R2→R3 rotation. The client
is then signed out on its next refresh, which is precisely the failure this PR exists to stop.

**Suggested fix:** When `remember` stores a new slot for a session, evict any slot whose
`current_refresh` is the hash being rotated away (chain the grace forward), or key slots by session
and always return the session's true current refresh.

---

## [SEVERITY: medium] [CONFIDENCE: high] Whether a submitted value is a secret is decided entirely by the model's own field metadata

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-tools/src/user_form.rs:43-51` (`FormField::is_secret`) and `:311-323` (`shared_values`)

**What's wrong:** `is_secret()` is `self.secret || type in {"password","otp"}`. Both come from the
model's `request_user_form` arguments. There is no server-side heuristic on label/id.

**Why it matters / how it fails:** The model raises
`{"id":"pin","label":"Bank PIN","type":"text","required":true}` — or simply forgets
`type: "password"` on a password field, which is exactly the class of mistake the rest of this module
defends against. The user types their PIN into a chat card. `shared_values` keeps it (it is not
"secret"), so it is written to the gateway entry as `sharedValues.pin`, journaled into the AG-UI
`user-form` CUSTOM, echoed to the model in the tool result as `Shared fields: pin=1234.`, and
replayed into every later turn's history via `history_line`. The headline promise ("the answer
reaches the box WITHOUT the password being echoed") holds only for correctly-typed fields.

**Suggested fix:** Treat a field as secret when its `id`/`label` matches a conservative pattern
(`password|passwd|pwd|pin|otp|code|secret|token|cvv|seed`) in addition to the declared type, and log
when the heuristic overrides the model.

---

## [SEVERITY: medium] [CONFIDENCE: high] Form / handoff / credential hold timeouts are bare `tokio::spawn` sleeps that do not survive a restart, and the recovery sweep deliberately skips parked runs

> **Fixed (#186).** The sleeping tasks are gone. The deadline is the card's own `timestampMs`:
> `settle_dead_holds` times out a form or live handoff older than `FORM_HOLD_TIMEOUT` (form settled
> dismissed + timedOut, its run resumed on its own call; handoff timed_out, its escalated form's run
> resumed) on every new turn and on every stop, and `hold_deadlines_forever` (spawned by the binary)
> walks the runs parked past the deadline across all accounts (`PgStore::parked_between`, one
> window per tick) so a run nobody writes to still times out after a restart. The recovery sweep
> still leaves parked runs alone. Tests: `a_form_past_its_deadline_times_out_on_the_next_turn_after_a_restart`,
> `the_deadline_sweep_times_out_a_parked_form_after_a_restart`,
> `a_live_handoff_past_its_deadline_times_out_and_resumes_its_run`.

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:365-387`, `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/credential.rs:194-204`, `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/recovery.rs:99-102`

**What's wrong:** The 10-minute hold timeout is `tokio::spawn(async { sleep(FORM_HOLD_TIMEOUT); ... })`.
A deploy, crash or replica move loses the task. `recovery::resolve` explicitly returns early for
`RunStatus::AwaitingApproval` ("waiting on a person is not abandonment"), so nothing else ever
settles it.

**Why it matters / how it fails:** Server restarts while a form card is open → the run is
`awaiting-approval` forever, the card is `unresolved` forever, `holds_the_screen` is true forever, and
every subsequent turn for that coworker refuses `computer` / `open_url` / `run_recipe` /
`request_user_form` / `credential.request`. The user's next message *does* clear it via
`interrupt_parked_hitl`, so it is usually self-healing — but combined with the `stop_run` finding
above (where the run is already `Stopped`, so `stopped == 0`) it becomes permanent. Each spawn also
holds a `GatewayState` clone for 10 minutes, unbounded in the number of cards minted.

**Suggested fix:** Persist the hold deadline (the card already has `timestampMs`) and have the
recovery sweep time out `awaiting-approval` runs whose `pending.reason` is `UserForm`/`Credential`
and whose card is older than `FORM_HOLD_TIMEOUT`.

---

## [SEVERITY: medium] [CONFIDENCE: high] `BOX_EGRESS_TUNNEL_BEARER` is the current time in nanoseconds — a predictable bearer for the guest egress-tunnel WebSocket

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-box/src/docker.rs:274` (`format!("BOX_EGRESS_TUNNEL_BEARER=og-{}", uuid_like())`) and `:881-888` (`uuid_like`)

**What's wrong:** `uuid_like()` is `format!("{nanos:x}")` — `SystemTime::now()` in nanoseconds, hex.
Its own doc comment says it is "a short unique-enough token for naming a process's log files", not a
credential. It now guards the egress-tunnel WS published on `127.0.0.1::8790`, which proxies the
user's own network into the box.

**Why it matters / how it fails:** Any local process (or any other container sharing the host
network namespace) that can reach the published loopback port can enumerate candidate bearers: the
container's creation time is visible in `docker ps`/`docker inspect` to anyone in the docker group,
and even without it the search space is a few seconds of nanoseconds. Both `BOX_TOKEN` and the
tunnel bearer are minted microseconds apart from the same clock, so knowing either narrows the other
to a handful of values. (`BOX_TOKEN` already had this weakness; this PR extends the pattern to a new
credential, so it is worth fixing both together.)

**Suggested fix:** Mint both from a CSPRNG (`uuid::Uuid::new_v4().simple()` or 32 bytes of
`getrandom` hex).

---

## [SEVERITY: medium] [CONFIDENCE: medium] `hydrate_agui_events` can inject the same form card into several runs of a thread, and its time window degenerates to "everything" when a run has no timestamps

> **Fixed (#188).** `run_time_window` falls back to an empty window, and the unplaced-card fallback
> appends a card only to the run whose own frames carry its `callId`, so a card paints once. A
> card with no `callId` (older rows) keeps the time-window rule. Tests:
> `hydrate_does_not_inject_another_runs_form`, `a_run_with_no_timestamps_gets_no_transcript_cards`.

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:1325-1341` and `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/agui/routes.rs:2561-2571` (`run_time_window`)

**What's wrong:** The unmatched-form fallback appends `agui_user_form_frame(form)` to any run whose
`[started-5s, updated+5s]` window contains the form's `timestampMs`. `replay_thread`
(`agui/routes.rs:2720-2748`) hydrates every run in the thread with the **same** per-coworker form
list, each with its own `used` set. And `run_time_window` returns `(0, i64::MAX)` when no emitted
event carries a `timestamp`.

**Why it matters / how it fails:** Two runs on one thread whose windows overlap (a resumed run, a
group round, or simply two turns within the ±5 s slack) both append the same card, so a cold
NativeChat load of `GET /ag-ui/threads/{id}?events=true` paints the same "Website login" card twice.
With a timestamp-less run, *every* form the coworker has ever raised is folded into that one run's
replay.

**Suggested fix:** Match forms to runs by `callId`/`runId` rather than a time window, or at minimum
fall back to an empty window instead of `(0, i64::MAX)` and deduplicate across runs in
`replay_thread`.

---

## [SEVERITY: medium] [CONFIDENCE: medium] Journal write failures on the park path are swallowed, leaving a painted card attached to a run that never suspended

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-harness/src/lib.rs:601-607`

```rust
let mut waiting_events = park_awaiting(&mut projection, &waiting);
let _ = record_round(journal, run_id, &round_events).await;
let _ = record_round(journal, run_id, &waiting_events).await;
emit_live(sink, &waiting_events).await;
```

**What's wrong:** Both journal writes discard their `Result`. Every other `record_round` call in
`converse` either checks the error (`:695`) or is on a path that is already ending.

**Why it matters / how it fails:** A brief Postgres hiccup at exactly this point means the
`RunEvent::Suspended` is never appended, so `run.pending` is `None` and `run.status` is not
`awaiting-approval`. The live emit then still fires, `AgUiSink` still mints the gateway card and
stamps `entryId`, and NativeChat paints a Continue-able form. When the user submits,
`pending_suspended` finds nothing → `resume_settled` returns `false` → the card is stamped
`submitted`, the values are typed into the box, and the turn never continues. The user sees
"✓ Submitted" on a dead turn.

**Suggested fix:** Treat a failed park record the same way the loop treats a failed round record —
`projection.fail(...)` and end the run honestly, rather than parking a card on a run that has no
suspension.

---

## [SEVERITY: medium] [CONFIDENCE: medium] Submit types into the live page with no check that the turn that raised the form is still the current one

> **Fixed (#188).** Before anything touches the box (passkey, saved login or typed fill),
> `submit_user_form` requires a parked run that still waits on the card's `callId`; otherwise the
> card settles `fill_failed` with nothing typed. The remaining window is a stop landing between the
> check and the typing. Tests: `a_submit_for_a_card_whose_run_is_no_longer_parked_types_nothing`,
> `a_twin_answered_after_its_run_moved_on_types_nothing`.

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:175` (`fill_on_box` runs before any resume check)

**What's wrong:** `submit_user_form` fills the box as soon as the entry is `unresolved`. Whether the
run that raised it is still parked — or still exists — is only discovered afterwards, in
`resume_user_form`.

**Why it matters / how it fails:** Stacked cards: the user answers card 2 (the parked one) first, the
run resumes and the model navigates elsewhere on the same box. The user then answers card 1, which
is still `unresolved`, and the server types the email into whatever field currently has focus — a
search box, a chat input, a different site's login. `is_unresolved` is the only gate; the
already-resumed run is not consulted, and `screen_hold` does not apply to the fill path (it only
gates `Executor::execute`).

**Suggested fix:** Refuse (or at least skip the fill and settle as `fill_failed`) when
`pending_suspended` shows the run is no longer parked on this card's `callId`.

---

## [SEVERITY: low] [CONFIDENCE: high] The slice1 smoke dropped its strongest assertion and nothing replaces it

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/scripts/slice1-auth-smoke.sh:81-100`

**What's wrong:** Step 5 used to assert that the rotated-away refresh token returns 401 ("Without
this, a leaked refresh token is immortal"). It now asserts the opposite (grace reuse succeeds) plus a
new, much weaker check that a *made-up* token is 401. Nothing at the HTTP layer proves the old hash
dies after 45 s. The comment points at the `Account::decide` unit test — but that unit test only
covers the aggregate, and the HTTP path now short-circuits the aggregate entirely via
`RefreshGrace::reuse` (see the revocation finding above).

**Why it matters / how it fails:** A regression that makes the in-memory slot never expire, or that
widens `REFRESH_GRACE_MS`, is invisible to every test in the repo. `tests/against_auth_refresh.rs`
has no expiry test and no revoked-session test either.

**Suggested fix:** Add an HTTP-level test that injects a clock (or lowers the grace via config) and
asserts 401 after the window, and one that asserts a revoked session cannot be resurrected through
the grace slot.

---

## [SEVERITY: low] [CONFIDENCE: high] The `#[allow(expect_used)]` in the user-form unit tests hides an assertion that can never fail

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-tools/src/user_form.rs:617` (the allow) and `:770-780` (the assertion)

**What's wrong:**

```rust
let line = history_line(&settled).expect("settled history");
assert!(
    line.contains("submitted")
        || line.contains("filled")
        || line.contains("Filled")
        || line.contains("submitted")   // duplicate of the first alternative
        || line.contains("form")
);
```

Every branch of `tool_result_content` contains the word "form", so the final alternative makes the
whole assertion vacuous; the duplicated `"submitted"` suggests it was edited until it passed.

**Why it matters / how it fails:** `unresolved_reads_the_official_rule` would still pass if
`history_line` returned the dismissal text for a submitted form, or the escalation text, or any
other message. The test's stated purpose — that a settled form produces the right history line — is
not actually checked. The `#[allow]` itself is fine (it is test-only); the weakened assertion under
it is the problem.

**Suggested fix:** Assert the exact string: `assert_eq!(line, tool_result_content(&form, FormResolution::Submitted, &shared, false))`.

---

## [SEVERITY: low] [CONFIDENCE: high] `history_line`'s "last-chance" secret check is dead code — it can never fire

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-tools/src/user_form.rs:455-462`, against `:344-355` (`contains_secret_value`) and `:311-323` (`shared_values`)

**What's wrong:** `history_line` builds `shared` from the entry's `sharedValues` and then calls
`contains_secret_value(&line, &form, &shared)`. `contains_secret_value` only inspects fields for
which `field.is_secret()` is true — and `sharedValues` is produced by `shared_values`, which filters
exactly those fields out. The predicate is therefore always `false`.

**Why it matters / how it fails:** The doc comment on `contains_secret_value` advertises it as "a
last-chance strip before a tool result is built", and `submit_user_form` never calls it at all. A
reader (or a future change that starts persisting secret fields) will believe there is a net where
there is none.

**Suggested fix:** Either delete the dead branch and the misleading doc, or run the check against the
raw submitted values in `submit_user_form` before the tool result is built.

---

## [SEVERITY: low] [CONFIDENCE: medium] `stamp_tool_calls` collapses repeated `callId`s to the last `entryId` on replay

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:1349-1380`

**What's wrong:** `by_call: BTreeMap<String, String>` keeps one entry per `callId`; a second CUSTOM
with the same `callId` overwrites the first. Every `TOOL_CALL_*` frame with that id is then stamped
with the later `entryId`.

**Why it matters / how it fails:** A run that parks, resumes, and parks again on a provider that
recycles call ids within a run (the repo's own `MockDoor::asking_for_user_form` reuses
`"mock-form-1"` on every turn) replays both turns' TOOL_CALL frames pointing at the second card. A
cold NativeChat load then shows the first turn's card as already-settled chrome for the second. Real
providers mostly mint unique ids, which is why this has not bitten yet.

**Suggested fix:** Key `by_call` by `(runId or event index range, callId)`, or only stamp the
TOOL_CALL frames that precede the CUSTOM they were matched to.

---

## [SEVERITY: low] [CONFIDENCE: high] `credential_hints` is written but never read by production code

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-store/src/postgres.rs:3120-3151` (`credential_hints`), written at `crates/opengrok-server/src/gateway/credential.rs:127`

**What's wrong:** `upsert_credential_hint` is called on every `filled` result, but the only caller of
`credential_hints()` is `tests/against_user_form.rs:1665`.

**Why it matters / how it fails:** The stated purpose of `credential.offer_save` is "so the *next*
login can be brokered instead of typed". Nothing consults the hint — neither the system prompt, nor
the `credential.request` path, nor the tool schema. The table accumulates rows (origin, username,
opaque credentialId) that no feature uses, which is storage of user-identifying data with no
consumer.

**Suggested fix:** Either wire the hints into the prompt/`credential.request` refusal text as
intended, or drop the write until the read side lands.

---

## [SEVERITY: low] [CONFIDENCE: high] `GET /coworkers/{id}/computer` now probes the guest on every poll, even when the tunnel is disabled

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/agui/provision.rs:812` (`let cap = provider.egress_tunnel(&box_id).await;`)

**What's wrong:** Unlike `AgUiState::egress_tunnel_for` (which returns early on `!host_wants`) and
`gateway::is_egress_tunnel_available` (same), `coworker_screen` probes unconditionally. For
`DockerComputer` that is `docker port` + `docker inspect` (two subprocess spawns) plus a 1-second
HTTP call to the guest, per request.

**Why it matters / how it fails:** The Computer pane polls this endpoint. With the tunnel off — the
default — every poll pays two process spawns and up to a second of latency for a capability whose
answer is discarded (`advertised(false, _)` is always `false`). On a busy host this is a visible
regression in Computer-pane responsiveness.

**Suggested fix:** Skip the probe when `host_wants` is false, exactly as the other two call sites do.

---

## [SEVERITY: low] [CONFIDENCE: high] `POST /ag-ui/user-form/submit` can block for up to 90 seconds waiting for a sleeping box

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:798-809` (`tools_for_coworker(..., TURN_WAKE_PATIENCE)`), constant at `crates/opengrok-server/src/agui/routes.rs:36`

**What's wrong:** The submit handler builds a full `ToolRunner` just to get `fill_target()`, and
passes the 90-second turn-wake patience.

**Why it matters / how it fails:** If the box was archived while the card was open (very likely — the
card can sit for 10 minutes), the HTTP submit blocks for up to 90 s. NativeChat will almost certainly
time out first and retry, which lands in `heal_or_already` on the now-settled entry — i.e. the
wrong-callId resume bug above.

**Suggested fix:** Use a short patience for the fill path (the box either has a live id or the fill
is `fill_failed`), or do the fill asynchronously and answer the HTTP request immediately.

---

## [SEVERITY: low] [CONFIDENCE: medium] Grace slots hold plaintext refresh tokens with purely lazy eviction, and expired `session_view` rows linger

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/auth/refresh_grace.rs:54-71`, and `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-store/src/postgres.rs:160-183`

**What's wrong:** `RefreshGrace::slots` is pruned only inside `remember` and `reuse`. If no refresh
happens after the last rotation, the plaintext `current_refresh` for that session stays resident for
the process's lifetime. On the DB side, the previous-hash row is deleted only by the *next* rotation
for that session (or a revoke), so a session that is never refreshed again leaves a stale row
(harmless to lookups thanks to `grace_until_ms >= $2`, but it is an unbounded table).

**Why it matters / how it fails:** Idle server → live refresh tokens sitting in heap memory long past
their 45-second usefulness, exposed to any core dump or memory-disclosure bug. The `Debug` impl is
correctly redacted (`refresh_grace.rs:38-42`), so this is residency, not logging.

**Suggested fix:** Prune on a timer (or at least on every `rotate` entry regardless of hit/miss), and
sweep `session_view where grace_until_ms < now()` periodically.

---

## [SEVERITY: low] [CONFIDENCE: medium] `internal_tool_name` resolves plugins before builtins and recomputes the whole wire-name table on every lookup

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-tools/src/lib.rs:806-833`

**What's wrong:** `internal_tool_name` calls `lookup_plugin_tool(call_name)` first, which matches
`tool.qualified_name == name` exactly, before checking `reserved_openai_names()`. And
`lookup_plugin_tool`'s fallback calls `plugin_wire_names()`, which allocates and sorts the full
plugin list on *every* miss — once per tool call and once per `internal_tool_name` in
`call_plugin_tool`.

**Why it matters / how it fails:** Correctness: a plugin whose `qualified_name` collides with a
builtin (only reachable if a plugin ever registers an undotted name — `split_qualified` requires two
dots today, so this is latent) would shadow the builtin's dispatch. Performance: an O(n log n) sort
plus a `Vec<(String,String)>` allocation per tool call is avoidable — the mapping is fixed for the
life of the `Executor`.

**Suggested fix:** Check `reserved_openai_names()` first, and compute the wire-name map once in
`with_plugin_tools` rather than per call.

---

## [SEVERITY: low] [CONFIDENCE: high] Test coverage gaps around the two features this PR is named for

**Where:** `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/tests/against_user_form.rs`, `/Volumes/goldcoders/OSS/opengrok-server/crates/opengrok-server/src/gateway/user_form.rs:1550-1605` (the only hold-buffer tests)

**What's wrong:** The hold-and-forward buffer has exactly two unit tests: the happy stacked path and
"a shell call passes through". Nothing covers the failure modes: CUSTOM never arrives, release on
`RUN_ERROR`, ordering relative to interleaved text/other tools, or a poisoned mutex. On the interrupt
side, the tests cover `sendPrompt` and `interruptAgentRun` but **not** `POST /ag-ui` (explicitly
promised in item 3) and **not** `POST /ag-ui/runs/{runId}/stop` (also promised). The interrupt tests
assert `formResolution == "dismissed"` but never assert that a live `sand://box` sibling went
`declined`, which is the other half of the stated contract. Every integration test in
`against_user_form.rs` and `against_auth_refresh.rs` silently `return`s when `OG_DATABASE_URL` is
unset, so a CI job without Postgres reports green having run none of them.

**Why it matters / how it fails:** The three highest-severity findings above (`stop_run` orphaning
chrome, the `heal_or_already` cross-resume, the unscrubbed SSE deltas) would each have been caught by
a test in one of these gaps.

**Suggested fix:** Add: (1) a hold-buffer test where the form call is refused and the frames must
still reach the stream in order; (2) an interrupt test driven by `POST /ag-ui` that also asserts the
handoff sibling is `declined`; (3) a `stop_run` test asserting the card is settled; (4) an SSE-wide
`!contains(smuggled_secret)` assertion.

---

## Categories that turned up nothing

- **`unwrap` / `expect` / `panic` on request paths:** clean. Every added `unwrap`/`expect`/`panic!`
  in the diff is inside `#[cfg(test)]` (verified by scanning all added lines). Production code uses
  `let ... else`, `unwrap_or`, `ok()?` throughout.
- **Blocking calls inside async:** clean. The `std::sync::Mutex` guards in `AgUiSink::hold_or_pass`,
  `GatewayState.settings`, `AgUiState.host_settings` and `RefreshGrace.slots` are all taken and
  dropped within a single non-`await` statement; none is held across an `await`.
- **Debug/Display impls on secret carriers:** correct. `RefreshGrace` has a hand-written
  `Debug` that prints `RefreshGrace(<redacted>)` (`refresh_grace.rs:38-42`); `GraceSlot` derives no
  `Debug`. `CuaAction::Type` does derive `Debug` and `Serialize`, but I found no log site that
  formats an action — `fill_into_focus` logs only `field.label` and the error, and
  `CuaAction::describe()` returns the constant `"typing"`.
- **Request-body logging:** clean. No `TraceLayer`/`on_request` body logging; the gateway command
  dispatcher logs the method name only (`gateway/routes.rs:515`).
- **The submitted-value path itself (the headline claim), for correctly-typed fields:** holds.
  `submitted_values` → `audit_lengths` (label/id/len only) → `fill_into_focus` → `shared_values`
  (secrets dropped) → `settle_entry` (removes `values`) → `tool_result_content` (shared only) →
  `journal_agui_custom` (scrubbed). I could not find a path by which a value submitted to a field the
  model marked `password`/`otp`/`secret` reaches a log, an AG-UI frame, the transcript, an error body
  or a journaled PNG.
- **OpenAI function-name sanitizer collisions:** correct. `openai_unique_tool_names` seeds `used`
  with every builtin's safe name, sorts the qualified list for order-independence, and suffixes
  `_2`, `_3`… with a 64-char cap; `lookup_plugin_tool` round-trips the wire name back to the dotted
  `qualified_name`, and policy/sessions keep the dotted form. I could not construct two distinct
  tools that map to one wire name.
- **Spend-cap principals upsert (405 handling):** correct. `classify_admin_reply` maps 405 to
  `Refused` (never `Unreachable`), `encode_path_segment` stops the derived `org-…@gateway.local`
  address collapsing a per-email path into the collection path, and both stand-in gateways now assert
  `principal_methods == ["POST"]`.
- **Egress-tunnel availability logic:** correct. `EgressTunnel::from_info` refuses partial objects,
  `advertised` is a strict AND of host intent and `ready`, docker publishes only `127.0.0.1::8790`
  (never 8791/8792) and only for a desktop image with host intent, and `share_scope_of` falls back to
  the non-dedicated `"user"` for unknown scopes.
- **`RUN_FINISHED`-but-still-`awaiting-approval` split:** correct on the server side.
  `append_events` (`agui/routes.rs:2469-2475`) skips `Finish` while the aggregate is
  `AwaitingApproval`, so `GET /ag-ui/runs/{id}` does report `awaiting-approval` with `RUN_FINISHED`
  in `events`, and `pending` is still populated. `resume_gateway_run`/`stop_parked_run` both handle
  the split correctly. The one consumer that would be confused (`recovery::resolve`) already
  short-circuits on `AwaitingApproval`.
- **Escalate is not a resume:** correct. `dismiss_user_form` mode `escalated` settles, starts the
  handoff and returns without resuming; `heal_or_already` returns early for `Escalated`; the
  `sand://box` card carries `boxRequestId` and no `boxResolution` while live, and
  `holds_the_screen` keeps the hold through escalation.
- **`Stopped` transition atomicity:** correct. `stop_parked_run` re-loads under a 5-iteration
  optimistic-concurrency loop and returns early if the status is no longer `AwaitingApproval`;
  `RunCommand::Answer` refuses on a terminal run and `append_run`'s `seq` check makes the loser
  `Conflict`. A run cannot end up both stopped and resumed.
- **Notify-based rotate wait (lost wakeup / starvation):** correct. `remember` inserts *before*
  `notify_waiters()`, and `wait_for_reuse` re-checks the map after constructing the `Notified`
  future and again on the sleep branch, so a lost wakeup costs at most `REUSE_WAIT` (100 ms) of
  latency, never a false 401. `notify_waiters()` wakes all registered waiters, so no starvation.
