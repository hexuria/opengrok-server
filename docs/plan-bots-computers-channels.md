# Plan — Bots, their Computers, and Channels

**Status:** proposed. Spans the server (`opengrok-server`, mine) and the desktop client
(`opengrok`, the peer's — frontend work relayed to that session).
**Date:** 2026-08-30.

## Goal (one sentence)

A signed-in person creates a bot; on creation it is given its own computer (a box.ascii.dev box
or a local Docker container, per the account's choice); a channel is created that the person can
join and chat in; messages sent in the channel are answered by the bot on its own computer.

## Decisions locked with the user

- **Box API key: per-account, entered in the app, stored encrypted on the server** (reuse the
  connector vault / `OG_CREDENTIAL_KEK`). Each account provisions with its own key.
- **Channel: a real multi-party room** — membership (people + bots), join/leave, its own message
  log, membership-scoped delivery. (Reopens the deliberately-deferred P11 rooms.)
- **Bot scope: full management** — create / rename / set-model / retire / list, wired to the UI.

## The one architectural pivot everything rests on

The gateway today resolves a **single fixed host account** (`state.email`) for every request, even
though the store is already per-account (`coworkers_for(account_id)`, `account_id` on every
coworker row). Per-account boxes and multi-party channels require the gateway to key off the
**authenticated caller's account** instead. Because the store is already scoped, this is a
contained change (resolve `account_id` from the caller's OpenGrok account token, thread it through
the conversation/roster/box code) — not a rewrite. This pivot lands in Phase 2 and unblocks 3–4.

## Phases (each slice: code + tests same commit, gate green, then commit)

### Phase 1 — Provision on creation, and real teardown (server only; no pivot needed)
1. **Shared provisioning helper.** Factor the REST hire's provisioning block
   (`agui/routes.rs:386-418`: `computer.create()` → `AssignComputer` → persist, `computerError`
   non-fatal) into one helper. Wire it into `gateway::lifecycle::create_agent`
   (`lifecycle.rs:117`) and seam-B `CreateGrokBotAgent` (`seamb.rs:258`) so the **desktop's**
   create path provisions a box. Add a `withComputer` (+ optional `boxMode`) intent to
   `createAgent`; report a failed box in the reply the way REST does.
2. **Real teardown.** A gateway/REST verb to release a bot's box: `Retire` → `ReleaseComputer` →
   actually call `computer.stop()`/`destroy()`. Fixes the billing leak (ascii) and dangling
   containers (docker). Make the P10 box-control verbs (`ensureForeverBox`/`resetForeverBox`/
   `handBackForeverBox`, `gateway/routes.rs:531-555`) real where they map to provider calls.
3. **Pin the two `AsciiBoxes` wire unknowns** (`ascii.rs:16` create-reply id field; `:356` DELETE
   confirm header) against a real box — needs the operator's box.ascii.dev key; flagged as a
   verify-with-operator step, Docker path stays fully testable without it.
- Tests: extend `slice6-computer-smoke.sh` to cover the gateway create path provisioning + teardown
  (docker door); unit tests for the helper. Gate green.

### Phase 2 — Per-account gateway + per-account box credentials (server + client relay)
1. **Gateway resolves the caller's account.** Thread `account_id` from the authenticated caller
   (the OpenGrok account token minted at sign-in) through `conversation.rs` / `live.rs` roster and
   send/read, replacing the fixed `state.email`. Bots/transcripts already keyed by `account_id` in
   the store — this scopes them to the real caller.
2. **Per-account box credential store.** New encrypted store (reuse `Vault`) keyed by account:
   set / clear / status. Endpoints: `POST /account/box-credential` (+ status/clear). When
   provisioning for an account, build a per-account `AsciiBoxes` from that account's key rather
   than the server-wide `AgUiState.computer`; Docker/none stay as the fallback.
3. **Client relay (peer):** the "Computer" settings tab gains a box.ascii.dev key input (the
   existing secrets-upsert/Gemini-key pattern) that sends the key to the server over the account
   session; selecting `box` vs `local-docker` sets the account's provisioning mode.
- Tests: box-credential store unit + smoke (set key → provision uses it → status/clear); the
  per-account scoping proven by two accounts not seeing each other's bots. Gate green.

### Phase 3 — Channels (the net-new multi-party surface; server)
1. **Channel aggregate** (`opengrok-core`): id, name, org/owner, members (accounts + bots),
   message log. Commands/events: `CreateChannel`, `AddMember`/`RemoveMember` (person or bot),
   `PostMessage`. Store: append + projection (`channel_view`, `channel_member`), reads
   (`channels_for(account)`, `channel_messages`).
2. **Channel messaging + bot replies.** `POST` a message to a channel; if a bot is a member, spawn
   the harness turn (reuse `run_turn`) with the channel's history and the bot's own computer/tools,
   streaming the reply back **into the channel** log. Membership-scoped SSE fan-out (a new
   `channel` event surface that only members receive).
3. **Join/leave + auth.** Only members may post/read; join creates membership; the SSE bus filters
   frames by channel membership.
- Tests: channel aggregate unit tests (membership guards, single-post idempotency); a
  `sliceNN-channels-smoke.sh` (create channel → add a bot → post → bot replies into the channel →
  a second account not a member is refused). Gate green.

### Phase 4 — Full bot + channel management UI (client relay to peer)
- The backend CRUD (create/rename/set-model/retire/list) and channel verbs from Phases 1–3 wired
  into the desktop: a bot manager (create with computer choice, rename, retire) and a channel view
  (create, add bot, join, chat). Server endpoints exist by then; this is client work relayed to
  the peer, verified against the running server.

## Out of scope (named)
- Real region/image/size selection for box.ascii.dev (API-key-only today); per-bot machine specs.
- Cross-org channels; federation. Voice/teach/memories (P11's other parked pieces).
- Web-console UI for bots/channels (the desktop is the client here; the console stays account/admin).

## Invariants
- One tested slice at a time; simplest thing that works. `unsafe_code` forbidden; no
  unwrap/expect/panic outside tests. Provider keys never touch client payloads/logs (identity args
  overwritten, never echoed). `.env` stays gitignored. Existing tests/smokes keep passing.
- Boxes must be destroyable — no slice ships a create path without its teardown.
