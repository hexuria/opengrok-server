# OpenGrok — build plan

**The one-line idea.** The coworkers move out of the browser tab and into a server of their own: one
Rust service that owns the harness, the tools, the computers and the policy, shipped together with
open-ai-gateway as a single AI infrastructure. Clients — the Grok Bot desktop app first, then web
and CLI — become windows onto it.

**The picture version:** [The Coworkers Move Out](https://claude.ai/code/artifact/1c526721-d19b-406c-b4f9-feef43a507dd)
(five minutes, vendored at `docs/artifacts/coworkers-move-out.html`). Full index in
[`DIAGRAMS.md`](DIAGRAMS.md).

---

## 1. Why — the bug that proved it

The previous product (OpenSesame, TypeScript) let a person broadcast to every coworker in a room
with `@All`. The walk went coworker to coworker **inside the page**: send, await the reply, rebind,
send the next. A browser refresh killed it after the first reply — four coworkers never heard the
question.

That is not a bug to patch. Delivery, the queue and the waiting had the **wrong owner**. Anything
that dies with a tab cannot be the thing that runs the work.

Three consequences follow from moving it, and none of them are features bolted on afterwards:

- refresh, tab close, lid shut, browser crash — the work continues;
- a broadcast becomes **parallel**, because the constraint that forced sequence (one live agent
  binding per page) is gone;
- the same coworker is visible from every client at once, doing the same work.

**The fuller story — what we built, why a working app wasn't enough, and what "remote first" means
— is [`WHY.md`](WHY.md).** Read it before the plan if you weren't there. The mechanism of this
particular bug: [`research/lessons-opensesame.md`](research/lessons-opensesame.md) §4.

---

## 2. The client

`/Volumes/goldcoders/OSS/grok-bot` is a reconstruction of a shipped Electron app whose brain lived on
someone else's servers. Three layers: **renderer** (the UI), **host** (Electron main, editable
TypeScript), and the **box** (the agent's computer).

**There are two network seams, and only one of them is ours:**

| Seam | What it is | Ours? |
|---|---|---|
| **The Sand gateway** | `POST /api/<cmd>` JSON + SSE `GET /events` + `/health` + `/avatars/<id>`, defined by `SAND_GATEWAY_COMMANDS` (`source/host/gateway-protocol.ts:4-128`) — **123 commands** (`:5-127`), 90 of them reachable from the renderer (`coordinator.ts:92-183`); the other 33 are host-only | **yes — this is OpenGrok** |
| The Cursor ConnectRPC backend | `api2.cursor.sh` | **no** — neutralised via `SAND_BACKEND_URL` and the repo's existing `source/mock/` server |

**We implement the Sand gateway.** The client keeps its UI, its transcript format and its activity
model; OpenGrok answers where the old backend did. Note the surface is wider than "JSON commands":
an SSE event stream with 18 channels and an avatar endpoint are part of booting.

**Minimal boot set** (verified): `/health`, `/events`, `listAgents`, `countAgents`, `getTrays`,
`isAgentNetworkEnabled`, `get/setHostSettings`. Then `sendPrompt` for the first answer.

> ### ⚠ The repoint trap — read before P1
> `SAND_HOST_GATEWAY_URL` (`source/electron-main/box/box-host-connector.ts:17,156-161`) is the whole
> repoint. But `createSettingsRoutedHostConnector`
> (`local-docker-host-connector.ts:437-443`) **throws when the resolved gateway host starts with
> `127.0.0.1` or `localhost`** — unless `boxRuntime === "local-docker"`, which ignores the env var
> and spawns its own host on port 1350. **OpenGrok must therefore serve on a non-loopback hostname**
> (a LAN address, or a hosts-file alias) or the app refuses to connect with no useful error.

Reference: [`research/client-grok-bot.md`](research/client-grok-bot.md) — the full 123-command
inventory with per-command shapes, transcript kinds, the 12 card types, activity events, the 18 SSE
channels, the 30-field roster row, the box seam, a 23-step first-boot checklist and 12 traps.

**The rights line is not optional and is written down separately:** [`LEGAL.md`](LEGAL.md). Short
version — we implement a *client-facing contract for interoperability*, we never vendor the
generated protobuf stubs, and the repo stays private until a rights review clears it. Nothing in
this plan depends on publishing.

---

## 3. Shape

```
crates/
  opengrok    the binary; wires the server and embeds the gateway
  og-core     ids, errors, domain types. No I/O.
  og-wire     the client contract: commands, transcript entries, activity
  og-harness  the agent loop (Rig): turns, tool calls, streaming
  og-box      the coworker's computer (trait; box.ascii.dev first)
  og-tools    tool definitions and the executor
  og-policy   what a principal may make a coworker do
  og-store    Postgres: coworkers, transcripts, runs, the fan-out ledger
  og-server   Axum: host-facing API and the event stream
```

Mirrors open-ai-gateway's crate-per-concern layout deliberately: the two ship together and a reader
who knows one should navigate the other. Axum 0.8, sqlx 0.9, Rust 2024 — the gateway's own versions,
so a shared workspace stays possible.

### One binary, two doors

The gateway serves an inference listener and an admin listener from `oag-server`, with `oag-core`
holding pure domain types and no I/O. OpenGrok takes the same shape, and the release ships both: the
gateway is the model door **in OpenGrok's own wall**, not a service deployed beside it.

**This is verified, not hoped for.** `oag_server::public_router(Arc<AppState>) -> axum::Router`
returns a fully-wired, state-erased router, so OpenGrok can `merge` or `nest` the entire inference
surface into its own Axum app — no fork, no vendored handlers. Three genuine obstacles the merged
binary must own:

1. **process-global singletons** — the Prometheus recorder (`metrics::install()`) and the tracing
   subscriber can only be installed once; OpenGrok installs them;
2. **`settings::load`'s `OAG_<SECTION>__<FIELD>` loader is private to the `oag` binary crate** — we
   either build `AppState` ourselves or upstream a public loader;
3. **skipping `oag_server::serve()` means spawning the catalogue refresh ourselves** — without it a
   replica silently serves a stale catalogue *while looking perfectly healthy*, which is the worst
   failure mode available and belongs in a startup check;
4. **Redis** — the gateway's key resolution uses a moka → Redis → Postgres chain, so a merged binary
   inherits Redis as a dependency (or a decision to run without it, which the reference ranks as a
   medium-severity change rather than a flag flip).

Full detail: [`research/gateway-open-ai-gateway.md`](research/gateway-open-ai-gateway.md) §8.

---

## 4. The five seams

### 4.1 The wire — `og-wire`

Transcribed, not designed. Three wires kept apart on purpose:

| Wire | What it is | Durable? |
|---|---|---|
| commands | the host's JSON request/response calls | n/a |
| transcript | the entries a conversation is made of | **yes** |
| activity | thinking/tool deltas while a coworker works | no |

Activity is disposable by design: a client that missed an update must be able to catch up from the
transcript alone. Unknown entry kinds round-trip untouched.

### 4.2 The harness — `og-harness`

**Rig** for provider abstraction; **our own loop** for durability. No Rust framework offers a loop
that survives the process — the 2026 ecosystem surveys say checkpointing and crash recovery are
"still on the user to implement" — and that suspension *is* the product. The loop is ~400 lines
either way; what a framework really sells is integrations, and those are the thinnest part of the
Rust ecosystem. (That tension is why `og-tools` is its own seam.)

Every model call exits through open-ai-gateway, so a coworker's pin means exactly what it means
today.

### 4.3 The computer — `og-box`

A trait, so a coworker's computer is a **seam and not a vendor**. box.ascii.dev is the first
implementation: persistent Ubuntu VMs, plain REST, bearer auth — a `reqwest` client is the whole
integration.

Verified: create, sync and detached exec, file read/write, port exposure with preview URLs,
stop/resume/fork, destroy. **Gaps designed around rather than discovered later:**

- **no live stdout socket** — detached + poll is the honest shape, which is why `run` and `watch`
  are separate trait methods rather than a `Stream` that hides the latency;
- no directory-list endpoint (shell `ls`);
- computer-use is a VNC URL, not click/screenshot primitives;
- hosted-only, EU-only regions, plan-gated concurrency;
- two response shapes still unpinned (the created-box id field; the delete confirmation header) —
  a P3 task against a real box.

Full report: [`research/sandbox-box-ascii-dev.md`](research/sandbox-box-ascii-dev.md).

### 4.4 Tools — `og-tools`

Native tools (shell, files, ports) execute against the box. Connectors come from **open-connector**
— Apache-2.0, ~1,450 providers / ~15,000 actions.

An earlier audit claimed its OAuth vaulting was paywalled. **That is false**: the OAuth flow,
connection tables and encryption path are all in the open repo; the hosted tier sells pre-registered
OAuth apps and managed infrastructure. Two real caveats: it is single-tenant-per-instance (no tenant
columns), and each action's method and path live inside TypeScript executors rather than as flat
data, so "one generic Rust executor over pure JSON" is not free.

**Decision:** run their Node runtime as a **sidecar behind our tool trait** for the first connectors,
keeping the option to extract definitions to JSON later, provider by provider. Our policy layer —
never theirs — decides who may call what.

Full report: [`research/connectors-open-connector.md`](research/connectors-open-connector.md).

### 4.5 Policy — `og-policy`

Configure from the client; the server accepts or denies. Five separate questions, each enforced
somewhere different — collapsing any two is where the leaks live:

| # | Question | Answered by | Enforced |
|---|---|---|---|
| 1 | May this principal talk to this coworker? | a grant row | every turn, not once at start |
| 2 | What may this coworker ever do? | its tool set — the ceiling | at definition |
| 3 | What may this principal make it do? | a capability profile | before every tool call |
| 4 | Whose records may this call touch? | run context + bind + RLS | inside the tool, and again in the DB |
| 5 | Which calls need a human yes? | an `approve` effect | an approval queue that suspends the run |

Layers 2 and 3 combine by **intersection, never union** — which is what makes coworker-to-coworker
delegation safe later, because delegation can then only narrow.

The trap this exists for: the dangerous message is not a jailbreak, it is *"what's the status of
order 8891?"* — a reasonable sentence about somebody else's order. The fix is not to check the
argument but to **overwrite** it from the session before the tool runs.

---

## 5. Phases

Each is shippable on its own and ends in something demonstrable. The order is chosen so the earliest
phases exercise the seams the later ones depend on.

| # | Phase | Est. | Done when |
|---|---|---|---|
| **P0** | **Foundations** ✅ | — | nine crates, workspace lints, `cargo check` clean |
| **P1** | The client says hello | ~1 wk | the desktop app lists coworkers that exist only in our Postgres |
| **P2** | A turn | 1–2 wk | a coworker answers in the app, and the answer survives a server restart mid-run |
| **P3** | The computer | ~1 wk | a coworker runs `ls`, writes a file, exposes a port whose preview URL opens |
| **P4** | Broadcast, done right | ~1 wk | the walk completes with the client closed the whole time |
| **P5** | Policy and approvals | ~2 wk | a refusal reaches the model as a result; an approval from another device resumes the run exactly once |
| **P6** | Connectors | 2–3 wk | two people in one room see their own accounts' data |

**P0 — Foundations** (done). Nine crates; workspace lints (`unsafe_code` forbidden,
`unwrap`/`expect`/`panic` denied); ids as newtypes; `og-wire` and `og-box` sketched with their
constraints written into the types. Remaining: CI, `.sqlx` offline data, the gateway wired in as a
dependency.

**P1 — The client says hello.** Not four commands: the client's boot is a *sequence*, and three of
its steps throw or divert the app if their reply has the wrong shape. `GET /health` (within 1500 ms)
and `GET /events` (SSE, `retry: 1000`, a `:ping` inside 15 s) come before any command at all — miss
either and `listAgents` is never called. Then `listAgents` (array), the resync chain's
`setHostSettings`/`getHostSettings`, `countAgents` (**a number**, or the app shows onboarding),
`getTrays` (**an array**, or the renderer throws), `isAgentNetworkEnabled`,
`isGlobalSearchEnabled`, `getForeverBoxStatus` (`null` or a record, or it throws), and
`openAgentTail`. `og-store` gets its first migrations alongside.

**The full ordered table, the four environment variables, the Postgres setup and the acceptance
script are in [`RUNBOOK.md`](RUNBOOK.md) — P1 is not startable without it.** The same list is
mirrored in `crates/og-wire/src/command.rs` as `P1_COMMANDS`, so the plan and the code cannot drift.

Watch for Trap 2: an *empty success* is the dangerous reply. `listAgents` returning `[]` is valid,
paints an empty sidebar, and reads as a broken app rather than a protocol mistake — seed one
coworker.

**P2 — A turn.** `sendPrompt` → `og-harness` runs a turn through Rig → OAG → a real model; text
streams back as activity; the transcript persists. The durability test is the point: kill the server
mid-run and the run resumes.

**P3 — The computer.** `og-box` against a real box.ascii.dev VM; shell and file tools wired through
`og-tools`; the two unpinned response shapes written down.

**P4 — Broadcast, done right.** The fan-out ledger: one request, a durable row, every coworker asked
**in parallel** (with a small concurrency cap for the subscription's sake), answers landing as each
finishes. The bug that started this project, fixed by construction rather than by retry logic.

**P5 — Policy and approvals.** The gate on every tool call; audit written in the same transaction as
the decision; approvals that suspend a run and resume days later, exactly once under retry.

**P6 — Connectors.** open-connector behind the tool trait; per-principal connections; OpenGrok
itself as an MCP server.

---

## 6. What carries over from OpenSesame

Proven designs, not wasted work — and its web app is a candidate second client. Full detail in
[`research/lessons-opensesame.md`](research/lessons-opensesame.md):

- **one thread per (person, channel, coworker)** — the hosted platform binds a thread to one agent
  permanently and refuses every other speaker; this is why "who answers" is a real control;
- **the woven room timeline** — stored messages carry no timestamps, so the room keeps its own
  append-only index;
- **the fan-out collapse** — one ask drawn once, however many coworkers it went to;
- **per-coworker model pins** through the gateway, and the plane separation behind them: identity ≠
  runtime ≠ harness ≠ model route ≠ credential;
- **speaker and author attribution** in a multi-party transcript;
- and every gateway bug that hunt turned up (reasoning-stream openers, unique response item ids, an
  honest model catalogue) — all fixed upstream and verified against real traffic.

**The most load-bearing lesson:** that product could not boot without a proprietary hosted service
providing threads, memory and run locks. OpenGrok must own those itself. That is `og-store`'s whole
reason to exist.

---

## 7. Open questions for the operator

These change what gets built and are not for an agent to decide alone:

**One is genuinely open. Two are decided and recorded here only so the operator can overturn them
— they are not blocking anything.**

1. 🔴 **OPEN — single-tenant (one workspace) or multi-tenant?** This is the only question that
   blocks work, and it blocks *P1*, not P5: the first migrations' shape depends on it, and
   retrofitting tenancy is the expensive mistake the OpenSesame spec named.
   **P1 proceeds on this default unless you say otherwise:** every scoped table carries a
   `workspace_id NOT NULL` referencing a single seeded workspace row. That costs nothing now, keeps
   the multi-tenant door open, and is *not* the tenancy decision — enforcement (RLS, the fence type,
   whether `WorkspaceId` gets a private constructor) is still P5's, and still yours.
2. ✅ *Decided — local Docker `Computer` lands **after** P3.* box.ascii.dev is hosted-only and
   EU-only; the trait exists so a Docker implementation is additive. Overturn this if EU-only
   latency or data residency bites sooner than expected.
3. ✅ *Decided — open-connector runs as a **Node sidecar** in v1* (§4.4), behind our tool trait,
   with per-provider extraction to a Rust executor as the follow-up. Overturn this if running a
   Node service alongside is operationally unacceptable.
