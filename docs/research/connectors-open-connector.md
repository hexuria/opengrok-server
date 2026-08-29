# open-connector — the connector catalogue

**Researched:** 29 Aug 2026 against the GitHub repository, its migrations and its live catalogue
endpoint. This report **corrects an earlier audit** that was wrong on its most consequential point.

**Role in OpenGrok:** the source of third-party tools (Slack, GitHub, Gmail, …) behind
`crates/opengrok-tools`' executor trait. Our own `opengrok-policy` decides who may call what — never theirs.

---

## Verdict

**Vendor the catalogue's definitions, do not adopt their runtime as the source of truth, and do not
believe you need their paid tier for OAuth.** open-connector is fully Apache-2.0 TypeScript, and the
OAuth flow *and credential vaulting are in the open repo*. For a Rust backend the pragmatic move is:
consume the catalogue metadata (the live JSON endpoint, or extracted definitions) and write our own
executor plus our own token vault — Rust has `oauth2`, `sqlx`, and `ring`/`age` for exactly this.

Running their Node runtime as a **sidecar behind our tool trait** is the correct first step: it buys
working executors for ~1,450 providers immediately, and can be replaced provider-by-provider later.

### Corrections to the earlier audit

| Earlier claim | Reality |
|---|---|
| "Per-user OAuth vaulting lives only in their paid gateway" | **False.** OAuth flow, `connections`, `oauth_client_configs`, `oauth_states`, `runtime_tokens` tables and the encryption path are all in the Apache-2.0 repo. The hosted tier sells **pre-registered OAuth apps** (so your users don't need you to register a GitHub/Slack app) plus managed hosting. |
| "1,451 providers, 15,154 actions" | Close but drifting — the live catalogue reported **1,453 providers / 14,922 actions** at time of checking, including ~45 composite "Fusion API" services. It is a live number; don't hardcode it. |
| "Zero tenant columns across fifteen migrations" | Tenancy claim **confirmed**; count wrong — there are **12** migrations (`0001`–`0012`) plus a `postgresql/` variant dir. No `tenant_id`/`org_id`/`workspace_id` anywhere. |
| "Encryption opt-in" | **Confirmed.** Without `OOMOL_CONNECT_ENCRYPTION_KEY`, provider secrets sit in a plaintext local SQLite file. |
| "Admin auth open when unconfigured" | **Confirmed.** Without `OOMOL_CONNECT_ADMIN_TOKEN` there is no barrier on the admin API/console; their docs say such a runtime "must remain on localhost or a private network." |

---

## What the repo actually contains

Both a **catalogue and a runtime**, self-hostable, one Apache-2.0 TypeScript repo. Per provider,
under `src/providers/<service>/`:

| File | Contents |
|---|---|
| `definition.ts` | provider metadata + auth config (OAuth2 URLs, token auth method, API-key shape) |
| `actions.ts` | action schemas — name, description, required scopes, input/output schema |
| `executors.ts` | **the actual HTTP-calling code** — auth header injection, path construction, JSON parsing |
| `runtime-*.ts` | shared per-resource execution helpers |
| `scopes.ts` | OAuth scope constants |

It ships as a deployable service (Docker/Node, Cloudflare Workers, Fly.io, Helm) with SQLite or
Postgres persistence, a web console, a CLI, an SDK, an **MCP server**, and an HTTP/OpenAPI surface.

**License:** `LICENSE.txt` = Apache-2.0 across the whole repo. `NOTICE.md` clarifies it grants no
rights to third-party trademarks/APIs — you still register your own OAuth apps for production.

---

## The action schema

A typed TypeScript DSL over a JSON-Schema-shaped builder (`s.object`, `s.string`, …) — **not** flat
JSON files on disk. Real example, `src/providers/github/actions.ts`:

```ts
action({
  name: "get_current_user",
  description: "Get the current authenticated GitHub user profile.",
  requiredScopes: githubUserReadScopes,
  inputSchema: s.object({}),
  outputSchema: githubCurrentUserSchema,
})
```

Provider auth lives in `definition.ts`:

```
service: "github"
authTypes: OAuth2 + API Key
oauth2: authorizationUrl https://github.com/login/oauth/authorize
        tokenUrl        https://github.com/login/oauth/access_token
        tokenAuthMethod client_secret_post
apiKey: bearer PAT (github_pat_...)
```

The HTTP call itself is in `executors.ts` (`requireBearerCredential()` + `githubRequestJson({ path:
"/user", accessToken, fetcher })`).

**The consequence that decides our design:** method and path are *implicit in TypeScript executor
code*, not declared as flat data. So a single generic Rust executor reflecting over pure JSON is
**not** free. Options, in order of increasing cost:

1. **Sidecar** — run their Node service, call it over HTTP/OpenAPI/MCP from Rust. Zero executor
   code. Cost: operating a Node service and its store. ← *chosen for v1*
2. **Extract to JSON** — a one-time Node script using their own `defineProviderAction` exports to
   dump schema + auth type + base URL per provider, then a generic Rust executor. Cost: an
   extraction step to keep in sync with upstream.
3. **Reimplement per provider** — highest fidelity, highest cost. Only for the handful of
   connectors that matter most.

---

## Auth reality (open vs hosted)

In the open repo:

- `migrations/0001_runtime.sql` — `connections`, `oauth_client_configs`, `oauth_states`,
  `runtime_tokens`, `runs`
- `migrations/0006_connection_identity.sql` — reshapes `connections` around `service` +
  `connection_name`, supporting several named accounts per provider per instance (`connectionName`
  param, `x-oo-connector-alias` header)
- `OOMOL_CONNECT_ENCRYPTION_KEY` encrypts stored credentials and OAuth client secrets
- `OOMOL_CONNECT_ADMIN_TOKEN` gates admin/API/console access

The hosted tier adds pre-registered OAuth apps and managed infrastructure — convenience, not
capability.

---

## Security and tenancy

**Single-tenant per instance, by design.** No tenant columns. Multi-tenant SaaS use means either one
instance per tenant, or a tenancy layer built on top. For OpenGrok this is fine while
single-workspace, and is a **decision gate** before multi-tenant (see `docs/PLAN.md` open questions).

Both weak defaults (opt-in encryption, open admin when unconfigured) are *self-hosted deployment*
defaults we control — set both env vars, keep the sidecar on a private network.

---

## Rust fitness

No Rust bindings; the repo is Node 22+ TypeScript. But the catalogue is exposed as JSON over HTTP
(`https://connector.oomol.com/v1/catalog`, plus a self-hosted instance's own OpenAPI), which
`reqwest` + `serde_json` consume trivially. Consuming the *TypeScript source* to extract executor
routing requires a build-time codegen step — that is the real engineering cost, not the JSON.

---

## Risks

- Executor routing is implicit in TS, not flat data — extraction is nontrivial and must track
  upstream changes.
- Single-tenant-per-instance; isolation is entirely on us.
- Live catalogue counts drift — never hardcode them.
- ~6 contributors for 1,400+ integrations — bus-factor risk.
- "Fusion API" composite services slightly inflate the provider count.

---

## Sources

- [oomol-lab/open-connector](https://github.com/oomol-lab/open-connector) ·
  [README](https://github.com/oomol-lab/open-connector/blob/main/README.md)
- [src/providers](https://github.com/oomol-lab/open-connector/tree/main/src/providers) —
  [github/actions.ts](https://raw.githubusercontent.com/oomol-lab/open-connector/main/src/providers/github/actions.ts) ·
  [github/definition.ts](https://raw.githubusercontent.com/oomol-lab/open-connector/main/src/providers/github/definition.ts) ·
  [github/executors.ts](https://raw.githubusercontent.com/oomol-lab/open-connector/main/src/providers/github/executors.ts)
- [migrations](https://github.com/oomol-lab/open-connector/tree/main/migrations) —
  [0001_runtime.sql](https://raw.githubusercontent.com/oomol-lab/open-connector/main/migrations/0001_runtime.sql) ·
  [0006_connection_identity.sql](https://raw.githubusercontent.com/oomol-lab/open-connector/main/migrations/0006_connection_identity.sql)
- [self-hosting guide](https://oomol.com/en/docs/openconnector-self-hosting/) ·
  [overview](https://oomol.com/en/docs/openconnector/)
- Live catalogue: `https://connector.oomol.com/v1/catalog`
