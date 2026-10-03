----------------------------- MODULE RelayCall -----------------------------
(***************************************************************************)
(* One model call a person's Mac carries (#292): the door's side of it in *)
(* `RelayBroker::stream` and `Call` (opengrok-harness/src/relay.rs), the  *)
(* answers `POST /inference-relay/responses/{id}` brings                  *)
(* (`RelayBroker::answer`, `Answering::pipe`), and which of the person's  *)
(* computers it goes down, each with its own relay switch.                *)
(*                                                                         *)
(* The turn reads the switches once, as it is routed (`local_proxy::route` *)
(* on the setting's `relays`): none on is "Relay off", and nobody is      *)
(* asked. The door then picks a stream it holds of a computer the turn    *)
(* read on (`latest`), or none is `relay_offline`, and sends `infer` down *)
(* it. Meanwhile the person switches a computer off (`PATCH /local-exec/  *)
(* daemon/{id}`: its row written, then its stream told `disabled` and     *)
(* closed) or on, and a computer opens its stream (`GET /inference-relay/ *)
(* requests`: the switch read, the stream registered, the switch read     *)
(* again) or drops it. Any machine holding a daemon token may POST an     *)
(* answer to the call's id, as often as it likes: the one asked, the same *)
(* person's other machine, or another account's machine enrolled under    *)
(* the same id. The door gives up at its deadline (no first byte, or      *)
(* quiet mid-stream: one `Timeout`, time not modelled) or when the run is *)
(* stopped, and tells the asked machine to cancel. The Mac ends its       *)
(* answer, or says it failed.                                             *)
(*                                                                         *)
(* Once the call is asked no step of it reads a switch or a stream, so    *)
(* those are left as the pick found them: a computer switched off with a  *)
(* call in flight ends it as a dropped stream does (answered by its       *)
(* token, or timed out), which is what the contract asks.                 *)
(*                                                                         *)
(* Left out: the order streams opened in (the pick takes any candidate,   *)
(* which covers the latest), the SSE bytes (a `Deliver` is one piece      *)
(* reaching the run), the reply codes, revocation, and other replicas: a  *)
(* computer switched off through one is `SwitchOff` whose `Close` never   *)
(* comes here.                                                            *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS Machines,       \* every daemon-token holder that may POST
          Mine,           \* the person's own computers, the ones the door may pick
          MaxPosts,       \* answers each machine may try
          MaxSwitches,    \* switches flipped in all, to bound the search
          CheckMachine,   \* FIX: an answer is taken only from the machine the call went to
          ForgetOnGiveUp, \* FIX: a call given up is forgotten, so a late answer is refused
          SkipOff,        \* FIX: the pick skips a computer the turn read off, its stream open or not
          Recheck         \* FIX: an opening stream reads the switch again once registered

VARIABLES
    on,           \* each of Mine's switch, as `local_exec_daemon.relay_enabled` holds it
    stream,       \* each of Mine's relay stream in this broker: "none" | "open"
    opening,      \* each of Mine's stream being opened: "idle" | "read" (on, then) | "registered"
    closing,      \* each of Mine's switch-off whose row is written and whose stream is not closed
    flips,        \* switches flipped so far
    read,         \* the computers whose switch was on as the turn read them; {} before
    asked,        \* the machine the call went down, or "none"
    door,         \* "routing" | "picking" | "waiting" (infer sent) | "streaming" | "done"
    ending,       \* "none" | "relayOff" | "offline" | "answered" | "failed" | "timedOut" | "stopped"
    ask,          \* the broker's entry: "none" (nobody asked) | "open" (answer untaken) | "taken" | "gone"
    answerer,     \* the machine whose answer was taken, or "none"
    takes,        \* answers taken
    takenLate,    \* an answer taken after the door gave up
    posts,        \* answers tried, per machine
    delivered,    \* the machines whose answer reached the run
    cancels       \* cancel frames sent to the asked machine

switches == <<on, stream, opening, closing, flips>>
call == <<read, asked, door, ending, ask, answerer, takes, takenLate, posts, delivered, cancels>>
vars == <<switches, call>>

\* Every computer starts on, as one enrolled does and as every one enrolled before the switch read.
Init == /\ on = [m \in Mine |-> TRUE] /\ stream = [m \in Mine |-> "none"]
        /\ opening = [m \in Mine |-> "idle"] /\ closing = [m \in Mine |-> FALSE] /\ flips = 0
        /\ read = {} /\ asked = "none" /\ door = "routing" /\ ending = "none" /\ ask = "none"
        /\ answerer = "none" /\ takes = 0 /\ takenLate = FALSE /\ posts = [m \in Machines |-> 0]
        /\ delivered = {} /\ cancels = 0

-----------------------------------------------------------------------------
\* THE SWITCHES AND THE STREAMS, until the call is asked.
Before == door \in {"routing", "picking"}

\* `switch_relay` (or `switch_every` for the account's `relayEnabled`): the row is written
\* (`set_relay`), and only after it the stream is told `disabled` and closed (`Close`).
SwitchOff(m) ==
    /\ Before /\ on[m] /\ flips < MaxSwitches
    /\ on' = [on EXCEPT ![m] = FALSE] /\ closing' = [closing EXCEPT ![m] = TRUE]
    /\ flips' = flips + 1
    /\ UNCHANGED <<stream, opening, call>>

\* `RelayBroker::disable`: the stream, if one is held, is sent `disabled` and dropped.
Close(m) ==
    /\ Before /\ closing[m]
    /\ closing' = [closing EXCEPT ![m] = FALSE] /\ stream' = [stream EXCEPT ![m] = "none"]
    /\ UNCHANGED <<on, opening, flips, call>>

SwitchOn(m) ==
    /\ Before /\ ~on[m] /\ flips < MaxSwitches
    /\ on' = [on EXCEPT ![m] = TRUE] /\ flips' = flips + 1
    /\ UNCHANGED <<stream, opening, closing, call>>

\* `relay_requests`' first read (`relaying`): a computer whose switch is off is answered 409
\* `relay_disabled` before any frame, which changes nothing here.
Open(m) ==
    /\ Before /\ on[m] /\ opening[m] = "idle" /\ stream[m] = "none"
    /\ opening' = [opening EXCEPT ![m] = "read"]
    /\ UNCHANGED <<on, stream, closing, flips, call>>

\* `RelayBroker::connect`: the stream is in the broker; without the second read the open is done.
Register(m) ==
    /\ Before /\ opening[m] = "read"
    /\ stream' = [stream EXCEPT ![m] = "open"]
    /\ opening' = [opening EXCEPT ![m] = IF Recheck THEN "registered" ELSE "idle"]
    /\ UNCHANGED <<on, closing, flips, call>>

\* The second read: a switch turned off meanwhile drops the stream before it is sent a frame.
Reread(m) ==
    /\ Before /\ opening[m] = "registered"
    /\ opening' = [opening EXCEPT ![m] = "idle"]
    /\ stream' = [stream EXCEPT ![m] = IF on[m] THEN "open" ELSE "none"]
    /\ UNCHANGED <<on, closing, flips, call>>

\* The Mac hangs up, or sleeps.
HangUp(m) ==
    /\ Before /\ stream[m] = "open" /\ opening[m] = "idle"
    /\ stream' = [stream EXCEPT ![m] = "none"]
    /\ UNCHANGED <<on, opening, closing, flips, call>>

-----------------------------------------------------------------------------
\* THE CALL.

\* `local_proxy::route`: the switches are read once, with the setting. None on is "Relay off":
\* the fallback, or a refusal in words, and no computer asked.
Route ==
    /\ door = "routing"
    /\ read' = {m \in Mine : on[m]}
    /\ IF read' = {} THEN door' = "done" /\ ending' = "relayOff"
                     ELSE door' = "picking" /\ UNCHANGED ending
    /\ UNCHANGED <<switches, asked, ask, answerer, takes, takenLate, posts, delivered, cancels>>

\* `RelayBroker::ask` through `latest`: a stream held of a computer the turn read on, or none,
\* `relay_offline`. Without the skip, any stream of the person's that is held.
Pick ==
    /\ door = "picking"
    /\ LET held == {m \in Mine : stream[m] = "open" /\ (SkipOff => m \in read)}
       IN IF held = {}
            THEN door' = "done" /\ ending' = "offline" /\ UNCHANGED <<asked, ask>>
            ELSE \E m \in held : asked' = m /\ ask' = "open" /\ door' = "waiting"
                                 /\ UNCHANGED ending
    /\ UNCHANGED <<switches, read, answerer, takes, takenLate, posts, delivered, cancels>>

\* `answer`: the entry, the machine check and the one `take()` of its answer sender, under the
\* broker's lock. A refused answer (404, 409, 401) takes nothing.
Post(m) ==
    /\ posts[m] < MaxPosts
    /\ posts' = [posts EXCEPT ![m] = @ + 1]
    /\ IF ask = "open" /\ (~CheckMachine \/ m = asked)
         THEN /\ ask' = "taken" /\ answerer' = m /\ takes' = takes + 1
              /\ takenLate' = (door = "done")
              /\ door' = IF door = "waiting" THEN "streaming" ELSE door
         ELSE UNCHANGED <<ask, answerer, takes, takenLate, door>>
    /\ UNCHANGED <<switches, read, asked, ending, delivered, cancels>>

\* `Answering::pipe` → the door's receiver: a piece reaches the run only while the door reads,
\* since the door drops its receiver when it is done.
Deliver ==
    /\ ask = "taken" /\ door = "streaming"
    /\ delivered' = delivered \cup {answerer}
    /\ UNCHANGED <<switches, read, asked, door, ending, ask, answerer, takes, takenLate, posts,
                   cancels>>

\* The answer's body ends (the Mac finished: no cancel), or it was `{error}`: `relay_failed`.
Ends(how) ==
    /\ door = "streaming"
    /\ door' = "done" /\ ending' = how /\ ask' = "gone"
    /\ UNCHANGED <<switches, read, asked, answerer, takes, takenLate, posts, delivered, cancels>>

\* The door gives up: `Call::bytes` past its clock (`relay_timeout`), or `RelayBroker::stop` for
\* a stopped run. Either way the asked machine is told to cancel, once (`forget`, `cancelled`).
GiveUp(why) ==
    /\ door \in {"waiting", "streaming"}
    /\ door' = "done" /\ ending' = why /\ cancels' = cancels + 1
    /\ ask' = IF ForgetOnGiveUp THEN "gone" ELSE ask
    /\ UNCHANGED <<switches, read, asked, answerer, takes, takenLate, posts, delivered>>

Done == door = "done" /\ \A m \in Machines : posts[m] = MaxPosts /\ UNCHANGED vars

Next == \/ \E m \in Mine : \/ SwitchOff(m) \/ Close(m) \/ SwitchOn(m)
                           \/ Open(m) \/ Register(m) \/ Reread(m) \/ HangUp(m)
        \/ Route \/ Pick
        \/ \E m \in Machines : Post(m)
        \/ Deliver \/ Ends("answered") \/ Ends("failed")
        \/ GiveUp("timedOut") \/ GiveUp("stopped")
        \/ Done
\* A turn is routed and picks; the door's clock always runs out on a call nobody finishes.
Spec == Init /\ [][Next]_vars /\ WF_vars(Route) /\ WF_vars(Pick) /\ WF_vars(GiveUp("timedOut"))

-----------------------------------------------------------------------------
TypeOK == /\ on \in [Mine -> BOOLEAN] /\ stream \in [Mine -> {"none", "open"}]
          /\ opening \in [Mine -> {"idle", "read", "registered"}]
          /\ closing \in [Mine -> BOOLEAN] /\ read \subseteq Mine /\ asked \in Mine \cup {"none"}
          /\ door \in {"routing", "picking", "waiting", "streaming", "done"}
          /\ ending \in {"none", "relayOff", "offline", "answered", "failed", "timedOut", "stopped"}
          /\ ask \in {"none", "open", "taken", "gone"}

\* A DISABLED MACHINE IS NEVER ASKED: a call goes down the stream of a computer whose switch was
\* on as its turn read them, though one switched off may still hold a stream here: its close not
\* yet come, an open between its two reads, another replica's switch.
NeverAskedDisabled == asked \in read \cup {"none"}
\* NOR DOES ONE KEEP A STREAM: once its switch-off has closed what it held and no open of its is
\* under way, a computer whose switch is off holds no stream here.
NoStreamWhileOff ==
    \A m \in Mine : (~closing[m] /\ opening[m] = "idle" /\ stream[m] = "open") => on[m]
\* A relayed answer is only ever piped into the call that asked, from the machine it asked.
OnlyTheAsked == delivered \subseteq {asked}
\* One answer per call, whoever tries again.
AnsweredOnce == takes <= 1
\* A call given up on takes no answer after: a late one is refused (404), not streamed into nothing.
NothingTakenAfterGivingUp == ~takenLate
\* A call given up on while the Mac may still be working is cancelled there, and only once; one
\* never asked, or answered, is not.
CancelledOnceIfGivenUp ==
    /\ cancels <= 1
    /\ (door = "done" /\ ending \in {"timedOut", "stopped"}) => cancels = 1
    /\ (ending \in {"answered", "failed", "relayOff", "offline"}) => cancels = 0
\* Every call ends.
EveryCallEnds == <>(door = "done")
=============================================================================
