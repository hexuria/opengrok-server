/-!
# The harness's semantic core, proved independent of scheduling

Five facts the TLA+ models check for small constants, or leave to a proof, proved here for every
constant:

1. `Budget`: the loop's budgets alone end it. Every round that continues spends one unit of
   one of two budgets, so a run makes at most `R + C - 1` model calls, which is strictly
   inside the `for` bound `R + C`. The loop never falls out of its `for`, and the wrap-up
   call a spent budget makes is the `R + C`-th at most.
2. `Ending`: the projection's terminal operations are guarded by one flag, so any sequence of
   operations emits at most one terminal event, and once ended nothing changes it.
3. `Close`: a clean finish yields to a recorded Stop; a park, a failure and a Stop are kept.
4. `Answer`: an append at an expected sequence number is a compare-and-set, so of any
   number of answers that read the same parked run, at most one commits, and each commit
   starts one continuation: an approved call runs at most once per answer.
5. `Chain`: Bots messaging each other cannot go on for ever. Every message of one chain has a
   hop of at most `MAX_HOPS`, and the chain holds at most its cap of messages (#314).

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

/-- WITH THE WRAP-UP (#93). A spent budget, or the wall clock, makes one more call with no
    tools after the last round reached. The segment's calls stay within `R + C`, the bound
    the `for` was always sized to, so the wrap-up costs no budget nobody granted. -/
theorem calls_with_wrap_up_bounded (R C : Nat) (rounds : Nat → Bool) (i : Nat)
    (h : within R C (after rounds i)) : (i + 1) + 1 ≤ R + C := by
  have := continues_bounded R C rounds i h; omega

end Budget

namespace Chain

/-! How far an exchange between one person's Bots can go (#314). A chain is every message sent
because of one turn nobody's Bot asked for: a person's message, a routine's. `message_bot` admits
a call from a run whose own message had hop `h` (0 for a turn no Bot started) only while
`h < maxHops`, so at the limit the tool is not offered and the call is refused, and only when the
chain's messages and the call's recipients together stay within `cap`, all or nothing. Each
recipient's message has hop `h + 1`. Both are read from the outbox rows (`enqueue_bot_messages`)
under the owner's lock, and every Bot in a chain is that one person's, so a chain's calls are
written one at a time: `after` below. A call carried out again writes no row, so it is not a call
here at all. -/

/-- `n` messages of hop `k`: what one call writes for its `n` recipients. -/
def copies (k : Nat) : Nat → List Nat
  | 0 => []
  | n + 1 => k :: copies k n

theorem copies_length (k : Nat) : ∀ n, (copies k n).length = n
  | 0 => rfl
  | n + 1 => by simp [copies, copies_length k n]

theorem mem_copies (k x : Nat) : ∀ n, x ∈ copies k n → x = k
  | 0, h => by simp [copies] at h
  | n + 1, h => by
    simp only [copies, List.mem_cons] at h
    cases h with
    | inl h => exact h
    | inr h => exact mem_copies k x n h

/-- One call as the outbox takes it: the chain's hops so far, the hop of the message that started
    the sending run, and how many Bots the call names. -/
def send (maxHops cap : Nat) (chain : List Nat) (h n : Nat) : List Nat :=
  if h < maxHops ∧ chain.length + n ≤ cap then copies (h + 1) n ++ chain else chain

theorem send_hops (maxHops cap : Nat) (chain : List Nat) (h n : Nat)
    (hc : ∀ x ∈ chain, x ≤ maxHops) : ∀ x ∈ send maxHops cap chain h n, x ≤ maxHops := by
  intro x hx
  unfold send at hx
  by_cases hg : h < maxHops ∧ chain.length + n ≤ cap
  · rw [if_pos hg] at hx
    rw [List.mem_append] at hx
    cases hx with
    | inl hx =>
      have hk := mem_copies (h + 1) x n hx
      have hlt := hg.1
      omega
    | inr hx => exact hc x hx
  · rw [if_neg hg] at hx
    exact hc x hx

theorem send_length (maxHops cap : Nat) (chain : List Nat) (h n : Nat)
    (hc : chain.length ≤ cap) : (send maxHops cap chain h n).length ≤ cap := by
  unfold send
  by_cases hg : h < maxHops ∧ chain.length + n ≤ cap
  · rw [if_pos hg, List.length_append, copies_length]
    have hle := hg.2
    omega
  · rw [if_neg hg]
    exact hc

/-- The chain after `calls`, each a (sending run's hop, recipients) pair, from no messages. -/
def after (maxHops cap : Nat) : List (Nat × Nat) → List Nat
  | [] => []
  | c :: cs => send maxHops cap (after maxHops cap cs) c.1 c.2

/-- A CHAIN IS BOUNDED. Whatever the Bots write, every message's hop is at most `maxHops` and the
    chain holds at most `cap` messages, so it starts at most `cap` turns: for every constant, not
    only `MAX_HOPS = 4` and 12. -/
theorem chain_bounded (maxHops cap : Nat) (calls : List (Nat × Nat)) :
    (∀ x ∈ after maxHops cap calls, x ≤ maxHops) ∧ (after maxHops cap calls).length ≤ cap := by
  induction calls with
  | nil => exact ⟨fun x hx => by simp [after] at hx, by simp [after]⟩
  | cons c cs ih =>
    simp only [after]
    exact ⟨send_hops maxHops cap _ c.1 c.2 ih.1, send_length maxHops cap _ c.1 c.2 ih.2⟩

end Chain

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

/-- What an ending is made of, as far as the client's count of endings goes. -/
inductive Frame
  | bracket   -- TEXT_MESSAGE_END / REASONING_MESSAGE_END / TOOL_CALL_END
  | timing    -- the run-timing CUSTOM
  | custom    -- a card, the stop notice
  | terminal  -- RUN_FINISHED / RUN_ERROR
deriving DecidableEq, Repr

def terminals (fs : List Frame) : Nat := (fs.filter (· = .terminal)).length

/-- `Projection::unrecorded`: keep the brackets and the timing, drop the cards, the stop notice
    and the terminal, and end with one RUN_ERROR. -/
def unrecorded (fs : List Frame) : List Frame :=
  fs.filter (fun f => f = .bracket || f = .timing) ++ [.terminal]

/-- Whatever ending the log refused, its replacement carries exactly one terminal. -/
theorem unrecorded_one_terminal (fs : List Frame) : terminals (unrecorded fs) = 1 := by
  induction fs with
  | nil => simp [unrecorded, terminals]
  | cons f fs ih =>
    cases f <;> simp_all [unrecorded, terminals, List.filter]

/-- What `close` sends: the ending once the write succeeded, its replacement otherwise. -/
def sent (written : Bool) (ending : List Frame) : List Frame :=
  if written then ending else unrecorded ending

/-- An ending with one terminal reaches the client with exactly one, written or not. -/
theorem close_sends_one_terminal (written : Bool) (ending : List Frame)
    (h : terminals ending = 1) : terminals (sent written ending) = 1 := by
  cases written
  · exact unrecorded_one_terminal ending
  · simpa [sent] using h

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
