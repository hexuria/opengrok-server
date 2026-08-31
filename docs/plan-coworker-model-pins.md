# Per-coworker model pins

Status: investigation, not implemented. Written 31 Aug 2026 for a later
adversarial pass (fable). Do not treat this as a merge plan until that pass
has agreed the dialect, the default, and the surfaces.

**The ask.** New bots hire on `oag/auto`. A person can pin a specific bot to a
specific route (`openai/gpt-5.6-luna` for a reasoning coworker, a Gemini id for
vision/media). Neither the desktop client nor the web console can do that today.

---

## 1. What happens when you create a bot today

Three hire paths, one stored fact, one run-time read.

```
desktop "New Bot"
    → coordinator createAgent {name, description, title, avatar…}
    → POST gateway command createAgent
    → CoworkerCommand::Hire { name, model: OG_MODEL }
    → coworker_view.model
    → roster description fallback = that model
         ↘ later sendPrompt / AG-UI run
           → load_coworker → coworker.model → GatewayDoor { "model": pin }
```

| Path | File | Which model is stored |
|---|---|---|
| Desktop / gateway `createAgent` | `crates/opengrok-server/src/gateway/lifecycle.rs` `hire()` | **always** `state.agui.model` (`OG_MODEL`). `args.model` is ignored. Profile `description` is whatever the client sent (usually empty). |
| Seam B `CreateGrokBotAgent` | `crates/opengrok-server/src/seamb.rs` | same: `state.agui.model`. No model field on the client's create request (`host-gateway-api.ts` `mintAgent` sends name/description/title/avatar only). |
| REST `POST /coworkers` | `crates/opengrok-server/src/agui/routes.rs` `HireRequest` | `request.model.unwrap_or(state.model)` — **the only path that already accepts a pin**. Not used by the desktop UI or the web console. |

`OG_MODEL` is loaded once at process start (`crates/opengrok/src/main.rs`). It is
the **deployment default for new hires**, not the model a run uses.

A run reads the **stored coworker pin**:

```
crates/opengrok-server/src/agui/routes.rs ~851
WHICH MODEL A COWORKER THINKS WITH IS THE COWORKER'S, NOT THE DEPLOYMENT'S.
… if let Ok((coworker, _)) = store.load_coworker(&coworker_id) {
    model = coworker.model.clone();
}
```

That comment exists because the previous bug was the opposite: hire stored a
model, the roster showed it, and the run silently used `OG_MODEL`. `scripts/slice5-roster-smoke.sh`
is the regression. So:

- changing `OG_MODEL` and restarting changes **new** bots only;
- an already-hired bot keeps the pin it was hired with, until something writes a
  new event (there is no such event today).

The sidebar "description" you saw is not a bio. Roster summary sets
`"description": view.model` (`gateway/summaries.rs`) so a never-spoken row
survives the client's blank-agent suppression (`client-grok-bot.md` §8.2). A
real profile description overlays it only when non-empty (`gateway/live.rs`).

The web console (`web/src/routes/{login,account,admin}.tsx`) is account + admin.
It does not hire coworkers and has no model picker.

---

## 2. The id format: `gpt-5.6-luna` vs `openai/gpt-5.6-luna`

**Against open-ai-gateway, the canonical catalog id is `openai/gpt-5.6-luna`.**
Bare `gpt-5.6-luna` is the *upstream* name the gateway sends *to OpenAI*, not
the pin OpenGrok should store.

Grammar (`docs/research/gateway-open-ai-gateway.md` §4, `oag-server/src/gateway/alias.rs`):

```
[anthropic/] <provider>/<model> [@api|@sub]
[anthropic/] oag/(auto|<rung>)  [@api|@sub]
```

| String | Who understands it | Notes |
|---|---|---|
| `openai/gpt-5.6-luna` | open-ai-gateway catalog | canonical. `provider/model`. Optional `@api` / `@sub`. |
| `gpt-5.6-luna` | OpenAI itself, and the local bun stub on `:8080` | no provider segment. OAG `catalog.resolve` is by full id; a bare name is `no_viable_model` unless a row exists under that exact id. |
| `oag/auto` | open-ai-gateway only | virtual; policy classifies and picks a rung. Not in the stub's list. |
| `gemini/gemini-3-pro` | open-ai-gateway | example of the same dialect. A "3.7 flash" pin is `gemini/<catalog-id>`, never a bare Gemini name, and only if the live catalogue actually has that row. |
| `anthropic/openai/gpt-5.6-luna` | discovery alias | Claude Code's `/v1/models` filter. OpenGrok must not store this; `alias.rs` strips it on the way in. |

OpenGrok's `GatewayDoor` posts the stored string verbatim as `"model"`
(`opengrok-harness/src/gateway.rs`). There is no CopilotKit dialect prefix.
The OpenSesame rule (`openai/` as a *wire format* wrapped around a pin,
`docs/research/lessons-opensesame.md` §5b) **does not apply here** — that was
for a runtime that split on the first slash to pick an SDK. OpenGrok talks to
one OpenAI-compatible door. Store the gateway catalog id, send it unchanged.

### Why the live server currently works with the bare name

`:1447` is pointed at `OG_GATEWAY_URL=http://127.0.0.1:8080` (bun stub), not
`http://127.0.0.1:29080` (real `oag serve`). The stub's `/v1/models` on 31 Aug
2026 listed:

```
gpt-5.6-sol, gpt-5.6-terra, gpt-5.6-luna, gpt-5.5, gpt-5.4,
gpt-5.4-mini, gpt-5.3-codex-spark, xai/grok-4.6
```

No `oag/auto`, no `openai/` prefix, no Gemini. `OG_MODEL=gpt-5.6-luna` is
correct **for this stub**. It is the wrong spelling **for open-ai-gateway**.

Real `oag` is up on `127.0.0.1:29080` (`OAG_SERVER__PUBLIC_ADDR`). `/v1/models`
returned 500 with the live `oag_live_` key during this investigation — the
catalogue could not be listed from here. Treat the research doc and
`alias.rs` tests as the dialect source of truth until that listing is green.

**Do not set `OG_MODEL=oag/auto` while the door is the stub.** New hires would
store a pin the stub cannot serve, and every turn would fail. Flip the default
in the same change that points `OG_GATEWAY_URL` at real OAG.

---

## 3. What already exists vs what is missing

### Already true

- A coworker *has* a model field, stored on hire, used on every run.
- REST hire already takes an optional `model`.
- The roster already surfaces the pin (as `description` when the bio is empty).
- The prior product's policy is written down and still the right one
  (`lessons-opensesame.md` §5): identity ≠ runtime ≠ harness ≠ model route ≠
  credential; a pin that cannot be honoured refuses the run; pins are opaque
  strings the gateway owns.

### Missing

| Gap | Evidence |
|---|---|
| No `Repin` command | `CoworkerCommand` is Hire / Rename / AssignComputer / ReleaseComputer / Retire (`opengrok-core/src/coworker.rs`). Model is write-once at hire. |
| Gateway `createAgent` ignores a pin | `lifecycle.rs` always clones `state.agui.model`. |
| `updateAgent` cannot change the pin | writes name + profile (`description`, `title`, avatar). |
| No REST patch for the pin | `/coworkers` is POST hire + GET list; per-id routes are approvals and bot keys. |
| Desktop create payload has no model | `host-gateway-api.ts` `mintAgent`: name, description, title, avatarShape, avatarColor. |
| Desktop has a *host* default, not a per-bot pin | `agentDefaultModel` in `sand-settings-store.ts` / `setHostSettings`. That is the client's own runner default, not `coworker.model`. |
| Web console has no coworker surface | account + admin only. |
| No catalogue for a picker | OpenGrok does not proxy `/v1/models`. A UI that listed models would have to invent them or call the gateway with the server's `oag_live_` key — which must never go to a browser. |
| Description vs pin collision | Using the pin as the subtitle was a blank-agent defence, not a bio. A picker that writes `description` will fight this fallback. |

---

## 4. Recommended shape

Keep the five-plane rule. The pin is an operating fact on the coworker
aggregate. Credentials stay in the gateway. The client never sees a key.

### 4.1 Dialect (store and send this)

- Default hire: `oag/auto` once the door is real OAG.
- Specific OpenAI: `openai/gpt-5.6-luna` (not `gpt-5.6-luna`).
- Specific Gemini: `gemini/<catalog-id>` as listed by *this* gateway's
  `/v1/models`, confirmed against a captured response (CLAUDE.md #10). Do not
  invent `gemini-3.7-flash` without a catalogue row.
- Qualifiers `@api` / `@sub` only when the person is pinning a credential
  *kind*. Unqualified remains the product default (cheapest live credential).
- Unknown / unlistable pin: **refuse the run**, naming the pin and the fix.
  Do not fall back to `OG_MODEL` (that is the lie `slice5` exists to catch).

One normalisation function on the way in (create and repin): trim, reject
empty, optionally ask the gateway whether the id is advertised. Do not
silently prepend `openai/`. A Gemini pin that got an `openai/` prefix would
be a different request.

### 4.2 Domain

Add `CoworkerCommand::Repin { model, at_ms }` and `CoworkerEvent::Repinned`.
Same fail-closed rules as Hire: empty name/model is a 400, not a default
invented in `decide`. Projection updates `coworker_view.model`. Roster
emit follows.

Null vs default: OpenSesame used SQL null = "use deployment default". OpenGrok
currently always stores a concrete string at hire. Prefer **always store a
concrete pin** (`oag/auto` is a pin) so a later change to `OG_MODEL` cannot
silently retarget existing bots. That matches the slice5 invariant.

### 4.3 Server API

Honour `args.model` on `createAgent` and `CreateGrokBotAgent`, falling back to
`OG_MODEL` when absent or blank — the REST `HireRequest` shape, on every
path.

Accept `model` on `updateAgent` (and a REST `PATCH /coworkers/{id}` if the
console needs it) by issuing `Repin`. Ignore unknown profile keys; do not
write the pin into `seamb_profile`.

Add a **server-side** catalogue endpoint, e.g. `GET /models`, that asks the
gateway with the process key and returns the advertised ids + display names.
The browser and the desktop talk to OpenGrok, never to `oag_live_`. Cache
briefly; a stale picker is better than a leaked key. Empty list is valid
(CLAUDE.md: empty success is the dangerous reply — return `[]` and a reason,
not a 200 that looks like "there are no models in the world").

A Test/probe action belongs next to the picker: one non-streaming completion
with the candidate pin, surface the error body if the gateway refuses. That
is how the previous product proved a pin before saving it.

### 4.4 Clients

**Desktop (opengrok).** The contract is transcribed, never invented
(CLAUDE.md #1). `createAgent` / `updateAgent` today have no model field in
the client's TypeScript. Two options, pick one in the adversarial pass:

1. Extend the existing commands with an optional `model` (and have the
   renderer grow a picker). Requires a client change we compile, plus a
   provenance comment naming the file it was read-from-or-added-to.
2. Keep the desktop create flow as-is (hires on `OG_MODEL` / `oag/auto`) and
   put the picker on the **web console**, which we do own.

(2) is the smaller client-contract risk. (1) is the one people will actually
use while creating a bot in the app they already have open. The
recommendation is **both**, sequenced: server + console first (we own both
ends), desktop second once the wire field is agreed.

`agentDefaultModel` in host settings is a different plane (the client's own
runner). Do not reuse it as `coworker.model`.

**Web console.** New coworker page: list, hire (name + model, default
`oag/auto`), repin. Catalogue from `GET /models`. Show the pin as its own
field, not as `description`.

### 4.5 Description fallback

Once pins are first-class, stop using the model string as the bio. Keep a
non-empty subtitle for blank-agent suppression — a dedicated `model` on the
summary if the client will accept an extra field, or a stable non-Grok
placeholder description — but do not overwrite a person's description with
the pin, and do not let a pin picker write `description`.

Confirm against `client-grok-bot.md` §8.1 whether an unknown extra field on
the summary is preserved or stripped. If stripped, the subtitle must stay a
known field.

---

## 5. Suggested implementation order

Each step is independently shippable. Do not start 5 until 1–3 are true on
real OAG, or the default is a pin nobody can serve.

1. **Dialect + default, server only.** Honour `model` on all three hire
   paths. Default `OG_MODEL=oag/auto` **in the same change that points the
   live door at OAG**. Document both spellings in `.env.example`. Keep the
   stub working with a comment: stub ids are bare, OAG ids are
   `provider/model`.
2. **`Repin` in the aggregate**, with a test that a hired coworker answered
   on pin A, was repinned to B, and the next turn's door saw B (extend
   slice5, do not weaken it).
3. **`updateAgent` + REST PATCH** write `Repin`. Fail closed on an empty
   string. Do not invent a default in `decide`.
4. **`GET /models`** proxy. Fixture test against a stand-in catalogue.
   Capture one real OAG `/v1/models` body before claiming Gemini ids.
5. **Web console coworker page**: hire with default `oag/auto`, picker from
   `/models`, free-text route for pins the list does not show, Test probe.
6. **Desktop**: optional `model` on create/update, picker in agent settings.
   Provenance comment on the wire field. Until then, new desktop bots get
   `oag/auto` and are repinned from the console.
7. **Roster honesty**: pin is a labelled field; description is the bio;
   blank-agent defence still non-empty.

Out of scope for this slice: per-tool model overrides, effort/reasoning
level on the coworker, billing UI, package-coworker lock (no packages yet).

---

## 6. What a later pass should try to break

- Hire without `model` still stores `OG_MODEL`, never empty, never the
  previous bot's pin.
- Hire with `model: "gpt-5.6-luna"` against real OAG: does it 400, or did
  someone "helpfully" prefix `openai/`? The latter is the bug.
- Hire with `model: "oag/auto"` against the bun stub: must fail loudly.
- Repin does not change name, box, or policy. Rename does not change pin.
- A turn in flight during Repin: which pin does it finish on? Pick one and
  test it (recommendation: the pin loaded at run start, so in-flight is
  stable).
- `listAgents` after Repin: `description` must not silently become the new
  pin if a bio exists.
- Catalogue empty / gateway 500: picker shows a reason, hire still possible
  via free-text, Test probe names the error.
- `GET /models` must not forward `oag_live_` to the client. A 200 with the
  key in a header or body is a ship-blocker.
- Existing bots hired on `gpt-5.6-terra` / `gpt-5.6-luna` keep that pin
  after `OG_MODEL` changes. A migration to `openai/…` is a separate,
  explicit rewrite, not a restart side-effect.

---

## 7. Current live state (this machine, 31 Aug 2026)

Recorded so the next session does not re-derive it:

- `:1447` `./target/debug/opengrok`, `OG_MODEL_DOOR=gateway`,
  `OG_GATEWAY_URL=http://127.0.0.1:8080`, `OG_MODEL=gpt-5.6-luna`
  (restarted this session from `gpt-5.6-terra`).
- Code fallback and `.env.example` were pointed at `gpt-5.6-luna` in the
  same session. That matches the stub, not the intended product default.
- Real `oag serve` is on `127.0.0.1:29080`; listing models with the live
  key returned 500. Do not claim the live OAG catalogue contents from this
  investigation.
- Committed research already names the dialect: `xai/grok-4.6@sub`,
  `oag/auto`, `oag/cheap` (`CLAUDE.md` #4, `gateway-open-ai-gateway.md` §4).
