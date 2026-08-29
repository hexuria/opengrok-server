# OpenGrok

The server the AI coworkers live on.

One Rust service that owns the agent harness, the tools, the computers and the policy — shipped
together with [open-ai-gateway](https://github.com/hexuria/open-ai-gateway) as a single AI
infrastructure. Clients are windows onto it.

**The client is [openbot](https://github.com/hexuria/openbot)** — MIT, public, AG-UI-native, with a
computer per agent and a policy gateway of its own. It accepts any AG-UI endpoint as a "Bot", and
OpenGrok is that endpoint: openbot governs and renders, OpenGrok owns the agents, the boxes, the
harness and the models.

```
openbot  ──AG-UI──▶  OpenGrok  ──▶  open-ai-gateway  ──▶  models
                        └──▶  box.ascii.dev (a computer per agent)
```

A Grok Bot compatibility mode is a second, optional client — see [`docs/GOAL.md`](docs/GOAL.md).

A coworker keeps working when you close the tab, because the work was never in the tab.

---

## Start here

| | |
|---|---|
| **Picking this up?** | [`docs/HANDOVER.md`](docs/HANDOVER.md) — state of play, decisions made, first task |
| **The idea, in pictures** | [`docs/DIAGRAMS.md`](docs/DIAGRAMS.md) — five minutes |
| **Why this exists** | [`docs/WHY.md`](docs/WHY.md) — what we built before, what it couldn't do, and why that forced a rebuild |
| **The plan** | [`docs/PLAN.md`](docs/PLAN.md) — seams, phases, open questions |
| **How to run it** | [`docs/RUNBOOK.md`](docs/RUNBOOK.md) — standing P1 up, and the acceptance script that proves it |
| **The goal** | [`docs/GOAL.md`](docs/GOAL.md) — the mission, the stack, the slice order |
| **The client** | [`hexuria/openbot`](https://github.com/hexuria/openbot) — AG-UI, MIT, ours to change |
| **The invariants** | [`CLAUDE.md`](CLAUDE.md) — ten rules that are not up for negotiation |
| **The rights line** | [`docs/LEGAL.md`](docs/LEGAL.md) — read before touching the client contract |
| **Reference docs** | [`docs/research/`](docs/research/) — the client, the gateway, the sandbox, connectors, and what the previous product taught us |

## Status

**Slice 1 done: sign-in works.** Accounts are event-sourced in Postgres, tokens are our own JWTs,
and `scripts/slice1-auth-smoke.sh` proves it end to end. Next is the AG-UI endpoint, which is what
openbot connects to. [`docs/GOAL.md`](docs/GOAL.md) has the slice order.

```sh
cargo check --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Layout

```
crates/
  opengrok          the binary; wires the server, embeds the gateway, drives the scheduler tick
  opengrok-core     ids, errors, domain types, domain events. No I/O. Everything depends on it; it depends on nothing.
  opengrok-wire     the client contract: commands, transcript entries, activity, AG-UI events
  opengrok-proto    seam B transcribed: Connect-over-HTTP/1.1 messages (prost). Read its lib.rs before touching it.
  opengrok-harness  the agent loop (Rig): turns, tool calls, streaming, durability; goal/plan/review behaviours
  opengrok-box      the coworker's computer — a trait; box.ascii.dev first, cua and Docker later
  opengrok-tools    tool definitions and the executor; MCP client (rmcp) for plugins: mem0, cua, skills
  opengrok-policy   what a principal may make a coworker do
  opengrok-store    Postgres: append-only event store + projections (CQRS reads), runs, scheduler rows
  opengrok-server   Axum: the host-facing API, the SSE event stream, the AG-UI endpoint
docs/
  PLAN.md · LEGAL.md · DIAGRAMS.md · research/ · artifacts/
```

## A note on privacy

This repository stays **private** until an independent rights review clears it. See
[`docs/LEGAL.md`](docs/LEGAL.md) — the reasoning is short, specific, and load-bearing.

## Licence

MIT for the code authored here. Third-party material is not relicensed by that grant; see
`docs/LEGAL.md`.
