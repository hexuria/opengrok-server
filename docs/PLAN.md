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

Full post-mortem: [`research/lessons-opensesame.md`](research/lessons-opensesame.md) §4.

---

## 2. The client

`/Volumes/goldcoders/OSS/grok-bot` is a reconstruction of a shipped Electron app whose brain lived on
someone else's servers. Three layers: **renderer** (the UI), **host** (Electron main, editable
TypeScript), and the **box** (the agent's computer). The host reaches its backend through one JSON
command surface — `SAND_GATEWAY_COMMANDS` in `source/host/gateway-protocol.ts`, roughly seventy
named commands — and renders a durable transcript of typed entries plus a live activity stream.

**We implement that command surface.** The client keeps its UI, its transcript format and its
activity model; OpenGrok answers where the old backend did.

Reference: [`research/client-grok-bot.md`](research/client-grok-bot.md) — the command inventory,
transcript kinds, activity events, the box seam, and the first-boot call order.

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
gateway is the model door **in OpenGrok's own wall**, not a service deployed beside it. Feasibility
and obstacles: [`research/gateway-open-ai-gateway.md`](research/gateway-open-ai-gateway.md) §8.

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

**P1 — The client says hello.** `og-server` answers `listAgents`, `createAgent`,
`getAgentTranscriptTail`, `openAgentTail`; `og-store` gets its first migrations. Watch for the trap
in `research/client-grok-bot.md` — a command answering empty success renders as "you have no
coworkers", which looks like a data problem and is a protocol one.

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

1. **box.ascii.dev is hosted-only and EU-only.** Fine for v1, or does a local Docker `Computer`
   implementation need to land inside P3 rather than after it?
2. **open-connector as a Node sidecar in v1** — acceptable operationally, or hold connectors until a
   Rust executor exists?
3. **Single-tenant (one workspace) or multi-tenant from the start?** P5's shape depends on it, and
   retrofitting tenancy is the expensive mistake the OpenSesame spec called out by name.
