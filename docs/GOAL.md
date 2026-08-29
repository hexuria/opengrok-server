# The goal

Stated by the operator, 29 Aug 2026. This supersedes the phase framing in `PLAN.md` §5 where they
disagree; `PLAN.md`'s seams and decisions otherwise stand.

**Rebuild Grok Bot's server from scratch** — every service the desktop client uses, derived from
the client's own manifest at `/Volumes/goldcoders/OSS/grok-bot` — so that each agent gets its own
remote computer and keeps working when the person's laptop is off. Clients are windows; the server
is the product.

The rights line still applies in full: message shapes are **transcribed with provenance, never
vendored** (`LEGAL.md`), and this repo stays private until a rights review clears it.

## Stack (operator decisions)

| Piece | Decision |
|---|---|
| Language / web | Rust, Axum 0.8 |
| RPC | **tonic + prost** for service/message definitions (operator reaffirmed with the Connect finding known). The desktop client speaks ConnectRPC over HTTP/1.1 (`cursor-inference.ts:157`), which a bare tonic server cannot answer — so Connect-compatible routes are served at the Axum edge, reusing the same prost types. tonic is the internal/service backbone. |
| Design | DDD + CQRS + ES: append-only event store in Postgres, projections for reads. No ES framework — a transcript is already an event log. |
| DB / cache | PostgreSQL; Redis added **when a measured hot query needs it**, not before |
| Model door | Every model call exits through open-ai-gateway (`oag_live_` key, OpenAI-compatible routes). Rig (`rig-core`) is the provider abstraction and brings MCP via rmcp — use Rig's integration rather than wiring rmcp separately. |
| Harness | Our own durable loop (the suspension is the product). Design it as an explicit graph — nodes as steps, edges as transitions — so runs can be resumed, branched, and inspected. |
| Computers | One per agent (or shared, by policy) behind the `Computer` trait. box.ascii.dev first (7-day trial to validate); cua via its MCP server; local Docker later. |
| AG-UI | Protocol yes, `ag-ui-rs` crate no (repo vanished, 108 downloads). Transcribe the events into `opengrok-wire`; barok-works' `grok-runtime` is the reference consumer. |
| Plugins / connectors | [Agent Plugins](https://agent-plugins.org/) format (`plugin.json` + `skills/` + `mcp.json`) for gmail, github, gdrive, mem0/OpenMemory. Connectors are MCP servers we connect to, not code we write. |
| Sandboxed shell | `vercel-labs/just-bash` is TypeScript — usable only inside a Node sidecar or on the box itself, not in-process. Evaluate when the tool executor needs a no-box shell; do not block on it. |
| Repo | `hexuria/opengrok`, **private** (LEGAL #4). CI: fmt + clippy + test on every push. |

### Evaluated and deferred

**Crux (`redbadger/crux`)** — proposed for "one backend, many clients". It does not fit *this*
crate: Crux is explicitly a **client-side** framework, a Rust core that runs inside each app
(iOS/Android/web) with a platform shell doing I/O. Putting agent logic in a core that ships inside
the client is the exact shape non-negotiable #5 forbids — the prior product died of it. The goal it
serves is already met here by the server owning the logic behind one wire contract, and the part
worth copying is already in place: `opengrok-core` is pure and I/O-free, like a Crux core, which is
why the domain tests need no database. **Revisit when we build our own clients** (the CLI and a
native app sharing view logic) — that is a client decision, not a server one.

## The client: openbot, over AG-UI

**Operator decision, 29 Aug 2026.** The client is
[`hexuria/openbot`](https://github.com/hexuria/openbot) — a fork of CopilotKit/OpenBot: **MIT,
public, React 19 + TanStack Router + Vite**, pinned to **`@ag-ui/core` 0.0.57**. It already has
computer-per-agent containers, a policy gateway and Postgres, and it accepts **any AG-UI endpoint**
as a "Bot".

**OpenGrok is that endpoint.** openbot governs and renders; OpenGrok owns the agents, the boxes,
the harness and the models.

```
openbot  ──AG-UI──▶  OpenGrok
(governance,          (agents, boxes,
 rendering)            harness, models)
```

Why this and not "replace openbot's backend": AG-UI is a published, versioned, MIT-licensed
protocol, so the contract is a specification we may implement freely — unlike grok-bot's surface,
which is a reconstruction under a rights review. It is also the smallest possible integration: one
endpoint, and openbot already knows how to talk to it.

### What this means for the grok-bot contract

The desktop-client work is **not deleted and not wasted** — `opengrok-wire` and the auth slice are
the record of how a real client behaves, and they are what a Grok Bot compatibility mode would be
built from. But grok-bot is no longer the thing we are racing to satisfy, and it is the reason this
repo cannot be published (`LEGAL.md`). Treat it as a second, optional client.

### `web/` is ours, and stays

The Next.js app in `web/` is a development harness, not the product: it proves a slice from a
browser in seconds without standing up openbot. Keep it small.

## Slices, in order — one at a time, tested, verified

Done means: implemented, tested, exercised against the Next.js client, and green in CI.

1. ✅ **Auth — replace Cursor's OAuth.** *Done 29 Aug 2026 (`582521f`).* Two endpoints, our own
   JWTs, event-sourced accounts, `scripts/slice1-auth-smoke.sh` green against real Postgres.
   Original scope, kept for reference: `SAND_BACKEND_URL` points the client's whole auth backend at
   us (`cursor-token.ts:39`). A non-default backend makes `isDevAuthBackend` true, which unlocks
   `GET /auth/cursor_dev_session_token?plan=&trial=&email=` — no browser flow needed for the first
   pass. Serve that plus `POST /oauth/token` (refresh grant → `{access_token, refresh_token}`) and
   the profile fetch (`cursor-profile.ts`), minting our own JWTs. `logged-in` is just "both tokens
   present" (`cursor-session-policy.ts:cursorSessionPresent`). Full browser flow
   (`/loginDeepControl` + `/auth/poll`, PKCE-style challenge/verifier) is the follow-up.
2. **AG-UI endpoint** — `POST /ag-ui` streaming the 32 event types of `@ag-ui/core` 0.0.57 over
   SSE, so openbot can add OpenGrok as a Bot and get a reply. This is now the spine: everything
   after it is reached through this endpoint.
3. **Say something and be answered** — the harness loop on Rig, out through open-ai-gateway,
   streamed back as AG-UI `TEXT_MESSAGE_*` / `TOOL_CALL_*`; runs and transcripts as events in the
   store. Durable: a run survives the client disconnecting.
4. **The computer** — box.ascii.dev behind the `Computer` trait; an agent works on its own box and
   keeps working when the laptop is off. This is the goal's headline.
5. **Plugins** — Agent Plugins loading; mem0 memory; gmail/github/gdrive connectors via MCP.
6. **Grok Bot compatibility (optional)** — the P1 command table and SSE from `RUNBOOK.md`, plus the
   remaining seam-B Connect services in `opengrok-proto`. Blocked on the rights review for
   publication, not for local work.

## Provenance for slice 1 (read before implementing)

- `grok-bot/source/electron-main/account/cursor-auth.ts` — the whole client-side flow: login URL
  construction (:115-118), poll (:121-130), dev session token (:311-314), refresh (:340-347),
  token body shape (:160-166).
- `grok-bot/source/shared/node/cursor-token.ts` — backend resolution (:39), client id / dev
  detection (:42-50).
- `grok-bot/source/shared/cursor-session-policy.ts` — what "signed in" means, and that Cursor
  sessions survive provider switches.
- `grok-bot/source/electron-main/account/cursor-profile.ts` — the profile endpoint the slice must
  also serve.
