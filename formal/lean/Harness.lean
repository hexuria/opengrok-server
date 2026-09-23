/-!
# The harness's semantic core, proved independent of scheduling

Four facts the TLA+ models check for small constants, proved here for every constant:

1. `Budget`: the loop's budgets alone end it. Every round that continues spends one unit of
   one of two budgets, so a run makes at most `R + C - 1` model calls, which is strictly
   inside the `for` bound `R + C`. The loop never falls out of its `for`.
2. `Ending`: the projection's terminal operations are guarded by one flag, so any sequence of
   operations emits at most one terminal event, and once ended nothing changes it.
3. `Close`: a clean finish yields to a recorded Stop; a park, a failure and a Stop are kept.
4. `Answer`: an append at an expected sequence number is a compare-and-set, so of any
   number of answers that read the same parked run, at most one commits, and each commit
   starts one continuation: an approved call runs at most once per answer.

Only Lean 4 core is used (no Mathlib), so `lean Harness.lean` is the whole check.
-/

namespace Budget

/-- The two counters `converse_raw` carries across rounds (lib.rs:788-789). -/
structure Counters where
  spoken : Nat
  computer : Nat
deriving Repr

/-- One continuing round's bookkeeping (lib.rs:1493-1501). -/
def spend (c : Counters) (onScreen : Bool) : Counters :=
  if onScreen then { c with computer := c.computer + 1 } else { c with spoken := c.spoken + 1 }

/-- The round may `continue` only while both budgets hold (lib.rs:1502-1569). -/
def within (R C : Nat) (c : Counters) : Prop := c.spoken < R ∧ c.computer < C

/-- The progress measure: units of budget left. -/
def measure (R C : Nat) (c : Counters) : Nat := (R - c.spoken) + (C - c.computer)

/-- Every continuing round strictly decreases the measure. -/
theorem measure_decreases (R C : Nat) (c : Counters) (s : Bool)
    (h : within R C (spend c s)) : measure R C (spend c s) < measure R C c := by
  unfold within spend at h
  unfold measure spend
  cases s <;> simp at h ⊢ <;> omega

/-- Counters after `k` continuing rounds, driven by any choice of rounds. -/
def after (rounds : Nat → Bool) : Nat → Counters
  | 0 => ⟨0, 0⟩
  | k + 1 => spend (after rounds k) (rounds k)

theorem after_sum (rounds : Nat → Bool) (k : Nat) :
    (after rounds k).spoken + (after rounds k).computer = k := by
  induction k with
  | zero => rfl
  | succ k ih =>
    simp only [after, spend]
    cases rounds k <;> simp <;> omega

/-- If `k` rounds have all continued, the last of them was within budget, so
    `k ≤ (R - 1) + (C - 1)`. -/
theorem continues_bounded (R C : Nat) (rounds : Nat → Bool) (k : Nat)
    (h : within R C (after rounds k)) : k + 2 ≤ R + C := by
  have hs := after_sum rounds k
  unfold within at h
  omega

/-- THE FOR BOUND IS NEVER REACHED. `converse_raw` runs `for _ in 0..(R + C)`; iteration `i`
    (0-based) is reached only after `i` rounds continued. Since at most `R + C - 2`
    rounds can continue, the last iteration reached is `R + C - 2 < R + C`, and it ends the
    run with a terminal event. For every R, C ≥ 1 — not only 8 and 24. -/
theorem never_falls_out (R C : Nat) (rounds : Nat → Bool) (i : Nat)
    (reached : i = 0 ∨ within R C (after rounds i)) (hR : 1 ≤ R) (hC : 1 ≤ C) :
    i < R + C := by
  cases reached with
  | inl h => omega
  | inr h => have := continues_bounded R C rounds i h; omega

/-- Model calls in one segment: one per round reached. -/
theorem calls_bounded (R C : Nat) (rounds : Nat → Bool) (i : Nat)
    (h : within R C (after rounds i)) : i + 1 ≤ R + C - 1 := by
  have := continues_bounded R C rounds i h; omega

end Budget

namespace Ending

/-- What can be asked of a `Projection` (projection.rs). -/
inductive Op
  | push | toolResult | awaiting | finish | fail | stop
deriving DecidableEq, Repr

def Op.terminal : Op → Bool
  | .finish | .fail | .stop => true
  | _ => false

/-- The projection's one bit that matters here: `finished`. -/
structure P where
  finished : Bool
  terminals : Nat
deriving Repr

/-- `finish`/`fail`/`stopped` return nothing once `finished` (projection.rs:286-365);
    `awaiting_approval` emits but deliberately does not set `finished`. -/
def step (p : P) (op : Op) : P :=
  if op.terminal then
    if p.finished then p else { finished := true, terminals := p.terminals + 1 }
  else p

def run (p : P) : List Op → P
  | [] => p
  | op :: ops => run (step p op) ops

/-- The invariant: at most one terminal, and exactly one iff ended. -/
def Inv (p : P) : Prop := (p.finished = true ∧ p.terminals = 1) ∨ (p.finished = false ∧ p.terminals = 0)

theorem step_inv (p : P) (op : Op) (h : Inv p) : Inv (step p op) := by
  unfold step
  split
  · split
    · exact h
    · unfold Inv at h ⊢; simp_all
  · exact h

theorem run_inv (p : P) (ops : List Op) (h : Inv p) : Inv (run p ops) := by
  induction ops generalizing p with
  | nil => exact h
  | cons op ops ih => exact ih _ (step_inv p op h)

/-- Any sequence of operations on a fresh projection emits at most one terminal event. -/
theorem at_most_one_terminal (ops : List Op) : (run ⟨false, 0⟩ ops).terminals ≤ 1 := by
  have := run_inv ⟨false, 0⟩ ops (Or.inr ⟨rfl, rfl⟩)
  unfold Inv at this; omega

/-- Once ended, nothing changes it: a completed run cannot end again. -/
theorem ended_is_stable (p : P) (ops : List Op) (h : p.finished = true) : run p ops = p := by
  induction ops with
  | nil => rfl
  | cons op ops ih =>
    simp only [run]
    have : step p op = p := by unfold step; simp [h]
    rw [this]; exact ih

end Ending

namespace Close

/-- How a segment of the loop wants to end. -/
inductive Verdict
  | finished | failed | stopped | parked
deriving DecidableEq, Repr

/-- `close` (lib.rs): a clean finish asks the journal once more. -/
def close (stopRecorded : Bool) : Verdict → Verdict
  | .finished => if stopRecorded then .stopped else .finished
  | v => v

/-- A Stop recorded before the close is never reported as a clean finish. -/
theorem stop_is_honoured (v : Verdict) : close true v ≠ .finished := by
  cases v <;> simp [close]

/-- Without a Stop, the close changes nothing. -/
theorem close_no_stop (v : Verdict) : close false v = v := by
  cases v <;> simp [close]

/-- A failure's own sentence and a park's card survive a Stop. -/
theorem close_keeps_failures : close true .failed = .failed ∧ close true .parked = .parked := by
  simp [close]

/-- Resuming: the approved call runs only when the answer was yes AND no Stop is recorded. -/
def runsApproved (stopRecorded approved : Bool) : Bool := approved && !stopRecorded

theorem no_approved_after_stop (a : Bool) : runsApproved true a = false := by
  simp [runsApproved]

end Close

namespace Answer

/-- The run's event stream, abstracted to its length (`stream_seq` of the last event). -/
abbrev Log := Nat

/-- `append_run(expected_seq)`: succeeds, extending the log, only if nothing was appended
    since the read — the unique `(stream_id, stream_seq)` constraint (postgres.rs:376-391). -/
def append (log : Log) (expected : Nat) : Option Log :=
  if log = expected then some (log + 1) else none

/-- Apply a batch of commits that all read the log at `seq`; count how many succeed. -/
def commitAll (seq : Nat) : Log → List Unit → Nat × Log
  | log, [] => (0, log)
  | log, _ :: rest =>
    match append log seq with
    | some log' => let r := commitAll seq log' rest; (r.1 + 1, r.2)
    | none => commitAll seq log rest

theorem append_stale (log seq : Nat) (h : seq < log) : append log seq = none := by
  have : log ≠ seq := by omega
  simp [append, this]

theorem append_fresh (seq : Nat) : append seq seq = some (seq + 1) := by
  simp [append]

/-- Once one commit has landed, every later one that read the same `seq` is refused. -/
theorem commitAll_none_after (seq : Nat) (log : Log) (xs : List Unit) (h : seq < log) :
    (commitAll seq log xs).1 = 0 := by
  induction xs with
  | nil => simp [commitAll]
  | cons x xs ih => simp only [commitAll, append_stale log seq h]; exact ih

/-- AT MOST ONE ANSWER COMMITS. However many requests (double clicks, two clients, a retry
    after a timeout) read the parked run at `seq`, at most one append succeeds. Since each
    successful append spawns exactly one continuation, and a continuation runs the approved
    call at most once, the approved call runs at most once. -/
theorem at_most_one_commit (seq : Nat) (xs : List Unit) : (commitAll seq seq xs).1 ≤ 1 := by
  induction xs with
  | nil => simp [commitAll]
  | cons x xs _ =>
    simp only [commitAll, append_fresh]
    have := commitAll_none_after seq (seq + 1) xs (by omega)
    omega

end Answer
