# Client Versions 0.18 → 0.30 — Protocol Reference

*Reference document for the OpenGrok backend. Transcribed from shipped binaries; every shape below carries a provenance comment naming the file it was read from.*

---

## Source refs

Every provenance comment in this document uses one of these refs. Resolve the ref, then the path suffix.

| ref | resolves to |
|---|---|
| `A18` | `/Volumes/goldcoders/OSS/grok-bot/.cache/runtime/Grok Bot.app/Contents/Resources/app.asar`, extracted with `npx --yes @electron/asar extract` |
| `E29` | `/private/tmp/claude-501/-Volumes-goldcoders-OSS-grok-bot/a4e01314-1252-4715-a4f0-b7f41e357999/scratchpad/app-0.29-extracted` |
| `E30` | `/private/tmp/claude-501/-Volumes-goldcoders-OSS-grok-bot/a4e01314-1252-4715-a4f0-b7f41e357999/scratchpad/app-0.30-extracted` |
| `F27` | `/Volumes/goldcoders/OSS/grok-bot/manifests/reconstruction/evidence/` |
| `OURS` | `/Volumes/goldcoders/OSS/grok-bot/source/` (the client reimplementation this backend must serve) |

Two independent extractions of `A18` into different directories were compared with `diff -rq` and are byte-for-byte identical, so the 0.18 evidence is reproducible from the asar alone.

---

## 1. TL;DR — the few things that change what you build

1. **The backend surface is the gateway HTTP API, not the "332 → 345 RPC methods."** Those 332/345 are Electron *inter-process* edges (`dune-rpc:<edge>:m:<method>` over `ipcRenderer`). They never touch the network. The wire a backend answers is unchanged in shape across all four versions: `POST {base}/api/<method>` with a JSON body, `GET {base}/events` SSE, `GET {base}/health`, `Authorization: Bearer <token>`.
   *Provenance:* `A18:dist/host/host-main.cjs:501991` (`url.pathname.slice(GATEWAY_API_PREFIX.length + 1)`); `E29/dist/node-agent-coordinator/main.cjs` (`fetch(\`${s.baseUrl}${ol}/${e}\`,{method:"POST",…})`, `ol="/api"`).

2. **Our pinned client is 0.18-era.** The renderer we patch is `index-UbX-y3il.js`, which is 0.18's bundle (0.29 ships `index-Dklr8uwG.js`, 0.30 ships `index-DCpFUyZ2.js`), and `OURS/host/gateway-protocol.ts` reproduces 0.18's 120-command table verbatim in order. So the contract to serve is **120 gateway commands, 6 transcript entry kinds, 13 send-message card types, 16 SSE families, 4 event types** — not the 0.29/0.30 supersets.

3. **0.18 is the only version whose gateway *server* ships.** `dist/host/` exists in 0.18 and in no later bundle; `SAND_GATEWAY_COMMANDS`, `routeCommand`, `handleRequest`, `SAND_GATEWAY_SLIM_COMMANDS`, `/prepare-upgrade`, `/avatars` have **zero** occurrences anywhere in 0.29 or 0.30. Everything host-private in 0.18 (SSE mechanics, avatar stripping, threading rewriters, the `/sand/notify` upstream) is therefore **unverified** for 0.29/0.30 — but it is also the code we are replacing, so 0.18's copy is the spec.

4. **The spine is stable and it is where og-wire should live.** Across all four versions: the four threadable entry kinds, the `event.type` union, the activity kind union `thinking|tool`, the activity wire shape `{kind, tool?, detail?, target?, callId?}`, 16 of the SSE families, and the `POST /api` + SSE + Bearer transport are unchanged. Every later delta is additive on top of that.

5. **No version — 0.18, 0.27, 0.29, 0.30 — has a per-message delete or edit RPC.** The only message-scoped mutations are `reactToMessage` (all versions) plus `submitUserForm`, `dismissUserForm`, `sendDraft`, `discardDraft`, `voteFeedback` (0.29+), and the settle-in-place approval RPCs. Our tree has invented a `deleteTranscriptEntries` gateway command that exists in no official binary — see §7.1.

6. **Large parts of 0.29/0.30 are dark by default and need not be built.** Voice calling (`sand_voice_call`), the org chart (`sand_agent_network`), multiplayer, channels, bot sharing/marketplace, and *the entire local→server storage migration* (`sand_send_via_server`, `sand_roster_via_server`, `sand_transcript_server_tail`, `sand_attachments_via_server`, `sand_transcript_store_*`) all ship `default:!1` in both 0.29 and 0.30. Zero gate defaults flipped between 0.18 and 0.30.

7. **Counts in the working brief that are wrong, corrected here:** feature gates are **699 → 708**, not 691 → 700. Daemon frame kinds are **12 → 15** (JSON) or 21 → 27 (JSON + protobuf oneof members), not 12 → 24. The 0.18 `sand:*` figure of 73 is a *string-literal* count; the true channel count is ~71.

---

## 2. Version matrix

### 2.1 What we physically hold

| version | artifact | status |
|---|---|---|
| **0.18** | `A18` — the shipped `app.asar` under `.cache/runtime`, plus the retained installers in the client repo. The host bundle inside is **unminified**, carries original `// src/...` module banners, and carries per-gate doc comments. | **Best evidence we will ever have.** Re-extractable and byte-reproducible. |
| **0.27** | **NO BINARY.** Auto-update overwrote the installed 0.27 app before it was archived; there is no asar, no dmg, no installer. All that survives is three hand-lifted fragments in `F27`: `grok_bot_service-0.27.fragment.js` (the `GrokBotService` method table, 76 rows), `sand_box_service-0.27.fragment.js` (`SandBoxService`, 37 rows), `grok_bot_pb-0.27.fragment.js` (132 `typeName` bindings observed; the brief's "130 aiserver.v1 type names" was not separately reconciled — **unverified**). | **Destroyed by auto-update.** Only the proto/service surface is recoverable for 0.27. Everything else in this document has a blank 0.27 column and that blank is honest, not lazy. |
| **0.29** | dmg, extracted to `E29`. Layout: `dist/{electron-main,electron-preload,node-agent-coordinator,local-exec-daemon,renderer,deps,native}` + `package.json`. All JS minified. | Complete, minified. |
| **0.30** | asar lifted from the installed application, extracted to `E30`. Same layout as 0.29. | Complete, minified. |

`dist/host/` and `dist/electron-dev-controls/` exist **only** in 0.18. 0.29 and 0.30 have seven `dist/` directories; 0.18 has nine.

### 2.2 Surface × version

Counts are unique names unless stated. "—" = the surface does not exist in that version. "?" = we hold no artifact that could answer it.

| surface | 0.18 | 0.27 | 0.29 | 0.30 | provenance |
|---|---:|:---:|---:|---:|---|
| **Gateway commands** (the backend API) | **120** | ? | **146** | **147** | `A18:dist/host/host-main.cjs:500975` (`SAND_GATEWAY_COMMANDS`); `E29,E30:dist/node-agent-coordinator/main.cjs` (`declareRpcEdge("gateway",…).methods`) |
| — of which no-arg | 23 | ? | n/a | n/a | `A18:dist/host/host-main.cjs:500976-501095` |
| — of which arg (`parseCommandArgs(body)`) | 97 | ? | n/a | n/a | same |
| Gateway slim-avatar overrides | 7 | ? | 0 (client-side) | 0 | `A18:dist/host/host-main.cjs:501122` |
| Proxy/reply-shape policy table | n/a | ? | 121 | 122 | `E29:…/node-agent-coordinator/main.cjs` (`mR={...pR,…}`); `E30` (`Iw={...xw,…}`) |
| Gateway server shipped? | **yes** | ? | **no** | **no** | `dist/host/` present only in 0.18 |
| **Internal RPC edges** | 5 | ? | 6 | 6 | `A18:dist/electron-preload/preload.cjs:4-31`; `E29,E30:dist/electron-main/main.cjs` |
| Internal RPC methods (unique) | **250** | ? | **332** | **345** | see §3.2 |
| Internal channel prefix | `sand-rpc:` | ? | `dune-rpc:` | `dune-rpc:` | `A18:…/preload.cjs:25-27`; `E29,E30:dist/electron-main/main.cjs` |
| **`sand:*` raw IPC** literals | 73 (~71 channels) | ? | 17 (12 channels) | 17 (12 channels) | union sweep over each `dist/` tree |
| **Transcript entry kinds** | **6** | ? | **8** | **8** | `A18:dist/host/host-main.cjs:628621`; `E29:dist/electron-main/main.cjs@5442123`; `E30:…@7117808` |
| **Send-message card types** | **13** | ? | **15** | **17** | `A18:dist/renderer/assets/index-UbX-y3il.js@5110149`; `E29:…/index-Dklr8uwG.js@3460734`; `E30:…/index-DCpFUyZ2.js@3480469` |
| `event.type` values | 4 | ? | 4 | 4 | same three renderer offsets |
| **SSE families** | **16** | ? | **18** | **18** | `A18:dist/node-agent-coordinator/main.cjs@87813`; `E29:…@137633`; `E30:…@147349` |
| Renderer SSE ingest map keys | 16 | ? | 18 | **17** | `A18@5728132`; `E29@4235285`; `E30@4264218` — see §7.8 |
| Activity kind union | 2 | ? | 2 | 2 | `A18:dist/host/host-main.cjs:645598`; `E29:…/node-agent-coordinator@~150015`; `E30:…@~160015` |
| Computer-action types | 4 | ? | 4 | 4 | `A18:dist/host/host-main.cjs:660877-660899`; coordinator `computerActionEventOf` in both later versions |
| **Daemon JSON frame kinds** | ? | ? | **12** (6 req + 6 resp) | **15** (7 + 8) | `E29,E30:dist/local-exec-daemon/main.cjs` guard sets |
| Daemon protobuf oneof members | ? | ? | 9 (5 + 4) | 12 (6 + 6) | same file, `RequestFrame`/`ResponseFrame` field lists |
| `SandLocalToolAction` values | ? | ? | 5 | 7 | daemon + `dist/electron-preload/preload.cjs` + `dist/electron-main/main.cjs`, all three in sync |
| **Feature gates** (`FLAGS`) | **614** | ? | **699** | **708** | `A18:dist/host/host-main.cjs:605331`; `E29:dist/electron-main/main.cjs` (`GJt`); `E30:…` (`vqt`) |
| — default ON / OFF | 187 / 427 | ? | 179 / 520 | 179 / 529 | same |
| — `sand_*` subset | 37 | ? | 73 | 77 | same |
| `EXPERIMENTS` | 109 | ? | 112 | 111 | same files (`QJt` / `Iqt`) |
| `DYNAMIC_CONFIGS` | 123 | ? | 136 | 137 | same files (`VJt` / `Cqt`) |
| **`aiserver.v1` top-level type names** | **3874** | ? | **3932** | **4001** | 0.18: `A18:dist/host/host-main.cjs`. 0.29/0.30: `E29,E30:dist/electron-main/main.cjs` |
| — nested `Outer.Inner` names | 1724 | ? | 660 | 660 | same |
| — same count in 0.18 `electron-main` | 3617 / 1679 | — | — | — | `A18:dist/electron-main/main.cjs` (subset of host-main) |
| — coordinator-scoped subset | 0 | ? | 265 | 303 | `dist/node-agent-coordinator/main.cjs` per version |
| **`GrokBotService` methods** | **30** | **76** | **89** | **106** | `A18:dist/host/host-main.cjs`; `F27/grok_bot_service-0.27.fragment.js`; `E29,E30:dist/node-agent-coordinator/main.cjs` |
| `SandBoxService` methods | — | 37 | 37 | 37 | same |
| `AutomationsService` / `InferenceService` | 42 / 3 | ? | — | — | `A18:dist/host/host-main.cjs` only |

**On the 3874 / 3932 / 4001 figures.** They are confirmed, but they are *top-level* names only and they do not all come from the same file. 3932 and 4001 are top-level names in `dist/electron-main/main.cjs`. **3874 is a count in `A18:dist/host/host-main.cjs`** — 0.18's `electron-main` carries only 3617 top-level names (a strict subset of host-main's). Counting nested names too gives 5598 / 4592 / 4661. Do not chase the 257-name difference; it is a file mix-up in the working brief, corrected here.

---

## 3. The architecture shift

### 3.1 What actually changed (and what did not)

The working brief's framing — "0.18's readable gateway table vs 0.29/0.30's opaque RPC bus" — is **wrong in one important way**: 0.18 already has the RPC bus. `declareRpcContract` / `methodChannel` are present, unminified, in 0.18's preload.

```js
// A18:dist/electron-preload/preload.cjs:4-31 — comments are the original source paths
// dune/src/internal/rpc/contract.ts
function declareRpcContract(edge, ...events) { return { edge, hasEvents: events.length > 0 }; }
// dune/src/internal/rpc/edge.ts
function methodChannel(edge, method) { return `sand-rpc:${edge}:m:${method}`; }
function eventChannel(edge, event)  { return `sand-rpc:${edge}:e:${event}`;  }
```

```js
// A18:dist/electron-preload/preload.cjs:83-88
// src/shared/rpc/main.ts
var mainRpcContract = declareRpcContract("main", "events");
var MAIN_METHOD_TABLE = {
  openExternal: { args: "object" },
  submitFeedback: { args: "object" },
  getDesktopEnvironment: { args: "none" },
```

What genuinely changed:

| | 0.18 | 0.29 / 0.30 |
|---|---|---|
| channel prefix | `sand-rpc:<edge>:m:<method>` (0 occurrences of `dune-rpc:`) | `dune-rpc:<edge>:m:<method>`, `:e:`, `:probe` (0 occurrences of `sand-rpc:`) |
| method row | declarative `{ args: "object" \| "none" }`, plus a `reply:` discriminator on the coordinator table | validator-carrying `C().args(<zod-ish schema>)` / `C().noArgs`; the `reply:` discriminator moved to a separate policy table |
| edges | 5: `main`, `coordinator`, `coordinator-main`, `box-vnc`, `dev-controls` | 6: `main`, `gateway`, `coordinator-control`, `coordinator-main`, `sand-dev`, `box-vnc` |
| lineage | — | `coordinator` renamed **`gateway`**; `dev-controls` (a whole separate `dist/electron-dev-controls/` app) dropped and replaced by the 8-method `sand-dev` edge; `coordinator-control` is **new** in 0.29 |
| raw `sand:*` IPC | 73 literals (~71 channels) — the whole MCP block, the `report-*` telemetry block, secrets, client-persistence, `*-get-sync` reads | 12 channels — port transfer, VNC, browser-popup, boot snapshot, render-trace port, new-chat |

The wire template is identical in 0.29 and 0.30; only the minified wrapper names differ, so grep with the template, not the identifier:

```js
// E29:dist/electron-main/main.cjs — function name wAr, registered via s(...)
// E30:dist/electron-main/main.cjs — function name lFr, registered via i(...)
function <name>(n,e){return `dune-rpc:${n}:m:${e}`}   // "methodChannel"
```

**None of this is backend surface.** It is renderer↔main↔coordinator IPC inside one Electron app. The only reason a backend implementer cares is that the `gateway` edge's method table *is* the list of `POST /api/<method>` calls the coordinator will make.

### 3.2 Internal edge counts

| edge | 0.18 | 0.29 | 0.30 | Δ 29→30 |
|---|---:|---:|---:|---:|
| `main` | 117 | 152 | 173 | +21 |
| `coordinator` → `gateway` | 85 | 146 | 147 | +1 |
| `coordinator-control` | — | 22 | 25 | +3 |
| `coordinator-main` (own methods) | 19 rows total | 2 own + 18 aliases | 2 own + 19 aliases | 0 own |
| `box-vnc` | 3 | 3 | 3 | 0 |
| `dev-controls` | 33 | — | — | — |
| `sand-dev` | — | 8 | 8 | 0 |
| **row sum** | 257 | 333 | 358 | |
| **unique names** | **250** | **332** | **345** | **+13** |

0.18's 257 → 250 reconciliation (three overlaps, 7 names): `COORDINATOR ∩ COORDINATOR_MAIN` = 5 (`listAgents, createAgent, deleteAgents, getConversationOutline, getSubagents`); `MAIN ∩ COORDINATOR_MAIN` = 1 (`readAttachmentText`); `MAIN ∩ DEV_CONTROLS` = 1 (`setThemePreference`).

0.29/0.30's row-sum ≠ unique is entirely `main ∩ gateway` — all 15 edge pairs were checked and no other pair overlaps at all. 0.29 has one mirrored name (`readAttachmentText`); 0.30 has thirteen (`authenticateMcpServer, generateAgentAvatarImage, getMcpCatalog, getMcpPluginLogo, getMcpState, listMcpServerTools, readAttachmentText, removeMcpAccount, removeMcpServer, renameMcpAccount, setMcpCustomInstructions, toggleMcpToolDisabled, transcribeAudio`).

`coordinator-main` is spread-only in 0.29/0.30 — it re-exports gateway methods and adds two of its own:

```js
// E30:dist/electron-main/main.cjs @5945876
JVr={uploadAttachment:…, readAttachmentImage:…, readAttachmentText:…, readAttachmentChunk:…,
     getHostSettings:…, setHostSettings:…, setBoxSecrets:…, injectChromeCookies:…, refreshMcp:…,
     listBoxMcpServers:…, completeMcpOAuth:…, updateForeverBox:…, setWindowFocused:…,
     getHostStatus:…, listAgents:…, createAgent:…, deleteAgents:…, getConversationOutline:…,
     getSubagents:…}                                              // 19 aliases
PVr={setDevGatewayOffline:C().args({induced:we()}), setGatewayPaused:C().args(RVr)}   // 2 own
```

0.18's `COORDINATOR_MAIN_METHOD_TABLE` is **19 rows** = 17 aliases + the same 2 own methods. So the trajectory is 17 → 18 → 19 aliases. Only `listBoxMcpServers` is a *restoration* of a 0.18 name; `injectChromeCookies` and `completeMcpOAuth` are post-0.18 additions to the alias set that were never in 0.18's table. (The mirrored copies of these tables in `dist/node-agent-coordinator/main.cjs` use different minified identifiers — `wA`/`RA` in 0.29, `BR`/`UR` in 0.30 — so a grep for `JVr`/`PVr` in the coordinator bundle finds nothing.)

0.18 carries an in-bundle comment explaining the alias overlap, worth quoting because it states design intent:

> `A18:dist/node-agent-coordinator/main.cjs:768-775` — *"DEV-ONLY rows: the agent-data legs behind the control-sand driver's sandDev capability… Each row is ALSO a renderer-table member… the one sanctioned overlap between the two data-port tables, pinned by coordinator-main.test.ts."*

### 3.3 What "our client is 0.18-era" means for a backend

Serve the **0.18 host gateway**, exactly:

```js
// A18:dist/host/host-main.cjs:501734-501739
async function routeCommand(api, method, body, res, req, onCommandError, onCommandComplete) {
  if (!Object.hasOwn(SAND_GATEWAY_COMMANDS, method)) {
    return respondError(res, 404, `unknown gateway method: ${method}`);
  }
  const table = clientWantsSlimAvatars(req) ? SAND_GATEWAY_SLIM_COMMANDS : SAND_GATEWAY_COMMANDS;
```

Note the membership test is against the **full** table before the slim table is selected, so slim can never widen the surface.

Paths and headers (0.18; `GATEWAY_PREPARE_UPGRADE_PATH` is declared in `gateway-protocol.ts` itself at `:501097`, the rest come from `src/shared/gateway-wire.ts` at `:501234-501241`, `src/shared/local-exec-gateway.ts` at `:501244`, `src/shared/webauthn-gateway.ts` at `:501311`):

| constant | value | method | handling in `handleRequest` (`A18:dist/host/host-main.cjs:501932-502005`) |
|---|---|---|---|
| `GATEWAY_HEALTH_PATH` | `/health` | GET | answered before the auth check; `{ok, pid, isBusy, busyOnlyAwaitingApproval?, activeAgentId, startedAt, lastBusyAtMs}` (`:501937-501945`) |
| `GATEWAY_PREPARE_UPGRADE_PATH` | `/prepare-upgrade` | POST | `deps.prepareForUpgrade()` → `{quiescing, runningTurns}`, falling back to `{quiescing:false, runningTurns:0}` (`:501967`) |
| `GATEWAY_EVENTS_PATH` | `/events` | GET | SSE; `parseSubscribedChannels(url)` reads `?channels=a,b,c` |
| `GATEWAY_AVATARS_PATH` | `/avatars` | GET `/avatars/<encoded agentId>` | `handleAvatarImage` |
| `GATEWAY_API_PREFIX` | `/api` | POST `/api/<method>` | `routeCommand` (`:501991`) |
| `GATEWAY_LOCAL_EXEC_REQUESTS_PATH` | `/local-exec/requests` | GET | 401 `"local-exec requires gateway authentication"` if `authToken == null` (`:501958`) |
| `GATEWAY_LOCAL_EXEC_RESPONSES_PATH` | `/local-exec/responses` | POST | same precondition |
| `GATEWAY_WEBAUTHN_REQUESTS_PATH` | `/webauthn/requests` | GET | 401 `"webauthn requires gateway authentication"` (`:501961`) |
| `GATEWAY_WEBAUTHN_RESPONSES_PATH` | `/webauthn/responses` | POST | same |

Headers: `GATEWAY_AUTH_SCHEME = "Bearer"`, `GATEWAY_SLIM_AVATARS_HEADER = "x-sand-slim-avatars"`, `GATEWAY_MINT_DEDUPE_HEADER = "x-sand-mint-dedupe"`, `GATEWAY_TRACEPARENT_HEADER = "traceparent"`. Loopback set `["127.0.0.1","localhost","::1","[::1]"]` (`:501098`). Anything unmatched → `respondError(res, 404, \`not found: ${req.method} ${url.pathname}\`)` (`:502003`).

Two gates precede everything: `rejectUntrustedBrowserRequest(deps, req, res)` at `:501934` runs *before* `/health` and can short-circuit it, and a generic `respondError(res, 401, "unauthorized")` at `:501963-501965` gates every non-health path when `deps.authToken != null`.

SSE mechanics (`A18:dist/host/host-main.cjs:501786`, `openSseStream`):

```js
res.writeHead(200,{ "content-type":"text/event-stream", "cache-control":"no-cache, no-transform",
                    connection:"keep-alive", ...gzip?{"content-encoding":"gzip",vary:"Accept-Encoding"}:{} });
sink.write("retry: 1000\n\n");
// heartbeat: setInterval(() => sink.write(":ping\n\n"), SSE_HEARTBEAT_MS)
// A18:dist/host/host-main.cjs:501622 — var SSE_HEARTBEAT_MS = 15e3;
```

Gzip kill switch: env `SAND_DISABLE_GATEWAY_SSE_GZIP=1`.

Slim avatars: the 0.29/0.30 client still sends `x-sand-slim-avatars` (exactly one occurrence per version, in `dist/node-agent-coordinator/main.cjs` only). **No client-side stripping was identified** in 0.29/0.30 — but the bundles are minified, so name-absence is weak evidence; treat "the server must strip" as the safe implementation and the location of any residual client stripping as **unverified**.

0.18's slim table, verbatim:

```js
// A18:dist/host/host-main.cjs:501122-501131
var SAND_GATEWAY_SLIM_COMMANDS = {
  ...SAND_GATEWAY_COMMANDS,
  listAgents:          async (api, body) => stripSummaryRows(await SAND_GATEWAY_COMMANDS.listAgents(api, body)),
  updateAgent:         async (api, body) => stripNullableSummary(await SAND_GATEWAY_COMMANDS.updateAgent(api, body)),
  setGroupMembers:     async (api, body) => stripNullableSummary(await SAND_GATEWAY_COMMANDS.setGroupMembers(api, body)),
  setAgentAvatarBytes: async (api, body) => stripNullableSummary(await SAND_GATEWAY_COMMANDS.setAgentAvatarBytes(api, body)),
  createAgent:         async (api, body) => stripCreateAgentResult(await SAND_GATEWAY_COMMANDS.createAgent(api, body)),
  createGroup:         async (api, body) => stripCreateAgentResult(await SAND_GATEWAY_COMMANDS.createGroup(api, body)),
  duplicateAgent:      async (api, body) => stripCreateAgentResult(await SAND_GATEWAY_COMMANDS.duplicateAgent(api, body))
};
```

Helpers in the same module (`:501102-501132`): `stripSummaryInlineAvatar` (sets `avatarDataUrl: null`), `stripSummaryRows`, `stripNullableSummary`, `stripCreateAgentResult`, and the SSE-side `stripInlineAvatarsFromEvent` (handles channels `"agents"` and `"agent-upserted"`).

### 3.4 The 0.18 command table (what to implement)

`SAND_GATEWAY_COMMANDS` is compiled verbatim into three 0.18 bundles — `dist/host/host-main.cjs:500975`, `dist/electron-main/main.cjs:491830`, `dist/local-exec-daemon/main.cjs:174348` — and the three emitted slices are byte-identical (compared in full, not by digest).

```js
// A18:dist/host/host-main.cjs:500972-500976
function parseCommandArgs(body) {
  return body.length > 0 ? JSON.parse(body) : {};
}
var SAND_GATEWAY_COMMANDS = {
  getTranscript: (api) => api.getTranscript(),
  getAgentTranscript: (api, body) => api.getAgentTranscript(parseCommandArgs(body)),
  …
```

Two entry shapes only: **23 no-arg** `(api) => api.name()` and **97 arg** `(api, body) => api.name(parseCommandArgs(body))`. The 23 no-arg commands are exactly:

`getTranscript, listAgents, countAgents, listAllAutomations, isAgentNetworkEnabled, isGlobalSearchEnabled, isEgressTunnelAvailable, getSharingState, skillsCatalog, syncPluginSkills, getPluginSyncStatus, getSkillPublishTargets, getListenerIntegrations, autoUpdateBoxNow, getBoxStoreStatus, clearBoxStoreNow, getHostStatus, prepareBoxForRecreate, getTeachRecordingStatus, getTrays, clearTrays, getHostSettings, getBoxSecretsStatus`

— that list is complete; there is no remainder.

All 120 in source order:

`getTranscript, getAgentTranscript, getAgentTranscriptPage, openAgentWindowed, getAgentTranscriptWindow, openAgentTail, getAgentTranscriptTail, getAgentThread, sendPrompt, promptAcceptanceStatus, respondToWidget, resolveAutoReviewApproval, resolveLocalToolPermission, dismissWidget, submitSecret, reactToMessage, appendConnectorCard, listAgents, countAgents, searchAgents, searchMedia, createAgent, kickstartAgent, requestDiskSaverAudit, createGroup, setGroupMembers, updateAgent, deleteAgent, deleteAgents, duplicateAgent, setAgentUnread, setAgentNotificationsEnabled, setAgentNotifyOnUpdates, setAgentHiddenFromSidebar, openAgent, setWindowFocused, getAgentMemories, deleteAgentMemory, clearAgentMemories, getAgentAutomations, listAllAutomations, isAgentNetworkEnabled, isGlobalSearchEnabled, isEgressTunnelAvailable, getSharingState, createRoomFromAgent, createRoomInvite, joinSharedRoom, respondToRoomJoinRequest, createSharedRoom, addOwnAgentToSharedRoom, removeOwnAgentFromSharedRoom, setSharedRoomTyping, leaveSharedRoom, setAgentAutomationEnabled, createAgentAutomation, updateAgentAutomation, deleteAgentAutomation, runAgentAutomationNow, broadcastToAgents, getAgentWorkflows, createAgentWorkflow, updateAgentWorkflow, setAgentWorkflowEnabled, deleteAgentWorkflow, runAgentWorkflowNow, importAgentWorkflowText, importAgentWorkflowUrl, portAgentLocalSkills, getConversationOutline, skillsCatalog, syncPluginSkills, getPluginSyncStatus, getSkillPublishTargets, publishSkill, resyncPublishedSkill, unpublishSkill, getAgentChannels, connectChannel, disconnectChannel, refreshChannel, getListenerIntegrations, getListenerConnectUrl, getSubagents, getAsyncTasks, setAgentAvatarBytes, getAgentAvatar, getForeverBoxStatus, getCloudAgentInfo, ensureForeverBox, resetForeverBox, updateForeverBox, autoUpdateBoxNow, snapshotBoxStoreNow, getBoxStoreStatus, clearBoxStoreNow, updateHostNow, getHostStatus, setBoxMigrating, prepareBoxForRecreate, resumeBoxAfterRecreate, handBackForeverBox, startTeachRecording, stopTeachRecording, getTeachRecordingStatus, getTrays, dismissTray, clearTrays, uploadAttachment, readAttachmentImage, readAttachmentText, readAttachmentChunk, getHostSettings, setHostSettings, setBoxSecrets, getBoxSecretsStatus, completeMcpOAuth, requestWebAuthnCeremony, refreshMcp, listBoxMcpServers`

In 0.18 the coordinator method name *is* the gateway wire command:

```js
// A18:dist/node-agent-coordinator/main.cjs:1904
dispatchCommand(method, args, init) {
  if (method === "sendPrompt") return this.sendPrompt(args);
  if (method === "getForeverBoxStatus" || method === "ensureForeverBox") return this.foreverBoxStatusCommand(method, args, init);
  if (method === "listAgents" || method === "countAgents") return this.boundedRosterRead(method, init);
  if (method === "createAgent") return this.createAgentWithRetry(args, init);
  if (method === "setDevGatewayOffline") { … }
  if (method === "setGatewayPaused")     { … }
  return this.command(method, args, init);       // the uniform POST path
}
```
```js
// A18:dist/node-agent-coordinator/main.cjs:2566-2568, :2586
function isCoordinatorMethod(name) { return Object.hasOwn(COORDINATOR_METHOD_TABLE, name); }
function createGatewayRequestDispatch(client, serves = isCoordinatorMethod) { … `no coordinator method named ${method}` … }
```
`COORDINATOR_METHOD_TABLE` spans `:2479-2565` (85 rows) and carries the `reply:` discriminator 0.29/0.30 dropped from the edge table:

```js
// A18:dist/node-agent-coordinator/main.cjs:2480-2482
getAgentTranscriptTail: { args: "object", reply: "transcript-page" },
openAgentTail:          { args: "object", reply: "transcript-page" },
sendPrompt:             { args: "object", reply: "send-result" },
```

In 0.29/0.30 that discriminator lives in a separate policy table (`mR` in 0.29, base `pR`; `Iw` in 0.30, base `xw`) with **121 / 122 effective entries** once the leading spread is counted — 25 gateway methods carry no reply policy in each version. Its full `reply` value set is 15 values, identical in both: `acceptance-lookup, array, boolean, box-secrets, box-status, channels-view, connect-url, count, host-status, import-result, record, record-or-null, send-result, transcript-page, void`. `args` takes exactly two values: `"none" | "object"`.

**0.18 → 0.29 gateway delta.** Removed 11, mostly the box-lifecycle/migration cluster: `appendConnectorCard, autoUpdateBoxNow, clearBoxStoreNow, deleteAgent` (singular; `deleteAgents` survives), `getBoxStoreStatus, prepareBoxForRecreate, resetForeverBox, resumeBoxAfterRecreate, setAgentWorkflowEnabled, setBoxMigrating, snapshotBoxStoreNow`. Added 37 = bot templates (6) + MCP management (14) + voice calls (5) + user forms/drafts (4) + 8 singletons (`createAgentFromTemplate, interruptAgentRun, voteFeedback, transcribeAudio, generateAgentAvatarImage, getAgentNotificationAvatar, getAutomationWebhookCredential, injectChromeCookies`). **0.29 → 0.30:** `+getBotTemplateExportPolicy` (position 28), `+resolveVirtualCardApproval` (position 14), `−isAgentNetworkEnabled`. Nothing else reordered.

Whether the removed 0.18 commands still exist *server-side* in 0.29/0.30 cannot be determined — the server is not shipped. **Unverified.**

---

## 4. The stable spine — safe ground for og-wire

Everything in this section is **identical across 0.18, 0.29 and 0.30**. Where a version is minified, "identical" means identical modulo minifier identifier renaming — never byte-identical, because the minifier renames every binding. Build og-wire on these and the additive deltas in §5 become opt-in.

### 4.1 Transport

`POST /api/<method>` with `Content-Type: application/json` and `JSON.stringify(args ?? {})`; `GET /events` SSE with `?channels=`; `GET /health`; `Authorization: Bearer`. The 0.29 wire-constant line is one statement:

```js
// E29:dist/node-agent-coordinator/main.cjs
var ol="/api",Cy="/events",ky="/health",Iy="Bearer",Ny="x-sand-slim-avatars",Oy="x-sand-mint-dedupe",Py="traceparent";
```
0.30 equivalents: api prefix `su`, traceparent `qg`; the fetch shape is unchanged. `/avatars` and `/prepare-upgrade` are absent from the *shipped* 0.29/0.30 wire module — but the client we serve is 0.18, which uses both.

### 4.2 Threadable entry kinds — four, unchanged

```js
// A18:dist/host/host-main.cjs:508268 — module banner // src/shared/transcript.ts
var SAND_REACTION_SELF = "me";
var SAND_REACTION_AGENT = "agent";
function getEntryReplyTo(entry) {
  if (entry.kind === "message" || entry.kind === "send-message"
   || entry.kind === "user-attachment" || entry.kind === "notice") return entry.replyTo;
  return void 0; }
function isBranchedEntry(entry) { /* same four kinds */ return entry.branched === true; }
```
```js
// E29:dist/renderer/assets/index-Dklr8uwG.js @3716485 — the same partition, later kinds explicitly non-threadable
function wCe(t){switch(t.kind){case"message":case"send-message":case"user-attachment":case"notice":return t.replyTo;
  case"tool-call":case"event":case"feedback":case"voice-call":return;default:return}}
```
`SAND_REACTION_SELF = "me"` survives as `jb` (0.29 @1509117) and `jk` (0.30 @1515535). `SAND_AUTO_REVIEW_STALE = "auto-review/stale"` and its message string `"The Auto-review request is stale, expired, or not authorized."` are unchanged in all three (`Jx`/`Uut` 0.29; `Zx`/`Qut` 0.30).

### 4.3 `event.type` union — four values, never changed

```js
// A18:dist/renderer/assets/index-UbX-y3il.js @5105634 (nIn)
// E29:dist/renderer/assets/index-Dklr8uwG.js @3460734 (ZJt)
// E30:dist/renderer/assets/index-DCpFUyZ2.js @3480469 (ken)
["name-changed","channel-connected","channel-disconnected","automation-changed"]
```
Confirmed host-side in 0.18 at `dist/host/host-main.cjs:637978` (`type: "name-changed"`) and `:643634` (`case "name-changed":`). Note the 0.18 host's own `describeTimelineEvent` is **open-ended** — no `_exhaustive` witness, it falls through:

```js
// A18:dist/host/host-main.cjs:643632-643647
function describeTimelineEvent(event){ switch(event.type){ …4 cases… }
  return fallbackForUnknownTimelineEvent(event, "Updated this conversation"); }
function fallbackForUnknownTimelineEvent(_event, fallback2) { return fallback2; }
```
`OURS/shared/sand-timeline-events.ts` reproduces that helper faithfully; only its TypeScript `{ readonly type: string; [key: string]: unknown }` union arm is a local widening.

### 4.4 Activity — kind union and wire shape, unchanged

```js
// A18:dist/host/host-main.cjs:645598 — module banner // src/host/sand-activity.ts
var MAX_ACTIVITY_DETAIL_CHARS = 80;
var THINKING_ACTIVITY = { kind: "thinking" };
var SURFACE_UNRESOLVED_TOOL_CASES = new Set(["shellToolCall","readToolCall","awaitToolCall"]);
function deriveToolCallActivity(input) {
  const { tool, detail, target } = resolveToolCall(input, parseArgsJson(input.args));
  return { kind: "tool", tool,
    ...(detail?.length>0 ? { detail: clampLine(detail, MAX_ACTIVITY_DETAIL_CHARS) } : {}),
    ...(target?.length>0 ? { target } : {}), callId: input.id };
}
```
```js
// E29:dist/node-agent-coordinator/main.cjs @~150015 (activityOf) — E30 @~160015, same body
function <activityOf>(t){let e=t.activity;
  if(e!==void 0 && !(e.kind!=="thinking" && e.kind!=="tool"))
    return {kind:e.kind, ...e.tool.length>0?{tool:e.tool}:{}, ...e.detail.length>0?{detail:e.detail}:{},
            ...e.target.length>0?{target:e.target}:{}, ...e.callId.length>0?{callId:e.callId}:{}}}
```
Wire shape `{ kind, tool?, detail?, target?, callId? }` in every version. The MCP-tool set (→ verb `connecting`) and the subagent set (→ `waiting`) are identical in all three:

```js
// A18:index-UbX-y3il.js @2294419 (dse) · E29 @2092286 (qb) · E30 @2111502 (Uk)
new Set(["GetMcpTools","McpAuth","SearchPlugins","GetPlugin","InstallPlugin","UninstallPlugin",
         "GetMcpServerStatus","AddMcpServer","UninstallMcpServer","AuthenticateMcpServer",
         "RestartMcpServers","SetMcpInstructions","SearchMcpServers","InstallMcpServer","EnableTeamServer"])
new Set(["CheckSubagent","MessageSubagent","StopSubagent"])
```

### 4.5 Computer actions — four types, unchanged (0.18 now verified)

```js
// A18:dist/host/host-main.cjs:660877-660899 — toReportedAction(args)
//   emits exactly "drag" (from dragPath(args)[0]), "move", "scroll", "click"; everything else → undefined
// emitted at A18:dist/host/host-main.cjs:667965
onComputerAction: ({agentId, action}) => deps.emitGatewayEvent({channel:"computer-action", payload:{agentId, ...action}})
```
```js
// E29 computerActionEventOf · E30 same
t.type!=="click"&&t.type!=="move"&&t.type!=="scroll"&&t.type!=="drag" || !isFinite(t.x)||!isFinite(t.y) ? null
  : {agentId,type,x,y, ...button.length>0?{button}:{}, ...count>0?{count}:{}}
```
One shape delta worth knowing: 0.18 emits `button` (default `"left"`) and `count` (default `1`) **always, and only on `click`**; the 0.29/0.30 projector emits them conditionally for every type.

### 4.6 SSE families — the 16 that never moved

```js
// A18:dist/node-agent-coordinator/main.cjs @87813 — src/node-agent-coordinator/gateway/gateway-event-families.ts
var SSE_CHANNEL_BY_FAMILY = {
  transcript:"transcript", agents:"agents", "agent-upserted":"agent-upserted", tray:"tray",
  "agents-workflow":"workflows", subagents:"subagents", "async-tasks":"async-tasks",
  "agents-automation":"automations", "mcp-servers-updated":"mcp-servers",
  "forever-box":"forever-box", "teach-recording":"teach-recording",
  "box-disk-pressure":"box-disk-pressure", "computer-action":"computer-action",
  outline:"outline", sharing:"sharing", "host-settings":"host-settings"
};
```

| family (coordinator event name) | SSE channel (gateway) | 0.18 | 0.29 | 0.30 | `OURS` |
|---|---|:--:|:--:|:--:|:--:|
| `transcript` | `transcript` | ✅ | ✅ | ✅ | ✅ |
| `agents` | `agents` | ✅ | ✅ | ✅ | ✅ |
| `agent-upserted` | `agent-upserted` | ✅ | ✅ | ✅ | ✅ |
| `tray` | `tray` | ✅ | ✅ | ✅ | ✅ |
| `agents-workflow` | **`workflows`** | ✅ | ✅ | ✅ | ✅ |
| `subagents` | `subagents` | ✅ | ✅ | ✅ | ✅ |
| `async-tasks` | `async-tasks` | ✅ | ✅ | ✅ | ✅ |
| `agents-automation` | **`automations`** | ✅ | ✅ | ✅ | ✅ |
| `mcp-servers-updated` | **`mcp-servers`** | ✅ | ✅ | ✅ | ✅ |
| `forever-box` | `forever-box` | ✅ | ✅ | ✅ | ✅ |
| `teach-recording` | `teach-recording` | ✅ | ✅ | ✅ | ✅ |
| `box-disk-pressure` | `box-disk-pressure` | ✅ | ✅ | ✅ | ✅ |
| `computer-action` | `computer-action` | ✅ | ✅ | ✅ | ✅ |
| `outline` | `outline` | ✅ | ✅ | ✅ | ✅ |
| `sharing` | `sharing` | ✅ | ✅ | ✅ | ✅ |
| `host-settings` | `host-settings` | ✅ | ✅ | ✅ | ✅ |
| `mcp-auth-completed` | **`mcp-auth`** | ❌ | ✅ | ✅ (declared) | ❌ |
| `agent-activity` | `agent-activity` | ❌ | ✅ | ✅ | ❌ |
| `client-side-tool-v2` | `client-side-tool-v2` | ❌ | ❌ | ❌ | **ours only** |

Four families are renamed on the wire (bold); the rest are pass-through. `client-side-tool-v2` in `OURS/node-agent-coordinator/gateway/gateway-event-families.ts` appears in **no** official version (0 occurrences in all three coordinators).

### 4.7 Card protocol keys

```js
// A18:index-UbX-y3il.js @~5110400 (tPe)
switch(n.kind){case"message":return"message:message";
  case"send-message":return `send-message:${n.message.type}`;
  case"user-attachment":return"user-attachment:user-attachment";
  case"notice":return"notice:notice";
  case"event":return `event:${n.event.type}`;
  case"tool-call":return null}
```
0.29 (`IH`) / 0.30 (`BH`) add only `case"feedback":return"feedback:feedback"` and `case"voice-call":return"voice-call:voice-call"`. **`tool-call` returns `null` in all three** — tool-call entries have never had a card and are never rendered as a transcript row. Placeholder heights unchanged: 32 (event) / 60 (default).

### 4.8 Send-message preview — the 0.18 function is the spec, and it is unchanged in substance

```js
// A18:dist/host/host-main.cjs — module banner // src/shared/send-message-preview.ts at :631644
var CURSOR_AGENT_FALLBACK_PREVIEW = "Cursor cloud agent";
function sendMessagePreviewText(message) { switch (message.type) {
  case "text": return message.content;                 case "attachment": return message.url;
  case "widget": return message.widget.prompt;         case "cursor-agent": return cursorAgentPreviewText(message.title);
  case "secret-request": return message.secretRequest.label;
  case "email-draft": return message.draft.subject || message.draft.body;
  case "slack-draft": return message.draft.body;       case "permission-request": return message.permission.title;
  case "auto-review-approval": return `Approval required: ${message.approval.summary}`;
  case "local-tool-permission": return `Permission required: ${message.ask.target}`;
  case "connector": return message.variant === "connected" ? `${message.connector} connected` : `Connect ${message.connector}`;
  case "connectors": return connectorsPreviewText(message.connectors);
  case "listener-connect": return `Connect ${message.platform === "slack" ? "Slack" : "GitHub"}`;
} const _exhaustive = message; void _exhaustive; return "Message"; }
```
`OURS/shared/send-message-preview.ts` matches this arm-for-arm and string-for-string, including both helpers. 0.29/0.30 replaced the literals with i18n catalog ids, but the **English strings are literally unchanged**: `"4wavf2":["Approval required: ",["summary"]]`, `"E3bWnA":["Permission required: ",["target"]]`, `"jozt9B":["Cursor cloud agent"]`, `"BUlkkQ":["Connect ",["connector"]]`, `"xDAtGP":["Message"]` (catalog inlined in `index-Dklr8uwG.js` / `index-DCpFUyZ2.js`, not in a locale chunk).

### 4.9 0.18-only host-side threading rewriters

`A18:dist/host/host-main.cjs:640386` (`stripReplyTo`) / `:640447` (`withReplyTo`), under banner `// src/host/extensions/transcript/send-message-shaping.ts` at `:640300`, both switch exhaustively over the 13 card types:

> `// The host-authored auto-review and local-tool-permission cards never carry reply_to.`

`auto-review-approval`, `local-tool-permission`, `email-draft`, `slack-draft` are returned untouched; the other 9 get `reply_to` added/stripped. Whether 0.29/0.30 retain this is **unverified** (host-side; the host is not in those bundles). Since we are the host, implement 0.18's behaviour.

---

## 5. Deltas that change backend scope

### 5.1 Transcript entry kinds — 6 → 8, and an id precondition

```js
// A18:dist/host/host-main.cjs:628621 — isValidTranscriptEntry
if (entry == null || typeof entry !== "object") return false;
switch (entry.kind) {
  case "send-message": { if (entry.message == null) return false;
    return entry.message.type !== "text" || typeof entry.message.content === "string"; }
  case "message":          return typeof entry.content === "string";
  case "user-attachment":  return typeof entry.file_path === "string";
  case "tool-call":        return typeof entry.name === "string";
  case "notice":           return typeof entry.text === "string";
  case "event":            return entry.event != null && typeof entry.event === "object"
                                  && typeof entry.event.type === "string";
  default: { const _exhaustive = entry; void _exhaustive; return false; }
}
```
```js
// E29:dist/electron-main/main.cjs @~5442123 · E30:dist/electron-main/main.cjs @~7117808 (same body)
if (typeof e.id!="string"||e.id.length===0) return !1;      // ← new precondition
switch (e.kind) {
  case "send-message": return e.message==null?!1:e.message.type!=="text"||typeof e.message.content=="string";
  case "message":         return typeof e.content=="string";
  case "user-attachment": return typeof e.file_path=="string";
  case "tool-call":       return typeof e.name=="string";
  case "notice":          return typeof e.text=="string";
  case "voice-call":      return e.call!=null&&typeof e.call.callId=="string";     // ← new
  case "event":           return e.event!=null&&typeof e.event=="object"&&typeof e.event.type=="string";
  case "feedback":        return typeof e.requestId=="string";                     // ← new
  default: { let t=e; return !1 }
}
```

Also: 0.18 has a separate `parseTranscriptEntry` at `A18:dist/host/host-main.cjs:628645` that **rejects `kind === "error"` before validating** (`if (entry.kind === "error") return null;`), making `"error"` a transient wire-only kind that is never durable. 0.29/0.30's `transcriptEntryOfJson` is just `validator(n) ? n : null` — the special case is gone. (The surviving `kind==="error"` in 0.29/0.30 `electron-main` belongs to the Chrome-import worker protocol, unrelated.)

Two companion enumerations are **stale** and must not be mistaken for the union:

```js
// telemetry known-kind list — E29:dist/electron-main/main.cjs @5428252 (Mcn) · E30 @5817496 (v_n)
["message","tool-call","send-message","user-attachment","notice","event"]           // never updated for the 2 new kinds
// extended for telemetry: var rdn=[...Mcn,"unknown-kind"]; Byr=new Set(rdn)
// mirrored into the renderer: E29 @1509117 (Kye) · E30 @1515535 (oke)
```
```js
// renderer replica-persistence allowlist, slice {slice:"transcript.replicas",schemaVersion:1,scope:"host-durable",accountSensitive:!0}
// A18:index-UbX-y3il.js @5694119 (iVn)  6 kinds
// E29:index-Dklr8uwG.js @4194448 (ekn)  7 kinds (+feedback)
// E30:index-DCpFUyZ2.js @4215947 (rvn)  identical to 0.29
```
`voice-call` is deliberately absent from all three replica allowlists.

### 5.2 Send-message card types — 13 → 15 → 17, purely additive

| `message.type` | 0.18 | 0.29 | 0.30 |
|---|:--:|:--:|:--:|
| `text`, `attachment`, `widget`, `cursor-agent`, `secret-request`, `email-draft`, `slack-draft`, `permission-request`, `connector`, `connectors`, `listener-connect`, `auto-review-approval`, `local-tool-permission` | ✅ | ✅ | ✅ |
| `user-form` | ❌ | ✅ | ✅ |
| `bot-template-share` | ❌ | ✅ | ✅ |
| `virtual-card-approval` | ❌ | ❌ | ✅ |
| `cookie-origin-approval` | ❌ | ❌ | ✅ |
| **total** | **13** | **15** | **17** |

New payload accessors visible in 0.30's preview switch: `case"user-form":return t.formRequest.title;`, `case"virtual-card-approval":return t.approval.title;`. New catalog ids: `"Im6Sey"` (cookie-origin-approval), `"GSpELP":["Shared bot template: ",["name"]]`.

Nothing was ever removed or renamed. `OURS/shared/send-message-preview.ts` is correct for 0.18 and is 4 card types behind 0.30 — which is fine, because our client is 0.18.

### 5.3 What 0.30 ADDED (and therefore what a 0.30-targeting backend would owe)

**Messages ops (macOS Messages.app bridge).** A whole new family on the local-exec daemon wire, plus two new tool actions and per-op mandatory approval.

```js
// E30:dist/local-exec-daemon/main.cjs — request union H1t, guard V1t
Me({kind:Je("messages-op"),requestId:H(),op:dx,approvalId:H().optional()})
var V1t=new Set(["exec","upload","download","retire-approval","cancel","welcome","messages-op"]);
// response union W1t, guard K1t
Me({kind:Je("messages-result"),requestId:H(),result:rY}),
Me({kind:Je("messages-error"),requestId:H(),error:H()}),
var K1t=new Set(["hello","client","control","file","file-error","messages-result","messages-error","ping"]);
// hello gains:
capabilities:Me({messagesOp:ar().optional(),messagesOpGeneration:kt().int().positive().optional()}).optional()
```
```js
// E30:dist/local-exec-daemon/main.cjs — the op union (dx), validated daemon-side
var k5e={kind:Je("send"),text:H(),service:On(["iMessage","SMS","auto"]).optional()},
    eY=kt().int().min(1).optional(),
    C5e=ll({date:$a,id:kt().int()}).optional();      // $a = ISO datetime with offset
```

| op kind | fields |
|---|---|
| `find-chats` | `displayName?`, `handle?`, `limit?` (int ≥1) |
| `items` | `chatGuid?`, `limit?`, `before?{date,id}` |
| `search` | `needle`, `limit?`, `before?{date,id}` |
| `activity` | `since` (ISO+offset), `until?` |
| `send` (to-variant) | `text`, `service?`, `to` |
| `send` (chat-variant) | `text`, `service?`, `chatId` |
| `check-permissions` | *(none)* |
| `fetch-attachment` | `messageGuid`, `attachmentGuid` |

Result union `N5e` → refined `rY`: `find-chats{chats[]}`, `items{items[]}`, `search{items[]}`, `activity{items:[{chatGuid,count}]}`, `send{text,service,via,verified,to?,chatId?}`, `check-permissions{version,verbs[],fullDiskAccess,automation:"granted"|"denied"|"unknown"}`, `fetch-attachment{filename,mime,bytesBase64}`. Shared paging fields `{total?, truncated?:"limit"|"bytes"}`, `nextBefore`. `via` is a free string on the wire but the helper only ever produces `"chat_id"` or `"participant"`.

`SandLocalToolAction` grows in lockstep across daemon, preload and electron-main:
```js
// E29 daemon Dat / preload Gr / electron-main O7n
["run-command","send-input","read-file","list-directory","write-file"]
// E30 daemon kdt / preload Xr / electron-main p_r
["run-command","send-input","read-file","list-directory","write-file","read-messages","send-imessage"]
```

**Marketplace.** New gateway/main methods (`listPublicBotMarketplace`, `getBotTemplateExportPolicy`), new gate `sand_marketplace_bots` (OFF), and a substantial new proto family — creators, categories, public browse, source preview — plus a **breaking reshape** of the existing listing family (see §6.5).

**Virtual card.** Gateway method `resolveVirtualCardApproval`, card type `virtual-card-approval`, RPCs `RaiseGrokBotVirtualCard` / `ResolveGrokBotVirtualCardApproval`, three new enums.

**Enterprise usage.** `getCursorEnterpriseUsage` on the `main` edge only — desktop-local, not a gateway command; no backend surface.

**Cookie origin approval.** `presentCookieOriginApproval` / `cancelCookieOriginApproval` on `coordinator-control`, card type `cookie-origin-approval`.

**Multi-machine local exec.** `isMultiMachineLocalExecEnabled` on `coordinator-control`, backed by gate `sand_multi_machine_local_exec` (OFF in both 0.29 and 0.30).

**Second authorization axis on local exec.** 0.29's gate call is `{approvalId, authorizedByStanding, describes, terminalsFolder}`; 0.30 adds `authorizedByApproval`, with matching protobuf fields `GrokBotUserComputerExec.4`, `Upload.5`, `Download.4` (all `bool`).

### 5.4 What 0.30 DELETED

| deleted | evidence |
|---|---|
| **Org chart / agent network** | `isAgentNetworkEnabled` removed from the `gateway` edge and from the gateway method list. In 0.29 it occurs in 3 files (`node-agent-coordinator/main.cjs`, `electron-main/main.cjs`, `renderer/assets/index-Dklr8uwG.js`); in 0.30 it occurs in **0 files** across the whole `dist/` tree. The gate `sand_agent_network` survives but is OFF. 0.18's doc comment on that gate: *"the Cmd-K palette's 'Open Org Chart' command and the org-chart primary view… When OFF (the default for everyone) the command is hidden and the org chart is unreachable."* |
| **Threads tray** | gate `sand_enable_threads_tray` present in 0.29, absent from 0.18 and from 0.30 — born and died between the two archived versions. |
| **Accent preference** | `setAccentPreference` removed from the `main` edge. 4 files in 0.29 (`electron-main/main.cjs`, `electron-preload/preload.cjs`, `local-exec-daemon/main.cjs`, `renderer/assets/index-Dklr8uwG.js`); **0 files** in 0.30, and even the substring `accentPreference` has zero hits. Note the *gate* `sand_enable_accent_theming` is still in the 0.30 registry — a gate surviving its RPC. |
| (also gone before 0.29) | `dev-controls` edge and its 33-method table; `setAgentWorkflowEnabled`; `deleteAgent` (singular); the 0.18 box-lifecycle gateway cluster listed in §3.4. |

**A backend does not need to implement any of these.** They are unreachable from the client that removed them, and for 0.18 (our client) only the org-chart command `isAgentNetworkEnabled` is even present — and it is a boolean read that can safely return the gate's default, `false`.

### 5.5 Flag-gated OFF by default in both 0.29 and 0.30

Every gate is `client: true` in all three versions — there is not one `client:false` entry — so the registry is fully client-visible and the server enforces separately. Only two carry a third property:

```js
// E30:dist/electron-main/main.cjs (registry vqt) — identical pair in E29 (registry GJt)
sand_scheduled_computer_updates:{client:!0,default:!1,requiresAuthenticatedBootstrap:!0}
sand_five_min_automation_floor:{client:!0,default:!1,requiresAuthenticatedBootstrap:!0}
```

**Voice calls — dark in both versions.** `sand_voice_call:{client:!0,default:!1}` in 0.29 and 0.30. Behind it: gateway methods `getVoiceCall, nudgeVoiceCall, recordVoiceCall, readVoiceCallAgentContext, readVoiceCallSentMessages`; main method `mintVoiceCallCredential`; RPC `MintSandVoiceCallSecret`; the `voice-call` entry kind (which is *not* in the renderer replica allowlist). **A backend need not implement voice calls.**

Other whole features dark by default in 0.30 (all `{client:!0,default:!1}`):

| gate | what is dark | 0.18 doc comment where one exists |
|---|---|---|
| `sand_multiplayer` | cross-user sharing end-to-end | *"share links for one agent, owner-approved access grants, the shared per-agent transcript room, and cross-user group chats… Evaluated on the backend for every sharing endpoint (fail closed) AND pinned at Sand host startup… Default OFF."* |
| `grok_bot_multiplayer` | GrokBot-side twin (new since 0.18) | — |
| `sand_agent_network` | org chart | quoted in §5.4 |
| `sand_channels` | channels (new since 0.18) | — |
| `sand_share_bot`, `sand_marketplace_bots` | bot sharing, marketplace (`sand_marketplace_bots` new in 0.30) | — |
| `sand_teach_by_demonstration` | box screen recorder + learn-from-demonstration skill | *"…both key off this one gate, so a bad rollout is a single kill switch."* |
| `sand_memory_dreaming` | replacement memory pipeline | *"control users keep legacy extraction and treatment users get background synthesis."* |
| `sand_create_temporal_agents`, `grok_bot_temporal_harness` | temporal agents | — |
| `sand_enable_account_switching` | multi-account | — |
| `sand_user_form`, `sand_usage_page`, `sand_on_demand_settings`, `sand_special_settings`, `sand_notification_sounds`, `sand_enable_accent_theming` | whole settings/card surfaces | — |

**The entire local→server storage migration is dark**, which is the single most consequential scope statement in this document: `sand_transcript_double_write`, `sand_transcript_store_read`, `sand_transcript_store_first`, `sand_transcript_server_tail`, `sand_mobile_transcript_server_tail` (new in 0.30), `sand_send_via_server`, `sand_roster_via_server`, `sand_attachments_via_server`, `sand_new_transcript_journal`, `sand_legacy_store_blob_retirement` — every stage, OFF, in both versions. The local gateway host stays authoritative for transcript, roster, sends and attachments. `sand_new_transcript_journal` carries a one-way ratchet: *"Once treatment claims a transcript, its durable mode marker keeps that conversation on the journal even if this gate turns off."*

Identity/billing (all new since 0.18, all OFF): `grok_bot_durable_identity`, `grok_bot_durable_identity_writes`, `grok_bot_shared_identity`, `grok_bot_identity_backfill` (new in 0.30), `grok_bot_stripe_link` (new in 0.30).

Security/egress/privacy (all OFF): `sand_box_egress_tunnel`, `sand_anonymized_egress_telemetry`, `sand_web_bot_auth_signing`, `sand_web_bot_auth_sign_xhr_fetch`, `sand_browser_fingerprint_spoof`, `sand_browser_ua_token_kill_switch`, `sand_action_audit_logs`, `sand_import_chrome_cookies`, `agent_prompted_cookie_sync`, `sand_codebase_telemetry`. Note the asymmetry: `sand_product_analytics` is **ON** while `sand_anonymized_egress_telemetry` and `sand_action_audit_logs` are OFF.

**Only 9 of the 77 `sand_*` gates are ON by default in 0.30** (identical set in 0.29's 73): `sand_mobile_app_store_update_indicator, sand_multitask, sand_computer_use_playwright, sand_product_analytics, sand_enable_pressure_cpu_profiler, sand_notify_safety_poll, sand_global_search, sand_pr_menu, sand_spotlight`.

### 5.6 Gate registry mechanics a backend should know

- Three registries are declared back-to-back and conflating them produces wrong totals: `FLAGS` (feature gates, `{client, default}`), `EXPERIMENTS` (`{client, fallbackValues}`), `DYNAMIC_CONFIGS` (`{client, fallbackValues}`). 0.18 also declares a fourth, `DYNAMIC_CONFIG_SCHEMAS` (123 keys, zod schemas paired to the configs), which exists only in the unminified form.
- Anchors: `var FLAGS = {` at `A18:dist/host/host-main.cjs:605331`; the same registry, keys/order/values/comments identical after whitespace normalization (the two bodies differ only in indentation — 121,080 vs 135,340 bytes), at `A18:dist/electron-main/main.cjs:452255` as `FLAGS = {` **without `var`**. The version-agnostic anchor for all three versions is the first key, `agent_goal_continuation`.
- 0.29 registry `GJt` (36,257 B), 0.30 registry `vqt` (36,715 B), both in `dist/electron-main/main.cjs`. Across the whole 0.30 `dist/` tree exactly one file contains the substring `client:!0,default` — there is no second registry in the renderer or anywhere else.
- **Zero default flips 0.18 → 0.30**, across all 580 gates present in both. Defaults are set once at gate birth and never edited in the literal; ramping is entirely server-side in Statsig.
- 0.29 → 0.30: 14 added (all default-OFF), 5 removed (all had been default-OFF), 0 flips. 0.18 → 0.30: 128 added (exactly one default-ON: `sand_mobile_app_store_update_indicator`), 34 removed (9 had been default-ON). Reconstruction key: **708 − 128 + 34 = 614**.
- Adjacent registries 0.29 → 0.30: `DYNAMIC_CONFIGS` **+`grok_bot_loop_detection`** (only change, 136 → 137); `EXPERIMENTS` **−`cursor_learn_banner_placement`** (only change, 112 → 111).
- Gate semantics come from the 0.18 doc comments — but only **190 of 614 gates (31%)** carry one. Coverage is far better for `sand_*`: 34 of 37, the exceptions being `sand_computer_use_playwright`, `sand_auto_update_when_idle`, `sand_renderer_heap_metrics`.
- Model selection is **not gated**: `sand_default_model` is a dynamic config, and its 0.18 comment says it *"replaces the retired `sand_default_model_auto` boolean gate"* (`A18:dist/host/host-main.cjs:610684`).
- The 102 unique `sand_*` identifiers in the 0.30 `dist/` tree = 77 gates + 15 `DYNAMIC_CONFIGS` keys (`sand_process_metrics, sand_rpc_tracing, sand_min_client_version, sand_mobile_version_support, sand_computer_use_playwright_config, sand_browser_use_model, sand_share_bot_export_policy, sand_model_filter, sand_default_model, sand_automations_model, sand_feedback_prompt_config, sand_pressure_cpu_profiler_config, sand_stream_deadline_config, sand_internal_release_track_override, sand_host_bundle_channel`) + 2 `EXPERIMENTS` keys (`sand_model_selection`, `sand_grok_bot_slim_system_prompt`) + **8 that are none of those**: `sand_action_audit_settings` and `sand_onboarding` are `aiserver.v1` message field names; `sand_trial_expires_at` and `sand_trial_cancelable` are proto fields; `sand_area`, `sand_operator_error`, `sand_unknown_send_message_type` are Sentry tag keys; `sand_version` is a telemetry base prop (`this.baseProps={client:"sand",sand_version:kc(),flavor:…}`). Scanning binaries too yields 119, the extra 17 being mangled Rust symbols from the `sand_webauthn_signer` native module.

### 5.7 Local-exec daemon constants (unchanged 0.29 → 0.30)

| constant | 0.29 | 0.30 |
|---|---|---|
| control POST deadline | `1e4` | `1e4` |
| data POST deadline | `12e4` | `12e4` |
| SSE connect deadline | `15e3` | `15e3` |
| stall watchdog | `35e3` | `35e3` |
| backoff base / cap | `1e3` / `1e4` | `1e3` / `1e4` |
| retry attempts | 3 (×2 paths), 200 ms | same |
| max single-file bytes | `100*1024*1024` | default `104857600` |
| max upload frame bytes | `Math.ceil(t*4/3)+64*1024` | identical |
| auth scheme | `"Bearer"` | `"Bearer"` |

Oversize messages, verbatim and identical in both (`E29,E30:dist/local-exec-daemon/main.cjs`):

```
File is ${MiB}, which exceeds Grok Bot's ${MiB} limit for reading or transferring a single file over local-exec. Read a slice with offset/limit, or use a shell command (grep, head, tail) to extract just what you need.
The upload exceeds Grok Bot's ${MiB} limit for transferring a single file over local-exec and was refused before being read into memory. Transfer a smaller file, or split it into parts.
```

---

## 6. Proto inventory

> **Inventory only — do not implement from this file.**
>
> Names and field names are transcribed for orientation and diffing. No generated stubs are vendored in this repo (invariant #3) and no wire encoding should be inferred from this section. Field numbers are given only where the 0.29 → 0.30 reshape retired one, because that is a compatibility fact, not an implementation aid.

Encoding in all bundles is protobuf-es / connect-es generated tables: `X.typeName="aiserver.v1.Y"; X.fields=i.util.newFieldList(()=>[{no:1,name:"…",kind:"scalar",T:9},…])`, enums via `i.util.setEnumType(V,"aiserver.v1.E",[{no,name}…])`, services via `{typeName:"aiserver.v1.S",methods:{jsName:{name:"RpcName",I:…,O:…,kind:w.Unary}}}`.

### 6.1 Services

| service | 0.18 | 0.27 | 0.29 | 0.30 |
|---|---:|---:|---:|---:|
| `aiserver.v1.DashboardService` | 601 | ? | 616 | 627 |
| `aiserver.v1.BackgroundComposerService` | 194 | ? | 224 | 224 |
| `aiserver.v1.AiService` | 192 | ? | 193 | 193 |
| `aiserver.v1.AutomationsService` | 42 (host-main only) | ? | — | — |
| `aiserver.v1.SandBoxService` | — | **37** | 37 | 37 |
| `aiserver.v1.GrokBotService` | **30** | **76** | **89** | **106** |
| `aiserver.v1.AnalyticsService` | 8 | ? | 8 | 8 |
| `aiserver.v1.InferenceService` | 3 (host-main only) | ? | — | — |

*Provenance:* 0.18 `A18:dist/host/host-main.cjs` and `A18:dist/electron-main/main.cjs`; 0.27 `F27/grok_bot_service-0.27.fragment.js`, `F27/sand_box_service-0.27.fragment.js`; 0.29/0.30 `E29,E30:dist/{electron-main,node-agent-coordinator,local-exec-daemon}/main.cjs` (the `GrokBotService` table is identical across all three bundles within a version).

`SandBoxService`'s 37 method names are byte-identical across 0.27 / 0.29 / 0.30 and are exactly `GrokBotService` rows #1–37 *in sequence* — `GrokBotService` is a strict superset that absorbed the whole Sand* surface. 0.18's `dist/node-agent-coordinator/main.cjs` contains **zero** `aiserver.v1` names; the proto surface arrived in the coordinator after 0.18.

### 6.2 `GrokBotService` trajectory

- **0.18 → 0.27: +46, 0 removed.** 39 GrokBot-domain RPCs plus 7 Sand* additions (`AdminListSandBoxStoreManifestVersions, AdminRestoreSandBoxStoreSnapshot, GetSandBoxUpgradeSchedule, ScheduleSandBoxUpgrade, CancelSandBoxUpgrade, RescheduleSandBoxUpgrade, MintSandVoiceCallSecret`). 30 + 39 + 7 = 76.
- **0.27 → 0.29: +13, 0 removed.** 11 appended at the tail (5 Slack + 6 Marketplace-Internal) and **2 inserted mid-table**: `ReportGrokBotClientPresence` at row 43 and `SetGrokBotAgentVisibility` at row 48. Ordinal positions shift; field numbering does not.
- **0.29 → 0.30: +17, 0 removed.**
- **No method was ever removed at any hop.** I/O types and streaming kinds are unchanged 0.29 → 0.30 for all 89 surviving methods (all resolved and diffed). For 0.27, 39 of 76 rows resolve against `F27/grok_bot_pb-0.27.fragment.js` and match; **the 37 Sand* rows are unverified** — the 0.27 fragments contain no Sand* type table.

0.18's 30, all Sand*: `EnsureSandBox, EnsureSandBoxWindow, RecreateSandBox, ForceRecreateSandBox, AdminRecreateSandBox, AdminForceRecreateSandBox, PresignSandBoxStoreWrites, CompleteSandBoxStoreMultipartWrites, AbortSandBoxStoreMultipartWrites, PresignSandBoxStoreReads, StatSandBoxStoreObject, ListSandBoxStoreObjects, AdminGetSandBoxStoreStatus, AdminUpdateSandBoxHost, AdminGetSandBoxHostStatus, AdminSnapshotSandBoxStore, AdminHibernateSandBox, AdminListSandAgents, AdminGetSandAgentTranscriptPage, WatchSandBoxMigration, AdminWatchSandBoxMigration, GetSandBoxRunState, ListSandBoxes, NotifySandAgentTurnFinished, ListSandSetupManifests, ListTeamSandSetupManifests, SaveTeamSandSetupManifest, DeleteTeamSandSetupManifest, ListTeamMemberSandBoxes, KillTeamMemberSandBox`.

### 6.3 `GrokBotService` — 0.30, all 106 in declaration order

`*` = new in 0.30, `†` = new in 0.29, `»` = ServerStreaming (5 total: rows 22, 23, 40, 77, 81; all others Unary). Unless noted, request/response are `<Name>Request` / `<Name>Response`.

1 EnsureSandBox · 2 EnsureSandBoxWindow *(→ EnsureSandBoxResponse)* · 3 RecreateSandBox · 4 ForceRecreateSandBox *(→ RecreateSandBoxResponse)* · 5 AdminRecreateSandBox *(→ RecreateSandBoxResponse)* · 6 AdminForceRecreateSandBox *(→ RecreateSandBoxResponse)* · 7 PresignSandBoxStoreWrites · 8 CompleteSandBoxStoreMultipartWrites · 9 AbortSandBoxStoreMultipartWrites · 10 PresignSandBoxStoreReads · 11 StatSandBoxStoreObject · 12 ListSandBoxStoreObjects · 13 AdminGetSandBoxStoreStatus *(I = **AdminSandBoxStoreStatusRequest**)* · 14 AdminUpdateSandBoxHost · 15 AdminGetSandBoxHostStatus *(I = **AdminSandBoxHostStatusRequest**)* · 16 AdminSnapshotSandBoxStore · 17 AdminListSandBoxStoreManifestVersions · 18 AdminRestoreSandBoxStoreSnapshot · 19 AdminHibernateSandBox · 20 AdminListSandAgents · 21 AdminGetSandAgentTranscriptPage · **22 » WatchSandBoxMigration** *(→ SandBoxMigrationEvent)* · **23 » AdminWatchSandBoxMigration** *(→ SandBoxMigrationEvent)* · 24 GetSandBoxRunState · 25 GetSandBoxUpgradeSchedule · 26 ScheduleSandBoxUpgrade · 27 CancelSandBoxUpgrade · 28 RescheduleSandBoxUpgrade · 29 ListSandBoxes · 30 NotifySandAgentTurnFinished · 31 ListSandSetupManifests · 32 ListTeamSandSetupManifests · 33 SaveTeamSandSetupManifest · 34 DeleteTeamSandSetupManifest · 35 ListTeamMemberSandBoxes · 36 KillTeamMemberSandBox · 37 MintSandVoiceCallSecret · 38 CommitGrokBotTranscriptEntries · 39 ListGrokBotTranscriptEntries · **40 » WatchGrokBotTranscripts** *(→ **GrokBotTranscriptWatchFrame**)* · 41 SetGrokBotAgentClientState · 42 ReadGrokBotAgentAttachmentChunk · 43† ReportGrokBotClientPresence · 44 CreateGrokBotAgent · **45\* CreateGrokBotTemporalAgent** *(reuses CreateGrokBotAgentRequest/Response)* · 46 ListGrokBotAgents · **47\* GetGrokBotRuntimeCapabilities** · 48† SetGrokBotAgentVisibility · 49 UpdateGrokBotAgent · 50 DeleteGrokBotAgent · 51 CreateGrokBotTemplate · 52 ListGrokBotTemplates · 53 DeleteGrokBotTemplate · 54 SetGrokBotTemplateVisibility · 55 ActivateGrokBotTemplateVersion · 56 GetGrokBotTemplateVersion · 57 GetGrokBotTemplateForSourceAgent · **58\* GetGrokBotTemplateExportPolicy** · 59 GetPublicGrokBotTemplate · **60\* ListPublicGrokBotMarketplaceListings** · **61\* GetPublicGrokBotMarketplaceListing** · 62 GetGrokBotTemplateImportDetails · 63 CreateGrokBotAgentFromTemplate · 64 SendGrokBotUserMessage · 65 GetGrokBotSendStatus · 66 InterruptGrokBotAgentRun · 67 RespondGrokBotWidget · 68 DismissGrokBotWidget · 69 ReactToGrokBotMessage · 70 VoteGrokBotFeedback · 71 SendGrokBotAgentMessage · 72 ResolveGrokBotAutoReviewApproval · 73 ResolveGrokBotLocalToolPermission · **74\* RaiseGrokBotVirtualCard** · **75\* ResolveGrokBotVirtualCardApproval** · 76 IssueGrokBotUserComputerCredential · **77 » WatchGrokBotUserComputerRequests** *(→ WatchGrokBotUserComputerRequestsEvent)* · 78 PollGrokBotUserComputerRequests · 79 SubmitGrokBotUserComputerResponses · 80 ListGrokBotUserComputers · **81 » OpenGrokBotUserComputerRequest** *(→ **GrokBotUserComputerResponseFrame**)* · 82 CancelGrokBotUserComputerRequest · 83 EndGrokBotBoxHandoff · 84 RequestGrokBotRoomMemberTurn · 85 CancelGrokBotRoomMemberTurn · 86† GetGrokBotSlackInstallState · 87† StartGrokBotSlackConnect · 88† InstallGrokBotSlackApp · 89† ReinstallGrokBotSlackApp · 90† UninstallGrokBotSlackApp · 91† CreateGrokBotMarketplaceListingInternal *(→ **GrokBotMarketplaceListing**)* · 92† ListGrokBotMarketplaceListingsInternal · 93† GetGrokBotMarketplaceListingInternal *(→ **GrokBotMarketplaceListing**)* · 94† UpdateGrokBotMarketplaceListingInternal *(→ **GrokBotMarketplaceListing**)* · 95† SetGrokBotMarketplaceListingStatusInternal *(→ **GrokBotMarketplaceListing**)* · 96† PresignGrokBotMarketplaceImageUploadInternal · **97\* CreateGrokBotMarketplaceCreatorInternal** *(→ **GrokBotMarketplaceCreator**)* · **98\* ListGrokBotMarketplaceCreatorsInternal** · **99\* GetGrokBotMarketplaceCreatorInternal** *(→ **GrokBotMarketplaceCreator**)* · **100\* UpdateGrokBotMarketplaceCreatorInternal** *(→ **GrokBotMarketplaceCreator**)* · **101\* PresignGrokBotMarketplaceCreatorProfileUploadInternal** *(→ PresignGrokBotMarketplaceImageUploadInternalResponse — cross-family reuse)* · **102\* PreviewGrokBotMarketplaceSourceInternal** · **103\* CreateGrokBotMarketplaceCategoryInternal** *(→ **GrokBotMarketplaceCategory**)* · **104\* ListGrokBotMarketplaceCategoriesInternal** · **105\* GetGrokBotMarketplaceCategoryInternal** *(→ **GrokBotMarketplaceCategory**)* · **106\* UpdateGrokBotMarketplaceCategoryInternal** *(→ **GrokBotMarketplaceCategory**)*

The 0.29 list (89) is this table minus the 17 starred rows, same relative order. The 0.27 list (76) is rows #1–42, #44, #46, #49–57, #59, #62–73, #76–85, same order.

### 6.4 New 0.30 message families — names and field names

*Provenance for all of §6.4 and §6.5: `E30:dist/node-agent-coordinator/main.cjs`.*

**Runtime capabilities**

```
GrokBotRuntimeCapabilities            durable_identity_enabled, durable_identity_writes_enabled,
                                      temporal_creation_enabled, agent_messaging_enabled   (all bool)
GetGrokBotRuntimeCapabilitiesRequest  (no fields)
GetGrokBotRuntimeCapabilitiesResponse capabilities
GrokBotUserComputerCapabilities       messages_op (opt), messages_op_generation (opt)
```
```js
// verbatim, E30:dist/node-agent-coordinator/main.cjs (single occurrence, including the `Ss` binding)
Ss.typeName="aiserver.v1.GrokBotRuntimeCapabilities";Ss.fields=i.util.newFieldList(()=>[
 {no:1,name:"durable_identity_enabled",kind:"scalar",T:8},
 {no:2,name:"durable_identity_writes_enabled",kind:"scalar",T:8},
 {no:3,name:"temporal_creation_enabled",kind:"scalar",T:8},
 {no:4,name:"agent_messaging_enabled",kind:"scalar",T:8}])
```

**Messages ops** — the op and result cross the protobuf boundary as **opaque JSON strings**, not structured protobuf.

```
GrokBotUserComputerMessagesOp      op_json, approval_id (opt)
GrokBotUserComputerMessagesResult  result_json
GrokBotUserComputerMessagesError   error
```

Frame wiring (new fields on pre-existing 0.29 types):

```
GrokBotUserComputerRequestFrame    request_id | exec | upload | download | retire_approval | cancel
                                 + messages_op                                 (field 7, oneof=frame)
GrokBotUserComputerResponseFrame   request_id | client | control | file | file_error
                                 + messages_result (field 6) + messages_error (field 7, both oneof=frame)
GrokBotUserComputerHello           label, local_root, terminals_folder, standing, supervised, variant,
                                   server_authoritative + capabilities (field 8)
GrokBotUserComputerExec          + authorized_by_approval (field 4, bool)
GrokBotUserComputerUpload        + authorized_by_approval (field 5, bool)
GrokBotUserComputerDownload      + authorized_by_approval (field 4, bool)
GrokBotTranscriptWatchFrame        connected | rows | cleared | cursor_too_old | heartbeat | agent_state
                                   | computer_actions + agent_state_changed (field 8)
```

**Virtual card**

```
RaiseGrokBotVirtualCardRequest            agent_id, amount_cents, currency, merchant_name, merchant_url, context
RaiseGrokBotVirtualCardResponse           outcome (GrokBotVirtualCardRaiseOutcome), request_id, merchant_name (opt)
ResolveGrokBotVirtualCardApprovalRequest  agent_id, entry_id, request_id, resolution (GrokBotVirtualCardResolution)
ResolveGrokBotVirtualCardApprovalResponse outcome (GrokBotVirtualCardOutcome), spend_request_id (opt),
                                          approval_url (opt), message (opt), refusal (opt, GrokBotHarnessRefusal)
```

**Marketplace — new entities and RPC messages**

```
GrokBotMarketplaceCreator          id, name, profile_photo_url, handles(map<string,string>), created_at_ms, updated_at_ms
GrokBotMarketplaceCategory         id, name
PublicGrokBotMarketplaceCreator    name, profile_photo_url, handles(map<string,string>)
PublicGrokBotMarketplaceListing    slug, name, description, image_url | default_avatar (oneof=avatar),
                                   category, created_at_ms, updated_at_ms, share_id, creator

CreateGrokBotMarketplaceCreatorInternalRequest   name, profile_photo_url, handles
GetGrokBotMarketplaceCreatorInternalRequest      creator_id
UpdateGrokBotMarketplaceCreatorInternalRequest   creator_id, name(opt), profile_photo_url(opt), handles,
                                                 replace_handles(opt)
ListGrokBotMarketplaceCreatorsInternalRequest    page_size, page_token
ListGrokBotMarketplaceCreatorsInternalResponse   creators[], next_page_token
PresignGrokBotMarketplaceCreatorProfileUploadInternalRequest  content_type, byte_size

CreateGrokBotMarketplaceCategoryInternalRequest  name
GetGrokBotMarketplaceCategoryInternalRequest     category_id
UpdateGrokBotMarketplaceCategoryInternalRequest  category_id, name(opt)
ListGrokBotMarketplaceCategoriesInternalRequest  page_size, page_token
ListGrokBotMarketplaceCategoriesInternalResponse categories[], next_page_token

ListPublicGrokBotMarketplaceListingsRequest      category(opt), page_size, page_token
ListPublicGrokBotMarketplaceListingsResponse     featured_listings[], listings[], next_page_token
GetPublicGrokBotMarketplaceListingRequest        slug
GetPublicGrokBotMarketplaceListingResponse       listing, template_get_url

PreviewGrokBotMarketplaceSourceInternalRequest   source_ref
PreviewGrokBotMarketplaceSourceInternalResponse  template_id, share_id, name, description, avatar_shape,
                                                 avatar_color, published, active_version, eligible,
                                                 ineligibility_reason
```

**Other new 0.30 types**

```
GrokBotTranscriptWatchAgentStateChanged  agent_id, families[], changed_at_ms
GetGrokBotTemplateExportPolicyRequest    (no fields)
GetGrokBotTemplateExportPolicyResponse   export_policy(opt), has_team(opt)
```

**Field additions on pre-existing 0.29 types — the complete list**

```
CreateGrokBotAgentRequest                + origin (field 13, opt)
CreateGrokBotTemplateRequest             + requested_visibility (field 10, opt, GrokBotTemplateVisibility)
ListGrokBotTemplatesResponse             + export_policy (field 2, opt)
GetGrokBotTemplateForSourceAgentResponse + export_policy (field 2, opt), has_team (field 3, opt)
GrokBotUserComputerHello                 + capabilities (field 8)
```

**Enum value additions on pre-existing enums — exactly one:** `SET_GROK_BOT_AGENT_VISIBILITY_OUTCOME_NO_TEAM`.

An exhaustive per-message field-signature diff over all 235 shared coordinator messages and all 29 shared enums produced exactly this set and nothing more — 15 changed messages, 1 changed enum. (`UpdateGrokBotAgentRequest.clear_avatar` shows a differing minified var between builds; both resolve to `google.protobuf.Empty` with `oneof=avatar_change`. Not a schema change.)

### 6.5 The one breaking reshape: marketplace listings, 0.29 → 0.30

Creator identity was normalized out of the listing into a `creator_id` FK, and a many-to-many `categories` was added alongside the legacy scalar `category`. **Retired field numbers are not reused.**

```
GrokBotMarketplaceListing
  − 12 creator_display_name        + 16 creator_id
  − 13 creator_image_url (opt)     + 17 share_id
                                   + 18 categories[]
  kept: 1 id, 2 template_id, 3 pinned_template_version_id, 4 blob_object_key, 5 name, 6 description,
        7 image_url | 8 default_avatar (oneof=avatar), 9 status, 10 slug, 11 category,
        14 created_at_ms, 15 updated_at_ms

CreateGrokBotMarketplaceListingInternalRequest
  − 4 creator_display_name, − 5 creator_image_url      + 6 creator_id, + 7 category_ids[]

UpdateGrokBotMarketplaceListingInternalRequest
  − 8 creator_display_name (opt)
  − 9 creator_image_url | − 10 clear_creator_image (the whole creator_image oneof)
  + 11 creator_id (opt), + 12 category_ids[], + 13 replace_categories (opt)
```

`GrokBotMarketplaceImageKind` already carried `_BOT` and `_CREATOR` at 0.29 (unchanged), and `PresignGrokBotMarketplaceCreatorProfileUploadInternal` reuses `PresignGrokBotMarketplaceImageUploadInternalResponse` `{put_url, public_url}`.

### 6.6 Coordinator enums — 32 in 0.30, 29 in 0.29

`*` = new in 0.30. Value counts in parentheses; each value carries the full `GROK_BOT_…` / `SAND_BOX_…` prefix on the wire and is abbreviated to `_SUFFIX` below.

`SandSetupManifestScopeKind` (4), `SandBoxMigrationPhase` (8), `SandBoxRunState` (4), `SandBoxUpgradeScheduleState` (9), `SandBoxStoreMultipartOperationFailureCode` (8), `GrokBotMarketplaceListingStatus` (4), `GrokBotMarketplaceImageKind` (3), `GrokBotAgentHarnessKind` (3), `GrokBotAgentVisibility` (3), `SetGrokBotAgentVisibilityOutcome` (4), `GrokBotTemplateVisibility` (3), `GrokBotTemplateOwnerType` (3), `GrokBotFirstPartyTemplate` (2), `GrokBotClientSurface` (3), `GrokBotUserMessageDelivery` (5), `GrokBotTemporalHarnessMode` (5), `GrokBotSendStatus` (6), `GrokBotFeedbackAction` (5), `GrokBotAgentMessageDelivery` (9), `GrokBotAutoReviewApprovalResolution` (4), `GrokBotLocalToolPermissionCardResolution` (5), **\*`GrokBotVirtualCardRaiseOutcome`** (3), **\*`GrokBotVirtualCardResolution`** (3), **\*`GrokBotVirtualCardOutcome`** (5), `GrokBotBoxHandBackTrigger` (4), `GrokBotRoomMemberTurnDispatch` (6), `GrokBotSlackInstallStatus` (5), `GrokBotSlackConnectOutcome` (3), `GrokBotSlackInstallOutcome` (11), `GrokBotSlackReinstallOutcome` (11), `GrokBotSlackUninstallOutcome` (7), `GrokBotRoomMemberTurnMessage.SpeakerKind` (3).

```
GrokBotAgentHarnessKind:      _UNSPECIFIED, _BOX, _TEMPORAL
GrokBotTemporalHarnessMode:   _UNSPECIFIED, _OFF, _SHADOW, _LIVE, _BOX
GrokBotAgentVisibility:       _UNSPECIFIED, _OWNER, _TEAM
SetGrokBotAgentVisibilityOutcome: _UNSPECIFIED, _UPDATED, _UNSUPPORTED_HARNESS, _NO_TEAM*
GrokBotMarketplaceListingStatus:  _UNSPECIFIED, _PENDING_REVIEW, _LISTED, _DELISTED
GrokBotMarketplaceImageKind:      _UNSPECIFIED, _BOT, _CREATOR
GrokBotTemplateVisibility:    _UNSPECIFIED, _PUBLIC, _TEAM
GrokBotTemplateOwnerType:     _UNSPECIFIED, _USER, _TEAM
GrokBotFirstPartyTemplate:    _UNSPECIFIED, _SWE
GrokBotClientSurface:         _UNSPECIFIED, _DESKTOP, _MOBILE
GrokBotSendStatus:            _UNSPECIFIED, _NOT_FOUND, _ACCEPTED, _REJECTED, _PENDING, _UNKNOWN_DURABILITY
GrokBotUserMessageDelivery:   _UNSPECIFIED, _ACCEPTED_BOX, _ACCEPTED_TEMPORAL, _DUPLICATE, _REFUSED
GrokBotAgentMessageDelivery:  _UNSPECIFIED, _DELIVERED_TEMPORAL, _DELIVERED_BOX, _DUPLICATE, _TARGET_NOT_FOUND,
                              _FORBIDDEN, _BOX_UNREACHABLE, _TEMPORAL_UNAVAILABLE, _INVALID_TARGET
GrokBotFeedbackAction:        _UNSPECIFIED, _UP, _DOWN, _SUBMIT, _REVERT
GrokBotAutoReviewApprovalResolution:      _UNSPECIFIED, _APPROVED, _DENIED, _ALWAYS
GrokBotLocalToolPermissionCardResolution: _UNSPECIFIED, _ALLOW_ONCE, _DENY, _ALWAYS, _NEVER
GrokBotVirtualCardRaiseOutcome*: _UNSPECIFIED, _RAISED, _ALREADY_PENDING
GrokBotVirtualCardResolution*:   _UNSPECIFIED, _APPROVED, _DENIED
GrokBotVirtualCardOutcome*:      _UNSPECIFIED, _APPROVED, _DENIED, _NEEDS_AUTH, _FAILED
GrokBotBoxHandBackTrigger:    _UNSPECIFIED, _BUTTON, _VIEWER_CLOSED, _DISMISSED
GrokBotRoomMemberTurnDispatch: _UNSPECIFIED, _ACCEPTED, _DUPLICATE, _NOT_TEMPORAL, _TARGET_NOT_FOUND,
                              _TEMPORAL_UNAVAILABLE
GrokBotRoomMemberTurnMessage.SpeakerKind: SPEAKER_KIND_UNSPECIFIED, _HUMAN, _AGENT
SandBoxRunState:              _UNSPECIFIED, _ABSENT, _HIBERNATED, _RUNNING
SandBoxMigrationPhase:        _UNSPECIFIED, _BACKING_UP, _CREATING, _MOVING, _CLEANING_UP, _WIPING, _DONE, _FAILED
SandBoxUpgradeScheduleState:  _UNSPECIFIED, _SCHEDULED, _CLAIMED, _RUNNING, _WAITING_FOR_IMAGE, _COMPLETED,
                              _MISSED, _FAILED, _CANCELLED
SandSetupManifestScopeKind:   _UNSPECIFIED, _USER, _TEAM, _ORGANIZATION
SandBoxStoreMultipartOperationFailureCode: _UNSPECIFIED, _PRECONDITION_FAILED, _UPLOAD_NOT_FOUND, _INVALID_PARTS,
                              _CHECKSUM_MISMATCH, _TRANSIENT, _INTERNAL, _RESTART_REQUIRED
GrokBotSlackInstallStatus:    _UNSPECIFIED, _NOT_CONNECTED, _APP_CREATED, _CONNECTED, _UNSUPPORTED_HARNESS
GrokBotSlackConnectOutcome:   _UNSPECIFIED, _STARTED, _UNSUPPORTED_HARNESS
GrokBotSlackInstallOutcome:   _UNSPECIFIED, _INSTALLED, _PENDING_ADMIN_APPROVAL, _NOT_CONNECTED,
                              _WORKSPACE_REQUIRED, _MANAGER_REAUTH_REQUIRED, _INSUFFICIENT_SCOPES,
                              _INSTALL_CONFLICT, _RATELIMITED, _SLACK_REJECTED, _UNSUPPORTED_HARNESS
GrokBotSlackReinstallOutcome: _UNSPECIFIED, _UPDATED, _UP_TO_DATE, _RECREATE_REQUIRED, _PENDING_ADMIN_APPROVAL,
                              _NOT_CONNECTED, _MANAGER_REAUTH_REQUIRED, _INSUFFICIENT_SCOPES, _RATELIMITED,
                              _SLACK_REJECTED, _UNSUPPORTED_HARNESS
GrokBotSlackUninstallOutcome: _UNSPECIFIED, _REMOVED, _NOT_CONNECTED, _MANAGER_REAUTH_REQUIRED, _RATELIMITED,
                              _SLACK_REJECTED, _UNSUPPORTED_HARNESS
```

### 6.7 Coordinator message names — 270 in 0.30 (`*` = new in 0.30; delete the 35 starred to get 0.29's 235)

AbortSandBoxStoreMultipartWritesRequest, AbortSandBoxStoreMultipartWritesResponse, ActivateGrokBotTemplateVersionRequest, ActivateGrokBotTemplateVersionResponse, AdminForceRecreateSandBoxRequest, AdminGetSandAgentTranscriptPageRequest, AdminGetSandAgentTranscriptPageResponse, AdminHibernateSandBoxRequest, AdminHibernateSandBoxResponse, AdminListSandAgentsRequest, AdminListSandAgentsResponse, AdminListSandBoxStoreManifestVersionsRequest, AdminListSandBoxStoreManifestVersionsResponse, AdminRecreateSandBoxRequest, AdminRestoreSandBoxStoreSnapshotRequest, AdminRestoreSandBoxStoreSnapshotResponse, AdminSandBoxHostStatusRequest, AdminSandBoxHostStatusResponse, AdminSandBoxStoreStatusRequest, AdminSandBoxStoreStatusResponse, AdminSnapshotSandBoxStoreRequest, AdminSnapshotSandBoxStoreResponse, AdminUpdateSandBoxHostRequest, AdminUpdateSandBoxHostResponse, AdminWatchSandBoxMigrationRequest, CancelGrokBotRoomMemberTurnRequest, CancelGrokBotRoomMemberTurnResponse, CancelGrokBotUserComputerRequestRequest, CancelGrokBotUserComputerRequestResponse, CancelSandBoxUpgradeRequest, CancelSandBoxUpgradeResponse, CommitGrokBotTranscriptEntriesRequest, CommitGrokBotTranscriptEntriesResponse, CompleteSandBoxStoreMultipartWritesRequest, CompleteSandBoxStoreMultipartWritesResponse, CreateGrokBotAgentFromTemplateRequest, CreateGrokBotAgentFromTemplateResponse, CreateGrokBotAgentRequest, CreateGrokBotAgentResponse, **\*CreateGrokBotMarketplaceCategoryInternalRequest**, **\*CreateGrokBotMarketplaceCreatorInternalRequest**, CreateGrokBotMarketplaceListingInternalRequest, CreateGrokBotTemplateRequest, CreateGrokBotTemplateResponse, DeleteGrokBotAgentRequest, DeleteGrokBotAgentResponse, DeleteGrokBotTemplateRequest, DeleteGrokBotTemplateResponse, DeleteTeamSandSetupManifestRequest, DeleteTeamSandSetupManifestResponse, DismissGrokBotWidgetRequest, DismissGrokBotWidgetResponse, EndGrokBotBoxHandoffRequest, EndGrokBotBoxHandoffResponse, EnsureSandBoxRequest, EnsureSandBoxResponse, EnsureSandBoxWindowRequest, ForceRecreateSandBoxRequest, **\*GetGrokBotMarketplaceCategoryInternalRequest**, **\*GetGrokBotMarketplaceCreatorInternalRequest**, GetGrokBotMarketplaceListingInternalRequest, **\*GetGrokBotRuntimeCapabilitiesRequest**, **\*GetGrokBotRuntimeCapabilitiesResponse**, GetGrokBotSendStatusRequest, GetGrokBotSendStatusResponse, GetGrokBotSlackInstallStateRequest, GetGrokBotSlackInstallStateResponse, **\*GetGrokBotTemplateExportPolicyRequest**, **\*GetGrokBotTemplateExportPolicyResponse**, GetGrokBotTemplateForSourceAgentRequest, GetGrokBotTemplateForSourceAgentResponse, GetGrokBotTemplateImportDetailsRequest, GetGrokBotTemplateImportDetailsResponse, GetGrokBotTemplateVersionRequest, GetGrokBotTemplateVersionResponse, **\*GetPublicGrokBotMarketplaceListingRequest**, **\*GetPublicGrokBotMarketplaceListingResponse**, GetPublicGrokBotTemplateRequest, GetPublicGrokBotTemplateResponse, GetSandBoxRunStateRequest, GetSandBoxRunStateResponse, GetSandBoxUpgradeScheduleRequest, GetSandBoxUpgradeScheduleResponse, GrokBotAgent, GrokBotAgentAwaitingState, GrokBotAgentClientState, GrokBotAgentLiveActivity, GrokBotAgentLiveState, GrokBotComputerAction, GrokBotHarnessRefusal, **\*GrokBotMarketplaceCategory**, **\*GrokBotMarketplaceCreator**, GrokBotMarketplaceDefaultAvatar, GrokBotMarketplaceListing, GrokBotRoomMemberTurnMessage, GrokBotRoomMemberTurnMessage.ReplyTarget, GrokBotRoomMemberTurnPeer, GrokBotRoomMemberTurnRoom, **\*GrokBotRuntimeCapabilities**, GrokBotSlackConnection, GrokBotSlackWorkspace, GrokBotTemplate, GrokBotTranscriptCursor, GrokBotTranscriptEntry, GrokBotTranscriptEntryDelete, GrokBotTranscriptEntryRejection, GrokBotTranscriptWatchAgentState, **\*GrokBotTranscriptWatchAgentStateChanged**, GrokBotTranscriptWatchCleared, GrokBotTranscriptWatchComputerActions, GrokBotTranscriptWatchConnected, GrokBotTranscriptWatchCursorTooOld, GrokBotTranscriptWatchFrame, GrokBotTranscriptWatchHeartbeat, GrokBotTranscriptWatchRows, GrokBotUserComputerCancel, **\*GrokBotUserComputerCapabilities**, GrokBotUserComputerClientMessage, GrokBotUserComputerControlMessage, GrokBotUserComputerDownload, GrokBotUserComputerExec, GrokBotUserComputerFile, GrokBotUserComputerFileError, GrokBotUserComputerHello, **\*GrokBotUserComputerMessagesError**, **\*GrokBotUserComputerMessagesOp**, **\*GrokBotUserComputerMessagesResult**, GrokBotUserComputerPresence, GrokBotUserComputerQueuedRequest, GrokBotUserComputerRequestFrame, GrokBotUserComputerResponseFrame, GrokBotUserComputerRetireApproval, GrokBotUserComputerUpload, InstallGrokBotSlackAppRequest, InstallGrokBotSlackAppResponse, InterruptGrokBotAgentRunRequest, InterruptGrokBotAgentRunResponse, IssueGrokBotUserComputerCredentialRequest, IssueGrokBotUserComputerCredentialResponse, KillTeamMemberSandBoxRequest, KillTeamMemberSandBoxResponse, ListGrokBotAgentsRequest, ListGrokBotAgentsResponse, **\*ListGrokBotMarketplaceCategoriesInternalRequest**, **\*ListGrokBotMarketplaceCategoriesInternalResponse**, **\*ListGrokBotMarketplaceCreatorsInternalRequest**, **\*ListGrokBotMarketplaceCreatorsInternalResponse**, ListGrokBotMarketplaceListingsInternalRequest, ListGrokBotMarketplaceListingsInternalResponse, ListGrokBotTemplatesRequest, ListGrokBotTemplatesResponse, ListGrokBotTranscriptEntriesRequest, ListGrokBotTranscriptEntriesResponse, ListGrokBotUserComputersRequest, ListGrokBotUserComputersResponse, **\*ListPublicGrokBotMarketplaceListingsRequest**, **\*ListPublicGrokBotMarketplaceListingsResponse**, ListSandBoxStoreObjectsRequest, ListSandBoxStoreObjectsResponse, ListSandBoxesRequest, ListSandBoxesResponse, ListSandSetupManifestsRequest, ListSandSetupManifestsResponse, ListTeamMemberSandBoxesRequest, ListTeamMemberSandBoxesResponse, ListTeamSandSetupManifestsRequest, ListTeamSandSetupManifestsResponse, MintSandVoiceCallSecretRequest, MintSandVoiceCallSecretResponse, NotifySandAgentTurnFinishedRequest, NotifySandAgentTurnFinishedResponse, OpenGrokBotUserComputerRequestRequest, PollGrokBotUserComputerRequestsRequest, PollGrokBotUserComputerRequestsResponse, **\*PresignGrokBotMarketplaceCreatorProfileUploadInternalRequest**, PresignGrokBotMarketplaceImageUploadInternalRequest, PresignGrokBotMarketplaceImageUploadInternalResponse, PresignSandBoxStoreReadsRequest, PresignSandBoxStoreReadsResponse, PresignSandBoxStoreWritesRequest, PresignSandBoxStoreWritesResponse, **\*PreviewGrokBotMarketplaceSourceInternalRequest**, **\*PreviewGrokBotMarketplaceSourceInternalResponse**, **\*PublicGrokBotMarketplaceCreator**, **\*PublicGrokBotMarketplaceListing**, **\*RaiseGrokBotVirtualCardRequest**, **\*RaiseGrokBotVirtualCardResponse**, ReactToGrokBotMessageRequest, ReactToGrokBotMessageResponse, ReadGrokBotAgentAttachmentChunkRequest, ReadGrokBotAgentAttachmentChunkResponse, RecreateSandBoxRequest, RecreateSandBoxResponse, ReinstallGrokBotSlackAppRequest, ReinstallGrokBotSlackAppResponse, ReportGrokBotClientPresenceRequest, ReportGrokBotClientPresenceResponse, RequestGrokBotRoomMemberTurnRequest, RequestGrokBotRoomMemberTurnResponse, RescheduleSandBoxUpgradeRequest, RescheduleSandBoxUpgradeResponse, ResolveGrokBotAutoReviewApprovalRequest, ResolveGrokBotAutoReviewApprovalResponse, ResolveGrokBotLocalToolPermissionRequest, ResolveGrokBotLocalToolPermissionResponse, **\*ResolveGrokBotVirtualCardApprovalRequest**, **\*ResolveGrokBotVirtualCardApprovalResponse**, RespondGrokBotWidgetRequest, RespondGrokBotWidgetResponse, SandAssignedSetupManifest, SandBoxDescriptor, SandBoxMigrationEvent, SandBoxStoreManifestVersion, SandBoxStoreMultipartAbortResult, SandBoxStoreMultipartAbortSuccess, SandBoxStoreMultipartOperationFailure, SandBoxStoreMultipartPart, SandBoxStoreMultipartUploadContext, SandBoxStoreMultipartUploadPartInstruction, SandBoxStoreMultipartUploadedPart, SandBoxStoreMultipartWriteAbort, SandBoxStoreMultipartWriteCompletion, SandBoxStoreMultipartWriteInstruction, SandBoxStoreMultipartWriteResult, SandBoxStoreMultipartWriteSuccess, SandBoxStoreObjectEntry, SandBoxStoreReadInstruction, SandBoxStoreWriteFile, SandBoxStoreWriteInstruction, SandBoxUpgradeSchedule, SandSetupManifestEntry, SandTeamSetupManifest, SaveTeamSandSetupManifestRequest, SaveTeamSandSetupManifestResponse, ScheduleSandBoxUpgradeRequest, ScheduleSandBoxUpgradeResponse, SendGrokBotAgentMessageRequest, SendGrokBotAgentMessageResponse, SendGrokBotUserMessageRequest, SendGrokBotUserMessageResponse, SetGrokBotAgentClientStateRequest, SetGrokBotAgentClientStateResponse, SetGrokBotAgentVisibilityRequest, SetGrokBotAgentVisibilityResponse, SetGrokBotMarketplaceListingStatusInternalRequest, SetGrokBotTemplateVisibilityRequest, SetGrokBotTemplateVisibilityResponse, StartGrokBotSlackConnectRequest, StartGrokBotSlackConnectResponse, StatSandBoxStoreObjectRequest, StatSandBoxStoreObjectResponse, SubmitGrokBotUserComputerResponsesRequest, SubmitGrokBotUserComputerResponsesResponse, TeamMemberSandBoxPod, UninstallGrokBotSlackAppRequest, UninstallGrokBotSlackAppResponse, UpdateGrokBotAgentRequest, UpdateGrokBotAgentResponse, **\*UpdateGrokBotMarketplaceCategoryInternalRequest**, **\*UpdateGrokBotMarketplaceCreatorInternalRequest**, UpdateGrokBotMarketplaceListingInternalRequest, VoteGrokBotFeedbackRequest, VoteGrokBotFeedbackResponse, WatchGrokBotTranscriptsRequest, WatchGrokBotUserComputerConnected, WatchGrokBotUserComputerHeartbeat, WatchGrokBotUserComputerNotify, WatchGrokBotUserComputerRequestsEvent, WatchGrokBotUserComputerRequestsRequest, WatchSandBoxMigrationRequest.

Only one nested family appears in the coordinator set: `GrokBotRoomMemberTurnMessage.ReplyTarget` (message) and `.SpeakerKind` (enum). Bundled well-known types in the 0.30 coordinator: **12** — `google.protobuf.{Empty, Value, NullValue, BoolValue, BytesValue, DoubleValue, FloatValue, StringValue, Int32Value, Int64Value, UInt32Value, UInt64Value}`. (`Struct`, `ListValue`, `Timestamp`, `Duration`, `FieldMask` are absent.)

---

## 7. Traps

### 7.1 No version has per-message delete or edit — and our tree invented one

Enumerated across all official surfaces: 0.18's 120 gateway commands contain no `delete*Entry`, `edit*`, `update*Message`, or `retract*`. 0.29/0.30's 332/345 internal methods yield only agent-, memory-, automation-, workflow-, template-, account-, secret-, cookie-, MCP- and persistence-scoped deletions.

The only entry-level deletion primitives in 0.18 are **private prepared statements**, unreachable from the gateway:

- `A18:dist/host/host-main.cjs:628389` — `deleteTranscriptEntry: db.prepare("DELETE FROM transcript_entries WHERE id = ?")`
- `:630597` — inside `repairHiddenTranscriptEntriesOnce(...)`, a one-shot versioned migration gated on `db.getHiddenEntryRepairVersion() >= HIDDEN_ENTRY_REPAIR_VERSION`
- `:648421` — inside `rollbackAppendedEntries()`, the send-failure undo for echo entries the host appended microseconds earlier

`updateTranscriptEntry` is the store-level upsert for streaming coalescence and status settling (approval `pending → approved`), not an edit.

**Correcting the tempting shorthand:** it is *not* true that `reactToMessage` is the only entry-scoped mutation. 0.29/0.30 also expose five entry-keyed write RPCs that 0.18 does not have:

```js
// E29:dist/electron-main/main.cjs @~5529100 (same block in E30 @~5931033)
submitUserForm:v().args({entryId:C(),values:f2e,agentId:C(),platform:z(wpn)}),
dismissUserForm:v().args({entryId:C(),mode:["dismissed","escalated"],agentId:C(),platform:z(wpn)}),
sendDraft:v().args({entryId:C(),draft:LEr,agentId:C()}),
discardDraft:v().args({entryId:C(),agentId:C()}),
reactToMessage:…,
voteFeedback:v().args({agentId:C(),entryId:C(),action:mn("feedback action",["up","down","submit","revert"]),…})
```

`voteFeedback`'s `"revert"` is the closest thing in any version to undoing a message-scoped write. The headline still holds: **no per-message DELETE, no per-message EDIT, in any version.**

**Our tree diverges.** `OURS/host/gateway-protocol.ts` declares 123 entries (lines 5–127) and its header comment reads:

```ts
/** Mechanically recovered from the immutable 0.18 host bundle. */
```

Three of those 123 are not recovered from anything — they occur in **no** official binary (`grep -rlF` over `A18/dist`, `E29/dist`, `E30/dist` returns zero files for each):

| our line | command |
|---|---|
| 21 | `deleteTranscriptEntries` (inserted between `reactToMessage` and `appendConnectorCard`) |
| 125 | `listRoutedMcpTools` |
| 126 | `executeRoutedMcpTool` |

Nothing from 0.18 is missing from our file, and the remaining 120 are in exact order. Likewise `client-side-tool-v2` in `OURS/node-agent-coordinator/gateway/gateway-event-families.ts` is a 17th SSE family present in no official version. **Serving these is a local decision, not a client contract** — the docstring is inaccurate for those four names and should be amended. (Cosmetic-only divergence: our SLIM `listAgents` is `(api, _body) => stripSummaryRows(await SAND_GATEWAY_COMMANDS.listAgents(api))` where 0.18 passes `body` through to a 1-arity handler. Behaviourally identical. Our arity split is therefore 24 no-arg / 99 arg vs 0.18's 23 / 97.)

### 7.2 Name tables are not contracts

Every method list in this document is a **name** table. Argument schemas drift invisibly:

```js
// E29:dist/electron-main/main.cjs @5446262
openExternal:v().args({url:C(),mcpServerName:z(C())})
// E30:dist/electron-main/main.cjs @5836401
openExternal:C().args({url:x()})              // mcpServerName gone
```
`mcpServerName` still exists elsewhere in 0.30 (3 occurrences in `electron-main/main.cjs` vs 8 in 0.29). A team implementing to a name diff ships the wrong signature. Diff schemas separately when a method matters.

### 7.3 Re-derivation traps (things that look like the document is wrong but are not)

- The `sort -u` grep in the reproduction recipes returns exactly 332 / 345. Without `-u` it returns 333 / 358 — the difference is `main ∩ gateway` mirroring, not an error.
- A raw `sand:` literal sweep over 0.29/0.30 `dist/` returns **17**, not 12. Five are not channels: `sand:` from `sand://`, `sand:M.string` from a schema, `sand:mcp` / `sand:privacy` from logger tags (`[sand:mcp] loopback OAuth`), and `sand:thread` from the cache key `` `sand:thread\0${t}\0${e}` ``.
- 0.18's "73" is the same kind of literal count. At least two entries are middleware identifiers, not channels — `sand:send-message-reminder-middleware` and `sand:start-of-turn-ack-reminder-middleware`, both only in `A18:dist/host/host-main.cjs:657663` and `:657849` as `createLogger(...)` names. **True 0.18 channel count ≈ 71.** Per-channel handler attribution for 0.18 is **unverified**: only 23 direct `.handle(`/`.on(` registrations with a `sand:` literal are visible in `A18:dist/electron-main/main.cjs`; the rest go through helper loops that were not traced.
- Two of the 12 surviving 0.29/0.30 channels are **new**, not survivors: `sand:boot-snapshot-sync` and `sand:render-trace-port` appear nowhere in 0.18's set. Ten of the twelve carry over.
- The same edge table appears in more than one bundle with identical contents and different minified identifiers. `main` also lives in `dist/electron-preload/preload.cjs` (152 / 173) and in a `main:methods` copy in the renderer; `gateway` + `coordinator-control` + `coordinator-main` also live in `dist/node-agent-coordinator/main.cjs` (146/22 and 147/25). Grepping a different bundle with this document's identifiers finds nothing.
- Minified identifiers differ between 0.29 and 0.30 for essentially everything. Gateway edge var `sy` → `fg`; edge factory `$t` → `on`; rpcMethod helper `E()` → `_()`; api prefix const `ol` → `su`; proxy table `mR`(base `pR`) → `Iw`(base `xw`); local-forward `wA`/`BR`; dev-only `RA`/`UR`; validator `bdn`/`Fgi`; `activityOf` `NR`/`Kw`. Where this document says "identical", it means identical modulo minifier renaming — **never** byte-identical.
- 0.18's renderer contains an `unknown-kind` literal at `index-UbX-y3il.js@5066245` (`` return `${e}:unknown-kind` ``). That is an unrelated memo-key fallback, not the telemetry bucket; the telemetry bucket exists only from 0.29.

### 7.4 Silent-failure traps in the local-exec daemon wire

- **Unknown frame kinds are dropped before schema validation, silently, in both versions:**
  ```js
  // E29 (dW) / E30 (XK) — character-identical bodies, both tagged "hasUnknownKind"
  function _(t,e){return typeof t=="object"&&t!==null&&"kind"in t&&typeof t.kind=="string"&&!e.has(t.kind)}
  ```
- **0.30 made one path quieter.** 0.29 pushed a zod issue for an unparseable response frame and dropped it (`e.addIssue({code:jM.custom,path:[n],message:uW(s.error)})`). 0.30 replaces that with `let i=Y1t(r);i!==void 0&&e.push(i)` — an unparseable **non-Messages** response frame is now dropped with no issue recorded at all. Messages frames get a typed degradation instead:
  ```js
  // E30:dist/local-exec-daemon/main.cjs
  var fx="This computer's local-exec daemon does not understand that request frame; update the desktop app.";
  var J1t="This computer answered with a Messages result this host could not read; the desktop app and the agent are on incompatible versions.";
  o(kNt,"refuseUnparseableMessagesOp");   // → {kind:"messages-error",requestId,error:fx}
  o(Y1t,"degradeInvalidResponseFrame");   // → {kind:"messages-error",requestId,error:J1t}
  ```
- **Oversized SSE blocks are rejected without parsing.** `rejectOversizedFrame` regex-scrapes `"requestId"\s*:\s*"([^"\\]+)"` out of the first 4096 characters and answers `file-error`. If your `requestId` is not in the first 4 KB of a large frame, the rejection is unattributable.
- **The two encodings are not the same shape.** The protobuf `GrokBotUserComputerFile` is **chunked** — `{1 data, 2 seq, 3 last}`, identical in 0.29 and 0.30 — while the JSON `file` frame carries a whole `bytesBase64?` in one shot. Similarly the JSON `hello` has `computerId?` and no `standing`/`serverAuthoritative`, while protobuf `Hello` has `standing` (4) and `server_authoritative` (7) and no `computerId`. Do not implement "both encodings of the same vocabulary" from a single table.
- **Messages ops can never ride a standing or prior approval.** The Messages gate call hard-codes both booleans false, so every Messages op must be approved individually:
  ```js
  // E30:dist/local-exec-daemon/main.cjs — handleMessagesOp
  let s=await this.options.isLocalUseBlocked?.({approvalId:e.approvalId,
    authorizedByStanding:!1, authorizedByApproval:!1,
    describes:L5e(e.op), terminalsFolder:this.terminalsFolder});
  if(s!==void 0){r({kind:"messages-error",requestId:n,error:s});return}
  ```
  Even `check-permissions` goes through the gate, described as `{action:"read-messages", target:"your Messages history", description:"Check Messages permissions"}`.
- **The send idempotency cache also memoizes failures.** `finishedSends` stores whatever `finish` emits for a `send` op — including a `messages-error` from a thrown `run()` — FIFO-capped at 64. So a *failed* send is replayed forever on the same `requestId` and never retried. An approval **denial**, by contrast, is emitted through the raw callback, bypasses the cache, and re-prompts on retry.
- **Two different file caps.** `fetch-attachment` is bounded at runtime by the executor's `maxFileBytes` (`this.messages.run(e.op,this.maxFileBytes)`, `maxFileBytes=e.maxFileBytes??104857600`), but the result *schema* refine hard-codes the number: `function q1t(t){return Math.floor(t.length*3/4)<=104857600}`. A host configured with a larger `maxFileBytes` would still have its own result parse rejected.
- **Helper verb allowlist is all-or-nothing.** The daemon parses the helper's self-reported verb list and fails unless **all four** are advertised: `["snapshot-messages-db","copy-attachment","send-message","check-messages-permissions"]` → else `helper_missing_messages_verbs`. Timeout is 10 s on every verb **except** `send-message`, which runs with no timeout.
- **Only one OSA error triggers the SMS fallback.** `{-1743: messages_automation_denied, -1728: messages_addressing_failed, -1700: messages_addressing_failed}`; anything else becomes `helper_<verb>_failed`. `auto` service means try `iMessage` then `SMS`, and only when the failure is exactly `messages_addressing_failed`. For a `chatId` target the service is inferred from the GUID prefix (`SMS;`/`any;` → SMS) and an explicit `--service` is rejected as `cli_unexpected_service`.
- **Message text never crosses argv.** Every send invokes the helper with `--text-file`; `--text-file` is not merely a CLI convenience.

### 7.5 Stale enumerations that will bite a 0.29/0.30-targeting client

The telemetry known-kind array (§5.1) was never updated for `voice-call` and `feedback`, so those two kinds land in the `"unknown-kind"` telemetry bucket, and the renderer mirror of that same array drives the placeholder-height function — meaning an unrecognized kind falls to a 32 px placeholder. This is a real 0.29/0.30 defect, not a documentation artifact.

### 7.6 `tool-call` entries validate but never render

`toolCallCardKey` returns `null` in all three versions. A tool-call entry is durable, is in the replica allowlist, is in the telemetry list — and produces no row. Do not treat "no card" as "drop the entry".

### 7.7 The `"error"` kind is 0.18-only and transient

0.18 rejects `kind:"error"` at parse time; 0.29/0.30 have no such rejection, and their validator will simply return `false` for it via the exhaustive default. If og-wire emits `"error"` entries, 0.18 clients silently drop them at `parseTranscriptEntry` and never persist them.

### 7.8 0.30's renderer moved `mcp-auth-completed` off SSE

- 0.18: desktop-event map only (`index-UbX-y3il.js@5729293`)
- 0.29: SSE map only (`index-Dklr8uwG.js@4235137`, `"mcp-auth-completed":$e=>je.ingestAuthCompleted($e)`)
- 0.30: **back to the desktop-event map** (`index-DCpFUyZ2.js@4265566`), while the family stays declared in the coordinator's family table

So a backend emitting the `mcp-auth` SSE channel to a 0.30 renderer is **silently ignored**. The 0.30 renderer's SSE ingest map has 17 keys, not 18.

### 7.9 `agent-activity` can be conditionally silent in 0.30

0.30's publisher gates emission behind a new predicate where 0.29 used a bare boolean:

```js
// E30:dist/node-agent-coordinator/main.cjs @~161133
d=o(U=>l&&t.isLegacySourceActive?.()!==!1||t.requiresServer?.({agentId:U})===!0,"shouldEmit")
```
0.30's `clientOverlayOf` also appends an eighth, non-optional field absent from 0.29: `isHiddenFromSidebar:t.hiddenFromSidebar`. The 0.29 shape is `{hasUnread, unreadCount, lastViewedAt?, lastActivityAt?, newestEntryId?, lastMessageId?, lastMessagePreview?}`; `liveOverlayOf` and `activityOf` are unchanged. Idle sentinel `{isRunning:!1,isComposingMessage:!1,isRetrying:!1,awaitingUserResponse:null}`; stale timeout 90 s; gate recheck 60 s; withdrawal is `{agentId, live:null, client:null}`.

0.18 has no `agent-activity` channel at all (`grep -c '"thinking"\|activityOf\|isComposingMessage'` over `A18:dist/node-agent-coordinator/main.cjs` → 0); it computes the same fields in the **host** and folds them into the roster row (`A18:dist/host/host-main.cjs:647406`, `withRunStates(agents)` → `{isRunning, isRunningTurn, isComposingMessage, isRetrying, currentActivity, activeRemoteMemberId}`). Note `isRunningTurn` and `activeRemoteMemberId` exist in the 0.18 host overlay and **not** in the 0.29/0.30 coordinator's `liveOverlayOf`. For our 0.18 client, the roster row is the delivery vehicle — there is no separate activity channel to implement.

### 7.10 `/sand/notify` is a second, unrelated SSE stream — do not merge the lists

```js
// A18:dist/host/host-main.cjs:627165 — module src/host/extensions/notify-bus/notify-bus-client.ts
var SAND_NOTIFY_TOPIC_FLAGS = {"automation-fires":true,"listener-events":true,"xuser-events":true};
var SAND_NOTIFY_TOPICS = Object.keys(SAND_NOTIFY_TOPIC_FLAGS);
```
Upstream `GET {backend}/sand/notify` with `accept: text/event-stream`; frames are `{kind:"connected"}` or `{kind:"notify",topic}`; stall watchdog 35 s, reconnect 1 s → 60 s, healthy-connection floor 30 s, every topic re-fired on reconnect. `OURS/host/extensions/notify-bus/notify-bus-client.ts` reproduces it exactly. This is **not** the gateway `/events` channel set. **Unverified for 0.29/0.30** (`grep -rlo 'automation-fires\|xuser-events\|sand/notify'` over both extracted trees returns nothing).

### 7.11 Gate defaults are not enablement

Every gate is `client:true`, defaults never change in the literal, and ramping happens server-side. The `default` value is only the client's fallback when it has no Statsig value. Two gates additionally require an authenticated bootstrap before they can be read at all (`sand_scheduled_computer_updates`, `sand_five_min_automation_floor`).

Also note the ten kill-switch-named gates in 0.30 all ship in the *not-killed* position (OFF): the nine commonly cited plus `local_commit_reflog_signal_killswitch`.

### 7.12 Small name traps

- `deleteAgent` (singular) exists **only** in 0.18's gateway table; `deleteAgents` exists in every version. Both are present in 0.18.
- `setAgentWorkflowEnabled` is in 0.18 and gone by 0.29.
- 0.30's `main` edge grew by 22 but 16 of those are the MCP/media block being *mirrored* from `gateway` (the `gateway` copies stay). Four arrive under **shortened names** that are actually 0.18 spellings — a revert, not a coinage:

| `main` (0.30, "new") | `gateway` (both versions, retained) | 0.18 `MAIN_METHOD_TABLE` |
|---|---|---|
| `getEffectivePlugins` | `getEffectiveMcpPlugins` | `getEffectivePlugins` |
| `installEntry` | `installMcpEntry` | `installEntry` |
| `updatePluginInstall` | `updateMcpPluginInstall` | `updatePluginInstall` |
| `uninstallPlugin` | `uninstallMcpPlugin` | `uninstallPlugin` |

Genuinely new-to-the-product in 0.30 are therefore only 11 methods: `cancelCookieOriginApproval, getBotTemplateExportPolicy, getCursorEnterpriseUsage, isMultiMachineLocalExecEnabled, listPublicBotMarketplace, presentCookieOriginApproval, reportOnboardingCompleted, reportTurnClientOutcome, reportTurnClientStart, resolveVirtualCardApproval, setGrokBotAgentSidebarHidden`.
- A gate can outlive its RPC: `setAccentPreference` is gone from 0.30 entirely while `sand_enable_accent_theming` remains in the registry.

---

## 8. Unverified ledger

Everything in this list was checked and could not be established from the artifacts we hold. Nothing here is smoothed over.

1. **All of 0.27 except the proto/service surface.** No binary exists. Transcript kinds, card types, SSE families, gateway commands, gate registry, daemon frames — all blank for 0.27.
2. **0.27 `SandBoxService` I/O types.** 37 of 76 `GrokBotService` rows are unresolvable because `F27/grok_bot_pb-0.27.fragment.js` binds only GrokBot-domain types.
3. **Whether `F27/grok_bot_pb-0.27.fragment.js` holds exactly 130 `aiserver.v1` names.** 132 `typeName` bindings were observed; the 130-vs-132 gap was not reconciled.
4. **Whether the 0.18 gateway commands removed by 0.29 still exist server-side** in 0.29/0.30. The server is not shipped.
5. **All host-private 0.18 behaviour, for 0.29/0.30**: the `stripReplyTo`/`withReplyTo` threading rewriters, avatar stripping location, SSE gzip/heartbeat mechanics, `/sand/notify`. `dist/host/` does not exist in those bundles.
6. **Where residual client-side avatar stripping lives in 0.29/0.30**, if anywhere. `x-sand-slim-avatars` is still sent; `stripSummaryInlineAvatar`/`stripSummaryRows` do not occur by name, but the bundles are minified so name-absence is weak evidence.
7. **Per-channel handler attribution for 0.18's `sand:*` set.** Only 23 of ~71 channels have a directly visible registration.
8. **0.18 daemon JSON frame kinds and `SandLocalToolAction` values.** Not extracted; the 0.29 baseline of 12 frame kinds / 5 actions is the earliest verified point.
9. **The brief's "daemon frame kinds 12 → 24."** 12 is right for 0.29. 0.30 is 15 JSON kinds, or 27 including protobuf oneof members (0.29: 21). No counting rule constructible from these bundles yields 24. Treat 24 as wrong, not merely unconfirmed.
10. **Field-granularity recovery of the ~3700 electron-main-only `aiserver.v1` types** (Dashboard / Ai / BackgroundComposer). Only the GrokBot/Sand coordinator slice was extracted at field level.
11. **The semantics of `PreviewGrokBotMarketplaceSourceInternalRequest.source_ref`** (share id vs template id vs URL). The literal gives only name and type.
12. **Whether every 0.29/0.30 `EXPERIMENTS` / `DYNAMIC_CONFIGS` entry conforms to `{client, fallbackValues}`.** Asserted from the registry's opening shape; not exhaustively checked across all 111 / 137 entries.
13. **Whether `OURS` has an equivalent of `src/shared/gateway-wire.ts`** carrying the path and header constants. Structurally 0.18 keeps them in a separate module too, so our `gateway-protocol.ts` correctly omits them — but the sibling module was not located.

---

## 9. Index row for `opengrok/docs/research/README.md`

| doc | what it covers | headline findings |
|---|---|---|
| [`client-versions-0.18-0.30.md`](./client-versions-0.18-0.30.md) | Every client protocol surface across 0.18 / 0.27 / 0.29 / 0.30, with per-version counts, provenance paths, and the exact deltas — gateway commands, internal RPC edges, transcript/card/SSE contracts, local-exec daemon frames, feature gates, and an inventory-only `aiserver.v1` proto listing. | **Our pinned client is 0.18-era** (renderer `index-UbX-y3il.js`; `gateway-protocol.ts` = 0.18's 120 commands + 3 local inventions) — build to 120 commands / 6 entry kinds / 13 card types / 16 SSE families, not the later supersets. The "332 → 345 RPC methods" are Electron IPC, **not backend surface**; the wire is `POST /api/<method>` + SSE `/events` + Bearer in every version. **0.27 has no binary** — auto-update destroyed it; only three evidence fragments survive. **No version has a per-message delete or edit RPC.** 0.29/0.30 additions are almost entirely gated **OFF**, including voice calls and the entire local→server storage migration, and **zero gate defaults flipped 0.18 → 0.30**. Corrected counts: feature gates **699 → 708** (not 691 → 700); daemon frame kinds **12 → 15** (not 12 → 24); 0.18's "73 `sand:*` channels" is a literal count, true value ≈ 71. |