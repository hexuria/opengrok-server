------------------------------ MODULE HeldSend ------------------------------
(***************************************************************************)
(* A queued send its person's Mac would carry (#292). The app fires it    *)
(* (`POST /ag-ui` with its pending id); `pending::consume_for_turn` reads *)
(* whether a Mac is connected (`Held::now`), then drains the row under    *)
(* its lock only if that read saw one: a send with no Mac to carry it     *)
(* stays queued (`heldFor: "relay_offline"`, 202). When a Mac opens its   *)
(* relay stream the server sends it too (`drain_held`), through the same  *)
(* check and drain, once nothing runs on the thread: `send_held` waits a  *)
(* turn in flight out while the Mac stays, and one sending per thread     *)
(* runs at a time, a trigger that finds it running having it go round     *)
(* again (`RelayBroker::draining`, `drained`). A drained send is a turn   *)
(* whose door then asks the Mac (`RelayBroker::ask`): there, or           *)
(* `relay_offline`.                                                       *)
(*                                                                         *)
(* The Mac connects and leaves at any moment, a bounded number of times;  *)
(* the app fires a bounded number of times, whenever it likes; the person *)
(* starts a bounded number of other turns on the thread, each of which    *)
(* ends. Left out: the words; which run is the thread's (`thread_now`:    *)
(* its newest in sight, else its most recently hidden, and none at all is *)
(* idle); and a second replica: the broker is per replica, so a Mac whose *)
(* stream is on another one is no Mac here (README, stated limits).       *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS MaxFlaps,     \* connects and leaves the Mac may make
          MaxFires,     \* fires the app may make
          MaxTurns,     \* other turns the person starts on the thread
          HoldOffline,  \* FIX: a fire whose check saw no Mac leaves the send queued
          WaitsOut,     \* FIX (#298 review): a turn in flight is waited out, not given up on
          ReArms        \* FIX: a trigger that finds the sending running has it go round again

VARIABLES
    mac,        \* a Mac holds the account's relay stream
    flaps,
    row,        \* the send: "pending" | "drained"
    drains,     \* turns the send became
    blind,      \* a drain went ahead though its check saw no Mac
    fires,
    app,        \* the app's fire: "idle" | "checked"
    appSaw,     \* what its check read
    trigger,    \* the thread's sending: "idle" | "armed" | "checked" | "leaving"
    trigSaw,
    again,      \* a trigger came while the sending ran
    busy,       \* another turn is in flight on the thread
    turns,
    turn        \* the drained turn's call: "none" | "asking" | "served" | "offline"

vars == <<mac, flaps, row, drains, blind, fires, app, appSaw, trigger, trigSaw, again, busy,
          turns, turn>>

Init == /\ mac = FALSE /\ flaps = 0 /\ row = "pending" /\ drains = 0 /\ blind = FALSE
        /\ fires = 0 /\ app = "idle" /\ appSaw = FALSE /\ trigger = "idle" /\ trigSaw = FALSE
        /\ again = FALSE /\ busy = FALSE /\ turns = 0 /\ turn = "none"

\* The relay route: the stream opens (`RelayBroker::connect`) and `drain_held` is spawned. It
\* claims the thread's sending, or, finding it running, has it go round again.
Connect == /\ ~mac /\ flaps < MaxFlaps
           /\ mac' = TRUE /\ flaps' = flaps + 1
           /\ IF trigger = "idle"
                THEN trigger' = "armed" /\ UNCHANGED again
                ELSE again' = (again \/ ReArms) /\ UNCHANGED trigger
           /\ UNCHANGED <<row, drains, blind, fires, app, appSaw, trigSaw, busy, turns, turn>>
Leave == /\ mac /\ flaps < MaxFlaps
         /\ mac' = FALSE /\ flaps' = flaps + 1
         /\ UNCHANGED <<row, drains, blind, fires, app, appSaw, trigger, trigSaw, again, busy,
                        turns, turn>>

\* The person's other turns on the thread, from their app.
TurnStarts == /\ ~busy /\ turns < MaxTurns
              /\ busy' = TRUE /\ turns' = turns + 1
              /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, trigger,
                             trigSaw, again, turn>>
TurnEnds == /\ busy /\ busy' = FALSE
            /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, trigger, trigSaw,
                           again, turns, turn>>

\* The row's lock, the drain's check (`matches_turn` and not held) and the drain, as one step.
Drain(saw) == IF row = "pending" /\ (saw \/ ~HoldOffline)
                THEN /\ row' = "drained" /\ drains' = drains + 1 /\ blind' = (blind \/ ~saw)
                     /\ turn' = "asking"
                ELSE UNCHANGED <<row, drains, blind, turn>>

AppCheck == /\ app = "idle" /\ fires < MaxFires
            /\ app' = "checked" /\ appSaw' = mac
            /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, trigger, trigSaw, again, busy,
                           turns, turn>>
AppDrain == /\ app = "checked" /\ Drain(appSaw)
            /\ app' = "idle" /\ fires' = fires + 1
            /\ UNCHANGED <<mac, flaps, appSaw, trigger, trigSaw, again, busy, turns>>

\* `send_held`'s loop: the Mac still there, nothing running on the thread, the send still queued,
\* and the fire's check. A turn in flight is waited out while the Mac stays (`WaitsOut`); without
\* it, the sending gave up there.
TriggerCheck == /\ trigger = "armed"
                /\ WaitsOut => ~(busy /\ mac)
                /\ IF mac /\ ~busy /\ row = "pending"
                     THEN trigger' = "checked" /\ trigSaw' = mac
                     ELSE trigger' = "leaving" /\ UNCHANGED trigSaw
                /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, again, busy,
                               turns, turn>>
\* `routes::turn` → `consume_for_turn`'s drain; then the loop goes round.
TriggerDrain == /\ trigger = "checked" /\ Drain(trigSaw)
                /\ trigger' = "armed"
                /\ UNCHANGED <<mac, flaps, fires, app, appSaw, trigSaw, again, busy, turns>>
\* `send_held` returned; `drained` says whether to go round again.
TriggerLeave == /\ trigger = "leaving"
                /\ trigger' = IF again THEN "armed" ELSE "idle"
                /\ again' = FALSE
                /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, trigSaw, busy,
                               turns, turn>>

\* The drained turn's door call picks the account's Mac as it is now.
Ask == /\ turn = "asking"
       /\ turn' = IF mac THEN "served" ELSE "offline"
       /\ UNCHANGED <<mac, flaps, row, drains, blind, fires, app, appSaw, trigger, trigSaw, again,
                      busy, turns>>

Next == Connect \/ Leave \/ TurnStarts \/ TurnEnds \/ AppCheck \/ AppDrain
        \/ TriggerCheck \/ TriggerDrain \/ TriggerLeave \/ Ask \/ UNCHANGED vars
Spec == Init /\ [][Next]_vars
             /\ WF_vars(AppDrain) /\ WF_vars(TriggerCheck) /\ WF_vars(TriggerDrain)
             /\ WF_vars(TriggerLeave) /\ WF_vars(Ask) /\ WF_vars(TurnEnds)

-----------------------------------------------------------------------------
\* A send is one turn at most, however the app and the server race to fire it.
DrainedOnce == drains <= 1
\* A send whose fire saw no Mac is never drained by that fire: it stays queued, held.
NeverDrainedBlind == ~blind
\* A drained send's turn finds a Mac. FAILS BY NATURE: a Mac that leaves between the check and
\* the door's call makes the one turn `relay_offline` (HeldSend_gap). Stated in README.
ServedIfDrained == turn /= "offline"
\* A Mac connected, nothing running on the thread, the send still queued: the server is sending
\* it. Otherwise only the app can, and with the app closed it waits for good (#298 review).
NeverStranded == ~(mac /\ ~busy /\ row = "pending" /\ trigger = "idle")
\* A Mac that comes to stay gets the send.
DrainsOnceTheMacStays == (<>[]mac) => <>(row = "drained")
=============================================================================
