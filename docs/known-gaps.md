# Known gaps

Real defects that are understood, reproduced, and not yet fixed. Each says what breaks, where,
and why it was left — so the next person inherits the reasoning and not just the symptom.

Written 8 Sep 2026, from a night where four of these were found by hitting them in sequence.
Nothing here is speculative; every one has an observation behind it. Entries are removed when
they are fixed: the revoked-key filter (fixed 9 Sep) and telling a live gateway key from a dead
one (fixed 9 and 25 Sep) are in `git show 99ec5c3:docs/known-gaps.md`.

---

## 1. `mint_late` will mint a key that cannot be used

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

## 2. The account API answers refusals as plain text, not JSON

**Where:** `crates/opengrok-server/src/agui/routes.rs` and `account_api.rs` — about 99 refusals
of the form `(StatusCode::X, "some sentence")` (counted 26 Sep 2026), against a minority that use
`json!({"error": …})`.

The removed `/api/{method}` door guaranteed `{"error": …}` for every refusal, transcribed from the
Grok Bot client's contract. The account API never adopted the same rule, so most of what it
refuses reaches a client as a bare sentence. `POST /ag-ui`'s refusals of an unnamed caller (no
bearer, a bearer that names nobody, an unsigned queued send) answer `{"error": …}` since 25 Sep
2026.

**Status:** it is an inconsistency between our own routes, and the next person to add a refusal
has two conventions to choose from. Whether NativeChat reads a JSON body, a plain one, or both is
not known from this repo ([`setup/nativechat.md`](setup/nativechat.md)).

---

## 3. A failed probe returns 200, so probe spend cannot be audited

**Where:** `crates/opengrok-server/src/agui/routes.rs`, `probe_model`.

```rust
Ok(probed) => Json({"ok": true, "served": …, "toolCalls": …})  // 200, a real billed completion
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
