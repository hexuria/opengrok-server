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

## Slice 7 — Bot ↔ coworker binding

Runs from a client Bot arrive anonymous today: no tools, no policy, the deployment's model.
Access tokens live one hour, so a Bot registered with a static header dies hourly.

- [ ] **7.1** `POST /coworkers/{id}/keys`: a durable, revocable bot-key (signed, names account +
  coworker; a `key_view` row makes revocation real).
- [ ] **7.2** `account_from_bearer` accepts it; a bot-key names the coworker for the run.
- [ ] **7.3** Proven from barok-works: a Bot registered with the key runs as its coworker — tools,
  approvals, and the coworker's own model, from the UI instead of curl.

## Slice 8 — Seam B + tonic

Operator decision 30 Aug 2026: scheduled, not deferred.

- [ ] **8.1** Hand-transcribe the P1 command-table messages into `opengrok-proto` — provenance
  comment per message, **no vendored stubs** (LEGAL.md stands).
- [ ] **8.2** Connect-style unary (POST + JSON over HTTP/1.1) on the existing Axum listener — the
  transport the desktop client actually speaks.
- [ ] **8.3** The same services on a tonic gRPC listener for internal use.

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
