# Account-owned plugin registry (#356)

The registry is `hexuria/plugin-marketplace` by default. `OG_PLUGIN_REGISTRY` can select another
public GitHub `owner/repository`; request bodies cannot choose hosts, repositories or URLs. A value
that is not one is logged at boot, by name, and every `/plugins` route then answers 503.
`OG_PLUGIN_REGISTRY_TOKEN`, when set, is sent to `api.github.com` only (never to raw file hosts or
source URLs): anonymous reads share one 60-an-hour budget per server address.
NativeChat #184 must transcribe the following server-owned shapes from
`crates/opengrok-plugin-api/src/lib.rs`, `crates/opengrok-integrations/src/registry.rs`,
and `crates/opengrok-integrations/src/installed.rs`, with a conformance ledger until recorded.

## Routes

All routes authenticate through the existing bearer/cookie verifier. Account identity comes
from that verifier, never from a body. Errors are `{ "error": "sentence" }`. A request the server
should not have been sent (a malformed `revision`) and a bundle it will not take answer 422; a
registry that did not answer, 502.

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
Unsupported components remain in the `parts` list with their reason. A file the bundle carries
but this server leaves behind (a symlink, a name outside the plain-path allow-list, bytes that are
not UTF-8 text) is a part of kind `file`, so one PNG never fails the whole plugin. A catalog entry
that cannot be installed (an unsupported or unsafe source, an invalid name, or two entries sharing
one name) has `unavailableReason`; its detail/install request is refused with that reason. One bad
entry is one unavailable entry: the rest of the catalog is still served. A row with no name at all
cannot be shown and is left out.

A pinned `revision` must be reachable from the registry's default branch (GitHub's compare API,
`<revision>...<HEAD>` answering `ahead` or `identical`); any other 40 hex characters, including a
commit from a fork, which GitHub serves under the parent's path, are refused. An install also
requires the plugin to be listed, and installable, at the registry's current HEAD: a plugin the
maintainers pulled cannot be installed again at an older pin. Existing installs are untouched.
Only GitHub's 404 means "not on the branch"; a compare that fails otherwise is an upstream error
(502), and a pin newer than this replica's cached HEAD is asked about again against a fresh one
before it is refused.
Commits never change, so what was read at one is cached in the process; only "which commit is
HEAD" expires, after five minutes.

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
can switch installed plugin names through the existing Bot ceiling routes. **The same switch
decides its skills and its servers**: both are on only while the ceiling and the grant both NAME
the plugin whole (`"<plugin>.*"`, `opengrok_policy::names_plugin`), and a skill is checked again
when `use_skill` reads one, since a resumed run re-offers what its start captured. A Bot whose
tools are "all" admits every plugin without naming one, so an account's install is off there, and
its ceiling row says so, until its owner switches it on by name; a deployment plugin keeps
following "all" as before. Install and uninstall each take the
plugin's entries out of the ceiling and grants of every Bot the account owns, in the same
transaction, bumping the ceiling's `version`: a new install, including the uninstall-then-install
update, starts switched off and waits for its owner to switch it on again. Built-in and
configured deployment plugin names are reserved at install, preventing tool-prefix collisions.

A remote MCP server named by an installed bundle must be HTTPS on a public host. Address literals
in loopback, private, link-local, CGNAT, documentation and other special-purpose ranges are
refused at install and again at dial, and so is `localhost`. A name is resolved at every dial by a
resolver that keeps public addresses only, and the session follows no redirects: a `307` to an
internal address would otherwise carry the POST and its credential headers with it. An operator's
own plugins (`OG_PLUGINS_DIR`) are configuration and keep the system resolver. An installed
plugin's session also ignores `HTTPS_PROXY`/`HTTP_PROXY`: through a proxy the resolver is asked
for the proxy's address while the proxy resolves the bundle's host, so the check never ran. A
deployment whose only way out is a proxy therefore cannot reach installed plugins' servers.

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
transports, commands, agents, hooks and LSP are listed with reasons. No install hooks run. A hosted
entry carrying anything besides `type`, `url` and `headers` is listed as unsupported rather than
dialled without it: `disabled: true`/`enabled: false` (its author switched it off), an `oauth`
block (#364), or any other field, by name.

Skills are exposed as `<plugin>.<skill>` through the existing `use_skill` source. Their names,
frontmatter, bodies and descriptions are checked before offering, against the same
`MAX_SKILL_BODY_CHARS` (8000) and `MAX_SKILL_DESCRIPTION_CHARS` (300) a person's skills are, and an
unsupported skill says which of the four failed.
Instructions are fenced and identified as third-party plugin text, followed by the existing
closing denial. Supporting files use the existing checked bundle-copy path, under the same rules
as a person's skill files: never a reason to wake a computer that is not running, and checked
against the path allow-list at the copy. A Bot without a computer is told the files are
unavailable. Resumes retain the offered skill's revision and
refuse it if the installation has been removed or changed.

Registry reads refuse traversal paths, non-GitHub source hosts, truncated trees, mismatched
manifest names, floating revisions and revisions off the default branch, and leave symlinks and
non-plain paths behind as `file` parts. Reads have a 15-second request timeout, a 60-second overall
bundle timeout (files are fetched eight at a time), 2 MiB file/bundle limits, 128 bundle files,
and 256 catalog entries. Public GitHub API rate limits can cause a read refusal; no GitHub token
is inferred from user credentials, only `OG_PLUGIN_REGISTRY_TOKEN` is used. Each refusal here is
asserted by `one_bad_entry_is_one_unavailable_entry_and_every_stated_refusal_holds`.

An install saved before the MCP field rules above was never judged by them, and its stored servers
have already lost the fields they read. The boot that brings the rules clears such installs once,
with their credentials and sealed secrets; only the unmerged #366 branch wrote any. Reinstall them.

Each installation carries a random `incarnation`, and a turn reads credentials for the incarnation
it loaded: an uninstall and reinstall between a turn's read and its credential read gives that
turn none. It replaced comparing the stored bundle JSON, which re-sent the bundle every turn and
would have withheld every credential the first time `Bundle` gained a defaulted field.

## Module boundaries and verification impact

`opengrok-integrations` owns remote reads, immutable snapshots, persistence, shared provider
rules, what installed plugins give one turn (`turn`: dialled endpoints, offered and read skills)
and how their servers are reached (`net`). `opengrok-plugin-api` is an authenticated HTTP adapter
with an injected identity verifier; `opengrok-server` supplies the verifier and assembles it into
its router. Network/database dependencies stay out of the pure plugin parser. The original
provider tests move with their implementation. The SQL schema moves byte-for-byte to
`crates/opengrok-store/src/schema.sql`, with the new tables appended; migration hashing, locking
and replay behavior use the same included string. That move is also what took
`opengrok-store` under its line ceiling: `scripts/crate-size.sh` counts `.rs` files only, so the
lower number measures where the SQL lives, not less of it.

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
