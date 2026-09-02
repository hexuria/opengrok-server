# Spend policy — limits on what a coworker may spend, and who sets them

Status: **design for review, 2 Sep 2026; the shape is decided, the code is being reworked.** Written
after the first cut (#32, a lifetime per-key cap) met the operator's answer: limits are a
**calendar month plus rolling 5-hour and 7-day windows**, "like other LLM subscriptions", and at a
limit the turn is **refused with a sentence**. This page is the design #32 is reworked to, and
the vocabulary that keeps three systems from talking past each other again.

---

## 0. Three vocabularies, and one word that meant two things

| The gateway says | OpenGrok says | The console says |
|---|---|---|
| principal (one per org) | org | the org budget card |
| route (a ladder of models) | a model pin like `xai/grok-4.6` | the route column |
| key (one credential; the gateway meters per key) | a member's key, or a coworker's own key | keys card; nothing for coworkers until #32 |

"Key" also named two unrelated things on the same day: the **bot key** Claude Code gets after a
person signs in through the OAuth door, and the **gateway key** the server mints for a coworker
at hire, silently, through the gateway's admin API. No person ever does OAuth for a spend limit;
the coworker's gateway key is plumbing, the only way the gateway can tell one coworker's spend
from another's. It stays. What changes is what is written on it.

## 1. What the gateway enforces, and what it cannot

Every request through the gateway is checked against three budgets at once (`budgets_for`,
`oag-server/src/gateway/mod.rs`); whichever is exhausted first refuses the request with a 402.

| Scope | Period | Grace | Set from |
|---|---|---|---|
| principal | calendar month, from the ledger | overshoot multiple | admin API (`PATCH /principals/{email}/budget`) |
| route | calendar month, from the ledger | none | gateway CLI only |
| key | **lifetime**, a denormalised counter | none | admin API (`PATCH /keys/{id}/quota`) |

So the gateway can meter per key, and its **ledger** (`usage_event`: `api_key_id`, `cost_usd`,
`occurred_at`) is the record of every coworker's spend to the second. What it cannot do today is
a windowed per-key limit: the key quota is a lifetime wall, and the gateway's own budget check
deliberately never sums the ledger per request. Hanging rolling windows on the key quota is the
wrong primitive, which is why #32 as first cut answered a different question.

## 2. The decision (operator, 2 Sep 2026)

Three limits per coworker, all optional, all in USD:

| Limit | Window | Resets |
|---|---|---|
| 5-hour | rolling | when the oldest spend inside the window ages out |
| 7-day | rolling | same |
| monthly | calendar month, UTC | on the first |

At any limit the turn is **refused**, before the model is called, with a sentence that names the
window and when it resets: *"Ada has used its 5-hour allowance ($4.90 of $5.00); it begins to
free up at 14:32. The 7-day and monthly allowances still have room."* "You are at your cap"
alone is the wrong sentence for somebody whose window clears in ten minutes. The console shows
the same three numbers as three meters per coworker — used, limit, and when each frees up or
resets — so a person can see which window is the one in the way before they hit it.

## 3. Where the three limits are evaluated, and where the numbers come from

**The gateway keeps the ledger; the server evaluates the policy.** That is CLAUDE.md #6 (the
server decides, on every action) applied to money.

- **Numbers.** `GET /admin/api/keys/{id}/usage` (open-ai-gateway #50) grows three windowed sums
  and their reset instants, computed from the ledger with an index on `(api_key_id, occurred_at)`:
  `five_hour_usd` / `five_hour_resets_at`, `seven_day_usd` / `seven_day_resets_at`,
  `month_usd` / `month_resets_at`, plus `requests`. One query per window; a coworker's rows in a
  window are few. The lifetime `spent_usd` stays in the reply for the record but is no longer
  what a limit is written against.
- **Limits.** Authored in OpenGrok (`spend_limit` rows, §4), never on the gateway. The gateway's
  per-key `quota_usd` is left unset — one enforcer per rule, or two answers disagree.
- **Evaluation.** Before **each model call**, not once per turn: a turn with a tool loop makes
  many calls, and a subscription-style limit is checked per request. The check lives in a
  `ModelDoor` wrapper in the server (`spend::GuardedDoor`) around the real `GatewayDoor`: the
  request carries the coworker it is for (`ModelRequest.spend_scope`), the guard reads the three
  sums (cached for fifteen seconds per coworker, so a tool loop costs one read), compares them to
  the coworker's effective limits, and refuses with `ModelError::SpendCap(sentence)` — the same
  path that already turns a refusal into the run's failure and the transcript's text. A guard
  that cannot read the numbers **holds** the turn with that reason; it never lets a turn through
  unmetered.
- **Granularity, and what a mid-turn refusal looks like.** A model call already in flight
  finishes; the next one is refused. The turn's run fails with the sentence, the transcript keeps
  everything the turn already produced (text, tool results, cards) and ends with the sentence;
  nothing is rolled back and nothing is lost. When the window has room again the person sends
  the next prompt; there is no automatic resume. The guard also runs before the FIRST call, so a
  coworker already over a limit never starts a turn only to fail at once. Two calls racing can
  both pass by one call's cost — the window is soft by one request, as every subscription's is.
- **When the meter cannot be read.** The read has a two-second timeout. A reading younger than
  sixty seconds is used in its place (the fifteen-second cache, extended under failure); with
  no such reading the turn is **held**, refused with "the spend meter could not be read; try
  again", and the failure is logged at error. A meter that is down is a visible outage of the
  coworkers that have limits, never an advisory cap: silently allowing turns would be the one
  outcome an admin who wrote a limit did not ask for. Coworkers with no limits at any layer skip
  the read entirely.
- **"Resets at", defined.** A rolling window has no boundary, so its reset instant is the moment
  the **oldest spend still inside the window ages out** — the earliest instant the used figure
  drops at all. The gateway returns that instant (`oldest + window`) and the sentence says
  "begins to free up at 14:32"; it does not claim the whole allowance is back then. The monthly
  window resets on the first of the next UTC month, and the sentence says "resets on 1 Oct".

## 4. Who sets what: the policy ladder

Rules are authored by the **admin**, in the admin dashboard; members see their coworkers'
meters read-only. A member may never raise a limit.

| Layer | Meaning | Where it lives |
|---|---|---|
| org budget | the org's monthly total | gateway principal budget (already in the admin dashboard) |
| model budget | the org's monthly total on one model family | gateway route budget — needs a gateway admin endpoint (later) |
| org default | the three limits every coworker gets unless something narrower says otherwise | `spend_limit` scope `org` |
| member override | the three limits a given member's coworkers get | `spend_limit` scope `member` |
| template | a coworker type with limits baked in, copied at hire | `spend_limit` scope `template` (with templates, later) |
| coworker | the effective limits on this one coworker | `spend_limit` scope `coworker` |

**Rules.**

1. Every applicable limit applies; a request must fit all of them. Mixing is native.
2. For a coworker's number, the most specific admin-written value wins: coworker > template >
   member > org default. An absent value means "no limit at this layer", not zero.
3. Only admins write limits. Members read.
4. Template limits are **copied** at hire, not linked; editing a template offers "apply to
   existing coworkers"; deleting one leaves them as they are. A linked limit would let one edit
   silently change fifty running coworkers, which a limit must never do.
5. What a person sees: three meters per coworker (used / limit / resets), in the console and in
   the refusal sentence.
6. Not built: a per-coworker, per-model limit. A coworker thinks with one pinned model, so its
   limits already are its model limits; "nobody burns the expensive model" is the org's model
   budget.

## 5. Work, in order

1. **Gateway #50, extended**: the three windowed sums and reset instants, the index. Half a day.
2. **Server (#32 reworked)**: `spend_limit` table + resolver; `GuardedDoor` + `ModelRequest.spend_scope`;
   the refusal sentences; admin dashboard card (org default, per-member override, per-coworker);
   coworker page meters read-only; tests over the stand-in gateway with windowed usage, including
   "the 5-hour window refuses while the month has room" and "the sentence names the reset". A day
   and a half.
3. **Templates** (coworker types: model pin, tool ceiling, approval policy, limits): its own slice.
4. **Model budgets**: a route-budget endpoint on the gateway, a card here. Small, later.

## 6. Questions for the operator

1. Org default limits to ship with: proposed none (unlimited until an admin writes one).
2. Warning threshold in the roster (e.g. 80 %) — wanted, or later?
