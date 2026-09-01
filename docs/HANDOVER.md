# Handover

You are picking up OpenGrok in a fresh session. This page is the state of play; everything else
is reference. Rewritten 2 Sep 2026 — previous versions:
[`HANDOVER-9v-10.3.md`](HANDOVER-9v-10.3.md) (1 Sep, after 9.v / 10.3) and
[`archive/handover-2026-08-29.md`](archive/handover-2026-08-29.md) (P0-era).

**Read [`../CLAUDE.md`](../CLAUDE.md) first** (it loads automatically), then this, then act.

## Where this stands, in one paragraph

The server is **real and serving**. Slices 1–18 are done: auth and our own OAuth, the AG-UI
endpoint, the durable harness, computers (local Docker + box.ascii.dev), connectors with a
credential vault, the scheduler/monitor autonomy pair, the gateway port that boots the packaged
desktop client (P2–P10 breadth), seam B transcribed, bot-keys, orgs/invites/credential accounts,
the web console at `/console`, the consent model (per-machine policy, never-expiring cards,
two-tier auto-review with a model judge), Claude Code through both doors (model + MCP, with
card-driven AutoReview Ask), org gateway keys from the console, and per-coworker model pins
that survive a resume. A typed Box Public API v1 client (`opengrok_box::ascii::Client`, shapes
from [`box/`](box/README.md)) replaced the old single-file ASCII driver; `AsciiBoxes` is still
the `Computer` adapter. Hexuria's right-sidebar screen paints a live noVNC desktop from
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

## What's left (do not relitigate "are we done")

In the order a fresh session should take them. Detail and the tick-rule live in [`ROADMAP.md`](ROADMAP.md). There is no missing slice.

1. **`12.later`** — DNS *ownership* proof (matching already works); password reset via Resend. Console admin for invites/enable shipped in slice 13.
2. **`16.later`** — OAuth 2.1 metadata on `/mcp`. PolicyApproval still has no transcribed desktop card (AutoReview Ask does — `16.cards`). Reverse-exec stays excluded from MCP.
3. **`17.later`** — SSO/SCIM onto the gateway's `oidc_subject` hook; self-service key rotation; mint idempotency; reconcile console listing vs gateway after a failed revoke; per-key admin scopes so a partner credential is not a full gateway admin. Per-member model pins are gateway-member pins, not coworker pins (those are slice 18).
4. **`18.later`** — Seam B `UpdateGrokBotAgent` has no repin. Desktop create/update model field + picker (console already has one). Roster `description = model` habit. `auto_review_model` is a separate deployment pin. Per-coworker spend caps.
5. **Later bucket** — `goal`/`plan`/`review` parked until the packaged app sends a `mode` (`verification/plan-mode-wire/`); passkey step-up for reverse-exec; rooms (provisioning shipped); mem0; artifacts; stdio MCP inside the box; graph harness; Redis after a measured hot query. ASCII endpoints not in the client yet (snapshots, environments, webhooks, ASCII's in-box prompt agent, secrets, repos, artifacts, events, `/me`) wait until a coworker path needs them.
6. **P11 is not unfinished work** — sharing, teach recording, memories sit on no path a user takes; upstream deleted adjacent features in 0.30.

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
