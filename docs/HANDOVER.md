# Handover

You are picking up OpenGrok in a fresh session. This page is the state of play; everything else
is reference. Rewritten 1 Sep 2026, at merge `12748f0` — the previous, P0-era version is at
[`archive/handover-2026-08-29.md`](archive/handover-2026-08-29.md).

**Read [`../CLAUDE.md`](../CLAUDE.md) first** (it loads automatically), then this, then act.

## Where this stands, in one paragraph

The server is **real and serving**. Slices 1–14 are done: auth and our own OAuth, the AG-UI
endpoint, the durable harness, computers (local Docker + box.ascii.dev), connectors with a
credential vault, the scheduler/monitor autonomy pair, the gateway port that boots the packaged
desktop client (P2–P10 breadth), seam B transcribed, bot-keys, orgs/invites/credential accounts,
the web console at `/console`, and the consent model (per-machine policy, never-expiring cards,
two-tier auto-review with a model judge). A dev instance runs on `:1447` against the real judge.
[`ROADMAP.md`](ROADMAP.md) is the tracker — a box is ticked only in the commit that makes it
true — and its unticked boxes are the work: 12.later, and the "Later" bucket. 9.v and 10.3
closed 1 Sep 2026.

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
| Repo stays private until a rights review clears it | `LEGAL.md` |
| Redis only after a measured hot query; artifacts land with the harness's first real files | `ROADMAP.md` Later |

## Blocked on the operator, not on code

GitHub Actions billing (the local gate is the gate), the rights review before publication, and
the gpt-5.6-luna upstream spending limit. Details at the bottom of [`ROADMAP.md`](ROADMAP.md).

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
| [`verification/`](verification/) | the evidence behind the ticked boxes |

Neighbouring repositories, all local: `/Volumes/goldcoders/OSS/opengrok` (the client we serve),
`/Volumes/goldcoders/OSS/open-ai-gateway` (the model door), and
`/Volumes/goldcoders/projects/opensesame/opensesame` (the prior product; if a lesson doc
contradicts that repo, the repo is newer).
