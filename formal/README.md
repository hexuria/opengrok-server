# Formal models of the harness

Three TLA+ models and one Lean file, deliberately smaller than the code: they carry only the
facts the loop and the run lifecycle branch on — no events, HTTP, SQL or serialisation. They
were written against `4a25af6` and drove the fixes that ship with them; each fix exists because
TLC printed a trace without it. `scripts/formal.sh` re-runs all of it.

| File | What it is |
|---|---|
| `tla/HarnessLoop.tla` | One segment of `converse_raw` (`crates/opengrok-harness/src/lib.rs`): the rounds, the two Stop check points, every exit, the budgets, a journal whose writes may fail. |
| `tla/RunLifecycle.tla` | One run across processes: the aggregate (`opengrok-core/src/run.rs`), the turn and its continuations, answers racing each other, Stop, the recovery sweep, crashes, lapsed leases, and a client retrying its POST with the same run id. |
| `tla/JournalAppend.tla` | One journal write racing a Stop, as the store sees it: read the run, append at the next seq, lose the race with a `Conflict`. Which errors a write may retry. |
| `lean/Harness.lean` | The four facts that must hold for every constant, not just the ones TLC can enumerate. Lean 4 core only. |
| `tla/*.cfg` | One per claim. A first line saying EXPECTED TO FAIL is a counterexample kept on purpose. |

## Model

**Loop** (`HarnessLoop`). States `open → top → call → check2 → run → judge → top …`, ending in
`done` with one of `finished | failed | stopped | parked`. A model call replies with words,
nothing, a plan-only flood, tool calls, a stream error or a door error. A tool round's results
are ok, refused (two distinguishable signatures), parked, a work failure, an argv mistake or a
missing binary; its calls are screen actions, other work, or a chart/form. The person may press
Stop at any step.

**Lifecycle** (`RunLifecycle`). The aggregate is `running | awaiting | finished | failed |
stopped`. Loop 1 is the turn; loop *k+1* continues the *k*-th answer. Each loop is `unborn →
approve → live ⇄ tool → dead`. An answer is two steps — read, then append at the seq it read —
so two requests can interleave. The sweep fails a `running` run that has no lease and no
journal write for `LEASE_MS`; a crash kills every loop and lease; a renewal can fail. Loop 0
is the loop a retried POST with the same run id starts, at any point in the run's life.

## Properties

| Property | Kind | Where |
|---|---|---|
| Every ending emits exactly one terminal event | safety | `ExactlyOneEnding`; Lean `Ending.at_most_one_terminal`, `ended_is_stable` |
| The loop never leaves its `for` without an ending | safety | `NeverFallsOut`; Lean `Budget.never_falls_out` |
| No model call while a round's tool results are not yet durable | safety | `DurableBeforeNextCall` |
| Spoken and screen budgets hold; model calls ≤ `R + C` | safety | `BudgetsHold`, `CallsBounded`; Lean `Budget.calls_bounded` |
| At most one tool batch runs after a Stop (the check-to-`run_all` gap) | safety | `AtMostOneToolRunAfterStop` |
| A Stop recorded before the close asks is how the run ends | safety | `StopIsHonoured` |
| An approved call runs at most once per committed answer | safety | `ApprovedAtMostOnce`; Lean `Answer.at_most_one_commit` |
| A Stop recorded before the continuation asks keeps the approved call from running | safety | `NoApprovedAfterStop` |
| After the log ends a run, a loop starts at most one more tool (the one already past its check) | safety | `AtMostOneStaleTool` — fails today: a retry runs a whole turn on an ended run |
| A retried POST is answered, never told to stop unless a person did | safety | `RetryNotRefused` |
| At most one loop drives a run at any moment | safety | `OneDriver` — fails today (`RunLifecycle_retry`) |
| The sweep never fails a run a live loop is driving | safety | `NoFalseFailure` |
| Every run ends | liveness | `Terminates`; Lean `Budget.measure_decreases` |
| No run is left `running` with nobody driving it | liveness | `NoOrphan` |
| The ending the client saw is in the journal | safety | `EndingIsDurable`, which fails by nature (see below) |
| The client is shown an ending only if the log holds it; otherwise it is told the run could not be recorded | safety | `ToldIsTrue` |
| The log never holds the round a run ended on without the ending it ended with | safety | `RoundNeverWithoutEnding` |
| A round journaled while a Stop lands is kept | safety | `JournalAppend` `RoundKept` |
| No round is written twice | safety | `JournalAppend` `NoDuplicate` |

## TLA+ findings

Each trace is TLC's shortest.

1. **A Stop during the final answer ended the run as "finished"** (`HarnessLoop_4a25af6_stop`).
   `open → top → call`, Stop, then the model answers in words → `RUN_FINISHED`. Stop was asked
   only at the top of a round and before tools, and the final round has neither.
2. **A resumed run was failed by the sweep while its approved call was running**
   (`RunLifecycle_4a25af6`, 5 states): park → answer → sweep. The turn holds a lease
   (`routes.rs` `Lease::new`), but its continuation (`continue_run`, `resume_suspended_run`)
   held none, and an approved call writes nothing while it runs. So a recipe longer than
   `LEASE_MS` was failed as "interrupted by a restart" while it was still playing.
3. **An approved call ran on a stopped run** (`RunLifecycle_nostopcheck`): answer → Stop →
   the continuation runs the call. `resume_conversation` executed it before its first
   `stopped` question. The model separates the question from the act, as the code does. With
   the check, a Stop recorded before the question is honoured; a Stop landing between the
   question and `run_all` still gets through. That one-step race is in every check-then-act;
   closing it would need the executor itself to ask.
4. **A loop keeps working on a run the log has already failed** (the lapse regime): a renewal
   fails, the lease lapses, the sweep fails the run, and the loop starts tool after tool until
   its budget runs out, because `stopped()` answers only for `Stopped`. The obvious fix, "any
   ended run means stop", shipped in this PR's first push and was reverted: see 8.
5. **Unreachable, but a trap**: `fellOut`. At production budgets (`HarnessLoop_production`,
   1.3M states) the `for` bound is never what ends a run. If it ever were, the code returned
   with no terminal event: the client's spinner would stay up forever.
6. **Holds as built**: double answers. A double-click, two clients, or a retry after a timeout
   all read the same seq, and the unique `(stream_id, stream_seq)` lets exactly one commit.
   This holds in every configuration, so no change was made.
7. **Stated limits, kept as failing configurations:**
   - `HarnessLoop_durable`: if the journal refuses the ending, it does not hold it. The loop
     cannot fix that alone, and the sweep eventually ends such a run.
   - `RunLifecycle_lapse`: a lease without a fencing token cannot stop a lapsed renewal
     letting the sweep fail a live run.
8. **CI found what the model assumed away** (`RunLifecycle_endedstop`). The first push made
   `stopped()` answer yes for any ended run, and TLC passed it — because the model let a loop
   run only on a run it had started. The slice2 smoke POSTs the same body twice; the second
   POST is a same-runId retry on a finished run, which the server treats as a new turn
   (`DrainResult::AlreadyThisRun`). That loop was now told "stopped" before it said a word:
   `RUN_STARTED CUSTOM CUSTOM RUN_FINISHED`. The model now has the retry (loop 0), and TLC
   prints the smoke's trace in four states: the turn finishes → the client retries → the
   retry's loop is told to stop. The assumption was wrong, not the property: stopping a loop on
   an ended run is right only once a retry can no longer start one there. That is the per-run
   claim, and `RunLifecycle_retry` (two loops on one run, `OneDriver`) is the same gap.

9. **A Stop dropped the round it interrupted** (`JournalAppend_4a25af6`). Every write reads
   the run and appends at the next seq; the loser of two concurrent writes gets `Conflict`
   and has written nothing. `stop_run` retries its own Conflicts; the journal did not, so
   the turn's round lost to the Stop and vanished from the log — the frames `run.rs`
   deliberately keeps accepting on a stopped run. A mid-run write that lost showed the
   person "the run could not be recorded" instead of "stopped". The journal now retries a
   Conflict, and only a Conflict: `JournalAppend_retryall` shows that retrying every error
   writes a round twice when a commit lands and its reply is lost. One concurrent writer needs
   two attempts (TLC, `MaxTries = 1` fails); the code allows five, like `STOP_ATTEMPTS`.
   `JournalAppend_truth` keeps the limit: a lost reply still reads as a failure.

10. **Eighteen exits, three ways to write an ending** (`HarnessLoop_4a25af6_told`,
    `HarnessLoop_4a25af6_split`). Some exits wrote the round and its ending in one write, some
    in two (plan flood, park, refused twice, same screen), and the chart and budget exits in
    three (the durable round, the pinned screenshot, the ending). Every ending was emitted to
    the client before its write, and every one of those writes ignored its error. TLC: a
    client shown `RUN_FINISHED` for a run the log still holds as running (9 states); a park
    whose tool call reached the log without the `Suspended` that makes its card answerable
    (15 states). Now every exit names an `Ending` and `close` does the rest: the Stop check,
    the fallback sentence, the pin, one write of the round with its ending, and only then the
    emit. A refused write is shown as the one true ending, "the run could not be recorded".
    The chart and budget exits are decided above the durable write, so they write once too.

## Lean findings

`lean/Harness.lean` checks with Lean 4.23 core, with no `sorry` and no axioms beyond core.

- **`Budget`.** Each continuing round strictly decreases `(R − spoken) + (C − computer)`
  (`measure_decreases`). After *k* continuing rounds, `spoken + computer = k` (`after_sum`),
  so at most `R + C − 2` rounds continue. Every iteration reached is `< R + C`
  (`never_falls_out`), for all `R, C ≥ 1`. Assumption: the only `continue` spends a budget,
  which is `lib.rs`'s bookkeeping, transcribed.
- **`Ending`.** The projection's `finished` flag is an invariant, "ended ⇔ exactly one terminal
  emitted", preserved by every operation. Any operation sequence emits at most one terminal,
  and an ended projection is a fixed point. `awaiting_approval` is modelled as non-terminal,
  because it is.
- **`Close`.** `close true v ≠ finished` for every verdict. Without a Stop, `close` is the
  identity. A failure and a park survive a Stop. These restate the one-line definitions and
  are not linked to the Rust. They record the close's intended rule; `StopIsHonoured` and the
  tests check the code.
- **`Answer`.** With `append(log, expected)` succeeding only when `log = expected`, any batch
  of commits that read the same seq has at most one success. Assumption: each successful
  commit spawns exactly one continuation (`answer_run`, `resume_settled`).
- **How much `Budget` rests on the transcription.** `never_falls_out` is a sum-of-counters
  bound; its hypothesis carries the loop's meaning. It holds because the loop has exactly one
  `continue` and it spends a budget, which the verifier re-checked against `lib.rs`.
- **Not proved in Lean:** the interleavings. The lifecycle properties rest on TLC's bounded
  search (two suspensions, two tool rounds per loop, up to two concurrent answer requests).
  So does the claim that the fix set is minimal (next section).

## Simplification

The state graph was the object being minimised. The results:

- **The `for`'s fall-out branch is now an ending.** One terminal-less transition is gone, so
  every exit ends the same way. The bound itself stays as the spend backstop Lean shows it
  never needs to be. Replacing it with `loop` would remove one more branch but trade a silent
  hang for an unbounded spend if a future `continue` forgot its budget.
- **"Stop" and "finish" meet in one place.** Six clean-finish sites each chose `finish()`
  directly. They now share `finish_or_stop`, the one close the Lean `Close` model describes.
- **The fix set is minimal among the candidates, by exhaustive bounded search.** With
  renewals that never fail, `{lease on resume, stop before approved}` is necessary and
  sufficient for `ApprovedAtMostOnce`, `NoApprovedAfterStop`, `NoFalseFailure` and
  `RetryNotRefused`: drop the lease and `RunLifecycle_4a25af6` fails, drop the check and
  `RunLifecycle_nostopcheck` fails. "Any ended run means stop" is needed for
  `AtMostOneStaleTool` once renewals can fail, but without the claim it contradicts
  `RetryNotRefused` (`RunLifecycle_endedstop`), so it waits for the claim. "Minimal" is
  claimed only among these candidates.
- **One way out.** `converse_raw` had 18 exits that each built their own ending, in one,
  two or three writes, with four helpers (`stop_here`, `finish_round`, `finish_or_stop`,
  `finish_ending`). There is now one: every exit is `end_run!(round, Ending::…)`, and `close`
  is the only function that ends a run. The model already had a single `Close` action; the
  code now matches it. Eleven ignored journal writes became one checked write.

## Implementation

- `crates/opengrok-harness/src/lib.rs`
  - `finish_or_stop` is now the close for every clean finish.
  - Falling out of the `for` fails the run with a sentence.
  - `resume_conversation` asks `stopped` before running (or refusing) the approved call.
- `crates/opengrok-server/src/agui/routes.rs`
  - `continue_run` holds a recovery `Lease`.
  - `StoreJournal::stopped` still answers only for `Stopped`, with a comment saying why it
    must not be widened before the claim (finding 8).
- `crates/opengrok-server/src/agui/resume.rs`: `resume_suspended_run` holds a recovery
  `Lease`.
- `crates/opengrok-server/src/agui/routes.rs`: `append_events` retries a `Conflict`
  (`APPEND_ATTEMPTS`), and no other error.
- `crates/opengrok-harness/src/lib.rs`: `Ending` and `close` replace the four ending helpers;
  `resume_conversation` goes through `close` too, and its durable write is now checked.
  `crates/opengrok-harness/src/projection.rs`: `unrecorded` swaps a refused ending for the one
  `RUN_ERROR` that is true.
- Tests derived from the traces, in `crates/opengrok-harness/tests/unit/loop_tests.rs`:
  - `a_stop_pressed_during_the_final_answer_ends_the_run_as_a_stop` (finding 1)
  - `a_run_stopped_after_its_card_was_answered_does_not_run_the_approved_call` (finding 3)
  - `a_refused_card_is_read_by_the_model_and_never_runs` (`Close.runsApproved`)

  The first two fail on `4a25af6` and pass now.
- Also in `loop_tests.rs`, from `ToldIsTrue` and `RoundNeverWithoutEnding`:
  `an_ending_the_journal_refused_is_told_as_unrecorded`,
  `a_park_the_journal_refused_shows_no_card`, `a_parked_round_and_its_card_are_journaled_together`,
  `a_chart_round_and_its_ending_are_journaled_together` (each fails on the previous commit), and
  `a_door_that_will_not_open_ends_the_run_once`.
- `crates/opengrok-server/tests/against_a_stopped_run.rs`:
  `a_round_journaled_while_a_stop_lands_keeps_its_frames` races three rounds against a Stop
  on Postgres (`RoundKept`, `NoDuplicate`).

## Remaining hazards the models name but this change does not fix

- **Same-runId retries** (`RunLifecycle_retry`). A retried `POST /ag-ui` with the same
  `runId` starts a second loop on the same run. Neither `DrainResult::AlreadyThisRun` nor an
  unqueued turn checks for a live loop, so both loops run tools until the run ends, and a
  retry of an ended run runs a whole turn whose events the log refuses. Closing it needs a
  per-run claim.
- **A journal that is down stays down.** `close` tells the client the truth, but a run whose
  ending could not be written is still `running` in the log until the sweep fails it, with the
  sweep's "interrupted by a restart" (`EndingIsDurable`).
- **Check-then-act races.** They are narrowed, not closed: a Stop landing between a
  `stopped` question and the act that follows it (`run_all`, or the RUN_FINISHED write)
  still gets through, once per loop.
- **Answer errors.** `answer_run` maps every `Conflict` to `alreadyAnswered`, including one
  caused by a concurrent Stop.
