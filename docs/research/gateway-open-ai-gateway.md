# open-ai-gateway (OAG) — OpenGrok's model door

**Researched:** 29 Aug 2026, against `/Volumes/goldcoders/OSS/open-ai-gateway` at `main` = `fa87b6a`
(*fix(proto): mint unique Responses item ids per response (#46)*), working tree clean apart from an
untracked `.agent-mail/`.

**Role in OpenGrok:** every model call `crates/opengrok-harness` makes exits through OAG. OAG owns
provider credentials, cost routing, failover, prompt-cache affinity, and the usage ledger; OpenGrok
owns the coworker, the transcript, and the policy over tools. The two are intended to ship as one
product, and OpenGrok's own `Cargo.toml` already names `crates/opengrok` as *"the binary: wires the
server, embeds the gateway"*.

**Verdict up front:** embedding is cheap — `oag_server::public_router(state) -> axum::Router` is a
fully-wired, state-erased router that OpenGrok can `merge`/`nest` into its own axum app. The costs
are a shared Postgres + Redis, a pinned Rust 1.95 toolchain, and one global metrics recorder.
Everything else is a path dependency.

---

## 1. What it is, and the crate layout

An internal AI gateway: one HTTP door for every model call, which routes each request to the
cheapest rung that can do the job, pools and rotates the organisation's own credentials, translates
between four wire dialects, and records what each request cost *and* what it would have cost on the
route's top tier. `README.md` is explicit that it is **not a resale product** — own credentials, own
members.

### Crates

Source: `Cargo.toml`, `README.md` §Shape.

| Crate | Owns | I/O? |
|---|---|---|
| `crates/oag-core` | Domain types, typed ids (`AccountId`, `RouteId`, `ApiKeyId`, `PrincipalId`, `ServiceId`, `RequestId`), `Provider`, `CredentialKind`, `Tier`/`TierName`, `Error`, typed `Config`, credential sealing (`Kek`, XChaCha20-Poly1305) | none |
| `crates/oag-router` | Model catalog (`Catalog`, `ModelSpec`, `Pricing`), request classifier, `TierLadder`/`Rung`, `RoutingPolicy::decide`, budgets & `BudgetPressure`, `Entitlement`, cost arithmetic | none |
| `crates/oag-proto` | The translation hub. Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, Gemini — parse in, render out, through one `CanonicalRequest` / `StreamEvent` form | none |
| `crates/oag-pool` | Credential scheduler, session affinity (`SessionKey`), circuit breakers | none |
| `crates/oag-upstream` | Provider adapters (Anthropic, Bedrock+SigV4, Gemini, one OpenAI-compat adapter for OpenAI/Kimi/DeepSeek/Zhipu/xAI, Codex subscription, xAI OAuth), `reqwest` transport pool, provider usage pollers, price sync | HTTP |
| `crates/oag-store` | Postgres (`sqlx`) and Redis. `repo` queries, three-tier `AuthCache`, `Cache` (rate tokens, auth L2, sticky sessions, refresh locks), readiness | DB |
| `crates/oag-server` | axum: the two listeners, inference pipeline, admin API, health, metrics, drain/shutdown, embedded dashboard | HTTP |
| `crates/oag` | The `oag` binary: clap CLI (`serve`, `migrate`, `config`, `admin …`), config loading, the operator surface | — |

**Dependency direction** (from each crate's `Cargo.toml`) is a strict DAG, no cycles:

```
oag-core  ←  everything
oag-router      → oag-core
oag-proto       → oag-core, oag-router
oag-pool        → oag-core
oag-upstream    → oag-core, oag-proto, oag-router
oag-store       → oag-core, oag-pool, oag-router, oag-proto
oag-server      → oag-core, oag-pool, oag-proto, oag-router, oag-store, oag-upstream
oag (bin)       → oag-core, oag-pool, oag-router, oag-server, oag-store, oag-upstream
```

Four crates (`core`, `router`, `proto`, `pool`) do no I/O at all — the stated structural bet, and
the reason the fast test suite exists (README claims 226 tests in those four, 495 workspace-wide;
not re-counted here).

### Workspace facts

Source: `Cargo.toml`, `rust-toolchain.toml`, `clippy.toml`.

| Thing | Value |
|---|---|
| Edition | **2024**, `resolver = "3"` |
| Toolchain | pinned `channel = "1.95"` with `rustfmt`, `clippy` (`rust-toolchain.toml`) |
| Version / licence / publish | `0.1.0`, MIT, **`publish = false` workspace-wide** |
| axum | `0.8` (features `macros`) |
| tokio | `1` (`rt-multi-thread`, `macros`, `signal`, `sync`, `time`) |
| sqlx | `0.9`, `default-features = false`, features `postgres uuid time json rust_decimal runtime-tokio tls-rustls-ring migrate macros` |
| reqwest | `0.12`, `default-features = false`, `rustls-tls http2 stream json charset` — **rustls only, no OpenSSL/BoringSSL** |
| serde / serde_json | `1` / `1` with `preserve_order` |
| Other notables | `tower 0.5`, `tower-http 0.6`, `http 1`, `redis 1`, `moka 0.12`, `rust_decimal 1` (money is never `f64`), `uuid 1` (v4/v5/v7), `time 0.3`, `serde_yaml_ng 0.10`, `thiserror 2`, `clap 4`, `metrics 0.24` + `metrics-exporter-prometheus 0.18`, `chacha20poly1305`, `hmac`/`sha2`/`hex` (SigV4 by hand, no AWS SDK) |
| `oag-server` extras | `hyper-util 0.1` (`server-auto`, `service`, `tokio`) — needed to apply `header_read_timeout`/`idle_timeout`, which `axum::serve` hides; `bytes 1`, `tokio-stream 0.1` |
| Release profile | `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"` |

**Lint policy** (`[workspace.lints]` in `Cargo.toml`, inherited by every crate via `[lints] workspace = true`):

```
rust:    unsafe_code = forbid; missing_debug_implementations = warn
clippy:  all = deny (priority -1); pedantic = warn (priority -2)
         unwrap_used = deny; panic = deny; expect_used = warn; todo = warn
         must_use_candidate / missing_errors_doc / missing_panics_doc /
         module_name_repetitions = allow
```

Rationale in-file: *"Nothing in a gateway should take the process down. A panic in the request path
is a 500 for every in-flight stream on that replica."* Test modules opt out per-crate with
`#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]`
(`crates/oag-server/src/lib.rs:1`, `crates/oag/src/main.rs:1`). `clippy.toml` only sets
`doc-valid-idents`. CI gate is `just check` = `fmt-check` + `clippy --workspace --all-targets -D warnings` + `cargo test --workspace`.

---

## 2. The two listeners

Source: `crates/oag-server/src/lib.rs` (`public_router`, `admin_router`, `serve`),
`crates/oag-core/src/config.rs` (`ServerConfig`), `justfile`, `README.md`.

| | Inference listener | Admin listener |
|---|---|---|
| Config field | `server.public_addr` | `server.admin_addr` |
| Default bind | `0.0.0.0:8080` | `127.0.0.1:8081` (loopback on purpose) |
| Local dev (`just serve`) | `127.0.0.1:29080` | `127.0.0.1:29081` |
| Serves | `/v1/messages`, `/v1/messages/count_tokens`, `/v1/chat/completions` (+ unversioned), `/v1/responses` (+ unversioned), `/v1beta/models/{model}:action`, `/v1/models`, `/models`, `/v1beta/models`, `/health/live` | `/` (dashboard), `/health/ready`, `/metrics`, `/health/live`, `/admin/api/*` |
| Auth | every inference route sits behind `gateway::require_key_layer`, applied as a **`route_layer`** so an unmatched path 404s without a DB hit and an anonymous POST is refused *before* its body is buffered | `/admin/api/*` behind `admin::require_admin_layer`; `/`, `/metrics`, `/health/ready` are deliberately **outside** the layer |
| Rationale | *"Splitting them makes 'do not expose the admin API' a deployment fact rather than a routing rule someone has to remember."* (`lib.rs:12-15`) | |

Why local ports are 29080/29081 (`justfile:16-27`): 8080/8081 collide with everything, and both
chosen ports sit below the kernel ephemeral range (macOS 49152+, Linux 32768+) so they cannot be
claimed out from under you. `just serve` walks upward to the first free *pair* and prints it;
`just ports` shows it in advance. Containers still listen on 8080/8081 internally.

### `single_listener`

`server.single_listener: bool` (default `false`) merges **all** of `admin_routes` onto the public
listener, for platforms that route to exactly one container port (Cloud Run, Azure Container Apps).
The cost is asymmetric and documented in the config comment and asserted in a test
(`lib.rs:613-647`): `/admin/api` keeps its admin key; `/`, `/metrics` and `/health/ready` never had
one, so on a shared port they are **unauthenticated**. `serve()` logs a `warn!` when this is on.

### Configuration mechanism

`crates/oag/src/settings.rs`: a YAML file (`--config` / `OAG_CONFIG`, **optional**) plus environment
overrides named `OAG_<SECTION>__<FIELD>` — double underscore for nesting, e.g.
`OAG_SERVER__PUBLIC_ADDR`, `OAG_DATABASE__URL`. Overrides are applied to the YAML document before
deserialisation; scalars are parsed to their narrowest type (`bool` → `i64` → string). Only the six
known sections (`server database redis security gateway telemetry`) are addressable; unrelated
`OAG_*` variables are **ignored, not rejected** (an `OAG_ACCOUNT_SECRET` in the operator's shell once
stopped the binary booting). The file itself is parsed with `deny_unknown_fields`.

Required config with no default: `database.url`, `redis.url`, `security.signing_secret` (≥32 bytes,
rejected if it contains `change`/`example`), `security.credential_kek` (base64, exactly 32 bytes,
parsed at config load so every subcommand fails identically). `telemetry.otlp_endpoint` is
**rejected** at startup — no OTLP exporter is linked in this build.

Notable gateway knobs and defaults (`GatewayConfig::default`): `stream_idle_timeout` 180s,
`stream_keepalive_interval` 10s, `max_stream_duration` 1800s (also the shutdown drain budget),
`same_account_retries` 2, `max_account_switches` 3, `catalog_refresh_interval` 60s,
`usage_poll_interval` 300s, `bedrock_region` `us-east-1`, `provider_base_urls` (per-provider base URL
override — this is the mock/self-host hook), `codex.*`, `claude_code_model_aliases: false`.
Server: `header_read_timeout` 10s, `idle_timeout` 120s, `max_body_bytes` 32 MiB.

---

## 3. Auth

### Key format

`crates/oag/src/admin/mod.rs:1122-1135`:

```
oag_live_<64 hex chars>          # "oag_live_" + 32 bytes of entropy, hex-encoded
```

Stored as `sha256(key)` hex in `api_key.key_hash` (`crates/oag-store/src/repo.rs:19` `hash_key`);
the first 16 characters (`oag_live_<8 hex>`) are stored as `api_key.key_prefix` for grepping and for
`oag admin key revoke <PREFIX>`. The plaintext is printed once and is unrecoverable
(`print_key`: *"Only its SHA-256 is stored"*). sub2api's plaintext-key mistake is called out in the
`hash_key` doc comment.

### Headers accepted

`crates/oag-server/src/gateway/mod.rs:1326-1338`, in this precedence order:

1. `Authorization: Bearer <key>` — wins if several are sent
2. `x-api-key: <key>`
3. `x-goog-api-key: <key>`

One extractor, used by **both** the inference layer and the admin layer. Consequence noted in
`gateway/models.rs:346-348`: auth headers carry no dialect signal, which is why `/v1/models` emits a
superset of the OpenAI and Anthropic model-object shapes.

### Resolution and caching

`crates/oag-store/src/auth.rs` — three tiers: L1 moka in-process (TTL **15s**, 10 000 entries), L2
Redis (TTL **5 min**), Postgres as truth. Misses are cached too (negative caching, so a key-scan
cannot amplify against Postgres), loads are single-flighted through `moka::try_get_with`, and an L2
entry is believed **only if it carries an HMAC tag** derived from `security.signing_secret`
(`AuthMac`) — Redis is network-reachable and a planted entry must not pass for an identity. L1 is
populated only from a verified L2 entry or from Postgres. Revocation calls `invalidate_hash`, which
drops L1 on this replica and DELs the Redis entry; other replicas' L1 entries expire within 15s.

### What a key maps to

One query (`repo::authenticate`, `repo.rs:33`) joins `api_key → principal → route`, requiring
`k.active AND p.active AND r.active` and `expires_at` in the future, and yields `AuthContext`:

| Field | Meaning |
|---|---|
| `api_key_id` | also the session-affinity salt (`SessionKey::resolve`) |
| `principal_id` | the org member; carries `monthly_budget_usd`, `hard_stop_multiple`, month-to-date spend |
| `route_id` | **the** binding — the route owns the tier ladder, `default_mode`, `floor_tier`, `rpm_limit`, `monthly_budget_usd` |
| `key_floor_tier` | per-key floor: never route below this rung |
| `quota_usd` / `spent_usd` | per-key budget |
| `admin` | `api_key.admin` — the admin-listener gate |

**Authority for the admin API is a property of the key, not the principal**
(`crates/oag-server/src/admin/auth.rs`). `require_admin_layer` checks `ctx.admin` first (403 with a
message naming `oag admin key create --admin`), then looks up `principal.role = 'admin'` (403), then
injects an `AdminActor { principal_id, email }` recorded on every mutation. An inference key on the
dashboard is a **403**, not a 401 — the single most common first failure per `docs/08-clients.md`.

Route→credential binding is `account_route`, with per-principal ownership:
`repo::route_channels` / `route_channel_status` scope on
`a.owner_principal_id IS NULL OR a.owner_principal_id = $2` (`repo.rs:225-246`), so a personal
subscription seat is visible only to its owner.

---

## 4. The model-pin dialect

The one place an inbound model string is interpreted: `crates/oag-server/src/gateway/alias.rs`,
called from `gateway::handle` (`mod.rs:233`) before routing. Two decorations, composable, and the
qualifier is stripped **before** the prefix so the catalog lookup in the middle sees a bare id.

### Grammar

```
[anthropic/] <provider>/<model> [@api|@sub]
[anthropic/] oag/(auto|<rung>)  [@api|@sub]
```

| Form | Meaning | Evidence |
|---|---|---|
| `xai/grok-4.6` | canonical catalog id, `<provider>/<model>`; the router picks the cheapest live credential | `Catalog::resolve` |
| `xai/grok-4.6@api` | pin to a metered API-key credential | `CredentialKind::ApiKey.qualifier() == "api"` |
| `xai/grok-4.6@sub` | pin to a subscription (OAuth) credential | `CredentialKind::OAuth.qualifier() == "sub"` |
| `anthropic/xai/grok-4.6@sub` | discovery-alias twin of the above; resolves to `xai/grok-4.6` on a subscription | `alias.rs` tests |
| `oag/auto` | virtual: let policy classify and choose the rung | `virtual_tier` returns `None` for `auto` |
| `oag/cheap`, `oag/frontier`, `oag/<rung>` | virtual: pin a named rung of **this route's** ladder | `virtual_tier` (`mod.rs:575`) |

Only **`@api` and `@sub`** exist (`CredentialKind::QUALIFIED`). Bedrock, Vertex and service-account
are different upstreams, and *"a different upstream is a different provider with an id of its own"* —
so `Cursor`-resold Gemini would be `cursor/gemini-flash`, not a qualifier. Separator is `@` because
`:` is already spent on the Gemini action delimiter and `/` on the provider separator; `@` is an RFC
3986 sub-delim, legal unescaped in a path segment, and a client that percent-encodes it anyway
arrives decoded (asserted in `lib.rs:649-705`).

### The `oag/*` ladder is not a fixed trio

`oag/cheap` / `oag/frontier` are only conventional names. `RoutingPolicy::virtual_names`
(`crates/oag-router/src/policy.rs:505`) derives the advertised virtual names **from the route's own
ladder**, clamped below by the key's floor tier and above by budget pressure
(`Constrained` clamps to the floor rung; `Exhausted` returns none). A route with rungs
`budget`/`premium` advertises `oag/auto`, `oag/budget`, `oag/premium`.

### Unknown / off-ladder / bad names

| Input | Behaviour | Evidence |
|---|---|---|
| Unknown model, no prefix | left intact; `decide()` fails with `Error::NoViableModel` → **400 `no_viable_model`**, message naming the route, the requested name and the fixing command | `alias.rs` `canonicalise`, `mod.rs:1530` `no_viable_message` |
| `anthropic/<junk>` | prefix is **not** stripped when the remainder resolves to nothing — stays unknown rather than becoming a different request | `a_model_that_resolves_to_nothing_stays_unknown` |
| `anthropic/claude-opus-5` (a real id that *looks* like an alias) | full string is resolved first, so it is not mistaken for an alias of `claude-opus-5` | `canonicalise` ordering |
| `@bogus`, `@oauth`, `@subscription`, `@api_key`, `@` | **400 `invalid_model_qualifier`** (`Error::UnknownModelChannel`), message listing `@api` and `@sub`. Deliberately not silently dropped: a dropped pin routes to the very credential the caller excluded | `alias.rs:163-167` |
| `gemini/gemini-3-pro@sub` (provider offers no subscription path) | **400 `invalid_model_qualifier`** (`Error::ChannelNotOffered`) — refused here rather than surfacing as "no credential", which reads as *add one* when there is nothing to add | `alias.rs:127-138` |
| Off-ladder catalog id in `passthrough` mode | **honoured** by `decide()`, and served — but see §7, it is only *advertised* when the route holds a metered credential for that provider | `models.rs::on_offer` |
| `x-oag-tier: <unknown rung>` | not an error: logged `warn!` and dropped back to classification, because mapping a typo to a rung would silently pin the cheapest one | `mod.rs:613-627` |

An id genuinely containing `@` wins over the split (`catalog.resolve(model)` is tried first,
`alias.rs:160`).

### Response headers

`oag_headers` (`crates/oag-server/src/gateway/mod.rs:839-851`), applied to both JSON and streamed
responses:

| Header | Value |
|---|---|
| `x-oag-model` | the **canonical** id that served the request (`decision.model.id`) — the same string the ledger records, so one model's spend never splits across spellings |
| `x-oag-request-id` | the `RequestId` (UUIDv7); the ledger's idempotency key and the lookup key for `GET /admin/api/usage` |
| `x-oag-tier` | the rung the model came from. **Omitted** when the model sits on no rung (passthrough of an off-ladder id) — it is not `cheap` |

Streamed responses additionally carry `content-type: text/event-stream`, `cache-control: no-cache`,
and `x-accel-buffering: no` (`mod.rs:1164-1173`).

`x-oag-token-count: estimate` is set by `/v1/messages/count_tokens`
(`gateway/count_tokens.rs:35,51`) — no tokeniser is linked, so the count is an estimate and the body
also carries `"oag_estimate": true`.

**Request** direction: `x-oag-tier: <rung>` asks for a rung and **outranks the body's model name**;
it forces managed handling even on a passthrough route (`mod.rs:608-640`).

---

## 5. Endpoints served

### Inference listener (`inference_routes`, `crates/oag-server/src/lib.rs:93-135`)

All POSTs are authenticated from the request head before the body is read.

| Method | Path | Purpose |
|---|---|---|
| POST | `/v1/messages` | Anthropic Messages. `Dialect::AnthropicMessages` |
| POST | `/v1/messages/count_tokens` | Prompt-size preflight, no upstream spend. Inner body limit 4 MiB (overrides `max_body_bytes` for this route) |
| POST | `/v1/chat/completions` | OpenAI Chat Completions |
| POST | `/chat/completions` | Same, unversioned — SDKs disagree about whether a custom base URL keeps `/v1` |
| POST | `/v1/responses` | OpenAI Responses (what current OpenAI SDKs default to) |
| POST | `/responses` | Same, unversioned |
| POST | `/v1beta/models/{*model_action}` | Gemini. Wildcard capture; split on the **last** `:`. Actions: `generateContent`, `streamGenerateContent`, `countTokens` (routed to the free preflight); anything else → **404 `not_found`** (`Error::UnsupportedAction`). Previously every action fell through to a billed completion |
| GET | `/v1/models` | Per-caller discovery + the `oag` diagnostics envelope (§6) |
| GET | `/models` | Same handler, unversioned |
| GET | `/v1beta/models` | Gemini-shaped discovery (`{ "models": [...] }`, `name`/`displayName`/`inputTokenLimit`/`outputTokenLimit`/`supportedGenerationMethods`). No discovery aliases here |
| GET | `/health/live` | Liveness. **Outside** the key layer — never checks dependencies, so a DB outage does not become a fleet-wide crash loop |

`/v1/models` accepts `?claude_code=1|0|true|false|yes|no|on|off` to force the Claude Code alias twins
on or off for one call, overriding `gateway.claude_code_model_aliases`. An unparseable value falls
back to the configured default rather than 400ing.

### Admin listener (`admin_routes`, `crates/oag-server/src/lib.rs:138-184`)

| Method | Path | Purpose | Key? |
|---|---|---|---|
| GET | `/` | The dashboard — one self-contained HTML file, `include_str!("../../../../web/index.html")` | no |
| GET | `/health/ready` | Postgres + Redis reachability; 503 while draining | no |
| GET | `/metrics` | Prometheus text, rendered from the process recorder | no |
| GET | `/health/live` | Liveness | no |
| GET | `/admin/api/summary` | Headline spend vs counterfactual, plus per-seat subscription economics | yes |
| GET | `/admin/api/accounts` | Upstream credentials and their live state | yes |
| GET | `/admin/api/routes` | Routes, ladders, modes, budgets | yes |
| GET | `/admin/api/usage` | The ledger, paged (`Query<Page>`); carries `selection_reason` | yes |
| GET | `/admin/api/keys` | Inbound keys by prefix — never the secret | yes |
| GET | `/admin/api/providers` | The provider support matrix the gateway derives from itself | yes |
| GET / POST | `/admin/api/services` | The capability-service catalog (sandboxes, guards, reducers) | yes |
| PATCH | `/admin/api/services/{id}` | Edit a service | yes |
| POST | `/admin/api/services/{id}/disable` · `/enable` · `/check` | Service lifecycle and health probe | yes |
| POST | `/admin/api/catalog/reload` | Reload the catalog **on this replica only** — fleet-wide is the periodic refresh | yes |
| GET | `/admin/api/models` | The catalog | yes |
| PATCH | `/admin/api/models/{*id}` | Edit a model (e.g. `display_label`). **Wildcard** because a catalog id contains a slash | yes |
| POST | `/admin/api/accounts/{id}/disable` · `/enable` · `/clear-cooldown` | Credential rotation controls | yes |
| POST | `/admin/api/keys/{id}/revoke` | Revoke by id; also evicts the shared auth cache | yes |

Route coverage is asserted by a hardcoded list in `lib.rs:351-397` — deliberately not derived from
the router under test.

### Error envelope

`gateway::error_response` (`mod.rs:1398+`) returns
`{"type":"error","error":{"type":<kind>,"message":<msg>}}` with these mappings:

| `error.type` | Status |
|---|---|
| `authentication_error` | 401 |
| `budget_exhausted` | 402 |
| `invalid_model_qualifier`, `no_viable_model`, `unsupported_field`, `invalid_request` | 400 |
| `not_found` (unsupported Gemini action) | 404 |
| `rate_limit_error` | 429 + `Retry-After` |
| `no_credential`, `no_credential_of_kind`, `quota_reserve_held`, `at_capacity` | 503 |
| `stream_idle` | 504 |
| `upstream_error` | remapped — see below |
| `internal_error` | 500, detail never surfaced |

An upstream error nests the provider's body as a **value** at `error.upstream` with
`error.upstream_status`, rather than JSON-encoded into `error.message`. Upstream statuses are
remapped at our edge (`client_status_for`) so that a provider's 401 does not tell the client its own
gateway key is wrong. A forwarded upstream 429 also gets a `Retry-After` (default 1s), rounded up and
never zero.

---

## 6. The `/v1/models` diagnostics envelope

Added by `938bd47` *feat(models): explain why a provider is absent from /v1/models (#43)*.
Implementation: `crates/oag-server/src/gateway/models.rs` (`envelope`, `list`) and
`crates/oag-server/src/gateway/presence.rs`. **Verified against the code at `main`.**

### Full body shape

```jsonc
{
  "object": "list",
  "has_more": false,
  "first_id": "oag/auto",     // null when data is empty — never indexed into
  "last_id": "xai/grok-4.6",  // null when data is empty
  "data": [ /* model objects, see below */ ],
  "oag": {
    "mode": "managed" | "passthrough",
    "claude_code_aliases": true | false,
    "budget": { "pressure": "normal" | "constrained" | "exhausted" },
    "providers": [ /* one object per provider, see below */ ]
  }
}
```

`oag.providers` is **always present, including when `data` is empty** — that is the case it exists
for. It is additive: `object`, `has_more`, `first_id`, `last_id`, `data`, `oag.mode`,
`oag.claude_code_aliases` are unchanged, so an SDK that never learned the field still parses.

### `data[]` element

`entry()` / `concrete_entry()` / `virtual_entry()`, `models.rs:349-436`. A **superset** of the
OpenAI and Anthropic model shapes, because the caller's SDK is not knowable from the request. Every
element — virtual or concrete, canonical or aliased — carries the **identical key set** (asserted in
`a_virtual_entry_has_the_same_shape_as_a_concrete_one`; virtual entries sort first and a thin one
would fail SDK validation on element 0).

```jsonc
{
  "id": "xai/grok-4.6@sub",
  "object": "model", "created": 0, "owned_by": "xai",          // OpenAI shape
  "type": "model", "display_name": "xAI: grok-4.6 · subscription",
  "created_at": "1970-01-01T00:00:00Z",                        // Anthropic shape
  "oag": {
    "tier": "frontier" | null,          // null for oag/auto and off-ladder models
    "provider": "xai",                  // "oag" for virtual names
    "virtual": false,
    "honoured": true,                   // would decide() serve this named model?
    "context_window": 200000,           // null on virtual entries
    "max_output_tokens": 64000,         // null on virtual entries
    "capabilities": { "vision": true, "tools": true,
                      "reasoning": true, "prompt_cache": true },  // null on virtual
    "channel": "sub" | "api" | null,    // the credential kind this id pins
    "alias_of": "xai/grok-4.6" | null   // canonical id, for dedupe
  }
}
```

**No pricing is emitted anywhere in `data`** — asserted by `no_entry_carries_pricing`. Cost data
stays on the admin listener.

### `oag.providers[]` element

`ProviderPresence::to_json`, `presence.rs:71-83`:

| Field | Type | Source |
|---|---|---|
| `provider` | string | `Provider::as_str()` — `anthropic`, `openai`, `gemini`, `kimi`, `deepseek`, `zhipu`, `xai`, `bedrock`, `vertex` |
| `serving` | bool | any credential in the group would pass `route_channels`, **and** the caller is not budget-exhausted |
| `reason` | string, **closed enum** | see below |
| `until` | RFC 3339 string or `null` | `rate_limited_until` for `rate_limited`; `window_resets_at` for `reserved`/`quota_spent`; `null` otherwise, and `null` when the timestamp is already in the past |
| `remaining_pct` | number or `null` | `account.usage_remaining_pct` (Decimal → f64). `null` for a provider with no usage API |
| `reserve_pct` | number or `null` | `account.usage_reserve_pct` (i16 → f64) |
| `kinds` | array of string | sorted set of `api`, `sub`, `bedrock`, `vertex`, `service_account`. Unknown `account.kind` values are dropped, not surfaced |
| `models` | integer | how many models this provider contributes to `data` — **0 whenever `serving` is false** |

`reason` is a closed enum, `PresenceReason` (`presence.rs:26-55`) — *"Never free text, never a
secret, never an upstream error string"*:

| Value | Meaning |
|---|---|
| `serving` | at least one scoped credential would pass `route_channels` |
| `reserved` | remaining allowance is **above zero and at or below** the operator reserve |
| `rate_limited` | provider `Retry-After` (`rate_limited_until`) is still in the future |
| `quota_spent` | remaining allowance is zero or below, reserve or not |
| `disabled` | operator set `schedulable = false` |
| `no_credential` | the route names this provider but this principal holds no credential |
| `budget_exhausted` | the **caller** cannot spend; the seats themselves may be healthy |

### Aggregation rules (worth knowing before matching on it)

- One row per provider, several seats collapsed (`summarise`). If **any** seat would serve, the
  provider serves — and it reports the *best* remaining allowance among the serving seats.
- If none would serve, the reason is the **most recoverable**, ordered by enum declaration:
  `reserved` < `rate_limited` < `quota_spent` < `disabled` < `no_credential` — *"so a status panel
  names the thing that will move first."*
- `BudgetPressure::Exhausted` forces `serving = false` and rewrites `serving` → `budget_exhausted`,
  but **does not** overwrite a seat-level `reserved`/`quota_spent`: waiting for Thursday and raising
  the quota are different sentences (`an_exhausted_budget_does_not_hide_a_reserved_reason`).
- Classification order (`classify`): disabled → rate-limited → spent → reserved → serving. Zero
  remaining with a reserve set is `quota_spent`, not `reserved`.
- An unparseable `account.provider` is **dropped** from diagnostics with a `warn!`, not listed.
- Operator account names (`grok-seat`, `mock`) never reach the wire — asserted.
- Diagnostics use the **same owner scope** as the picker query (`route_channel_status`, `repo.rs:225`)
  but **without** the serving filters, so a reserved or rate-limited seat still appears.
- A failure loading the status rows **refuses the whole listing** rather than returning an
  unexplained picker (`models.rs:105-111`).

### When `data` is empty

`advertise(pressure, concrete.len())` = `pressure != Exhausted && concrete > 0`. So an exhausted key,
route, or principal budget, *or* zero reachable models, empties `data` — **including the `oag/*`
virtual names**, because `oag/auto` on a hard-stopped key is a 402 on the first turn. The envelope is
how a client tells "raise the quota" (`budget.pressure = exhausted`) from "wait for the window"
(`providers[].reason = quota_spent` with an `until`).

---

## 7. Recent relevant fixes (all on `main`)

Four commits from 29 Aug 2026, in order. Each is confirmed present in the working tree.

### `938bd47` — feat(models): explain why a provider is absent from /v1/models (#43)

Added `crates/oag-server/src/gateway/presence.rs` and the `oag.budget` / `oag.providers` envelope
above. **Why it matters:** the filtering was already correct — an inference key just had no way to
tell the causes apart, since `/admin/api` is 403 for it and empty `data` is empty `data`. For
OpenGrok this is the surface to render "why can't I pick a model", and the closed `reason` enum makes
an exhaustive `match` in Rust safe.

### `5b3c142` — fix(proto): open a Responses reasoning item before its deltas (#44)

`crates/oag-proto/src/responses.rs`, +175/−2. Chat→Responses translation emitted
`response.reasoning_summary_text.delta` for `item_id: rs_0` without ever having sent
`response.output_item.added` (`type: "reasoning"`) or `response.reasoning_summary_part.added`. The
*message* item already had that lifecycle; reasoning skipped it. The Vercel AI SDK maps
`reasoning_summary_part.added` → `reasoning-start`, so **every reasoning model on `/v1/responses`
aborted** with `reasoning part rs_0:0 not found`.

Fix (`RenderState::open_reasoning` / `close_reasoning`, `responses.rs:604-679`): open the reasoning
item on the first `ThinkingDelta`, emitting `output_item.added` then `reasoning_summary_part.added`
(`summary_index: 0`, empty part); close it before the next output item (message or tool) and on
stop/fail, emitting `reasoning_summary_text.done`, `reasoning_summary_part.done`,
`output_item.done`. Ordering is asserted frame-by-frame in `responses.rs:1393-1435`.

**Why it matters to OpenGrok:** if `opengrok-harness` streams through `/v1/responses` (or ships an
AI-SDK-based client), reasoning models were unusable before this.

### `076db83` — fix(models): a subscription seat is not the API catalog (#45)

`crates/oag-server/src/gateway/models.rs`, +66/−3, plus a docs edit. In passthrough mode the listing
dumped **every catalog id** for any provider the route could reach. Correct for a metered API key —
you really can call whatever the provider sells. Wrong for a subscription seat: one OAuth `grok-seat`
plus LiteLLM's 46 `xai/*` rows made the picker read as **forty-six Grok models**.

Fix is `on_offer` (`models.rs:446-453`) applied through `offered()`:

```rust
fn on_offer(e: &Entitlement<'_>, channels: &Channels) -> bool {
    if e.tier.is_some() { return true; }               // ladder models always list
    channels.get(&e.spec.provider)
        .is_some_and(|kinds| kinds.iter().copied().any(|k| !k.flat_rate()))
}
```

`CredentialKind::flat_rate()` is `true` only for `OAuth`. So an **off-ladder** catalog name lists only
when the route holds a **non-flat-rate (metered) credential** for that provider. Ladder models the
seat can reach still list, and `decide()` still honours an off-ladder name someone typed —
**this changed advertisement, not routing.** Holding both an API key and a seat re-enables the
catalog.

**Why it matters:** OpenGrok's model picker (and any `/v1/models`-driven UI) is now a short,
honest list. Do not treat `/v1/models` as the set of servable names — it is the set of *advertised*
names.

### `fa87b6a` — fix(proto): mint unique Responses item ids per response (#46)

`crates/oag-proto/src/responses.rs`, +42/−8. Translated `/v1/responses` streams reused `msg_1` and
`rs_0` on **every** turn. Clients that key transcript entries by item id (the commit names CopilotKit
and OpenSesame) merged every new reply into the first assistant bubble.

Fix (`RenderState::item_id`, `responses.rs:573-581`): item ids now carry the request id.
`self.id` is `resp_{request_id}`; ids are minted as

```
{kind}_{request_id}_{index}      // kind ∈ { msg, rs, fc }
```

Applied at all four sites: `open_reasoning` (`rs_`), the message opener (`msg_`), `render_tool_start`
(`fc_`), and the non-streaming `render_response` (`msg_{request_id}_0`, `fc_{request_id}_{n}`). Chat
Completions already used `chatcmpl-{request_id}` and is unchanged. Asserted by
`item_ids_do_not_repeat_across_responses`.

**Why it matters:** OpenGrok's transcript will key on these ids. Without the fix, every turn in a
session collides.

---

## 8. Embedding OAG into the OpenGrok binary

### Can another Rust workspace depend on these crates?

**Yes, by path only.** Every crate is `publish.workspace = true` → `publish = false`
(`Cargo.toml:[workspace.package] publish = false`), and `repository = ""`. Nothing is on crates.io,
and `README.md` §"On the name" says the name is trademark-adjacent and should be reconsidered before
that changes. So OpenGrok depends on them as path deps (or a git dep on the repo), e.g.

```toml
# opengrok/Cargo.toml
[workspace.dependencies]
oag-server = { path = "../open-ai-gateway/crates/oag-server" }
oag-store  = { path = "../open-ai-gateway/crates/oag-store" }
oag-core   = { path = "../open-ai-gateway/crates/oag-core" }
```

or vendor the whole `crates/` tree into the OpenGrok workspace as extra `members`.

### Is `oag-server` usable as a library?

**Yes — this is the key fact.** `crates/oag-server/src/lib.rs` is a normal library with a public
surface:

```rust
pub fn public_router(state: Arc<AppState>) -> axum::Router;   // state already applied
pub fn admin_router(state: Arc<AppState>) -> axum::Router;    // state already applied
pub async fn serve(state: Arc<AppState>) -> oag_core::Result<()>;   // binds both listeners
pub use state::AppState;   // AppState::new(Config, Db, Cache) -> Result<Self>
pub use shutdown::Lifecycle;
pub mod metrics;  // install() -> PrometheusHandle, describe()
pub mod gateway; pub mod admin; pub mod health; pub mod listen; pub mod breakers;
```

Because both routers return `Router` (i.e. `Router<()>` — `.with_state(state)` is applied inside),
OpenGrok's `opengrok-server` can do exactly this:

```rust
let app = opengrok_router()
    .merge(oag_server::public_router(Arc::clone(&oag_state)));   // same origin, same port
// or, to keep the paths namespaced:
//  .nest("/gateway", oag_server::public_router(Arc::clone(&oag_state)))
```

No fork, no vendoring of handlers. `serve()` is only a convenience that binds two `TcpListener`s and
joins them; skipping it costs you the two spawned background tasks and the tuned connection
deadlines, both of which you can start yourself (see below).

### The binary entrypoint

`crates/oag/src/main.rs` — `#[tokio::main]`, clap `Cli { config: Option<String> (env OAG_CONFIG), command }`
with subcommands `Serve | Migrate | Config | Admin(Box<AdminCommand>)`. `Serve` does, in order:

1. `oag_server::metrics::install()?` + `describe()` — installs the **process-global** Prometheus recorder
2. `Db::connect(url, max_connections)` (lazy pool), `Cache::connect(redis_url)` (lazy client)
3. `AppState::new(config, db, cache)?` — builds the Kek, the adapter map, the Codex adapter (reads
   `gateway.codex.instructions_path` from **disk** here), `AuthCache`, `TransportPool`, `Breakers`,
   an empty `Catalog`
4. `state.lifecycle.set_metrics(handle)`
5. `state.reload_catalog().await` — logs a loud `warn!` on an empty catalog
6. `oag_store::readiness(...)` once, so an operator sees why a replica will never be ready
7. `oag_server::serve(state).await`

`serve()` itself additionally spawns `spawn_catalog_refresh` (interval `gateway.catalog_refresh_interval`,
skips the first immediate tick) and `usage_poll::spawn_usage_poll` (interval
`gateway.usage_poll_interval`), and wires `shutdown::signal(lifecycle, max_stream_duration)` as the
graceful-shutdown future.

### What "one binary serving both" requires

A merged binary needs to replicate steps 1–6 and start the two background tasks, then merge the
router. Concretely:

1. **Config.** `oag_core::config::Config` is loaded by `crates/oag/src/settings.rs::load()`, which is
   **private to the `oag` binary crate** (`mod settings;` in `main.rs`). Either re-implement the
   ~120 lines of `OAG_<SECTION>__<FIELD>` override logic in OpenGrok, build a `Config` by hand, or
   use `Config::from_yaml(&str)` (public, `config.rs:340`) on a document you assemble. This is the
   one piece of the binary that is not library-reachable. *Recommendation: upstream `settings::load`
   into `oag-core` or `oag-server` — it is pure and already unit-tested.*
2. **Migrations.** `Db::migrate()` runs `migrations/0001…0008` under an advisory lock (safe from every
   replica at once). OpenGrok has its own `opengrok-store` migrations; the two must live in one Postgres
   database with non-colliding table names, or in two databases with two `Db` handles. OAG's tables:
   `principal`, `api_key`, `route`, `account`, `account_route`, `model`, `usage_event`, `service`,
   `subscription_usage`, … (no `og_` prefixes, so a name audit is required).
3. **Redis.** OAG requires a Redis URL in config. It degrades gracefully at runtime — rate limiting
   fails **open** with a `warn!` (`cache.rs:197-211`), and the auth cache falls back to Postgres —
   but `/health/ready` reports not-ready and `AppState::new` still needs the URL. OpenGrok's current
   `Cargo.toml` has no redis dependency, so this is a **new runtime dependency** for the product.
4. **Metrics.** `metrics::install()` installs a **process-global** recorder
   (`PrometheusBuilder::install_recorder`). It can only be called once per process, and OAG's
   `/metrics` route renders from the handle it returns. If OpenGrok also wants `metrics`, both must
   share one recorder — install it once in `crates/opengrok` and hand the handle to
   `state.lifecycle.set_metrics(handle)`. Calling `install()` twice returns `Err`.
5. **Tracing.** `init_telemetry` in `main.rs` calls `tracing_subscriber::fmt().init()`, also
   process-global and also once-only. OpenGrok must own the subscriber and not call OAG's; OAG's
   crates only ever emit `tracing` events, never install.
6. **Listener strategy.** Choose one:
   - *Two ports, unchanged*: run `oag_server::serve(state)` on a `tokio::spawn` beside OpenGrok's
     own server. Simplest; keeps the admin listener genuinely separate.
   - *One port*: `merge`/`nest` `public_router` into OpenGrok's app. You lose `listen::Deadlines`
     (`header_read_timeout`, `idle_timeout`) unless OpenGrok's own serving loop applies equivalents —
     `axum::serve` cannot, which is exactly why `oag-server` depends on `hyper-util`.
   - Do **not** merge `admin_router` onto a public port unless you have edge/IAM restriction: `/`,
     `/metrics` and `/health/ready` sit outside the admin key layer by design.
7. **Background tasks.** If you skip `serve()`, spawn the catalog refresh and the usage poll
   yourself. `spawn_catalog_refresh` is **private**; `usage_poll::spawn_usage_poll` is public. Without
   the refresh, a repriced or newly-seeded model never reaches a running replica and the replica looks
   perfectly healthy while failing to route. *This is the easiest thing to get wrong.*
8. **Shutdown.** `AppState::lifecycle` (`Arc<Lifecycle>`) tracks in-flight streams; the
   `InFlightGuard` is moved into the SSE pump task so a rolling deploy waits for streams rather than
   severing them. A merged binary must drive the same drain (`shutdown::signal`) or long streams die
   on deploy.

### Obstacles, ranked

| Obstacle | Severity | Detail |
|---|---|---|
| Global metrics recorder | **medium** | once-per-process; must be installed by the merged binary and the handle passed in |
| Global tracing subscriber | **medium** | same; OpenGrok must own it |
| `settings::load` is binary-private | **medium** | the env-override scheme is not library-reachable; either duplicate or upstream it |
| Redis becomes a hard dependency | **medium** | new to OpenGrok's stack; degrades gracefully but is required by config and by readiness |
| Toolchain skew | **medium** | OAG pins `channel = "1.95"` (`rust-toolchain.toml`); OpenGrok declares `rust-version = "1.90"`. OAG uses `Duration::from_mins` (`config.rs`, `state.rs`), which needs a recent stable. The merged workspace must build on the newer toolchain. *(exact minimum not re-derived — confirm by building)* |
| sqlx feature unification | **low, verify** | OAG: `tls-rustls-ring`, `time`, `rust_decimal`, `migrate`. OpenGrok: `tls-rustls`, `chrono`. Cargo unifies to the union; both TLS spellings and both date crates would be compiled in. Expected to work, **not verified by building** |
| `include_str!("../../../../web/index.html")` | **low** | `crates/oag-server/src/admin/mod.rs:85` reaches four levels up out of the crate. Fine for a path dep into a full checkout; breaks if you copy `crates/oag-server` alone |
| `AppState::new` reads the filesystem | **low** | `gateway.codex.instructions_path` is read with `std::fs::read_to_string` at construction; a missing file is a hard `Error::Config` |
| `Db`/`Cache` connect lazily | **helpful** | `Db::connect` builds a lazy pool and `Cache::connect` only builds a client — neither touches the network, which is why OAG's own router tests construct a full `AppState` with unreachable URLs |
| Binary-only assumptions | **none found** | no `OnceLock`/`lazy_static`/`static mut` in the request path; the only `OnceLock` is `Lifecycle::metrics` (`shutdown.rs:35`), and the only `std::env::var` reads outside tests are in `crates/oag` (HOME/CODEX_HOME for CLI session import) |
| Admin CLI is binary-only | **note** | `oag admin …` lives in `crates/oag/src/admin/`; key minting, account import, catalog seeding are **not** exposed as a library. OpenGrok either shells out to `oag`, re-implements minting against the same schema, or the binary keeps the `admin` subcommand tree |

Nothing in `oag-server` requires being the process's only server. No `main`-only invariants, no
`std::process::exit` outside `oag`'s `main`.

---

## 9. Running it locally

Source: `justfile`, `docs/07-running-locally.md`, `deploy/compose/dev.yml`.

```bash
just dev        # Postgres + Redis in Docker, migrated
just bootstrap  # principal + default route + an ADMIN key, then the model catalog
just serve      # :29080 inference, :29081 admin + dashboard
```

### The environment the recipes use

```bash
export OAG_DATABASE__URL=postgres://oag:oag@127.0.0.1:5452/oag     # non-default ports on purpose
export OAG_REDIS__URL=redis://127.0.0.1:6399
export OAG_SECURITY__SIGNING_SECRET=dev-only-signing-secret-do-not-use-in-production-0001
export OAG_SECURITY__CREDENTIAL_KEK=b2FnLWRldi1vbmx5LWtlay0zMi1ieXRlcy0wMDAwMDA=
```

The database is **Postgres**, in the `deploy/compose/dev.yml` container, host port **5452**
(Redis on **6399**). Migrations are `migrations/0001_baseline.sql` … `0008_usage_origin.sql`, applied
by `oag migrate` (`Db::migrate`, advisory-locked).

### Cold-start sequence (each step exists because the next fails without it)

```bash
oag() { cargo run --quiet -p oag -- "$@"; }

oag admin init --email dev@localhost        # principal + route + the first ADMIN key
oag admin catalog seed                      # prices and context windows (built-in snapshot)
oag admin account add --name anthropic-1 --provider anthropic --secret sk-ant-...
oag admin route tiers --route default \
    cheap=anthropic/claude-haiku-4.5 \
    balanced=anthropic/claude-sonnet-4.5 \
    frontier=anthropic/claude-opus-5
oag admin key create --email dev@localhost --name opengrok   # the INFERENCE key
oag admin doctor                                             # should print `ok`
```

`--secret` is read from `OAG_ACCOUNT_SECRET` when omitted, keeping it out of shell history and the
process table. A subscription seat is imported rather than typed:
`oag admin account add --name grok-seat --from grok` (or `--from codex`), reading the CLI's own
`auth.json` and never writing it.

### Creating a key

| | Inference key | Admin key |
|---|---|---|
| Command | `oag admin key create --email <e> --name <n>` (`just key name=opengrok`) | the same `+ --admin` (`just admin-key name=ops`) |
| Reaches `:29080` inference | yes | yes |
| Reaches `:29081` `/admin/api` and the dashboard | **no — 403** | yes |
| Goes in | OpenGrok's config | the dashboard key box only |

The key `oag admin init` prints is an **admin** key. Shown once; only its SHA-256 is stored.
Revoke by prefix: `oag admin key revoke oag_live_abc…` (also evicts the shared auth cache — a raw
`UPDATE` in psql leaves the key working on every replica for up to the L1 TTL).

### Other recipes

`just check` (fmt + clippy `-D warnings` + tests) · `just dev-serve` (infra then gateway) ·
`just migrate` · `just catalog-update` (seeds from LiteLLM's live table, ~2 MB, needs network) ·
`just catalog-prices provider=xai` (overlay a provider's authoritative prices) · `just stack-up`
(full Caddy → Envoy → 3 replicas topology) · `just floci-up` (Cloud Run against a local GCP emulator)
· `just verify` (whole request path against a mock upstream, **no credentials needed**) ·
`just verify-breakers` / `verify-dialects` / `verify-translate` / `verify-bedrock` · `just ports` ·
`just config`.

When a request fails: `oag admin doctor [--route <name>]` checks, in the order a request meets them,
migrations → catalog non-empty → route + ladder parse → credentials attached and live → every rung
has a serving provider → Codex instructions present; prints the fixing command beside each failure and
exits non-zero.

---

## 10. A minimal Rust example: OpenGrok streaming a chat turn

Everything below is anchored in verified code: header names from `extract_key`
(`gateway/mod.rs:1326`), paths from `inference_routes` (`lib.rs:93`), response headers from
`oag_headers` (`mod.rs:839`) and `stream_response` (`mod.rs:1164`), and the `[DONE]` terminator from
`crates/oag-proto/src/openai.rs:812`.

**Recommended surface for `opengrok-harness`: `POST /v1/chat/completions` with `"stream": true`.** It is the
native dialect of five of the nine providers (`Provider::native_dialect`), so OAG takes the
byte-passthrough path when the upstream is one of them, and its stream framing is the simplest to
consume. Use `/v1/messages` if the harness is Anthropic-shaped; use `/v1/responses` only if you need
reasoning items as first-class stream objects (and note §7's two fixes).

```rust
use futures_util::StreamExt as _;
use serde_json::json;

/// Stream one assistant turn through OAG.
///
/// `base` is the inference listener, e.g. "http://127.0.0.1:29080".
/// `key`  is an OAG *inference* key: "oag_live_<64 hex>".
pub async fn stream_turn(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,          // "oag/auto" | "oag/cheap" | "xai/grok-4.6" | "xai/grok-4.6@sub"
    messages: serde_json::Value,
) -> anyhow::Result<()> {
    let response = http
        .post(format!("{base}/v1/chat/completions"))
        // Bearer wins if several key headers are sent; `x-api-key` and
        // `x-goog-api-key` are also accepted, by any client.
        .bearer_auth(key)
        .json(&json!({
            "model": model,
            "messages": messages,
            "stream": true,
            // Optional; a client cap that truncates is NOT treated as a
            // weak-model failure, so it will not silently escalate a rung.
            "max_tokens": 4096,
        }))
        // Optional: ask for a specific rung. Outranks the body's model name and
        // forces managed handling even on a passthrough route. An unknown rung
        // is logged and ignored, not an error.
        // .header("x-oag-tier", "frontier")
        .send()
        .await?;

    // Routing identity, before a single byte of body. Record all three on the
    // transcript entry: request_id is the ledger's key.
    let status      = response.status();
    let served      = header(&response, "x-oag-model");      // canonical id; ledger truth
    let tier        = header(&response, "x-oag-tier");       // None for an off-ladder pin
    let request_id  = header(&response, "x-oag-request-id"); // -> GET /admin/api/usage

    if !status.is_success() {
        // {"type":"error","error":{"type":<closed kind>,"message":...}}
        // Upstream failures nest the provider's body at error.upstream, with
        // error.upstream_status; 429 carries Retry-After.
        anyhow::bail!("oag {status}: {}", response.text().await?);
    }

    // text/event-stream, cache-control: no-cache, x-accel-buffering: no.
    let mut body = response.bytes_stream();
    let mut pending = String::new();

    while let Some(chunk) = body.next().await {
        pending.push_str(std::str::from_utf8(&chunk?)?);

        // SSE frames are separated by a blank line.
        while let Some(cut) = pending.find("\n\n") {
            let frame: String = pending.drain(..cut + 2).collect();

            for line in frame.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue; // `event:` lines: present on the Anthropic and
                              // Responses surfaces, absent on this one.
                };
                if payload == "[DONE]" {
                    return Ok(()); // the terminator decides success, not EOF
                }
                let v: serde_json::Value = serde_json::from_str(payload)?;
                if let Some(delta) = v["choices"][0]["delta"]["content"].as_str() {
                    // -> opengrok-wire transcript delta
                    print!("{delta}");
                }
                // v["usage"] arrives on the final data frame when the upstream
                // sends it; OAG meters independently either way.
            }
        }
    }
    Ok(())
}

fn header(r: &reqwest::Response, name: &str) -> Option<String> {
    r.headers().get(name)?.to_str().ok().map(str::to_owned)
}
```

### Notes that will bite otherwise

- **`x-oag-model` is the truth; the model's self-report is worthless.** `docs/08-clients.md` records
  a real session where the API `model` field said `grok-4.5` while the model's own answer said
  `grok-4.6`, and an earlier one where Grok claimed to be "Claude Fable 5". Only the `model` field,
  `x-oag-model`, and the ledger are evidence.
- **Managed mode ignores the model you name.** A route with `default_mode = 'managed'` classifies
  every request and picks the rung; the client's model string is not consulted. `oag/*` names are
  always managed whatever the route says. Check `x-oag-model` against what you sent, and read
  `selection_reason` (`classified` | `passthrough` | `floor_pinned`) from `GET /admin/api/usage`.
- **Escalation is one rung and non-streaming only.** A quality gate is knowable only once the answer
  is complete, so a streamed response is never retried — retrying would mean the client saw two
  answers. Rejections (the upstream refused before sending anything) are the exception.
- **A stream is metered even if OpenGrok hangs up.** The pump task outlives the client connection
  because the provider will bill for the tokens regardless (`gateway/sse.rs:12-16`). Cancelling a
  turn does not make it free.
- **Long streams and rolling deploys.** `max_stream_duration` (default 30 min) is both the per-stream
  ceiling and the drain budget; your orchestrator's grace period must be at least that long.
- **`/v1/models` is the *advertised* set, not the servable set.** After `076db83`, an off-ladder
  catalog name is only advertised where a metered credential exists — but `decide()` still honours it
  if OpenGrok sends it.
- **Discovery aliases are accepted on inference unconditionally**, whether or not
  `claude_code_model_aliases` is on, so a cached listing never starts failing.

---

## Open items / unverified

- Nothing was **built or run**; every claim is read from source at `fa87b6a`. The sqlx/TLS feature
  unification between the two workspaces and the exact minimum rustc are the two things worth
  proving with a `cargo build` of a merged workspace before committing to the single-binary plan.
- Test counts (226 / 495) are quoted from `README.md`, not re-counted.
- The admin API's response *bodies* were not transcribed field-by-field; only routes, purposes and
  auth were verified. If OpenGrok's UI will render `/admin/api/summary` or `/admin/api/usage`, read
  `crates/oag-server/src/admin/mod.rs:435` and `:827` before shaping types against them.
