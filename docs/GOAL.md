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

## Slices, in order — one at a time, tested, verified

Done means: implemented, tested, exercised against the real client, and green in CI.

1. **Auth — replace Cursor's OAuth.** `SAND_BACKEND_URL` points the client's whole auth backend at
   us (`cursor-token.ts:39`). A non-default backend makes `isDevAuthBackend` true, which unlocks
   `GET /auth/cursor_dev_session_token?plan=&trial=&email=` — no browser flow needed for the first
   pass. Serve that plus `POST /oauth/token` (refresh grant → `{access_token, refresh_token}`) and
   the profile fetch (`cursor-profile.ts`), minting our own JWTs. `logged-in` is just "both tokens
   present" (`cursor-session-policy.ts:cursorSessionPresent`). Full browser flow
   (`/loginDeepControl` + `/auth/poll`, PKCE-style challenge/verifier) is the follow-up.
2. **The client says hello** — roster from Postgres, SSE, the P1 command table. `RUNBOOK.md` is the
   procedure; it predates this goal and still holds.
3. **Say something and be answered** — `sendPrompt` through the harness loop on Rig, out through
   open-ai-gateway, streamed back; runs and transcripts as events in the store.
4. **The computer** — box.ascii.dev behind the `Computer` trait; an agent works on its own box and
   survives the client disconnecting.
5. **Seam B beyond auth** — the remaining Connect services the client calls, transcribed into
   `opengrok-proto`, served at the Axum edge.
6. **AG-UI endpoint** — the web window onto the same server.
7. **Plugins** — Agent Plugins loading; mem0 memory; gmail/github/gdrive connectors via MCP.

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
