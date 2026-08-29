# OpenGrok

The server the AI coworkers live on. One Rust service that owns the harness, the tools, the
computers and the policy — shipped together with **open-ai-gateway** as a single AI infrastructure.
Clients (the Grok Bot desktop app first, then web and CLI) are windows onto it.

**New here? Read in this order:** [`docs/DIAGRAMS.md`](docs/DIAGRAMS.md) №1 (five minutes, pictures)
→ [`docs/PLAN.md`](docs/PLAN.md) → [`docs/LEGAL.md`](docs/LEGAL.md) → the reference doc for whatever
you are about to touch, in `docs/research/`.

---

## Non-negotiables

1. **The client contract is transcribed, never invented.** Shapes in `crates/og-wire` exist because
   the desktop client emits or expects them. A tidier field name breaks a client we do not compile.
   Every shape carries a provenance comment naming the file it was read from.
2. **Unknown wire shapes round-trip untouched.** An entry kind we do not recognise is preserved and
   re-emitted, never dropped — dropping one deletes somebody's message from their own history.
3. **No vendored generated protobuf stubs, ever.** See [`docs/LEGAL.md`](docs/LEGAL.md). This repo
   stays private until a rights review clears it.
4. **Every model call exits through open-ai-gateway.** A coworker's pin (`xai/grok-4.6@sub`) is a
   route, not a key. Provider credentials never touch a coworker's row, a client payload, or a log.
5. **Nothing that matters lives in a client.** If losing a tab, a process or a machine loses work,
   the design is wrong. Queues are rows; runs resume; delivery is the server's job. *This is the bug
   that created this project — see `docs/research/lessons-opensesame.md` §4.*
6. **The client configures; the server decides.** A client's word is a request. Policy is enforced
   on every action, every time — not once at the start of a session.
7. **Identity arguments are overwritten, not validated.** Before a tool runs, the session's identity
   replaces the argument. The model never gets a say in whose data it fetches.
8. **Fail closed and say why.** A refusal reaches the model as a *result* it can reason about, not
   an exception that kills the run. A broken condition on a deny rule counts as a match; on an allow
   rule it does not. A typo may only ever narrow access.
9. **The compiler is the reviewer that never gets bored.** `unsafe_code` is forbidden;
   `unwrap`/`expect`/`panic` are denied workspace-wide. Ids are newtypes. Keep it that way.
10. **Evidence or it doesn't ship.** "200 accepted" is not "honoured". Claims about a provider's
    behaviour need a captured response; claims about the client's behaviour need a file path.

---

## Workspace

```
crates/
  opengrok    the binary; wires the server, embeds the gateway
  og-core     ids, errors, domain types. No I/O. Everything depends on it; it depends on nothing.
  og-wire     the client contract: commands, transcript entries, activity
  og-harness  the agent loop (Rig): turns, tool calls, streaming, durability
  og-box      the coworker's computer — a trait; box.ascii.dev first, Docker later
  og-tools    tool definitions and the executor
  og-policy   what a principal may make a coworker do
  og-store    Postgres: coworkers, transcripts, runs, the fan-out ledger
  og-server   Axum: the host-facing API and the event stream
```

Mirrors open-ai-gateway's crate-per-concern layout on purpose — the two ship together and a reader
who knows one should navigate the other. Axum 0.8, sqlx 0.9, Rust 2024, matching the gateway.

## Where things are

| What | Where |
|---|---|
| The client we serve | `/Volumes/goldcoders/OSS/grok-bot` — reference: `docs/research/client-grok-bot.md` |
| The model door | `/Volumes/goldcoders/OSS/open-ai-gateway` — reference: `docs/research/gateway-open-ai-gateway.md` |
| The prior product's lessons | `/Volumes/goldcoders/projects/opensesame/opensesame` — reference: `docs/research/lessons-opensesame.md` |
| The coworker's computer | `docs/research/sandbox-box-ascii-dev.md` |
| Connectors | `docs/research/connectors-open-connector.md` |
| Picture-explainers | `docs/DIAGRAMS.md` (sources vendored in `docs/artifacts/`) |

## Commands

```sh
cargo check --workspace          # must stay clean
cargo clippy --workspace --all-targets
cargo test --workspace
```

## Writing style in this repo

Comments explain **constraints**, not narration — why a shape is the way it is, what broke when it
was otherwise, what a future reader must not "simplify". Never what the next line does. Commit
subjects are lowercase sentences; the body explains why. Both conventions are inherited from the
prior product and are worth keeping: several of its commit bodies are the only record of a bug that
cost a day.
