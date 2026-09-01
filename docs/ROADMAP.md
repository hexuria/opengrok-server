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
  when that URL is unset. The client refuses a loopback host, so the configured address must
  not be one — asserted by `slice13-seamb-smoke.sh`, not by the mint. *(this commit)*
- [x] **9.4** tonic is in: `proto/opengrok_seamb.proto` (hand-transcribed, provenance in the
  file, codegen into target/ never the tree), both services on an opt-in `OG_GRPC_BIND`
  listener, proven by a real tonic client in `against_our_own_grpc.rs` — unauthenticated
  refusal, GetMe, the mint. *(this commit)*
- [x] **9.v** The client mints its own connection through us: packaged Open Grok.app called
  `signInToOpenGrokServer`, PKCE login bound a throwaway account, `/auth/poll` went 404→200,
  `EnsureSandBox` returned `OG_PUBLIC_GATEWAY_URL` and the bearer, and
  `boxRuntime`/`openGrokGatewayUrl` persisted. `docs/verification/real-client/9v-mint.md`.
  *(this commit)*

## Slice 10 — Bot ↔ coworker binding (barok-works)

A client Bot used to arrive anonymous: no tools, no policy, the deployment's model.
Access tokens live one hour, so a Bot registered with a static header died hourly.

- [x] **10.1** `POST /coworkers/{id}/keys`: a durable, revocable bot-key — signed with a `use`
  discriminator so an access token can never pass as one, shown exactly once at mint, its
  `bot_key_view` row making revocation real. List and revoke ride the same 404-not-403
  ownership rule. *(this commit)*
- [x] **10.2** `principal_from_bearer` accepts it, and the key NAMES the coworker: a bare
  POST /ag-ui with nothing but the key runs as the coworker, on its model, owned by the minting
  account — and a revoked key answers 401 rather than silently downgrading to anonymous.
  *(this commit)*
- [x] **10.3** Proven from barok-works end to end. A browser send from the OpenGrok channel
  landed as Hexuria, owned (`run_view.account_id` non-null, thread id not `gateway-<coworker>`).
  The first send of the day *looked* like success and was anonymous: the vault held a key
  minted for a previous account, which does not verify, which is `Ok(None)`. `hasAuth: true`
  is not proof. `docs/verification/barok-bot-binding/`. *(this commit)*

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
- [x] **16.r** Peer review (two reviewer agents on the diff) closed one real security bug and
  hardened the door: `overwrite_identity`/`strip_identity` in `opengrok-tools` missed the
  camelCase identity aliases (`coworkerId`/`boxId`) they were supposed to strip, so a plugin
  call could forward an attacker-chosen coworker id to a connector — the exact confused-deputy
  #7 prevents; now every alias from one shared list is stripped. Door: auth + browser-origin
  refusal moved to a transport-edge layer (real 401/403, `initialize` gated), an unreachable
  computer is an error not an empty toolbox, reverse-exec excluded from MCP, the ask reply no
  longer promises a nonexistent card, serverInfo names OpenGrok, concurrent calls serialized
  per coworker. (`3980be2`)
- [x] **16.v** Validated on Claude Code itself: `claude mcp list` shows the live door
  connected, and one invocation with both doors configured answered via the gateway, called
  `mcp__opengrok__shell`, and ran it on the coworker's own container — the tool's hostname
  output IS the container id. Evidence: `docs/verification/door1/README.md`. (`2502deb`)
- [x] **16.cards** An MCP Ask synthesizes a durable run and a real `auto-review-approval`
  card (`requestId` = the tool call id); the MCP error names it and does not wait. The person
  answers in OpenGrok (`resolveAutoReviewApproval`), which Finishes the synthesized run;
  the MCP client retries under the remembered call id. PolicyApproval still has no
  transcribed desktop card; reverse-exec stays excluded. *(this commit)*
- [ ] **16.later** OAuth 2.1 metadata on /mcp; the org-key mint console surface (slice 17 of the
  three-doors order).

## Slice 17 — one identity across both doors

"Same org, different front doors" made literal: an org admin, from the console, hands a member a
key that opens the model door, sets the org's budget and that member's cap, and sees the spend.

- [x] **17.1** open-ai-gateway gains the identity-integration admin surface (its own repo, PR
  `ac3effc`): `POST /admin/api/principals`, `POST /admin/api/keys` (plaintext once),
  `PATCH …/budget`, `PATCH …/quota`, `GET …/usage` — behind the existing admin auth. A key minted
  over HTTP is **never** `admin`, so the surface cannot widen its own authority; money crosses the
  wire as a string, never a float. Store round-trips + the router's hardcoded admin-route table.
- [x] **17.2** The mapping, so the gateway's own machinery does the work: org ↔ **principal**
  (its `monthly_budget_usd` is the org budget, its usage is the org rollup), member key ↔
  **api_key** on it (`quota_usd` is the member's cap, revoke is per-member). The principal's
  address is derived from the org id, so nothing gateway-side is stored here. (`2e30be7`)
- [x] **17.3** `/admin/gateway/*` behind the existing `admin_org` gate + the console's "Gateway
  access" card: mint (revealed once), list, revoke, per-member cap, org budget, live spend. The
  admin connection is a field on `AuthState` resolved at boot — a seam, not a per-request env
  read. (`c9ac064`)
- [x] **17.t** `tests/against_the_gateway_keys.rs` (real router, stand-in gateway) +
  `scripts/slice21-org-keys-smoke.sh` in the gate: a member is refused 403 and never reaches the
  gateway, another org's key is 404 not 403, a listing never carries a secret. *(this commit)*
- [x] **17.v** Live, against the real gateway: budget set → key minted from the console → Claude
  Code answered on that key → $0.097560 rolled up under the org's principal → revoke → 401.
  Evidence: `docs/verification/one-identity/README.md`. *(this commit)*
- [x] **17.r** Peer review closed a real lockout in the gateway half: `upsert_principal`'s
  `ON CONFLICT` rewrote `role`, and that path only ever asks for `member` — so binding an org
  whose email collided with an existing admin's SILENTLY DEMOTED that operator (the admin gate
  wants an admin key *and* an admin principal). Role is now untouched on conflict, with a
  regression test; amounts too large for `numeric(14,6)` are a 400 rather than a swallowed 500;
  and the module doc no longer implies more than the code enforces — `require_admin_layer` is
  all-or-nothing, so the key handed to a partner service is a FULL gateway admin credential.
  Ours: the mint's non-idempotence and the revoke mirror are documented where they bite.
  *(this commit)*
- [ ] **17.later** Per-member model pins (slice 18), SSO/SCIM mapping onto the gateway's
  `oidc_subject` hook, self-service key rotation, an idempotency key on the mint, reconciling
  the console's key listing against the gateway (a failed revoke mirror leaves it reading
  stale), and per-key admin scopes in the gateway so a partner service's credential is not a
  full operator credential.

## Slice 18 — per-coworker model pins

A coworker's model was decided once, at hire, and could never change; every create path except
REST ignored a requested model and stored the deployment default. Investigation:
`plan-coworker-model-pins.md` (this pass corrected several of its claims — see below).

- [x] **18.1** `CoworkerCommand::Repin` / `CoworkerEvent::Repinned` — a pin is a decision that can
  be revisited, with the same `alive()` guard every command but `Hire` takes. And `Hire` finally
  validates: `decide(Hire)` was an unconditional `Ok`, so the `400` arms its three callers had
  written were unreachable and `model: ""` was stored and later asked of the gateway verbatim.
  Both commands now trim and refuse blank. *(this commit)*
- [x] **18.2** Every create path honours a pin — gateway `createAgent`, seam-B
  `CreateGrokBotAgent`, REST hire (blank now falls back too, not just absent), and
  `duplicateAgent`, which was silently re-hiring a deliberately-pinned bot on the default.
  `updateAgent` and a new `PATCH /coworkers/{id}` both issue `Repin`; ownership answers 404.
  *(this commit)*
- [x] **18.3** `GET /models` + `POST /models/probe`: the picker needs the routes this gateway
  advertises and only the deployment's key may ask, so the server asks and returns ids — the
  browser never touches the key (asserted in test and smoke). An empty catalogue is `[]` **with a
  reason**. `account_from_bearer` also accepts the console's cookie, without which a browser could
  reach none of it. *(this commit)*
- [x] **18.4** Console `/console/coworkers`: hire, list, repin, with the route as its own column
  and a **Test** button that reports the gateway's own words. *(this commit)*
- [x] **18.v** Live against the real gateway: catalogue proxied with no key in the reply; a
  coworker hired on `openai/gpt-5.5` answered a real turn; repinned to `oag/auto`, the **next**
  turn failed with the gateway's own sentence rather than quietly using the deployment model.
  Evidence: `docs/verification/model-pins/README.md`. *(this commit)*
- [x] **18.r** Peer review (two reviewer agents) closed three real ones: `update_agent` swallowed
  a REFUSED repin — folding `decide` into an `if let` chain made a rejection indistinguishable
  from "no model sent", so a caller asking to think with nothing got a 200 and no change (the
  sibling PATCH already answered 400); `probe` forwarded the gateway's error body verbatim while
  its neighbour `list` deliberately discards bodies precisely because one "could echo the request,
  and the request carried the key" — the sentence now travels scrubbed and clipped, with a test
  driving a gateway that echoes our key; and `POST /models/probe` was an unbounded real-money
  amplifier, now one probe per account per few seconds. *(this commit)*
- [x] **18.pin** A resumed run thinks with the pin its turn started on. Stored on
  `RunEvent::Started` (`#[serde(default)]` so old logs replay); `pin_for_resume` falls back to
  the current pin only when that field is absent. Gateway and AG-UI continue paths both honour
  it. *(this commit)*
- [ ] **18.later** Seam B's `UpdateGrokBotAgent` has no repin path. The roster's
  `description = model` habit (a blank-agent defence in the desktop
  client, not a statement of choice — the console shows the pin as its own field); the desktop
  app's own create/update model field + picker; `auto_review_model` is a second deployment model
  a pin deliberately does not move; per-coworker spend caps (the gateway has no per-day cap, and
  metering a coworker natively means giving each its own gateway key).

**Corrections this slice made to `plan-coworker-model-pins.md`** (kept because the doc is still
the reference): its recommended default `oag/auto` is **refused on this deployment** — advertised
in the catalogue, unservable on a route whose ladder has no matching credential, which is exactly
why the probe exists; `Hire` had no validation to "follow"; `duplicateAgent` was an unlisted
fourth create path; six sites read the pin (incl. autonomy and seam-B send), none cached, so
`Repin` needed no change at any of them.

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

- [ ] Commands: `goal`, `plan`, `review`. Parked: the packaged app's `sendPrompt` has no
  `mode` field and no Plan-mode picker (`docs/verification/plan-mode-wire/`). Honouring
  one here would invent a contract. A client composer control is the prerequisite.
  Auto-review (the consent judge) is a different product and already shipped.
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

- [x] GitHub Actions CI — resolved 1 Sep 2026 by the repo going public: the workflow now runs
  `scripts/gate.sh --smoke` itself, green. *(this commit)*
- [ ] **Rights review — now OVERDUE rather than blocking**: the operator published the repo on
  1 Sep 2026 with the review still outstanding (`LEGAL.md` status note).
- [ ] gpt-5.6-luna — upstream credits (`personal-team-blocked:spending-limit`); 5.5/5.4-mini
  verified working through the same gateway.
