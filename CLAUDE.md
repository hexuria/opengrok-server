# OpenGrok

The server the AI coworkers live on. One Rust service that owns the harness, the tools, the
computers and the policy — shipped together with **open-ai-gateway** as a single AI infrastructure.
Clients (NativeChat, the native desktop app, first; the `/console` web app; Claude Code over
`/mcp`) are windows onto it. They speak AG-UI and the REST routes beside it.

**Picking this up cold? Start with [`docs/HANDOVER.md`](docs/HANDOVER.md)** — the state of play,
what is already decided, and your first task.

**New here? Read in this order:** [`docs/GOAL.md`](docs/GOAL.md) (the mission and the stack)
→ [`docs/DIAGRAMS.md`](docs/DIAGRAMS.md) №1 (five minutes, pictures)
→ [`docs/WHY.md`](docs/WHY.md) (what we built before, and why a working app wasn't enough)
→ [`docs/ROADMAP.md`](docs/ROADMAP.md) (what is done, with commits, and what is left)
→ [`docs/setup/`](docs/setup/README.md) (how to actually stand it up)
→ the reference doc for whatever you are about to touch, in `docs/research/`.

## Three facts that each cost a day if you learn them the hard way

1. **A coworker on a route that only talks cannot act.** The hire default is `xai/grok-4.6`
   (`OG_MODEL`) because it is the route on the record making real tool calls; the old default
   answered shell requests with invented text and zero tool calls, so the demo showed no card and
   no computer. Press **Test** before **Hire**: it asks for one real completion that calls a
   tool. `docs/setup/environment.md` (`OG_MODEL`), `docs/setup/first-run.md`.
2. **The gateway is embeddable — `oag_server::public_router()` returns a wired Axum router.** But if
   you skip `oag_server::serve()` you must spawn the catalogue refresh yourself, or a replica
   serves a **stale catalogue while reporting healthy**. `docs/research/gateway-open-ai-gateway.md` §8.
3. **An empty success is the dangerous reply.** An empty roster or thread is *valid*, so a
   client paints nothing and the person blames the app. Reply shapes matter as much as replies:
   a count must be a number and a list an array, or a client diverts or throws. And
   `OG_PUBLIC_GATEWAY_URL` must be the address clients actually reach: emailed links and the MCP
   door's OAuth issuer are built from it. `docs/setup/nativechat.md`, `docs/setup/environment.md`.

The Grok Bot desktop client, its two doors and the facts that went with them were removed on
20 Sep 2026; they are kept in `docs/archive/seam-a-client-facts.md`.

---

## Non-negotiables

1. **The client contract is transcribed, never invented.** Shapes in `crates/opengrok-wire` exist because
   the desktop client emits or expects them. A tidier field name breaks a client we do not compile.
   Every shape carries a provenance comment naming the file it was read from.
2. **Unknown wire shapes round-trip untouched.** An entry kind we do not recognise is preserved and
   re-emitted, never dropped — dropping one deletes somebody's message from their own history.
3. **No vendored generated protobuf stubs, ever.** Generated stubs are never vendored; wire shapes
   are transcribed with a provenance comment naming their source.
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
11. **A change to the run protocol goes through the model first.** The turn loop, the run
    aggregate, journal writes, parking, Stop, the sweep and leases are modelled in `formal/`;
    `formal/POLICY.md` says when a change must touch the models and what to do with a
    counterexample (fix, keep it as EXPECTED TO FAIL, add a regression test).

---

## Workspace

```
crates/
  opengrok          the binary; wires the server, embeds the gateway, drives the scheduler tick
  opengrok-core     ids, errors, domain types, domain events. No I/O. Everything depends on it; it depends on nothing.
  opengrok-wire     the client contract: commands, transcript entries, activity, AG-UI events
  opengrok-harness  the agent loop: turns, tool calls, streaming, durability. Auto-review's model judge lives here; goal/plan/review as composer commands do not — no client sends a mode yet, and honouring one would invent a contract (ROADMAP "Commands")
  opengrok-box      the coworker's computer — a trait; typed box.ascii.dev v1 client + local Docker
  opengrok-plugins  Agent Plugins: the bundle of skills + MCP servers a coworker is given
  opengrok-recipes  a taught tape into the box's recipe steps; the lint that keeps them runnable
  opengrok-tools    tool definitions and the executor; MCP client (rmcp) for plugins: mem0, cua, skills
  opengrok-policy   what a principal may make a coworker do
  opengrok-store    Postgres: append-only event store + projections (CQRS reads), runs, scheduler rows
  opengrok-testdb   test support: the `_gate` database guard, and each test binary's own database
  opengrok-server   Axum: the host-facing API, the AG-UI endpoint, the MCP door, /console
```

Mirrors open-ai-gateway's crate-per-concern layout on purpose — the two ship together and a reader
who knows one should navigate the other. Axum 0.8, sqlx 0.9, Rust 2024, matching the gateway.

## Where things are

| What | Where |
|---|---|
| The client we serve | `hexuria/nativechat` — reference: `docs/research/client-nativechat.md` (the removed Grok Bot client: `docs/research/client-grok-bot.md`) |
| The model door | `/Volumes/goldcoders/OSS/open-ai-gateway` — reference: `docs/research/gateway-open-ai-gateway.md` |
| The prior product's lessons | `/Volumes/goldcoders/projects/opensesame/opensesame` — reference: `docs/research/lessons-opensesame.md` |
| The coworker's computer | `docs/research/sandbox-box-ascii-dev.md` (our notes); vendor API pages in `docs/box/` |
| Connectors | `docs/research/connectors-open-connector.md` |
| Picture-explainers | `docs/DIAGRAMS.md` (sources vendored in `docs/artifacts/`) |

## Commands

```sh
cargo check --workspace          # must stay clean — this is the DEFAULT build, the one that ships
cargo check -p opengrok --no-default-features  # one reqwest, one hyper; `jev` is typesafe-sdk
cargo clippy --workspace --all-targets
cargo test --workspace          # many tests need Postgres; they skip loudly without it
scripts/serve.sh                 # build + (re)start the dev server from .env
scripts/gate.sh --smoke          # the merge gate; CI runs the same script; docs/setup/gate.md
scripts/crate-size.sh            # fail if any crate's src/ is over its recorded ceiling
scripts/check-architecture.sh    # fail on a crate edge scripts/architecture.txt does not allow
scripts/formal.sh                # TLC + Lean on formal/ (install: scripts/install-tla.sh, install-lean.sh)
scripts/install-ci-tools.sh      # pinned cargo-deny + cargo-nextest; gate.sh uses them when present
```

**`OG_MODEL_DOOR=mock-cards` REFUSES TO START.** It served the mock transcript catalogue, which
was deleted on 20 Sep 2026 with the desktop client's door it rendered into. It fails closed and
names the doors that do exist rather than quietly falling back to a real, billed one. `mock` and
`mock-tools` are what the smokes drive.

## Writing style in this repo

Comments explain **constraints**, not narration — why a shape is the way it is, what broke when it
was otherwise, what a future reader must not "simplify". Never what the next line does. Commit
subjects are lowercase sentences; the body explains why. Both conventions are inherited from the
prior product and are worth keeping: several of its commit bodies are the only record of a bug that
cost a day.
