# What to port, in what order

Derived 30 Aug 2026 from the live client at `/Volumes/goldcoders/OSS/opengrok`, re-verified
against source rather than recalled. Successor to the ordering implied by
`research/client-grok-bot.md` §9; that document remains the per-command reference and is not
replaced. Where the two disagree, the corrections in §6 here are the measured ones.

---

## 1. The bottom line

Three facts decide the whole plan.

**The gateway is already ours in shape.** Seam A is 123 unique commands
(`source/host/gateway-protocol.ts` declares 130 keys; 7 are duplicate keys the object literal
overwrites). 90 are reachable from the renderer, 33 are host-only. This is a JSON+SSE contract,
not protobuf, and it is the surface the app spends its life talking to.

**Seam B is small where it is load-bearing.** The Cursor ConnectRPC backend defines 8 services
and hundreds of messages, but the client's own mock — `source/mock/`, the thing the app boots
against in development — implements exactly **two services and 18 methods**. That mock is a
working specification for the minimum, and it is checked in. Do not port from the proto
inventory; port from the mock.

**Our own usage already bypasses most of Seam B.** With a routed provider selected, the
coordinator's `inference-router.ts` answers **20 of the 123 gateway commands locally** and never
dials Cursor at all. That is why the app works today with no account. It also means a server that
omits those 20 still serves our usage — but is not a drop-in.

**Consequence:** the order you *ship* in and the order you *test* in are different, and this is
the single most useful thing on this page. `SAND_HOST_GATEWAY_URL` repoints the client at any
gateway without touching auth, so P2–P4 are testable **before P0 exists**. Build auth first if
the goal is drop-in fidelity; build the gateway first if the goal is a working system this month.
Recommended: **P2 → P3 → P4 first**, then P0/P1, then the rest.

---

## 2. The priority ladder

Ranked by the order a person actually meets them, which is also roughly the order the client
calls them on a cold boot.

### P0 — Identity (Seam B auth)

The user's stated first priority, and the gate on drop-in fidelity. It is **not** a gate on a
working system — see §1.

| Piece | Surface |
|---|---|
| Browser login | `GET {website}/loginDeepControl?challenge=&uuid=&mode=login&redirectTarget=` |
| Poll | `GET {api}/auth/poll?uuid=&verifier=` → `{accessToken, refreshToken}` |
| Refresh | `POST {api}/oauth/token` |
| Dev sign-in | `POST {api}/auth/cursor_dev_session_token` → `{accessToken, refreshToken?}` |
| Callback | the `sand://` deep link — never rename it (`docs/…` in the client repo explains why) |

Challenge is PKCE-shaped: `verifier = base64url(random 32)`, `challenge = base64url(sha256(verifier))`,
`uuid = randomUUID()` (`source/packages/cursor-config/auth/login.ts:17-21`). Tokens are JWTs the
client parses for expiry (`shared/node/cursor-token.ts`) and hashes for a cache scope; issue
something `parseJwtPayload` accepts with a real `exp`, or the client refreshes in a loop.

Identity is then read over ConnectRPC — **`DashboardService`, 6 methods**, all of which the mock
already implements: `getMe`, `getTeams`, `getTeamAdminSettings`,
`getTeamAdminSettingsOrEmptyIfNotInTeam`, `getUserPrivacyMode`, `updateUserName`.

*Done when:* the app shows a signed-in account with no login wall and no refresh loop.

### P1 — The mint (Seam B → Seam A handoff)

One method, and it is the hinge of the whole architecture:
`GrokBotService.EnsureSandBox` → `{gatewayUrl, gatewayToken, networkToken, vncUrl, forkVncBaseUrl}`
(`electron-main/box/box-host-connector.ts:28,85-100`). This is where the cloud backend hands the
client the address and bearer of a gateway. Empty `gatewayUrl` throws a named error.

**`gatewayUrl` must not be loopback.** `local-docker-host-connector.ts:465` refuses a gateway
whose host starts with `127.0.0.1` or `localhost` unless `boxRuntime === "local-docker"`. Serve on
a LAN address or a hosts alias.

*Done when:* the client dials your gateway with your token, unprompted.

### P2 — Gateway liveness

`GET /health` (1500 ms deadline, 5 s TTL) → `{ok, pid, isBusy, activeAgentId, startedAt, lastBusyAtMs}`.
`GET /events` with `Accept: text/event-stream` → `retry: 1000`, then `:ping` at ≤15 s or a 35 s
watchdog aborts and reconnects forever. Auth is a single shared bearer, timing-safe compared
(`host/gateway-server.ts:21`); the local-exec relay additionally requires
`x-anyrun-network-token`.

There are **21 SSE channels**, not 18. Nineteen are in the family map
(`gateway-event-families.ts`): `agents`, `agent-upserted`, `transcript`, `tray`, `outline`,
`subagents`, `async-tasks`, `automations`, `agents-automation`, `workflows`, `agents-workflow`,
`host-settings`, `mcp-servers`, `mcp-servers-updated`, `forever-box`, `box-disk-pressure`,
`computer-action`, `sharing`, `teach-recording`. Two more are emitted outside it and are easy to
miss: `memory` (`host/sand-host.ts:939`) and `mcp-oauth-pending` (handled specially at
`node-agent-coordinator/main.ts:126`).

*Done when:* `transport-connected`, and no reconnect loop for ten minutes.

### P3 — Roster and settings — 12 commands

`listAgents`, `countAgents`, `getTrays`, `dismissTray`, `clearTrays`, `isAgentNetworkEnabled`,
`isGlobalSearchEnabled`, `isEgressTunnelAvailable`, `getHostSettings`, `setHostSettings`,
`getForeverBoxStatus`, `getHostStatus`.

Shape discipline matters more than behaviour here: `countAgents` must return a **number**,
`getTrays` an **array**, `getForeverBoxStatus` `null` or a well-formed record, and
`setHostSettings` must **echo the full settings record back** — the resync chain reads it. Wrong
container types throw named errors in the renderer.

*Done when:* sidebar populates, no onboarding screen, no malformed-reply throw.

### P4 — One conversation — 13 commands

`openAgentTail`, `getAgentTranscriptTail`, `openAgentWindowed`, `getAgentTranscriptWindow`,
`getAgentTranscript`, `getAgentTranscriptPage`, `getTranscript`, `getAgentThread`,
`getConversationOutline`, `sendPrompt`, `promptAcceptanceStatus`, `openAgent`, `setWindowFocused`,
plus SSE `transcript` (`appended`/`updated`) and `agent-upserted`.

*Done when:* a message sends and an answer streams in. **This is the milestone that proves the
port**; everything below is breadth.

### P5 — Agent lifecycle — 19 commands (10 already answered locally)

Create/update/delete/duplicate/search, avatars, unread, notification prefs, sidebar hiding,
groups, subagents, async tasks, reactions, transcript deletion.

### P6 — Tools, approvals, widgets — 7 commands (1 local)

`resolveLocalToolPermission`, `resolveAutoReviewApproval`, `respondToWidget`, `dismissWidget`,
`submitSecret`, `appendConnectorCard`, `requestWebAuthnCeremony`. The approval model is
already ours conceptually — `opengrok-core` has approvals with exactly-once answers (slice 4).

### P7 — Attachments and media — 5 commands

`uploadAttachment`, `readAttachmentImage`, `readAttachmentText`, `readAttachmentChunk`,
`searchMedia`, plus the `/avatars/<id>` endpoint and the `x-sand-slim-avatars` header.

### P8 — MCP and skills — 15 commands

MCP refresh/list/execute, box secrets, OAuth completion, the skills catalogue and publishing.
Largely already built in `opengrok-plugins` and the credential vault (slice 5) — this tier is
mostly adapting existing work to the gateway's method names.

### P9 — Automations and workflows — 15 commands (7 local)

Maps onto slice 6's scheduler and monitor. The server already fires runs on its own; this tier
is the client-facing surface for it.

### P10 — Box lifecycle and store — 13 commands

`ensureForeverBox`, reset/update/auto-update/hand-back, store snapshot/status/clear, host update,
migration prepare/resume. `opengrok-box` covers the substance; this is the control surface.

### P11 — Defer deliberately — 24 commands

Sharing and rooms (10), teach recording (3), channels and listener integrations (6), agent
memories (3), `broadcastToAgents`, `requestDiskSaverAudit`, `clearAgentImageMetadata`. None are
on any path a user takes in normal use, and upstream deleted adjacent features in 0.30.

---

## 3. Seam B: the honest minimum

Eight services exist. Two carry the boot. The mock is the specification.

**`DashboardService`** — 6 methods (all listed in P0).

**`GrokBotService`** — the client ships two variants of this service; the mock serves the
`.ported` one (46 methods declared). It implements **12**, and that is the working minimum:
`ListGrokBotAgents`, `CreateGrokBotAgent`, `UpdateGrokBotAgent`, `DeleteGrokBotAgent`,
`ListGrokBotTranscriptEntries`, `CommitGrokBotTranscriptEntries`, `SendGrokBotUserMessage`,
`GetGrokBotSendStatus`, `SetGrokBotAgentClientState`, `ListGrokBotUserComputers`,
`ReadGrokBotAgentAttachmentChunk`, plus `EnsureSandBox` from the non-ported variant (P1).

The other six services are reachable but not on the boot path: `AiService` (models, transcribe,
web tools, image gen), `AnalyticsService` (telemetry — safe to accept and drop),
`InferenceService` (`Stream` — only used when Cursor is the inference provider, which it is not
for us), `BackgroundComposerService`, `AutomationsService`, `SandBoxService`.

**Transport note that will bite:** the client speaks **Connect-style unary — POST + JSON over
HTTP/1.1** (`cursor-inference.ts:157`). A bare tonic gRPC server cannot answer it. This is
already the decision recorded in `GOAL.md` (Connect routes at the Axum edge, prost types shared,
tonic internal) and slice 8.1–8.2 is exactly this work. Nothing here changes that plan; it
confirms it and narrows the message set to transcribe from "hundreds" to **18 methods across two
services**.

`LEGAL.md` stands: transcribe with provenance, never vendor the generated stubs.

---

## 4. What changed since `research/client-grok-bot.md`

That document predates three things in the client.

**The routed provider architecture.** `inference-router.ts` `dispatch()` intercepts before the
gateway client, so with `codex`/`openrouter`/`claude-code` selected the coordinator *is* the
backend for 20 commands. The research doc's framing — "the client keeps its UI, OpenGrok answers
where the old backend did" — is still right, but the client is no longer helpless without a
backend, which is why P2–P4 can be built and tested before P0.

**The privilege split.** Local tools that need macOS permissions cannot run in the helper
process; they are delegated to the Electron main process. A server implementing local-exec must
expect the *client* to decide where a tool physically runs, and must not assume the daemon can do
everything the app can.

**0.30.** The macOS Messages tools and the per-action consent card exist now. They ride the
existing local-tool permission model, so they add no new gateway commands — but a server should
expect `resolveLocalToolPermission` traffic for action kinds the research doc does not list.

---

## 5. How to rebuild or update the server

Do not restructure. The crate layout already matches the seams, and slices 1–6 are done. Three
changes to the plan:

1. **Re-scope slice 8.1.** "Hand-transcribe the P1 command-table messages" is now a bounded job:
   two services, 18 methods, sourced from `source/mock/` rather than the proto inventory. Say so
   in the roadmap so the next person does not open `grok_bot_pb.ts` and lose a week.

2. **Insert a gateway slice before slice 8.** P2–P4 (38 commands, plus `/health` and `/events`)
   is the shortest path to a client that boots and holds a conversation against our server, and
   it needs no auth work at all thanks to `SAND_HOST_GATEWAY_URL`. It is also the strongest
   possible smoke test: the real client, unmodified.

3. **Keep P11 out of the roadmap entirely.** Listing 24 commands nobody uses as pending work
   makes the tracker lie about how far away done is.

Suggested slice shape, consistent with the existing tick discipline:

- **Slice 9 — the gateway boots the real client.** `/health`, `/events` with the 19 channels,
  P3's 12 commands. Verified by launching the shipped app against it with
  `SAND_HOST_GATEWAY_URL` on a non-loopback host and seeing a populated sidebar.
- **Slice 10 — a conversation.** P4's 13 commands and the two SSE event shapes. Verified by
  sending a message from the real app and watching it stream.
- **Slice 11 — Seam B.** P0 + P1: auth endpoints, `DashboardService`'s 6, `EnsureSandBox`.
  Verified by removing `SAND_HOST_GATEWAY_URL` and letting the client mint its own connection.
- **Slice 12+ — breadth.** P5 through P10, in that order.

---

## 6. Corrections to existing docs

Measured against the client on 30 Aug 2026. All are small; all would cost someone time.

| Claim | Where | Correction |
|---|---|---|
| "128 commands" | `research/client-grok-bot.md` §0 | **123 unique.** The literal declares 130 keys; 7 are duplicates (`listAgents`, `createAgent`, `createGroup`, `duplicateAgent`, `setGroupMembers`, `setAgentAvatarBytes`, `updateAgent`). PLAN.md's "123" is right. |
| "18 SSE channels" | §4.4 | **21.** The §4.4 table is missing `agents-automation`, `agents-workflow` and `mcp-servers-updated`. Its `memory` and `mcp-oauth-pending` rows are correct — they are emitted outside the family map. |
| loopback refusal at `local-docker-host-connector.ts:437-443` | PLAN.md trap | Now `:465`. The behaviour is unchanged and still the first thing to trip over. |
| "only **91** [renderer-reachable]" | §11 | **90.** Measured from `COORDINATOR_METHOD_TABLE`. |
| "Seam B — neutralise, do not implement" | §0, §6.3 | Superseded by the 30 Aug operator decision (roadmap slice 8). Implement it — the minimum is 18 methods. |
