# Formal models of the harness

Four TLA+ models and one Lean file, deliberately smaller than the code: they carry only the
facts the loop and the run lifecycle branch on — no events, HTTP, SQL or serialisation. They
were written against `4a25af6` and drove the fixes that ship with them; each fix exists because
TLC printed a trace without it. `scripts/formal.sh` re-runs all of it, and CI's `formal` job
runs it on every change that is not docs only (TLC 1.7.4 and Lean 4.23, both pinned by sha256
in `scripts/install-tla.sh` and `scripts/install-lean.sh`; under a minute). When a change
must touch the models, and what to do with a counterexample: [`POLICY.md`](POLICY.md).

| File | What it is |
|---|---|
| `tla/HarnessLoop.tla` | One segment of `converse_raw` (`crates/opengrok-harness/src/lib.rs`): the rounds, the two Stop check points, every exit, the budgets, a journal whose writes may fail. |
| `tla/RunLifecycle.tla` | One run across processes: the aggregate (`opengrok-core/src/run.rs`), the turn and its continuations, answers racing each other, Stop, the recovery sweep, crashes, lapsed leases, and a client retrying its POST with the same run id. |
| `tla/JournalAppend.tla` | One journal write racing a Stop, as the store sees it: read the run, append at the next seq, lose the race with a `Conflict`. Which errors a write may retry. |
| `tla/RecipeLease.tla` | Starting a recipe run on one bot (`start_recipe_run`, `opengrok-store/src/postgres.rs`): starters that each lock, take the insert's snapshot, insert where no live lease is visible, and commit; a landed run clears its lease. |
| `lean/Harness.lean` | The four facts that must hold for every constant, not just the ones TLC can enumerate. Lean 4 core only. |
| `tla/*.cfg` | One per claim. A first line saying EXPECTED TO FAIL is a counterexample kept on purpose; its `\* VIOLATES:` line names the one invariant it must break, and breaking any other fails the check. |

## Model

**Loop** (`HarnessLoop`). States `open → top → call → check2 → run → judge → top …`, ending in
`done` with one of `finished | failed | stopped | parked`. A model call replies with words,
nothing, a plan-only flood, tool calls, a stream error or a door error. A tool round's results
are ok, refused (two distinguishable signatures), parked, a work failure, an argv mistake or a
missing binary; its calls are screen actions, other work, or a chart/form. The person may press
Stop at any step. With `WrapUp` (#93) a spent budget, or the wall clock at the top of any
round after the first, goes to `wrap`: a Stop recorded by then wins, and otherwise one more
call with no tools finishes the run with its words or fails it with the budget's reason.
Before a round's call, or the wrap-up's, the context guard (#90) may find the request too long
for the model and fail the run with no call at all (`TooLong`).

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
| Spoken and screen budgets hold; model calls ≤ `R + C`, the wrap-up call included | safety | `BudgetsHold`, `CallsBounded`; Lean `Budget.calls_bounded`, `calls_with_wrap_up_bounded` |
| At most one tool batch runs after a Stop (the check-to-`run_all` gap) | safety | `AtMostOneToolRunAfterStop` |
| A Stop recorded before the close asks is how the run ends — no finish, no card | safety | `StopIsHonoured` |
| An approved call runs at most once per committed answer | safety | `ApprovedAtMostOnce`; Lean `Answer.at_most_one_commit` |
| A Stop recorded before the continuation asks keeps the approved call from running | safety | `NoApprovedAfterStop` |
| After the log ends a run, a loop starts at most one more tool (the one already past its check) | safety | `AtMostOneStaleTool` |
| A retried POST is answered, never told to stop unless a person did | safety | `RetryNotRefused` |
| At most one loop drives a run at any moment | safety | `OneDriver` |
| The sweep never fails a run a live loop is driving | safety | `NoFalseFailure` |
| Every run ends | liveness | `Terminates`; Lean `Budget.measure_decreases` |
| No run is left `running` with nobody driving it | liveness | `NoOrphan` |
| The ending the client saw is in the journal | safety | `EndingIsDurable`, which fails by nature (see below) |
| The client is shown an ending only once its write succeeded; otherwise it is told the run could not be recorded | safety | `ToldIsTrue`; Lean `Close.close_sends_one_terminal` (one ending either way) |
| The log never holds the round a run ended on without the ending it ended with | safety | `RoundNeverWithoutEnding` |
| A round journaled while a Stop lands is kept | safety | `JournalAppend` `RoundKept` |
| No round is written twice | safety | `JournalAppend` `NoDuplicate` |
| At most one run of a bot holds a live recipe lease | safety | `RecipeLease` `AtMostOneLive` |
| Every recipe start answers, won or refused | liveness | `RecipeLease` `EveryStartAnswers` |

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

11. **A run id was a key anyone could turn twice** (`RunLifecycle_retry`, `OneDriver`). A POST
    whose run id already had a run started a second loop on it, and `append_run` preferred the
    newer account, so another account's POST with a leaked run id appended its own turn to the
    run and became its owner. Now the first POST claims the run: its `Started` goes in at the
    first seq under the unique `(stream_id, stream_seq)`, exactly as an answer commits (Lean
    `Answer.at_most_one_commit` is the same argument), and only the claimer runs a loop. A POST
    that finds the run already there gets it back if it owns it — replayed, then followed to
    its end, with one closer — and a 409 otherwise; an anonymous caller owns nothing. The owner
    is now set once. With the claim, no loop can start on an ended run, so "any ended run means
    stop" is back: `RunLifecycle_endedstop` still prints CI's trace, but only with the claim off.
    The claim's first gate run caught two smokes, slice2 and slice3, that had been sharing a run
    id whenever they started in the same second; slice3's turn used to be appended to slice2's
    run. Each now adds its process id.

12. **The second verifier pass** (fresh context, the four commits above). Confirmed and fixed:
    - `attach` skipped real frames: replay hydration appends transcript cards it cannot place,
      and for a run with no frames yet it places none, so every card the coworker ever showed
      reached the stream and shifted the count. It now sends only the log's own frames.
    - `attach` closed a parked run that was stopped (or swept after its card was answered) with
      the park's `RUN_FINISHED`, leaving a live card. The closer now follows the run's status.
    - A park after a Stop landed during `run_all` still showed its card, whose `Suspended` the
      log refuses on a stopped run (TLC prints it in eight states once `StopIsHonoured` covers
      parks). A park now asks `stopped` like a finish.
    - After a failed durable write, `close` wrote the round again with its ending: the "retry
      every error" that `JournalAppend_retryall` shows duplicates a round whose reply was lost.
      Only the ending is written now; the opening write goes through `close` the same way.
    - `AgUiSink` released held login-form frames at a `RUN_ERROR`, so NativeChat could paint a
      card with no suspension behind it. They now go out only after a clean `RUN_FINISHED`.

    Overclaims corrected here: `ToldIsTrue` is about the write succeeding, not about everything
    the store accepted (see the hazards below); `RetryNotRefused` holds trivially once the claim
    is on, because `attach` is not modelled, and its closer and frames are unit-tested instead.

13. **The peer review** (`origin/main..3eb8304`, three traces, each now fixed):
    - **A park still showed a dead card after a Stop** (`HarnessLoop_3eb8304_park`, nine
      states). `close` asks `stopped`, hears no, and a Stop commits before the park's write.
      That write lost to the Stop with a `Conflict`, and `append_events` retried it (finding 9).
      On a stopped run the frames were accepted and the `Suspended` refused. The refusal was
      dropped, so the write said Ok and the card went out with nothing behind it. For a run the
      sweep had failed the frame itself was refused, with the same Ok. A batch that parks is
      now all or nothing: the write answers `JournalError::Ended` and writes none of it. `close`
      writes the round again with the stop's ending, the one the log can back. The model
      splits the park's question from its write (`ParkWrite`), and `EndedRefusesPark` is the
      fix.
    - **A Stop's `RUN_FINISHED` released held login forms.** A form that parked after a Stop
      became a stop, so its stamping CUSTOM never went out, and the stop's `RUN_FINISHED`
      counted as clean. NativeChat painted a login card with a raw `call-…` id. The hold now
      notes the `run-stopped` frame and withholds the forms at the closer.
    - **A failed ownership read was a `409 run-exists`.** Obeying it, the owner of a dropped
      stream started a new run id with the same words: every model call and tool twice. It is
      a 503 now, like the failed `load_run` before it.

14. **Two recipe starts at once both got the lease** (`RecipeLease_nolock`, 48 states; #227).
    `start_recipe_run` inserted where no row of the bot held a live lease, in one statement.
    Under READ COMMITTED that statement's snapshot is taken when it begins, and a row another
    start has inserted but not committed is not in it, so both inserted and two recipes played
    on one screen. The integration test saw four winners of eight in its second round. The
    start now takes `pg_advisory_xact_lock` on the bot as its own statement before the insert,
    in the same transaction: inside the insert, the lock would be granted after the snapshot had
    already missed the winner's row. A unique index cannot do it, since an expired lease stays
    non-null. The lease still lapses unfenced, the limit `RunLifecycle_lapse` states.

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
  tests check the code. (The code now also turns a park into a stop; the Lean `close` predates
  that and still says a park survives.) `unrecorded_one_terminal` and `close_sends_one_terminal`
  are about frames: whatever ending the log refused, its replacement carries exactly one
  terminal, so `close` sends one ending whether or not its write succeeded.
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
- **"Stop", "finish" and "park" meet in one place.** Six clean-finish sites each chose
  `finish()` directly, and a park never asked. All of them now go through `close`, which asks
  `stopped` for a finish and for a park.
- **The fix set is minimal among the candidates, by exhaustive bounded search.** Over all
  2⁴ combinations of the four lifecycle switches (lease on resume, stop before the approved
  call, any ended run means stop, the claim), in both regimes:
  - **Renewals that never fail.** Exactly two combinations pass all six properties: all four,
    and all but "any ended run means stop". Drop the lease and `NoFalseFailure` fails, the stop
    check and `NoApprovedAfterStop` fails, the claim and `OneDriver` fails.
  - **Renewals that may fail** (`recovery.rs` only logs a failed `hold_run`). All four, and all
    but the lease, pass the five properties this regime can have (`NoFalseFailure` cannot, see
    `RunLifecycle_lapse`). Drop "any ended run means stop" and `AtMostOneStaleTool` fails.

  No proper subset passes both regimes, so all four ship. "Minimal" is claimed only among these
  candidates.

- **One way out.** `converse_raw` had 18 exits that each built their own ending, in one,
  two or three writes, with four helpers (`stop_here`, `finish_round`, `finish_or_stop`,
  `finish_ending`). There is now one: every exit is `end_run!(round, Ending::…)`, and `close`
  is the only function that ends a run. The model already had a single `Close` action; the
  code now matches it. Eleven ignored journal writes became one checked write, and a write that
  failed is never repeated (it may have landed): only the ending is written after it.

## Implementation

- `crates/opengrok-harness/src/lib.rs`
  - Every exit names an `Ending` and returns through `close`: the stop check (a finish or a
    park yields to a recorded Stop), the fallback sentence, the pin, one write of the round with
    its ending, and only then the emit. A write that failed is never repeated.
  - `resume_conversation` asks `stopped` before running (or refusing) the approved call, goes
    through `close`, and checks its durable write.
  - Falling out of the `for` fails the run with a sentence.
  - `TooLong` is `window.fit` (`context.rs`) before each round's call and in `wrap_up!`, after its Stop check:
    what cannot fit fails the run with a sentence and asks the door nothing. It is not
    switched, because the door-error branch of `Call` already ends the same way one call later.
- `crates/opengrok-harness/src/projection.rs`: `unrecorded` swaps a refused ending for the one
  `RUN_ERROR` that is true, keeping its brackets and its `run-timing`.
- `crates/opengrok-server/src/agui/routes.rs`
  - `append_events` retries a `Conflict` (`APPEND_ATTEMPTS`), and no other error.
  - `continue_run` holds a recovery `Lease`; so does `resume_suspended_run` (`resume.rs`).
  - `start_claimed_turn` answers a run id that already has a run before any side effect, and
    claims the run (`StoreJournal::claim`) before it starts a loop. `answer_for_existing_run`
    gives the owner its run back through `attach` (the log's own frames, then one closer by
    status); anybody else gets `409 run-exists`.
  - `StoreJournal::stopped` answers yes for any ended run (findings 8 and 11).
  - `append_events` refuses whole a batch that parks a run that has ended (`AppendError::Ended`,
    surfaced as `JournalError::Ended`); `close` then ends the run stopped.
  - `AgUiSink` releases held login-form frames only after a clean ending, and a stop's
    `RUN_FINISHED` is not one.
  - `answer_for_existing_run` answers a failed ownership read with 503, never `run-exists`.
- `crates/opengrok-store/src/postgres.rs`: a run's owner is set once; `run_progress` is the
  cheap read `attach` polls.
- `scripts/slice2-agui-smoke.sh`, `scripts/slice3-harness-smoke.sh`: one run id per run.
- Tests, each failing on the commit before its fix:
  - `loop_tests.rs`: `a_stop_pressed_during_the_final_answer_ends_the_run_as_a_stop`,
    `a_run_stopped_after_its_card_was_answered_does_not_run_the_approved_call`,
    `an_ending_the_journal_refused_is_told_as_unrecorded`,
    `a_park_the_journal_refused_shows_no_card`,
    `a_parked_round_and_its_card_are_journaled_together`,
    `a_chart_round_and_its_ending_are_journaled_together`,
    `a_park_after_a_stop_ends_the_run_stopped_with_no_card`,
    `a_round_whose_write_failed_is_not_written_again`,
    `a_park_whose_write_finds_the_run_stopped_ends_stopped_with_no_card`; and, covering paths that were already
    right, `a_refused_card_is_read_by_the_model_and_never_runs` and
    `a_door_that_will_not_open_ends_the_run_once`.
  - `against_a_stopped_run.rs` (Postgres): `a_round_journaled_while_a_stop_lands_keeps_its_frames`,
    `a_park_written_after_the_run_ended_is_refused_whole`.
  - `against_a_retried_run.rs` (Postgres): a retry gets its run back and the model is asked
    once; two POSTs at once run one loop; another account's POST is refused and leaves the run
    its owner's; an anonymous caller cannot reuse a run id; the owner is set once.
  - `routes.rs` and `user_form.rs` unit tests: `an_attached_stream_closes_with_what_the_run_is`,
    `an_attached_stream_sends_only_the_logs_own_frames`,
    `held_form_frames_go_out_only_after_a_clean_ending`,
    `held_form_frames_do_not_go_out_after_a_stop`,
    `a_failed_ownership_read_is_a_503_not_run_exists` (only the second of two reads would have
    to fail, and no test store injects that, so the mapping is tested alone).

## Remaining hazards the models name but this change does not fix

- **A reattached stream moves a round at a time.** The owner's retry follows the log, which
  the turn writes once per round, so its words arrive per round rather than per token.
- **An ending the log accepts but the run refuses.** `ToldIsTrue` is about the write
  succeeding. The store also drops, without an error, what the aggregate refuses once a run was
  ended from outside: a loop whose run the sweep failed after a lost renewal ends its live stream
  as `run-stopped`, while the log keeps the failure.
- **A queued message sent under a run id that already has a run** is drained and answered with
  that run. The existing-run check comes after the queued-send check on purpose, so that a
  same-run retry whose words changed gets its `stale-pending-message` refusal; no client is known
  to reuse a run id for a different message.
- **An attached stream reads the whole run on every change** (one primary-key read per second
  otherwise), with no cap on how many are attached.
- **A journal that is down stays down.** `close` tells the client the truth, but a run whose
  ending could not be written is still `running` in the log until the sweep fails it, with the
  sweep's "interrupted by a restart" (`EndingIsDurable`).
- **Check-then-act races.** They are narrowed, not closed: a Stop landing between a
  `stopped` question and the act that follows it (`run_all`, or a finish's `RUN_FINISHED`
  write) still gets through, once per loop. For a park the gap is closed (finding 13): the
  log's refusal of the `Suspended` is the answer.
- **Answer errors.** `answer_run` maps every `Conflict` to `alreadyAnswered`, including one
  caused by a concurrent Stop.
