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
| Approvals | A tool may need a human yes (PLAN §4.5 layer 5). The run suspends; it does not fail. |
| Design | DDD + CQRS + ES: append-only event store in Postgres, projections for reads. No ES framework — a transcript is already an event log. |
| DB / cache | PostgreSQL; Redis added **when a measured hot query needs it**, not before |
| Model door | Every model call exits through open-ai-gateway (`oag_live_` key, OpenAI-compatible routes). **Two doors, both behind `ModelDoor`:** direct (default) and `OG_MODEL_DOOR=rig` through `rig-core`. Rig abstracts one provider today, so the direct door stays default; Rig earns its keep when a coworker is pinned somewhere the gateway does not route, and brings MCP via rmcp. |
| Harness | Our own durable loop (the suspension is the product). Design it as an explicit graph — nodes as steps, edges as transitions — so runs can be resumed, branched, and inspected. |
| Computers | One per agent (or shared, by policy) behind the `Computer` trait. **Local Docker is the default** and needs no account; box.ascii.dev when `OG_BOX_API_KEY` is set, for computers that outlive this machine; cua via its MCP server later. |
| AG-UI | Protocol yes, `ag-ui-rs` crate no (repo vanished, 108 downloads). Transcribe the events into `opengrok-wire`; barok-works' `grok-runtime` is the reference consumer. |
| Plugins / connectors | [Agent Plugins](https://agent-plugins.org/) format (`plugin.json` + `skills/` + `mcp.json`) for gmail, github, gdrive, mem0/OpenMemory. Connectors are MCP servers we connect to, not code we write. |
| Sandboxed shell | `vercel-labs/just-bash` is TypeScript — usable only inside a Node sidecar or on the box itself, not in-process. Evaluate when the tool executor needs a no-box shell; do not block on it. |
| Repo | [`hexuria/opengrok-server`](https://github.com/hexuria/opengrok-server), **private** (LEGAL #4). Checked out at `/Volumes/goldcoders/OSS/opengrok-server`. Renamed 29 Aug 2026 to free the name `opengrok` for another repo; **the crates did not change** — `opengrok-server` in `crates/` is the Axum crate and always was. |
| The gate | `scripts/gate.sh` runs everything the workflow runs; `--smoke` adds all five smoke scripts. **GitHub Actions is currently blocked on account billing** ("recent account payments have failed or your spending limit needs to be increased"), so the local gate is the gate until that is settled. Publishing to get free minutes is not an option the rights review has cleared. |

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
   - ✅ *The durable loop — done 29 Aug 2026.* `run_conversation` runs model → tools → model, with
     each round's events reaching the journal **before** the next model call. `RunJournal` carries
     that ordering as a seam; the test asserting it was verified by breaking the rule and watching
     it fail. `MAX_ROUNDS` bounds a model that never stops, ending the run as a readable error.
   - ✅ *The roster — done 29 Aug 2026.* `POST /coworkers` hires (optionally with a computer),
     `GET /coworkers` returns the roster as an array scoped to the bearer's account, and tools are
     built **per request** from the coworker named in AG-UI's `forwardedProps`.
     `scripts/slice5-roster-smoke.sh` includes the first tenancy check: a coworker does not appear
     on another account's roster.
   - ✅ *A computer with no signup — done 29 Aug 2026.* `DockerComputer` makes a local container a
     `Computer`, chosen automatically without a box key, so the headline works on a laptop today.
     Three tests drive a real daemon; `scripts/slice6-computer-smoke.sh` is the goal in one script.
   - ✅ *Policy — done 29 Aug 2026.* `opengrok-policy` answers PLAN §4.5's layers 1–3: a grant per
     principal-and-coworker, a ceiling per coworker, combined by **intersection, never union**.
     Checked on every turn and before every tool call, never once at sign-in. Every unknown denies.
     `scripts/slice7-policy-smoke.sh` is the attack itself: one account naming another's coworker.
   - ✅ *Layer 4 — done 29 Aug 2026.* A run carries its owner and only they may replay it; both
     "no such run" and "not yours" answer 404 so an id reveals nothing. This closed a real hole:
     `GET /ag-ui/runs/{id}` had returned any conversation to anyone who named the id.
   - ✅ *Layer 5 — done 29 Aug 2026.* A tool can need a human yes; the run suspends rather than
     ending, stays `running` in the log, and `POST /coworkers/{id}/approvals` sets the list.
     `scripts/slice8-approval-smoke.sh` proves it suspends, that waiting does not read as success,
     and that withdrawing the requirement lets the same tool run.
   - ✅ *Answering — done 29 Aug 2026.* `POST /ag-ui/runs/{id}/answer` settles a suspended call
     **exactly once**: the aggregate refuses every later answer and the store's sequence check
     covers the concurrent case. A retry reports the settled state rather than failing, because an
     answer unsafe to resend is a decision a flaky network loses. `GET /ag-ui/approvals` lists what
     is waiting, with the arguments, since approving `shell` unseen is approving nothing.
   - ✅ *Continuing — done 29 Aug 2026.* The server resumes an answered run itself, in the
     background, and **runs the call that was approved** rather than re-prompting: the person
     approved that command, and a second prompt could produce a different one. Approval is per
     call, never per tool. `scripts/slice8-approval-smoke.sh` sends no further request and watches
     the run finish on its own, then checks the approved command's marker on the box.

   - ✅ *Recovery — done 29 Aug 2026.* A sweep claims runs abandoned by a restart and ends them,
     told apart from live ones by a **lease** the run endpoint holds while serving. Claiming is one
     `update … returning`, so two replicas cannot take the same run. A run interrupted between a
     tool call and its result is **not** re-run: the outcome is genuinely unknown, and the failure
     says so rather than repeating whatever the command did. `scripts/slice9-recovery-smoke.sh`.

**PLAN §4.5 layers 1–5 are all enforced, and every run reaches an ending.** The arc runs unattended:
sign in → hire → a computer of its own → talk → policy every turn → tools on its own box → a risky
call waits for a person → one answer settles it → the server resumes itself → durable throughout →
and a run whose process dies is picked up rather than left hanging.
5. **Plugins** — *in progress.*
   - ✅ *The bundle format* — `opengrok-plugins` reads `plugin.json` + `skills/` + `mcp.json`,
     transcribed from the published 1.0.0 schemas. A plugin is a **bundle**; MCP is the protocol one
     of its servers speaks; `rmcp` is a client for that protocol — three layers, not one.
   - ✅ *Curation, not authorship.* We do not write Gmail or GitHub — we keep a list we have read.
     `Trust::Unverified` is not a badge: its tools arrive needing a human yes, via slice 4b's
     approval machinery. `Policy::CuratedOnly` (the default) refuses the rest outright.
   - ✅ *Connections* — `opengrok-core::connection`. Three scopes (**global / user / bot**) and the
     lend: a person authenticates once and lends the connection to as many coworkers as they like,
     rather than each one signing in again. Resolution is most-specific-first, so a bot's own
     account beats one lent to it.
   - **Still to do:** storing the tokens encrypted (needs `OG_CREDENTIAL_KEK`), connecting to an
     MCP server with `rmcp`, and surfacing installed plugins as tools on the executor.
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
