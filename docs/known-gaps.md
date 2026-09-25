# Known gaps

Real defects that are understood, reproduced, and not yet fixed. Each says what breaks, where,
and why it was left — so the next person inherits the reasoning and not just the symptom.

Written 8 Sep 2026, from a night where four of these were found by hitting them in sequence.
Nothing here is speculative; every one has an observation behind it.

---

## 1. A revoked coworker key is still treated as usable — FIXED 9 Sep 2026

**Where it was:** five call sites, not the three this entry originally claimed. `ensure_key_for`'s
early return, **both** of `key_for`'s lookups (the one dispatch authenticates with),
`GuardedDoor::stream`'s meter lookup, and `set_limit`'s feasibility check — which was introduced
by #76 *without* the filter, so the guard written to refuse an uncountable cap was itself accepting
one from a revoked row. Miscounting this entry is how that happened; the count is now checked
rather than remembered.

**Coverage, stated rather than assumed** (`against_a_revoked_key.rs`): `set_limit`, the mint check
and dispatch are each independently revert-checked and go red alone. The meter lookup is NOT
discriminated by a test — with no vault it holds the turn either way and only the sentence differs
— so that one site is verified by reading, not by behaviour.

The original entry follows, kept because the constraint in it still governs any future change.

`PgStore::coworker_key` returns a row without checking `revoked_at_ms`, deliberately: the row
must survive revocation so a member's month still counts toward their pool, and
`tests/against_member_keys.rs` asserts exactly that. The convention is therefore to filter at the
CALL SITE, and three places already do — `points.rs:262`, `points.rs:573`, `spend.rs:469`. The
three named above do not.

**What breaks:** retiring a coworker revokes its keys, after which `ensure_key_for` still reports
`Minted` with the revoked prefix and the meter still tries to read against a credential the
gateway has been told to reject.

**The fix:** add `.filter(|row| row.revoked_at_ms.is_none())` at those three sites. Do NOT push
the filter into the SQL — `coworker_key` returning revoked rows is load-bearing for pool
accounting and is under test.

**Why it is still open:** it needs a review pass rather than a hurried edit, and the outage that
surfaced its neighbourhood was fixed a different way (see §2).

---

## 2. `mint_late` will mint a key that cannot be used

**Where:** `crates/opengrok-server/src/spend.rs`, `mint_late` → `ensure_key_for`.

`ensure_key_for` mints a coworker's key on the **org principal**
(`org-<org_id>@gateway.local`), not on the deployment principal that holds `OG_GATEWAY_TOKEN`.
If that org principal's route carries no provider credentials, the mint still succeeds and the
key still authenticates — and then every turn using it fails at dispatch with
`503 no credential available for provider … on this route`.

**Observed 8 Sep 2026.** Coworker keys were re-minted onto an org principal whose route had no
seats bound. Matched pair, thirteen seconds apart, same model:

```
05:03:22  coworker key (org principal)   -> 503 "no subscription credential for xai"
05:03:35  deployment key (admin@…)       -> 200, answered by grok-4.6
```

**The fix:** refuse the mint when the resulting key would land on a route with no credential, and
say what is missing — the same shape as the limit write in `points.rs`, which now refuses a cap
it cannot count rather than storing one that holds every turn. `ensure_key_for` already calls
`ensure_org_principal` before minting, so the check has a natural home.

**Why it matters beyond this bug:** a write that reports success while leaving the thing unusable
is the failure mode of CLAUDE.md's third header fact, and this is the third instance of it found
in one night. It looks like success at every layer until dispatch.

**PARTLY MITIGATED 9 Sep 2026, and deliberately not closed.** `GuardedDoor` now retries an
UNCAPPED coworker's turn on the deployment's key when the gateway refuses the credential
(`against_a_dead_coworker_key.rs`). That removes the user-visible half — a dead key no longer
costs the conversation — but it does not stop the unusable key being minted, and a CAPPED
coworker still fails closed, correctly.

Two reasons the refusal above is still wanted rather than superseded:

- The mitigation is a retry, not a prevention. The bad key still exists, still meters nothing,
  and still makes the usage panel under-report every turn it touches.
- **The obvious implementation of the refusal does not work.** `GET /admin/api/routes` returns a
  `credentials` count that looks exactly right and is not: the gateway's own query is
  `COUNT(ar.account_id)` on a plain join, with no principal parameter and no `owner_principal_id`
  filter, and blind to `schedulable`, `cooldown_until`, `rate_limited_until` and the reserve. On
  8 Sep it would have read `2` while the org principal could reach zero — the guard would have
  passed its own test and failed in the one case it exists for. The ownership-aware view is
  `repo::candidates` on the gateway's request path, which no admin endpoint exposes. So this
  needs either a principal-aware route probe from the gateway, or a different question entirely.

**FLAGGED 25 Sep 2026, still not prevented.** When a coworker's own key is refused with a 503
naming a credential, `GuardedDoor` no longer leaves it unexplained: an uncapped turn still falls
back, and the log line names the route (`org-<org_id>@gateway.local`); a CAPPED turn is held with
a sentence naming that route and saying an admin binds a seat to it, instead of a bare 503. The
key is deliberately NOT re-minted — a new key on the same principal lands on the same route. The
mint-time refusal above still waits on the principal-aware probe from open-ai-gateway
(`against_spend_caps.rs::a_key_whose_route_reaches_no_credential_is_named_not_re_minted`).

**AND NAMED WHERE THE USAGE IS READ, 25 Sep 2026.** The fallback kept the conversation alive but
left the console lying: the coworker's own meter was empty and every reply still said
`metered: true`. `GuardedDoor` now records each credential refusal on the key's row
(`coworker_gateway_key.refusal`, key-scoped so a stale turn cannot flag a fresh key) and clears it
on the next call the key serves or on a re-mint. The spend, limit and usage replies read it first
and answer `metered: false` with "this coworker's key cannot serve: <reason>" — the route and the
seat for a 503, "revoked or disabled there" for a 401 on a key the gateway still knows
(`…a_key_the_gateway_refuses_but_still_knows_is_named_in_the_console_not_re_minted`). What is
still open is only the PREVENTION: a key that will not serve is still minted, and is found at its
first turn rather than refused at hire. A billed probe completion per mint was considered and not
taken: it costs every hire a request on a model the coworker may never use, and a ladder id can
pass on one rung and fail on another.

---

## 3. Nothing can tell a live gateway key from a dead one

**Where:** conceptual — `coworker_gateway_key` rows versus the gateway's `api_key` table.

Our Postgres is on a named volume and survives; the gateway's dev Postgres was tmpfs and did not.
So our key rows outlived the gateway keys they name, and nothing on either side noticed:
`coworker_key` returns the row, the vault opens the sealed secret happily, and the door presents
a syntactically perfect credential the gateway has never heard of — `401 authentication failed`,
with no way to see why from our logs.

**Two cheap improvements, either of which would have collapsed an hour into a minute:**

- ~~**Log the presented key prefix on a 401.**~~ **DONE 9 Sep 2026** (`gateway.rs`,
  `logged_prefix`). The gateway records *nothing* for a rejected key — it proved this by sending
  junk and watching its log stay flat — so ours is the only place that can say which credential
  was sent. Sixteen characters, which is exactly `api_key.key_prefix` on the gateway and so
  exactly what names the row; never the value. It also logs `owned`, because a coworker's own key
  and the deployment's fail with the same status and the same sentence, and which one it was
  decides where to look next.

  Coverage, stated rather than assumed: the PREFIX RULE is behaviour-tested and revert-checked
  three ways (logging the whole key, a longer prefix, and a byte slice — the last passes the
  prefix test and fails the panic test, which is why both exist). That the line is *emitted on a
  401* is not covered by a test: the harness crate keeps its tests socket-free on purpose, and a
  subscriber-capture test would have cost a dev-dependency for one assertion.
- ~~**Validate a stored prefix against the gateway**~~ **DONE 25 Sep 2026, on first refusal
  rather than first use.** When a coworker's own key draws a 401 — or the capped path's meter
  read says the gateway has no such key — `GuardedDoor::retire_if_forgotten` asks the gateway
  (`GET /admin/api/keys/{id}/usage`) whether it still knows the key. Only a definite "no key"
  retires the row (pair- and key-scoped, so a concurrent re-mint survives), drops the sealed
  secret and clears the mint back-off; the next turn mints a fresh key and is metered again. A
  key the gateway still knows (revoked or disabled there) is an operator's decision and is left
  alone. The uncapped turn that found it still falls back once; a capped one is held once with
  "a fresh key is minted on its next turn; send it again". `insert_coworker_key` now clears
  `revoked_at_ms`, without which a re-mint over a retired row stayed invisible and was re-minted
  every retry interval. Probing on first use after a restart was not done: the reactive probe
  costs nothing on the happy path and also catches a wipe while the server is running.

  **The gateway's "no key" does not say why.** A key lost in a wipe and one an operator deleted on
  purpose answer the same, so a key DELETED on the gateway is re-minted on the coworker's next
  turn. To cut a coworker off, disable its org's principal on the gateway or retire the coworker
  here; a key merely REVOKED or disabled on the gateway is left alone, and the console names it.
  (`against_spend_caps.rs::a_key_the_gateway_forgot_is_re_minted_rather_than_retried_forever`,
  `…a_capped_coworker_whose_key_was_forgotten_is_held_once_and_then_runs_on_a_fresh_key`.)

---

## 4. The account API answers refusals as plain text, not JSON

**Where:** `crates/opengrok-server/src/agui/routes.rs` and `account_api.rs` — **62** refusals of
the form `(StatusCode::X, "some sentence")` against 11 that use `json!({"error": …})`.

The `/api/{method}` gateway seam already guarantees `{"error": …}` — `reply()` and `refusal()`
enforce it, transcribed from the client contract in that file's header. The account API never
adopted the same rule, so the majority of what it refuses arrives at the desktop as
`failed (NNN).` rather than the sentence we wrote. Among the casualties: "the current password is
wrong", "you are not in an organization", "no such account".

**Status:** the desktop shipped a helper that falls back to raw text on a non-JSON body, so this
is invisible to users today. `POST /ag-ui`'s refusals of an unnamed caller (no bearer, a bearer
that names nobody, an unsigned queued send) answer `{"error": …}` since 25 Sep 2026. It remains an inconsistency between two of our own surfaces, and the
next person to add a refusal has two conventions to choose from.

---

## 5. A failed probe returns 200, so probe spend cannot be audited

**Where:** `crates/opengrok-server/src/agui/routes.rs`, `probe_model`.

```rust
Ok(served) => Json({"ok": true,  "served": served})    // 200, a real billed completion
Err(detail) => Json({"ok": false, "detail": detail})   // ALSO 200, nothing spent
```

The status code says nothing about whether money moved, and `probe_model` has no `tracing` call
of its own — so the only trace is the generic request middleware, where a billed probe and a
gateway refusal are indistinguishable except by duration. That worked once, by luck, because the
durations happened to separate cleanly; it stops working the first time a provider refuses slowly
or answers fast.

**The fix:** log the outcome inside `probe_model`, and put "the gateway refused this pin" at the
status level rather than only in the body.

**Note:** the rate limit is real and was measured — a refused probe returns `429` in `ms=0`,
before any gateway call. That part works.
