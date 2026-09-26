# OpenGrok

The server the AI coworkers live on.

One Rust service that owns the agent harness, the tools, the computers and the policy — shipped
together with [open-ai-gateway](https://github.com/hexuria/open-ai-gateway) as a single AI
infrastructure. Clients are windows onto it: NativeChat (`hexuria/nativechat`, a native desktop
app in Rust + GPUI) and any other AG-UI client (openbot among them) through `POST /ag-ui` and the
REST routes beside it, and a browser through the web console at `/console`.

```
NativeChat / AG-UI / console  ──▶  OpenGrok  ──▶  open-ai-gateway  ──▶  models
                                      └──▶  a computer per coworker (Docker / box.ascii.dev)
```

A coworker keeps working when you close the tab, because the work was never in the tab.

## Status

Slices 1–14 are done and the server is real: auth, the AG-UI endpoint, the durable harness,
computers, connectors, the scheduler/monitor autonomy pair, the MCP door, orgs and invites, the
web console, and the consent model with model-judged auto-review.
**[`docs/ROADMAP.md`](docs/ROADMAP.md) is the tracker** — a box is ticked only in the commit that
makes it true, and its unticked boxes are the remaining work.

The two doors built for the discontinued Grok Bot desktop client — seam A (`POST /api/{method}`
and the `/events` stream) and seam B (ConnectRPC + its gRPC mirror) — were **deleted on
20 Sep 2026**. What a client talks to now is AG-UI, the REST routes beside it, `/mcp` and
`/health`. `docs/research/client-grok-bot.md` and `docs/setup/desktop-client.md` are kept as the
record of what was there.

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

# 4. the gate (local and CI both run scripts/gate.sh --smoke)
cargo build -p opengrok && OG_PORT=1449 \
  OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate scripts/gate.sh --smoke
```

The full chain, one file per topic: **[`docs/setup/`](docs/setup/README.md)** —
postgres → environment → running → first run (the first admin, a gateway key, a real turn) →
gate → TLS → NativeChat. The routes NativeChat and the console call are mapped in
[`docs/research/client-nativechat.md`](docs/research/client-nativechat.md).

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
| **Reference docs** | [`docs/research/`](docs/research/README.md) — the client, the gateway, the sandbox, connectors, the prior product |

## Layout

```
crates/
  opengrok          the binary; wires the server, embeds the gateway, drives the scheduler tick
  opengrok-core     ids, errors, domain types, domain events. No I/O. Everything depends on it; it depends on nothing.
  opengrok-wire     the client contract: commands, transcript entries, activity, AG-UI events
  opengrok-harness  the agent loop: turns, tool calls, streaming, durability; the auto-review judge
  opengrok-box      the coworker's computer — a trait; local Docker and box.ascii.dev (typed v1 client) today
  opengrok-tools    tool definitions and the executor; MCP client (rmcp) for plugins
  opengrok-policy   what a principal may make a coworker do
  opengrok-store    Postgres: append-only event store + projections (CQRS reads), runs, scheduler rows
  opengrok-server   Axum: the host-facing API, the AG-UI endpoint, the MCP door, /console
docs/
  setup/ · research/ · box/ (vendor API pages) · verification/ · archive/ · the documents in the table above
scripts/
  serve.sh (run the dev server) · gate.sh (the merge gate) · crate-size.sh (the crate-size ceiling) · slice*-smoke.sh (the evidence)
web/
  the web console (Bun/Vite/React SPA served at /console)
```

## A note on rights

The operator made this repository **public on 1 Sep 2026 with the rights review still
outstanding**.

## Licence

MIT for the code authored here. Third-party material is not relicensed by that grant.
