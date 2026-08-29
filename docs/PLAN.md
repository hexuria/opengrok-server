# OpenGrok — build plan

**The one-line idea.** The coworkers move out of the browser tab and into a server of their own:
one Rust service that owns the harness, the tools, the computers and the policy, shipped together
with open-ai-gateway as a single AI infrastructure. Clients — the Grok Bot desktop app first, then
web and CLI — become windows onto it.

## Why (the bug that proved it)

An `@All` broadcast in the previous product walked coworker to coworker *inside the page*. A
refresh killed the walk after one reply. That is not a bug to patch: delivery, the queue and the
waiting had the wrong owner. Anything that dies with a tab cannot be the thing that runs the work.

Consequences that follow from moving it, not added on top:
- refresh, tab close, lid shut, browser crash — the work continues;
- a broadcast is **parallel**, because the constraint that forced sequence (one live binding per
  page) is gone;
- the same coworker is visible from every client at once, doing the same work.

## The client is the Grok Bot desktop app

`/Volumes/goldcoders/OSS/grok-bot` is a reconstruction of a shipped Electron app whose brain lived
on somebody else's servers. It has three layers: **renderer** (the UI), **host** (Electron main,
editable TypeScript), and the **box** (the agent's computer). The host talks to its backend through
one JSON command surface — `SAND_GATEWAY_COMMANDS` in `source/host/gateway-protocol.ts`, ~70 named
commands — and renders a durable transcript of typed entries plus a live activity stream.

**We implement that command surface.** That is the compatibility promise: the client keeps its UI,
its transcript format and its activity model, and OpenGrok answers where the old backend did.

### The legal line, stated once and kept

- We implement a **wire contract for interoperability** — the message shapes the client already
  emits, from stubs already in that tree. No upstream server source exists to copy and none is
  used. The brain behind the wire (loop, tools, routing) is ours.
- `grok-bot/NOTICE.md` requires an independent rights review before public redistribution, and
  `docs/grok-0.27-disparity-proto.md` says "inventory only." Both hold: **OpenGrok stays private
  until a rights review clears it.** Nothing in this plan depends on publishing.
- No Cursor trademarks in product surfaces. Vendored generated stubs stay out of this repo; where a
  shape is needed it is transcribed into `og-wire` with its provenance noted.

## Shape

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

Mirrors open-ai-gateway's crate-per-concern layout deliberately: the two ship together and a
reader who knows one should navigate the other. Axum 0.8, sqlx 0.9, Rust 2024 — the gateway's
versions, so a shared workspace stays possible.

### One binary, two doors

The gateway already serves an inference listener and an admin listener from `oag-server`, with
`oag-core` holding pure domain types. OpenGrok takes the same shape and the release ships both:
the gateway is the model door in OpenGrok's own wall, not a service to deploy beside it.

## The four seams

**1. The wire (`og-wire`).** Transcribed, not designed. Unknown entry kinds round-trip untouched —
dropping an entry deletes somebody's message from their own history. Three wires kept apart:
commands (request/response), transcript (durable), activity (live, disposable — a client that
missed an update catches up from the transcript).

**2. The harness (`og-harness`).** Rig for provider abstraction, our own loop for durability. No
Rust framework offers a loop that survives the process; that suspension *is* the product, and it is
~400 lines either way. Every model call exits through open-ai-gateway, so a coworker's pin
(`xai/grok-4.6@sub`) means the same thing it means today.

**3. The computer (`og-box`).** A trait, so a coworker's computer is a seam and not a vendor.
box.ascii.dev is the first implementation: persistent Ubuntu VMs, plain REST, bearer auth — a
`reqwest` client is the whole integration. Verified: create, sync/detached exec, file read/write,
port exposure with preview URLs, stop/resume/fork, destroy. **Known gaps, designed around rather
than discovered later:** no live stdout socket (detached + poll is the honest shape, which is why
`run` and `watch` are separate methods); no directory-list endpoint (shell `ls`); computer-use is a
VNC URL, not click/screenshot primitives; hosted-only, EU regions; concurrency is plan-gated.

**4. Tools (`og-tools`).** Native tools (shell, files, ports) execute against the box. Connectors
come from **open-connector** — Apache-2.0, ~1,450 providers / ~15,000 actions, and contrary to the
earlier audit, its OAuth flow and credential vaulting are *in the open repo*, not behind a paid
tier. What the hosted tier sells is pre-registered OAuth apps. Two real caveats: it is
single-tenant-per-instance (no tenant columns), and each action's method/path lives inside
TypeScript executors rather than as flat data — so "one generic Rust executor over pure JSON" is
not free. Decision: **run their Node runtime as a sidecar behind our tool trait** for the first
connectors, keep the option of extracting definitions to JSON later. Our policy layer, not theirs,
decides who may call what.

**5. Policy (`og-policy`).** Configure from the client; the server accepts or denies. Five separate
questions (may this principal talk to this coworker / what may the coworker ever do / what may this
principal make it do / whose records may a call touch / which calls need a human yes), enforced in
Rust so tenancy is a compiler fence. Identity arguments are **overwritten** from the session, not
validated — the model never gets a say in whose data it fetches.

## Phases

Each is shippable and each ends in something demonstrable.

**P0 — the workspace.** ✅ done: nine crates, workspace lints (`unsafe_code` forbidden;
`unwrap`/`expect`/`panic` denied), `cargo check` clean. Next: CI, `.sqlx` offline, the gateway
embedded as a dependency.

**P1 — the client says hello (~1 week).** `og-server` answers `listAgents`, `createAgent`,
`getAgentTranscriptTail`, `openAgentTail`. The desktop app boots against OpenGrok and shows a
roster from our Postgres. *Done when:* the app lists coworkers that exist only in our database.

**P2 — a turn (~1–2 weeks).** `sendPrompt` → `og-harness` runs a turn through Rig → OAG → a real
model; text streams back as activity; the transcript persists. *Done when:* a coworker answers in
the desktop app, and the answer survives a restart of the server mid-run.

**P3 — the computer (~1 week).** `og-box` against a real box.ascii.dev VM; shell and file tools
wired; the two unpinned response shapes written down. *Done when:* a coworker runs `ls`, writes a
file, and exposes a port whose preview URL opens.

**P4 — broadcast, done right (~1 week).** The fan-out ledger: one request, a durable row, every
coworker asked **in parallel** (small concurrency cap for the subscription's sake), answers landing
as each finishes. *Done when:* the walk completes with the client closed the whole time — the bug
that started this, fixed by construction.

**P5 — policy and approvals (~2 weeks).** The gate on every tool call, audit in the same
transaction, approvals that suspend a run and resume days later. *Done when:* a refusal reaches the
model as a result rather than an exception, and an approval granted from another device resumes the
run exactly once.

**P6 — connectors (~2–3 weeks).** open-connector behind the tool trait; per-principal connections;
OpenGrok itself as an MCP server. *Done when:* two people in one room see their own accounts' data.

## What carries over from OpenSesame

Not wasted work — proven designs, and a second client later: threads bound one-per-coworker (the
platform refuses otherwise), the woven room transcript, the collapse of a fan-out's copies into one
ask, per-coworker model pins through the gateway, and every gateway bug we found and got fixed.

## Open questions for the operator

1. box.ascii.dev is hosted-only and EU-only. Fine for v1, or does a local Docker `Computer`
   implementation need to land in P3 rather than later?
2. open-connector as a Node sidecar in v1 — acceptable operationally, or hold connectors until a
   Rust executor exists?
3. Single-tenant (your workspace) or multi-tenant from the start? P5's shape depends on it.
