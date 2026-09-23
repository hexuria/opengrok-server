---------------------------- MODULE HarnessLoop ----------------------------
(***************************************************************************)
(* The agent loop `converse_raw` (crates/opengrok-harness/src/lib.rs) AS  *)
(* IT IS at 4a25af6: one run segment, one process, a person who may press *)
(* Stop at any moment, a model that may say anything, tools that may      *)
(* succeed, fail, refuse or park, and a journal whose every write may     *)
(* fail.                                                                   *)
(*                                                                         *)
(* Deliberately smaller than the code: no events, no HTTP, no images, no  *)
(* text — only the facts the loop branches on. Each action names the      *)
(* lines it abstracts so a counterexample reads back onto the source.     *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS MaxRounds,       \* MAX_ROUNDS
          MaxComputer,     \* MAX_COMPUTER_ROUNDS
          SameLimit,       \* SAME_SCREEN_LIMIT
          MaxFailedWork,   \* intent::MAX_FAILED_WORK_ROUNDS
          JournalCanFail,  \* may a journal write return Err?
          FallOutFails,    \* FIX: leaving the `for` ends the run with RUN_ERROR, never silently
          StopAtClose      \* FIX: a clean finish asks `stopped` once more and yields to a Stop

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
    ending,      \* what the client was told: "none" | "finished" | "failed" | "stopped" | "parked" | "fellOut"
    terminals,   \* terminal events emitted (RUN_FINISHED / RUN_ERROR)
    journaledEnd,\* did the journal durably receive the ending?
    unjournaled, \* tool results exist that the journal has not got
    modelCalls, toolRuns, toolRunsAfterStop,
    batch, outcome

vars == <<pc, round, spoken, computer, workFails, lastRefused, lastScreen, sameScreen,
          anyDelta, stop, ending, terminals, journaledEnd, unjournaled, modelCalls,
          toolRuns, toolRunsAfterStop, batch, outcome>>
loopVars == <<round, spoken, computer, workFails, lastRefused, lastScreen, sameScreen>>
endVars  == <<ending, terminals, journaledEnd, unjournaled>>

JournalWrite == IF JournalCanFail THEN BOOLEAN ELSE {TRUE}

Init ==
    /\ pc = "open"
    /\ round = 0 /\ spoken = 0 /\ computer = 0 /\ workFails = 0
    /\ lastRefused = "none" /\ lastScreen = 0 /\ sameScreen = 0
    /\ anyDelta = FALSE /\ stop = FALSE
    /\ ending = "none" /\ terminals = 0 /\ journaledEnd = FALSE /\ unjournaled = FALSE
    /\ modelCalls = 0 /\ toolRuns = 0 /\ toolRunsAfterStop = 0
    /\ batch = "other" /\ outcome = "ok"

\* One terminal event; the journal may or may not get it. Every terminal write in the code
\* is `let _ = record_round(..)` inside finish_round / finish_ending (lib.rs:653, 672).
EndWith(e) ==
    /\ pc' = "done" /\ ending' = e /\ terminals' = terminals + 1 /\ unjournaled' = FALSE
    /\ journaledEnd' \in JournalWrite

\* lib.rs:764-776 — the opening record. On failure the RUN_ERROR is returned, never journaled.
\* A clean finish. With StopAtClose the loop asks the journal once more, so a Stop recorded
\* while the model was talking or a tool was running is how the run ends.
Finish == EndWith(IF StopAtClose /\ stop THEN "stopped" ELSE "finished")

Open ==
    /\ pc = "open"
    /\ \E ok \in JournalWrite :
         IF ok THEN pc' = "top" /\ UNCHANGED endVars
               ELSE pc' = "done" /\ ending' = "failed" /\ terminals' = 1
                    /\ journaledEnd' = FALSE /\ UNCHANGED unjournaled
    /\ UNCHANGED <<loopVars, anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* lib.rs:813-833 — the for bound, then stop check #1.
Top ==
    /\ pc = "top"
    /\ IF round >= ForBound
         THEN \* lib.rs:1607 — falling out of the for returns with NO terminal event.
              IF FallOutFails THEN EndWith("failed")
              ELSE pc' = "done" /\ ending' = "fellOut" /\ UNCHANGED <<terminals, journaledEnd, unjournaled>>
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
           [] r = "planFlood"   -> EndWith("failed") /\ anyDelta' = TRUE /\ UNCHANGED <<batch, outcome>>
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
    /\ UNCHANGED <<loopVars, anyDelta, stop, ending, terminals, journaledEnd, modelCalls, batch, outcome>>

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
              EndWith("parked") /\ UNCHANGED loopVars
         [] Refused(outcome) /\ lastRefused = outcome ->                  \* lib.rs:1326-1358
              EndWith("failed") /\ UNCHANGED loopVars
         [] OTHER ->
            \E img \in (IF batch = "screen" /\ outcome = "ok" THEN Screens ELSE {0}) :
            LET ss == IF img = 0 THEN sameScreen ELSE IF img = lastScreen THEN sameScreen + 1 ELSE 0
                ls == IF img = 0 THEN lastScreen ELSE img
            IN
            /\ workFails' = wf /\ lastRefused' = lr /\ lastScreen' = ls /\ sameScreen' = ss
            /\ IF ss + 1 >= SameLimit THEN EndWith("failed") /\ UNCHANGED <<round, spoken, computer>>   \* 1373-1399
               ELSE IF wf >= MaxFailedWork THEN Finish /\ UNCHANGED <<round, spoken, computer>> \* 1411-1439
               ELSE \E ok \in JournalWrite :                                   \* 1443-1459 DURABLE BEFORE NEXT CALL
                    IF ~ok THEN EndWith("failed") /\ UNCHANGED <<round, spoken, computer>>
                    ELSE IF batch = "render" THEN Finish /\ UNCHANGED <<round, spoken, computer>> \* 1464-1489
                    ELSE LET onScreen == batch = "screen" /\ outcome = "ok"
                             sp == IF onScreen THEN spoken ELSE spoken + 1
                             co == IF onScreen THEN computer + 1 ELSE computer
                         IN /\ spoken' = sp /\ computer' = co
                            /\ IF sp >= MaxRounds \/ co >= MaxComputer
                                 THEN EndWith("failed") /\ UNCHANGED round           \* 1502-1568 ("finished" when opened)
                                 ELSE pc' = "top" /\ round' = round + 1 /\ unjournaled' = FALSE
                                      /\ UNCHANGED <<ending, terminals, journaledEnd>>
    /\ UNCHANGED <<anyDelta, stop, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

\* The person presses Stop. It is written to the journal; the loop only reads it.
PressStop ==
    /\ pc /= "done" /\ ~stop /\ stop' = TRUE
    /\ UNCHANGED <<pc, loopVars, anyDelta, endVars, modelCalls, toolRuns, toolRunsAfterStop, batch, outcome>>

Done == pc = "done" /\ UNCHANGED vars

Step == Open \/ Top \/ Call \/ Check2 \/ RunTools \/ Judge
Next == Step \/ PressStop \/ Done
Spec == Init /\ [][Next]_vars /\ WF_vars(Step)

-----------------------------------------------------------------------------
(* PROPERTIES *)

TypeOK ==
    /\ pc \in {"open", "top", "call", "check2", "run", "judge", "done"}
    /\ ending \in {"none", "finished", "failed", "stopped", "parked", "fellOut"}
    /\ terminals \in 0..1

ExactlyOneEnding      == (pc = "done") => (terminals = 1)
NeverFallsOut         == ending /= "fellOut"
DurableBeforeNextCall == (pc = "call") => ~unjournaled
BudgetsHold           == spoken <= MaxRounds /\ computer <= MaxComputer
CallsBounded          == modelCalls <= MaxRounds + MaxComputer
\* One batch may slip through the gap between stop check #2 and `run_all` — never two.
AtMostOneToolRunAfterStop == toolRunsAfterStop <= 1
\* What the client was told is what the journal holds (CLAUDE.md non-negotiable #5).
EndingIsDurable       == (pc = "done") => journaledEnd
\* A Stop that is recorded before the run ends makes the run end as stopped.
StopIsHonoured        == (pc = "done" /\ stop) => ending /= "finished"
\* Deliberately small budgets make the `for` bound reachable in a variant (see ForBoundTight).

Terminates == <>(pc = "done")
=============================================================================
