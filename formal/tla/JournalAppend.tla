---------------------------- MODULE JournalAppend ----------------------------
(***************************************************************************)
(* One journal write racing a Stop, at the level the store sees it:       *)
(* `append_events` (opengrok-server/src/agui/routes.rs) loads the run,    *)
(* decides what to append, and inserts at the next stream_seq; the unique *)
(* (stream_id, stream_seq) makes the insert a compare-and-set, so a write *)
(* that lost the race gets `Conflict` and wrote nothing                   *)
(* (opengrok-store/src/postgres.rs append_run, one transaction).          *)
(*                                                                         *)
(* `stop_run` already retries a Conflict (routes.rs STOP_ATTEMPTS); the   *)
(* journal did not. Policy is the candidate: never retry (the code at     *)
(* 4a25af6), retry a Conflict only, or retry every error. An error is not *)
(* always a failure: a commit can land and the reply still be lost        *)
(* (Ambiguous), and only a Conflict is known to have written nothing.     *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS Policy,       \* "none" | "conflict" | "any"
          Ambiguous,    \* may a commit land and its reply be lost?
          MaxTries      \* attempts the loop's write may make

VARIABLES
    log,        \* the run's stream: "round" (the loop's frames) and "stop"
    lp, lseen, ltries,   \* the loop's write: "load" | "commit" | "done"; the seq it read; attempts
    sp, sseen,           \* the Stop's write: "idle" | "load" | "commit" | "done"
    told                 \* what the loop's caller was told: "none" | "ok" | "err"

vars == <<log, lp, lseen, ltries, sp, sseen, told>>

Count(x) == Len(SelectSeq(log, LAMBDA e : e = x))

Init == /\ log = <<>> /\ lp = "load" /\ lseen = 0 /\ ltries = 0
        /\ sp = "idle" /\ sseen = 0 /\ told = "none"

\* The loop's write: load...
LLoad == /\ lp = "load" /\ lp' = "commit" /\ lseen' = Len(log) /\ ltries' = ltries + 1
         /\ UNCHANGED <<log, sp, sseen, told>>

\* ...then insert at the seq it read. Frames are still accepted on a stopped run (run.rs Emit),
\* so the only way this write fails is losing the race.
Retry(ok) == IF /\ ltries < MaxTries
                /\ \/ Policy = "any"
                   \/ Policy = "conflict" /\ ~ok      \* ~ok: known to have written nothing
             THEN lp' = "load" /\ UNCHANGED told
             ELSE lp' = "done" /\ told' = "err"
LCommit ==
    /\ lp = "commit"
    /\ IF Len(log) = lseen
         THEN /\ log' = Append(log, "round")
              /\ \/ lp' = "done" /\ told' = "ok"
                 \/ Ambiguous /\ Retry(TRUE)       \* it landed; the reply did not
         ELSE /\ UNCHANGED log /\ Retry(FALSE)     \* Conflict: nothing was written
    /\ UNCHANGED <<lseen, ltries, sp, sseen>>

\* The person presses Stop; `stop_run` loads, appends, and retries its own Conflicts.
Press  == sp = "idle" /\ sp' = "load" /\ UNCHANGED <<log, lp, lseen, ltries, sseen, told>>
SLoad  == sp = "load" /\ sp' = "commit" /\ sseen' = Len(log) /\ UNCHANGED <<log, lp, lseen, ltries, told>>
SCommit ==
    /\ sp = "commit"
    /\ IF Len(log) = sseen THEN log' = Append(log, "stop") /\ sp' = "done"
                           ELSE UNCHANGED log /\ sp' = "load"
    /\ UNCHANGED <<lp, lseen, ltries, sseen, told>>

Next == LLoad \/ LCommit \/ Press \/ SLoad \/ SCommit
        \/ (lp = "done" /\ sp \in {"idle", "done"} /\ UNCHANGED vars)
Spec == Init /\ [][Next]_vars /\ WF_vars(LLoad \/ LCommit \/ SLoad \/ SCommit)

-----------------------------------------------------------------------------
\* The round the person was looking at when they pressed Stop is in the log. run.rs accepts
\* frames on a stopped run for exactly this reason; losing the race must not undo that.
RoundKept   == (lp = "done") => Count("round") >= 1
\* No frame is written twice.
NoDuplicate == Count("round") <= 1
\* The caller is told the truth: "ok" only if the round landed, "err" only if it did not.
ToldTruth   == /\ (told = "ok")  => Count("round") = 1
               /\ (told = "err") => Count("round") = 0
\* Every write finishes.
Finishes    == <>(lp = "done")
=============================================================================
