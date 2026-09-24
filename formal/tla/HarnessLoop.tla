---------------------------- MODULE HarnessLoop ----------------------------
(***************************************************************************)
(* The agent loop `converse_raw` (crates/opengrok-harness/src/lib.rs):   *)
(* the code at 4a25af6 with its fixes as switches (all FALSE = 4a25af6):  *)
(* one run segment, one process, a person who may press *)
(* Stop at any moment, a model that may say anything, tools that may      *)
(* succeed, fail, refuse or park, and a journal whose every write may     *)
(* fail.                                                                   *)
(*                                                                         *)
(* Deliberately smaller than the code: no events, no HTTP, no images, no  *)
(* text — only the facts the loop branches on. Each action names the      *)
(* lines it abstracts (numbered as at 4a25af6) so a counterexample reads *)
(* back onto the source.                                                   *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS MaxRounds,       \* MAX_ROUNDS
          MaxComputer,     \* MAX_COMPUTER_ROUNDS
          SameLimit,       \* SAME_SCREEN_LIMIT
          MaxFailedWork,   \* intent::MAX_FAILED_WORK_ROUNDS
          JournalCanFail,  \* may a journal write return Err?
          FallOutFails,    \* FIX: leaving the `for` ends the run with RUN_ERROR, never silently
          StopAtClose,     \* FIX: a clean finish asks `stopped` once more and yields to a Stop
          OneWriteClose,   \* FIX: every exit writes the round it ends on and its ending in ONE write
          WriteBeforeEmit, \* FIX: an ending reaches the client only once the log holds it
          EndedRefusesPark \* FIX: a park whose write finds the run ended writes nothing and ends stopped

ForBound == MaxRounds + MaxComputer   \* lib.rs:813 `for _round in 0..(MAX_ROUNDS + MAX_COMPUTER_ROUNDS)`

\* What one model call amounts to.
Replies == {"words", "nothing", "planFlood", "tools", "streamError", "doorError"}
\* What the round's calls were: all screen actions, other work, or a chart/form.
Batches == {"screen", "other", "render"}
\* What the round's results amount to.
Outcomes == {"ok", "refusedA", "refusedB", "await", "workFail", "argvMistake", "unrecoverable"}
Screens == {1, 2}

VARIABLES
    pc,          \* where the loop is
    round,       \* the `for` counter
    spoken, computer, workFails,
    lastRefused, \* "none" | "refusedA" | "refusedB"
    lastScreen, sameScreen,
    anyDelta,
    stop,        \* the person's Stop, as the journal holds it
    ending,      \* how the loop ended the run: "none" | "finished" | "failed" | "stopped" | "parked" | "fellOut"
    told,        \* what the client was shown: an ending, "unrecorded" (the run could not be recorded), or "none"
    terminals,   \* terminal events emitted (RUN_FINISHED / RUN_ERROR)
    journaledEnd,\* did the journal durably receive the ending?
    orphan,      \* the log holds the round a run ended on, without the ending it ended with
    unjournaled, \* tool results exist that the journal has not got
    modelCalls, toolRuns, toolRunsAfterStop,
    batch, outcome

vars == <<pc, round, spoken, computer, workFails, lastRefused, lastScreen, sameScreen,
          anyDelta, stop, ending, told, terminals, journaledEnd, orphan, unjournaled, modelCalls,
          toolRuns, toolRunsAfterStop, batch, outcome>>
loopVars == <<round, spoken, computer, workFails, lastRefused, lastScreen, sameScreen>>
endVars  == <<ending, told, terminals, journaledEnd, orphan, unjournaled>>

JournalWrite == IF JournalCanFail THEN BOOLEAN ELSE {TRUE}

Init ==
    /\ pc = "open"
    /\ round = 0 /\ spoken = 0 /\ computer = 0 /\ workFails = 0
    /\ lastRefused = "none" /\ lastScreen = 0 /\ sameScreen = 0
    /\ anyDelta = FALSE /\ stop = FALSE
    /\ ending = "none" /\ told = "none" /\ terminals = 0 /\ journaledEnd = FALSE /\ orphan = FALSE
    /\ unjournaled = FALSE
    /\ modelCalls = 0 /\ toolRuns = 0 /\ toolRunsAfterStop = 0
    /\ batch = "other" /\ outcome = "ok"

\* What the client is shown when the loop ends the run with `e` and the ending's write came
\* back `w`. With WriteBeforeEmit the ending is emitted only after the log holds it, and a
\* write that failed is shown as "the run could not be recorded". Without it the ending was
\* emitted first, so the client saw `e` whatever the write did.
Shown(e, w) == IF WriteBeforeEmit /\ ~w THEN "unrecorded" ELSE e

\* One terminal event. How it is written depends on the exit (at 4a25af6):
\*   "one"   — the round and the ending in one write (finish_round)
\*   "split" — the round, then the ending (record_round + finish_ending: plan flood, park,
\*             refused twice, same screen): the first can land without the second
\*   "after" — the round was already written as DURABLE BEFORE THE NEXT CALL, then the
\*             ending (the chart/form and budget exits): the ending can fail after it
\* With OneWriteClose every exit is "one". Every ending write ignored its error (`let _ =`).
Close(kind, e) ==
    /\ pc' = "done" /\ ending' = e /\ terminals' = terminals + 1 /\ unjournaled' = FALSE
    /\ \E first, w \in JournalWrite :
         /\ journaledEnd' = w /\ told' = Shown(e, w)
         /\ orphan' = CASE OneWriteClose \/ kind = "one" -> FALSE
                         [] kind = "split"                -> first /\ ~w
                         [] kind = "after"                -> ~w
EndWith(e) == Close("one", e)

\* A clean finish. With StopAtClose the loop asks the journal once more, so a Stop recorded
\* while the model was talking or a tool was running is how the run ends. A park asks too.
FinishBy(kind) == Close(kind, IF StopAtClose /\ stop THEN "stopped" ELSE "finished")
Finish == FinishBy("one")

\* The DURABLE BEFORE THE NEXT CALL write failed: the run fails saying it could not be recorded
\* (lib.rs:1443-1459) — a true sentence whether or not the retry of the write lands.
EndUnrecorded ==
    /\ pc' = "done" /\ ending' = "failed" /\ terminals' = terminals + 1 /\ unjournaled' = FALSE
    /\ told' = "unrecorded" /\ journaledEnd' \in JournalWrite /\ UNCHANGED orphan

Open ==
    /\ pc = "open"
    /\ \E ok \in JournalWrite :
         IF ok THEN pc' = "top" /\ UNCHANGED endVars
               ELSE pc' = "done" /\ ending' = "failed" /\ told' = "unrecorded" /\ terminals' = 1
                    /\ journaledEnd' = FALSE /\ UNCHANGED <<orphan, unjournaled>>
    /\ UNCHANGED <<loopVars, anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* lib.rs:813-833 — the for bound, then stop check #1.
Top ==
    /\ pc = "top"
    /\ IF round >= ForBound
         THEN \* lib.rs:1607 — falling out of the for returns with NO terminal event.
              IF FallOutFails THEN EndWith("failed")
              ELSE pc' = "done" /\ ending' = "fellOut"
                   /\ UNCHANGED <<told, terminals, journaledEnd, orphan, unjournaled>>
         ELSE IF stop THEN EndWith("stopped")
                      ELSE pc' = "call" /\ UNCHANGED endVars
    /\ UNCHANGED <<loopVars, anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* lib.rs:835-992 — one model call and its stream.
Call ==
    /\ pc = "call"
    /\ modelCalls' = modelCalls + 1
    /\ \E r \in Replies :
         CASE r = "doorError"   -> EndWith("failed") /\ UNCHANGED <<anyDelta, batch, outcome>>
           [] r = "streamError" -> EndWith("failed") /\ anyDelta' \in {anyDelta, TRUE}
                                   /\ UNCHANGED <<batch, outcome>>
           \* lib.rs:911-941 — prose past PLAN_ONLY_TEXT_LIMIT with work tools offered and none started.
           [] r = "planFlood"   -> Close("split", "failed") /\ anyDelta' = TRUE /\ UNCHANGED <<batch, outcome>>
           \* lib.rs:1573-1604 — no tools asked for: last round either way.
           [] r = "words"       -> Finish /\ anyDelta' = TRUE /\ UNCHANGED <<batch, outcome>>
           [] r = "nothing"     -> (IF anyDelta THEN Finish ELSE EndWith("failed"))
                                   /\ UNCHANGED <<anyDelta, batch, outcome>>
           [] r = "tools"       -> \E b \in Batches, o \in Outcomes :
                                      pc' = "check2" /\ anyDelta' = TRUE /\ batch' = b /\ outcome' = o
                                      /\ UNCHANGED endVars
    /\ UNCHANGED <<loopVars, stop, toolRuns, toolRunsAfterStop>>

\* lib.rs:1018-1103 — stop check #2, then the two early finishes that run no tool
\* (a repeated listing, the same open action again).
Check2 ==
    /\ pc = "check2"
    /\ IF stop THEN EndWith("stopped")
       ELSE \/ pc' = "run" /\ UNCHANGED endVars
            \/ Finish          \* lib.rs:1038-1066, 1077-1103
    /\ UNCHANGED <<loopVars, anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* lib.rs:1104-1165 — `run_all`: one await, not interruptible.
RunTools ==
    /\ pc = "run"
    /\ toolRuns' = toolRuns + 1
    /\ toolRunsAfterStop' = IF stop THEN toolRunsAfterStop + 1 ELSE toolRunsAfterStop
    /\ unjournaled' = TRUE
    /\ pc' = "judge"
    /\ UNCHANGED <<loopVars, anyDelta, stop, ending, told, terminals, journaledEnd, orphan, modelCalls,
                   batch, outcome>>

Refused(o) == o \in {"refusedA", "refusedB"}

\* lib.rs:1222-1569 — what the results mean, in the code's order.
Judge ==
    /\ pc = "judge"
    /\ LET wf == CASE outcome = "unrecoverable" -> MaxFailedWork        \* lib.rs:1243-1247
                   [] outcome = "workFail"      -> workFails + 1        \* lib.rs:1248-1249
                   [] outcome = "ok"            -> 0                    \* lib.rs:1251-1255
                   [] OTHER                     -> workFails            \* argv mistake, refusal, await
           lr == IF Refused(outcome) THEN outcome ELSE "none"
       IN
       CASE outcome = "await" ->                                          \* lib.rs:1276-1304 park
              \* With StopAtClose a park asks too: a Stop pressed while the tool ran must not
              \* end the run on a card, which its answer would find already stopped. The question
              \* and the write are two steps (ParkWrite): a Stop can land between them.
              IF StopAtClose
                THEN IF stop THEN Close("split", "stopped") /\ UNCHANGED loopVars
                     ELSE pc' = "park" /\ UNCHANGED <<loopVars, endVars>>
                ELSE Close("split", "parked") /\ UNCHANGED loopVars
         [] Refused(outcome) /\ lastRefused = outcome ->                  \* lib.rs:1326-1358
              Close("split", "failed") /\ UNCHANGED loopVars
         [] OTHER ->
            \E img \in (IF batch = "screen" /\ outcome = "ok" THEN Screens ELSE {0}) :
            LET ss == IF img = 0 THEN sameScreen ELSE IF img = lastScreen THEN sameScreen + 1 ELSE 0
                ls == IF img = 0 THEN lastScreen ELSE img
            IN
            /\ workFails' = wf /\ lastRefused' = lr /\ lastScreen' = ls /\ sameScreen' = ss
            /\ IF ss + 1 >= SameLimit THEN Close("split", "failed") /\ UNCHANGED <<round, spoken, computer>> \* 1373-1399
               ELSE IF wf >= MaxFailedWork THEN Finish /\ UNCHANGED <<round, spoken, computer>> \* 1411-1439
               ELSE LET onScreen == batch = "screen" /\ outcome = "ok"
                        sp == IF onScreen THEN spoken ELSE spoken + 1
                        co == IF onScreen THEN computer + 1 ELSE computer
                        over == sp >= MaxRounds \/ co >= MaxComputer
                        Next_ == pc' = "top" /\ round' = round + 1 /\ unjournaled' = FALSE
                                 /\ UNCHANGED <<ending, told, terminals, journaledEnd, orphan>>
                    IN
                    IF OneWriteClose
                    THEN \* The chart/form and budget exits are decided BEFORE the durable write,
                         \* so the round they end on goes down with their ending, once.
                         IF batch = "render" THEN Finish /\ UNCHANGED <<round, spoken, computer>>
                         ELSE /\ spoken' = sp /\ computer' = co
                              /\ IF over THEN (EndWith("failed") \/ Finish) /\ UNCHANGED round
                                 ELSE \E ok \in JournalWrite :      \* DURABLE BEFORE THE NEXT CALL
                                      IF ~ok THEN EndUnrecorded /\ UNCHANGED round ELSE Next_
                    ELSE \E ok \in JournalWrite :                   \* 1443-1459 DURABLE BEFORE NEXT CALL
                         IF ~ok THEN EndUnrecorded /\ UNCHANGED <<round, spoken, computer>>
                         ELSE IF batch = "render"                 \* 1464-1489
                              THEN FinishBy("after") /\ UNCHANGED <<round, spoken, computer>>
                         ELSE /\ spoken' = sp /\ computer' = co
                              /\ IF over                          \* 1502-1568: RUN_ERROR, or the opened sentence
                                   THEN (Close("after", "failed") \/ FinishBy("after")) /\ UNCHANGED round
                                   ELSE Next_
    /\ UNCHANGED <<anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* The park's write, after `close` asked `stopped` and heard no. A Stop recorded in the gap makes
\* the log refuse the `Suspended`; at 3eb8304 the refusal was dropped and the rest written, so the
\* run ended on a card the log could not answer (the peer review's trace). With EndedRefusesPark
\* the write refuses the batch whole and the round goes in with the stop's ending.
ParkWrite ==
    /\ pc = "park"
    /\ Close("split", IF EndedRefusesPark /\ stop THEN "stopped" ELSE "parked")
    /\ UNCHANGED <<loopVars, anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* The person presses Stop. It is written to the journal; the loop only reads it.
PressStop ==
    /\ pc /= "done" /\ ~stop /\ stop' = TRUE
    /\ UNCHANGED <<pc, loopVars, anyDelta, endVars, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

Done == pc = "done" /\ UNCHANGED vars

Step == Open \/ Top \/ Call \/ Check2 \/ RunTools \/ Judge \/ ParkWrite
Next == Step \/ PressStop \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Step)

-----------------------------------------------------------------------------
(* PROPERTIES *)

TypeOK ==
    /\ pc \in {"open", "top", "call", "check2", "run", "judge", "park", "done"}
    /\ ending \in {"none", "finished", "failed", "stopped", "parked", "fellOut"}
    /\ told \in {"none", "finished", "failed", "stopped", "parked", "unrecorded"}
    /\ terminals \in 0..1

ExactlyOneEnding      == (pc = "done") => (terminals = 1)
NeverFallsOut         == ending /= "fellOut"
DurableBeforeNextCall == (pc = "call") => ~unjournaled
BudgetsHold           == spoken <= MaxRounds /\ computer <= MaxComputer
CallsBounded          == modelCalls <= MaxRounds + MaxComputer
\* One batch may slip through the gap between stop check #2 and `run_all` — never two.
AtMostOneToolRunAfterStop == toolRunsAfterStop <= 1
\* The ending the loop decided is in the journal. Fails by nature when the journal refuses the
\* write: no loop can make a broken journal hold anything.
EndingIsDurable       == (pc = "done") => journaledEnd
\* What the client is shown is true: it sees an ending only if the log holds it, and otherwise
\* it is told the run could not be recorded (CLAUDE.md non-negotiable #5, as far as it can hold).
ToldIsTrue            == (pc = "done" /\ told \notin {"unrecorded", "none"}) => journaledEnd
\* The log never holds the round a run ended on without the ending it ended with.
RoundNeverWithoutEnding == ~orphan
\* A Stop that is recorded before the run ends makes the run end as stopped.
StopIsHonoured        == (pc = "done" /\ stop) => ending \notin {"finished", "parked"}


Terminates == <>(pc = "done")
=============================================================================
