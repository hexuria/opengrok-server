---------------------------- MODULE RunLifecycle ----------------------------
(***************************************************************************)
(* One run's lifecycle across processes: the aggregate in the event log    *)
(* (opengrok-core/src/run.rs), the loops that drive it (converse_raw and   *)
(* resume_conversation), people answering its card or pressing Stop, the  *)
(* recovery sweep (opengrok-server/src/recovery.rs), crashes, and a       *)
(* client that retries its POST with the same run id.                     *)
(*                                                                         *)
(* Each switch is one candidate fix; the .cfg files say which combination *)
(* is the code at 4a25af6, which is this change, and which counterexample *)
(* each one exists for.                                                   *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    LeaseOnResume,     \* continue_run / resume_suspended_run hold a recovery Lease
    StopBeforeApproved,\* resume_conversation asks `stopped` before running the approved call
    EndedMeansStop,    \* the journal's `stopped` answers for ANY terminal status, not only Stopped
    RetryAttaches,     \* a POST whose run already exists replays it instead of running a loop
    LeaseCanLapse,     \* a renewal may fail (recovery.rs:295 only logs it) and the lease lapse
    MaxSuspends,       \* how many times the run may park
    MaxToolRounds      \* tool rounds per loop

\* Loop 1 is the turn; loop k+1 continues the k-th answer. Loop 0 is the loop a retried POST
\* with the same run id starts (`DrainResult::AlreadyThisRun` → a new turn, routes.rs:2495).
Loops    == 0..(MaxSuspends + 1)
Terminal == {"finished", "failed", "stopped"}
Active   == {"approve", "approveArmed", "live", "armed", "tool"}

VARIABLES
    status,         \* the aggregate: "running" | "awaiting" | "finished" | "failed" | "stopped"
    phase,          \* per loop: "unborn" | "approve" | "approveArmed" | "live" | "armed" | "tool" | "dead"
                    \* "armed" / "approveArmed": asked `stopped`, got no, not yet acted — the gap
                    \* between the question and the act is a real await in the code, so a Stop or
                    \* a sweep can land in it.
    lease,          \* per loop: holds a renewing lease
    rounds,         \* per loop: tool rounds spent
    suspends,       \* parks so far
    reading,        \* answer requests that have read `awaiting` and not yet committed
    answers,        \* answers committed (each spawns one continuation)
    approvedRuns,   \* times an approved call ran
    approvedAfterStop, \* an approved call ran although its loop's own check SAW the Stop
    staleTools,     \* per loop: tools started on a run that had already ended
    falseFailure,   \* the sweep failed a run a live loop was still driving
    retried,        \* the client has retried its POST
    retryRefused    \* the retry's loop was told to stop though nobody pressed Stop

vars == <<status, phase, lease, rounds, suspends, reading, answers, approvedRuns,
          approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

Init ==
    /\ status = "running"
    /\ phase = [l \in Loops |-> IF l = 1 THEN "live" ELSE "unborn"]
    /\ lease = [l \in Loops |-> l = 1]            \* routes.rs:2718 — the turn holds one
    /\ rounds = [l \in Loops |-> 0]
    /\ suspends = 0 /\ reading = 0 /\ answers = 0
    /\ approvedRuns = 0 /\ approvedAfterStop = FALSE /\ staleTools = [l \in Loops |-> 0] /\ falseFailure = FALSE
    /\ retried = FALSE /\ retryRefused = FALSE

\* The journal's answer to "should this loop stop" (routes.rs:2811).
Told == IF EndedMeansStop THEN status \in Terminal ELSE status = "stopped"

Kill(l) == /\ phase' = [phase EXCEPT ![l] = "dead"] /\ lease' = [lease EXCEPT ![l] = FALSE]

\* A live loop at a step boundary.
LoopStep(l) ==
    /\ phase[l] = "live"
    /\ IF Told
         THEN /\ Kill(l) /\ UNCHANGED <<status, rounds, suspends, staleTools>>
              /\ retryRefused' = (retryRefused \/ (l = 0 /\ status \in {"finished", "failed"}))
         ELSE \/ \* the model answers in words: RUN_FINISHED. The log refuses it once terminal.
                 /\ status' = IF status = "running" THEN "finished" ELSE status
                 /\ Kill(l) /\ UNCHANGED <<rounds, suspends, staleTools, retryRefused>>
              \/ \* a tool is asked for and parks: Suspended, then the loop ends.
                 /\ suspends < MaxSuspends /\ status = "running"
                 /\ status' = "awaiting" /\ suspends' = suspends + 1
                 /\ Kill(l) /\ UNCHANGED <<rounds, staleTools, retryRefused>>
              \/ \* a tool is asked for and the loop goes to run it.
                 /\ rounds[l] < MaxToolRounds
                 /\ rounds' = [rounds EXCEPT ![l] = @ + 1]
                 /\ phase' = [phase EXCEPT ![l] = "armed"]
                 /\ UNCHANGED <<status, lease, suspends, staleTools, retryRefused>>
              \/ \* budget spent: RUN_ERROR.
                 /\ rounds[l] = MaxToolRounds
                 /\ status' = IF status = "running" THEN "failed" ELSE status
                 /\ Kill(l) /\ UNCHANGED <<rounds, suspends, staleTools, retryRefused>>
    /\ UNCHANGED <<reading, answers, approvedRuns, approvedAfterStop, falseFailure, retried>>

\* The tool starts (a long await: no journal write while it runs). Whatever the log says now.
Act(l) ==
    /\ phase[l] = "armed" /\ phase' = [phase EXCEPT ![l] = "tool"]
    /\ staleTools' = [staleTools EXCEPT ![l] = @ + (IF status \in Terminal THEN 1 ELSE 0)]
    /\ UNCHANGED <<status, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, falseFailure, retried, retryRefused>>

\* The approved call starts: `box_wake_frame` then `run_all` (lib.rs, resume_conversation).
ApproveAct(l) ==
    /\ phase[l] = "approveArmed"
    /\ approvedRuns' = approvedRuns + 1
    /\ phase' = [phase EXCEPT ![l] = "tool"]
    /\ UNCHANGED <<status, lease, rounds, suspends, reading, answers, approvedAfterStop,
                   staleTools, falseFailure, retried, retryRefused>>

ToolDone(l) ==
    /\ phase[l] = "tool" /\ phase' = [phase EXCEPT ![l] = "live"]
    /\ UNCHANGED <<status, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

\* resume_conversation's first act: run the call the person approved (lib.rs:511-518).
\* A Stop the check SAW that is not honoured is the bug; a Stop landing in the gap after the
\* check is the residual race (ApprovedRace counts it, it is not claimed away).
Approve(l) ==
    /\ phase[l] = "approve"
    /\ IF StopBeforeApproved /\ Told
         THEN Kill(l) /\ UNCHANGED <<approvedRuns, approvedAfterStop>>
         ELSE /\ approvedAfterStop' = (approvedAfterStop \/ status = "stopped")
              /\ phase' = [phase EXCEPT ![l] = "approveArmed"]
              /\ UNCHANGED <<lease, approvedRuns>>
    /\ UNCHANGED <<status, rounds, suspends, reading, answers, staleTools, falseFailure, retried, retryRefused>>

\* Somebody presses the card's button: `answer_run` loads the run...
AnswerRead ==
    /\ status = "awaiting" /\ reading < 2
    /\ reading' = reading + 1
    /\ UNCHANGED <<status, phase, lease, rounds, suspends, answers, approvedRuns,
                   approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

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
    /\ UNCHANGED <<rounds, suspends, approvedRuns, approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

\* Stop: `stop_run` on a running or parked run (routes.rs:3599).
Stop ==
    /\ status \in {"running", "awaiting"} /\ status' = "stopped"
    /\ UNCHANGED <<phase, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

\* The sweep: a `running` run whose lease lapsed and whose log has been quiet for LEASE_MS
\* (postgres.rs:666-695) is failed as "interrupted by a restart" (recovery.rs:86-140).
\* Quiet is possible exactly when no loop holds a lease and none is between journal writes
\* at a step boundary — a loop awaiting a long tool or an approved call writes nothing.
Sweep ==
    /\ status = "running"
    /\ \A l \in Loops : ~lease[l]
    /\ \A l \in Loops : phase[l] \in {"unborn", "dead", "tool", "approve", "approveArmed", "armed"}
    /\ status' = "failed"
    /\ falseFailure' = (falseFailure \/ \E l \in Loops : phase[l] \in {"tool", "approve", "approveArmed", "armed"})
    /\ UNCHANGED <<phase, lease, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, staleTools, retried, retryRefused>>

\* A renewal fails and the lease lapses while the loop is still alive.
Lapse ==
    /\ LeaseCanLapse
    /\ \E l \in Loops : lease[l] /\ lease' = [lease EXCEPT ![l] = FALSE]
    /\ UNCHANGED <<status, phase, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

\* The client POSTs again with the same run id — after its stream dropped, or as the slice2
\* smoke does, after the first response closed. Today that starts a whole new turn on the
\* existing run, holding its own lease (routes.rs:2718). With RetryAttaches the handler finds
\* the run already claimed and replays it instead: no loop starts.
Retry ==
    /\ ~retried /\ retried' = TRUE
    /\ IF RetryAttaches
         THEN UNCHANGED <<phase, lease>>
         ELSE /\ phase' = [phase EXCEPT ![0] = "live"]
              /\ lease' = [lease EXCEPT ![0] = TRUE]
    /\ UNCHANGED <<status, rounds, suspends, reading, answers, approvedRuns, approvedAfterStop,
                   staleTools, falseFailure, retryRefused>>

\* The process dies: every loop and every lease with it. The log stays.
Crash ==
    /\ \E l \in Loops : phase[l] \in {"approve", "approveArmed", "live", "armed", "tool"}
    /\ phase' = [l \in Loops |-> IF phase[l] = "unborn" THEN "unborn" ELSE "dead"]
    /\ lease' = [l \in Loops |-> FALSE]
    /\ UNCHANGED <<status, rounds, suspends, reading, answers, approvedRuns,
                   approvedAfterStop, staleTools, falseFailure, retried, retryRefused>>

Next ==
    \/ \E l \in Loops : LoopStep(l) \/ Act(l) \/ ToolDone(l) \/ Approve(l) \/ ApproveAct(l)
    \/ AnswerRead \/ AnswerCommit \/ Stop \/ Sweep \/ Crash \/ Lapse \/ Retry
    \/ (status \in Terminal \cup {"awaiting"} /\ reading = 0 /\ UNCHANGED vars)

Spec == Init /\ [][Next]_vars
        /\ \A l \in Loops : WF_vars(LoopStep(l)) /\ WF_vars(Act(l)) /\ WF_vars(ToolDone(l))
                          /\ WF_vars(Approve(l)) /\ WF_vars(ApproveAct(l))
        /\ WF_vars(AnswerCommit) /\ WF_vars(Sweep)

-----------------------------------------------------------------------------
(* PROPERTIES *)

\* Every approved call runs at most once per committed answer — double clicks included.
ApprovedAtMostOnce  == approvedRuns <= answers
\* A Stop recorded before the continuation asks is honoured: the approved call does not run.
\* (A Stop landing between the question and `run_all` still lets it run — the one-step race
\* every check-then-act has; closing it would need the executor itself to ask.)
NoApprovedAfterStop == ~approvedAfterStop
\* Once the log says the run is over, a loop starts AT MOST ONE more tool — the one already
\* past its check — and then stops at its next step boundary. Without EndedMeansStop a loop
\* on a failed run keeps starting tools until its own budget runs out.
AtMostOneStaleTool  == \A l \in Loops : staleTools[l] <= 1
\* The sweep never fails a run somebody is still driving.
NoFalseFailure      == ~falseFailure
\* A retry is answered, not stopped: its loop is never told to stop unless a person did.
\* This is what the slice2 smoke checks, and what EndedMeansStop broke without the claim.
RetryNotRefused     == ~retryRefused
\* At most one loop drives a run at any moment.
OneDriver == \A l, m \in Loops : (l # m) => ~(phase[l] \in Active /\ phase[m] \in Active)
\* No run is left `running` with nobody driving it: it ends, or parks for a person.
NoOrphan == <>[](status /= "running")
=============================================================================
