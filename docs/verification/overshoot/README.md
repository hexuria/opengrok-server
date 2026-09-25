# The overshoot (#120): why a search became twenty-five plays, and what now stops it

A coworker was asked to *search YouTube for kabisado*. It ran its taught recipe, then ran it again
and again. Each run opened the browser and typed the search term into a field that already held
it, until the box's search field read `kabisadokabisado`. About twenty-five plays went out, roughly
a minute apart. The person typed "stop it" into the chat and it reached nothing
(`crates/opengrok-server/tests/against_a_stopped_run.rs`, header).

This page answers the diagnosis the issue asked for. **It is not the recorded transcript** that
the acceptance item wants. That needs a live box, a real model and the client, and none of them
exists in the environment this was written in. The procedure for recording it is at the end.

## What ends a turn

`converse_raw` (`crates/opengrok-harness/src/lib.rs`) is the loop behind `run_conversation`. Every
exit goes through `close`. In the order the loop reaches them:

| Exit | Ending | Says |
|---|---|---|
| The opening could not be journaled | fail | "the run could not be recorded: …" |
| Stop, at the top of a round | stop | — |
| Past the run's wall clock (`RunBudget::max_wall_ms`, 15 min), at the top of any round after the first | the wrap-up call (below) | — |
| The door would not open (after its safe retries), did not start answering within `call_timeout_ms`, went quiet for `idle_ms`, or the stream broke | fail | the door's sentence (`ModelError::sentence`) |
| A plan with no tool started, past `PLAN_ONLY_TEXT_LIMIT` withheld characters | fail | "the coworker described N characters of work without starting any of it…" |
| Stop, after the model asked for a tool and before it runs | stop | — |
| A catalog listing asked for again after one was already skipped | finish | the listing's first line |
| The same open-target / open-editor action again | finish | the opened sentence |
| **A recipe replay asked for again after one was already answered without playing** (new) | finish | "The recipe X already ran for this request, so it was not played again." |
| A call waiting on a person | park | a card each |
| Every call refused, the same way as last round | fail | "`tool` was refused the same way twice…" |
| The same screenshot `SAME_SCREEN_LIMIT` (4) times running | fail | "the screen has not changed after 4 looks…" |
| `MAX_FAILED_WORK_ROUNDS` (2) failures in a row | finish | one short failure fact |
| A chart or form was painted | finish | — |
| Spoken rounds reach `MAX_ROUNDS` (8), or screen rounds reach `MAX_COMPUTER_ROUNDS` (24) | the opened sentence if there is one, else the wrap-up call | — |
| The wrap-up call: no tools, a `[harness]` line saying which limit | finish with the model's words; fail with "this run reached its limit of …" if it fails or says nothing; stop if a Stop landed first | the model's summary |
| A round with no tool call | finish | the model's words |

A round counts toward the screen budget only when **every** call in it is a `computer` action that
succeeded. `run_recipe` is not `computer`, so each recipe round is a spoken round.

## The arithmetic

- **One segment plays a recipe at most 8 times.** Each play is a spoken round, and the eighth
  spoken round ends the run.
- **One request could play it at most 9 times.** `run_recipe` leaves the box
  (`opengrok_tools::leaves_the_box`), so its first call can raise the egress card and park. On
  the answer, `resume_conversation` plays the approved call. It then starts a new
  `converse_raw` whose spoken and screen budgets begin again at zero, and a yes consents egress
  for the rest of the run, so no second card comes. That is 1 + 8.
- **Twenty-five plays therefore span at least three requests.** Nothing inside one turn allowed
  it. Plays about a minute apart fit a recipe of a few steps plus its screenshot.

The repo's own account ("the replays happened while the model was mid-turn") and the arithmetic
agree once the resume is counted. Within each request the model was mid-turn. The count still
needs more than one request.

## Where the other requests could have come from

Each candidate below is a code path. Without the recorded run, none of them can be named as the
cause.

1. **The person's own messages.** "stop it" is a new user message on the thread, so it starts a
   new turn. That turn's model sees the unfinished search: `STEER_CONTINUATION` in
   `agui/routes.rs` tells it to continue from the stopped or failed turn's tool results. It can
   then play the recipe again. A stop typed as chat asks the model to stop, and a model that
   reads its history as unfinished work may not. `POST /ag-ui/runs/{id}/stop` (026903f, 1d2ebcf)
   is the stop that reaches the run.
2. **A client retrying its POST.** Before 765d60c, a retried POST with the same run id started the
   turn again. Now it attaches to the run that exists.
3. **A routine or monitor firing** (`autonomy/mod.rs` → `run_conversation`). Each firing is a new
   request with fresh budgets.
4. **A resume after a card.** This path adds at most one segment per card, and the egress consent
   is once per run, so it adds one segment per request (see the arithmetic).

## What changed

- **Prompt (PR #129, 7c80b45).** "Do what was asked, then stop", and "run a given recipe at most
  once per request". This is text only, so it holds only as well as a model follows it.
- **Loop (this change).** The loop now enforces the recipe rule. A `run_recipe` call for a recipe
  this request already played is answered without touching the box. The model is told it was not
  played again and why. If it asks again, the turn ends with that sentence. The recipe the
  approved call played in a resume is carried into the resumed segment, so the resume cannot
  replay it either. A recipe counts as played only when the box played it: a success, or a run
  that stopped part way (`ToolResult::stopped_part_way`). A refusal before the box (a missing
  parameter, a recipe not granted, an unreachable box, a policy block) played nothing, so the
  corrected call runs. Tests: `a_recipe_is_played_at_most_once_per_request`,
  `a_resumed_run_does_not_replay_the_recipe_it_was_approved_for`,
  `a_recipe_refused_before_the_box_still_plays_when_corrected`,
  `a_recipe_that_stopped_part_way_is_not_played_again`,
  `a_resumed_recipe_refused_before_the_box_may_be_asked_again`, and
  `a_second_recipe_in_the_same_request_still_plays` (a different recipe is a different task).
- **Stop.** The Stop button is a command against the run (026903f). It is honoured at every step
  boundary and at the close (`formal/tla/HarnessLoop.tla` StopIsHonoured).
- **Clocks (#93).** A model call that never starts, or goes quiet, now ends the run with a
  sentence. Before this it held the run, and its lease, for as long as the process lived. A run
  past its wall clock starts no new work. Neither of these would have stopped the kabisado plays,
  which were work, not silence. They bound what the round caps never could.

## Still open

- **The recorded transcript.** Run *search youtube for kabisado* against a coworker that has the
  YouTube search recipe granted, on a live box with the real gateway door. Capture the run's
  `events` stream (`run/run_…`) and the AG-UI frames the client received. File them here the same
  way `docs/verification/auto-review/` does. The capture passes if the stream has exactly one
  `run_recipe` `TOOL_CALL_RESULT` that played, and no `computer` click after it that plays the
  video, skips an ad or dismisses a popup. It must not include the gateway URL or any key.
- **Recipes played before an earlier card.** Only the approved call's recipe is carried into a
  resumed segment. A recipe that played in a segment before a later card (a shell auto-review
  card, say) is not, so the segment after that card could play it once more. Carrying the whole
  set needs it from the journal, like the budgets below.
- **Budgets that restart on resume.** A resumed segment still starts its spoken and screen budgets
  from zero. Carrying them across needs the rounds already spent from the journal. That belongs
  with the run budget work in #93.
