# Handover

You are picking up OpenGrok in a fresh session. This page is the state of play; everything else
is reference. Rewritten 2 Sep 2026 — previous versions:
[`HANDOVER-9v-10.3.md`](HANDOVER-9v-10.3.md) (1 Sep, after 9.v / 10.3) and
[`archive/handover-2026-08-29.md`](archive/handover-2026-08-29.md) (P0-era).

**Read [`../CLAUDE.md`](../CLAUDE.md) first** (it loads automatically), then this, then act.

## Where this stands, in one paragraph

The server is **real and serving**. Slices 1–18 are done (12.later included): auth and our own OAuth, the AG-UI
endpoint, the durable harness, computers (local Docker + box.ascii.dev), connectors with a
credential vault, the scheduler/monitor autonomy pair, the gateway port that boots the packaged
desktop client (P2–P10 breadth), seam B transcribed, bot-keys, orgs/invites/credential accounts,
the web console at `/console`, the consent model (per-machine policy, never-expiring cards,
two-tier auto-review with a model judge), Claude Code through both doors (model + MCP, with
card-driven AutoReview Ask), org gateway keys from the console, and per-coworker model pins
that survive a resume. A typed Box Public API v1 client (`opengrok_box::ascii::Client`, shapes
from [`box/`](box/README.md)) replaced the old single-file ASCII driver; `AsciiBoxes` is still
the `Computer` adapter. Domain ownership is proven, not assumed: a console admin's claim admits
nobody until its `_opengrok-verify` TXT record resolves, while the operator's shell still vouches
directly; password reset rides Resend with a one-shot signed link (12.later). Hexuria's right-sidebar screen paints a live noVNC desktop from
`getForeverBoxStatus.vncUrl`. **Slice 19 is done and merged (4 Sep 2026): a coworker can be shared.** A standing role reaches
the model every turn; an owner marks a coworker `org`; two people hold one coworker without
sharing a conversation, a gateway key, a pool or a permission card. Building it uncovered that
**seam A authorised nothing per coworker** — every verb checked that the caller was somebody and
none checked the coworker was theirs — which is now one gate before the dispatch. A dev instance
runs on `:1447` against the real judge, rebuilt from `main` after the last merge.
[`ROADMAP.md`](ROADMAP.md) is the tracker — a box is ticked only in the commit that makes it
true. Unticked work is the `*.later` boxes and the Later bucket, not a missing slice.

## How to stand it up

[`setup/`](setup/README.md), in order: prerequisites → postgres → environment → running → gate
→ desktop-client. `scripts/serve.sh` builds and (re)starts the dev server from `.env`;
`scripts/gate.sh --smoke` is the merge gate, and CI runs the same script since the repo went public (1 Sep 2026).

## Decisions already made — do not relitigate

Recorded with their reasoning where they belong; overturn deliberately with the operator, never
by drift.

| Decision | Where |
|---|---|
| Rust, Axum 0.8, sqlx 0.9, edition 2024, crate-per-concern mirroring open-ai-gateway | `PLAN.md` §3 |
| Rig for providers, our own loop for durability — the suspension is the product | `PLAN.md` §4.2 |
| The client contract is transcribed, never invented; no vendored protobuf stubs | `CLAUDE.md` #1, `LEGAL.md` |
| Every model call exits through open-ai-gateway; a pin is a route, not a key | `CLAUDE.md` #4 |
| Port from the client's own mock (2 services, 18 methods), never the proto inventory | `PORT-PRIORITY.md` §3 |
| One consent model: the server decides, cards never expire, judge failure = ask | `AUTO-REVIEW.md` §0 |
| Repo went public 1 Sep 2026 with the rights review still outstanding — transcription rule is harder, not softer | `LEGAL.md` |
| Redis only after a measured hot query; artifacts land with the harness's first real files | `ROADMAP.md` Later |
| A coworker's computer is a seam (`Computer`), not a vendor. ASCII is one adapter over a typed v1 client; do not invent vendor shapes | `PLAN.md` §4.3, `research/sandbox-box-ascii-dev.md` |
| Live site wins if `docs/box/` drifts; vendor pages are ASCII's, not ours | `box/README.md` |
| The shell vouches, the console proves: a domain from `opengrok admin` admits signups at once; a console claim admits nothing until DNS says so. Verify is a live lookup on click, no poller | `opengrok-core/src/org.rs` module doc, `opengrok-server/src/domain_proof.rs` |
| A coworker's spend is metered on a gateway key of its own, and its limits are three windows — rolling 5 hours, rolling 7 days, calendar month — evaluated by the server before each model call from the gateway's ledger; at a limit the turn is refused with a sentence that names the window and when it resets; a key that cannot be opened holds the turn rather than falling back to the deployment's key | [`plan-spend-policy.md`](plan-spend-policy.md), `opengrok-server/src/spend.rs` |
| What a second replica must see is a row taken once with `delete … returning`; budgets and caches stay per replica | `opengrok-store/src/replica.rs`, `auth/budget.rs` |
| A PR is based on `main`, never stacked on a branch about to merge — GitHub closes a PR whose base branch is deleted and it cannot be reopened | this page, 2 Sep 2026 |
| Sharing lets somebody TALK to a coworker; it is never a write grant. The gate asks two questions — `may_use` (owner or org-shared) and `owns` (owner only) — and the two lists fail in opposite directions on purpose, so only the ownership one has a drift test | `gateway/routes.rs`, `tests/against_constant_verbs.rs` |
| A refusal is the verb's OWN not-found answer, per verb — never 403, and never a uniform 404. A 404 where the verb answers `null` for an unknown id is the same disclosure one step removed | `gateway/routes.rs::never_heard_of_it` |
| A live frame carries WHO IT IS FOR. `None` is a payload naming nobody; `Some(account)` reaches that account's streams alone. Stamped frames carry no audience — filtering one spends a sequence other streams expect | `gateway/live.rs`, issue #59 |
| Points: a member's pool is the PAYER's — the person talking, not the hirer. Three caches key on three different things on purpose (pair, pair, payer-alone) and harmonising them reintroduces the bug | `opengrok-server/src/spend.rs` |
| Every model call the server makes is metered, including the auto-review judge — which needs a scope AND a key AND an actor; any two of the three is a silent half-fix | `opengrok-harness/src/review.rs` |

## What's left (do not relitigate "are we done")

In the order a fresh session should take them. Detail and the tick-rule live in [`ROADMAP.md`](ROADMAP.md). There is no missing slice.

**The queue is empty as of 4 Sep 2026.** Seven pull requests merged in order, each verified the
same way — local clean-env gate, CI on the branch, CI on `main` after the merge, and a live
rebuild of `:1447` — then `main` was deployed and its schema checked in the database:

| | What it was | Merge |
|---|---|---|
| #57 | the points meter counted everything except the auto-review judge | `f075711` |
| #54 | two ways the gate blamed the code for something else | `c3f3d88` |
| #56 | six handlers asked who the deployment was, not who was asking | `a1e28aa` |
| #52 | visibility, and a consent record that says whose yes it is | `973916a` |
| #53 | a gateway key per person, not per coworker | `97db9ed` |
| #55 | a conversation each — and the door that was never locked | `b54ea4e` |
| #60 | a live frame goes to the stream it is for | `c1555b4` |

The full account — what each was for, what broke, what was found reviewing it, and the wrong
turns kept in rather than tidied away — is in [`../clearing-up-pr.md`](../clearing-up-pr.md).
Read that before re-opening any of it.

**Three issues are open and none is guessed at.** Each was investigated to the point where the
next person can act, and deliberately not started:

- **#61 — chat renders in one burst.** The app's answer arrives all at once. The bubble is
  already marked `"streaming": true` and the two calls that would grow it already exist and run
  ONCE (`gateway/conversation.rs`). The journal guarantee is per-ROUND and about the server's own
  ordering, so streaming does not weaken it — and the comment defending the buffering describes a
  property the code does not have. **A restart mid-answer leaves an empty bubble marked
  "typing" forever**; nothing anywhere flips that flag off. That last part is broken today,
  independent of streaming, and is the smallest useful thing to fix first.
- **#59 — the roster stream sends the deployment's roster to everybody.** `listAgents` is
  correctly per-caller; the SSE opener and every update frame are not. It cannot take #60's fix
  because those frames are stamped: filtering one per person spends a sequence the other streams
  expect. Needs per-account sequences, which is a replica-contract change.
- **Three gaps in the points meter**, recorded in the plan file: no ceiling above a member
  (`PointsScope` has only `Member` and `Coworker`); a limit can be overshot by one turn (the
  meter is read before the call and never reconciled after); and turns inside the 15s freshness
  window share one reading. The last two are one problem and want a reservation design agreed
  with the gateway session before any code.

**Operational things that each cost time this session:**

- **Check which `gh` account is active before diagnosing a merge failure.** A permissions error
  on merge reads like branch protection and was neither — the active account had changed to one
  with `pull` only. Reads keep working, so it stays invisible until a write.
- **`git merge-tree` over every pair of open branches before choosing a merge order.** It said
  every collision between the six was `docs/ROADMAP.md` and nothing else, and that held exactly.
- **Identical patch-ids dissolve on their own.** Two branches carried a copy of #54; merging #54
  first made both vanish with nobody editing anything. Verified with `git cherry` before and
  after rather than assumed.

1. **Rooms** — [`plan-rooms.md`](plan-rooms.md): the sharing verbs answer in the client's
   shapes (#35) and groups are built (a coworker with members, the client's own orchestrator
   transcribed, `gateway/group.rs`). Left: shared rooms, parked until groups have been used.
   Note that slice 19 shared a COWORKER, which is not a room: one coworker, several people, a
   conversation each. Rooms are several coworkers in one conversation.
2. **`17.later`** — SSO/SCIM onto the gateway's `oidc_subject` hook; self-service key rotation;
   per-key admin scopes so a partner credential is not a full gateway admin.
3. **`18.later`** — Seam B `UpdateGrokBotAgent` has no repin. Desktop create/update model field +
   picker (console already has one). Roster `description = model` habit. `auto_review_model` is
   a separate deployment pin. Per-coworker limits are **points** — one token at the gateway's reference price, so seats
   and API keys count the same — monthly per member (the admin's pool) and per coworker (the
   owner's cap, at most the pool) with an optional daily brake; decided 3 Sep 2026, design in
   [`plan-spend-policy.md`](plan-spend-policy.md), built as 18.points with gateway #52/#53.
   The USD windows' limits are retired. Templates carry points. As of slice 19 the pool is the
   PAYER's, not the hirer's. Left: drop the `spend_limit` table after a month; retire
   `/coworkers/{id}/spend` once the desktop modal no longer reads it; org-wide per-model budgets
   and "apply a template edit to its coworkers" follow — and the three meter gaps above.
4. **Later bucket** — `goal`/`plan`/`review` parked until the packaged app sends a `mode`
   (`verification/plan-mode-wire/`); passkey step-up for reverse-exec; mem0; artifacts; stdio
   MCP inside the box; graph harness; Redis after a measured hot query. Rate-limit budgets are
   the one thing that stays per process when a second replica appears (by design — a limit that
   costs a database write per unauthenticated request defeats itself); a shared limiter is Redis
   work, after that measured hot query. ASCII endpoints not in the client yet wait until a
   coworker path needs them.
5. **P11 is not unfinished work** — teach recording and memories sit on no path a user takes;
   upstream deleted adjacent features in 0.30. Sharing is now `plan-rooms.md` §3.

The desktop app you verify against is **`/Applications/Open Grok.app`** (`bot.opengrok.app`). Do not run `just install` in the client repo — that justfile still writes `/Applications/Grok-0.27.app`. Install in place: `rsync -a --delete "dist/Open Grok.app/" "/Applications/Open Grok.app/"`. `setup/desktop-client.md`.

## Blocked on the operator, not on code

The rights review is **overdue** (repo public 1 Sep 2026 with it still outstanding — `LEGAL.md`), and gpt-5.6-luna is on an upstream spending limit (5.5 / 5.4-mini work through the same gateway). GitHub Actions CI runs `scripts/gate.sh --smoke` itself since the repo went public. Details at the bottom of [`ROADMAP.md`](ROADMAP.md).

## The map

| Read this | For |
|---|---|
| [`GOAL.md`](GOAL.md) | the mission and the stack decisions |
| [`DIAGRAMS.md`](DIAGRAMS.md) №1 | the idea in five minutes of pictures |
| [`WHY.md`](WHY.md) | what we built before and why a working app wasn't enough |
| [`ROADMAP.md`](ROADMAP.md) | what is done (with commits) and what is left |
| [`setup/`](setup/README.md) | standing the server up, end to end |
| [`AUTO-REVIEW.md`](AUTO-REVIEW.md) | the consent model and the judge |
| [`LEGAL.md`](LEGAL.md) | the line, before touching the client contract |
| [`research/`](research/README.md) | the client, the gateway, the sandbox, connectors, the prior product |
| [`box/`](box/README.md) | local copy of box.ascii.dev Public API v1 (vendor pages; live site wins) |
| [`verification/`](verification/) | the evidence behind the ticked boxes |

Neighbouring repositories, all local: `/Volumes/goldcoders/OSS/opengrok` (the client we serve),
`/Volumes/goldcoders/OSS/open-ai-gateway` (the model door), and
`/Volumes/goldcoders/projects/opensesame/opensesame` (the prior product; if a lesson doc
contradicts that repo, the repo is newer).
