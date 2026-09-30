----------------------------- MODULE RelayCall -----------------------------
(***************************************************************************)
(* One model call a person's Mac carries (#292): the door's side of it in *)
(* `RelayBroker::stream` and `Call` (opengrok-harness/src/relay.rs), and *)
(* the answers `POST /inference-relay/responses/{id}` brings              *)
(* (`RelayBroker::answer`, `Answering::pipe`).                            *)
(*                                                                         *)
(* The door sends `infer` down the stream of the machine it picked, the   *)
(* account's latest (`Asked`), and waits. Any machine holding a daemon    *)
(* token may POST an answer to the call's id, as often as it likes: the   *)
(* one asked, the same person's other machine, or another account's       *)
(* machine enrolled under the same id. The door gives up at its deadline  *)
(* (no first byte, or quiet mid-stream: one `Timeout`, time not modelled) *)
(* or when the run is stopped, and tells the asked machine to cancel. The *)
(* Mac ends its answer, or says it failed.                                *)
(*                                                                         *)
(* Left out: which machine is latest (one call picks once), the SSE       *)
(* bytes (a `Deliver` is one piece reaching the run), the reply codes.    *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS Machines,       \* every daemon-token holder that may POST
          Asked,          \* the machine the call went down
          MaxPosts,       \* answers each machine may try
          CheckMachine,   \* FIX: an answer is taken only from the machine the call went to
          ForgetOnGiveUp  \* FIX: a call given up is forgotten, so a late answer is refused

VARIABLES
    door,         \* "waiting" (infer sent) | "streaming" (an answer taken) | "done"
    ending,       \* "none" | "answered" | "failed" | "timedOut" | "stopped"
    ask,          \* the broker's entry: "open" (answer untaken) | "taken" | "gone"
    answerer,     \* the machine whose answer was taken, or "none"
    takes,        \* answers taken
    takenLate,    \* an answer taken after the door gave up
    posts,        \* answers tried, per machine
    delivered,    \* the machines whose answer reached the run
    cancels       \* cancel frames sent to the asked machine

vars == <<door, ending, ask, answerer, takes, takenLate, posts, delivered, cancels>>

Init == /\ door = "waiting" /\ ending = "none" /\ ask = "open" /\ answerer = "none"
        /\ takes = 0 /\ takenLate = FALSE /\ posts = [m \in Machines |-> 0]
        /\ delivered = {} /\ cancels = 0

\* `answer`: the entry, the machine check and the one `take()` of its answer sender, under the
\* broker's lock. A refused answer (404, 409, 401) takes nothing.
Post(m) ==
    /\ posts[m] < MaxPosts
    /\ posts' = [posts EXCEPT ![m] = @ + 1]
    /\ IF ask = "open" /\ (~CheckMachine \/ m = Asked)
         THEN /\ ask' = "taken" /\ answerer' = m /\ takes' = takes + 1
              /\ takenLate' = (door = "done")
              /\ door' = IF door = "waiting" THEN "streaming" ELSE door
         ELSE UNCHANGED <<ask, answerer, takes, takenLate, door>>
    /\ UNCHANGED <<ending, delivered, cancels>>

\* `Answering::pipe` → the door's receiver: a piece reaches the run only while the door reads,
\* since the door drops its receiver when it is done.
Deliver ==
    /\ ask = "taken" /\ door = "streaming"
    /\ delivered' = delivered \cup {answerer}
    /\ UNCHANGED <<door, ending, ask, answerer, takes, takenLate, posts, cancels>>

\* The answer's body ends (the Mac finished: no cancel), or it was `{error}`: `relay_failed`.
Ends(how) ==
    /\ door = "streaming"
    /\ door' = "done" /\ ending' = how /\ ask' = "gone"
    /\ UNCHANGED <<answerer, takes, takenLate, posts, delivered, cancels>>

\* The door gives up: `Call::bytes` past its clock (`relay_timeout`), or `RelayBroker::stop` for
\* a stopped run. Either way the asked machine is told to cancel, once (`forget`, `cancelled`).
GiveUp(why) ==
    /\ door \in {"waiting", "streaming"}
    /\ door' = "done" /\ ending' = why /\ cancels' = cancels + 1
    /\ ask' = IF ForgetOnGiveUp THEN "gone" ELSE ask
    /\ UNCHANGED <<answerer, takes, takenLate, posts, delivered>>

Done == door = "done" /\ \A m \in Machines : posts[m] = MaxPosts /\ UNCHANGED vars

Next == \/ \E m \in Machines : Post(m)
        \/ Deliver \/ Ends("answered") \/ Ends("failed")
        \/ GiveUp("timedOut") \/ GiveUp("stopped")
        \/ Done
\* The door's clock always runs out on a call nobody finishes.
Spec == Init /\ [][Next]_vars /\ WF_vars(GiveUp("timedOut"))

-----------------------------------------------------------------------------
TypeOK == /\ door \in {"waiting", "streaming", "done"}
          /\ ending \in {"none", "answered", "failed", "timedOut", "stopped"}
          /\ ask \in {"open", "taken", "gone"}

\* A relayed answer is only ever piped into the call that asked, from the machine it asked.
OnlyTheAsked == delivered \subseteq {Asked}
\* One answer per call, whoever tries again.
AnsweredOnce == takes <= 1
\* A call given up on takes no answer after: a late one is refused (404), not streamed into nothing.
NothingTakenAfterGivingUp == ~takenLate
\* A call given up on while the Mac may still be working is cancelled there, and only once.
CancelledOnceIfGivenUp ==
    /\ cancels <= 1
    /\ (door = "done" /\ ending \in {"timedOut", "stopped"}) => cancels = 1
    /\ (ending \in {"answered", "failed"}) => cancels = 0
\* Every call ends.
EveryCallEnds == <>(door = "done")
=============================================================================
