----------------------------- MODULE RecipeLease -----------------------------
(***************************************************************************)
(* Starting a recipe run on one bot (#227): `start_recipe_run`            *)
(* (opengrok-store/src/postgres.rs) inserts the run with a lease only     *)
(* where no row of that bot holds a live lease. Under READ COMMITTED the  *)
(* `not exists` reads a snapshot taken when the statement begins, and a   *)
(* row another start has inserted but not committed is not in it. The    *)
(* fix takes `pg_advisory_xact_lock` on the bot, as its own statement,    *)
(* before the insert, and holds it to commit.                             *)
(*                                                                         *)
(* A start is: lock (when UseLock), take the snapshot, insert if the      *)
(* snapshot held no live lease, commit (which releases the lock). A run   *)
(* that lands (`record_recipe_run`) clears its lease. The lease never     *)
(* lapses here: that it is not fenced once it does is the limit           *)
(* `RunLifecycle_lapse` already states for the agent run's lease.         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS Starters,     \* the page's run button, the chat tool, a workflow: anything that starts
          UseLock,      \* the per-bot advisory lock around the check and the insert
          None

VARIABLES
    pc,         \* per starter: "idle" | "locked" | "read" | "wrote" | "done"
    holder,     \* who holds the bot's advisory lock, or None
    sawLive,    \* per starter: did its snapshot hold a live lease?
    pending,    \* inserted, not yet committed: invisible to every other snapshot
    live        \* committed rows whose lease is held

vars == <<pc, holder, sawLive, pending, live>>

Init == /\ pc = [s \in Starters |-> "idle"] /\ holder = None
        /\ sawLive = [s \in Starters |-> FALSE] /\ pending = {} /\ live = {}

\* `select pg_advisory_xact_lock(class, hashtext(bot))`: blocks while another start holds it.
Lock(s) == /\ pc[s] = "idle"
           /\ IF UseLock THEN holder = None /\ holder' = s ELSE UNCHANGED holder
           /\ pc' = [pc EXCEPT ![s] = "locked"]
           /\ UNCHANGED <<sawLive, pending, live>>

\* The insert statement begins: its snapshot sees committed rows only.
Read(s) == /\ pc[s] = "locked"
           /\ sawLive' = [sawLive EXCEPT ![s] = live # {}]
           /\ pc' = [pc EXCEPT ![s] = "read"]
           /\ UNCHANGED <<holder, pending, live>>

\* `insert … where not exists (live lease)`, judged on that snapshot.
Insert(s) == /\ pc[s] = "read"
             /\ pending' = IF sawLive[s] THEN pending ELSE pending \cup {s}
             /\ pc' = [pc EXCEPT ![s] = "wrote"]
             /\ UNCHANGED <<holder, sawLive, live>>

\* `tx.commit()`: the row becomes visible and the transaction's lock is released.
Commit(s) == /\ pc[s] = "wrote"
             /\ live' = live \cup (pending \cap {s})
             /\ pending' = pending \ {s}
             /\ holder' = IF holder = s THEN None ELSE holder
             /\ pc' = [pc EXCEPT ![s] = "done"]
             /\ UNCHANGED sawLive

\* The run lands: `record_recipe_run` finishes the row and nulls its lease.
Land(s) == /\ s \in live
           /\ live' = live \ {s}
           /\ UNCHANGED <<pc, holder, sawLive, pending>>

Next == \/ \E s \in Starters : Lock(s) \/ Read(s) \/ Insert(s) \/ Commit(s) \/ Land(s)
        \/ (\A s \in Starters : pc[s] = "done") /\ live = {} /\ UNCHANGED vars
Spec == Init /\ [][Next]_vars
             /\ \A s \in Starters : WF_vars(Lock(s) \/ Read(s) \/ Insert(s) \/ Commit(s))

-----------------------------------------------------------------------------
\* Two recipes never click on one screen: at most one run of a bot holds a live lease.
AtMostOneLive == Cardinality(live) <= 1
\* Every start answers, won or refused.
EveryStartAnswers == <>(\A s \in Starters : pc[s] = "done")
=============================================================================
