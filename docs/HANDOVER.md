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
`getForeverBoxStatus.vncUrl`. A dev instance runs on `:1447` against the real judge.
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

## What's left (do not relitigate "are we done")

In the order a fresh session should take them. Detail and the tick-rule live in [`ROADMAP.md`](ROADMAP.md). There is no missing slice.

**Landed or in review on 2 Sep 2026** (the operator decides and the peer session merges on their
explicit go, PR by PR — no human presses the button, and no session merges without the go): #26 one
spelling ("Open Grok"); #27 budgets on every unauthenticated door (`auth/budget.rs`); #30 a
durable audit of every MCP door call (`mcp_call_audit`, console "Door calls"); #31 the three
process maps a second replica would break, as rows (`opengrok_store::replica`); #32 per-coworker
spend caps (a gateway key of the coworker's own; being reworked to the three-window shape the
operator decided, [`plan-spend-policy.md`](plan-spend-policy.md));
open-ai-gateway #50 the per-key usage endpoint #32 reads. Each PR body carries its evidence and the
decisions it asks for. The plan they came from: `~/.claude/plans/elegant-marinating-noodle.md`
(session-local) — the order was hardening → caps → a rooms plan.

1. **Rooms** — [`plan-rooms.md`](plan-rooms.md): the sharing verbs answer in the client's
   shapes (#35) and groups are built (a coworker with members, the client's own orchestrator
   transcribed, `gateway/group.rs`). Left: shared
   rooms, parked until groups have been used.
2. **`17.later`** — SSO/SCIM onto the gateway's `oidc_subject` hook; self-service key rotation;
   per-key admin scopes so a partner credential is not a full gateway admin.
3. **`18.later`** — Seam B `UpdateGrokBotAgent` has no repin. Desktop create/update model field +
   picker (console already has one). Roster `description = model` habit. `auto_review_model` is
   a separate deployment pin. Per-coworker limits are **points** — one token at the gateway's reference price, so seats
   and API keys count the same — monthly per member (the admin's pool) and per coworker (the
   owner's cap, at most the pool) with an optional daily brake; decided 3 Sep 2026, design in
   [`plan-spend-policy.md`](plan-spend-policy.md), built as 18.points with gateway #52/#53.
   The USD windows' limits are retired. Templates carry points. Left: drop the `spend_limit`
   table after a month; retire `/coworkers/{id}/spend` once the desktop modal no longer reads
   it; org-wide per-model budgets and "apply a template edit to its coworkers" follow.
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
