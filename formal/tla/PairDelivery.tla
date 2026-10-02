----------------------------- MODULE PairDelivery -----------------------------
(***************************************************************************)
(* A message one Bot sends another (#314): the durable `bot_message`      *)
(* outbox and the receiving Bot's turn in the pair's side thread.         *)
(*                                                                         *)
(* A sender's `message_bot` call writes one outbox row per receiver, in   *)
(* one transaction (`enqueue_bot_messages`, opengrok-store `pairs.rs`),   *)
(* unique on (sender run, call, receiver), and asks the pair to drain     *)
(* (`pairs::drain_soon`). A drain takes the pair's lock, looks whether a   *)
(* run of the pair is still in flight, and if not claims the oldest       *)
(* queued row (`claim_pair_message`); then it starts that row's run       *)
(* (`pairs::fire`), whose id is the row's, minted when the row was        *)
(* written, so its `Started` commits once (`StoreJournal::claim`, the     *)
(* argument of Lean `Answer.at_most_one_commit`). A run that cannot be    *)
(* had (its Bot retired, on its person's own plan, …) is still started:   *)
(* journaled and failed at once, in words, in the pair's thread, which is *)
(* how the person sees that the message was refused. When a pair's run    *)
(* ends, its pair drains again (`pairs::ended`, from the ending's write). *)
(* The recovery sweep drains every pair and starts any claimed row whose  *)
(* run never began, its drain having died with its process               *)
(* (`pairs::sweep`, from `recovery::sweep_once`).                         *)
(*                                                                         *)
(* One pair: pairs never share a lock, a row or a run. The sends are all  *)
(* into its thread, in either direction. A sender's call may carry its    *)
(* enqueue out again (a replay standing for any second run of one call,   *)
(* which the harness itself never makes: a crash mid-tool fails the run,  *)
(* `NothingReExecutes`). The process may crash and restart, a bounded     *)
(* number of times, losing every drain in memory. Left out: the words,    *)
(* the caps (Lean `Chain`), who may message whom, and a run's own steps   *)
(* (`RunLifecycle`): here a run is started, then ends.                    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Sends,          \* each one (sender run, call, receiver), all into the pair's thread
          MaxReplays,     \* times a send's call may carry its enqueue out again
          MaxCrashes,     \* restarts of the process
          None,
          IdemKey,        \* FIX: a row is unique on (sender run, call, receiver)
          PairClaim,      \* FIX: a drain takes the pair's lock before it looks and claims
          RunEndTrigger   \* FIX: a pair run's ending drains its pair again

Rows == 1..(Cardinality(Sends) * (MaxReplays + 1))
\* Drains asked for and not yet looking. One more than two changes nothing a drain can do.
MaxArmed == 2

VARIABLES
    send,     \* per row slot, in the order rows are written: the send it carries, or None
    st,       \* per row: "free" | "queued" | "claimed" (the outbox's `queued` | `started`)
    run,      \* per row: its run: "none" | "live" | "ended" (a refusal ends at once)
    calls,    \* per send: the times its call carried the enqueue out
    armed,    \* drains spawned and not yet looking
    checked,  \* without the claim: drains that saw the pair idle and are about to claim
    holds,    \* per row: a drain claimed it and is about to start its run
    up,
    crashes,
    owed      \* the process restarted and the sweep has not run since

vars == <<send, st, run, calls, armed, checked, holds, up, crashes, owed>>

Init == /\ send = [r \in Rows |-> None] /\ st = [r \in Rows |-> "free"]
        /\ run = [r \in Rows |-> "none"] /\ calls = [s \in Sends |-> 0]
        /\ armed = 0 /\ checked = 0 /\ holds = [r \in Rows |-> FALSE]
        /\ up = TRUE /\ crashes = 0 /\ owed = FALSE

Capped(n) == IF n > MaxArmed THEN MaxArmed ELSE n
Arm(n) == armed' = Capped(armed + n)

\* In flight: claimed, and its run not ended — not begun yet, live, or parked on a card.
InFlight(r) == st[r] = "claimed" /\ run[r] # "ended"
Busy == \E r \in Rows : InFlight(r)
Queued == {r \in Rows : st[r] = "queued"}
Oldest == CHOOSE r \in Queued : \A q \in Queued : r <= q
FreeSlot == CHOOSE r \in Rows : st[r] = "free" /\ \A q \in Rows : st[q] = "free" => r <= q

\* `message_bot`: the rows, in one transaction, then a drain of the pair. With the key, a call
\* carried out again finds its row and writes nothing (`on conflict do nothing`).
Enqueue(s) ==
    /\ up /\ calls[s] < MaxReplays + 1
    /\ calls' = [calls EXCEPT ![s] = @ + 1]
    /\ IF IdemKey /\ \E r \in Rows : send[r] = s
         THEN UNCHANGED <<send, st>>
         ELSE /\ send' = [send EXCEPT ![FreeSlot] = s]
              /\ st' = [st EXCEPT ![FreeSlot] = "queued"]
    /\ Arm(1)
    /\ UNCHANGED <<run, checked, holds, up, crashes, owed>>

\* `claim_pair_message`: the pair's advisory lock, as its own statement, then the look and the
\* claim of the oldest queued row, committed together.
Drain ==
    /\ PairClaim /\ up /\ armed > 0
    /\ armed' = armed - 1
    /\ IF ~Busy /\ Queued # {}
         THEN /\ st' = [st EXCEPT ![Oldest] = "claimed"]
              /\ holds' = [holds EXCEPT ![Oldest] = TRUE]
         ELSE UNCHANGED <<st, holds>>
    /\ UNCHANGED <<send, run, calls, checked, up, crashes, owed>>

\* Without the lock the look and the claim are two statements, each on its own snapshot.
Look ==
    /\ ~PairClaim /\ up /\ armed > 0
    /\ armed' = armed - 1
    /\ checked' = IF ~Busy /\ Queued # {} THEN Capped(checked + 1) ELSE checked
    /\ UNCHANGED <<send, st, run, calls, holds, up, crashes, owed>>
Claim ==
    /\ ~PairClaim /\ up /\ checked > 0
    /\ checked' = checked - 1
    /\ IF Queued # {}
         THEN /\ st' = [st EXCEPT ![Oldest] = "claimed"]
              /\ holds' = [holds EXCEPT ![Oldest] = TRUE]
         ELSE UNCHANGED <<st, holds>>
    /\ UNCHANGED <<send, run, calls, armed, up, crashes, owed>>

\* A run is begun once: its id is the row's, and `Started` commits only at the first seq. `how`
\* is "ended" for a refusal, journaled and failed at once; its ending drains the pair like any.
Begin(r, how) ==
    /\ run' = [run EXCEPT ![r] = how]
    /\ IF how = "ended" /\ RunEndTrigger THEN Arm(1) ELSE UNCHANGED armed

\* `pairs::fire`, by the drain that claimed the row. A row the sweep began first is not begun
\* again: the claim of its run id finds the run there.
Start(r, how) ==
    /\ up /\ holds[r]
    /\ holds' = [holds EXCEPT ![r] = FALSE]
    /\ IF run[r] = "none" THEN Begin(r, how) ELSE UNCHANGED <<run, armed>>
    /\ UNCHANGED <<send, st, calls, checked, up, crashes, owed>>

\* `pairs::sweep`: a claimed row whose run never began, its drain gone (or slow past the grace).
SweepStart(r, how) ==
    /\ up /\ st[r] = "claimed" /\ run[r] = "none"
    /\ Begin(r, how)
    /\ UNCHANGED <<send, st, calls, checked, holds, up, crashes, owed>>

\* The run's ending is written, by its loop, a Stop or the recovery sweep; then its pair drains.
RunEnds(r) ==
    /\ up /\ run[r] = "live"
    /\ run' = [run EXCEPT ![r] = "ended"]
    /\ IF RunEndTrigger THEN Arm(1) ELSE UNCHANGED armed
    /\ UNCHANGED <<send, st, calls, checked, holds, up, crashes, owed>>

\* Every drain in memory dies with the process; rows and runs are in Postgres. A run that was
\* live is the recovery sweep's to carry on or fail, either of which ends it (`RunEnds`).
Crash ==
    /\ up /\ crashes < MaxCrashes
    /\ up' = FALSE /\ crashes' = crashes + 1
    /\ armed' = 0 /\ checked' = 0 /\ holds' = [r \in Rows |-> FALSE]
    /\ UNCHANGED <<send, st, run, calls, owed>>
\* `recovery::sweep_forever` sweeps at once on start, so a restart owes one.
Restart ==
    /\ ~up /\ up' = TRUE /\ owed' = TRUE
    /\ UNCHANGED <<send, st, run, calls, armed, checked, holds, crashes>>
\* Every `SWEEP_INTERVAL`, `pairs::sweep` drains the pair.
Sweep ==
    /\ up
    /\ owed' = FALSE
    /\ Arm(1)
    /\ UNCHANGED <<send, st, run, calls, checked, holds, up, crashes>>

Next == \/ \E s \in Sends : Enqueue(s)
        \/ Drain \/ Look \/ Claim
        \/ \E r \in Rows, how \in {"live", "ended"} : Start(r, how) \/ SweepStart(r, how)
        \/ \E r \in Rows : RunEnds(r)
        \/ Crash \/ Restart \/ Sweep
Spec == Init /\ [][Next]_vars
             /\ WF_vars(Drain) /\ WF_vars(Look) /\ WF_vars(Claim) /\ WF_vars(Restart)
             /\ WF_vars(Sweep)
             /\ \A r \in Rows : /\ WF_vars(\E how \in {"live", "ended"} : Start(r, how))
                                /\ WF_vars(\E how \in {"live", "ended"} : SweepStart(r, how))
                                /\ WF_vars(RunEnds(r))

-----------------------------------------------------------------------------
TypeOK == /\ send \in [Rows -> Sends \cup {None}]
          /\ st \in [Rows -> {"free", "queued", "claimed"}]
          /\ run \in [Rows -> {"none", "live", "ended"}]
          /\ calls \in [Sends -> 0..(MaxReplays + 1)]
          /\ armed \in 0..MaxArmed /\ checked \in 0..MaxArmed
          /\ holds \in [Rows -> BOOLEAN]
          /\ up \in BOOLEAN /\ crashes \in 0..MaxCrashes /\ owed \in BOOLEAN

\* A message starts at most one run, however often its call is carried out.
OneRunPerSend == \A s \in Sends : Cardinality({r \in Rows : send[r] = s /\ run[r] # "none"}) <= 1
\* At most one run of a pair's thread is in flight, its claim and its start counted.
OnePerPair == Cardinality({r \in Rows : InFlight(r)}) <= 1
\* A message waiting on an idle pair has a drain coming, unless a restart has left it to the
\* sweep: with the process up and the sweep run, nothing queued waits for the next sweep.
NeverStranded == (up /\ ~owed /\ Queued # {} /\ ~Busy) => armed + checked > 0
\* Every message is started, or refused in words, which is a run that ended at once.
EveryMessageStarted == \A r \in Rows : (st[r] # "free") ~> (run[r] # "none")
=============================================================================
