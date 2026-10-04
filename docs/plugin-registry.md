# Account-owned plugin registry (#356)

The registry is `hexuria/plugin-marketplace` by default. `OG_PLUGIN_REGISTRY` can select another
public GitHub `owner/repository`; request bodies cannot choose hosts, repositories or URLs.
NativeChat #184 must transcribe the following server-owned shapes from
`crates/opengrok-plugin-api/src/lib.rs`, `crates/opengrok-integrations/src/registry.rs`,
and `crates/opengrok-integrations/src/installed.rs`, with a conformance ledger until recorded.

## Routes

All routes authenticate through the existing bearer/cookie verifier. Account identity comes
from that verifier, never from a body. Errors are `{ "error": "sentence" }`.

| Route | Request | Reply |
| --- | --- | --- |
| `GET /plugins/catalog` | Optional `?revision=<40-character commit SHA>` | `{registry, revision, plugins: [{name, description, repository, revision, path, unavailableReason}]}` |
| `GET /plugins/catalog/{name}` | Optional registry `revision` query | `{entry, registryRevision, parts: [{kind, name, supported, reason}], connectors: [names]}` |
| `GET /plugins/installations` | None | Array of the caller's snapshots: `{name, registry, registryRevision, repository, revision, installedAtMs, bundle}` |
| `POST /plugins/installations` | `{name, registryRevision}` | 201 `{name, revision, registryRevision, parts}`; 409 if already installed |
| `DELETE /plugins/installations/{name}` | None | 204, or 404 when absent for this account |
| `PUT /plugins/installations/{name}/credentials/{connector}` | `{token}` | 204; no credential is returned |

`bundle` contains `manifest`, `mcp`, `skills`, `files`, and `parts`. It contains registry data,
never account tokens. `GET /connectors` retains configured provider rows and adds installed
connector rows `{name, label, plugin, authentication: "token"}`. The plugin field disambiguates
identically named connectors. Configured providers retain their existing OAuth authorize routes;
a registry connector's token is saved through its plugin credential route above.

Clients should read catalog details at the catalog's `revision` before offering an install.
Unsupported components remain in the `parts` list with their reason. Catalog entries with an
unsupported source have `unavailableReason`; their detail/install request is refused.

## Pins and ownership

A local source is read from the registry commit; a URL source must be a public GitHub repository
with a full pinned commit. Both pins are recorded. Manifests, MCP configuration, skill bodies,
and supporting text files are stored as a database snapshot. Listing, turns, and resumes never
fetch registry content. Advancing the registry cannot change an install.

An explicit update is uninstall followed by install at a newly chosen registry revision.
POST cannot overwrite a pin. Uninstall deletes its encrypted credentials in the same transaction
as the installation. Reinstall never reuses an old token with a changed remote URL.

`plugin_installation(account_id, name)` is a new account-owned scope, separate from
`Owner::Global` and the existing lendable connection aggregate. `plugin_credential` belongs to
that installation and has no loan or bot scope. Tokens are sealed in the existing vault, bound
to `plugin/<account>/<plugin>/<connector>` as AEAD associated data. No filesystem credential
store is used. Saving a credential and uninstall serialize on the installation's row lock.

A turn must be driven by the same account that owns the Bot to see its registry installations.
A member's turn on an org-shared Bot gets neither the owner's installations nor the member's
installations for their own Bots. Existing Bot-scoped connection lending is unchanged.
The existing ceiling/profile gates still decide which installed MCP tools can run; the owner
can switch installed plugin names through the existing Bot ceiling routes. Built-in and
configured deployment plugin names are reserved at install, preventing tool-prefix collisions.

Each installed plugin resolves only its own token namespace. Header placeholders follow the
existing `${CONNECTOR_TOKEN}` convention. A streamable-HTTP server declaring no auth headers or token placeholders may
operate keylessly; routing headers are preserved, and an explicitly supplied token adds a bearer
header only to that server. Explicit Authorization/x-api-key headers are not overwritten.
This iteration does not launch browser OAuth flows for arbitrary MCP servers. Discovery,
OAuth consent, and refresh integration require an account-bound adapter before they can ship.

## Bundle adapter and intentional limits

Manifests are read from root `plugin.json`, `.grok-plugin/plugin.json`, or
`.claude-plugin/plugin.json`. Root `mcp.json` and `.mcp.json` are accepted. The marketplace
`http` transport maps to agent-plugins `streamable-http`; missing type with a command maps to
stdio, which is listed and refused. HTTPS remote MCP is supported; legacy SSE, stdio, unknown
transports, commands, agents, hooks and LSP are listed with reasons. No install hooks run.

Skills are exposed as `<plugin>.<skill>` through the existing `use_skill` source. Their names,
frontmatter, 8000-character bodies and 300-character descriptions are checked before offering.
Instructions are fenced and identified as third-party plugin text, followed by the existing
closing denial. Supporting files use the existing checked bundle-copy path. A Bot without a
computer is told the files are unavailable. Resumes retain the offered skill's revision and
refuse it if the installation has been removed or changed.

Registry reads reject traversal paths, non-GitHub source hosts, symlinks, truncated trees,
duplicate names, mismatched manifest names, floating revisions, and ambiguous tool prefixes.
Reads have a 15-second request timeout, a 60-second overall bundle timeout, 2 MiB file/bundle
limits, 128 bundle files, and 256 catalog entries. Public GitHub API rate limits can cause a
read refusal; no deployment-wide GitHub token is inferred from user credentials.

## Module boundaries and verification impact

`opengrok-integrations` owns remote reads, immutable snapshots, persistence and shared provider
rules. `opengrok-plugin-api` is an authenticated HTTP adapter with an injected identity verifier;
`opengrok-server` supplies the verifier and assembles it into its router. This split keeps the
fixed crate ceilings without placing network/database dependencies in the pure plugin parser.
The original provider tests move with their implementation. The SQL schema moves byte-for-byte
to `crates/opengrok-store/src/schema.sql`, with the two new tables appended; migration hashing,
locking and replay behavior use the same included string.

Verification boundaries affected: deterministic parsing, account authorization, credential
persistence, install/uninstall atomicity, per-turn tool and skill assembly, and HTTP contracts.
The run protocol, scheduler, lease/parking/recovery model and proof kernels are unchanged.
No additional state machine mirrors them. Rust tests own parsing and account filtering;
Postgres transaction tests own persistence and the credential/uninstall ordering. No new external
dependency versions are introduced. Miri and concurrency model tools are not justified by these
safe-Rust changes using existing vault and database transaction primitives.

## Grok Build reuse decision (4 October 2026)

Reviewed xAI's MCP integration at commit
[`2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`](https://github.com/xai-org/grok-build/tree/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-mcp).
Its [manifest](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-mcp/Cargo.toml)
declares Apache-2.0 and rmcp 3.4/reqwest 0.13. Our lockfile currently uses rmcp 3.1.4 and reqwest 0.13.4: the same families, but newer Grok helpers may require an explicit rmcp upgrade.
Its OAuth orchestrator and HTTP reconnect/backoff handling are useful upstream references.

The whole crate is not a drop-in server dependency: it reaches many Grok configuration,
telemetry, sandbox and tool-runtime workspace crates. Its credential adapter writes to
`$GROK_HOME/mcp_credentials.json`, keyed by server name and URL, and its interactive OAuth flow
opens a local browser/callback listener. Those assumptions do not provide our multi-account
server authorization and encrypted persistence. Prefer upstream rmcp APIs with our own
account-bound CredentialStore adapter; adapt narrow Grok helpers only where useful, pin their
provenance and retain required license notices if code is copied. No xAI source is copied in
this registry iteration and no Grok dependency is added.
