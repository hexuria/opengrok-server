# Spend policy — points: what a coworker may spend, and who sets it

Status: built 3 Sep 2026 (server #49; open-ai-gateway #52 and #53). Supersedes the USD
three-window design of 2 Sep 2026, which is retired (§6). The desktop's usage modal
(hexuria/opengrok #55 and after) reads the shapes in §5.

## 0. Vocabulary

- **Reference price R** — USD per million tokens, the org admin's, kept on the gateway with
  the prices. **One point is one token at R.**
- **List price** — what a request's tokens would cost at the model's own pay-per-token price
  (`counterfactual_api_usd`). A subscription seat's cost is truthfully zero; its list price is
  the bill it displaced. Points are charged from the list price, so a seat and an API key count
  the same.
- **Points of a request** — `list price × 1,000,000 / R`, rounded half up per request, summed as
  integers. Derived at read time, never stored: changing R re-values every past figure alike,
  so "N points" keeps meaning "N reference tokens".
- **Multiplier** — a model's list price per token class over R (`×10` input on xai/grok-4.6 at
  R = 0.20). Shown after the id in pickers; orientation only, the charge is exact.
- **Pool** — a member's month, set by the org admin. Every coworker the member owns draws on it.
- **Cap** — a coworker's month, set by its owner, at most the pool.
- **Brake** — a coworker's rolling 24 hours, set by its owner. Off by default.
- **Windows** — `5h`, `24h`, `7d`, `month` (the UTC month). Reporting uses all four; limits use
  the month and the day.

## 1. What the gateway does, and does not

The gateway is the meter. It keeps the prices, R (`points_reference`, one row), the ledger,
and the arithmetic in one place: the per-key read (`GET /admin/api/keys/{id}/usage`) carries
`*_points` for the four windows and the rolling day's `day_*` figures; the per-model report
(`GET /admin/api/keys/{id}/usage/models?window=`) carries requests, tokens by class, cost, list
price and points per model; the batch read (`POST /admin/api/usage/points {keys, window}`)
answers the points each of several keys spent and their total in one query. Multipliers come
from `GET /admin/api/points/models`. The gateway never enforces points: one enforcer per rule,
or two answers disagree.

## 2. The decision (operator, 3 Sep 2026)

- Limits are in points, **monthly**: the admin sets each member's pool, the owner may cap a
  coworker below it. No cap means the coworker draws on the pool; no pool means unlimited.
- An **optional daily brake** per coworker, owner-set, off by default — the only burst brake,
  chosen over an automatic share-of-pool. It is a rolling 24 hours, the same family as 5h/7d.
- At a limit the turn is **refused with a sentence** in the bubble; nothing is sent to the
  model; no warnings before.
- **Usage is a report** and knows nothing about limits: per model, per window, in a modal.
- The USD windows' **limits retire**; their **meters stay** for the modal's periods.

## 3. Where the limits are evaluated

`spend::GuardedDoor`, before every model call, from the coworker's `spend_scope`:

1. `points::effective` — the coworker's cap and brake, its owner, the owner's pool (cached
   per coworker for 15 s). Nothing set anywhere ⇒ the call passes and never touches the meter.
2. The coworker's key row. No key ⇒ **held** with the reason (a limit cannot be honoured
   without something to count on). A pool makes every coworker of that member limited, so an
   unmetered coworker under a pool is held, not run uncounted on the deployment key.
3. The meter: the per-key read (15 s fresh, a reading under 60 s stands in when the gateway
   does not answer within 2 s, else held with the reason). Points `null` — no reference price
   on the gateway — ⇒ held, with the sentence that says who sets it.
4. The pool, when one is set: the batch read over every key the owner's coworkers ever had
   (revoked rows included — a retired coworker's month still counts, so retire-and-rehire does
   not reset the month), cached **per owner** for 15 s so N active coworkers of one member share
   one read.
5. The verdict, in order: cap, then pool, then day. The sentences:
   - "New Bot has used its 100,000 points for September (102,340 used); it resets on
     1 October. 412,000 of your 1,000,000 remain for other agents."
   - "Your pool of 1,000,000 points for September is used up (1,000,000 used); it resets on
     1 October."
   - "New Bot has used its 30,000 points for today (30,000 used); it frees up at 14:32 UTC."

A burst can overrun a limit by at most one reading's worth (15 s) per coworker.

## 4. Who sets what

| Who | What | Where |
|---|---|---|
| Org admin | R | console → gateway (`PUT /admin/points/reference`, audited on the gateway) |
| Org admin | each member's pool | `PUT /admin/points/members/{accountId} {pool}` |
| Org admin | a template's cap and brake | `/admin/templates` (`points.monthPoints`, `points.dayPoints`), copied at hire as the coworker's row, set by the hirer |
| Coworker's owner | cap and brake | `PUT /coworkers/{id}/limit {cap, dayCap}` — absent leaves, null clears, a cap above the pool is refused with the numbers |

`effectiveCap` in the limit read is a **ceiling on `usedPoints`**, like the cap: `min(cap,
pool − what the owner's other coworkers used)` when both exist, either alone otherwise, null
when neither. A pool lowered after a cap was set binds through it at check time too. Room left
for this coworker is `effectiveCap − usedPoints`.

## 5. Wire

Server (account API, the signed-in person's token):

```
GET  /coworkers/{id}/usage?window=5h|24h|7d|month
     → { metered, note, seat, keyPrefix, window,
         models: [{ modelId, requests, inputTokens, outputTokens, cacheReadTokens,
                    cacheWriteTokens, costUsd, listUsd, points }], totals: { …same… } }
GET  /coworkers/{id}/limit
     → { metered, note, cap, effectiveCap, usedPoints, dayCap, usedToday, dayFreesAt,
         pool: { max, used, resetsAt, setBy }, reference: { usdPerMtok } | null }
PUT  /coworkers/{id}/limit           ← { cap, dayCap }        (owner)
GET  /models                          + points: { inputX, outputX, cacheReadX, cacheWriteX, shownX } | null
GET  /admin/points                    → reference, note, members[{id,email,pool,setBy,usedPoints}],
                                        coworkers[{id,name,ownerEmail,cap,dayCap,usedPoints}]
PUT  /admin/points/reference          ← { usdPerMtok }         (admin; proxied to the gateway)
PUT  /admin/points/members/{accountId} ← { pool }              (admin)
GET  /coworkers/{id}/spend            (kept, `limits` empty, until the modal has replaced it)
```

Money is a `"%.6f"` string; points are integers; every field is present and `null` where the
gateway does not say (no reference price, an older gateway, an unmetered coworker). An unknown
route on an older server is a bare 404, which the desktop reads as "not served yet".

## 6. What is retired

The USD `spend_limit` rows at org/member/coworker scope, `/admin/spend/*`, the template USD
columns, and the three-window `over_limit` branch are no longer read or written. The
`spend_limit` table and the template USD columns stay in the schema until a later cleanup, once
points have run for a month — a table drop in the single SCHEMA string is the one thing we
cannot take back. `GET /coworkers/{id}/spend` keeps serving its meters with an empty `limits`
until the desktop's modal has replaced it.

## 7. Known gaps

- Turns a coworker took on the deployment key before its own key existed are not counted
  (narrowed by #45 and #46: the key is minted on the next turn, on a bound principal).
- Without a reference price nothing is counted: limits hold every turn, in words, until the
  admin sets R.
- A change of R re-values every existing pool and cap: rare, audited on the gateway, and shown
  with its dollar equivalent on the admin page while typing.
- A per-turn request ledger (which request cost what) is a possible follow-up; it needs a
  per-key request-rows route on the gateway.
