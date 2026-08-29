# Lessons from OpenSesame

What the TypeScript product learned the hard way, so the Rust one does not learn it again.

**Audience.** A fresh engineer on OpenGrok with no history. You do not need to have seen
OpenSesame. Every claim below names the file or the commit it came from, in
`/Volumes/goldcoders/projects/opensesame/opensesame`.

**What OpenSesame is.** A TypeScript/Bun product built on CopilotKit's OpenBot substrate:
a React app (`app/`), a Hono API server (`server/`), Postgres, and AI "coworkers" that
live in channels alongside people. It was built in about a dozen commits, each of which
records the failure that motivated it — `git log --format=%B` is the real design document
and is worth reading in full.

**What OpenGrok is.** A Rust replacement for the *server half*: the agent harness, tools,
computers and delivery move out of the browser onto a server. OpenSesame's web app may
survive as one client among several. Everything below is written for that transition.

Read lesson 8 first if you only read one.

---

## 1. Threads are per (person, channel, coworker)

**What we tried.** A channel is a room; a room has several coworkers; so a channel gets
one durable thread and whichever coworker you address answers on it. The UI even shipped
a "Who answers" selector.

**What broke.** The hosted state platform (CopilotKit Intelligence) binds a thread
**permanently to the first agent that owns it**. A turn sent under any other agent is
refused with `409 THREAD_AGENT_MISMATCH` — and the client library renders that as the
completely misleading string **"Thread … is locked"**, which reads like transient
contention and is not. Two live rooms "wedged" exactly this way: adding a coworker
re-sorted `agentIds`, so `agentIds[0]` stopped being the thread's owner and every
subsequent turn knocked on the wrong door and got a lock error forever.

The first fix (commit `e3fe3ac`, *"Bind each channel to its thread's owner; drop the
responder selector"*) was a retreat: derive the runtime agent from
`lastMessageAgentId` — the owner in practice — and **delete the selector**, because it
offered a choice the platform then refused. A bounded retry for genuinely transient locks
stayed (`app/src/components/channels/channel-chat.tsx`, the `/is locked/i` branch, 3
attempts at 900ms × attempt), which is the right shape but was papering over a permanent
error with a transient-error remedy.

The real fix (commit `ce01e4a`, *"One thread per coworker per room: who answers is now a
real control"*) re-keyed the mapping table.

**Where the code is.**

- `server/drizzle/0026_per_coworker_threads.sql` — adds `agent_id` to
  `intelligence_channel_mappings`, backfills each existing row to its thread's *actual*
  owner (`channels.last_message_agent_id` when that coworker is still in the room, else
  `min(channel_agents.agent_id)` — the pre-rooms binding), deletes mappings into rooms
  with no coworkers, then swaps the primary key from `(user_id, channel_id)` to
  `(user_id, channel_id, agent_id)` and adds the FK to `agents`.
- `server/src/db/schema/core.ts` — the schema side.
- `server/src/channels/routes.ts` — `ChannelStore.get` (~line 580) LEFT-joins the mapping
  and **heals missing rows by minting on read**, with `onConflictDoNothing` plus a re-read
  so two tabs healing at once converge on one thread instead of racing to two.
  `AgentChannel.threadId` became `threads: Record<agentId, threadId>`.
- `server/src/channels/thread-identity.ts` — thread ids are minted, not requested:
  a UUIDv8 whose first six bytes are a digest of `DEPLOYMENT_ID`, so a deployment can ask
  "is this thread mine?" about a thread it has never seen. Needed because the platform
  offers no field for a deployment to stamp, and one Intelligence project can serve a prod
  and a dev copy at once.
- `ARCHITECTURE.md` § *Known limits* states the invariant in prose.

> **The rule.** **A transcript is owned by exactly one (person, channel, agent) triple,
> and that ownership is permanent.** Design the key that way from the first migration, not
> after the platform refuses your second speaker. Corollaries:
> - Never derive the responder from a *positional* fact (`agentIds[0]`). Roster changes
>   re-sort it and the binding silently moves.
> - Mint-on-read healing beats mint-on-write: a coworker added to a room is speakable the
>   moment the room is opened, and the invariant lives in one place.
> - When you own the store (OpenGrok does), you get to choose. A channel-owned transcript
>   with per-message authorship is strictly better than N per-agent threads — see lessons
>   3 and 6, both of which are *entirely* consequences of the per-agent constraint. **Do
>   not port the constraint. Port the lesson that the constraint costs you a weave, a
>   collapse rule and a fan-out walk.**

---

## 2. The woven room timeline

**What we tried.** Show the whole room. Interleave every coworker's exchanges into one
transcript, in the order they happened.

**What broke.** *There is no order.* `GET /threads/:id/messages` returns
`{id, role, content}` — **no timestamps**. "Which of these two came first" is not a
question either thread can answer. Before this, the room showed one conversation at a time
(the responder's thread), so an answer from the coworker you mentioned two turns ago was
on a screen you had to navigate back to.

**The fix** (commit `8607a05`, *"The room weaves every coworker's exchanges into one
transcript"*): the room keeps its own order.

**Where the code is.**

- `server/drizzle/0028_channel_timeline.sql` — `channel_timeline`, PK
  `(channel_id, user_id, message_id)`, plus `agent_id` (which **thread** the message lives
  in, not who said it), `role`, and `at timestamptz default now()`. Index on
  `(channel_id, user_id, at)`. **An index, not a copy — no message text.** A room that
  loses this loses its ordering, not its conversation.
- `server/src/channels/routes.ts` — `POST /activity` gains an optional `entries[]`; the
  rows are written **in the same transaction as the roster preview and BEFORE it**,
  because a report that arrives too late to move `last_message` still knows where its
  message goes ("a stale preview is a stale preview; a missing timeline row is a message
  that reads as though it was never said"). `onConflictDoNothing` makes replay idempotent.
  Entries reported together share one clock read and are offset by their index
  (`now() + place * interval '1 microsecond'`) so a question never ties with its answer.
  Every agent id named in the report is checked to be in the room first — otherwise the FK
  turns a 404 into a constraint violation.
- `GET /:channelId/timeline` (routes.ts ~1728) — member-guarded like `/people`, reads
  `order by at desc limit 500` and **reverses in code**, because `asc limit 500` would
  answer with the first 500 things ever said in the room and call it the room.
- `app/src/lib/channels/timeline.ts` — `weaveRoom`, pure, plus the two query options.
- `app/tests/weave-room.test.ts` — the rules as tests, provable with no room, socket or
  running agent.

**The weave rules, in the order they are applied:**

1. **Anything the index names is drawn in index order.** The ordinary case.
2. **Unnamed, with nothing named before it in its own thread** = history from before the
   index existed. Nothing knows where it goes, so it is **not** interleaved: grouped per
   coworker in stored order, roster order between groups, placed *ahead* of everything the
   index can place. An old room reads as "each coworker's earlier history, then the woven
   part" — degraded and honest, rather than an invented order.
3. **Unnamed, but with a named message before it in the same thread** rides along with
   that message, drawn immediately after it. Only the *last* message of a reply is
   reported, so the tool calls that produced it are unnamed; stranding them at the end
   would separate the browsing from the sentence it produced.
4. **The live coworker's messages the store has never seen** are the turn in flight and go
   last, appended as they stream.

**Live vs stored reconciliation — the subtle one.**

> **The live copy wins on CONTENT; the stored copy wins on ORDER.**

The message store and the gateway hand back **different orders for the same thread**. This
was found against a live room, not reasoned about. Taking the live order for the live
coworker and the stored order for everyone else put an exchange from this morning at the
bottom of the conversation. So: iterate the stored order, substitute the live copy by id
(the live one is being written to as the answer streams, and the store's copy of the same
id can be a fragment), and append only the live ids the store has never seen. A `settled`
set — not position — distinguishes "still being said" from "old and never reported",
because a coworker that has never been reported has no named message to be "after".

**Two operational notes that are easy to lose.**

- Both room reads are keyed on the roster's `lastMessageAt`, which the socket keeps live.
  Freshness is *in the query key*, so "has anything been said since" is a cache miss rather
  than a decision the module makes — no polling, and a turn taken in another tab refetches.
- Both use `placeholderData: keepPreviousData`, and this is **not** a nicety. Without it
  the room has no order and no history for the second or two the refetch takes — which is
  exactly the second somebody is watching a reply arrive. The transcript collapsed to the
  live thread and came back.

> **The rule.** **If you interleave, you must own the clock.** Never assume a message store
> orders across streams; most do not even timestamp. Keep a small append-only index
> `(room, member, message) → (thread, role, at)` written in the same transaction as the
> turn, before anything that may be dropped as stale. Make the weave a **pure function**
> with the index and the histories as inputs — every rule above is testable with no
> infrastructure. And **degrade visibly**: unplaceable history gets grouped and labelled,
> never invented into an order.
>
> For OpenGrok: since the server owns the transcript, timestamps and a monotonic sequence
> number are yours for free. Keep the index anyway as the *ordering* authority (it is the
> thing that survives a store swap), but the whole weave/degrade path collapses to an
> `ORDER BY seq`. Budget the saved complexity elsewhere.

---

## 3. The fan-out collapse

**What we tried.** `@Bots` / `@All` — ask every coworker in the room the same thing.

**What broke.** A broadcast is N sends, because a thread belongs to one agent (lesson 1).
Per-thread views never showed that. The **woven** room drew the person's message **five
times**, each copy introducing its coworker's answer. It read as a stutter.

**Where the code is.** `collapseFanout` at the bottom of
`app/src/lib/channels/timeline.ts`, applied inside `weaveRoom` to the assembled list
(commit `d0e85f2`, *"One ask, one bubble: the weave collapses a fan-out's copies"*).

**The collapse rule, and its deliberate exceptions:**

- A user message **folds into the previous user message** when the trimmed text matches
  **and it lives in a different coworker's thread**.
- **Exception — same thread keeps both.** Sending the same words twice to the same
  coworker is a *retry*, and hiding a retry rewrites what happened.
- **Exception — any different user text in between starts a new ask.** So two separate
  walks with the same words stay two.
- Empty text never folds.
- Applies to history as well as live turns: the room that stuttered reads clean **with no
  migration**. This is why the rule lives in the pure render path and not in the writer.

> **The rule.** **One ask, one bubble — however many coworkers it went to.** Fan-out is a
> delivery detail; the transcript is a record of what a person did. Collapse on
> (identical text) × (different recipient) × (adjacent), and preserve retries and repeated
> asks as distinct events. Put the rule in the *read* path so it repairs history for free.
>
> For OpenGrok this is cheaper still: a broadcast should be **one stored user message with
> N deliveries**, and then there is nothing to collapse. Model the ask and the delivery
> separately — `og-store`'s "fan-out ledger" in the plan is exactly the right instinct.

---

## 4. The bug that motivated the pivot

**What we tried.** `@Bots` walks the room: the current responder answers first, then the
rest are asked one at a time (commit `eaa787b`, *"Latch the room's responder; name who
replied; @Bots and @All mentions"*).

**What broke — precisely.** The walk's entire state lived in **React state on the channel
page**:

```
app/src/routes/_authed/_app/channel/$channelId.tsx
  const [fanout, setFanout] = useState<{ text: string; remaining: string[] } | null>(null)
  const replyLanded = () => { ...stashFirstMessage(...); setChosenResponderId(next); setFanout(rest) }
```

`ChannelChat` is bound to exactly one runtime agent for its lifetime, so advancing the walk
means **remounting the component onto the next coworker's thread** and replaying a stashed
draft on mount. The advance is driven by `onReplyLanded`, fired from an
`agent.subscribe({ onRunFinishedEvent })` handler inside that component
(`channel-chat.tsx` ~line 555).

Every one of those is browser-resident:

- the queue (`remaining`) is component state;
- the *pointer* into the queue is a remount of a React subtree;
- the "one leg finished" signal is a subscription on a client-side agent object;
- the draft in flight is a `stashFirstMessage` handoff between two mounts.

So: **a refresh, a tab close, a laptop lid, or a browser crash kills the walk after the
first reply.** The first coworker's answer is durable (it was reported to the server); the
remaining N−1 asks simply never happen, and nothing anywhere knows they were owed. There is
no server row that says "a broadcast is in progress". Worse, the constraint that made the
walk *sequential* was not a product decision — it was "one live agent binding per page".
The person watching a five-coworker broadcast waits for five round trips in series for no
reason but the client's shape.

There is more evidence of the same misplacement elsewhere: `server/src/channels/stall-guard.ts`
and `turn-watchdog.ts` exist because a stalled Bot stream leaves "the channel busy, the
composer locked and a person watching a Bot that appears to be thinking and is not" — run
supervision that only works while somebody is watching.

**Why the fix belongs on a server.** Not "it would be nicer there" — the *owner* is wrong.
Delivery, the queue and the waiting are durable facts about work that was requested; a
browser tab is a viewport. Once the queue is a table and the advance is a worker:

- refresh / tab close / lid shut / crash — the work continues;
- the broadcast becomes **parallel**, because the one-live-binding-per-page constraint that
  forced sequence is gone;
- the same coworker is visible from every client at once, doing the same work;
- a failed leg is a row with a status, not an error message that vanishes with the page.
  (OpenSesame's walk stopped where a leg failed so the error stayed on screen — the right
  call given the constraint, and unnecessary once failures are durable.)

> **The rule.** **Anything that dies with a tab cannot be the thing that runs the work.**
> Put the queue, the run lock, the retry and the "who has answered so far" ledger in the
> database, and make the client a subscriber. A UI-owned orchestration loop is a prototype,
> and it will be indistinguishable from a working feature right up until somebody refreshes.

---

## 5. Model routing

Five commits' worth of a single idea: **which model** is an operating decision, separate
from identity, from the runtime, from the harness, and above all from the credential.

### 5a. Per-coworker route pins

`agent_profiles.model_route text` (`server/drizzle/0024_agent_model_route.sql`). A pin is a
name like `oag/auto`, `oag/cheap`, `oag/frontier`, `xai/grok-4.6`, `xai/grok-4.6@sub`
(`@sub` = subscription seat, `@api` = pooled API key). Null means "deployment default",
which is what almost every coworker does. Carried as an **opaque string** on purpose,
because the gateway owns that vocabulary and a copy of its catalogue in the product would
go stale (`server/src/copilot.ts`, `type ModelRoute`).

Commit `40b2bcc` (*"a package coworker may be repinned, and rotate its key"*) settled the
policy: `requireManageable` refuses every edit to a package-declared coworker, because its
name and role come from `agents.yaml` and the next sync writes them again — "an edit there
looks like it worked and is silently reverted." **The route is not that kind of fact.**
Which model is worth its cost today is answered by whoever watches the spend, and making it
a redeploy means the cheapest fix for a runaway bill is the slowest one. Nothing has to
defend the override: the package sync writes `agents` and `agent_profiles` and has *never*
written `model_route`, so a pin already survived every redeploy.

Two things the first cut got wrong, both worth stealing:

- `canManageAgent` returns false for a package coworker for **everybody, including
  administrators**, so the new guard could never have passed. Hence a separate
  `canRepinAgent`: administrators only, since package coworkers are shared and public by
  construction.
- **The key was locked by category and should not have been.** It lives in the vault as a
  credential id and the sync never writes a vault row, so like the route, nothing reverts
  it. Locking it would have prevented a package coworker from ever rotating its credential.

The form now disables **exactly what the sync overwrites** — name, title, role, visibility,
endpoint — and says why.

### 5b. The dialect rule (the sharpest edge in the whole product)

`ARCHITECTURE.md` § *One sharp edge: the first segment is a dialect*.

The CopilotKit runtime parses a built-in coworker's model string itself: it **splits on the
first slash**, looks the leading segment up in its own short list of SDKs (openai,
anthropic, google, minimax, vertex), and hands everything after it to that client as the
model name. So the leading segment chooses a *client and base URL*; the remainder is what
the far end receives. A gateway pin must therefore travel **inside** the remainder:

| Stored pin | Sent to the runtime | Gateway is asked for |
|---|---|---|
| `oag/auto` | `openai/oag/auto` | `oag/auto` |
| `zhipu/glm-5.3-flash` | `openai/zhipu/glm-5.3-flash` | `zhipu/glm-5.3-flash` |
| `xai/grok-4.6@sub` | `openai/xai/grok-4.6@sub` | `xai/grok-4.6@sub` |

`openai/` here names a **wire format, not a vendor**: the gateway serves the OpenAI dialect
and `OPENAI_BASE_URL` points at it, so a pin for an Anthropic model travels this way too and
the gateway translates. Sending a pin bare instead reads as "the zhipu SDK" and the run dies
with `Unknown provider "zhipu"` **before a byte reaches the gateway**.

`modelNameFor` in `server/src/copilot.ts:245` is the one place that knows this — nine lines,
one constant `GATEWAY_DIALECT = "openai"`, and a comment explaining that it is a wire format.

### 5c. How pins travel to remote agents

A remote AG-UI coworker runs its own loop and builds its own model client, so the server can
*state* the pin but cannot enforce it. It is sent as a forwarded prop, and **only when a pin
exists**, so the agent can tell "no preference" from "prefers the default"
(`server/src/copilot.ts:716`):

```ts
...(agent.modelRoute ? { openbotModelRoute: agent.modelRoute } : {}),
```

The bundled `agent-langgraph` honours it (commit `f71c95f`,
*"honour the model route pin, or refuse the run"*, `agent-langgraph/src/model-route.ts`):

- the pin goes **verbatim, with no dialect prefix**, because `ChatOpenAI` has already chosen
  the dialect and the endpoint owns what the name means;
- the Responses-API rule (`gpt-5.6`-tier) is applied to the pin's own model name — the
  segment after the last `/`, minus the `@api`/`@sub` qualifier — so a pinned 5.6-tier model
  behaves exactly like a configured one;
- **a pin it cannot honour refuses the run.** `BOT_PROVIDER=anthropic|google` throws, lands
  in the run's catch, and reaches the channel as `RUN_ERROR` naming the pin, the cause and
  the fix. Running the configured model instead "would answer the room from a model the form
  does not show — the exact lie a pin exists to prevent"; swapping SDKs to chase the pin
  "would make `BOT_PROVIDER` a different lie".
- One assumption travels with the pin and is documented rather than solved: **both sides
  point at the same gateway.** A deployment that points them apart gets pins resolved
  against the wrong catalogue — visibly, when the run fails, never silently.
- The coworker form says the pin is a preference the bundled Bot honours and a third-party
  agent may not. The product does not promise what it cannot enforce.

### 5d. A bot is not an API key — the five planes

From `ARCHITECTURE.md` and `README.md`. Mixing any two of these "is the bug that made the
earlier attempts feel stuck":

| Plane | What it is | Who owns it |
|---|---|---|
| **Identity** | Name, role, avatar, membership, visibility | Product DB: `agents`, `agent_profiles`, `channel_agents`, `channel_memberships` |
| **Runtime** | The process that speaks AG-UI (or ACP) | `agent-langgraph`, a custom AG-UI server |
| **Harness** | System prompt, tool loop, computer, skills, boundaries | This server + supervisor + CEL gateway |
| **Model route** | Which model, with which effort/thinking/slug wire | A route pin on the coworker + `packages/wire-maps` |
| **Credential** | Who pays: a pooled API key or a bound OAuth seat | open-ai-gateway, only. **Keys never touch an agent row.** |

An agent row stores neither a model nor a key. `agents.configuration` is `{systemPrompt}`
for a built-in coworker and `{endpoint, auth: {header, credentialId}}` for a remote one —
a credential *id*, with the secret in an AES-GCM vault (`server/src/credentials.ts`). A
coworker may carry a **runtime token** (`MANAGED_AGENT_TOKEN`, or a write-only AG-UI auth
header resolved from the vault per run, `server/src/agents/auth-header.ts`) — that
authenticates the *product to the agent process*, is not an LLM key, and is stored as an id.
Keys are resolved **per load, not cached on the row**, so revoking a credential takes effect
on the next run rather than the next restart (`server/src/agents/runtime-agents.ts`).

There is a test that enforces the separation: `tests/provenance.test.ts` asserts among other
things that no provider key appears on a coworker row.

> **The rule.** **Keep identity, runtime, harness, model route and credential in five
> different places, and let nothing carry two of them.** Then:
> - A model pin is an *operating* decision — editable by whoever watches the spend, never a
>   redeploy, and specifically never locked by the same guard that protects declarative
>   fields. Lock exactly what a sync overwrites; a field the sync never writes needs no
>   guard.
> - **A pin that cannot be honoured must refuse the run**, loudly, naming the pin and the
>   fix. Degrading to the configured model is a lie in the shape of a success.
> - Model-name strings are **parsed by somebody**. Find out who splits on what before you
>   design your pin format, and keep the translation in exactly one function.
> - To a remote agent a pin is a **preference**; say so in the UI. To a built-in loop it is
>   a rule.

---

## 6. Attribution in a multi-party transcript

**What we tried.** A chat transcript: bot messages left, the person's messages
right-aligned in a bubble. That works for exactly one bot and one person.

**What broke.** Twice.

- Rooms have several coworkers. A reply with no caption does not say *which* coworker
  answered — and after a fan-out, five uncaptioned replies are indistinguishable. Fixed in
  commit `eaa787b`: in rooms, the coworker's avatar and name caption the assistant message
  that **opens each exchange**.
- Channel membership is open — anybody can be added — so "a bare right-aligned bubble stops
  saying whose words these are the moment a second person exists" (commit `d478524`,
  *"Caption the person's messages too: name plus monogram"*). The person's caption **mirrors**
  the coworker's: once per exchange, name plus a monogram chip where an avatar would go,
  hugging the same edge their bubbles do.

**Where the code is.** `app/src/components/channels/chat-transcript.tsx` —
`speakers?: Record<string, TranscriptSpeaker>`, `author?: {name, initials}`, and
`opensExchange`, which opens a new caption **on a change of speaker as well as on a
person's turn**. `app/src/lib/auth/initials.ts` — `userInitials`, moved out of the sidebar
so the chip in the sidebar and the caption in the transcript "cannot drift into different
letters for the same person". A drawn coworker gets its avatar at 16px, static.

**One performance constraint that shaped the API.** `TranscriptMessage` is memoised **on
primitives**, so the caption travels as `speakerName` / `speakerSeed` / `speakerRecipe`
(JSON) and is parsed *inside* the memo boundary. Passing a speaker object would change
identity every render and defeat the memo. Conversely, the room weave itself is
**deliberately not memoised**: the running agent hands back the *same* messages array and
mutates it in place, so a `useMemo` keyed on it would never see a chunk land and the reply
would never appear. The cheap walk is unmemoised; the expensive part (markdown, tool
renderers) sits behind the memo.

**The seam that matters.** Today every user turn in a thread belongs to the thread's member,
so "one author per transcript" is exact. The commit says it plainly:

> "per-message authorship is the seam shared transcripts will replace."

`server/tests/channel-roster.integration.test.ts` asserts the current limit
*deliberately* — joining a room mints you your own threads so you can use it immediately,
but you cannot see what was said before or what other members say — **so the limit cannot
drift without a test failing and saying so.**

> **The rule.** **Store authorship per message, from the first schema.** Not per thread, not
> per channel, not inferred from `role`. A transcript with more than one participant on
> either side needs `author = person | coworker` on the row, and every renderer needs to
> caption on *change of speaker*. This is the one seam OpenGrok gets to build correctly on
> day one, because it owns the store — and it is the seam that unlocks genuinely shared
> multi-human rooms, which OpenSesame never reached.
>
> Secondary rule: **derive display identity (monograms, colours, seeds) in one module.** Two
> surfaces computing the same person's initials will disagree eventually.

---

## 7. What to port, what to leave behind

Opinionated. Argue with it, but argue explicitly.

### Port

**Schema ideas**

- **An append-only ordering index for a room, separate from message storage.** Even with
  server timestamps, having "the order the room was said in" as its own table — written in
  the same transaction as the turn, idempotent on replay, capped from the *newest* end —
  survives changing your message store. (`0028_channel_timeline.sql`)
- **Composite keys that encode the real invariant.** `(user, channel, agent)` was discovered
  after production wedged. Write the key you mean.
- **Credential *ids* on rows, secrets in a vault, resolved per run.** Never cached on the
  registered agent. (`server/src/credentials.ts`, `runtime-agents.ts`)
- **A route pin as an opaque string owned by the gateway's vocabulary.** No local catalogue
  copy to go stale.
- **Deployment-fingerprinted ids** (`thread-identity.ts`): UUIDv8 with a 6-byte digest of
  the deployment name in the leading bytes, so ownership is answerable without a lookup.
  Cheap, and it saves you when two deployments share one backing store. Note the honesty in
  its doc comment: `owns() == false` means "not certainly mine", not "certainly not mine",
  and callers must treat those differently.
- **Tombstones over deletes for agents.** A deleted coworker becomes
  `type: "unavailable"` with a reason; conversations stay readable and the UI says why it
  cannot reply.
- **Mint-on-read healing** for derived per-(person, thing) rows, with
  `onConflictDoNothing` + re-read for concurrency.
- **Write the ordering fact before the denormalised preview**, inside one transaction. The
  preview may legitimately be dropped as stale; the ordering fact never may.

**UX rules**

- **Latch the responder when a room opens.** Deriving it live from "who spoke last" meant
  every reply flipped the default, remounted the chat onto another thread mid-conversation,
  and the transcript vanished and crawled back. **Only a person changes the responder.**
- **Never let a transcript get shorter while somebody watches.** `keepPreviousData` on both
  room reads. Whatever OpenGrok's client is, this rule survives.
- **Caption on change of speaker**; monogram for people, avatar for coworkers.
- **One ask, one bubble** (lesson 3), with retries preserved.
- **Degrade visibly.** Un-orderable history is grouped and labelled, not invented. Unreadable
  messages are counted and announced ("One earlier message could not be read and is not
  shown. The rest of this conversation is complete.") rather than silently dropped.
- **Silence limits, not duration limits.** `turn-watchdog.ts`: a turn may legitimately run
  for an hour; a turn that has produced *nothing* for `stallMs` is wedged. A duration cap
  puts a ceiling on how much work a Bot may do (nobody asked for that); a silence cap puts a
  ceiling on how long a person watches a spinner (the actual complaint). And **stop the
  clock during handover** to the consumer, or you blame the Bot for your own backpressure.
- **Findability is a policy, and it lives in the store, not the UI** (commit `19ee0fa`): an
  administrator can find anybody, everybody else only people they already share a room with.
  "A workspace where any member can enumerate every account by typing 'a' into a picker has
  turned a chat feature into a directory leak."
- **Not-a-member and no-such-thing return the same answer**, so membership is not probeable.
- **Fail closed, and record the noop.** "A control that cannot be expressed on the wire is a
  recorded noop with a reason, never a fake success." `applied.wire.<control> = {status:
  "noop", reason}`.
- **Tell the agent what it holds.** A Bot with no connector grants and no guidance treated a
  connected vendor as an ordinary website and browsed to it. And put grants *before* the
  long computer prose, or the Bot reaches for the browser even when it holds the right tool.
- **A run stops after one step unless told otherwise** — which for a tool-holding Bot means
  it calls one tool and never speaks. `maxSteps` only when tools exist.

**Test approaches**

- **Pure core, tested without infrastructure.** `weaveRoom` takes histories + index + live
  messages and returns a list. `app/tests/weave-room.test.ts` proves every ordering rule
  with no room, socket or agent. Rust makes this natural — keep the ordering/collapse logic
  in `og-core` with zero I/O.
- **Tests that assert *limits* on purpose**, so a known gap cannot silently change
  (`channel-roster.integration.test.ts`: "gives each person their own thread in a shared
  room").
- **Provenance tests** (`tests/provenance.test.ts`): assert what is *not* in the repo — no
  generated protobuf, no recovered renderer chunks, no provider key on an agent row. Note
  the guard against vacuous passes: it checks `git ls-files` exited 0 and returned >100
  files, because outside a repo every assertion would hold and the check would go green
  precisely when it can see least. **OpenGrok has an active legal line
  (`grok-bot/NOTICE.md`, "inventory only") and needs this test on day one.**
- **A stub for the external dependency in-tree** (`scripts/oag-stub.ts`) so a fresh clone
  reaches a working turn with no credentials. It serves `/v1/responses` too, "which is not
  optional: 5.6-tier models use it." Ship `og-gateway-stub` equivalently.
- **Verify against the live thing and say so in the commit.** Nearly every commit body ends
  with what was verified live and against which binary. `200-accepted ≠ honored`: "a field
  that returns 200 and changes nothing is worse than a 400. Prove the behaviour."
- **Migration/snapshot pairing checked by CI.** A drift probe caught hand-written SQL with
  no drizzle snapshot beside it, twice (`c2e3c55`, `5f8814f`). Whatever `sqlx migrate` does,
  have a probe that fails when the schema and the migrations disagree.

**Prose practice**

- Commit messages that state **what broke** and **why the fix is shaped this way**. This
  document is only possible because they exist. Keep doing it in Rust.
- `ARCHITECTURE.md` with a *Known limits, stated rather than hidden* section, and a
  *Verdicts* section that records rejected options (V4: take Buzz's room model, not its
  Nostr stack — "two sources of truth is how this dies"; V5: the Cursor graft is a legitimate
  optional coworker type behind AG-UI, but not the platform).

### Leave behind

- **The per-agent thread constraint itself.** Lessons 2, 3 and part of 6 are all workarounds
  for it. OpenGrok owns its store; use a channel-scoped transcript with per-message
  authorship and delete the whole category.
- **Client-side orchestration.** The fan-out queue, the remount-to-rebind trick, the
  stashed draft handoff (`stashFirstMessage`), `onReplyLanded`, the latched-responder
  bookkeeping in `$channelId.tsx`. All of it exists because delivery lived in a page.
- **Sequential fan-out.** Its only justification was one live agent binding per page.
- **"Thread is locked" retry-with-backoff** as a response to `409 THREAD_AGENT_MISMATCH`.
  Retrying a permanent error is a bug; keep bounded retry only for genuinely transient
  contention, and make your own error strings distinguish the two.
- **The React-memo-on-primitives dance.** It is a symptom of caption data threading through a
  render tree that mutates an array in place. A server-pushed, immutable event stream makes
  it moot.
- **Single-`role` messages as the authorship model.** `role: "user" | "assistant"` cannot say
  *which* user or *which* assistant. It leaked into `channel_timeline.role` too.
- **A hosted state platform as a boot requirement.** See lesson 8.
- **`OPENBOT_SINGLE_USER=true`** as a shipped default. It admits every request as one
  administrator. It is a fine dev affordance; it is not a default.
- **The CopilotKit runtime's model-string parser.** OpenGrok routes through the gateway
  directly; there is no reason to inherit a first-slash dialect split. But **keep the
  lesson**: if any layer parses your model names, own the translation in one place.
- **Deriving anything from `agentIds[0]`** or any other positional roster fact.

---

## 8. The hosted-dependency trap

**The load-bearing lesson. Read this one twice.**

**What we tried.** Build on OpenBot, which builds on CopilotKit Intelligence for durable
state. It is a good product and the integration is clean. The reasoning was explicitly
recorded in `ARCHITECTURE.md`: Intelligence "is durable state and coordination, not
reasoning" — the harness is ours, so replacing Intelligence later is "a runner-and-storage
swap, not a harness rewrite." The library even ships an `InMemoryAgentRunner`; OpenBot
simply declines to expose that branch.

**What broke.** All of that is true, and it did not help.

The product **cannot boot** without a proprietary hosted service. `server/src/config.ts`
(~line 548) refuses to start when any of `INTELLIGENCE_API_URL`,
`INTELLIGENCE_GATEWAY_WS_URL`, `INTELLIGENCE_API_KEY`, `COPILOTKIT_LICENSE_TOKEN` are
missing, with `CopilotKit Intelligence is required and is not configured`.
`server/src/copilot.ts` says it outright:

> "There is no SSE branch. Intelligence is a requirement of the product, not a tier: it owns
> durable threads, memory and learning, and a deployment without it silently forgets every
> conversation. `config.ts` refuses to boot without the full contract, so by the time this
> runs the settings are present and this file has one mode."

The README's requirements list a CopilotKit Intelligence project **and license**, and the
quick start's step 2 is three `npx copilotkit` commands against a vendor account — in a
product whose entire pitch is "runs inside your own infrastructure, your data in your
PostgreSQL, no model in the box."

And the dependency was not inert. It set the product's shape:

- It **dictated the thread key** and therefore the room model (lesson 1), via a 409 with a
  misleading error string.
- It **withheld timestamps**, forcing an entire index table and a weave function (lesson 2).
- It **disagreed with the gateway about message order** for the same thread, forcing the
  live-vs-stored reconciliation rule (lesson 2).
- Its **run-id reuse** in the `/v1/responses` translator (`msg_1` / `rs_0` across responses)
  made id-keyed clients merge every reply into the first assistant message — "which is what
  made replies look like they never arrived" (commit `eaa787b`). Filed upstream. Not
  fixable locally.
- It offers **no field for a deployment to stamp its own name**, so thread ownership had to
  be smuggled into the *id bytes* (`thread-identity.ts`).
- Its **404 shape** had to be duck-typed, because its error class is not part of the
  package's public surface (`thread-status.ts`).

Every one of those is a design decision made by a vendor, discovered in production, and
worked around in this repo.

**What the dependency actually provided.** Three things, and they are the three things
OpenGrok must implement itself:

1. **Durable transcripts.** Threads (`/api/threads`, `/api/threads/:id/messages`), thread
   subscription, and shipping AG-UI events over a websocket so a run survives the process
   that produced it (`IntelligenceAgentRunner`). *"It records the turn; it does not think
   it."*
2. **Run locks.** Coordination so two clients cannot drive one conversation into
   inconsistency.
3. **Long-term memory.** `/api/memories` create / supersede / forget, and
   `POST /api/memories/recall` for hybrid RAG.

That is the whole surface. It is a **week or two of Postgres**, not a platform:

- transcripts → an append-only `messages` table with `(channel, seq, author, role, content,
  at)` and a `LISTEN/NOTIFY` or SSE fan-out. You get timestamps, ordering, per-message
  authorship and shared rooms for free — three of this document's eight lessons evaporate.
- run locks → a row with a lease and an expiry, `SELECT … FOR UPDATE SKIP LOCKED` for the
  worker. The same table is the fan-out queue from lesson 4.
- memory → pgvector, which the deployment **already runs** (`README.md`: "PostgreSQL with
  pgvector, 5432"). The embedding call goes out through the gateway like everything else.

`og-store` in `docs/PLAN.md` already lists "coworkers, transcripts, runs, the fan-out
ledger". That is exactly right, and it is the crate that must not be deferred.

> **The rule.** **A dependency you cannot boot without is not a dependency, it is your
> architecture — and its bugs, its missing fields and its error strings become your
> product's.** Before adopting a hosted service for *state*:
>
> 1. **Write down its surface.** If you can enumerate it in a paragraph (threads, locks,
>    memory), you can implement it, and you should — state is the part you can least afford
>    to rent.
> 2. **Ask what it refuses to tell you.** No timestamps, no deployment field, no public error
>    types, one agent per thread. Those absences will become tables, index columns, id
>    encodings and duck-typed checks in *your* repo.
> 3. **Require a working local mode from commit one.** Not a stub for CI — a real,
>    supported, no-account path. OpenSesame did this correctly for the *model* door
>    (`scripts/oag-stub.ts`, so a fresh clone reaches a working turn with no credentials)
>    and not at all for the *state* door. The asymmetry is the whole lesson.
> 4. **"Replacing it later is just a storage swap" is true and irrelevant** once the vendor's
>    constraints have shaped your schema, your UI and your invariants. The swap is cheap; the
>    workarounds you built around it are not.
>
> OpenGrok's version: **own transcripts, run locks and memory in `og-store` before anything
> else is built on them.** Rent the model door (that is what open-ai-gateway is *for*, and it
> is MIT and ships in the same binary). Never rent the record.

---

## Appendix: where to look

| Lesson | Primary files | Commits |
|---|---|---|
| 1. Thread key | `server/drizzle/0026_per_coworker_threads.sql`, `server/src/channels/routes.ts` (`ChannelStore.get`), `server/src/channels/thread-identity.ts` | `e3fe3ac`, `ce01e4a` |
| 2. Woven timeline | `app/src/lib/channels/timeline.ts`, `server/drizzle/0028_channel_timeline.sql`, `server/src/channels/routes.ts` (`POST /activity`, `GET /:id/timeline`), `app/tests/weave-room.test.ts` | `8607a05` |
| 3. Fan-out collapse | `collapseFanout` in `app/src/lib/channels/timeline.ts` | `d0e85f2` |
| 4. Browser-owned walk | `app/src/routes/_authed/_app/channel/$channelId.tsx`, `app/src/components/channels/channel-chat.tsx`, `server/src/channels/{stall-guard,turn-watchdog}.ts` | `eaa787b` |
| 5. Model routing | `server/src/copilot.ts` (`modelNameFor`, `forwardedProps`), `agent-langgraph/src/model-route.ts`, `server/drizzle/0024_agent_model_route.sql`, `server/src/agents/{runtime-agents,auth-header,profile-policy}.ts`, `ARCHITECTURE.md` | `3f2fa1a`, `f71c95f`, `40b2bcc` |
| 6. Attribution | `app/src/components/channels/chat-transcript.tsx`, `app/src/lib/auth/initials.ts`, `server/tests/channel-roster.integration.test.ts` | `eaa787b`, `d478524` |
| 7. Port / leave | this document | — |
| 8. Hosted dependency | `server/src/config.ts` (~548), `server/src/copilot.ts` (~30-56), `README.md` § Requirements, `ARCHITECTURE.md` § Where the intelligence actually lives | `3f2fa1a` |

Read `git -C /Volumes/goldcoders/projects/opensesame/opensesame log --format=%B` end to end
once. It is forty minutes and it is the best documentation in the repository.
