# Registry verification — 4 October 2026

Base: `origin/main` at `9ccd1a0813afb1a9ad1d112c09106f94d73356fb`.
Work: #356, branch `gol/plugin-registry`. The dev servers were not restarted and no production
ports were used. HTTP fixtures bind ephemeral loopback ports. Integration tests use the isolated
`opengrok_against_plugin_registry_gate` database beside `opengrok_gate`, never the dev database.
The model door is scripted and does not make billed provider calls.

## Evidence

| Claim | Verifier | Bounds and result |
| --- | --- | --- |
| Unit-tested bundle adaptation, refusal reporting, third-party framing, token namespace construction, and original provider behavior | `cargo test --locked -p opengrok-plugins -p opengrok-integrations --lib` | 52 passed. Includes unchanged provider tests moved with their implementation. |
| Property-tested path and revision acceptance | Two proptest cases within that unit run | Random strings up to 150 characters for paths and 70 for revisions; sampled, not exhaustive. Accepted paths have no traversal/URL controls and accepted revisions are full hexadecimal hashes. |
| Integration-tested catalog and ownership contract | `OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate cargo test --locked -p opengrok-server --test against_plugin_registry` | 2 passed at the time (now 4: see the review follow-up below). One local HTTP registry adapter test and one Postgres/HTTP/turn test. |
| Installation pin persists when catalog HEAD moves | Local registry starts at one commit and advances to another after installation | GET installations keeps the old bundle and pin; only uninstall followed by POST selects the new pin. External sources retain their own commit. |
| Account secrets remain encrypted and private | HTTP credential save, ciphertext query, cross-account reads/writes, and scripted shared-Bot turn | Another account cannot save an absent installation, read the owner's token, or read their skill. Even installing its own copy does not offer that copy to a shared Bot the account does not own. Token is absent from ciphertext plaintext windows and listing responses. |
| Uninstall removes credentials atomically | Postgres integration: concurrent credential save and uninstall via `tokio::join!` | One two-request interleaving run; row locking and the transaction leave zero orphan secret rows. This is an integration test, not exhaustive concurrency verification. |
| Installed skills reach an actual turn correctly | Scripted model calls `use_skill` through POST `/ag-ui` | SSE contains the pinned skill body, third-party framing, unavailable-file notice for a Bot without a computer, and RUN_FINISHED. The member's shared-Bot turn finishes without that body or framing. |
| Existing connector, ceiling and attached-skill behavior remains tested | `cargo test --locked -p opengrok-server --test against_connections --test against_a_tool_ceiling --test against_coworker_skills` with the gate database | 46 passed. |
| Repository checks pass | `bash scripts/gate.sh --checks` | Passed formatting, crate ceilings, architecture, script pipeline check, cargo-deny, existing formal expected verdicts, workspace/all-target compilation, build without default features, and workspace/all-target Clippy with `-D warnings`. Lean was skipped by the existing gate because it is unavailable; no new proof is claimed. |
| Schema extraction preserves existing SQL | Compared the original raw schema string from the base commit with the included SQL file | Existing bytes unchanged; the installation and credential tables are appended. |

Total focused Rust tests: 100 passed across these six binaries/suites. Crate ceilings were
tightened after extraction (`opengrok-server` 38623, `opengrok-store` 8155); neither was raised.
No external dependency versions changed.

## Limits and remaining work

This is server #356. NativeChat #184/#185/#186, several service accounts and per-Bot account
pins (#359), the account chooser (#360), additional plugin loaders (#361), the live official
GitHub MCP pilot (#362), and the blocked Gmail server decision (#363) are not claimed complete.
No client UI or wire-corpus entry is added here. The client must keep its conformance ledger
until new route recordings are vendored. The base verification above uses controlled HTTP fixtures. The live supplement below checks
real services; it still does not assert a successful OAuth login.

The source and verification ownership decisions, API contract and Grok Build reuse assessment
are in [`docs/plugin-registry.md`](../../plugin-registry.md). No Grok Build code was copied or
added as a dependency. Its desktop token-file store does not replace this account-scoped vault.

## Live supplement — 4 October 2026

The owner requested live verification. Public responses and immutable source pins are captured
in [`live-2026-10-04.json`](live-2026-10-04.json). The two network tests are explicitly ignored
in normal CI; run them with:

```sh
OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate \
  cargo test --locked -p opengrok-server --test against_plugin_registry live_ -- --ignored --nocapture
```

- The actual `hexuria/plugin-marketplace` catalog returned 30 entries at commit
  `77a16ec85ded1c3b133f686bd2e1bea36090e124`.
- The API installed Exa from `exa-labs/exa-grok-plugin` at
  `879fae0c814765c43f39ea8f56aae1d44e9d9bc8`, kept it private to the synthetic account, and
  uninstalled it successfully. The manifest's unsupported skill and command components remain
  visible with reasons; no unsupported skill body was silently truncated.
- The pinned bundle declares `https://mcp.exa.ai/mcp/oauth`, with a routing header and an explicit
  upstream note requiring OAuth. Its MCP initialization returned `Auth required`. The initial
  expectation of usable tools therefore FAILED; the final boundary probe asserts this refusal.
  Passing the boundary probe means the limitation was reproduced, not that OAuth works.
- Exa's separate documented public endpoint `https://mcp.exa.ai/mcp` initialized and listed
  `web_search_exa` and `web_fetch_exa` through the production MCP client. It was tested as a
  transport baseline; the installed bundle's URL was never rewritten to that public endpoint.
- No search, fetch, or other remote tool was executed. No personal token or browser login was
  used. OAuth discovery/consent/refresh and the Grok reconnect helper are still NOT adopted.

The first test Bot had no computer, so the existing toolbox path exited before MCP connection.
The live harness now uses an inert computer provider, creates no VM, and refuses every command.
With computer availability and policy allowance established, a direct production-client probe
isolated the real refusal to authentication. Temporary diagnostic logging was removed.

This exposed a separate bearer-connector defect: a routing header such as `x-exa-source` hid the
server's token connector and prevented optional bearer injection. A regression test failed with
`[]` instead of `["demo"]` before the fix, and passed after it. The fix preserves routing headers,
does not overwrite explicit auth headers or token placeholders, and keeps the existing account
credential namespace. It does not implement OAuth.

Fresh checks after that fix: 53 plugin/integration unit tests, 2 fixture/HTTP tests, and 2 opt-in
live boundary tests passed; affected crates' all-target Clippy passed with warnings denied.
The live tests' outcome for the pinned OAuth connector remains **blocked pending implementation**.
The earlier formal/run checks are unchanged: this fix only changes deterministic header and
connector construction, with no journal, scheduler, ownership, or persistence protocol change.

## Review follow-up — 4 October 2026

A code review of PR #366 raised fifteen findings. Each was checked against the code before it
was fixed; the regression test that now asserts it is named. Base for these checks: the PR head
`899f0ef`.

| Finding | Confirmed by | Fixed in | Asserted by |
| --- | --- | --- | --- |
| A bundle's MCP URL only had to start `https://`; the client followed up to 10 redirects, so a `307` to `http://127.0.0.1:<port>` re-sent the POST and its headers inside the deployment | `Session::connect_within` built its client with no redirect policy; nothing refused private targets | `opengrok_plugins::bundle::public_https`, `opengrok_integrations::net` (`harden`: no redirects, public-only resolver), `Endpoint::harden` | `a_hosted_server_must_name_a_public_host`, `private_and_disguised_addresses_are_never_public`, `a_name_resolving_to_loopback_is_refused`, `a_hardened_client_does_not_follow_redirects` |
| Plugin skills were offered and read with the plugin switched off on the Bot | `skills::onto` had no ceiling check; the old integration test asserted `use_skill` after switching the plugin off | `turn::switched_on`, checked at offer and again at read | `pinned_installations_and_credentials_belong_only_to_the_driving_account` (off: no `use_skill`, no body; on: body), `a_plugin_is_on_only_where_ceiling_and_grant_both_admit_it_whole` |
| Any 40-hex `registryRevision` was trusted | `catalog(Some(sha))` fetched it without asking where it came from | `Registry::reachable` (compare API) and `installable` (still listed at HEAD) | `registry_resolves_local_and_external_commits_without_a_database` (diverged commit), the hostile registry's 404 commit |
| Uninstall left `<plugin>.*` in ceilings and grants, so a reinstall went live unasked | `uninstall` touched neither table | `installed::switch_off` in the install and uninstall transactions | main integration test: no `demo` row after uninstall, `enabled: false` after reinstall; `switching_a_plugin_off_takes_only_its_own_entries` |
| Plugin skill files were copied without the running-computer check | `plugin_skill` called `bundle::place` directly | shared `skills::files_line` | existing skill-file tests through the shared function |
| One non-UTF-8 or oddly named file failed the whole bundle | `file()` and `path_ok` returned `Err` | per-file `file` parts | `one_bad_entry_is_one_unavailable_entry_and_every_stated_refusal_holds` |
| One malformed catalog entry failed the catalog | early `return Err` for name, path, duplicates | per-entry `unavailableReason`; `./` stripped for external sources | same test |
| Credentials compared the bundle JSON every turn | `p.bundle = $4::jsonb` | `incarnation` column | `an_old_installation_snapshot_cannot_receive_a_replacement_token` |
| The purge left plugin rows | not in the per-table list | two deletes | `everyone_but_the_allowlist_goes_and_the_allowlist_keeps_everything` footprint |
| Registry reads uncached, sequential, anonymous | no cache; one fetch at a time | immutable-commit cache, 5-minute HEAD, eight fetches at a time, optional `OG_PLUGIN_REGISTRY_TOKEN` | covered by the fixture tests; the budget itself is GitHub's and not exercised |
| Unknown MCP fields dropped, server reported supported | `McpServer` drops unknown keys | `hosted_refusal` | hostile registry: `disabled`, `oauth`, `timeout` |
| One unreadable installation broke the ceiling and Connections | `collect::<StoreResult<_>>` | per-row `readable`, logged | — (corrupt JSON cannot be written through the API) |
| A bad `OG_PLUGIN_REGISTRY` was silent | `.ok()` | `Registry::from_env` logs at boot | — |
| Most documented refusals were untested | — | — | the hostile-registry test |
| Crate ceilings met by moving SQL; docs not updated | `crate-size.sh` counts `.rs` only | stated in `docs/plugin-registry.md`; CLAUDE.md and `postgres.md` updated; unused `opengrok-plugins` edge of `opengrok-plugin-api` removed | `scripts/check-architecture.sh` |

Not changed: the `opengrok-integrations`/`opengrok-plugin-api` split itself and the
`schema.sql` extraction stand; the follow-up states what the line count measures rather than
undoing the structure. `MANIFEST.json`'s `server_sha` is rewritten by `scripts/record-wire.sh`
on the next recording after merge.

### Second review — 4 October 2026

A second, independent review of PR #367 found two of the fixes above incomplete and several
smaller problems. Each fix below has a regression test, and the three marked † were run against
the previous source and failed there.

| Finding | Fixed in | Asserted by |
| --- | --- | --- |
| `harden` left reqwest's environment proxy on: through `HTTPS_PROXY` the resolver judged the proxy's address and the proxy resolved the bundle's host | `net::harden` adds `.no_proxy()` | `a_hardened_client_goes_direct_and_does_not_follow_redirects`: a builder arriving with a proxy set; observed failing with `.no_proxy()` removed |
| The bundle cache answered before `unavailable_reason`, so an entry unavailable at one registry commit was served from another's cache | the reason is checked before the cache | hostile-registry test, `pulled` entry |
| A pin newer than a replica's cached HEAD compared as `behind` and was refused | `reachable` re-reads HEAD before refusing | † `registry_resolves_local_and_external_commits_without_a_database` (five-minute TTL) |
| Every refusal from the compare call, including a reply over 2 MiB, read as "a fork's commit" | only a 404 means "not ours"; compare asks `per_page=1` and reads up to 16 MiB | hostile-registry test: a 500 from compare is `Upstream` |
| A Bot whose tools were "all" had a reinstalled plugin live at once | `opengrok_policy::names_plugin`: an install is on only where it is named; its servers and ceiling row follow it | † main integration test, `ToolSet::All` grant; `an_installed_plugin_is_on_only_where_both_layers_name_it` |
| Names were cut to 64 characters before they were judged | the whole name is judged | † hostile-registry test, a 72-character name |
| `switch_off` locked rows in no fixed order | `order by` on both queries | `installs_by_one_account_at_once_never_deadlock`: eight installs/uninstalls of one account at once. Without the `order by` it hit "deadlock detected" in 3 of 6 runs; with it, in none of 8 (nor of 16 more at smaller sizes). A race: it catches the bug often, not always |
| A server dropped as non-public at dial was not logged | `warn` naming the server | — |
| Skills read each bundle whole, and judged the plugin by `policy_for` while the turn ran under `policy_to_use` | `installed::skills_for_turn` reads only skills; both skill paths use `policy_to_use` | main integration test |
| The plugin switch copied the start of `may_run_any_under` | moved into `opengrok-policy` | `an_installed_plugin_is_on_only_where_both_layers_name_it` |

Installs saved before these rules were never judged by them, and their stored servers no longer
carry the raw fields the rules read. The schema clears them once
(`installs-before-the-mcp-field-rules` in `schema.sql`), credentials and sealed secrets with them;
only the unmerged #366 branch could have written any. `installs_from_before_the_rules_go_once_and_installs_since_stay`
asserts it, from a freshly created database: a first version of the pass sat above
`plugin_credential` and failed every first boot, which that test caught.
