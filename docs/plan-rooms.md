# Rooms — a plan for review, before any code

Status: **draft for review, 2 Sep 2026.** Written from the recovered client bundle and the server
as they are today; nothing here is built. Step 3 of the tranche that closed the hardening notes
(#27, #30, #31) and shipped spend caps (#32). Reviewed by the peer session and the operator before
a slice is cut from it; the operator's questions are at the end.

**The ask** (`ROADMAP.md` Later: "Channels / multi-party rooms (phases 3–4 of
`archive/plan-bots-computers-channels.md`)"): let coworkers talk to each other, and to more than one
person, in one place.

---

## 0. What is true today

**"Rooms" in the client is two features, not one.** Reading the recovered bundle, the ten sharing
verbs and the two group verbs sit in different subsystems with different backends:

| | Local groups | Shared rooms ("multiplayer") |
|---|---|---|
| Verbs | `createGroup {name, description, memberAgentIds}` → `{agent, transcript}`; `setGroupMembers {id, memberAgentIds}` → summary or `null` (`research/client-grok-bot.md` §D rows 30–31) | the ten in §F: `getSharingState`, `createRoomFromAgent`, `createRoomInvite`, `joinSharedRoom`, `respondToRoomJoinRequest`, `createSharedRoom`, `addOwnAgentToSharedRoom`, `removeOwnAgentFromSharedRoom`, `setSharedRoomTyping`, `leaveSharedRoom` |
| What it is | **An agent that is a group.** The host creates an ordinary agent and writes a group config beside it (`{version, memberIds, remoteMembers?, sharedRoomId?}`, `group-chat-glue.ts:90-155`). The roster row carries `isGroup: true`, `memberIds` (`session-summaries.ts:16`); its transcript is the group's chat. A prompt to it runs the **group orchestrator** (below). | **One user's agents in another user's room**, relayed through the vendor backend: `POST /sand/share-rooms/*`, `/sand/xuser/poll`, `/sand/xuser/send`, `/sand/share-state` (`cross-user-sharing/*.ts`), behind the `sand_multiplayer` feature gate (default **off**, "evaluated on the backend for every sharing endpoint, fail closed"), with typing presence, invite links, join requests, tombstones and a relay queue. |
| Who is in it | This user's own agents, no people other than the owner | Several *accounts* (`RoomMember {kind, authId, agentId?, displayName?}`), each bringing agents; a `hostAuthId` |
| Server today | `createGroup`/`setGroupMembers` → `400 "groups are not supported by this server yet"` (`gateway/routes.rs:505`) | honest empties: `getSharingState` → `{rooms: [], invites: [], requests: []}` (`routes.rs:586`); the other nine are unrouted |

**The group orchestrator is a fixed, transcribable policy** (`group-chat-orchestrator.ts`): up to
`GROUP_MAX_ROUNDS` rounds; each round the responders are resolved from the history
(`resolveResponders`), ordered (`orderRoundSpeakers`, rotating by round), and each speaks once
with a system prompt built from its own profile, the group and its peers, and a prompt built from
the messages since it last spoke; a member may say "pass" (skipped); caps on messages per turn
and total member turns; the round ends when nobody spoke; a member's turn uses the member's own
model, tools and computer (`runGroupMemberTurn`, `createGroupMemberRunner`). While a member is
speaking the group row carries `activeRemoteMemberId` (`run-lifecycle.ts:312`). Every member's
turn is a normal agent turn — which is exactly what our harness already runs.

**Three server facts the design leans on.** A coworker is an aggregate with a model, a computer
and a policy, and a run is durable and resumable (CLAUDE.md #5). The desktop transcript is a
wire-format projection per coworker (`store/gateway.rs`), and the roster is stamped and emitted
per account (`gateway/live.rs`). The `sand_multiplayer` gate is not something we serve: the
client has no experiments verb against us (`host-gateway-api.ts:384` only asks
`isAgentNetworkEnabled`), so the gate stays at its baked default, off — which means the ten
sharing verbs never fire against this server today, and `getSharingState` is the only one the
boot calls.

**One shape mismatch to fix regardless.** Our `getSharingState` answers `{rooms, invites,
requests}`; the client's `EMPTY_SAND_SHARING_STATE` is `{isEnabled: false, selfAuthId: null,
pendingJoinRequests: [], rooms: [], typingUsers: []}` (`shared/agents/sharing.ts:43`). The
bridge only `requireState`s a record, so nothing breaks, but the transcribed shape is the second
one and the first is invented. Fix in the first slice below.

## 1. The decision: groups first, shared rooms not yet

**Build local groups on the server; leave shared rooms parked, and say so honestly.**

- Groups are one aggregate and one orchestrator over machinery we have. They deliver "coworkers
  talk to each other" for one person's team, which is the product's own story (a team of AI
  coworkers), and they are the base the client's shared rooms are built ON: a shared room is a
  group whose config has `sharedRoomId` and `remoteMembers`.
- Shared rooms are a cross-account relay with its own protocol (twelve backend endpoints,
  presence, invites, tombstones, dedupe stores) gated off by default and not reachable from
  this server at all until we serve the gate. That is a slice of its own, after groups, and only
  if the operator wants cross-account rooms at all — orgs already put several people on one
  server, which may be the multi-person story that matters here.

## 2. Groups on the server

### 2.1 What a group is

**A group is a coworker with members** — not a new aggregate. Reasons: the client already treats it
as an agent (`{agent, transcript}` on create, a roster row, a transcript); every per-coworker
surface we have (transcript projection, roster emit, policy, bot keys, spend cap, retirement)
applies unchanged; and `deleteAgents` retires it like any other.

`opengrok-core::coworker`:

```rust
pub struct Coworker { …, pub members: Vec<CoworkerId> }      // empty ⇒ not a group
CoworkerCommand::HireGroup { name, description, members, at_ms }
CoworkerCommand::SetMembers { members, at_ms }
CoworkerEvent::GroupHired { … }, CoworkerEvent::MembersSet { members, at_ms }
```

Rules the aggregate enforces (transcribed from `createGroup`): members are de-duplicated; a
group cannot be a member of a group (`assertMembersAreNotGroups`); at most `GROUP_MAX_MEMBERS`
(read the constant from the bundle — do not guess); at least one member; a second `createGroup`
with the same member set returns the **existing** group (idempotent by member set, the client's
own rule). `CoworkerView` gains `members: Vec<CoworkerId>`; the roster row emits `isGroup`,
`memberIds` (already in the transcribed row shape, today always `false`/`[]`).

A group has **no computer and no model of its own**: its members think. `kind_for_new` /
`ensure_computer_for` skip groups; `run_turn` on a group dispatches to the orchestrator instead.

### 2.2 The turn

`gateway/group.rs` — the orchestrator, transcribed from `group-chat-orchestrator.ts`, as one
durable run on the **group's** thread (`gateway-{group}`), so a crash mid-round resumes:

1. Resolve members (retired ones drop out, as the client filters non-existent ids).
2. For each round up to the cap: pick responders from the group history, order them by round,
   and for each run **one member turn** = `run_turn` with the member's own model, computer, tools
   and policy (`ToolContext::from_coworker(member)`), the group system prompt
   (`buildGroupMemberSystemPrompt`: the member's profile, the group, the peers) and the
   since-you-last-spoke prompt (`buildGroupTurnPrompt`). The member's reply is appended to the
   **group's** transcript as a message from that member (the client renders member messages by
   `authorId`/name — transcribe the entry shape from `postGroupMemberMessage` before writing it),
   streamed as today's deltas; "pass" is dropped.
3. `activeRemoteMemberId` on the group's roster row while a member speaks; cleared after.
4. Stop when a round produces nothing, or the caps hit.

Policy and cards: a member's tool call inside a group turn is the member's — its policy, its
card, its spend cap (the member's own gateway key, #32). A card raised inside a group suspends
the group's run the way a coworker's run suspends today; the resume continues the round.

### 2.3 Wire

- `createGroup` → `HireGroup`, reply `{agent, transcript}` exactly as `createAgent` does
  (`lifecycle::create_agent` shape); idempotent by member set.
- `setGroupMembers` → `SetMembers`, reply the summary or `null` when the id is not a group of this
  account's.
- `sendPrompt` to a group id → the orchestrator; `getAgentTranscriptTail` unchanged.
- Seam B: no group verbs exist there (`GrokBotService` has none) — nothing to add.
- `getSharingState` → the transcribed empty state (§0), unchanged in meaning.

### 2.4 Tests and evidence

- Aggregate unit tests: de-dupe, no group in a group, cap, at least one, same-set idempotency,
  retire drops membership.
- `tests/against_groups.rs` through the real router with the mock door scripted per member: a
  group of two, one prompt, both members speak in round one and in the transcribed order, the
  second round is empty and the run finishes; a member that "passes" is silent; a member's Ask
  suspends the group run and the resume finishes the round; `setGroupMembers` from another
  account is `null`; `deleteAgents` on a group retires it and leaves the members.
- A smoke on the packaged app: create a group from two coworkers in the sidebar, send one prompt,
  watch both answer; the evidence folder `docs/verification/groups/`.

## 3. Shared rooms — parked, with the shape recorded

If the operator wants cross-account rooms later, the work is: serve the `sand_multiplayer` gate
(an experiments surface we do not have — transcribe the client's `SandExperimentService`
contract first), then the twelve `/sand/share-rooms/*` + `/sand/xuser/*` endpoints as the vendor
backend implements them (`handleSandShareEndpoints.ts` is named in the client's own comment as
the reference — we do not have it; the client side is the only transcription source), the relay
queue with `/sand/notify` frames, presence with TTL, invite links, join requests, tombstones. A
group with `sharedRoomId` + `remoteMembers` is the client-side representation, so §2 is a
prerequisite, not an alternative. Estimate: a slice larger than groups, with the relay as the
risky part; none of it is worth starting before §2 has been used.

## 4. Order and size

1. Fix `getSharingState`'s shape (an afternoon, its own PR — a transcription correction).
2. Groups: aggregate + wire (a day), orchestrator + tests (two days), packaged-app evidence.
3. Shared rooms: decide, then plan again with the gate contract transcribed.

## 5. Questions for the operator

1. Groups first and shared rooms parked, as above — agreed?
2. A group is a coworker with members (not a new aggregate) — agreed?
3. Does a group get its own spend cap, or do member turns spend on each member's key only
   (the proposal: members only; a group has no key)?
4. Cross-account rooms: wanted at all, given orgs already share one server?
