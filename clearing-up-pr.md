# Clearing the pull request queue

A running record of what actually happened to each PR on the way to main: what it was for, what
went wrong, what was found while reviewing it, and what was deliberately left behind.

Written for reading **after** the fact, so every claim here is one that was checked rather than
assumed. Where something was asserted and later proved wrong, the correction is left in rather
than tidied away — the wrong turns are the useful part.

Session: <https://claude.ai/code/session_011xsfbn1QUCyVNFWFcPbsdk>

---

## Where the queue started

Six PRs open against `main` at `0f822f3`, and **CI dead for every one of them**.

The CI failure looked like a code problem and was not. Every run finished in three to five
seconds having executed **zero steps**, with:

> The job was not started because recent account payments have failed or your spending limit
> needs to be increased.

The repo is public and `ci.yml` asks for `ubuntu-latest`, which is free with unlimited minutes on
a public repository — so there was no runner downgrade to make and no quota to trim. It was an
account billing state, fixed outside the repo. Re-running the two most relevant runs confirmed
jobs began executing again. **No file was changed to fix CI.**

---

## Merge order, and why

| Order | PR | Reason for the position |
|---|---|---|
| 1 | #57 | Off `main`, stops unmetered spending immediately, blocks nothing |
| 2 | #54 | Merging it dissolves the copies of itself riding in #53 and #55 |
| 3 | #56 | Collides with nothing; closes a cross-account read |
| 4 | #52 | #55 is built on it, so it cannot go later |
| 5 | #53 | Needed a rebase after #57, plus the judge's actor |
| 6 | #55 | Largest, and changes authorisation on every verb — wants the quietest base |

Every collision between the open branches was tested with `git merge-tree` rather than guessed.
The result was worth knowing: **all of them were `docs/ROADMAP.md` and nothing else.** The code
auto-merged in every pair.

---

## #57 — the meter had holes in it

**Merged `f075711`. Main green.**

Three gaps found auditing the points limits that shipped in `18.points`. All three predated the
open PRs.

1. **The auto-review judge was not metered.** It built its request with `spend_scope: None` and
   `gateway_key: None`, so the guard passed it through and its tokens went out on the
   deployment's key. It fires once per tool call, so a coworker at its cap — refused for
   everything else — could still spend indefinitely through the safety check.
2. **Caps could add up to more than the pool.** A single cap above the pool was refused; the sum
   never was. Five caps of 300k against a 1M pool were all accepted, turning the pool into
   first-come-first-served.
3. **A pool refusal named nobody.** Four coworkers stopped with no indication which one ate the
   month.

### What was learned building it

**One field was not enough, and the half-fix would have looked complete.** The plan said give the
judge a `spend_scope`. Reading what `ModelRequest` carries showed it also had `gateway_key: None`
— so scope alone would have refused the judge at a cap its own spending could never contribute
to reaching, because the meter the cap is read from would still not see its tokens. Both fields
were needed and neither is sufficient.

**Metering the judge fails in the right direction**, which was checked rather than hoped: a
refused judge returns `Unavailable`, which the executor turns into `ReviewOutcome::Ask`. So a
coworker over its cap raises a card for a person rather than proceeding unreviewed.

### Honest verification note

Only one of the three tests is genuinely red-to-green. The over-allocation test was copied alone
onto a throwaway `main` worktree and the 6,000,000 cap was **accepted** there. The other two
exercise APIs the PR introduces, so on `main` they fail to compile rather than fail an assertion.
Their evidence is direct code reading. This was stated in the PR rather than implying three red
tests.

### Deliberately left open

- **No ceiling above a member.** `PointsScope` has only `Member` and `Coworker`; the org default
  the original design specified was not carried across when dollars became points.
- **A limit can be overshot by one turn.** The meter is read before the call and never reconciled
  after; there is no reservation and no settle-up.
- **Concurrent turns share one reading.** `FRESH_MS` is 15s. Same problem as the overshoot,
  widened. Both need a reservation design agreed with the gateway side before any code.

---

## #54 — the gate blamed the code for something else

**Merged `c3f3d88`. Main green.**

Two files, +53 lines, no product code. Both fixes are to things that reported a failure in the
wrong place.

1. **A missing database.** The gate got through fmt, check and clippy, then died inside the
   integration tests with four red names. The tail read `GATE FAILED: tests`, so the obvious next
   move was to read four tests that were perfectly fine. Cost two full runs to learn.
2. **A racy transcript read.** `what_the_model_was_told` returned the newest finished answer,
   which has no notion of which turn produced it. On a test's second prompt it returns the
   **first** turn's answer whenever the second has not landed. It failed exactly that way inside
   a gate and printed turn one's system message, sending me looking for a regression in code that
   had not changed.

### The cleanup, proved rather than assumed

#53 and #55 were each carrying a copy of this commit so they could pass their own gate runs. The
three commits had **identical patch-ids**, so git should recognise them as the same change.
Checked before and after:

| Branch | Before merge | After merge |
|---|---|---|
| `member-key` (#53) | `+ gate: two ways it blamed…` | `-` (already upstream) |
| `member-transcript` (#55) | `+ gate: two ways it blamed…` | `-` (already upstream) |

Both dissolved with nobody editing anything, and no branch gained a new conflict site.

### Found while reading `gate.sh`, not fixed

- **The server's boot output is discarded** (`./target/debug/opengrok >/dev/null 2>&1`), so
  `fail "the server did not come up"` throws away the one thing that would explain why. Same
  class as the above, arguably worse: the cause is known at the moment it is dropped.
- **Docker is never checked** although four smokes need it.

---

## #56 — handlers that asked who the deployment was

**Merged `a1e28aa`. Main green.**

Six seam-A handlers resolved `account(state, &state.email)` — the deployment identity — and never
looked at the caller. It failed in both directions at once: a person could not see a routine they
had just created, and anyone signed in could list, edit and delete everyone else's, because
`owned_schedule` compared the schedule's owner to the deployment account rather than to them.

Proved on unmodified `main`, with a stranger receiving another member's routine:

```
another account's routines were listed to a stranger:
[{"prompt":"weekly report","trigger":{"type":"cron","schedule":"0 9 * * 1"}, ...}]
```

The same root cause had a second symptom: `agent_reply` built its answer from the deployment's
roster, so a coworker hired by anybody else was written correctly and then reported as
`"the hired coworker did not appear"` — **a 500 on a create that had succeeded**.

### A mistake I made and corrected

The first duplicate test had the harness owner duplicate their own coworker, and it **passed on
main**. The harness email *is* the deployment email, so the two identities coincided and the test
demonstrated nothing. It was rewritten to duplicate as a caller who is not the deployment, and
then failed on main as it should. This is the same failure shape being called out elsewhere in
this document — an example that does not demonstrate its own claim — and it was mine.

### Issue raised, not fixed: #58

Reviewing #56 before merging turned up that the **live push was not fixed along with the reads**.
`emit_automations` sends on a global broadcast and the SSE stream filters subscribers by channel
*name* only, never by account.

| | payload | delivery |
|---|---|---|
| before #56 | the deployment's pooled routines | every stream |
| after #56 | **one identified person's** routines | every stream |

#56 does not cause it and does not worsen the mechanism — it changes its character, from
uniformly wrong data to one person's correct data going to other people's streams. Today a
receiving client has no reason to render a routine for an agent it does not know, but that is the
client declining to display what it was sent: obscurity, not a lock. It becomes reachable when
#55 lands, which is why it is cross-referenced there.

Filed as **#58** with two candidate fixes. The tag-and-filter direction is recommended because
the alternative changes a channel name the desktop client subscribes to, and the client contract
is transcribed, never invented.

---

## #52 — a yes that says whose it is

**Merged `973916a`. Main green.**

Two changes in one PR, and they are **not equal**.

**Live the moment it merged:** a remembered permission approval now records the account that gave
it, so one member's yes can no longer authorise another member's identical command. A consent
record with no owner fails open the instant two people can reach one coworker.

**Inert until #55:** the visibility setting. Every path that could set a coworker to shared was
traced, and there is exactly one — the route that refuses it. So after #52 alone, **the setting
can only ever hold `private`**; the shared state is unreachable by any caller.

That refusal is deliberate and was the result of review feedback. Answering 200 would report a
coworker as shared on the roster while sharing nothing with anybody — a security-adjacent setting
reporting success and doing nothing, which is a false statement to a person rather than merely a
hazard for a future developer.

### Notes for the day it deploys

- An approval given **seconds before the deploy** has no account on it, and every caller
  afterwards arrives with one, so that record matches nobody and the retry asks again. It fails
  in the safe direction and expires in ten minutes.
- The consent predicate is `account_id is not distinct from $5`, so rows written before the
  column stay takeable by exactly the accountless caller they were written for and by nobody
  else. A broken condition on a consent check may only ever narrow.

---

## #53 — a key per person, not per coworker

**Merged `97db9ed`.** Rebased first; gate passed, CI green on the rebased commit.

When two people share one coworker, each person's turns should be billed to them and drawn from
their own pool, instead of everything landing on whoever hired it. The pair becomes the key row's
identity, the payer's pool sums the payer's own keys wherever they are, retirement revokes every
member's key rather than the hirer's, and all three per-coworker caches re-key on the pair.

### The rebase, and the one conflict that mattered

Landing #57 first was a deliberate trade: it meant #53 would conflict in the three files #57
rewrote. That bill came due here, and one of the five conflict sites was the follow-up #57's own
description committed to:

```
<<<<<<< HEAD  (#53)
    spend_scope: None,        // "the judge is the deployment's own call"
    spend_actor: None,
=======       (main, from #57)
    gateway_key: self.key.clone(),
    spend_scope: self.scope.clone(),
```

Both naive resolutions are silent failures:

- Take #53's side and the judge stops being metered — #57's fix quietly undone.
- Take main's side and the judge names a scope with no actor, which **#53 itself holds** — which
  is auto-review switched off everywhere, and nothing would have failed loudly.

The judge now carries all three, with the reasoning on the field itself so the next reader does
not have to reconstruct it.

The other four sites were the payer/owner rename colliding with #57's new `PoolReading` type,
resolved to keep **both** intents rather than picking a side: #53's payer naming and its
explanation of why the pool cache keys on the person alone, plus #57's heaviest-spender reading.

### A correction I made during this PR

Earlier in the session I reported #53's gate as green when it had failed. I had read the tail
without reading the cause. Two separate things were wrong and neither was this PR's code: a
database that did not exist, and the flaky transcript read later fixed by #54. The retraction was
made before anyone acted on it.

### An environment failure, not a code one

The merge was refused:

> `codeitlikemiley does not have the correct permissions to execute MergePullRequest`

Nothing about the PR. The active `gh` account had changed from `hexuria` to `codeitlikemiley`
partway through the session — the four earlier merges were all performed as `hexuria` — and the
now-active account holds `pull` only:

```
permissions = {"admin":false, "maintain":false, "pull":true, "push":false, "triage":false}
```

Reads still worked, which is why nothing else had shown it. There is no branch protection on
`main`; it was purely the account. Resolved by switching the active account to `hexuria` for the
merge and switching straight back, with the operator's say-so.

**Worth remembering:** a permissions error on a merge reads like a repository policy and can be
neither. Check which account is active before looking for branch protection.

---

## #55 — a conversation each, and a door that was never locked

**Merged onto main after the other five; gate passed, 505 tests.** Last in the order, deliberately:
largest change in the queue and the only one that alters authorisation on every command, so it
went onto the quietest possible base.

Two people share a coworker without sharing a transcript. Entries gain an account, backfilled to
each coworker's owner; `seq` stays per coworker so no existing row moves.

The larger half is what was found while building it: **seam A authorised nothing per coworker.**
Every verb resolved the caller from a token and none checked the coworker was theirs. Any
signed-in person who knew an id could read a transcript, send prompts as somebody else's
coworker, delete entries or flip approval cards. Survivable only because ids were undiscoverable
— and this slice makes them discoverable on purpose.

### Two holes found reviewing my own PR, both fixed in it

1. **`deleteAgents` was never gated.** It takes a list, and the extractor reads `agentId` then
   `id`, so the check never saw it. A colleague retired somebody else's coworker: `{"deleted": 1}`.
   I had written in a comment that this verb was "gated per id inside lifecycle". It was not.
2. **Sharing granted the write surface.** Every gated verb asked `may_use`, true for a colleague
   on a shared coworker. A colleague renamed Ada's coworker to "Bo's now" and the reply told him
   `mine: true`, while the roster promised him `canManage: false`.

The gate now asks two different questions, and the two lists fail in opposite directions on
purpose: a verb missing from the exempt list is *checked*, while a verb missing from the
ownership list falls back to the *wider* answer — so only the second one has a drift test.

### The gate was its own oracle

A smoke caught it. `updateAgent` answers `null` for an unknown id and the gate answered 404, so a
stranger could tell a real id from an invented one by which shape came back — the disclosure the
404 was chosen to prevent, one step removed. The refusal is now the verb's own not-found answer,
per verb.

### Known, and written into the code rather than only here

The refusal answers from a table immediately while a genuine unknown answers the same shape after
a store round trip. Same bytes, different latency, still sortable by someone who can time
replies. The fix is not a matching dummy lookup — that matches only until either query changes —
but authorisation inside the lookup itself.

### What merging this made true

Issue **#58** stops being unreachable. The routines push goes to every open stream, and until
this PR that was covered only because two accounts could not see the same coworker. That is no
longer so. The leak did not get worse; it stopped being theoretical, and closing it moved from
"worth doing" to "next".

### The merge conflict, predicted and confirmed

Before any of the six merged, `git merge-tree` was run over every pair of open branches. The
result was that **every collision between them was `docs/ROADMAP.md` and nothing else** — the
code auto-merged in all of them. That held exactly: this branch's only conflict with the final
main was one paragraph of the roadmap, resolved to keep every ticked entry including #53's note
about why the metered judge must carry both fields.

---

## An environment failure that cost two attempts

Halfway through the queue, merging began failing with:

> `codeitlikemiley does not have the correct permissions to execute MergePullRequest`

and later, pushing failed with a bare `403`.

Neither was a repository policy. There is no branch protection on `main`. The active `gh` account
had changed from `hexuria` to `codeitlikemiley` partway through the session — the earlier merges
were all performed as `hexuria` — and the now-active account holds `pull` only:

```
permissions = {"admin":false, "maintain":false, "pull":true, "push":false, "triage":false}
```

Reads kept working, which is exactly why it stayed invisible until a write was attempted. Git
pushes failed too, because the https credential helper follows the active `gh` account.

Resolved each time by switching to `hexuria`, performing the one action, and switching straight
back — with the operator's say-so the first time.

**Worth remembering:** a permissions error on a merge reads like a branch-protection rule and can
be neither. Check which account is active before going looking for repository policy.

---

## Live verification

CI and the local clean-env gate are not a running server. After the fifth merge the dev server on
`:1447` was rebuilt from the merged main and restarted, and four things were checked rather than
assumed:

- it boots clean — `opengrok listening bind=127.0.0.1:1447`, no errors or panics in that boot
- `/health` answers 200 and the desktop client reconnects to it
- **all four schema changes actually landed**: `coworker_view.visibility`,
  `mcp_allow_once.account_id`, `coworker_gateway_key.secret_scoped`, and the composite
  `coworker_gateway_key_pair` index
- the binary on disk is the one the server is running

One note from the restart: `serve.sh` reported `still draining after 10s; kill -9`. That is the
script's documented behaviour — graceful shutdown holds open SSE connections — not a fault, but
it does mean the previous process was force-killed with a client attached.

---

## Patterns worth carrying out of this

**A comment stating a property is a claim, not evidence.** Three times a comment sat directly
above code that did not have the property, and twice the comment was mine. The more precisely
true a comment sounds, the less likely anybody is to test it.

**An example that does not demonstrate its own claim survives review by agreement.** It happened
in both directions between me and the reviewer, and each time the conclusion was right while the
example could not produce it.

**A list that must agree with code, with nothing forcing agreement, will drift.** The exempt-verb
list got a test that reads the dispatch and refuses a half-exempt arm. It found a fifth split arm
on its first run that neither reader had seen.

**Verify against the branch the claim is about.** Both sessions nearly shipped findings read from
the wrong ref.

**Retract a green before anyone acts on it.** A gate that failed on a missing database is not a
passing gate, however inconvenient the correction is.
