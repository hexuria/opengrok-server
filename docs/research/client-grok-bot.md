# The grok-bot client contract — what OpenGrok must serve

**Audience:** an engineer/agent who has never opened either repository and must implement
the OpenGrok backend so the shipped Grok Bot desktop app boots against it.

**Client repository under study:** `/Volumes/goldcoders/OSS/opengrok`
(an evidence-based reconstruction of the shipped Grok Bot 0.18.0 macOS app,
`com.anysphere.sand`, Electron 42.1.0 — see `PROVENANCE.md`).
All paths below are relative to that repository unless prefixed with `opengrok/`.

**Method note.** Every claim is anchored to a file (and line, where the file has
meaningful line structure — several recovered files are one-statement-per-line
minified-style transcriptions, so a line number points at a long single line).
Where the tree does not answer a question, this document says
**"not found in tree"** rather than guessing.

---

## 0. TL;DR — the one thing to understand

There are **two** network seams in this application, and only one of them is OpenGrok's job.

| Seam | Protocol | Who is the client | Who is the server | OpenGrok? |
|---|---|---|---|---|
| **A. The Sand gateway** | JSON-over-HTTP `POST /api/<command>` + SSE `GET /events` | the desktop app's coordinator process (`source/node-agent-coordinator/`) | the in-box "host" (`source/host/`) | **YES — this is OpenGrok** |
| **B. The Cursor backend** | ConnectRPC over `https://api2.cursor.sh` (`GrokBotService`, `DashboardService`, `InferenceService`) | Electron main | Anysphere's servers | No — but see §6.3, you must neutralise it |

Seam A is the "client-facing contract" the brief refers to. Its command table is
`SAND_GATEWAY_COMMANDS` in `source/host/gateway-protocol.ts` (**123 unique commands** — the
object literal declares 130 keys, but 7 are duplicates it overwrites: `listAgents`, `createAgent`,
`createGroup`, `duplicateAgent`, `setGroupMembers`, `setAgentAvatarBytes`, `updateAgent`.
Corrected 30 Aug 2026; PLAN.md's 123 was right).
Seam B's only load-bearing role for boot is minting the gateway URL/token
(`ensureSandBox` → `{gatewayUrl, gatewayToken, networkToken, vncUrl, forkVncBaseUrl}`,
`source/electron-main/box/box-host-connector.ts:28`), and it can be **completely bypassed**
with one environment variable (§6.1).

**The single most important discovery:** `source/electron-main/box/box-host-connector.ts:17,156-161`
— if `SAND_HOST_GATEWAY_URL` is set, `createRemoteHostConnector` returns
`EnvDescriptorHostConnector` and never calls the vendor broker at all. That is the
repoint. **But** `source/electron-main/box/local-docker-host-connector.ts:465` then
wraps it and **throws if the resolved gateway host starts with `127.0.0.1` or `localhost`**
unless the app's `boxRuntime` setting is `"local-docker"`. See §10, Trap 1.

---

## 1. Architecture — the layers

```
┌───────────────────────────────────────────────────────────────────┐
│ RENDERER (Chromium)                                               │
│  · pinned minified bundle  src/app/dist/renderer/assets/          │
│      index-UbX-y3il.js       ← DO NOT REWRITE (see §1.2)          │
│  · readable reconstruction  frontend/src/recovered/**             │
│  · talks over a MessagePort ("coordinator port"), method table    │
│    COORDINATOR_METHOD_TABLE  source/shared/rpc/coordinator.ts:92  │
└──────────────────────┬────────────────────────────────────────────┘
                       │ sand-rpc:* IPC + a transferred MessagePort
┌──────────────────────▼────────────────────────────────────────────┐
│ ELECTRON MAIN            source/electron-main/**                  │
│  · window/theme/updater/secrets/auth  (MAIN_METHOD_TABLE,         │
│    frontend/src/recovered/contracts/main-rpc.ts:5)                │
│  · owns the box connector: which gateway URL to use               │
│    source/electron-main/box/box-host-connector.ts                 │
│  · spawns the coordinator child process                           │
└──────────────────────┬────────────────────────────────────────────┘
                       │ control port (JSON frames)
┌──────────────────────▼────────────────────────────────────────────┐
│ COORDINATOR              source/node-agent-coordinator/**         │
│  · CoordinatorGatewayClient  gateway/gateway-client.ts:115        │
│  · SandHostSupervisor        gateway/host-supervisor.ts:80        │
│  · serves the renderer port  renderer-port-server.ts              │
│  · optional local inference router (Claude/Codex/OpenRouter)      │
└──────────────────────┬────────────────────────────────────────────┘
                       │  ==== THE WIRE OPENGROK IMPLEMENTS ====
                       │  POST http://<base>/api/<command>   (JSON)
                       │  GET  http://<base>/events          (SSE)
                       │  GET  http://<base>/health
                       │  GET  http://<base>/avatars/<agentId>
┌──────────────────────▼────────────────────────────────────────────┐
│ HOST ("the box")         source/host/**                           │
│  · gateway server            gateway-server.ts:46-57              │
│  · command table             gateway-protocol.ts:4                │
│  · method implementations    host-gateway-api.ts:65-687           │
│  · transcript store, roster, runner, tools                        │
│  · reaches the agent's computer through the BOX SEAM (§5)         │
└──────────────────────┬────────────────────────────────────────────┘
                       │ ConnectRPC binary over HTTP/1.1, port 1337
┌──────────────────────▼────────────────────────────────────────────┐
│ EXEC DAEMON (agent's computer)  source/box-exec-daemon/server.ts   │
│  ControlService (ping/getCapabilities/updateEnvironmentVariables)  │
│  ExecService    (shell / read / background shell / stdin)          │
└───────────────────────────────────────────────────────────────────┘
```

### 1.1 Where "the host" physically runs in the shipped product

The host is a Node bundle that runs **inside the agent's sandbox container**
("the box") and binds its gateway on `SAND_GATEWAY_BIND_HOST` / `SAND_HOST_PORT`
(`source/host/gateway-config.ts:45-46`). The desktop never runs the host in-process;
it always dials it over HTTP. That is precisely why OpenGrok can replace it: the
client already speaks to it as a remote HTTP service.

Evidence that the shipped host is remote and brokered:
`source/electron-main/box/box-host-connector.ts:82-107` (`BrokeredHostConnector.connect()`
calls `ensureSandBox({})` and reads `box.gatewayUrl`).

### 1.2 What is editable vs. pinned — read this before touching anything

Governance is stated in four files and **must be honoured**:

| File | Rule that binds you |
|---|---|
| `CLAUDE.md` | "Renderer changes are exact-string patches in `scripts/lib/router-renderer-patch.mjs` applied to the pinned minified bundle at package time; pre-flight every new anchor string against `src/app/dist/renderer/assets/index-UbX-y3il.js` (must match exactly once)." |
| `PROVENANCE.md` (§ "Evidence-only reconstruction rule") | "Recovered source may express only behavior supported by at least one inspectable artifact anchor… **Do not invent or redesign a screen, route, control, label, selector, state, or interaction to fill an evidence gap.**" |
| `NOTICE.md` | "No upstream source-code license is asserted or granted here… independently review copyright, trademark, third-party dependency, and service-terms obligations." |
| `README.md` | "`frontend/` is a readable partial reconstruction and design workspace… it should not be mistaken for Anysphere's missing original frontend source." |

Practical consequence for OpenGrok:

- **Pinned / not ours to change:** `src/app/dist/renderer/**` (the shipped minified
  renderer; ignored by git, hydrated by `npm run bootstrap` from the checksum-verified
  0.18.0 DMG — `README.md` "Quick start"), and `research-archives/original/0.18.0/**`
  (Git LFS copies of the original installers).
- **Editable TypeScript we control:** `source/**` (Electron main, host, coordinator,
  shared, box-exec-daemon) and `frontend/src/**` (the readable reconstruction).
- **The contract itself is neither.** `SAND_GATEWAY_COMMANDS`, the transcript entry
  shapes, and the SSE channel payloads are *observed behaviour of the pinned artifact*.
  OpenGrok must match them; it must not "improve" them.

`source/host/gateway-protocol.ts:1` carries the marker
`/** Mechanically recovered from the immutable 0.18 host bundle. */` — treat that file
as transcription of the artifact, not as a design.

---

## 2. The command surface — the crux

### 2.0 Wire mechanics (implement these first)

From `source/shared/gateway-wire.ts:1-9` and `source/host/gateway-server.ts:46-57`:

| Constant | Value | Meaning |
|---|---|---|
| `GATEWAY_API_PREFIX` | `/api` | commands are `POST /api/<methodName>` |
| `GATEWAY_EVENTS_PATH` | `/events` | `GET`, `text/event-stream` |
| `GATEWAY_HEALTH_PATH` | `/health` | `GET`, unauthenticated-ish (still auth-gated if a token is configured) |
| `GATEWAY_AVATARS_PATH` | `/avatars` | `GET /avatars/<agentId>[?v=<version>]` returns raw image bytes |
| `GATEWAY_AUTH_SCHEME` | `Bearer` | `Authorization: Bearer <token>` |
| `GATEWAY_SLIM_AVATARS_HEADER` | `x-sand-slim-avatars` | client sends `1`; server must then null out `avatarDataUrl` |
| `GATEWAY_MINT_DEDUPE_HEADER` | `x-sand-mint-dedupe` | server sets `1` on every JSON reply; proves nonce dedupe support |
| `GATEWAY_TRACEPARENT_HEADER` | `traceparent` | W3C trace context, optional |
| `GATEWAY_NETWORK_TOKEN_HEADER` | `x-anyrun-network-token` | pod-proxy token, only in the brokered path |
| `GATEWAY_PREPARE_UPGRADE_PATH` | `/prepare-upgrade` | `POST`, `gateway-protocol.ts:129` |

Additional wire rules the client depends on:

- **Request body** is `JSON.stringify(args ?? {})` (`gateway-client.ts:264`).
  Empty body must parse as `{}` (`gateway-protocol.ts:3`, `parseCommandArgs`).
- **Response body** is `JSON.stringify(result ?? null)` with
  `content-type: application/json` and `x-sand-mint-dedupe: 1`
  (`gateway-server.ts:19`). Gzip when ≥ 1400 bytes and the client accepts it.
- **Errors:** `{"error": "<message>"}` with status. `gateway-server.ts:15` —
  `SandAgentLimitError` / `SandSkillPublishError` ⇒ **409**, everything else ⇒ **500**.
  The client treats `< 500` as a *command* error (surfaced to the user, no retry) and
  `>= 500` as *unreachable* (retry/backoff) — `gateway-client.ts:270-271`.
- **Unknown method** ⇒ **404** `unknown gateway method: <name>` (`gateway-server.ts:36`).
- **Browser-origin rejection:** any request carrying an `Origin` header ⇒ **403**
  (`gateway-server.ts:23`). If no auth token is configured, the `Host` header must be a
  loopback name or ⇒ 403.
- **`/health` reply shape** (`gateway-server.ts:48`):
  `{ ok: true, pid, isBusy, busyOnlyAwaitingApproval?, activeAgentId, startedAt, lastBusyAtMs }`.
  The supervisor only accepts it when `ok === true` (`host-supervisor.ts:55`).
  Probe deadline **1500 ms** (`host-supervisor.ts:4`), TTL 5 s.
- **SSE framing** (`gateway-server.ts:38`): first line `retry: 1000\n\n`, then
  `data: <json>\n\n` per event, plus `:ping\n\n` every **15 s**. The client's stall
  watchdog aborts after **35 s** of silence (`gateway-client.ts:40`) — heartbeats are
  mandatory, not optional.
- **SSE channel filter:** `GET /events?channels=a,b,c` (`gateway-server.ts:39`).
- **SSE event envelope:** `{ channel: string, payload: unknown }` (`gateway-client.ts:473-476`).
- **Body cap:** 256 MiB payload (`gateway-server.ts:14`).

Client-side deadlines OpenGrok must beat:

| Operation | Timeout | Source |
|---|---|---|
| SSE connect | 15 s | `gateway-client.ts:41` |
| SSE stall | 35 s | `gateway-client.ts:40` |
| `sendPrompt` POST | 15 s (`SAND_SEND_POST_TIMEOUT_MS` overrides) | `gateway-client.ts:42,74` |
| `listAgents` / `countAgents` | 15 s (`SAND_ROSTER_READ_TIMEOUT_MS` overrides) | `gateway-client.ts:43,76` |
| SSE reconnect backoff | 1 s → 10 s, ×2, unbounded attempts | `gateway-client.ts:38-39,79` |
| `createAgent` retry | 3 attempts, 1 s → 4 s | `gateway-client.ts:37` |

### 2.1 Full inventory of `SAND_GATEWAY_COMMANDS`

All 128 names, grouped by purpose. `gateway-protocol.ts:<line>` is the declaration;
`host-gateway-api.ts:<line>` is the reference implementation the shipped host used.
"Renderer?" = the command appears in `COORDINATOR_METHOD_TABLE`
(`source/shared/rpc/coordinator.ts:92-183`), i.e. the renderer can call it directly.

#### A. Roster (agents / coworkers) — 17 commands

| Command | proto line | api line | Renderer? | What a backend must do |
|---|---|---|---|---|
| `listAgents` | 23 | 286 | ✅ `array` | Return the **full roster array** of agent summaries (§8). No args. |
| `countAgents` | 24 | 287 | ✅ `count` | Return a **number** — agents on disk. Drives the onboarding gate. |
| `searchAgents` | 25 | 288 | ✅ `array` | `{query, limit}` → array. Returns `[]` when global search is off. |
| `searchMedia` | 26 | 292 | ✅ `array` | `{query, limit}` → array. `[]` when search is off. |
| `createAgent` | 27 | 296 | ✅ `record` | `{name, description, title?, avatarShape?, avatarColor?, origin, isIntroductionSuppressed?, isKickstartRequested?, purpose?, templateId?, clientNonce}` → `{agent, transcript}`. **Must dedupe by `clientNonce`.** |
| `createGroup` | 30 | 317 | ✅ `record` | `{name, description, memberAgentIds}` → `{agent, transcript}`-shaped. |
| `setGroupMembers` | 31 | 322 | ✅ `record-or-null` | `{id, memberAgentIds}` → updated summary or `null`. |
| `updateAgent` | 32 | 324 | ✅ `record-or-null` | `{id, profile:{name, description, …}}` → updated summary or `null`. |
| `deleteAgent` | 33 | 326 | ❌ | `{id}`. Also cascades: sharing, schedules, box release, permissions. |
| `deleteAgents` | 34 | 341 | ✅ `record` | `{ids: string[]}`. The renderer deletes via this one, not `deleteAgent`. |
| `duplicateAgent` | 35 | 360 | ✅ `record` | `{id}` → `{agent, transcript}`. |
| `setAgentUnread` | 36 | 361 | ✅ `void` | `{id, isUnread, atMs}`. |
| `setAgentNotificationsEnabled` | 37 | 363 | ✅ `void` | **The shipped host returns `undefined` and does nothing.** Keep it a no-op. |
| `setAgentNotifyOnUpdates` | 38 | 364 | ✅ `void` | `{id, isEnabled}`. |
| `setAgentHiddenFromSidebar` | 39 | 366 | ✅ `void` | `{id, isHidden}`. |
| `setAgentAvatarBytes` | 91 | 552 | ✅ `record-or-null` | `{id, pngBase64 \| null}` → updated summary or `null`. |
| `getAgentAvatar` | 92 | 559 | ✅ `record` | `{id}` → `{dataUrl, version}`. Also backs `GET /avatars/<id>` (`gateway-server.ts:42`). |

#### B. Transcript reads — 8 commands

| Command | proto line | api line | Renderer? | Shape |
|---|---|---|---|---|
| `getTranscript` | 5 | 182 | ❌ | no args → the *active* transcript (`manager.ensureLoaded()`). |
| `getAgentTranscript` | 6 | 183 | ❌ | `{id}` → **entry array** (whole transcript, unbounded). |
| `getAgentTranscriptPage` | 7 | 185 | ❌ | `{id, beforeSeq?, sinceMs?, untilMs, limit}` → `{entries, nextBeforeSeq?}`. |
| `getAgentTranscriptWindow` | 9 | 187 | ✅ `transcript-window` | `{id, beforeSeq?, limit?}` → `{entries, nextBeforeSeq?, threadCounts}`. **Validated** (`source/shared/rpc/coordinator.ts:40-52`) — a missing `threadCounts` object makes the reply malformed and the call rejects. |
| `getAgentTranscriptTail` | 11 | 192 | ✅ `transcript-page` | `{id, beforeSeq?, limit?}` → `{entries, nextBeforeSeq?}`. |
| `openAgentWindowed` | 8 | 369 | ❌ | `{id, limit}` → window, **and** marks the agent active. |
| `openAgentTail` | 10 | 370 | ✅ `transcript-page` | `{id, limit}` → `{entries, nextBeforeSeq?}`, **and** marks the agent active + emits a roster update. This is what the renderer calls when you click a coworker (`frontend/src/production/ProductionRenderer.tsx:2262`). |
| `getAgentThread` | 12 | 194 | ✅ `agent-thread` | `{id, rootId}` → `{entries}`. Malformed request throws (`host-gateway-api.ts:196`). |

Paging semantics (`source/host/extensions/session/agent-db-transcript-pages.ts:10-20`):
rows are read newest-first `limit+1`, sliced to `limit`, then **reversed** so the array is
oldest→newest; `nextBeforeSeq` is the `seq` of the **oldest returned row** and is present
**only when more rows exist**. Default limit 500, hard cap 5000.

#### C. Sending & delivery — 3 commands

| Command | proto line | api line | Renderer? | Shape |
|---|---|---|---|---|
| `sendPrompt` | 13 | 200 | ✅ `send-result` | see below |
| `promptAcceptanceStatus` | 14 | 231 | ✅ `acceptance-lookup` | `{accountSlot, clientNonce}` → `{outcome:"found", record}` \| `{outcome:"unknown-durability"\|"not-found"}` (`source/host/extensions/transcript/prompt-acceptance-ledger.ts:225-238`) |
| `broadcastToAgents` | 65 | 447 | ✅ `record` | `{targets: "all"\|string[], message}` → `{total, scheduled}` |

**`sendPrompt` request** — exactly what the renderer sends
(`frontend/src/production/ProductionRenderer.tsx:1013-1030`), destructured by
`host-gateway-api.ts:200-230`:

```jsonc
{
  "agentId": "…",                 // optional; falls back to active agent
  "prompt": "…",                  // plain text
  "directAddressedAcceptance": true,
  "attachmentPaths": ["…"],       // parallel arrays
  "attachmentNames": ["…"],
  "attachments": [                // optimistic durable rows, kind "user-attachment"
    { "kind":"user-attachment", "id":"…", "file_path":"…", "file_name":"…" }
  ],
  "clientNonce": "uuid",          // REQUIRED for retry-safety
  "enterEpochMs": 1730000000000,
  "composedAtMs": 1730000000000,
  "richText": "<tiptap json string>",   // optional
  "replyToId": "entry-id",              // optional
  "isFork": false,                      // optional
  "traceparent": "00-…"                 // optional
}
```

**`sendPrompt` response is `{"accepted": true}`** (`host-gateway-api.ts:229`) — returned
**as soon as the turn is accepted**, not when it completes. The client uses that to prove
the endpoint is dedupe-capable (`gateway-client.ts:383`) and will only retry a send
against a base URL that has already answered `accepted:true`. Getting this wrong causes
either duplicate sends or a stuck composer.

Idempotency contract (`prompt-acceptance-ledger.ts:34-49, 245-254`): the ledger keys on
`(accountSlot, clientNonce)` and stores an `inputDigest` over
`[agentId, prompt, richText, replyToId, isFork, attachmentPaths, attachmentNames]`.
A repeat nonce with a **different** digest must throw
`NONCE_DIGEST_MISMATCH` (`send/nonce-digest-mismatch` on the renderer side,
`frontend/src/recovered/runtime/coordinator-source.ts:32`).

#### D. Widgets, approvals, reactions, entry mutation — 8 commands

| Command | proto line | api line | Renderer? | Shape |
|---|---|---|---|---|
| `respondToWidget` | 15 | 233 | ✅ `record-or-null` | `{entryId, value, agentId}` |
| `dismissWidget` | 18 | 255 | ✅ `record` | `{entryId, agentId, …}` |
| `resolveAutoReviewApproval` | 16 | 244 | ✅ `void` | `{requestId, status, …}`; stale ⇒ throw `auto-review/stale` (`source/shared/transcript.ts:1-2`) |
| `resolveLocalToolPermission` | 17 | 251 | ✅ `void` | `{requestId, status, …}` |
| `submitSecret` | 19 | 262 | ✅ `void` | `{entryId, value, agentId}` |
| `reactToMessage` | 20 | 268 | ✅ `void` | `{entryId, emoji, agentId}`; `by` is `"me"` or `"agent"` (`source/shared/transcript.ts:18-19`) |
| `deleteTranscriptEntries` | 21 | 279 | ✅ `record` | `{agentId, ids…}` → emits `{type:"removed", id}` per entry |
| `appendConnectorCard` | 22 | 283 | ❌ | host-internal card append |

Approval settlement helpers the host runs on the durable entry:
`settlePendingAutoReviewApprovalEntry` (`source/shared/transcript.ts:6-10`) flips
`entry.message.approval.status` from `"pending"`; `settlePendingLocalToolPermissionEntry`
(`:12-16`) flips `entry.message.ask.status`. Both refuse to act unless
`entry.kind === "send-message"` and `entry.message.type` matches.

#### E. Memories & automations & workflows — 21 commands

| Command | proto | api | Renderer? |
|---|---|---|---|
| `getAgentMemories` | 42 | 374 | ❌ |
| `deleteAgentMemory` | 43 | 376 | ❌ |
| `clearAgentMemories` | 44 | 378 | ❌ |
| `getAgentAutomations` | 45 | 380 | ✅ `array` |
| `listAllAutomations` | 46 | 382 | ✅ `array` |
| `setAgentAutomationEnabled` | 60 | 408 | ✅ `array` |
| `createAgentAutomation` | 61 | 414 | ✅ `array` |
| `updateAgentAutomation` | 62 | 432 | ✅ `array` |
| `deleteAgentAutomation` | 63 | 438 | ✅ `array` |
| `runAgentAutomationNow` | 64 | 440 | ✅ `void` |
| `getAgentWorkflows` | 66 | 461 | ✅ `array` |
| `createAgentWorkflow` | 67 | 463 | ✅ `array` |
| `updateAgentWorkflow` | 68 | 490 | ✅ `array` |
| `setAgentWorkflowEnabled` | 69 | 496 | ✅ `array` |
| `deleteAgentWorkflow` | 70 | 502 | ✅ `array` |
| `runAgentWorkflowNow` | 71 | 504 | ✅ `void` |
| `importAgentWorkflowText` | 72 | 506 | ✅ `import-result` |
| `importAgentWorkflowUrl` | 73 | 512 | ✅ `import-result` |
| `portAgentLocalSkills` | 74 | 514 | ✅ `import-result` |
| `getConversationOutline` | 75 | 516 | ✅ `array` |
| `getSubagents` / `getAsyncTasks` | 89 / 90 | 550 / 551 | ✅ `array` |

Note the mutating automation/workflow commands **return the new full array**, not void.
Memory commands are **not** in the renderer method table — they are reached through
another surface (not found in tree: which one; `source/host/extensions/memory/` emits the
`memory` SSE channel, `sand-host.ts:939`).

#### F. Sharing / rooms — 10 commands

`getSharingState` (50/390), `createRoomFromAgent` (51/391), `createRoomInvite` (52/393),
`joinSharedRoom` (53/395), `respondToRoomJoinRequest` (54/396), `createSharedRoom` (55/398),
`addOwnAgentToSharedRoom` (56/400), `removeOwnAgentFromSharedRoom` (57/402),
`setSharedRoomTyping` (58/404), `leaveSharedRoom` (59/406). All in the renderer table.
`getSharingState` is called by `frontend/src/recovered/features/agent-info/shared-room/bridge.ts:23`
and **must return the sharing state** — `requireState` projects it through
`projectSharingState` (`shared-room/model.ts:36`): `isEnabled` boolean, `selfAuthId` string or
null, `pendingJoinRequests`/`rooms`/`typingUsers` arrays; anything else throws "Sharing returned
a malformed state". The disabled state the host itself emits is `EMPTY_SAND_SHARING_STATE`
(`shared/agents/sharing.ts:43`), which is what we answer for it and for the four other verbs the
bridge projects the same way. `createRoomFromAgent`, `createRoomInvite`, `joinSharedRoom` and
`createSharedRoom` get the host's disabled reply `{status: "error", message: "Sharing isn't
enabled for your account."}`; `setSharedRoomTyping` gets nothing. `gateway/routes.rs`.

#### G. Settings, secrets, capabilities, feature gates — 9 commands

| Command | proto | api | Renderer? | Notes |
|---|---|---|---|---|
| `getHostSettings` | 118 | 638 | ❌ (main only) | see §9 for the exact record |
| `setHostSettings` | 119 | 639 | ❌ (main only) | **returns the full settings record** (`settings-service.ts:56`) |
| `setBoxSecrets` | 120 | 682 | ❌ | `{secrets}` |
| `getBoxSecretsStatus` | 121 | 684 | ✅ `box-secrets` | |
| `isAgentNetworkEnabled` | 47 | 383 | ✅ `boolean` | renderer gates the org-chart/agent-network UI on it |
| `isGlobalSearchEnabled` | 48 | 385 | ✅ `boolean` | gates search |
| `isEgressTunnelAvailable` | 49 | 387 | ✅ `boolean` | shipped host: `process.env.SAND_EGRESS_TUNNEL_ENABLED === "1"` |
| `getHostStatus` | 103 | 594 | ❌ | `{…versionState, isBusy, capabilities}` where `capabilities = ["orderedReplicasV1","sendAcceptanceV1"]` (`host-gateway-api.ts:8-11`) |
| `updateHostNow` | 102 | 592 | ❌ | host self-update |

#### H. Box lifecycle / computer — 15 commands

`getForeverBoxStatus` (93/561), `getCloudAgentInfo` (94/565), `ensureForeverBox` (95/570),
`resetForeverBox` (96/574), `updateForeverBox` (97/578), `autoUpdateBoxNow` (98/582),
`snapshotBoxStoreNow` (99/584), `getBoxStoreStatus` (100/588), `clearBoxStoreNow` (101/590),
`setBoxMigrating` (104/599), `prepareBoxForRecreate` (105/605), `resumeBoxAfterRecreate` (106/609),
`handBackForeverBox` (107/617), `kickstartAgent` (28/311), `requestDiskSaverAudit` (29/314).

**Box status shape** (`source/host/extensions/forever-box/host-box.ts:6`):
`{ agentId: string, state: string, vncUrl: string|null, windows?: [{windowIndex, vncUrl}],
imageUpdateAvailable?: boolean, pull?: {percent} }`. The renderer **validates** it:
`frontend/src/production/coordinator-client.ts:43-47` throws unless the reply is `null`
**or** has string `agentId` **and** string `state`. Observed states: `"running"`,
`"absent"` (`host-box.ts:15,19,24`).

#### I. Channels, listeners, MCP, skills, trays, teach, attachments — 24 commands

- **Channels:** `getAgentChannels` (83/533), `connectChannel` (84/535),
  `disconnectChannel` (85/539), `refreshChannel` (86/543) — all reply `channels-view`.
- **Listeners:** `getListenerIntegrations` (87/545) → record;
  `getListenerConnectUrl` (88/547) → `{url}`.
- **MCP:** `completeMcpOAuth` (122/679 — shipped host returns `undefined`),
  `requestWebAuthnCeremony` (123/680), `refreshMcp` (124/652 — multiplexed:
  `routedAction:"list-tools"|"execute-tool"`, or `completion`, or restart),
  `listRoutedMcpTools` (125/661), `executeRoutedMcpTool` (126/669),
  `listBoxMcpServers` (127/663) → `{servers:[{serverIdentifier, status, statusDetail?, toolCount}]}`.
- **Skills:** `skillsCatalog` (76/519), `syncPluginSkills` (77/520),
  `getPluginSyncStatus` (78/522), `getSkillPublishTargets` (79/524),
  `publishSkill` (80/526), `resyncPublishedSkill` (81/528), `unpublishSkill` (82/530).
- **Trays:** `getTrays` (111/629) → **array**, `dismissTray` (112/630), `clearTrays` (113/632).
- **Teach recording:** `startTeachRecording` (108/623), `stopTeachRecording` (109/625),
  `getTeachRecordingStatus` (110/627).
- **Attachments:** `uploadAttachment` (114/634), `readAttachmentImage` (115/635),
  `readAttachmentText` (116/636), `readAttachmentChunk` (117/637).

#### J. Session/window misc — 2 commands

`openAgent` (40/368 — switch the active agent, returns the transcript),
`setWindowFocused` (41/371 — `{isFocused}`).

### 2.2 The slim-avatar variant — do not skip this

`SAND_GATEWAY_SLIM_COMMANDS` (`gateway-protocol.ts:137-146`) is selected when the request
carries `x-sand-slim-avatars: 1` (`gateway-server.ts:18,36`). The **coordinator always
sends it** unless `SAND_DISABLE_SLIM_AVATARS=1` (`gateway-client.ts:140,145`).

In slim mode the server must set `avatarDataUrl: null` on every summary it returns from
`listAgents`, `updateAgent`, `setGroupMembers`, `setAgentAvatarBytes`, and on
`result.agent` for `createAgent` / `createGroup` / `duplicateAgent`
(`gateway-protocol.ts:133-136`). It must do the same for SSE events on the `agents` and
`agent-upserted` channels (`gateway-protocol.ts:147-151`, `stripInlineAvatarsFromEvent`).
The renderer then loads faces through `GET /avatars/<agentId>?v=<avatarVersion>`.

So: **`avatarVersion` must be non-null whenever an avatar exists**, or the avatar route
404s (`gateway-server.ts:42`, "agent has no avatar").

### 2.3 MINIMAL SUBSET (a) — boot and show a roster of coworkers

Ordered by when the client asks. `⚠` = the app visibly breaks without it.

| # | Command / endpoint | Called by | Must return |
|---|---|---|---|
| 1 ⚠ | `GET /health` | `SandHostSupervisor.ensureConnection` → `fetchHealth`, `host-supervisor.ts:49` | `{ok:true, …}` within 1500 ms |
| 2 ⚠ | `GET /events` (SSE) | `CoordinatorGatewayClient.streamEvents`, `gateway-client.ts:427` | 200 + `text/event-stream`, `retry:`, then heartbeats ≤ 15 s |
| 3 ⚠ | `listAgents` | coordinator `seedAgentsRosterToMain`, `source/node-agent-coordinator/main.ts:151`; renderer `refreshRoster`, `ProductionRenderer.tsx:2185` | **JSON array** of summaries (§8) |
| 4 | `setHostSettings` ×7 + `getHostSettings` ×1 | `createCoordinatorResyncChain().onTransportConnected()`, `source/electron-main/coordinator/coordinator-resync.ts:7-8` (the `runOnce` step list) | any record; failures are caught per-step but each one is a logged failure |
| 5 | `getBoxSecretsStatus`, `setWindowFocused` | same resync chain (`box_secrets`, `window_focus` steps) | any record / void |
| 6 | `countAgents` | `resolveOnboarding`, `ProductionRenderer.tsx:2154` | **number** ≥ 0 (else the app shows onboarding) |
| 7 | `isAgentNetworkEnabled` | `refreshAgentNetworkAvailability`, `ProductionRenderer.tsx:2232` | boolean |
| 8 | `getTrays` | `notification-host.tsx:131,249` | **array** |
| 9 | `getForeverBoxStatus` | computer surface | `null` **or** `{agentId, state, …}` |

**That is the whole boot set: `/health`, `/events`, `listAgents`, `countAgents`,
`getTrays`, `isAgentNetworkEnabled`, `getHostSettings`, `setHostSettings`.**
Everything else can 404/500 on first boot without stopping the roster from painting —
but see §10 Trap 3 about *which* failures are silent.

### 2.4 MINIMAL SUBSET (b) — one message sent and answered

On top of (a):

| # | Step | Command / event | Notes |
|---|---|---|---|
| 1 | user clicks a coworker | `openAgentTail {id, limit:200}` | reply `{entries, nextBeforeSeq?}`; validated at `coordinator-client.ts:37-41` |
| 2 | user hits Enter | `sendPrompt {…, clientNonce}` | reply **`{accepted:true}`** |
| 3 | echo the user's own message | SSE `transcript` `{type:"appended", entry:{kind:"message", role:"user", …, clientNonce}}` | the renderer matches on `clientNonce` to settle the optimistic bubble (`ProductionRenderer.tsx:1010`) |
| 4 | show "thinking" | SSE `agent-upserted` with `agent.isRunning:true` and `agent.currentActivity:{kind:"thinking"}` | §4 |
| 5 | stream the answer | SSE `transcript` `{type:"appended", entry:{kind:"send-message", message:{type:"text", content:""}, streaming:true}}` then `{type:"updated", entry:{…content grows…}}` then a final `updated` with `streaming` absent/false | §3 |
| 6 | stop "thinking" | SSE `agent-upserted` with `isRunning:false`, `currentActivity` absent | |
| 7 | keep the sidebar honest | SSE `agent-upserted` with new `lastMessageId`, `lastMessagePreview`, `lastEntry`, `lastActivityAt`, `hasUnread` | §8 |

Optional but cheap: `promptAcceptanceStatus` so a reconnect mid-send can resolve.

---

## 3. Transcript model

### 3.1 Durable entry kinds (what OpenGrok stores and ships)

Threadable kinds are enumerated at `source/shared/transcript.ts:21-22`:
`message | send-message | user-attachment | notice`. Two more kinds appear on the wire.

| `kind` | Fields | What the UI draws | v1? |
|---|---|---|---|
| `message` | `id`, `role:"user"\|"assistant"`, `content`, `richText?`, `isStreaming`, `timestampMs`, `replyTo?`, `batchId?`, `branched?`, `clientNonce?`, `sentWhileOfflineAtMs?`, plus optional `fromAgent`/`toAgent` peer hops | the ordinary chat bubble (`TranscriptMessage`, `frontend/…/workspace/model.ts:239-274`) | **ESSENTIAL** |
| `send-message` | `id`, `message:{type, …}` (12 card types, §3.2), `timestampMs`, `replyTo?`, `streaming?`, `respondedValue?`, `widgetDismissed?`, `widgetSkipped?`, `respondedValueEchoed?`, `draftSendState?`, `secretProvided?`, `boxInstruction?`, `boxRequest?`, `boxRequestId?`, `boxResolution?`, `boxSnapshot?`, `reactions?`, `permissionScope?`, `permissionScopeRevision?` | every assistant utterance and every card | **ESSENTIAL** |
| `user-attachment` | `id`, `file_path`, `file_name?`, `width?`, `height?`, `byteSize?`, `batchId?`, `clientNonce?`, `replyTo?`, `branched?`, `timestampMs` | the file/image chips on a user row | optional v1 |
| `notice` | `id`, `text` (+ `timestampMs`) | a muted system line (`TranscriptNotice`, `model.ts:299-304`) | optional v1 |
| `event` | `event: {type, …}` — `name-changed` / `channel-connected` / `channel-disconnected` / `automation-changed` (`source/shared/sand-timeline-events.ts:9-18`) | "Renamed to X" timeline row | optional v1 |
| `tool-call` (also `toolCall`/`tool`) | `name`, `status`, `summary?`, `args` | the tool line | optional v1 |

Constructors, verbatim (`source/host/extensions/transcript/send-message-shaping.ts`):
- `createUserMessage` → line **77-103**, emits `kind:"message"` at **line 86**.
- `createSendMessageEntry` → line **104-117**, emits `kind:"send-message"` at **line 111**;
  copies `message.reply_to` → entry `replyTo`.
- `createUserAttachmentEntry` → line **128-160**, emits `kind:"user-attachment"` at **line 151**.

Thread/branch derivation runs client-side on these fields
(`source/shared/transcript.ts:23-36`): a "thread" is the transitive closure of
`replyTo` among entries with `branched === true`; `getMainTranscriptEntries` filters
threaded children out of the main column. **`threadCounts` on
`getAgentTranscriptWindow` is the per-root count.**

Peer-message helpers (`source/shared/transcript.ts:38-41`): a `kind:"message"` entry with
`fromAgent` or `toAgent` is an agent↔agent hop; an outbound hop whose
`toAgent.kind !== "agent"` is **hidden**; peer messages do **not** raise a user-activity
signal (so they must not light the unread pip).

### 3.2 `send-message` card types (`message.type`)

Canonical list at
`frontend/src/recovered/features/conversation/cards/transcript-card/protocol.ts:9-22`
(`TRANSCRIPT_CARD_ENTRY_TYPES`). The projector at `:410-495` **rejects the whole entry**
if any field fails its check, and rejected entries are silently dropped
(`projectTranscriptCardEntries`, `:497-502` — it returns a `rejectedCount` and no error).

| `message.type` | Required fields | Optional fields | What the UI draws | v1 |
|---|---|---|---|---|
| `text` | `content: string` | `images[]`, `channel` | the assistant bubble; `streaming:true` renders as a live/typing bubble | **ESSENTIAL** |
| `widget` | `widget.prompt` (non-empty), `widget.options` (**1–6** items, each with non-empty `label`) | `widget.helpText`, `option.value`, `option.description`, `option.style ∈ {default,primary,danger}`, `widget.allowCustom`, `widget.dismissOnMoveOn` | a choice card; answered via `respondToWidget` | optional |
| `cursor-agent` | `bcId` (non-empty) | `title` | cloud-agent card | optional |
| `email-draft` | `draft.to: string[]`, `draft.subject: string`, `draft.body: string` | `draft.from`, `draft.cc` | editable email draft | optional |
| `slack-draft` | `draft.target` (non-empty), `draft.body: string` | `draft.workspace`, `draft.thread` | editable Slack draft | optional |
| `auto-review-approval` | `approval.requestId` (non-empty), `approval.summary: string`, `approval.status ∈ {pending,approved,always,denied,expired}` | `approval.surface` (defaults `"unknown"`), `reason`, `command`, `proposedRule` | the approval card; answered via `resolveAutoReviewApproval` | optional |
| `listener-connect` | `platform ∈ {github, slack}` | `reason` | connect-a-listener card | optional |
| `secret-request` | `secretRequest.label` (non-empty) | `secretRequest.description` | secret prompt; answered via `submitSecret` | optional |
| `attachment` | `url` (non-empty) | `alt` | an assistant file/media chip | optional |
| `connector` | `connector` (non-empty) | `reason`, `serverId`, `suggestions[]`, `variant` | plugin-connect card | optional |
| `connectors` | `connectors: string[]` | — | multi-connect card | optional |
| `local-tool-permission` | `ask.requestId` (non-empty), `ask.status ∈ {pending,always,never,denied,expired,allow-once}` | `ask.action`, `ask.target` (untyped) | local-exec permission card; answered via `resolveLocalToolPermission` | optional |

`docs/grok-0.27-disparity-proto.md` §3.2 also records that host shaping emits a
**13th** type, `permission-request`, which the recovered renderer's
`TRANSCRIPT_CARD_ENTRY_TYPES` does **not** list — it appears instead as a first-class UI
kind (`TranscriptPermissionRequest`, `model.ts:352-358`). Treat `permission-request`
as real-but-undocumented; do not emit it in v1.

### 3.3 The UI-side union (for context, not for the wire)

`ConversationTranscriptEntry` at `frontend/…/workspace/model.ts:360` is
`message | tool-call | thinking | notice | timeline-event | time-separator |
unread-divider | computer-handoff | local-tool-permission | permission-request |
TranscriptCardEntry`. Note that `time-separator` and `unread-divider` are **renderer
inventions** — never send them over the wire.

`TranscriptToolCallStatus` = `pending | running | done | failed | error | aborted`
(`model.ts:276`).

---

## 4. Live activity — the second stream

The durable transcript answers "what happened". The **activity stream** answers "what is
happening right now", and it is a *different channel with a different vocabulary*.

### 4.1 Producer types

`source/host/sand-activity.ts`:

```ts
// :3, :6
AgentActivity = { kind: "thinking" }
              | { kind: "tool"; tool: string; detail?: string; target?: string; callId: string }

// :8
ActivityUpdate = { type: "thinking-delta" }
               | { type: "text-delta";  text: string }
               | { type: "tool-call";   id: string; name: string; status: string; args?: string; summary?: string }
               | { type: "send-message" }
               | { type: "turn-ended" }
               | { type: string; [k: string]: unknown }   // open union

// :7
ActivityTransition = { type: "keep" | "clear" } | { type: "set"; activity: AgentActivity }
```

### 4.2 The reduction rules (`deriveActivityFromUpdate`, `sand-activity.ts:10`)

| Update | Transition |
|---|---|
| `thinking-delta`, `text-delta` | `set` → `{kind:"thinking"}` |
| `send-message`, `turn-ended` | `clear` |
| `tool-call` where `name === "SendMessage"` | `keep` (never shows as a tool) |
| `tool-call` where `status !== "pending"` | `keep` |
| `tool-call` where name ∈ `{shellToolCall, readToolCall, awaitToolCall}` | `keep` (unresolved, `:9`) |
| any other `tool-call` | `set` → `deriveToolCallActivity(update)` |
| anything else | `keep` |

`deriveToolCallActivity` (`sand-activity.ts:22`) renames the raw tool name and extracts a
`detail`, clamped to **80 chars** (`MAX_ACTIVITY_DETAIL_CHARS`, `:2`):

| raw `name` | emitted `tool` | `detail` extracted from |
|---|---|---|
| `shellToolCall`, `Shell` | `Shell` | redirect/tee/sed target of `args.command` (`extractShellEditTarget`, `:21`) |
| `ExternalShell` | `ExternalShell` | same |
| `readToolCall`, `Read` | `Read` | basename of `args.path` |
| `ExternalRead` | `ExternalRead` | basename of `args.path` |
| `webSearchToolCall` | `WebSearch` | `args.searchTerm` |
| `webFetchToolCall` | `WebFetch` | hostname of `args.url` |
| `generateImageToolCall` | `GenerateImage` | `args.description` |
| `mcpToolCall` | `CallMcpTool` | `args.providerIdentifier ?? args.serverIdentifier` |
| `getMcpToolsToolCall` | `GetMcpTools` | `args.server` |
| `mcpAuthToolCall` | `McpAuth` | `args.serverIdentifier` |
| `awaitToolCall`, `AwaitShell` | `AwaitShell` | — |
| `computerUseToolCall` | `Computer` | — |
| `Task` | `Task` | `update.summary` |
| `communicateUpdateToolCall` | from `args.currentStep` JSON when `__sand_tool__ === true` | `.detail`, `.target` |

A **hold** keeps a named tool activity visible for at least
`NAMED_ACTIVITY_MAX_HOLD_MS = 2500` ms before thinking can overwrite it
(`resolveNamedActivityHold`, `:13`).

### 4.3 How it reaches the renderer

**Not** as its own SSE channel. The reduced `AgentActivity` is written onto the roster row
and shipped as `agent-upserted`. `withRunStates`
(`source/host/extensions/transcript/run-lifecycle.ts:300-333`) stamps every summary with:

```
isRunning, isRunningTurn, isComposingMessage, isRetrying,
currentActivity: AgentActivity | undefined,
activeRemoteMemberId
```

and returns the *same object identity* when nothing changed (`:317-323`) so the emitter
can skip. `RosterEmit.runEmitAgentUpdate` (`roster-emit.ts:152-207`) then emits
`agent-upserted`; if a *sibling* row also changed it escalates to a full `agents` emit
(`:180-186`).

So the answer to "how does the renderer learn a coworker is thinking?" is:
**`agent-upserted` events carrying `isRunning:true` + `currentActivity`.**

Renderer-side verbs (`frontend/…/conversation/activity/agent-activity.ts`, catalogued in
`docs/grok-0.27-disparity-proto.md` §7): `thinking, searching, browsing, reading,
connecting, writing, coding, generating, running-commands, on-its-computer,
on-your-computer, working, messaging, sending, waiting`.

### 4.4 SSE channels — the complete list

> **Corrected 30 Aug 2026: there are 21, not 18.** The table below is missing three that the
> family map carries — `agents-automation`, `agents-workflow`, `mcp-servers-updated`. The
> `memory` and `mcp-oauth-pending` rows are correct and deliberately outside
> `gateway-event-families.ts`; do not delete them when reconciling.

`source/node-agent-coordinator/gateway/gateway-event-families.ts:1-19` maps coordinator
event families to SSE channel names. The channel names OpenGrok may emit:

| Channel | Payload | Emitted at |
|---|---|---|
| `transcript` | `{type, …}` — see §4.5 | `sand-host.ts:891` |
| `agents` | `{activeAgentId, agents: Summary[], ordered, coverage:{kind:"complete-roster"}}` | `sand-host.ts:908`, built at `roster-emit.ts:118-124` |
| `agent-upserted` | `{activeAgentId, agent: Summary, ordered}` | `sand-host.ts:922`, built at `roster-emit.ts:190-196` |
| `client-side-tool-v2` | tool relay frames | `sand-host.ts:894` |
| `outline` | `{type:"snapshot", agentId, items}` / `appended` / `updated` | `roster-projection.ts:275,283,293,499` |
| `subagents` | `{parentAgentId, …}` | `roster-projection.ts:534` |
| `async-tasks` | `{parentAgentId, tasks}` | `roster-projection.ts:511,518` |
| `automations` | automation snapshot | `sand-host.ts:925-934` channel table |
| `workflows` | workflow snapshot | same |
| `memory` | memory change | `sand-host.ts:939` |
| `tray` | tray change | `sand-host.ts:942` |
| `forever-box` | a `BoxStatus` (decorated) | `sand-host.ts:840,946` |
| `teach-recording` | recording status | `sand-host.ts:951` |
| `box-disk-pressure` | disk pressure | `sand-host.ts:607,624` |
| `computer-action` | computer action | `host-runner-composition.ts:1049` |
| `mcp-servers` | MCP server list | `sand-host.ts:431` |
| `sharing` | sharing state | `sand-host.ts:434` |
| `host-settings` | settings change | `sand-host.ts:437` |
| `mcp-oauth-pending` | `{serverName, redirectUrl, state}` | consumed at `source/node-agent-coordinator/main.ts:125-129`; **not** in the family map — it is handled specially |

### 4.5 The `transcript` channel event union

Emitted through `RosterProjection.emit` (`roster-projection.ts:460-467`):

| `type` | Payload | Emitted at |
|---|---|---|
| `snapshot` | `{type:"snapshot", activeAgentId, entries, ordered, coverage:{kind:"transcript-live-range", fromSequence:1, throughSequence}}` | `agent-lifecycle.ts:304-308, 428-432`; coverage added at `roster-projection.ts:469-486` |
| `appended` | `{type:"appended", entry, agentId, ordered}` | `send-acceptance.ts:129`, `group-chat-glue.ts:651`, `session-runtime.ts:352` |
| `updated` | `{type:"updated", entry, agentId, ordered}` | `turn-runtime.ts:263`, `roster-projection.ts:90,381`, `box-request-entries.ts:74` |
| `removed` | `{type:"removed", id, agentId, ordered}` | `entry-deletion.ts:87` |
| `cleared` | `{type:"cleared"}` | `agent-lifecycle.ts:54,446,455` |

**Ordering stamps.** Every non-`cleared` transcript event and every roster event carries
`ordered: {replicaKey, epoch, sequence}` (`replica-writer.ts:3-7,31-34`). Replica keys are
`"roster"` and `` `transcript:${agentId}` `` (`source/shared/ordering.ts:1-5`). `epoch` is a
per-process UUID; `sequence` is monotonic per key. The host advertises support via the
`orderedReplicasV1` capability (`host-gateway-api.ts:9`). **Emit these.** A client that
sees a sequence gap or an epoch change treats the replica as stale.

---

## 5. The box seam — where a different sandbox provider plugs in

This is the host's *downstream* boundary: how the host reaches the agent's computer.
OpenGrok owns both sides of it, so it can be replaced wholesale — but the shapes below
tell you what capability set the rest of the host expects.

### 5.1 `BoxEndpoint` — the address of a computer

Declared **twice**, identically (transcription artifact):
`source/host/box/box-remote-accessor.ts:9-14` and `source/host/box/loopback-sand-box.ts:20`.

```ts
interface BoxEndpoint {
  readonly host: string;                                  // e.g. "127.0.0.1"
  readonly port: number;                                  // EXEC_DAEMON_PORT = 1337
  readonly authToken: string;                             // DEFAULT_AUTH_TOKEN = "local"
  readonly headers?: Readonly<Record<string, string>>;    // display / owner headers for fork windows
}
```

Constants: `EXEC_DAEMON_PORT = 1337`, `VNC_PORT = SAND_BOX_PRIMARY_NOVNC_PORT`,
`DEFAULT_AUTH_TOKEN = "local"`, `BOX_TERMINALS_FOLDER = "/root/.cursor/projects/workspace/terminals"`,
`DAEMON_READY_TIMEOUT_MS = 90_000`, `DAEMON_WATCHDOG_INTERVAL_MS = 30_000`
(`loopback-sand-box.ts:12-17`).

### 5.2 The transport (`box-remote-accessor.ts`)

```ts
// :151-161
createBoxTransport(endpoint, factory) => factory({
  httpVersion: "1.1",
  baseUrl: `http://${endpoint.host}:${endpoint.port}`,
  useBinaryFormat: true,                 // ConnectRPC binary
  interceptors: [createBoxAuthorizationInterceptor(endpoint)]
})

// :139-149  the interceptor sets:
//   Authorization: Bearer <endpoint.authToken>
//   …plus every entry of endpoint.headers verbatim
```

Ping classification (`:163-177`): Connect code `4` or `/deadline/i` ⇒ `"timeout"`;
`ECONNREFUSED` ⇒ `"refused"`; anything else ⇒ `"crash"`.
`causeSummary` = `<ConnectCodeName>` or `<ConnectCodeName>/<errno>`.

Exec streaming (`:244-269`, `BoxRemoteExecManager`): the manager assigns monotonic ids,
calls `client.exec(ctx, serialize(id))`, and yields `execClientMessage` values.
An `execClientControlMessage` whose `message.case === "throw"` becomes a thrown `Error`
with the remote `stackTrace` attached. Control cases are
`streamClose | throw | heartbeat` (`:55-73`).

### 5.3 `LoopbackOperations` — the substitution point

`source/host/box/loopback-sand-box.ts:24`. **This is the interface a different sandbox
provider implements.**

```ts
interface LoopbackOperations<Accessor extends ShellAccessor> {
  ping(ctx, endpoint): Promise<PingResult>;                    // {outcome, causeSummary?}
  createRemoteAccessor(endpoint): Accessor;
  protectRemoteAccessor(accessor, assertFileReadAllowed): Accessor;
  applyEnvironment?(ctx, endpoint, update: BoxEnvironmentUpdate): Promise<void>;
  loadMcpServers?(ctx, endpoint, configJson: string): Promise<string[]>;
  uploadFile?(ctx, accessor, path, data: Uint8Array): Promise<void>;
  sleep?(ms, signal?): Promise<void>;
  now?(): number;
}
```

`LoopbackSandBox` (`:28-…`) is the concrete box built from those operations. Its public
surface — what the rest of the host calls — is:

| Method | Returns | Line |
|---|---|---|
| `describe()` | `{backend:"loopback"}` | 35 |
| `getTerminalsFolder()` | the terminals path | 35 |
| `isAvailable()` | `true` | 35 |
| `primaryEndpoint()` | `{host, port:1337, authToken}` | 36 |
| `ensureReady(ctx, agentId)` | `{remoteAccessor, vncUrl, terminalsFolder}` | 37 |
| `applyEnvironment(ctx, update)` | void | 38 |
| `loadMcpServers(ctx, configJson)` | `string[]` | 39 |
| `mcpResourceAccessor(ctx)` | accessor | 40 |
| `maxWindows()` | `SAND_BOX_MAX_WINDOWS` | 41 |
| `ensureWindow(ctx, agentId, windowIndex, {ownerToken?})` | `{windowIndex, computerUse, vncUrl}` | 42 |
| `releaseWindow` / `hibernate` / `runState` / `listBoxes` / `dispose` | — | 43-45 |
| `uploadFile` / `downloadFile` | — | 46-47 |
| `waitUntilReady(ctx, endpoint, timeoutMs)` | throws `SandBoxDaemonUnreachableError` on timeout | 48-49 |

Capability probing is duck-typed, not inheritance:
`source/host/box/box-capabilities.ts` — `boxMaxWindows`, `boxSupportsMultiWindow`,
`boxAgentWindowIndex`, `boxTerminalsFolder`, `boxIsAvailable`, `boxIsPreparing`,
`boxDescription`, `boxApplyEnvironment` (throws `BoxEnvironmentSyncUnsupportedError`
when absent), `boxLoadMcpServers` / `boxMcpResourceAccessor` (throw `BoxMcpUnsupportedError`).

### 5.4 `box-factory.ts` — loopback vs. shared-desktop

`source/host/box/box-factory.ts` is four lines of composition:

- `createSandBox(options)` → `new LoopbackSandBox(options)`.
- `applySharedDesktop(box, options)` → wraps in `SharedDesktopSandBox` **only if**
  `boxSupportsMultiWindow(box)`, i.e. `maxWindows() > 1`.
- `formatSandBoxStartupSummary({autoUpdateEnabled, isPackaged})` emits the log line
  `[sand-host] agent box backend: loopback (in-box); image: host's own container; …`

**There is no `RemoteSandBox` class.** The name "loopback" is literal: the shipped host
runs *inside* the box and dials `127.0.0.1:1337`. `source/host/box/production.ts:36-51`
defines `ProductionBoxGeneratedPorts` — the erased ConnectRPC constructors
(`createTransport`, `createControlClient`, `createExecClient`, `createResourceAccessor`,
`withFileReadGuard`, `withNoMonitorComputerUse`) that turn the generated stubs into a
`LoopbackOperations`. `production.ts:69-76` notes the reconstructed standalone daemon
deliberately does **not** advertise fork desktops or the 1339 router.

### 5.5 The exec daemon (the far side)

`source/box-exec-daemon/server.ts:10-70` serves two ConnectRPC services over
`connectNodeAdapter`:

- **`ControlService`**: `ping`, `getCapabilities`, `updateEnvironmentVariables`
  (and `loadMcpServers` per `LoadMcpServersResponse`).
- **`ExecService`**: `exec` — bidi stream of `ExecServerMessage` ↔ `ExecStreamElement`,
  carrying shell / read / background-shell / stdin work
  (`shell_exec_pb`, `read_exec_pb`, `background_shell_exec_pb`).

Shell invocation shape: `buildHostShellArgs({command, name, workingDirectory, toolCallId})`
(`source/host/box/box-shell-command.ts`) constructs a `ShellArgs` with
`skipApproval: true` and a single `executableCommand`.

`ShellAccessor` is minimal (`source/host/box/box-windows.ts:22`):
`{ get(resource: typeof shellExecutorResource): ShellExecutor }`.

Guardrails you must keep: `assertPathOutsideProtectedRoots`
(`source/host/box/protected-path-guard.ts`, called from `loopback-sand-box.ts:34`) and
`resolveBoxWorkspacePath` (`box-transfer.ts`).

Error vocabulary the UI already renders (`source/host/ports/box.ts:1-7`):
`SAND_BOX_NOT_READY_MESSAGE`, `SAND_BOX_NOT_RESPONDING_MESSAGE`,
`SAND_BOX_NO_MONITOR_AVAILABLE_MESSAGE`; classes `SandBoxDaemonUnreachableError`
(with `outcome`) and `SandBoxNoMonitorAvailableError`. The gateway maps these to
telemetry reasons `daemon_refused | daemon_timeout | daemon_crash | no_monitor`
(`source/host/gateway-command-error.ts`).

---

## 6. How the host finds its backend — the exact repoint

Two directions, do not confuse them.

### 6.1 Desktop → gateway (this is what you repoint at OpenGrok)

`source/electron-main/box/box-host-connector.ts:17-19`:

```ts
export const GATEWAY_URL_ENV           = "SAND_HOST_GATEWAY_URL";
export const GATEWAY_TOKEN_ENV         = "SAND_HOST_GATEWAY_TOKEN";
export const GATEWAY_NETWORK_TOKEN_ENV = "SAND_HOST_GATEWAY_NETWORK_TOKEN";
```

`createRemoteHostConnector` (`:156-161`):

```ts
if ((env[GATEWAY_URL_ENV]?.trim() ?? "").length > 0) return new EnvDescriptorHostConnector(env);
// …otherwise: BrokeredHostConnector → ensureSandBox() against api2.cursor.sh
```

`EnvDescriptorHostConnector.connect()` (`:149-153`) builds
`buildConnection(baseUrl, token, networkToken)` (`:53-61`) which yields:

```ts
{ baseUrl,                                              // required
  token?,                                               // → Authorization: Bearer <token>
  headers?: { "x-anyrun-network-token": networkToken },  // only if networkToken non-empty
  vncProxy? }                                            // only in the brokered path
```

**So: `SAND_HOST_GATEWAY_URL=https://opengrok.example:PORT` (+ optionally
`SAND_HOST_GATEWAY_TOKEN=…`) is the entire repoint.** Everything downstream —
`SandHostSupervisor` → `CoordinatorGatewayClient` → `POST {baseUrl}/api/<cmd>` and
`GET {baseUrl}/events` — follows.

Read the caveat in §10 Trap 1 before you set it to a loopback URL.

### 6.2 Gateway server bind configuration (what OpenGrok should mirror)

`source/host/gateway-config.ts:41-53`, `resolveGatewayServerConfig(env)`:

| Env var | Effect |
|---|---|
| `SAND_GATEWAY_BIND_HOST` | bind host; default `127.0.0.1` |
| `SAND_HOST_PORT` | bind port; default `0` (ephemeral) |
| `SAND_GATEWAY_TOKEN` | pins the bearer token; **its presence also forces auth on** |
| `SAND_GATEWAY_REQUIRE_AUTH` | `1`/`true`/`yes` forces auth on a loopback bind |
| `SAND_GATEWAY_TLS_CERT` + `SAND_GATEWAY_TLS_KEY` | both-or-neither; enables HTTPS |
| `SAND_DISABLE_GATEWAY_SSE_GZIP` | `1` disables SSE gzip (`gateway-server.ts:14`) |

Auth is **required** whenever the bind host is not loopback (`:49`). If no token is pinned,
one is generated as `randomBytes(32).toString("base64url")` (`:43`) — which means the
shipped host's token is only knowable by whoever launched it. OpenGrok should always pin
a token via `SAND_GATEWAY_TOKEN` and hand the same value to the client via
`SAND_HOST_GATEWAY_TOKEN`.

### 6.3 Desktop → Cursor backend (seam B)

> **Superseded 30 Aug 2026.** This section's "neutralise, do not implement" advice no longer
> reflects the plan: seam B is scheduled (roadmap slice 8). The minimum is small and known —
> two services, 18 methods, specified by the client's own `source/mock/`. See
> [`../PORT-PRIORITY.md`](../PORT-PRIORITY.md) §3. The neutralisation route below remains the
> right way to *test* the gateway before seam B exists.

`source/shared/node/cursor-token.ts:3,37-39`:

```ts
DEFAULT_CURSOR_BACKEND_URL = "https://api2.cursor.sh";
getConfiguredBackendUrl(env) = env.SAND_BACKEND_URL ?? env.CURSOR_API_BASE_URL ?? DEFAULT_CURSOR_BACKEND_URL
```

`SAND_BACKEND_URL` points every ConnectRPC client (`DashboardService`, `InferenceService`,
`AgentService`, `GrokBotService`) elsewhere. `getAuthClientId` (`:41-46`) auto-selects
`DEV_AUTH_CLIENT_ID` for `localhost` / `127.0.0.1` / `*.lclhst.build` / `dev-staging.cursor.sh`,
which unlocks the `devLogin` path (`source/electron-main/account/cursor-auth.ts:311`,
which *refuses* dev-login against a non-dev backend).

A working local stand-in already exists in this repo: `source/mock/` (see
`docs/0.27-mock-server.md`) — a ConnectRPC mock of `GrokBotService` on `127.0.0.1:8787`
with `/auth/cursor_dev_session_token`, `/oauth/token`, `/auth/poll` returning long-lived
unsigned JWTs. Use it (or an OpenGrok equivalent) so the app gets past sign-in; do **not**
make OpenGrok implement 600+ Dashboard RPCs.

Other relevant env knobs found in `source/`:
`SAND_DATA_ROOT`, `SAND_PACKAGED`, `SAND_HOST_IN_BOX`, `SAND_BOX_EXEC_DAEMON_PORT`,
`SAND_BOX_EXEC_DAEMON_AUTH_TOKEN`, `SAND_USE_EXISTING_BOX_EXEC_DAEMON`,
`SAND_EGRESS_TUNNEL_ENABLED`, `SAND_DISABLE_SEND_ACCEPT_RETURN`,
`SAND_DISABLE_SLIM_AVATARS`, `SAND_DISABLE_GATEWAY_HEALTH_TTL`,
`SAND_DISABLE_GATEWAY_STREAM_LIVENESS`, `SAND_SEND_POST_TIMEOUT_MS`,
`SAND_ROSTER_READ_TIMEOUT_MS`, `SAND_AUTH_CLIENT_ID`, `SAND_DEV_LOGIN`,
`SAND_DEV_LOGIN_EMAIL`, `SAND_DEV_INFERENCE_TOKEN_FILE`.

---

## 7. Auth / identity — what the client assumes

**Toward OpenGrok (seam A), the assumptions are thin:**

1. A single opaque **bearer token** for the whole gateway, sent as
   `Authorization: Bearer <token>` (`gateway-client.ts:97`). It is compared with
   `timingSafeEqual` server-side (`gateway-server.ts:21`) — length-mismatched tokens fail
   fast, so treat the token as a fixed-length secret.
2. **No per-user identity travels on the gateway.** There is no user id, tenant, or
   session in any command's arguments. The gateway is single-account by construction —
   the *desktop* is what is account-scoped (`accountScope` derives from a SHA-256 of the
   JWT `sub`, `cursor-token.ts:24-26`).
3. An optional `x-anyrun-network-token` header (`GATEWAY_NETWORK_TOKEN_HEADER`) is passed
   through verbatim when the brokered path supplied one. OpenGrok can ignore it.
4. Two auxiliary channels are **hard-gated on auth being enabled** — if `authToken == null`
   they answer **401** (`gateway-server.ts:51`): `/local-exec/*` and `/webauthn/*`
   (paths in `source/shared/local-exec-gateway.ts` and `source/shared/webauthn-gateway.ts`).
5. `accountSlot` appears in `promptAcceptanceStatus` args and send-stage telemetry; its
   only observed value is the constant `"host"`
   (`HOST_ACCOUNT_SLOT`, `gateway-client.ts:45` and `source/shared/send-acceptance.ts:2`).

**Toward the Cursor backend (seam B), the assumptions are heavy:** a Cursor OAuth session
producing a JWT with `sub`, `email`, `exp`; refresh when expiring within 5 minutes
(`TOKEN_REFRESH_LEEWAY_MS`, `cursor-token.ts:5`); an access gate
(`getSandAccess`, block reasons enumerated at `source/shared/sand-access.ts:3`:
`unspecified, none, teamPrivacyMode, teamSetupRequired, teamAccessRequired, notOffered,
freeTrialAvailable, paywallIndividual, paywallTeamMember, paywallTeamAdmin`).
The renderer will not even attempt `listAgents` unless
`accountRef.current?.kind === "logged-in"` (`ProductionRenderer.tsx:2182`).

User identity that reaches the *agent* is a single string, the user's full name, injected
as a system prompt line (`source/host/sand-user-identity.ts:4`), max 200 chars.

`sand://` vs `opengrok://`: `CLAUDE.md` — "Never remove or rename `sand://` — it is the
scheme Cursor's official auth callback redirects to." Both are parsed by
`source/shared/deep-link.ts`.

---

## 8. The agent/coworker model — what a roster row is

### 8.1 The authoritative shape

`minimalAgentSummary` (`source/host/extensions/session/session-summaries.ts:13`) defines
the base; `buildSummary` (`:16`) overlays the durable extras. Combined, a roster row is:

| Field | Type | Source | Notes |
|---|---|---|---|
| `id` | string | agent dir name / `db.agentId` | |
| `name` | string | profile; **default `"Grok"`** | |
| `description` | string | profile | subtitle |
| `title` | string | profile | job title |
| `avatarDataUrl` | `string \| null` | `readAvatar()` | **nulled in slim mode** (§2.2) |
| `avatarVersion` | `string \| null` | `readAvatar()` | ETag for `/avatars/<id>?v=` |
| `avatarShape` | `string \| null` | profile | the generative "mark" |
| `avatarColor` | `string \| null` | profile | |
| `createdAt` / `updatedAt` | number (ms) | db / mtime | **`updatedAt` is the roster sort key** (`shared/agents/agent-summaries.ts:6-11`, descending) |
| `path` | string | db path | |
| `isActive` | boolean | `dirName === activeAgentId` | |
| `isRunning` | boolean | `withRunStates` | green pip / eyes |
| `isRunningTurn` | boolean | `withRunStates` | |
| `isComposingMessage` | boolean | `withRunStates` | "sending" overlay |
| `isRetrying` | boolean | `withRunStates` | |
| `currentActivity` | `AgentActivity \| undefined` | `withRunStates` | §4 |
| `activeRemoteMemberId` | string? | `withRunStates` | group rooms |
| `lastEntry` | `{kind:"text",text} \| {kind:"attachment",count,kinds} \| {kind:"link",url} \| null` | `getLastEntryFromTranscript` | the structured sidebar preview (`model.ts:225-228`) |
| `lastMessageId` | `string \| null` | `getLastMessageFromTranscript` | notification dedupe key |
| `lastMessagePreview` | `string \| null` | same | OS-notification body |
| `newestEntryId` | any | last entry id | |
| `hasUnread` | boolean | `isManuallyUnread \|\| lastActivityAt > lastViewedAt` (`:16`) | blue pip |
| `unreadCount` | number | `max(unreadState.unreadCount, 1)` when unread, else 0 | badge |
| `lastViewedAt` | number (ms) | unread state | |
| `lastActivityAt` | number (ms) | unread state | |
| `awaitingUserResponse` | `{reason?} \| null` | db | "needs you" pip + `agent-needs-input` sound |
| `notificationsEnabled` | boolean (default `false`) | | |
| `notifyOnUpdatesEnabled` | boolean (default `true`) | settings file | |
| `isHiddenFromSidebar` | boolean | settings file | roster filter |
| `origin` | string, default `"user"` | db | |
| `purpose` | `"disk-saver" \| "plugin-auth"` — **omitted when null** | db | hides system bots; validated at `host-gateway-api.ts:15` |
| `isGroup` | boolean | caller | |
| `memberIds` | `string[]` | caller | group avatar cluster |
| `remoteRoom` | record — **omitted when null** | caller | shared rooms |
| `conversationPartnerIds` | `string[]` | db | |
| `snapshotEpoch` / `snapshotSeq` | string / number | `applySnapshotStamp` (`roster-emit.ts:88-93`) | stamped onto every row before emit |

The renderer's view of that row is `ConversationAgentSummary`
(`frontend/…/workspace/model.ts:200-223`) — a *subset*; it also reads a UI-only
`draftPrompt`, `isPinned`, `waitingReason`, and `lastMessage`.

### 8.2 The blank-agent suppression rule (subtle, and it will bite you)

`buildSummary` (`session-summaries.ts:16`) **returns `null`** — dropping the agent from the
roster entirely — when *all* of:

```
extras != null  &&  !extras.hasTranscript  &&  !isActive
  && !hasIdentity        // hasIdentity = isGroup || name !== "Grok" || description.trim() || title
  && !includeBlank
  && !(await agentHasDurableFootprint(dir, agentHasMemory))
```

`agentHasDurableFootprint` (`:15`) = quarantined store db **or** memory **or** automations
**or** workflows. If OpenGrok mints a coworker named literally `"Grok"` with no
description, no title, and no transcript, **it will not appear in the sidebar.**

### 8.3 Roster emission protocol

- `runEmitAgents` (`roster-emit.ts:105-125`) — full snapshot on the `agents` channel with
  `coverage:{kind:"complete-roster"}`.
- `runEmitAgentUpdate` (`roster-emit.ts:152-207`) — single-row delta on `agent-upserted`,
  but **escalates to a full emit** when the roster cache is unseeded, the summary is
  missing, or a sibling row changed.
- Emits are **coalesced** through a task boundary (`scheduleCoalescedEmit`, `:51-77`);
  a pending full emit wins over pending deltas.
- `upsertAgentSummary` (`shared/agents/agent-summaries.ts:13-26`) inserts-or-replaces by
  `id` and **re-sorts by `updatedAt` descending** — so `updatedAt` must move when anything
  user-visible changes, or the sidebar will not reorder.

### 8.4 Roster bookkeeping the host performs on every roster event

`createHostRosterBookkeeping` (`source/host/host-roster-bookkeeping.ts:46-103`): on each
`agents`/`agent-upserted` emit the host recomputes `runningAgentIds` from
`transcript.liveRunningAgentIds()`, sets `isBusy = runningAgentIds.size > 0` (which feeds
`/health`), enrols newly-started agents in the disk-pressure reminder, and schedules
state/store snapshots for agents that just stopped.

---

## 9. First-boot checklist — implement in this order

Ordered by the actual call sequence. Sources: `SandHostSupervisor.ensureConnection`
(`host-supervisor.ts:135-168`), `handleTransportEvent`
(`source/node-agent-coordinator/main.ts:157-172`), `createCoordinatorResyncChain`
(`source/electron-main/coordinator/coordinator-resync.ts:7-8`), and the renderer effects in
`frontend/src/production/ProductionRenderer.tsx`.

| # | The client does… | OpenGrok must answer | Failure mode if you skip it |
|---|---|---|---|
| **1** | resolve the connection (`SAND_HOST_GATEWAY_URL` or broker) | — | app never dials you |
| **2** | `GET /health` (1500 ms deadline, TTL 5 s) | `{ok:true, pid, isBusy:false, activeAgentId, startedAt, lastBusyAtMs}` | supervisor discards the cached connection and re-resolves forever |
| **3** | `GET /events` with `Accept: text/event-stream`, `x-sand-slim-avatars: 1` | 200, `retry: 1000\n\n`, then `:ping\n\n` at ≤15 s | 35 s stall watchdog aborts → reconnect loop; `transport-down` propagates to the renderer |
| **4** | on `transport-connected`: coordinator calls `listAgents` (roster seed → main) | JSON **array** | main never seeds; the sidebar has no fallback source |
| **5** | resync chain step `notifications` → `setHostSettings {notifications:{isEnabled:false}}` | any record | logged failure only |
| **6** | step `timezone` → `setHostSettings {userTimeZone, userTimeZoneOverride}` | any record | logged failure only |
| **7** | step `computer_use_model` → `setHostSettings {computerUseModel}` | any record | — |
| **8** | step `auto_review` → `setHostSettings {autoReviewInstructions}` | any record | — |
| **9** | step `local_tool_permission` → `setHostSettings {localToolPermission}` | any record | — |
| **10** | step `webauthn_proxy` → `setHostSettings {webauthnProxyEnabled}` | any record | — |
| **11** | step `feature_flags` → `setHostSettings {featureFlagOverrides}` | any record | — |
| **12** | step `mcp_merge` → **`getHostSettings`**, then `setHostSettings {mcpCustomInstructionsAccountScope, mcpCustomInstructions, mcpCustomInstructionsByServerId, mcpDisabledToolsByServerId}` | the full settings record (see below) | MCP instructions never reconcile |
| **13** | step `box_secrets` → `setBoxSecrets` / `getBoxSecretsStatus` | any | — |
| **14** | step `window_focus` → `setWindowFocused {isFocused}` | void | — |
| **15** | renderer `refreshRoster` → `listAgents` | array of summaries (§8) | **empty sidebar** |
| **16** | renderer onboarding gate → `countAgents` | **number** | onboarding screen instead of the app |
| **17** | renderer → `isAgentNetworkEnabled` | boolean | agent-network UI hidden (fails closed, harmless) |
| **18** | renderer → `getTrays` | **array** | `getTrays returned a malformed array reply` |
| **19** | renderer → `isGlobalSearchEnabled` | boolean | search hidden |
| **20** | computer surface → `getForeverBoxStatus` | `null` **or** `{agentId:string, state:string, …}` | `malformed box status` throw |
| **21** | user clicks a coworker → `openAgentTail {id, limit:200}` | `{entries:[…], nextBeforeSeq?}` | transcript load error banner |
| **22** | user sends → `sendPrompt {…}` | `{accepted:true}` | composer never settles; retries may duplicate |
| **23** | you stream the answer | SSE `transcript` `appended`/`updated`, SSE `agent-upserted` | messages never appear |

`getHostSettings` reply shape (`source/host/extensions/settings/settings-service.ts:27-33`):

```jsonc
{
  "notifications": { "isEnabled": false, "allowedApps": [], "minIntervalMs": 5000,
                     "maxPerWindow": 10, "windowMs": 300000 },   // source/shared/host-settings.ts:1
  "mcpCustomInstructions": {},
  "mcpCustomInstructionsByServerId": {},
  "mcpDisabledToolsByServerId": {},
  "mcpCustomInstructionsAccountScope": "…",   // omitted when undefined
  "mcpBoxServers": [],
  "autoReviewInstructions": …,
  "localToolPermission": …,
  "webauthnProxyEnabled": false,
  "inferenceProvider": "cursor",
  "inferenceRouterUsage": …,
  "userTimeZone": "…",            // omitted when undefined
  "userTimeZoneOverride": "…",    // omitted when undefined
  "agentDefaultModel": {…},       // omitted when undefined
  "computerUseModel": {…},        // omitted when undefined
  "pinnedAgentIds": [],           // omitted when undefined
  "sidebarSections": [],
  "hasSeenOnboarding": true       // omitted when undefined
}
```

`setHostSettings` **must return the same record** (`settings-service.ts:56` returns
`this.getHostSettings()`), because the resync chain reads it back.

---

## 10. Traps — things that fail silently

### Trap 1 — the loopback refusal (**the one that will cost you a day**)

`source/electron-main/box/local-docker-host-connector.ts:465`:

```ts
const connection = await remote.connect();
const gatewayHost = new URL(connection.baseUrl).host;
if (gatewayHost.startsWith("127.0.0.1") || gatewayHost.startsWith("localhost")) {
  throw new Error("Account computer resolved to a loopback gateway; refusing to treat the local VM as Grok VM.");
}
```

This wrapper (`createSettingsRoutedHostConnector`, `:413`) sits **outside**
`EnvDescriptorHostConnector`. It only skips the check when the persisted setting
`boxRuntime === "local-docker"` (`:429-430`) — and that branch ignores your env var
entirely and instead spawns its own host on `127.0.0.1:1350`
(`ensureDesktopHost`, `:257-262`, `DESKTOP_HOST_PORT = 1350`).

**Mitigation for OpenGrok:** serve on a non-loopback name. Give the machine a hosts-file
alias (e.g. `opengrok.local`) or bind a LAN/container IP, and set
`SAND_HOST_GATEWAY_URL=http://opengrok.local:PORT`. `127.0.0.2` also passes the literal
`startsWith` test but is fragile — prefer a name. (`isLoopbackGatewayUrl` in
`source/shared/box-runtime.ts:44-51` is stricter — `127.0.0.1 | localhost | ::1` — and is
used elsewhere for the descriptor cache.)

### Trap 2 — an empty-but-successful reply renders as "no coworkers"

`listAgents` returning `[]` is a **valid** reply. The renderer sets
`setRosterLoadFailed(false)` and paints an empty sidebar
(`ProductionRenderer.tsx:2185-2193`). There is no "backend didn't answer" state — you
either return rows or the user sees nothing and blames the app. During bring-up, return
one seeded coworker rather than an empty array.

Compounding this, §8.2: a summary that *is* returned can still be dropped by the
blank-agent rule if it is named `"Grok"` with no description/title/transcript.

### Trap 3 — reply-shape validators that reject rather than degrade

`frontend/src/production/coordinator-client.ts:33-48` throws — not warns — when:

- `listAgents` / `searchAgents` / `getTrays` / `listAllAutomations` is not an `Array`
  (`{agents:[…]}` is **wrong**; the bare array is the reply);
- `openAgentTail` / `getAgentTranscriptTail` is not `{entries: […]}` with a numeric-or-absent
  `nextBeforeSeq`;
- `getForeverBoxStatus` / `ensureForeverBox` is neither `null` nor
  `{agentId: string, state: string}`.

And `source/shared/rpc/coordinator.ts:40-52` rejects `getAgentTranscriptWindow` unless
`threadCounts` is a record of `string → finite number`. Returning `{}` is fine; omitting
the key is not.

### Trap 4 — transcript-card entries are dropped, not errored

`projectTranscriptCardEntry` (`…/transcript-card/protocol.ts:410-495`) returns `null` on
any field violation, and `projectTranscriptCardEntries` (`:497-502`) just skips those and
reports a `rejectedCount` nobody surfaces. So a widget with 7 options, an
`auto-review-approval` with a bogus `status`, or a `send-message` with a non-string `id`
**vanishes from the transcript with no error anywhere**. When a card does not appear,
check the projector's validation list first.

### Trap 5 — slim avatars and the missing `avatarVersion`

The coordinator always sends `x-sand-slim-avatars: 1` (`gateway-client.ts:145`), so
`avatarDataUrl` is nulled and the renderer falls back to `GET /avatars/<id>?v=<version>`.
If `avatarVersion` is `null`, the route answers **404 "agent has no avatar"**
(`gateway-server.ts:42`) and every face is blank. Either set both, or set neither and let
`avatarShape` + `avatarColor` draw the generative mark.

### Trap 6 — heartbeats are load-bearing

No `:ping` for 35 s ⇒ the stall watchdog aborts the stream (`gateway-client.ts:40,443`),
which fires `transport-down`, which invalidates the health cache and forces a full
reconnect + roster reseed (`main.ts:157-172`). A quiet backend looks like a dead backend.

### Trap 7 — `sendPrompt` must answer immediately

`{accepted:true}` is the *acceptance* receipt, not the completion. If OpenGrok blocks the
POST until the turn finishes, the 15 s `sendPostDeadline` (`gateway-client.ts:42`) fires,
the client treats the send as failed, and its retry logic engages — potentially producing
a duplicate turn unless you honour `clientNonce` idempotency
(`prompt-acceptance-ledger.ts:245-254`). Two escape hatches exist but are not defaults:
`SAND_DISABLE_SEND_ACCEPT_RETURN=1` (`host-gateway-api.ts:13,227`).

### Trap 8 — `Origin` header ⇒ 403

`gateway-server.ts:23` rejects **any** request carrying an `Origin` header with 403
"browser-origin gateway requests are not allowed". Do not try to exercise OpenGrok from a
browser tab or a fetch that sets `Origin`; use curl / the app.

### Trap 9 — local-inference mode silently suppresses the roster stream

`source/node-agent-coordinator/main.ts:136` drops `agents` and `agent-upserted` SSE events
entirely when the persisted `inferenceProvider` is `claude-code`, `codex`, or `openrouter`
(`usesLocalInference`, `:34-37`). If the roster stops updating for no visible reason,
check `settings.json` in the data root before suspecting OpenGrok.

### Trap 10 — the 409 statuses

`gateway-server.ts:15`: only `SandAgentLimitError` and `SandSkillPublishError` map to 409;
everything else is 500. The renderer special-cases the strings `"50 is the maximum"` →
`agent-limit-reached` and the prefix `"skill-publish/refused: "`
(`frontend/src/recovered/runtime/coordinator-source.ts:36-38`). Preserve those exact
messages if you implement the limits.

### Trap 11 — command *count* vs. renderer *reachable* count

123 unique commands exist in `SAND_GATEWAY_COMMANDS`; only **90** appear in
`COORDINATOR_METHOD_TABLE` (`source/shared/rpc/coordinator.ts:92-183`). The other 37 —
`getTranscript`, `getAgentTranscript`, `getAgentTranscriptPage`, `openAgentWindowed`,
`getAgentMemories`, `deleteAgentMemory`, `clearAgentMemories`, `openAgent`,
`setWindowFocused`, `appendConnectorCard`, `deleteAgent`, all the box-lifecycle and
host-upgrade commands, `getHostSettings`/`setHostSettings`, `setBoxSecrets`,
`completeMcpOAuth`, `refreshMcp`, `requestWebAuthnCeremony`, `listBoxMcpServers`,
`uploadAttachment`, `read*` — are reached from **Electron main**, not the renderer, over
the control port. Both callers hit the same `/api/<cmd>` endpoint. Do not conclude a
command is dead because the renderer never names it.

### Trap 12 — LEGAL: transcribe shapes, do not copy generated stubs

`source/packages/proto/generated/**` contains **161 generated `@bufbuild/protobuf` TypeScript
files** (`agent/v1` 101, `aiserver/v1` 38, `anyrun/v1` 6, `internapi/v1` 1 — counts per
`docs/grok-0.27-disparity-proto.md` §2.1, confirmed by file count in this tree), plus
`source/packages/redacted-protos/generated/**`. These are **vendored recovered artifacts**,
not OpenGrok's to relicense.

`NOTICE.md`: *"No upstream source-code license is asserted or granted here… Anyone
publishing or distributing this repository should independently review copyright,
trademark, third-party dependency, and service-terms obligations."*

Therefore:

- **DO** transcribe, into OpenGrok's own Rust types, the shapes the client already emits
  on the JSON wire — command names, argument keys, entry `kind`s, `message.type`s, SSE
  channel names, roster field names. That is interoperability data: it is what the client
  sends and expects, observable from the wire.
- **DO** cite provenance next to each transcribed shape, in the style this repo already
  uses: `// @evidence source/host/gateway-protocol.ts:23 (listAgents)`. See
  `frontend/src/recovered/features/conversation/cards/transcript-card/protocol.ts:1-3`
  for the house format.
- **DO NOT** copy any file from `source/packages/proto/generated/**` or
  `source/packages/redacted-protos/**` into `opengrok/`, and do not translate them
  wholesale. OpenGrok implements seam A, which is **plain JSON** — it needs no protobuf
  at all. Protobuf appears only on seam B (Cursor backend) and on the box exec daemon
  (§5.5), and OpenGrok owns the latter's far side by definition.
- **DO NOT** implement from `docs/grok-0.27-disparity-proto.md`. Its own header, line 3,
  reads: **"Status: inventory only. Do not implement from this file."** It is a
  Grok-vs-Barok comparison of the *vendor's* server surface, written to find gaps in a
  third product. Use it the way this document has — as a cross-check that a shape you
  read out of `source/` is real — never as a spec. OpenGrok implements the
  **client-facing contract**, not the vendor's server.
- `PROVENANCE.md`'s evidence-only rule applies transitively: if a field is not observable
  in `source/` or `frontend/src/recovered/`, record the uncertainty in OpenGrok rather
  than inventing it.

---

## Appendix A — known gaps in this document

- **`getTranscript` / `ensureLoaded` reply shape**: `host-gateway-api.ts:182` delegates to
  `manager.ensureLoaded()`; the concrete return type was not resolved in this pass.
  It is not in the renderer method table, so it is not needed for boot.
- **`channels-view`, `import-result`, `box-secrets`, `connect-url` reply shapes**: named
  in `COORDINATOR_METHOD_TABLE` but their validators are `validateCoordinatorReply`
  passthroughs; concrete field lists not traced. Not needed for §2.3/§2.4.
- **Memory command surface**: which UI reaches `getAgentMemories` — not found in tree.
- **`permission-request` card**: emitted by host shaping per
  `docs/grok-0.27-disparity-proto.md` §3.2, absent from `TRANSCRIPT_CARD_ENTRY_TYPES`.
  Contradiction unresolved.
- **`client-side-tool-v2` payload union**: relayed at
  `source/node-agent-coordinator/client-side-tool-v2-relay.ts`; not decoded here.
- **The 60 s activity hold** noted in `docs/grok-0.27-disparity-proto.md` §7 as present in
  official 0.27 (`mL = 6e4`) is **not** in this tree; the host hold is 2500 ms.

## Appendix B — the fastest possible bring-up

A backend that answers exactly this makes the app boot, show one coworker, accept one
message, and paint one reply:

```
GET  /health                     → {"ok":true,"pid":1,"isBusy":false,"activeAgentId":"a1",
                                    "startedAt":<ms>,"lastBusyAtMs":0}
GET  /events                     → SSE, "retry: 1000\n\n", ":ping\n\n" every 10s
POST /api/listAgents             → [ <one summary, §8.1, name != "Grok"> ]
POST /api/countAgents            → 1
POST /api/getTrays               → []
POST /api/isAgentNetworkEnabled  → false
POST /api/isGlobalSearchEnabled  → false
POST /api/getHostSettings        → <record, §9>
POST /api/setHostSettings        → <same record>
POST /api/getForeverBoxStatus    → null
POST /api/getSharingState        → {isEnabled:false, selfAuthId:null, pendingJoinRequests:[], rooms:[], typingUsers:[]}
POST /api/openAgentTail          → {"entries":[…]}
POST /api/sendPrompt             → {"accepted":true}
```

…then push, over `/events`:

```
data: {"channel":"transcript","payload":{"type":"appended","agentId":"a1",
       "ordered":{"replicaKey":"transcript:a1","epoch":"<uuid>","sequence":1},
       "entry":{"kind":"message","id":"e1","role":"user","content":"hi",
                "isStreaming":false,"timestampMs":<ms>,"clientNonce":"<echo it>"}}}

data: {"channel":"agent-upserted","payload":{"activeAgentId":"a1",
       "ordered":{"replicaKey":"roster","epoch":"<uuid>","sequence":1},
       "agent":{ …summary…, "isRunning":true, "currentActivity":{"kind":"thinking"} }}}

data: {"channel":"transcript","payload":{"type":"appended","agentId":"a1",
       "ordered":{"replicaKey":"transcript:a1","epoch":"<uuid>","sequence":2},
       "entry":{"kind":"send-message","id":"e2","timestampMs":<ms>,"streaming":true,
                "message":{"type":"text","content":""}}}}

data: {"channel":"transcript","payload":{"type":"updated", … "entry":{ …content grows… }}}

data: {"channel":"transcript","payload":{"type":"updated", … "entry":{"kind":"send-message",
       "id":"e2","timestampMs":<ms>,"message":{"type":"text","content":"hello"}}}}   // no streaming flag

data: {"channel":"agent-upserted","payload":{"activeAgentId":"a1",
       "ordered":{"replicaKey":"roster","epoch":"<uuid>","sequence":2},
       "agent":{ …summary…, "isRunning":false, "lastMessageId":"e2",
                 "lastMessagePreview":"hello", "lastEntry":{"kind":"text","text":"hello"},
                 "lastActivityAt":<ms>, "updatedAt":<ms> }}}
```

Keep `epoch` constant for the process lifetime and `sequence` monotonic per `replicaKey`.
