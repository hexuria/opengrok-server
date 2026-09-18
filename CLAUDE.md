# OpenGrok

The server the AI coworkers live on. One Rust service that owns the harness, the tools, the
computers and the policy — shipped together with **open-ai-gateway** as a single AI infrastructure.
Clients (AG-UI clients, the web console, MCP clients, a CLI later) are windows onto it. The two
doors that booted the reconstructed desktop app were deleted on 18 Sep 2026; `docs/LEGAL.md` says
what went and what stays.

**Picking this up cold? Start with [`docs/HANDOVER.md`](docs/HANDOVER.md)** — the state of play,
what is already decided, and your first task.

**New here? Read in this order:** [`docs/GOAL.md`](docs/GOAL.md) (the mission and the stack)
→ [`docs/DIAGRAMS.md`](docs/DIAGRAMS.md) №1 (five minutes, pictures)
→ [`docs/WHY.md`](docs/WHY.md) (what we built before, and why a working app wasn't enough)
→ [`docs/ROADMAP.md`](docs/ROADMAP.md) (what is done, with commits, and what is left)
→ [`docs/setup/`](docs/setup/README.md) (how to actually stand it up)
→ [`docs/LEGAL.md`](docs/LEGAL.md) → the reference doc for whatever you are about to touch,
in `docs/research/`.

## Three facts that each cost a day if you learn them the hard way

1. **`OG_MODEL_DOOR=mock` is the whole stack with no provider, no key and no spend.** CI and
   every smoke run on it. `mock-tools` asks for one shell call per turn, which is how the consent
   cards are driven deterministically. Anything else exits through open-ai-gateway and needs
   `OG_GATEWAY_TOKEN`. `mock-cards` was removed with the desktop client and refuses to boot rather
   than falling through to a billed door. `crates/opengrok/src/main.rs`.
2. **The gate is `scripts/gate.sh --smoke`, and CI runs that same script.** It needs Postgres and
   `OG_DATABASE_URL`; without them the Postgres-backed tests skip loudly and the smokes cannot
   run, so a green local `cargo test` is not a green gate. The gate refuses to share a database
   with a running server, because the autonomy sweeps would race it. `docs/setup/gate.md`.
3. **An empty success is the dangerous reply.** `GET /coworkers` returning `[]` is *valid*, and
   an AG-UI client paints an empty roster and the person blames the client. A KEK, credential or
   database failure must be a non-2xx with a sentence, never an empty list; the MCP door's empty
   toolbox rule (`mcp_door.rs`) is the same fact. Reply shapes matter as much as replies.

---

## Non-negotiables

1. **The client contract is transcribed, never invented.** Shapes in `crates/opengrok-wire` exist because
   the desktop client emits or expects them. A tidier field name breaks a client we do not compile.
   Every shape carries a provenance comment naming the file it was read from.
2. **Unknown wire shapes round-trip untouched.** An entry kind we do not recognise is preserved and
   re-emitted, never dropped — dropping one deletes somebody's message from their own history.
3. **No vendored generated protobuf stubs, ever.** See [`docs/LEGAL.md`](docs/LEGAL.md). The repo
   was made public by the operator on 1 Sep 2026 with the rights review still outstanding — which
   makes this rule harder, not softer.
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
  opengrok          the binary; wires the server, embeds the gateway, drives the scheduler tick
  opengrok-core     ids, errors, domain types, domain events. No I/O. Everything depends on it; it depends on nothing.
  opengrok-wire     transcribed shapes with provenance: transcript entries, AG-UI events; the harness, store and AG-UI door read them
  opengrok-harness  the agent loop: turns, tool calls, streaming, durability. Auto-review's model judge lives here; goal/plan/review as composer commands do not — the packaged app does not send a mode on sendPrompt (`docs/verification/plan-mode-wire/`)
  opengrok-box      the coworker's computer — a trait; typed box.ascii.dev v1 client + local Docker
  opengrok-tools    tool definitions and the executor; MCP client (rmcp) for plugins: mem0, cua, skills
  opengrok-policy   what a principal may make a coworker do
  opengrok-store    Postgres: append-only event store + projections (CQRS reads), runs, scheduler rows
  opengrok-server   Axum: the host API (/coworkers, /account, /admin), the AG-UI endpoint (SSE), the MCP door, /console
```

Mirrors open-ai-gateway's crate-per-concern layout on purpose — the two ship together and a reader
who knows one should navigate the other. Axum 0.8, sqlx 0.9, Rust 2024, matching the gateway.

## Where things are

| What | Where |
|---|---|
| The desktop client we used to serve | `/Volumes/goldcoders/OSS/opengrok` — reference: `docs/research/client-grok-bot.md` (history since 18 Sep 2026) |
| The model door | `/Volumes/goldcoders/OSS/open-ai-gateway` — reference: `docs/research/gateway-open-ai-gateway.md` |
| The prior product's lessons | `/Volumes/goldcoders/projects/opensesame/opensesame` — reference: `docs/research/lessons-opensesame.md` |
| The coworker's computer | `docs/research/sandbox-box-ascii-dev.md` (our notes); vendor API pages in `docs/box/` |
| Connectors | `docs/research/connectors-open-connector.md` |
| Picture-explainers | `docs/DIAGRAMS.md` (sources vendored in `docs/artifacts/`) |

## Commands

```sh
cargo check --workspace          # must stay clean
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace           # Postgres-backed tests skip loudly without OG_DATABASE_URL
scripts/serve.sh                 # build + (re)start the dev server from .env
scripts/gate.sh --smoke          # the merge gate; CI runs the same script. docs/setup/gate.md
```

**The mock doors need nothing compiled in.** The `mock-fixtures` feature and its catalogue left
with the desktop client's doors on 18 Sep 2026, so the gate builds and tests the default
configuration; the mock doors that remain (`OG_MODEL_DOOR=mock`, `mock-tools`) live in
`opengrok-harness`.

## Writing style in this repo

Comments explain **constraints**, not narration — why a shape is the way it is, what broke when it
was otherwise, what a future reader must not "simplify". Never what the next line does. Commit
subjects are lowercase sentences; the body explains why. Both conventions are inherited from the
prior product and are worth keeping: several of its commit bodies are the only record of a bug that
cost a day.
