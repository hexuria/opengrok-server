------------------------------ MODULE HeldSend ------------------------------
(***************************************************************************)
(* A queued send its person's Mac would carry (#292). The app fires it    *)
(* (`POST /ag-ui` with its pending id); `pending::consume_for_turn` reads *)
(* whether a Mac is connected (`Held::now`), then drains the row under    *)
(* its lock only if that read saw one: a send with no Mac to carry it     *)
(* stays queued (`heldFor: "relay_offline"`, 202). When a Mac opens its   *)
(* relay stream the server fires the send itself (`drain_held`), through  *)
(* the same check and drain. A drained send is a turn whose door then     *)
(* asks the Mac (`RelayBroker::ask`): there, or `relay_offline`.          *)
(*                                                                         *)
(* The Mac connects and leaves at any moment, a bounded number of times;  *)
(* the app fires a bounded number of times, whenever it likes. Left out:  *)
(* the thread's other runs (`drain_held` waits for an idle thread), the   *)
(* words, and a second replica: the broker is per replica, so a Mac whose *)
(* stream is on another one is no Mac here (README, stated limits).       *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS MaxFlaps,     \* connects and leaves the Mac may make
          MaxFires,     \* fires the app may make
          HoldOffline   \* FIX: a fire whose check saw no Mac leaves the send queued

VARIABLES
    mac,        \* a Mac holds the account's relay stream
    flaps,
    row,        \* the send: "pending" | "drained"
    drains,     \* turns the send became
    blind,      \* a drain went ahead though its check saw no Mac
    fires,
    app,        \* the app's fire: "idle" | "checked"
    appSaw,     \* what its check read
    trigger,    \* the reconnect drain: "idle" | "armed" | "checked"
    trigSaw,
    turn        \* the drained turn's call: "none" | "asking" | "served" | "offline"

vars == <<mac, flaps, row, drains, blind, fires, app, appSaw, trigger, trigSaw, turn>>

Init == /\ mac = FALSE /\ flaps = 0 /\ row = "pending" /\ drains = 0 /\ blind = FALSE
        /\ fires = 0 /\ app = "idle" /\ appSaw = FALSE /\ trigger = "idle" /\ trigSaw = FALSE
        /\ turn = "none"

\* The relay route: the stream opens (`RelayBroker::connect`), and `drain_held` is spawned.
Connect == /\ ~mac /\ flaps < MaxFlaps
           /\ mac' = TRUE /\ flaps' = flaps + 1 /\ trigger' = "armed"
           /\ UNCHANGED <<row, drains, blind, fires, app, appSaw, trigSaw, turn>>
Leave == /\ mac /\ flaps < MaxFlaps
         /\ mac' = FALSE /\ flaps' = flaps + 1
         /\ UNCHANGED <<row, drains, blind, fires, app, appSaw, trigger, trigSaw, turn>>

\* The row's lock, the drain's check (`matches_turn` and not held) and the drain, as one step.
Drain(saw) == IF row = "pending" /\ (saw \/ ~HoldOffline)
                THEN /\ row' = "drained" /\ drains' = drains + 1 /\ blind' = (blind \/ ~saw)
                     /\ turn' = "asking"
                ELSE UNCHANGED <<row, drains, blind, turn>>

AppCheck == /\ app = "idle" /\ fires < MaxFires
            /\ app' = "checked" /\ appSaw' = mac
            /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, trigger, trigSaw, turn>>
AppDrain == /\ app = "checked" /\ Drain(appSaw)
            /\ app' = "idle" /\ fires' = fires + 1
            /\ UNCHANGED <<mac, flaps, appSaw, trigger, trigSaw>>

\* `drain_held` → `routes::turn` → `consume_for_turn`: the same check, then the same drain.
TriggerCheck == /\ trigger = "armed"
                /\ trigger' = IF row = "pending" THEN "checked" ELSE "idle"
                /\ trigSaw' = mac
                /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, turn>>
TriggerDrain == /\ trigger = "checked" /\ Drain(trigSaw)
                /\ trigger' = "idle"
                /\ UNCHANGED <<mac, flaps, fires, app, appSaw, trigSaw>>

\* The drained turn's door call picks the account's Mac as it is now.
Ask == /\ turn = "asking"
       /\ turn' = IF mac THEN "served" ELSE "offline"
       /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, trigger, trigSaw>>

Next == Connect \/ Leave \/ AppCheck \/ AppDrain \/ TriggerCheck \/ TriggerDrain \/ Ask
        \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars
             /\ WF_vars(AppDrain) /\ WF_vars(TriggerCheck) /\ WF_vars(TriggerDrain) /\ WF_vars(Ask)

-----------------------------------------------------------------------------
\* A send is one turn at most, however the app and the server race to fire it.
DrainedOnce == drains <= 1
\* A send whose fire saw no Mac is never drained by that fire: it stays queued, held.
NeverDrainedBlind == ~blind
\* A drained send's turn finds a Mac. FAILS BY NATURE: a Mac that leaves between the check and
\* the door's call makes the one turn `relay_offline` (HeldSend_gap). Stated in README.
ServedIfDrained == turn /= "offline"
\* A Mac that comes to stay gets the send.
DrainsOnceTheMacStays == (<>[]mac) => <>(row = "drained")
=============================================================================
