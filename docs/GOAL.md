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
3. ✅ **Say something and be answered** — *Done 29 Aug 2026.* The harness, the projection and two
   doors (gateway + mock); `scripts/slice3-harness-smoke.sh` green. **Not yet durable** — a run is
   not written to the event log, so it does not survive a restart. That moves into slice 4, where
   it belongs with the boxes. Original scope: the harness loop on Rig, out through open-ai-gateway,
   streamed back as AG-UI `TEXT_MESSAGE_*` / `TOOL_CALL_*`; runs and transcripts as events in the
   store. Durable: a run survives the client disconnecting.
4. **Durability, then the computer.** Two halves of the same promise.
   - ✅ *Durable runs — done 29 Aug 2026.* Every event is appended to the log **before** the client
     sees it, and `GET /ag-ui/runs/{id}` replays a run without asking a model again.
     `scripts/slice4-durability-smoke.sh` SIGKILLs the server mid-run and proves the work survived.
     `PgStore::interrupted_runs` makes a run orphaned by a restart findable rather than merely
     absent — nothing consumes it yet; resumption starts there.
   - ✅ *The computer — done 29 Aug 2026.* `AsciiBoxes` implements `Computer` over box.ascii.dev's
     REST API, with 15 integration tests driving it against a stand-in server (paths, bearer token,
     request bodies, error mapping). **Unverified against the live service** — needs a `box_` key,
     and two shapes the vendor's reference leaves unpinned are marked in `ascii.rs`.
   - ✅ *Assignment and tools — done 29 Aug 2026.* The `coworker` aggregate owns the box (assigned,
     never requested; dedicated or shared), and `opengrok-tools::Executor` runs shell/read/write on
     the coworker's own box with identity arguments **overwritten, not validated**.
   - ✅ *The chain joins — done 29 Aug 2026.* `run_turn_with_tools` reassembles tool calls from the
     stream, runs them on the coworker's own box, and emits `TOOL_CALL_RESULT`. The end-to-end test
     has a model ask for another coworker's box and get its own.
   - **Still to do:** a **multi-round** tool loop. One round runs today; feeding results back for
     another model call must be resumable *between* rounds, which means each round reaching the log
     before the next call. That is the next slice, and doing it with an in-memory `while` would
     build the exact thing this project exists to avoid.
5. **Plugins** — Agent Plugins loading; mem0 memory; gmail/github/gdrive connectors via MCP.
6. **Grok Bot compatibility (optional)** — the P1 command table and SSE from `RUNBOOK.md`, plus the
   remaining seam-B Connect services in `opengrok-proto`. Blocked on the rights review for
   publication, not for local work.

## The mock door — test without spending a token

**Operator decision, 29 Aug 2026.** Every model call goes through the `ModelDoor` trait
(`opengrok-harness/src/model.rs`), and one implementation is a **mock** that replays a scripted
stream. Selected with `OG_MODEL_DOOR=mock`, it runs the entire stack — endpoint, harness,
projection, SSE — without reaching a provider or a subscription.

Why it earns its place beyond saving money:

- **CI can exercise the whole path.** A test that needs a live key is a test that gets deleted.
- **It reproduces what a live call cannot.** A truncated stream, a tool call split across ten
  fragments, a provider that never closes — all trivial to script, all impossible to request.
- **It is the answer when a dependency is down.** Proven the day it was written: the dev gateway's
  inference handler was closing connections without replying, and slice 3 was verified anyway.

The rule that keeps it honest: the mock produces `ModelDelta`s, the same vocabulary the real door
produces. It never gets its own path through the projection, so a bug the mock hides is a bug in
the door, not in everything downstream of it.

## Artifacts — remembered on 29 Aug 2026, not yet scheduled

Uploads and generated files — images, videos, documents — need somewhere to live and a URL the app
can load. Recorded now so it is not rediscovered late; **do not pull it forward**, it lands with or
after the harness produces files worth storing.

What is already decided by the shape of the rest:

- **Scoped, not global.** An artifact belongs to a workspace and an agent, and its reach is set the
  way the agent is configured: private to the owner, visible to a shared session, or public. That
  is a policy decision on every read, not a signed URL handed out once (non-negotiable #6).
- **A trait, like `Computer`.** Local disk behind the same seam as object storage, so a laptop
  serves files from a directory and production serves them from R2 or a VPS volume with no code
  change. Axum handles both; the difference is the domain in front.
- **The row is the truth, the bytes are not.** An artifact is a row (owner, scope, content type,
  size, checksum) with bytes attached — so it survives a storage migration, and a lost object is a
  broken link rather than a lost record. Same rule as everything else here: nothing that matters
  lives only in one place a client controls.
- **Provenance for later:** the desktop client already models this — `user-attachment` entries carry
  `file_path`/`file_name`/`byteSize` (`opengrok-wire/src/transcript.rs`) and it fetches avatars from
  `/avatars/<id>`. AG-UI carries files as message content. Both consume the same store.

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
