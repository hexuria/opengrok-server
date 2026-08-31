# Auto-review: three tiers, one gate

Status (2026-08-31): §2, §3 and §6 are **implemented** (`crates/opengrok-store/src/auto_review.rs`,
`crates/opengrok-server/src/auto_review.rs`, unit + Postgres-backed tests); §4 enforcement and the
real `resolveAutoReviewApproval` in §5 are **still to build**. The audit of what existed before is §1. The client half (three settings surfaces) is the desktop peer's; this doc is the server
half: storage, precedence, enforcement, and the contract the client writes against.

## 0. What auto-review is

User-written natural-language instructions that judge a coworker's actions before they run:
*allow instructions* ("routine git commands are fine"), *block instructions* ("never touch
production configs"), and an enabled toggle. On the Cursor route the box agent enforces them
(`syncHostSettingsToBox` pushes the record to the box). On the OpenGrok route nothing enforces
them today — this doc gives them a real home and a real gate.

The user's design, which this implements: **three tiers, most specific wins.**

1. **Global** — the General-tab block (today's Cursor-route surface, kept as-is).
2. **Per-device** — overrides global; lives beside the per-device execution policy.
3. **Per-agent** — first-class, highest precedence; set when creating/editing an agent.

Precedence: **agent > device > global**. On conflict at judgment time, **ask beats allow**
(the existing UI copy promises this).

## 1. What exists today (audited, with evidence)

- `gateway/mod.rs:141` — `"autoReviewInstructions": null` in the `getHostSettings` default
  record; present only because the client's resync chain reads the record whole.
- `gateway/routes.rs` `setHostSettings` — shallow-merges the client's patch into an **in-memory**
  mutex and echoes it. Unpersisted (restart resets it), client-writable, and read by nothing.
  **The wrong home for policy** — tiers live in Postgres beside the local-exec rules instead.
- `gateway/routes.rs` `resolveAutoReviewApproval` — answered `200 null`, a transcribed-contract
  stub. §5 makes it real.
- Zero review logic in `opengrok-harness`, `opengrok-tools`, `opengrok-policy`.

## 2. Storage

One table, one row per (account, scope). Not lists of pattern rows like `local_exec_rule` —
the client shape is two instruction *texts* plus a toggle, and a tier **overrides** the tier
below it per-field rather than merging (the user's word: device rules *override* global).

```sql
create table auto_review_policy (
    account_id         text   not null,
    scope_kind         text   not null,          -- 'global' | 'machine' | 'coworker'
    scope_id           text   not null,          -- '' for global; machine_id; coworker_id
    enabled            boolean,                  -- null = inherit from the tier below
    allow_instructions text,                     -- null = inherit; '' = explicitly none
    block_instructions text,                     -- null = inherit; '' = explicitly none
    updated_at_ms      bigint not null,
    primary key (account_id, scope_kind, scope_id)
);
```

Every field is independently tri-state: `null` inherits, a value overrides. Deleting a row
restores full inheritance for that scope.

## 3. Effective policy (resolution)

Computed at judgment time, never cached across turns:

```
effective(account, machine_id, coworker_id):
    for each field in (enabled, allow_instructions, block_instructions):
        coworker row's field  ??  machine row's field  ??  global row's field  ??  default
    defaults: enabled = false, instructions = ''
```

`enabled = false` effective ⇒ auto-review is off for this action: no judge call, no cost, no
behavior change. The default is off — auto-review is an opt-in the user switches on, unlike the
execution channel whose default is the closed `Never` (that gate guards *reaching a machine at
all*; this one refines what an already-reachable coworker may do).

**Short-circuit (user requirement: "no rules or auto-review off ⇒ don't walk bot→device→global
per call").** The three-tier walk happens **once per run**, when the runner is built — the same
moment `tools_for_coworker` decides which tools to offer — and the resolved policy is carried on
the runner. Per tool call the check is one in-memory test:

```
!effective.enabled || (effective.allow == "" && effective.block == "")  ⇒  skip entirely
```

No DB read, no judge call. A run that started before a `PUT`/`DELETE` keeps the policy it
started with; the next run picks up the change (a run is minutes, and a resumed run rebuilds its
runner and so re-resolves — see `resume_gateway_run`). If per-run resolution ever shows up in
profiles, the upgrade is a per-account epoch bumped on every `PUT`/`DELETE` and a
`(account, machine, coworker) → policy` cache keyed on it — not a change to the ladder.

## 4. Enforcement: THE GATE, second chair

The enforcement point is the tool executor — the same seam every tool call already passes
through — placed **after** identity overwrite and **after** the local-exec gate:

1. **Order.** For `user_machine_shell`, the local-exec gate (machine consent) judges first —
   if the channel says deny, there is nothing to review. Auto-review judges what survives.
2. **Most restrictive verdict wins**: block > ask > allow. A block from either gate blocks.
3. **At most one card per call.** If the local-exec gate already suspended for the user's
   consent, auto-review does not raise a second ask for the same call; the human's explicit
   per-command approval subsumes a review "ask" (a review *block* still blocks — a standing
   written rule outranks a click, and the refusal says which rule).

**The judge.** Instructions are natural language, so the verdict is a bounded model call —
exiting through open-ai-gateway like every model call (non-negotiable #4) — given ONLY the tool
name, the arguments, and the effective instruction texts, returning one token of
`allow | block | ask`. Fail-closed ladder (non-negotiable #8):

- block instructions apply → **block**: the tool call returns a refusal *result* naming the rule
  (the model can reason about it; the run does not die).
- else allow instructions apply → **allow**.
- else, or the judge is uncertain, or both sides apply → **ask** (ask beats allow).
- judge call fails or times out → **ask** (never silently allow; never hard-fail the run).

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
  `mcp` for plugin tools.
- `summary` is hidden when `command` is present or when it matches the renderer's boilerplate
  (`Run a command on your local computer`, `…the agent's VM`, `^Run "…`, `^Use … tool … with …`).
  So: set `command` for shell tools; write a *meaningful* summary for everything else.
- `approved` renders as "Allowed once"; `always` / `denied` / `expired` render as settled states.

**The answer.** `resolveAutoReviewApproval {entryId, requestId, resolution, agentId}` — and on
the wire `resolution` is **only `approved | denied`**; the client's type excludes `always`. The
Always button is client-side: it appends `proposedRule` to the instructions store, then sends
`approved`. So the server never sees "always" here — which is *correct* for the tiers: the
client's Always will write `PUT /auto-review/policy` at the most specific tier (§6), never the
local store and never global. The server-side handler mirrors `resolveLocalToolPermission`:
find the suspended run by `requestId` (= tool call id), answer the aggregate exactly once, flip
the card to `approved`/`denied`, resume on approval. The client expects a resolved-vs-stale
distinction: 200 ⇒ resolved; a dead request heals the card to `expired` and returns 410 ⇒ stale.
No expiry timer anywhere, ever — the user may answer hours later.

## 6. Management API (the contract the client writes against)

Account-authed like `/local-exec/*` (Bearer-or-cookie). Scope addressing is uniform:

```
GET    /auto-review/policy
       → { "global":   { "enabled": …, "allowInstructions": …, "blockInstructions": … } | null,
           "machines":  { "<machineId>":  { …same fields… }, … },
           "coworkers": { "<coworkerId>": { …same fields… }, … } }
       Rows as stored: null field = inherits. Absent row = null. The client renders inheritance
       itself; nothing here is pre-resolved.

PUT    /auto-review/policy
       { "scopeKind": "global" | "machine" | "coworker",
         "scopeId":   "" | "<machineId>" | "<coworkerId>",
         "enabled": true | false | null,
         "allowInstructions": "<text>" | null,
         "blockInstructions": "<text>" | null }
       Upsert of the whole row (all three fields every time — null means inherit, not "keep").
       422 on unknown scopeKind, on a scopeId that isn't the account's, or global with a scopeId.
       → 204

DELETE /auto-review/policy   { "scopeKind": …, "scopeId": … }
       Remove the row entirely (full inheritance). → 204

GET    /auto-review/effective?machineId=…&coworkerId=…
       → { "enabled": bool, "allowInstructions": "…", "blockInstructions": "…",
           "decidedBy": { "enabled": "coworker|machine|global|default", … per field } }
       The resolved view + which tier decided each field — for the settings UI to show "inherited
       from …" honestly instead of re-implementing precedence.
```

Wiring: General tab → `PUT {scopeKind:"global", scopeId:""}` (in addition to its Cursor-route
`setHostSettings` write, which stays untouched for Cursor); Computer tab → `scopeKind:"machine"`;
agent settings → `scopeKind:"coworker"`.

**One example** — device tier says ask about installs, the agent tier for a trusted bot
overrides nothing but enables:

```
PUT /auto-review/policy
{ "scopeKind": "machine", "scopeId": "mac_01a056b6e5a97af39215894387c6994a",
  "enabled": true, "allowInstructions": null,
  "blockInstructions": "anything that installs software or changes system settings" }
→ 204

GET /auto-review/effective?machineId=mac_01a056…&coworkerId=cw_01a0562a-…
→ { "enabled": true, "allowInstructions": "", "blockInstructions": "anything that installs…",
    "decidedBy": { "enabled": "machine", "allowInstructions": "default",
                   "blockInstructions": "machine" } }
```

`brew install jq` from that coworker → judge says block → the tool call returns the refusal as a
result; the bot tells the user which rule stopped it.

## 7. Explicitly not in v1

- No merging of instruction texts across tiers (override only). Revisit if users ask.
- No per-rule granularity (the row is the unit). The client's "always allow" card control can
  append text to a tier; structured rule lists are a later shape.
- No enforcement on the Cursor route (the box agent already owns that; double enforcement would
  double-ask).
- No caching of judge verdicts. Every action is judged fresh (policy is enforced every time,
  non-negotiable #6).
