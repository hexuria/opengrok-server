# Roadmap

The living tracker. `docs/GOAL.md` is the canonical mission record and the why; this file is the
what-is-left, in checkboxes.

**The rule: a box is ticked only in the commit that makes it true.** Never ticked in advance,
never ticked for work that is "basically done", and each tick names its commit — *(this commit)*
is allowed when the tick rides with the work itself, since `git blame` resolves it. A box with no
commit next to it is a claim nobody has to believe.

---

## Done

- [x] **Slice 1 — Auth.** Our own OAuth replacing Cursor's: dev sign-in, refresh, revocation.
  (`582521f`)
- [x] **Slice 2 — AG-UI.** openbot can add us as a Bot; correct event envelope, shape asserted not
  frame-counted. (`6d3a896`, honest smoke `8df6c1e`)
- [x] **Slice 3 — A run reaches a model.** Harness, projection, SSE; the mock door so CI never
  needs a key. (`5d6323c`)
- [x] **Slice 4 — Durability.** Journal-before-next-call ordering (`30166d6`), computers via
  box.ascii.dev and local Docker (`2ea50ae`, `144a0db`), the joined chain (`87ef659`), roster
  (`817ec33`), policy layers 1–3 (`be35525`), run ownership (`9d6cbc4`), approvals + exactly-once
  answers (`47705a1`, `51f4084`), self-resuming runs (`28c0182`), crash recovery (`e3e0963`).
- [x] **Slice 5 — Connectors.** Connection scopes + lending and the plugin catalogue (`2983ec8`),
  the credential vault (`d8fe3ea`), OAuth 2.0 with provider habits as tests (`22f220d`), MCP over
  HTTP (`869708f`), plugin tools under the same policy as `shell` (`726a3e6`), wired end to end
  with refresh-before-use (`48b885a`, `dd394e9`).
- [x] **First real client run.** barok-works registered OpenGrok as a Bot and a real model answered
  through the whole stack — which immediately found the coworker-model bug. (`afd2cf3`)

## Slice 6 — Scheduler + monitor: coworkers that act on their own

The mission is "keeps working when the laptop is off", and today every run starts with a client
POST. This slice makes the server start runs itself.

- [x] **6a.1** `schedule` aggregate in `opengrok-core`: Create/Pause/Resume/Delete, cron validated
  in `decide`, pure and unit-tested. *(this commit)*
- [x] **6a.2** Projection + store: `schedule_view`, `claim_due_schedules` advancing `next_due_ms`
  inside the claiming update (crash between claim and fire skips one occurrence, never
  double-fires). *(this commit)*
- [x] **6a.3** The tick: a sweep beside `recovery::sweep_forever` that fires due schedules through
  `run_conversation`, on the coworker's own model and tools, with `policy_for` checked at fire
  time. *(this commit)*
- [x] **6a.4** Endpoints: `POST/GET /schedules`, pause/resume, delete — ownership as 404, same as
  runs. *(this commit)*
- [x] **6b.1** `monitor` aggregate + projection: an event-type matcher over our own `events` table,
  cursor-driven, no new infra. *(this commit)*
- [x] **6b.2** The loop guard: fired runs are stamped with their monitor, and a monitor never
  matches events from its own firings. *(this commit)*
- [x] **6b.3** Endpoints: `/monitors`, same shape as `/schedules`. *(this commit)*
- [x] **6.v** `scripts/slice10-autonomy-smoke.sh` in `gate.sh`: a run appears that no client
  started; SIGKILL mid-schedule and firing resumes after restart; a monitor fires and the loop
  guard holds — it watches `run-started`, the sharpest case, since its own run emits that very
  event. *(this commit)*

## The port ladder — from `docs/PORT-PRIORITY.md` (30 Aug 2026)

The other half of the mission is the real Grok Bot client running against us. The port plan
measured the client rather than recalling it: **123 gateway commands (JSON+SSE, not protobuf)
and 18 Seam-B methods** — the client's own checked-in mock (`source/mock/`) implements exactly
two services and those 18 methods, and the app boots against it. **Port from the mock, never
from the proto inventory.** And because `SAND_HOST_GATEWAY_URL` repoints the client at any
gateway with no auth work, the build order inverts the ship order: gateway first, identity
after — the real, unmodified client is the strongest smoke test we can have.

## Slice 7 — The gateway boots the real client (P2 + P3)

- [x] **7.1** `GET /health` on a 1500 ms deadline, and `GET /events`: `retry: 1000`, `:ping`
  at ≤15 s (a 35 s watchdog aborts otherwise), channel filter parsed, one shared bearer
  compared timing-safe. *(this commit)*
- [x] **7.2** P3's 12 roster/settings commands — shape discipline over behaviour:
  `countAgents` a number, `getTrays` an array, `getForeverBoxStatus` null-or-record,
  `setHostSettings` echoing the full record back. *(this commit)*
- [x] **7.3** The trap, honoured: serve on a **non-loopback** host — verified live on
  `http://192.168.100.21:1447` with the pinned bearer (`OG_GATEWAY_BEARER`), 401 without it.
  *(this commit)*
- [ ] **7.v** Launch the shipped app with `SAND_HOST_GATEWAY_URL` pointed at us and see a
  populated sidebar. **Blocked in the client, not here:** setting that env var deadlocks the
  reconstructed app before its window opens — isolated and written up in
  `docs/port-blockers.md` B1. The wire contract is held by `slice11-gateway-smoke.sh`
  meanwhile.

## Slice 8 — A conversation from the real app (P4)

The milestone that proves the port; everything after it is breadth, not risk.

- [x] **8.1** P4's 13 conversation commands (`sendPrompt` with the Postgres acceptance ledger —
  idempotent on a repeated nonce, 409 `NONCE_DIGEST_MISMATCH` on a reused one — the four
  tail/window/page reads, the array forms, `openAgent`, `promptAcceptanceStatus`) backed by the
  harness we already have; turns run on the coworker's own model and are journaled like every
  other run. *(this commit)*
- [x] **8.2** The two SSE shapes that carry an answer: `transcript` `appended`/`updated` (user
  echo carrying `clientNonce`, streaming placeholder, final update) and `agent-upserted` pulsing
  `isRunning` — every frame stamped `ordered: {replicaKey, epoch, sequence}`, plus an `agents`
  snapshot on every `/events` connect. *(this commit)*
- [ ] **8.v** Send a message from the real, unmodified app and watch the answer stream back.
  Blocked by the same client bug as 7.v (`docs/port-blockers.md` B1); the choreography is held by
  `slice12-conversation-smoke.sh` meanwhile.

## Slice 9 — Seam B: identity and the mint (P0 + P1)

Re-scoped by the port plan from "hundreds of messages" to a bounded job: **two services,
18 methods, transcribed from `source/mock/`** with provenance comments (LEGAL.md stands —
no vendored stubs). Connect-style unary (POST + JSON over HTTP/1.1) at the Axum edge; a bare
tonic gRPC server cannot answer the client.

- [ ] **9.1** Auth: `/loginDeepControl` (PKCE-shaped challenge), `/auth/poll`, `/oauth/token`
  with a real `exp` — the client parses the JWT for expiry and loops on refresh without one.
- [ ] **9.2** `DashboardService` — 6 methods (`getMe`, `getTeams`, both admin-settings
  variants, `getUserPrivacyMode`, `updateUserName`).
- [ ] **9.3** `GrokBotService` — the mock's 12, plus `EnsureSandBox` returning
  `{gatewayUrl, gatewayToken, networkToken, vncUrl, forkVncBaseUrl}` with a **non-loopback**
  `gatewayUrl`.
- [ ] **9.4** The same services on a tonic gRPC listener for internal use — the operator
  decision of 30 Aug, unchanged.
- [ ] **9.v** Remove `SAND_HOST_GATEWAY_URL`; the client mints its own connection through us.

## Slice 10 — Bot ↔ coworker binding (barok-works)

Runs from a client Bot arrive anonymous today: no tools, no policy, the deployment's model.
Access tokens live one hour, so a Bot registered with a static header dies hourly.

- [ ] **10.1** `POST /coworkers/{id}/keys`: a durable, revocable bot-key (signed, names account +
  coworker; a `key_view` row makes revocation real).
- [ ] **10.2** `account_from_bearer` accepts it; a bot-key names the coworker for the run.
- [ ] **10.3** Proven from barok-works: a Bot registered with the key runs as its coworker — tools,
  approvals, and the coworker's own model, from the UI instead of curl.

## Slice 11+ — breadth (P5 → P10, in order)

Per-tier, verified against the running client. Most of it adapts work that exists:
P6 approvals ride slice 4's exactly-once answers, P8 MCP/skills ride `opengrok-plugins` and the
vault, P9 automations ride slice 6's scheduler and monitor, P10 box lifecycle rides
`opengrok-box`.

- [ ] P5 agent lifecycle (19 commands, 10 already answered client-side)
- [ ] P6 tools, approvals, widgets (7)
- [ ] P7 attachments, media, `/avatars/<id>`, `x-sand-slim-avatars` (5)
- [ ] P8 MCP and skills (15)
- [ ] P9 automations and workflows (15)
- [ ] P10 box lifecycle and store (13)

**P11 is deliberately not here.** Sharing/rooms, teach recording, channels, memories and the
other 24 commands sit on no path a user takes, and upstream deleted adjacent features in 0.30.
Listing them as pending would make this tracker lie about how far away done is.

## Later — unordered, deliberately

- [ ] Commands: `goal`, `plan`, `review`.
- [ ] mem0 (exists only as a catalogue entry today).
- [ ] Artifacts/uploads — parked on purpose; lands with or after the harness produces files worth
  storing (design notes in GOAL.md).
- [ ] stdio MCP servers inside a coworker's own container (the follow-up to HTTP-only).
- [ ] Graph harness (the loop is linear today, `MAX_ROUNDS = 8`).
- [ ] Redis — only after a measured hot query, per the standing decision.

## Blocked on the operator, not on code

- [ ] GitHub Actions CI — billing. `scripts/gate.sh --smoke` is the gate meanwhile.
- [ ] Rights review → publication (LEGAL.md; the repo stays private until then).
- [ ] gpt-5.6-luna — upstream credits (`personal-team-blocked:spending-limit`); terra/5.5/5.4-mini
  verified working through the same gateway.
