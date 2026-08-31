# Auto-review: two tiers, one gate

Status (2026-08-31): §2, §3 and §6 are **implemented** (`crates/opengrok-store/src/auto_review.rs`,
`crates/opengrok-server/src/auto_review.rs`, unit + Postgres-backed tests); §4 enforcement and the
real `resolveAutoReviewApproval` in §5 are **still to build**. The audit of what existed before is
§1. The consent model this sits inside is §0.

## 0. The consent model (one question per layer)

Running a bot command on the user's own machine passes through these controls, and only these:

| Layer | The one question it answers | Where it is enforced | Values | Default |
|---|---|---|---|---|
| The machine's switch | does this computer accept bot commands at all? | the daemon, on the machine | on / off | on after enrol |
| Remote control | may bots reach this machine, and how? | the server, per machine (`local_exec`) | off / ask / always (stored `never`/`ask`/`bypass`) + a visible, deletable list of standing rules | off |
| The card | consent for THIS command | the server; answerable from any device | allow once / always / deny / never — **never expires** | — |
| Auto-review | what may bots do? | the server, global → per coworker | on/off + allow/block instructions, judged by a model across ALL tools | off |

Rules that follow from it:

- The card is the **one** consent surface. "Always"/"Never" on it write a server standing rule
  and nothing else — the client does not flip the machine's switch on the user's behalf.
- The machine's switch has **no "ask"**. A daemon-side ask cannot be answered from a phone and
  had to expire; consent has to live where every device can reach it, which is the server.
  **The trade-off, stated plainly:** this removes the machine's per-command veto, so a
  compromised server can drive an enrolled machine whose switch is on. The switch is the local
  brake — it is enforced by the daemon alone, with no server involvement, and must stay that way.
- Auto-review has **two tiers**: global, overridden per coworker. A device tier was designed and
  cut the same evening: "what may bots do on this machine" is already that machine's standing
  rules, and the user asked for one answer per question.
- **At most one card per tool call.** When the remote-control gate already suspended for consent,
  auto-review does not raise a second ask for the same call.

## 1. What existed before (audited, with evidence)

- `gateway/mod.rs:141` — `"autoReviewInstructions": null` in the `getHostSettings` default
  record; present only because the client's resync chain reads the record whole.
- `gateway/routes.rs` `setHostSettings` — shallow-merges the client's patch into an **in-memory**
  mutex and echoes it. Unpersisted (restart resets it), client-writable, and read by nothing.
  **The wrong home for policy** — tiers live in Postgres beside the local-exec rules instead.
- `gateway/routes.rs` `resolveAutoReviewApproval` — answered `200 null`, a transcribed-contract
  stub. §5 makes it real.
- Zero review logic in `opengrok-harness`, `opengrok-tools`, `opengrok-policy`.
- On the Cursor route the box agent enforces these instructions (`syncHostSettingsToBox` pushes
  the record to the box). This doc is the OpenGrok route only; the Cursor route is untouched.

## 2. Storage

One table, one row per (account, scope). Not lists of pattern rows like `local_exec_rule` —
the client shape is two instruction *texts* plus a toggle, and a tier **overrides** the tier
below it per-field rather than merging.

```sql
create table if not exists auto_review_policy (
    account_id         text   not null,
    scope_kind         text   not null,          -- 'global' | 'coworker'
    scope_id           text   not null,          -- '' for global; coworker_id
    enabled            boolean,                  -- null = inherit from the tier below
    allow_instructions text,                     -- null = inherit; '' = explicitly none
    block_instructions text,                     -- null = inherit; '' = explicitly none
    updated_at_ms      bigint not null,
    primary key (account_id, scope_kind, scope_id)
);
```

Every field is independently tri-state: `null` inherits, a value overrides. Deleting a row
restores full inheritance for that scope. The column is plain text and the store is generic over
it; the **server** is what refuses any scope kind but the two, and the tier query never returns
another kind (a legacy `machine` row is invisible to resolution and purged by the schema script).

## 3. Effective policy (resolution)

```
effective(account, coworker_id):
    for each field in (enabled, allow_instructions, block_instructions):
        coworker row's field  ??  global row's field  ??  default
    defaults: enabled = false, instructions = ''
```

`enabled = false` effective ⇒ auto-review is off for this action: no judge call, no cost, no
behavior change. The default is off — auto-review is an opt-in the user switches on, unlike the
remote-control channel whose default is the closed `Never` (that gate guards *reaching a machine
at all*; this one refines what an already-reachable coworker may do).

**Short-circuit (user requirement: "no rules or auto-review off ⇒ don't walk the tiers per
call").** The tier walk happens **once per run**, when the runner is built — the same moment
`tools_for_coworker` decides which tools to offer — and the resolved policy is carried on the
runner. Per tool call the check is one in-memory test:

```
!effective.enabled || (effective.allow == "" && effective.block == "")  ⇒  skip entirely
```

No DB read, no judge call. A run that started before a `PUT`/`DELETE` keeps the policy it
started with; the next run picks up the change (a run is minutes, and a resumed run rebuilds its
runner and so re-resolves). If per-run resolution ever shows up in profiles, the upgrade is a
per-account epoch bumped on every `PUT`/`DELETE` and a `(account, coworker) → policy` cache
keyed on it — not a change to the ladder.

## 4. Enforcement: THE GATE, second chair

The enforcement point is `Executor::execute` in `opengrok-tools` — the one funnel every tool
call passes through (box shell, read/write, plugin tools, `user_machine_shell`) — placed
**after** identity overwrite and **after** the primary gate for the tool:

1. **Order.** For `user_machine_shell` the remote-control gate (`local_exec::decide`) judges
   first — if the channel says deny, there is nothing to review. For box tools the coworker's
   policy grant judges first. Auto-review judges what survives.
2. **Most restrictive verdict wins**: block > ask > allow. A block from either gate blocks.
3. **At most one card per call.** If the primary gate already suspended for the user's consent,
   auto-review does not raise a second ask for the same call; the human's explicit per-command
   approval subsumes a review "ask". A review *block* still blocks — a standing written rule
   outranks a click, and the refusal says which rule.

**The judge.** Instructions are natural language, so the verdict is a bounded model call —
exiting through open-ai-gateway like every model call (non-negotiable #4), on a deployment-owned
route (`OG_AUTO_REVIEW_MODEL`, default the server's own model), never the coworker's own route:
one call per tool call must be cheap, the reviewer must not be the reviewed, and a coworker-route
outage must not become a wall of cards. Given ONLY the tool name, redacted arguments, and the
effective instruction texts, it returns one word of `allow | block | ask`. Fail-closed ladder
(non-negotiable #8):

- block instructions apply → **block**: the tool call returns a refusal *result* naming the
  instruction (the model can reason about it; the run does not die).
- else allow instructions apply → **allow**.
- else, or the judge is uncertain, or both sides apply → **ask** (ask beats allow).
- judge call fails, times out, or answers anything but one bare word → **ask** (never silently
  allow; never hard-fail the run).

An ask suspends the run through the proven machinery: `RunCommand::Suspend` →
`AwaitingApproval`, no expiry ever — the card answers whenever the user returns.

## 5. The ask card and `resolveAutoReviewApproval`

The suspension emits a transcript entry using the renderer's **auto-review approval card**.
Shape transcribed by the desktop peer from the shipped renderer (non-negotiable #1 — nothing
below is invented):

```json
{ "kind": "send-message", "id": "<entryId>", "timestampMs": 0,
  "message": { "type": "auto-review-approval",
    "approval": {
      "requestId": "<tool call id>",
      "status": "pending | approved | always | denied | expired",
      "surface": "host_shell | box_shell | mcp | computer | automation_write | cloud_agent",
      "summary": "<required string>",
      "reason": "<optional paragraph — judge uncertainty goes here>",
      "command": "<optional; when set the card shows it and HIDES summary>",
      "proposedRule": "<optional pre-filled Always text; client redacts secrets>" } } }
```

Rules the renderer imposes, which the server must honour:

- Dedup key is `auto-review-approval:${requestId}:${status}` — re-emit the **same entryId**
  changing only `approval.status` (the exec-card discipline, unchanged).
- `surface` = `host_shell` for `user_machine_shell`, `box_shell` for the box's `shell`,
  `mcp` for plugin tools. Never absent: the client's fallback `"unknown"` is not in its enum.
- `summary` is hidden when `command` is present or when it matches the renderer's boilerplate
  (`Run a command on your local computer`, `…the agent's VM`, `^Run "…`, `^Use … tool … with …`).
  So: set `command` for shell tools; write a *meaningful* summary for everything else.
- `approved` renders as "Allowed once"; `always` / `denied` / `expired` render as settled states.

**The answer.** `resolveAutoReviewApproval {entryId, requestId, resolution, agentId}` — and on
the wire `resolution` is **only `approved | denied`**; the client's type excludes `always`. The
Always button is client-side: it appends `proposedRule` to the most specific tier (the coworker's,
via `PUT /auto-review/policy`), then sends `approved`. The server never sees "always" here, and
must not write a rule of its own on this verb. The handler mirrors `resolveLocalToolPermission`:
find the suspended run by `requestId` (= tool call id) **and by suspension reason** (the wrong
verb must not settle the other card), answer the aggregate exactly once, flip the card to
`approved`/`denied` on the same entryId, resume on approval, resume with a refusal result on
denial. The client expects a resolved-vs-stale distinction: 200 ⇒ resolved; a dead request heals
the card to `expired` and returns 410 ⇒ stale. No expiry timer anywhere, ever.

## 6. Management API (the contract the client writes against)

Account-authed like `/local-exec/*` (Bearer-or-cookie). Scope addressing is uniform:

```
GET    /auto-review/policy
       → { "global":    { "enabled": …, "allowInstructions": …, "blockInstructions": …, "updatedAtMs": … } | null,
           "coworkers": { "<coworkerId>": { …same fields… }, … } }
       Rows as stored: null field = inherits. Absent row = null. The client renders inheritance
       itself; nothing here is pre-resolved.

PUT    /auto-review/policy
       { "scopeKind": "global" | "coworker",
         "scopeId":   "" | "<coworkerId>",
         "enabled": true | false | null,
         "allowInstructions": "<text>" | null,
         "blockInstructions": "<text>" | null }
       Upsert of the whole row (all three fields every time — null means inherit, not "keep").
       422 on an unknown scopeKind (including "machine"), on a coworkerId that isn't the
       account's, on global with a scopeId, or on more than 20 000 characters of instructions.
       → 204

DELETE /auto-review/policy   { "scopeKind": …, "scopeId": … }
       Remove the row entirely (full inheritance). → 204

GET    /auto-review/effective?coworkerId=…
       → { "enabled": bool, "allowInstructions": "…", "blockInstructions": "…",
           "decidedBy": { "enabled": "coworker|global|default", … per field } }
       The resolved view + which tier decided each field — for the settings UI to show "inherited
       from global" honestly instead of re-implementing precedence.
```

Wiring: General tab → `PUT {scopeKind:"global", scopeId:""}` (in addition to its Cursor-route
`setHostSettings` write, which stays untouched for Cursor); the agent's settings →
`scopeKind:"coworker"`.

**One example** — global blocks installs; one trusted coworker overrides nothing but is switched
off, another inherits everything:

```
PUT /auto-review/policy
{ "scopeKind": "global", "scopeId": "",
  "enabled": true, "allowInstructions": null,
  "blockInstructions": "anything that installs software or changes system settings" }
→ 204

PUT /auto-review/policy
{ "scopeKind": "coworker", "scopeId": "cw_01a0562a-…", "enabled": false,
  "allowInstructions": null, "blockInstructions": null }
→ 204

GET /auto-review/effective?coworkerId=cw_01a0562a-…
→ { "enabled": false, "allowInstructions": "", "blockInstructions": "anything that installs…",
    "decidedBy": { "enabled": "coworker", "allowInstructions": "default",
                   "blockInstructions": "global" } }
```

`brew install jq` from any *other* coworker → judge says block → the tool call returns the
refusal as a result; the bot tells the user which instruction stopped it. From `cw_01a0562a-…`
auto-review is off, so only the remote-control gate speaks.

## 7. Explicitly not in v1

- No merging of instruction texts across tiers (override only). Revisit if users ask.
- No per-rule granularity (the row is the unit). The client's "always allow" card control
  appends text to the coworker tier; structured rule lists are a later shape.
- No device tier (see §0).
- No enforcement on the Cursor route (the box agent already owns that; double enforcement would
  double-ask).
- No caching of judge verdicts. Every action is judged fresh (policy is enforced every time,
  non-negotiable #6).
