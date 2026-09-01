# Handover — closing roadmap 9.v and 10.3, and what comes after

Written 2026-09-01. Everything below is verified against the live box unless marked ASSUMPTION.
Read `CLAUDE.md` (invariants) and `docs/ROADMAP.md` (the tracker) first.

**Update, same day:** 10.3 is closed. The recon below was right that `"the Bot replied"` is not
proof, and wrong that the likely miss was a missing `Bearer ` prefix — the vault already had
the prefix, but the JWT was minted for a previous account and did not verify, which is the
anonymous path. Rebound to a live Hexuria key; one channel send produced
`run_view.account_id = acct_01a0551e-…`. Evidence: `docs/verification/barok-bot-binding/`.
9.v is still blocked on the human.

---

## 1. Where the project actually is

**All PRs are merged. `main` is green.**

| PR | What | State |
|---|---|---|
| #1 | consent model | merged |
| #2 | docs overhaul | merged |
| #3 | Door 1 / MCP door | merged |
| #4 | Slice 17 / one identity | merged |
| #5 | **Slice 18 / per-coworker model pins** | **merged `14427ee`, CI green** |

Both repos (`hexuria/opengrok-server`, `hexuria/opengrok`) are **public**. CI is real and runs
`scripts/gate.sh --smoke` itself — it is no longer billing-blocked.

**Slice 18 landed these:** `CoworkerCommand::Repin`/`CoworkerEvent::Repinned`; all four create
paths honour a pin (gateway `createAgent`, seam-B `CreateGrokBotAgent`, REST hire,
`duplicateAgent`); `PATCH /coworkers/{id}`; `GET /models` + `POST /models/probe` (catalogue proxy
that keeps the gateway key server-side, rate-limited to 1 probe/account/3s, gateway error text
scrubbed of anything key-shaped); a console page at `/console/coworkers`.

**The load-bearing finding from slice 18:** an *advertised* catalogue id is not a *servable* one.
`oag/auto` is listed by the live gateway but REFUSED ("no credential available for provider
anthropic on this route"). Never trust a catalogue id without probing it.

---

## 2. The two boxes still open

### 9.v — "the client mints its own connection through us"

Switch the packaged desktop app into OpenGrok server mode, sign in at `/loginDeepControl`, and
watch `EnsureSandBox` hand back `OG_PUBLIC_GATEWAY_URL` + the bearer.

**STATUS: CLOSED 1 Sep 2026.** Throwaway account + throwaway app profile; live
`signin@acme.test` session untouched. `docs/verification/real-client/9v-mint.md`.

> **THE BLOCKER: nobody knows the password for `signin@acme.test`.**
> It is not in `.env` (which holds only `OG_*` names), not in either repo's docs, not in the
> session memory files. `POST /loginDeepControl` authenticates real credentials
> (`crates/opengrok-server/src/auth/routes.rs:210-249`) — there is no bypass, and adding one
> would be a security regression, not a shortcut.
>
> **What unblocks it, pick one:**
> 1. The operator types the password into the browser form when the flow opens it.
> 2. Reset it against the live DB and use the new one (an admin/CLI path — check
>    `crates/opengrok/src/main.rs` subcommands for an account command).
> 3. Sign up a *fresh* account through the console and run 9.v as that account instead. Cleanest
>    autonomous option — but note `OG_GATEWAY_EMAIL=signin@acme.test` is load-bearing for the
>    gateway bearer's identity, so a new account proves the client leg without disturbing it.

**Everything else about 9.v is already mapped and ready:**

- The client's whole path is real and shipped in the installed bundle. One 247-line module:
  `/Volumes/goldcoders/OSS/opengrok/source/electron-main/box/opengrok-signin.ts`.
  - `createLoginParams` → `/loginDeepControl?challenge=…&uuid=…&mode=login&redirectTarget=sand`,
    PKCE where `challenge = base64url(sha256(verifier))`.
  - `pollForOpenGrokToken` → `GET /auth/poll?uuid=&verifier=`, treats **404 as "not yet"**.
  - `mintOpenGrokGateway` → POSTs `{}` with the account JWT as bearer to
    `/aiserver.v1.GrokBotService/EnsureSandBox`, reads `gatewayUrl` + `gatewayToken` off the reply.
  - Driven by exactly ONE RPC: `window.desktop.agent.signInToOpenGrokServer(url)`.
- Server side: the mint is `crates/opengrok-server/src/seamb.rs:679-701`. It returns a fixed
  12-field envelope; only `gatewayUrl` (`OG_PUBLIC_GATEWAY_URL`), `gatewayToken`
  (`OG_GATEWAY_BEARER`) and `tenantId` are live values.
- Seam B accepts **only a signed access token or the console cookie**. A bot key returns 401 —
  do not try to shortcut the sign-in with one.

**Three traps that will cost you a session:**

1. **The mint LOGS NOTHING.** `seamb.rs:679-701` contains zero `tracing` calls, and the live
   server does **not** have `OG_TRACE_REQUESTS=1`. *"No EnsureSandBox in the log" is NOT evidence
   the mint never ran* — that mistake is already recorded once. To get server-side evidence you
   must restart with `OG_TRACE_REQUESTS=1`, and restart **before** the sign-in: the pending-login
   map is in-memory with a 5-minute TTL (`auth/routes.rs:39-50`), so a restart between
   `GET /loginDeepControl` and `/auth/poll` loses the challenge.
2. **`signInToOpenGrokServer` blocks up to 180s** waiting for a human in the browser.
   `cdp-eval-main.mjs` gives up at 20s — use `cdp-eval-long.mjs` or you will read a spurious
   timeout while the sign-in is actually succeeding. (Client poll deadline 3 min vs server TTL
   5 min — the client gives up first.)
3. **The app is ALREADY signed in and already in server mode**
   (`boxRuntime: "opengrok"`, `openGrokGatewayUrl: "http://192.168.100.24:1447"`). Re-running
   9.v as a *fresh* flow means signing out first, which clears both secrets AND
   `openGrokGatewayUrl` AND demotes `boxRuntime` to `local-docker`
   (`main-edge.ts:794-804`) — you must restore all three afterwards. Also: crossing into/out of
   opengrok mode deliberately logs the account out ("one account, never two", `main-edge.ts:546`).

**A doc bug to fix while you are here (free win, no live run needed):**
`docs/setup/environment.md:21` and `docs/ROADMAP.md:123` both claim the mint "is refused outright
when the address is loopback". **It is not.** The server refuses only an *empty* address
(`seamb.rs:680-687`). The loopback assertion lives in `scripts/slice13-seamb-smoke.sh`, not in the
server. Correct both docs.

### 10.3 — "proven from barok-works end to end"

One browser send from barok-works, carrying the bot-key header, landing on our live server as an
**owned** coworker run.

**STATUS: DOABLE AUTONOMOUSLY, but the whole thing hinges on one byte-level detail.**

> **THE BEARER PREFIX IS THE WHOLE BALLGAME.**
> barok sends the vault value **verbatim** — it never adds a `Bearer ` prefix
> (`server/src/agents/auth-header.ts:181-186`). OpenGrok does
> `.strip_prefix("Bearer ")` and, on failure, returns `Ok(None)` → **the run proceeds
> ANONYMOUSLY** (`crates/opengrok-server/src/agui/routes.rs:936-941`).
> So the stored credential value MUST literally begin with `Bearer `.

**Why "the Bot replied" is NOT evidence:**
- An anonymous AG-UI run is *deliberately allowed* — it just gets no tools
  (`agui/routes.rs:979-987`). It looks like success.
- A revoked or missing vault row makes barok send **no header at all**
  (`auth-header.ts:178-180`) — same invisible anonymous run.
- **The model won't discriminate either:** both live bot keys name `gpt-5.6-luna` coworkers and
  `OG_MODEL` is *also* `gpt-5.6-luna`. Identical answer whether identity bound or not.

**The ONLY proof:** `run_view.account_id` non-null on a run whose thread came from barok.
Note `run_view` has **no `coworker_id` column** — the link is
`events.payload->>'coworker_id'` where `event_type='run-started'`.
Discriminate barok's run from the desktop's by thread id shape (desktop uses `gateway-<coworker>`).

**barok-works facts (corrects the roadmap's assumptions):**
- It is a Bun/TS monorepo, a **git worktree of `/Volumes/goldcoders/OSS/openbot`**, slot 2.
- **It does NOT run on :3030.** `.env.local` overrides: app **3010**, server **3021**,
  grok-runtime 4310, postgres 5462.
- **DO NOT run `just wt-up`** — it rewrites `APP_PORT` to 3030 but leaves `TRUSTED_ORIGINS` at
  `http://localhost:3010` (`TRUSTED_ORIGINS` is not in `SLOT_KEYS`,
  `scripts/worktree-stack.sh:114`). That origin mismatch is what broke the previous run.
  **Use `bash scripts/start.sh` (`just start`)**, which honours `.env.local` as-is.
- The stack is currently **down** (its postgres container exited); the data volume
  `barok-works_postgres-data` survives, so the Bot rows and vault row are still on disk.
- `OPENBOT_SINGLE_USER=true` and no identity provider → every request is `dev@barok.works`
  (administrator). Plain unauthenticated curl against its API works.
- **DO NOT regenerate `KEY_ENCRYPTION_KEY`** (currently the public example key; the server only
  warns). Changing it makes the stored bot key undecryptable and barok then silently sends nothing.

**The stale-duplicate Bot — the thing that killed the last attempt:**
- OpenGrok was registered **twice**: a tenant-package Bot id `opengrok` (from
  `examples/fintech/agents.yaml`, since reverted) and a real one via `POST /api/agents` as
  `agent_d8432b1e-44f6-4ed8-acbc-864b0cd9ce98`. Both point at `http://127.0.0.1:1447/ag-ui`.
- **The package sync never prunes** (`tenant-package.ts:587-594` says so outright). Reverting the
  yaml stopped it being *recreated*; it did not delete it.
- The stale one is `systemOwned` and **cannot be patched or deleted** —
  `requireManageable` throws `ProtectedAgentError` (`profile-store.ts:227-232`). Only
  `POST /api/agents/:id/hide` works on it.
- **Tell them apart by `hasAuth`, not by name or avatar.** Both are called "OpenGrok". The package
  row structurally cannot hold a key, so `hasAuth: true` identifies the real one.

**The runbook for 10.3:**
```sh
# 1. Bring barok up (NOT `just wt-up`)
cd /Volumes/goldcoders/OSS/barok-works && bash scripts/start.sh && just health

# 2. Identify the RIGHT Bot — hasAuth is the discriminator
curl -sS 'http://127.0.0.1:3021/api/agents?hidden=true' | python3 -c "import json,sys;[print(a['id'],'|',a['name'],'|',a['endpoint'],'| hasAuth=',a.get('hasAuth'),'| systemOwned=',a.get('systemOwned')) for a in json.load(sys.stdin)]"

# 3. Hide the stale package Bot so it cannot be picked by accident
curl -sS -X POST http://127.0.0.1:3021/api/agents/opengrok/hide -o /dev/null -w '%{http_code}\n'

# 4. Watermark OpenGrok's DB BEFORE the send
docker exec opengrok-postgres psql -U oag -d opengrok_web_verify -c \
  "select count(*) runs_before, max(updated_at_ms) watermark from run_view;"

# 5. Make ONE send from the browser at http://localhost:3010 to the hasAuth=true Bot

# 6. THE PROOF — account_id must be NON-NULL on the new run
docker exec opengrok-postgres psql -U oag -d opengrok_web_verify -c \
  "select id, account_id, thread_id, updated_at_ms from run_view order by updated_at_ms desc limit 3;"
docker exec opengrok-postgres psql -U oag -d opengrok_web_verify -c \
  "select payload->>'coworker_id' cw, occurred_at_ms from events where event_type='run-started' order by occurred_at_ms desc limit 3;"
```
If `account_id` is null → the header did not bind. Check the vault value literally starts with
`Bearer `. **That is the single most likely failure.**

---

## 3. The live box, exactly as it stands

- **Server:** pid 40759, `./target/debug/opengrok`, `OG_BIND=0.0.0.0:1447`,
  `OG_MODEL_DOOR=gateway`, `OG_MODEL=gpt-5.6-luna`,
  `OG_PUBLIC_GATEWAY_URL=http://192.168.100.24:1447`, `OG_GATEWAY_EMAIL=signin@acme.test`.
  **`OG_TRACE_REQUESTS` is NOT set.** Log: `/private/tmp/opengrok-serve.log`.
- **LAN address `192.168.100.24`** (DHCP moved it from `.21` once already — when the roster looks
  dead in server mode, check `openGrokGatewayUrl` against `ipconfig getifaddr en0` FIRST).
- **Live DB:** `postgres://oag:oag@127.0.0.1:5455/opengrok_web_verify`, container
  `opengrok-postgres` (has a volume, survives reboots). 30 tables; the ones that matter:
  `account_view`, `coworker_view`, `bot_key_view`, `run_view`, `gateway_entry`, `events`.
  - `bot_key_view` is keyed by **`jti`** and has **no `id` column** —
    columns are `jti, account_id, coworker_id, label, revoked, created_at_ms`.
    Both live keys carry the identical hardcoded label `'bot key'` (`routes.rs:844`) — cite the jti.
  - One account: `signin@acme.test`, `acct_01a0551e-29ad-74b3-b1d6-236a8122d6d8`, org Acme.
  - 19 coworkers, **two live**: `Hexuria` and `DoorProbe` (both `gpt-5.6-luna`), one unrevoked
    bot key each. **No stale duplicates remain on the OpenGrok side.**
  - Newest activity `2026-09-01 00:55Z` — anything newer is unambiguously yours.
- **Gate DB (separate!):** `oag-dev-postgres-1` on `:5452`. Gate command:
  `OG_PORT=1449 OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate scripts/gate.sh --smoke`
  (`OG_PORT=1449` protects the live `:1447`; run `cargo build -p opengrok` first — the gate never
  rebuilds). Fixed DBs `opengrok_s17_gate`/`s18`/`s19`/`s21` must exist.
- **Desktop app:** CDP **is** available on `:9223`. Tools:
  `/Volumes/goldcoders/OSS/opengrok/docs/research/tools/cdp-eval-main.mjs` (20s cap) and
  **`cdp-eval-long.mjs`** (280s — use this for sign-in).
- `127.0.0.1:8080` is the opencodex bun stub that backs the `openai` credential.

---

## 4. What comes after these two boxes

In the order I'd take them:

1. **The `goal` / `plan` / `review` commands** — parked. The harness does **not** already
   have these behaviours (`review.rs` is the auto-review judge). The packaged app's
   `sendPrompt` has no `mode` and no Plan-mode picker
   (`docs/verification/plan-mode-wire/`). A client composer control is the prerequisite;
   honouring a field we invented would break CLAUDE.md #1.
2. **Deferred hardening**, all deliberately parked, none urgent:
   - `18.later` — a run resumed after an approval picks up the coworker's *current* pin, not the
     one its turn started on (straight-through turns ARE stable). Fixing it means storing the pin
     on the suspension. Also: seam B has no repin path; the roster's `description = model` habit.
   - `16.later` — card-driven MCP approvals.
   - `12.later` — DNS ownership proof for domains.
   - `17.later` — SSO/SCIM onto the gateway's org mapping.
3. **Also on the Later list:** artifacts/uploads, mem0, channels/multi-party rooms, stdio MCP
   servers inside a coworker's container, graph harness (`MAX_ROUNDS = 8` today), Redis (only
   after a measured hot query — standing decision).

**Blocked on the operator, not on code — and now OVERDUE:** the rights review. The repo was made
public on 1 Sep 2026 with it still outstanding. `docs/LEGAL.md`. The no-vendored-protobuf-stubs
rule is *harder* now, not softer.

---

## 5. House rules you must not drift on

- **Evidence or it doesn't ship.** "200 accepted" is not "honoured". Provider-behaviour claims
  need a captured response; client-behaviour claims need a file path.
- **Fail closed and say why.** A refusal reaches the model as a *result*, not an exception.
- **Never overturn a recorded decision by drift.** If code carries a written rationale, raise it —
  don't silently "fix" it.
- **The client contract is transcribed, never invented.** Shapes in `opengrok-wire` carry a
  provenance comment naming the file they were read from.
- **Secrets:** `.env` is mode 600 — print names, never values. Never echo an `oag_live_` key. The
  repo has a pre-commit secret scanner that trips on key-*shaped* literals even in tests — build
  test fixtures with `format!` rather than writing them out.
- Commit subjects are lowercase sentences; the body explains **why**. Comments explain
  constraints, never what the next line does.
- **The workflow, no stage skipped:** build in compiling steps → test with each step →
  `scripts/gate.sh --smoke` green locally AND in CI → PR with captured evidence → peer review by
  independent reviewer agents, every finding answered with a fix commit or evidence → validate
  live → only then report.
