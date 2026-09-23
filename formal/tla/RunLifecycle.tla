---------------------------- MODULE RunLifecycle ----------------------------
(***************************************************************************)
(* One run's lifecycle across processes: the aggregate in the event log    *)
(* (opengrok-core/src/run.rs), the loops that drive it (converse_raw and   *)
(* resume_conversation), people answering its card or pressing Stop, the  *)
(* recovery sweep (opengrok-server/src/recovery.rs), and crashes.          *)
(*                                                                         *)
(* Each of the three switches is one candidate fix. The model is checked  *)
(* with all of them off (the code at 4a25af6) and all on (this change);   *)
(* the counterexamples with them off are why each one exists.             *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    LeaseOnResume,     \* continue_run / resume_suspended_run hold a recovery Lease
    StopBeforeApproved,\* resume_conversation asks `stopped` before running the approved call
    EndedMeansStop,    \* the journal's `stopped` answers for ANY terminal status, not only Stopped
    LeaseCanLapse,     \* a renewal may fail (recovery.rs:295 only logs it) and the lease lapse
    MaxSuspends,       \* how many times the run may park
    MaxToolRounds      \* tool rounds per loop

Loops    == 1..(MaxSuspends + 1)     \* loop 1 is the turn; loop k+1 continues the k-th answer
Terminal == {"finished", "failed", "stopped"}

VARIABLES
    status,         \* the aggregate: "running" | "awaiting" | "finished" | "failed" | "stopped"
    phase,          \* per loop: "unborn" | "approve" | "live" | "tool" | "dead"
    lease,          \* per loop: holds a renewing lease
    rounds,         \* per loop: tool rounds spent
    suspends,       \* parks so far
    reading,        \* answer requests that have read `awaiting` and not yet committed
    answers,        \* answers committed (each spawns one continuation)
    approvedRuns,   \* times an approved call ran
    approvedAfterStop, \* an approved call ran on a run already stopped
    workAfterEnd,   \* a tool started on a run that had already ended
    falseFailure    \* the sweep failed a run a live loop was still driving

vars == <<status, phase, lease, rounds, suspends, reading, answers, approvedRuns,
          approvedAfterStop, workAfterEnd, falseFailure>>

Init ==
    /\ status = "running"
    /\ phase = [l \in Loops |-> IF l = 1 THEN "live" ELSE "unborn"]
    /\ lease = [l \in Loops |-> l = 1]            \* routes.rs:2718 — the turn holds one
    /\ rounds = [l \in Loops |-> 0]
    /\ suspends = 0 /\ reading = 0 /\ answers = 0
    /\ approvedRuns = 0 /\ approvedAfterStop = FALSE /\ workAfterEnd = FALSE /\ falseFailure = FALSE

\* The journal's answer to "should this loop stop" (routes.rs:2811).
Told == IF EndedMeansStop THEN status \in Terminal ELSE status = "stopped"

Kill(l) == /\ phase' = [phase EXCEPT ![l] = "dead"] /\ lease' = [lease EXCEPT ![l] = FALSE]

\* A live loop at a step boundary.
LoopStep(l) ==
    /\ phase[l] = "live"
    /\ IF Told
         THEN Kill(l) /\ UNCHANGED <<status, rounds, suspends, workAfterEnd>>
         ELSE \/ \* the model answers in words: RUN_FINISHED. The log refuses it once terminal.
                 /\ status' = IF status = "running" THEN "finished" ELSE status
                 /\ Kill(l) /\ UNCHANGED <<rounds, suspends, workAfterEnd>>
              \/ \* a tool is asked for and parks: Suspended, then the loop ends.
                 /\ suspends < MaxSuspends /\ status = "running"
                 /\ status' = "awaiting" /\ suspends' = suspends + 1
                 /\ Kill(l) /\ UNCHANGED <<rounds, workAfterEnd>>
              \/ \* a tool runs (a long await: no journal write while it runs).
                 /\ rounds[l] < MaxToolRounds
                 /\ rounds' = [rounds EXCEPT ![l] = @ + 1]
                 /\ phase' = [phase EXCEPT ![l] = "tool"]
                 /\ workAfterEnd' = (workAfterEnd \/ status \in Terminal)
                 /\ UNCHANGED <<status, lease, suspends>>
              \/ \* budget spent: RUN_ERROR.
                 /\ rounds[l] = MaxToolRounds
                 /\ status' = IF status = "running" THEN "failed" ELSE status
                 /\ Kill(l) /\ UNCHANGED <<rounds, suspends, workAfterEnd>>
    /\ UNCHANGED <<reading, answers, approvedRuns, approvedAfterStop, falseFailure>>

ToolDone(l) ==
    /\ phase[l] = "tool" /\ phase' = [phase EXCEPT ![l] = "live"]
    /\ UNCHANGED <<status, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, workAfterEnd, falseFailure>>

\* resume_conversation's first act: run the call the person approved (lib.rs:511-518).
Approve(l) ==
    /\ phase[l] = "approve"
    /\ IF StopBeforeApproved /\ Told
         THEN Kill(l) /\ UNCHANGED <<approvedRuns, approvedAfterStop>>
         ELSE /\ approvedRuns' = approvedRuns + 1
              /\ approvedAfterStop' = (approvedAfterStop \/ status = "stopped")
              /\ phase' = [phase EXCEPT ![l] = "tool"] /\ UNCHANGED lease
    /\ UNCHANGED <<status, rounds, suspends, reading, answers, workAfterEnd, falseFailure>>

\* Somebody presses the card's button: `answer_run` loads the run...
AnswerRead ==
    /\ status = "awaiting" /\ reading < 2
    /\ reading' = reading + 1
    /\ UNCHANGED <<status, phase, lease, rounds, suspends, answers, approvedRuns,
                   approvedAfterStop, workAfterEnd, falseFailure>>

\* ...and appends Answered at the seq it read. The unique (stream_id, stream_seq) is a
\* compare-and-set: only a request whose read is still current commits (postgres.rs:376-391).
AnswerCommit ==
    /\ reading > 0
    /\ reading' = reading - 1
    /\ IF status = "awaiting" /\ answers < suspends
         THEN LET l == answers + 2 IN
              /\ status' = "running" /\ answers' = answers + 1
              /\ phase' = [phase EXCEPT ![l] = "approve"]
              /\ lease' = [lease EXCEPT ![l] = LeaseOnResume]
         ELSE UNCHANGED <<status, answers, phase, lease>>    \* Conflict → alreadyAnswered
    /\ UNCHANGED <<rounds, suspends, approvedRuns, approvedAfterStop, workAfterEnd, falseFailure>>

\* Stop: `stop_run` on a running or parked run (routes.rs:3599).
Stop ==
    /\ status \in {"running", "awaiting"} /\ status' = "stopped"
    /\ UNCHANGED <<phase, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, workAfterEnd, falseFailure>>

\* The sweep: a `running` run whose lease lapsed and whose log has been quiet for LEASE_MS
\* (postgres.rs:666-695) is failed as "interrupted by a restart" (recovery.rs:86-140).
\* Quiet is possible exactly when no loop holds a lease and none is between journal writes
\* at a step boundary — a loop awaiting a long tool or an approved call writes nothing.
Sweep ==
    /\ status = "running"
    /\ \A l \in Loops : ~lease[l]
    /\ \A l \in Loops : phase[l] \in {"unborn", "dead", "tool", "approve"}
    /\ status' = "failed"
    /\ falseFailure' = (falseFailure \/ \E l \in Loops : phase[l] \in {"tool", "approve"})
    /\ UNCHANGED <<phase, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, workAfterEnd>>

\* A renewal fails and the lease lapses while the loop is still alive.
Lapse ==
    /\ LeaseCanLapse
    /\ \E l \in Loops : lease[l] /\ lease' = [lease EXCEPT ![l] = FALSE]
    /\ UNCHANGED <<status, phase, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, workAfterEnd, falseFailure>>

\* The process dies: every loop and every lease with it. The log stays.
Crash ==
    /\ \E l \in Loops : phase[l] \in {"approve", "live", "tool"}
    /\ phase' = [l \in Loops |-> IF phase[l] = "unborn" THEN "unborn" ELSE "dead"]
    /\ lease' = [l \in Loops |-> FALSE]
    /\ UNCHANGED <<status, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, workAfterEnd, falseFailure>>

Next ==
    \/ \E l \in Loops : LoopStep(l) \/ ToolDone(l) \/ Approve(l)
    \/ AnswerRead \/ AnswerCommit \/ Stop \/ Sweep \/ Crash \/ Lapse
    \/ (status \in Terminal \cup {"awaiting"} /\ reading = 0 /\ UNCHANGED vars)

Spec == Init /\ [][Next]_vars
        /\ \A l \in Loops : WF_vars(LoopStep(l)) /\ WF_vars(ToolDone(l)) /\ WF_vars(Approve(l))
        /\ WF_vars(AnswerCommit) /\ WF_vars(Sweep)

-----------------------------------------------------------------------------
(* PROPERTIES *)

\* Every approved call runs at most once per committed answer — double clicks included.
ApprovedAtMostOnce  == approvedRuns <= answers
\* A Stop pressed while the card was up keeps the approved call from running.
NoApprovedAfterStop == ~approvedAfterStop
\* Once the log says the run is over, no loop starts another tool on it.
NoWorkAfterEnd      == ~workAfterEnd
\* The sweep never fails a run somebody is still driving.
NoFalseFailure      == ~falseFailure
\* No run is left `running` with nobody driving it: it ends, or parks for a person.
NoOrphan == <>[](status /= "running")
=============================================================================
