# When and how to use the formal models

Condensed from hexuria/gol's `AGENTS.md` (§1–9, 26, 28) and fitted to this repo. The models,
what they found and how to run them are in [`README.md`](README.md). This page says when a change
must go through them. It is written for whoever makes the change, person or agent.

The models are a design tool, not a certificate. Each finding in `README.md` was a trace TLC
printed before the code was fixed, and one (finding 8) was CI proving the model had assumed a
case away. Use the models to ask what can go wrong before the code decides.

## 1. When a change must touch the models

Model it, or say in the PR why the existing model already covers it, when the change touches:

- the turn loop: `converse_raw` and `close` (`crates/opengrok-harness/src/lib.rs`), budgets,
  wrap-up, and where Stop is asked;
- the run aggregate (`crates/opengrok-core/src/run.rs`): its commands, events, and which
  endings it accepts;
- journal writes, retries and `Conflict` (`crates/opengrok-store`);
- parking, answering and resuming a run, cards, holds and their deadlines;
- Stop, the recovery sweep, leases, and the per-run claim;
- anything where running something twice, or not at all, would be wrong: schedules, monitors,
  refresh tokens, sweeps that claim work.

It is not needed for CRUD, formatting, wire shapes, UI, config, or a wrapper whose correctness
is visible in the diff.

## 2. Model the semantic core, and keep it small

A model holds only what the code branches on: run and loop states, who owns a run, what may race.
It leaves out HTTP, SQL, JSON, logging and the client. A model as big as the code checks nothing
the code does not already say. Write down what it answers:

- What states exist, and which transitions are allowed?
- Who owns each run, and what may race it?
- What must never happen (safety), and what must eventually happen (liveness)?
- Which steps may repeat, and which must be idempotent?

## 3. One claim per configuration

Each `.cfg` in `tla/` checks one claim with the smallest constants that can break it. Name it for
what it shows. A configuration that is expected to fail is a counterexample kept on purpose:

- its first line is `\* EXPECTED TO FAIL: <what happens, in a sentence>`;
- a `\* VIOLATES: <Invariant>` line names the one invariant it must break. `scripts/formal.sh`
  fails if it passes, and also if it fails on a different invariant, because then it no longer
  shows what it was kept to show.

## 4. When TLC prints a counterexample

1. Read the trace as a story in the code's words: which request, which step, which write.
2. Decide whether it is real: a bug in the code, a gap the design states, or the model being
   wrong. Say which in the PR.
3. If real, fix the code, and change the model to match the fix, never the other way round.
4. Keep the pre-fix configuration as EXPECTED TO FAIL with its `VIOLATES` line.
5. **Add a regression test** that fails without the fix: a unit test on the aggregate, or an
   integration test against Postgres. `crates/opengrok-core/tests/run_properties.rs` checks the
   model's invariants on the aggregate itself. A finding there becomes a named unit test in
   `run.rs`, as its three first findings did.
6. Add the finding to `README.md`'s numbered list.

## 5. TLA+ or Lean

TLA+ searches executions for small constants: races, interleavings, crashes, retries. Lean proves
a fact for every constant: bounds, termination measures, "at most one". Reach for TLA+ first. Use
Lean when TLC's answer depends on the constants you picked, as with `Budget.calls_bounded`.
`formal/lean` uses Lean 4 core only, with no Mathlib, `sorry` or extra axioms.

## 6. The code must stay recognisably the model

Every variable and action in a model names the code it stands for (`README.md` §Model). When the
code moves, move the mapping in the same change. A model that describes last month's code proves
nothing about today's. `scripts/formal.sh` runs in CI on every change that is not prose only, so a
model that stops matching fails the gate. That mapping is the part a person has to keep true.

| Model | Code |
|---|---|
| `HarnessLoop` rounds, check points, exits | `converse_raw`, `close` in `opengrok-harness/src/lib.rs` |
| `RunLifecycle` aggregate, loops, sweep, claim | `opengrok-core/src/run.rs`, `agui/routes.rs` claim, `opengrok-server/src/recovery.rs` |
| `JournalAppend` read/append/Conflict | `append_events` / `append_run` in `opengrok-store` |
| Lean `Ending`, `Answer`, `Budget`, `Close` | the same, for every constant |
| `run_properties.rs` | `Run::decide` + `apply`, against `ExactlyOneEnding`, `ended_is_stable`, `ApprovedAtMostOnce` |

## 7. What a PR that touches the models reports

- which properties were checked, and the state counts (`scripts/formal.sh` prints them);
- any counterexample, what it meant, and what changed;
- what is still open, stated as a limit, not left out.

## 8. Do not over-formalise

The smallest model that answers the question wins. It is fine to model one step of a protocol
and leave the rest to tests. It is not fine to skip the model because the change "looks obvious"
when it touches anything in §1. Every finding in `README.md` looked obvious beforehand.
