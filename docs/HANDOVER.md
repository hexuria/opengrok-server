# Handover

You are picking up OpenGrok in a fresh session. This page is the state of play; everything else
is reference. Rewritten 25 Sep 2026 for the client served today. Previous versions:
`git show d73042b:docs/HANDOVER.md` (last updated 6 Sep, the Grok Bot era),
[`archive/handover-2026-09-01.md`](archive/handover-2026-09-01.md) (1 Sep) and
`git show 99ec5c3:docs/archive/handover-2026-08-29.md` (P0 era).

**Read [`../CLAUDE.md`](../CLAUDE.md) first** (it loads automatically), then this, then act.
CLAUDE.md's client line and its "Three facts" #1 and #3 still describe the Grok Bot desktop
client. Its non-negotiables stand as written, but for **which client, and how it connects**,
this page and [`setup/nativechat.md`](setup/nativechat.md) are current. The pre-removal facts are
kept in [`archive/seam-a-client-facts.md`](archive/seam-a-client-facts.md).

## Where this stands, in one paragraph

The server is **real and serving**, and its client is **NativeChat** (`hexuria/nativechat`, a
native desktop app in Rust + GPUI). NativeChat talks to `POST /ag-ui` and the REST routes beside
it; [`research/client-nativechat.md`](research/client-nativechat.md) maps every route the server
mounts. The Grok Bot desktop client is discontinued. On **20 Sep 2026** P0-E deleted the two doors
built for it: seam A (`POST /api/{method}`, `GET /events`) and seam B (ConnectRPC, its tonic
mirror, `opengrok-proto`), about 20,500 lines (`ROADMAP.md`, Phase 0). Its host-settings verbs
were re-homed first, at `GET/PUT /ag-ui/host-settings`. What exists: our own auth and OAuth, the
AG-UI endpoint with streamed text and widgets, the durable harness (journal, suspension, stop,
resume, crash recovery), computers (local Docker by default, box.ascii.dev with a key), connectors
with a credential vault, schedules, monitors and webhook routines, the MCP door for Claude Code,
orgs, invites and credential accounts, the web console at `/console`, the consent model (policy
tiers, never-expiring cards, a model judge), per-coworker model pins and points limits, recipes,
skills and workflows, site logins, reverse-exec to the person's own Mac, and one artifact store.
[`ROADMAP.md`](ROADMAP.md) is the tracker. A box is ticked only in the commit that makes it true.

## How to stand it up

[`setup/`](setup/README.md), in order: postgres → environment → running → **first run** (the first
admin, the gateway key, a real tool-calling turn) → gate → TLS → **NativeChat** (connect it, then
the demo runbook: a chat, a card, the computer, a routine). `scripts/serve.sh` builds and
(re)starts the dev server from `.env`. `scripts/gate.sh --smoke` is the merge gate, and CI runs the
same script.

## Decisions already made — do not relitigate

Recorded with their reasoning where they belong. Overturn deliberately with the operator, never
by drift.

| Decision | Where |
|---|---|
| Rust, Axum 0.8, sqlx 0.9, edition 2024, crate-per-concern mirroring open-ai-gateway | `PLAN.md` §3 |
| Our own loop and our own door — the suspension is the product; the `rig-core` door was retired 17 Sep 2026 | `PLAN.md` §4.2, `GOAL.md` stack |
| The client contract is transcribed, never invented; no vendored generated stubs. For NativeChat that means a route's place in the contract is proven by the NativeChat file that calls it — the route map says which rows are still unread | `CLAUDE.md` #1, #3; `research/client-nativechat.md` |
| Every model call exits through open-ai-gateway; a pin is a route, not a key. The shipped default route is `xai/grok-4.6`, chosen because it is on the record making tool calls | `CLAUDE.md` #4; `setup/environment.md` |
| One consent model: the server decides, cards never expire, judge failure = ask | `AUTO-REVIEW.md` §0 |
| Repo went public 1 Sep 2026 with the rights review still outstanding — the transcription rule is harder, not softer | this page |
| Redis only after a measured hot query | `ROADMAP.md` Later |
| A coworker's computer is a seam (`Computer`), not a vendor. ASCII is one adapter over a typed v1 client; do not invent vendor shapes | `PLAN.md` §4.3, `research/sandbox-box-ascii-dev.md` |
| Live site wins if `docs/box/` drifts; vendor pages are ASCII's, not ours | `box/README.md` |
| The shell vouches, the console proves: a domain from `opengrok admin` admits signups at once; a console claim admits nothing until DNS says so | `opengrok-core/src/org.rs` module doc, `opengrok-server/src/domain_proof.rs` |
| The first org and admin are made from the operator's shell, never over HTTP: a fresh server has nobody to authorize an admin call | `crates/opengrok/src/admin.rs`, `setup/first-run.md` |
| A coworker's spend is metered on a gateway key of its own, limited in **points** (one token at the gateway's reference price); a member's pool is the PAYER's; the server refuses at a limit with a sentence naming it; a key that cannot be opened holds the turn | [`plan-spend-policy.md`](plan-spend-policy.md), `opengrok-server/src/spend.rs`, `points.rs` |
| Every model call the server makes is metered, including the auto-review judge — which needs a scope AND a key AND an actor | `opengrok-harness/src/review.rs` |
| What a second replica must see is a row taken once with `delete … returning`; budgets and caches stay per replica | `opengrok-store/src/replica.rs`, `auth/budget.rs` |
| Every surviving door takes a signed per-account token (or a coworker's bot key) and has no identity fallback to fail open into. The 5 Sep 2026 bug it closed — both ends of seam A failing open, each citing the other's fallback — is recorded in `git show d73042b:docs/HANDOVER.md` | `agui/routes.rs::principal_from_bearer` |
| A refusal of somebody else's thing is a 404, never a 403: a person who may not use a coworker must not learn it exists | per route; the ownership checks in `agui/routes.rs` |
| The schema is idempotent statements run on every boot under one advisory lock; a data-transforming migration follows the rules in `setup/postgres.md` | `opengrok-store/src/migrations.rs` |
| A PR is based on `main`, never stacked on a branch about to merge — GitHub closes a PR whose base branch is deleted and it cannot be reopened | this page, 2 Sep 2026 |

## What's left

[`ROADMAP.md`](ROADMAP.md) holds the unticked boxes: the `*.later` boxes and the Later bucket.
The open issues on `hexuria/opengrok-server` hold the rest, many of them filed on 25 Sep 2026 by a
demo-readiness audit. Take them from the tracker, not from a list copied here, which would go
stale the day it was written. Three things that are easy to lose:

- **The NativeChat route map's client column.** Every `*unverified*` row in
  [`research/client-nativechat.md`](research/client-nativechat.md) is waiting on somebody with
  `hexuria/nativechat` checked out. Until then, a route with no NativeChat file behind it may be
  used or unused, and nobody can say which.
- **Reverse-exec findings (#147).** Four of five land in NativeChat. The one server-side stage is
  small and needs no decision.
- **The points meter's three gaps.** No ceiling above a member. A limit can be overshot by one
  turn. Turns inside the 15 s freshness window share one reading. They are recorded in
  [`plan-spend-policy.md`](plan-spend-policy.md), and the last two want a reservation design
  agreed with the gateway session before any code.

**Operational things that each cost time:**

- **Check which `gh` account is active before diagnosing a merge failure.** A permissions error
  on merge reads like branch protection. Reads keep working, so it stays invisible until a write.
- **`git merge-tree` over every pair of open branches before choosing a merge order.**
- **The dev Postgres has no volume.** A Docker restart wipes every database on it. Keep demo data
  on the durable container in [`setup/postgres.md`](setup/postgres.md), and back it up.
- **A local `gate.sh --smoke` kills whatever holds its port.** Run it on `OG_PORT=1449` when a live
  server holds `1447`, and `scripts/serve.sh` again afterwards before trusting the client.

## Blocked on the operator, not on code

The rights review is **overdue** (repo public 1 Sep 2026 with it still outstanding).
`gpt-5.6-luna` is on an upstream spending limit and makes no tool calls through the gateway. It
is no longer the shipped default (`xai/grok-4.6` is), but coworkers already pinned to it stay
there until repinned. Details are at the bottom of [`ROADMAP.md`](ROADMAP.md).

## The map

| Read this | For |
|---|---|
| [`GOAL.md`](GOAL.md) | the mission and the stack decisions |
| [`DIAGRAMS.md`](DIAGRAMS.md) №1 | the idea in five minutes of pictures |
| [`WHY.md`](WHY.md) | what we built before and why a working app wasn't enough |
| [`ROADMAP.md`](ROADMAP.md) | what is done (with commits) and what is left |
| [`setup/`](setup/README.md) | standing the server up, end to end, and connecting NativeChat |
| [`research/client-nativechat.md`](research/client-nativechat.md) | every route, what it answers, and who is on record calling it |
| [`AUTO-REVIEW.md`](AUTO-REVIEW.md) | the consent model and the judge |
| [`research/`](research/README.md) | the gateway, the sandbox, connectors, the prior product, and the removed client's record |
| [`box/`](box/README.md) | local copy of box.ascii.dev Public API v1 (vendor pages; live site wins) |
| [`verification/`](verification/) | the evidence behind the ticked boxes |

Neighbouring repositories: `hexuria/nativechat` (the client we serve),
`/Volumes/goldcoders/OSS/open-ai-gateway` (the model door), and
`/Volumes/goldcoders/projects/opensesame/opensesame` (the prior product; if a lesson doc
contradicts that repo, the repo is newer). `/Volumes/goldcoders/OSS/opengrok` is the removed Grok
Bot client, kept as the source of the transcription record.
