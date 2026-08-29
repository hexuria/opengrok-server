# OpenGrok

The server the AI coworkers live on.

One Rust service that owns the agent harness, the tools, the computers and the policy — shipped
together with [open-ai-gateway](https://github.com/hexuria/open-ai-gateway) as a single AI
infrastructure. Clients are windows onto it: the Grok Bot desktop app first, then web and CLI.

A coworker keeps working when you close the tab, because the work was never in the tab.

---

## Start here

| | |
|---|---|
| **The idea, in pictures** | [`docs/DIAGRAMS.md`](docs/DIAGRAMS.md) — five minutes |
| **Why this exists** | [`docs/WHY.md`](docs/WHY.md) — what we built before, what it couldn't do, and why that forced a rebuild |
| **The plan** | [`docs/PLAN.md`](docs/PLAN.md) — seams, phases, open questions |
| **How to run it** | [`docs/RUNBOOK.md`](docs/RUNBOOK.md) — standing P1 up, and the acceptance script that proves it |
| **The invariants** | [`CLAUDE.md`](CLAUDE.md) — ten rules that are not up for negotiation |
| **The rights line** | [`docs/LEGAL.md`](docs/LEGAL.md) — read before touching the client contract |
| **Reference docs** | [`docs/research/`](docs/research/) — the client, the gateway, the sandbox, connectors, and what the previous product taught us |

## Status

**P0 — foundations.** Nine crates, workspace lints, `cargo check` clean, `og-wire`'s casing and
round-trip invariants under test. Nothing serves yet.
See [`docs/PLAN.md`](docs/PLAN.md) §5 for what P1 is, and [`docs/RUNBOOK.md`](docs/RUNBOOK.md) for
how to stand it up.

```sh
cargo check --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Layout

```
crates/
  opengrok    the binary; wires the server, embeds the gateway
  og-core     ids, errors, domain types. No I/O.
  og-wire     the client contract: commands, transcript entries, activity
  og-harness  the agent loop (Rig): turns, tool calls, streaming
  og-box      the coworker's computer — a trait, not a vendor
  og-tools    tool definitions and the executor
  og-policy   what a principal may make a coworker do
  og-store    Postgres: coworkers, transcripts, runs, the fan-out ledger
  og-server   Axum: the host-facing API and the event stream
docs/
  PLAN.md · LEGAL.md · DIAGRAMS.md · research/ · artifacts/
```

## A note on privacy

This repository stays **private** until an independent rights review clears it. See
[`docs/LEGAL.md`](docs/LEGAL.md) — the reasoning is short, specific, and load-bearing.

## Licence

MIT for the code authored here. Third-party material is not relicensed by that grant; see
`docs/LEGAL.md`.
