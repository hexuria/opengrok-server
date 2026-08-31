# OpenGrok

The server the AI coworkers live on.

One Rust service that owns the agent harness, the tools, the computers and the policy — shipped
together with [open-ai-gateway](https://github.com/hexuria/open-ai-gateway) as a single AI
infrastructure. Clients are windows onto it: the reconstructed Grok Bot desktop app connects
through its OpenGrok server mode, any AG-UI client (openbot among them) through `POST /ag-ui`,
and a browser through the web console at `/console`.

```
desktop app / AG-UI / console  ──▶  OpenGrok  ──▶  open-ai-gateway  ──▶  models
                                      └──▶  a computer per coworker (Docker / box.ascii.dev)
```

A coworker keeps working when you close the tab, because the work was never in the tab.

## Status

Slices 1–14 are done and the server is real: auth, the AG-UI endpoint, the durable harness,
computers, connectors, the scheduler/monitor autonomy pair, the gateway port that boots the
packaged desktop client, seam B, orgs and invites, the web console, and the consent model with
model-judged auto-review. **[`docs/ROADMAP.md`](docs/ROADMAP.md) is the tracker** — a box is
ticked only in the commit that makes it true, and its unticked boxes are the remaining work.

## Quick start

```sh
# 1. Postgres (the gateway's dev instance) and OpenGrok's database
cd /Volumes/goldcoders/OSS/open-ai-gateway && just dev
docker exec oag-dev-postgres-1 psql -U oag -d postgres -c 'create database opengrok'

# 2. configuration
cp .env.example .env       # then fill the secrets — docs/setup/environment.md

# 3. build, start, verify
scripts/serve.sh
curl -fsS http://127.0.0.1:1447/health

# 4. the gate (what CI would run; CI is billing-blocked, so this IS the gate)
cargo build -p opengrok && OG_PORT=1449 \
  OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate scripts/gate.sh --smoke
```

The full chain, one file per topic: **[`docs/setup/`](docs/setup/README.md)** —
postgres → environment → running → gate → desktop-client.

## Start here

| | |
|---|---|
| **Picking this up?** | [`docs/HANDOVER.md`](docs/HANDOVER.md) — state of play, decisions made, where the work is |
| **The idea, in pictures** | [`docs/DIAGRAMS.md`](docs/DIAGRAMS.md) — five minutes |
| **Why this exists** | [`docs/WHY.md`](docs/WHY.md) — what we built before and why a working app wasn't enough |
| **The mission and the stack** | [`docs/GOAL.md`](docs/GOAL.md) |
| **What's done, what's left** | [`docs/ROADMAP.md`](docs/ROADMAP.md) — the single tracker |
| **Standing it up** | [`docs/setup/`](docs/setup/README.md) |
| **The consent model** | [`docs/AUTO-REVIEW.md`](docs/AUTO-REVIEW.md) — policy tiers, the judge, the cards |
| **The invariants** | [`CLAUDE.md`](CLAUDE.md) — ten rules that are not up for negotiation |
| **The rights line** | [`docs/LEGAL.md`](docs/LEGAL.md) — read before touching the client contract |
| **Reference docs** | [`docs/research/`](docs/research/README.md) — the client, the gateway, the sandbox, connectors, the prior product |

## Layout

```
crates/
  opengrok          the binary; wires the server, embeds the gateway, drives the scheduler tick
  opengrok-core     ids, errors, domain types, domain events. No I/O. Everything depends on it; it depends on nothing.
  opengrok-wire     the client contract: commands, transcript entries, activity, AG-UI events
  opengrok-proto    seam B transcribed: Connect-over-HTTP/1.1 messages (prost). Read its lib.rs before touching it.
  opengrok-harness  the agent loop (Rig): turns, tool calls, streaming, durability; the auto-review judge
  opengrok-box      the coworker's computer — a trait; local Docker and box.ascii.dev today
  opengrok-tools    tool definitions and the executor; MCP client (rmcp) for plugins
  opengrok-policy   what a principal may make a coworker do
  opengrok-store    Postgres: append-only event store + projections (CQRS reads), runs, scheduler rows
  opengrok-server   Axum: the host-facing API, the SSE event stream, the AG-UI endpoint, /console
docs/
  setup/ · research/ · verification/ · archive/ · the documents in the table above
scripts/
  serve.sh (run the dev server) · gate.sh (the merge gate) · slice*-smoke.sh (the evidence)
web/
  the web console (Bun/Vite/React SPA served at /console)
```

## A note on privacy

This repository stays **private** until an independent rights review clears it. See
[`docs/LEGAL.md`](docs/LEGAL.md) — the reasoning is short, specific, and load-bearing.

## Licence

MIT for the code authored here. Third-party material is not relicensed by that grant; see
`docs/LEGAL.md`.
