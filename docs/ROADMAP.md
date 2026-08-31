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
- [x] **7.v** The packaged app boots against us and shows a populated sidebar — via the
  client's **OpenGrok server mode** (`boxRuntime: "opengrok"` + the `openGrokGatewayUrl`
  setting), not the `SAND_HOST_GATEWAY_URL` env var, which still deadlocks the app and is a
  dead path now (B1 re-checked 1 Sep). Evidence: `docs/verification/real-client/README.md`
  and the 31 Aug acceptance run it cites. (`0ae194e`)

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
- [x] **8.v** A message sent from the packaged app runs on this server and the answer streams
  back — the 31 Aug acceptance flows (real-judge refusal, auto-review cards) each began with a
  prompt typed in the app and ended with its reply rendered there. Evidence:
  `docs/verification/real-client/README.md` → `docs/verification/auto-review/README.md`
  (streams `run_01a057f0-…`, `run_01a057f5-…`). (`0ae194e`)

## Slice 9 — Seam B: identity and the mint (P0 + P1)

Re-scoped by the port plan from "hundreds of messages" to a bounded job: **two services,
18 methods, transcribed from `source/mock/`** with provenance comments (LEGAL.md stands —
no vendored stubs). Connect-style unary (POST + JSON over HTTP/1.1) at the Axum edge; a bare
tonic gRPC server cannot answer the client.

- [x] **9.1** Auth at the mock's own surface (`source/mock/auth-http.ts`): `/auth/poll` minting
  the `{accessToken, refreshToken}` pair, on top of slice 1's `/auth/cursor_dev_session_token`
  and `/oauth/token` with real `exp`s. *(this commit)*
- [x] **9.1b** The `/loginDeepControl` PKCE browser leg — built, gated, live. Registers the
  challenge, binds it to the host account (the person opening the URL is the auth on a
  single-user self-hosted server), and `/auth/poll` only releases a token for a matching
  verifier (404-as-pending otherwise) — which CLOSED a real hole the peer caught: the old blind
  poll minted a LAN-reachable account token that leaked the gateway bearer via EnsureSandBox.
  Dev sign-in is now loopback-only. `slice16-browser-login-smoke.sh`. (`0deb7a3`)
- [x] **9.2** `DashboardService` — 6 methods, Connect unary on Axum, enums by name, plus the
  mock's load-bearing leniency: an unmodelled method answers an empty message. *(this commit)*
- [x] **9.3** `GrokBotService` — the mock's 12 (transcripts as base64 bodies with string seqs,
  sends idempotent on `(agent, messageId)`, real turns instead of the mock's canned line), plus
  `EnsureSandBox` minting OUR gateway: `OG_PUBLIC_GATEWAY_URL` + the gateway bearer, refused
  outright when no non-loopback address is configured. *(this commit)*
- [x] **9.4** tonic is in: `proto/opengrok_seamb.proto` (hand-transcribed, provenance in the
  file, codegen into target/ never the tree), both services on an opt-in `OG_GRPC_BIND`
  listener, proven by a real tonic client in `against_our_own_grpc.rs` — unauthenticated
  refusal, GetMe, the mint. *(this commit)*
- [ ] **9.v** The client mints its own connection through us: switch the packaged app into
  OpenGrok server mode, sign in at `/loginDeepControl`, and watch `EnsureSandBox` hand back
  `OG_PUBLIC_GATEWAY_URL` + the bearer. (The old "remove `SAND_HOST_GATEWAY_URL`" framing is
  obsolete — the env var is a dead path; see `docs/verification/real-client/README.md`.) The
  contract is held by `slice13-seamb-smoke.sh` and the tonic round-trip test meanwhile.

## Slice 10 — Bot ↔ coworker binding (barok-works)

Runs from a client Bot arrive anonymous today: no tools, no policy, the deployment's model.
Access tokens live one hour, so a Bot registered with a static header dies hourly.

- [x] **10.1** `POST /coworkers/{id}/keys`: a durable, revocable bot-key — signed with a `use`
  discriminator so an access token can never pass as one, shown exactly once at mint, its
  `bot_key_view` row making revocation real. List and revoke ride the same 404-not-403
  ownership rule. *(this commit)*
- [x] **10.2** `principal_from_bearer` accepts it, and the key NAMES the coworker: a bare
  POST /ag-ui with nothing but the key runs as the coworker, on its model, owned by the minting
  account — and a revoked key answers 401 rather than silently downgrading to anonymous.
  *(this commit)*
- [ ] **10.3** Proven from barok-works end to end. Every hop holds separately — the key sits in
  their vault (`hasAuth: true`), their loader attaches it per load, and the same minted key via
  curl runs owned on the same live server — but the one browser send with the header attached is
  still owed: the first attempt bound the STALE duplicate Bot (the package-sync-never-prunes
  finding, now cleaned up), and the retry died under machine load. One quiet-machine send
  closes it.

## Slice 12 — Identity: orgs, invites, credential accounts (`796bf61`)

Uriah's UI review turned the single-user host into a real, multi-tenant identity model.

- [x] **12.1** `org` aggregate — name, admin, domains, invites; `RedeemInvite` enforces BOTH the
  open-code gate and the domain-match gate atomically, each refusal distinguishable.
- [x] **12.2** `account` extended — argon2id password, name, org, `verified` (Resend-driven),
  `enabled` (admin-flipped); credential login checks all three in order.
- [x] **12.3** CLI (`opengrok admin org create / invite / account enable / account create`) — the
  operator bootstraps the first org from shell; no HTTP admin surface. `account create` mints a
  ready test identity (the multi-account-under-a-different-name need).
- [x] **12.4** HTTP — `POST /auth/signup` (both gates), the credential form at `/loginDeepControl`
  (superseding 9.1b's opener-is-host), `GET /auth/verify`. Resend behind `OG_RESEND_API_KEY`:
  set ⇒ send + require verification, unset ⇒ auto-verify.
- [x] **12.v** `slice17-identity-smoke.sh` — CLI bootstrap → invite → domain-gated signup →
  verify → enable → credential login → token; verified live over the LAN. (`796bf61`)
- [ ] **12.later** Domain OWNERSHIP proof (DNS challenge) — matching is in v1, ownership deferred;
  password reset via Resend; an in-app admin surface for invites/enable (CLI-only in v1).

## Slice 13 — Web console

- [x] **13** The account + admin dashboards as a Bun/Vite/React/TanStack SPA the
  server hosts at `/console` (Axum `ServeDir`-style handler, SPA deep-links 200 via index). Browser
  auth is httpOnly cookies (`/auth/login|logout|refresh`; no token in JS), `caller()` accepts the
  cookie or the Bearer header. Account self-service (name, avatar data-URL, password; email fixed)
  and the org-admin surface (users list, enable/disable, invite links) that 12.later deferred —
  `isAdmin`-gated in the client, enforced on the API. Guards: an admin cannot self-disable (409);
  login no longer clobbers the account projection. `slice19-web-console-smoke.sh` +
  `tests/against_the_web_console.rs`; browser-verified in `docs/verification/web-console/`.
  (merged in `12748f0`)

## Slice 14 — One consent model (merged 31 Aug, PR #1 `12748f0`)

Running a bot command on the user's own Mac had four overlapping controls grown slice by slice.
Collapsed, with the user's review, to one model: the Mac switch is a local on/off kill switch;
the server's per-machine policy (off/ask/always + visible, deletable standing rules) decides;
the inline card is the ONE consent surface and never expires; card Always/Never write a server
standing rule and nothing else. Design: `docs/AUTO-REVIEW.md`.

- [x] **14.1** `SuspendReason` on suspend/answer/pending (`#[serde(default)]` so old rows keep
  their meaning); cards chosen by reason, not tool name.
- [x] **14.2** Auto-review, two tiers (global → per-coworker, per-field inheritance,
  `''` = explicit none): store rows, `/auto-review/policy` + `/effective`, machine tier removed
  and legacy rows purged idempotently.
- [x] **14.3** Enforcement at the executor seam: identity overwrite for every tool, primary gate,
  ONE judge call site (`ModelJudge` — own route via `OG_AUTO_REVIEW_MODEL`, empty tools, 8 s
  timeout, strict one-word verdict, failure ⇒ ask, never a silent allow), pure
  `combine(gate, review, approved)` ladder — a block refuses naming the instruction; at most one
  card per call; a review approval never releases the machine's own consent.
- [x] **14.4** `resolveAutoReviewApproval` for real: same-entry status flip, exactly-once answers,
  heal-on-press to `expired`+410 for dead runs, deny resumes the run with a refusal result the
  bot explains. `tests/against_auto_review_gate.rs` drives it through the real router.
- [x] **14.v** End-to-end on the shipped path: real judge refusing `brew install jq` with the
  rule named in the bot's reply; mock window raising exactly one card that flips in place.
  Paired evidence `docs/verification/auto-review/README.md` + the client repo's
  `docs/consent-model-B5-acceptance.md`. Known, accepted v1 scope: the pinned 0.18 card's
  "Always" writes the global tier (client-side; the per-agent widget writes the coworker tier).

## Slice 15 — Door 1, the model half: Claude Code through our gateway

The three-doors assessment (1 Sep) found this already built in open-ai-gateway; the slice is
the proof, not construction.

- [x] **15.v** With nothing but `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`, Claude Code
  chats through open-ai-gateway and the usage ledger records it (25k-token system prompt, real
  cost, streamed). `/v1/models?claude_code=1` serves the alias twins. Dev provider is the
  machine's own opencodex door via `gateway.provider_base_urls` — no credential invented.
  Evidence: `docs/verification/door1/README.md`. (`81845a2`)

## Slice 16 — Door 1, the tool half: the MCP door

- [x] **16.1** `POST /mcp` (rmcp server-side, streamable HTTP, stateless + json_response): a
  bot key names the coworker; `tools_for_coworker` builds the same policy-wired runner a run
  gets; every call goes through `Executor::execute` — identity overwrite, gate, judge, audit.
  Ask fails closed with instructions (no run to suspend; cards are a follow-up); a
  computerless coworker lists an empty toolbox, not a broken handshake. (`46a353c`)
- [x] **16.2** Listings declare `ttlMs: 0, cacheScope: private` — required by MCP protocol
  2026-07-28 (SEP-2549), and right for a per-key, policy-filtered list. Found live: Claude
  Code rejected the listing without them. (`2502deb`)
- [x] **16.t** `tests/against_the_mcp_door.rs` (hand-written JSON-RPC client, never
  rmcp-to-rmcp) + `scripts/slice20-mcp-door-smoke.sh` in the gate: the slice-7 attack replayed
  through MCP (foreign coworkerId overwritten), ungranted tool refused naming the rule,
  person-token pointed at the bot-key mint, revoked key refused. (`46a353c`)
- [x] **16.v** Validated on Claude Code itself: `claude mcp list` shows the live door
  connected, and one invocation with both doors configured answered via the gateway, called
  `mcp__opengrok__shell`, and ran it on the coworker's own container — the tool's hostname
  output IS the container id. Evidence: `docs/verification/door1/README.md`. (`2502deb`)
- [ ] **16.later** Card-driven MCP approvals (synthesize a durable run so an ask raises a real
  card); OAuth 2.1 metadata on /mcp; the org-key mint console surface (slice 17 of the
  three-doors order).

## Slice 11+ — breadth (P5 → P10, in order)

Per-tier, verified against the running client. Most of it adapts work that exists:
P6 approvals ride slice 4's exactly-once answers, P8 MCP/skills ride `opengrok-plugins` and the
vault, P9 automations ride slice 6's scheduler and monitor, P10 box lifecycle rides
`opengrok-box`.

- [x] P5 agent lifecycle — create (nonce-deduped), update, delete(s), duplicate, search,
  avatars, the shipped host's no-ops kept as no-ops; groups refused readably. (`c8ee938`)
- [x] P6 entry mutation — reactions, widget answers/dismissal, deletion, each with its
  `updated`/`removed` SSE frame. (`c8ee938`)
- [x] P7 — `GET /avatars/<id>` serves the stored bytes behind slim rosters; attachment
  commands refuse readably until the artifacts slice lands (they are its client surface).
  *(this commit)*
- [x] P8 — `skillsCatalog` lists the curated plugins' own skills; sync status is real;
  publishing and routed-MCP execution refuse readably (a coworker's connections drive MCP on
  this server, from runs). *(this commit)*
- [x] P9 automations — slice 6's schedules wearing the client's names; one scheduler, two
  vocabularies, the same rows readable under `/schedules`. Workflows stay honest empties.
  (`c8ee938`)
- [x] P10 — the box control surface over what the deployment has: null (the validated
  truth) with no provider, a status record with one, lifecycle verbs accepted as no-ops so a
  click is not an error banner. Real assignment stays slice 4's machinery. *(this commit)*

**P11 is deliberately not here.** Sharing/rooms, teach recording, channels, memories and the
other 24 commands sit on no path a user takes, and upstream deleted adjacent features in 0.30.
Listing them as pending would make this tracker lie about how far away done is.

## Later — unordered, deliberately

- [ ] Commands: `goal`, `plan`, `review`.
- [ ] Per-coworker model pins — investigated, not implemented; the dialect/default/surfaces
  decision is written up in `plan-coworker-model-pins.md` and awaits an adversarial pass.
- [ ] Passkey step-up for reverse-exec (scope 3 of the original design, now in
  `archive/reverse-exec-design.md`) — parked on the peer's macOS WebAuthn ceremony.
- [ ] Channels / multi-party rooms (phases 3–4 of `archive/plan-bots-computers-channels.md`) —
  the provisioning half shipped (`41245b5`); the rooms half deliberately waits with P11.
- [ ] mem0 (exists only as a catalogue entry today).
- [ ] Artifacts/uploads — parked on purpose; lands with or after the harness produces files worth
  storing (design notes in GOAL.md).
- [ ] stdio MCP servers inside a coworker's own container (the follow-up to HTTP-only).
- [ ] Graph harness (the loop is linear today, `MAX_ROUNDS = 8`).
- [ ] Redis — only after a measured hot query, per the standing decision.

## Blocked on the operator, not on code

- [ ] GitHub Actions CI — billing. `scripts/gate.sh --smoke` is the gate meanwhile.
- [ ] Rights review → publication (LEGAL.md; the repo stays private until then).
- [ ] gpt-5.6-luna — upstream credits (`personal-team-blocked:spending-limit`); 5.5/5.4-mini
  verified working through the same gateway.
