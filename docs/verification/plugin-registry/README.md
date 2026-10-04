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
| Integration-tested catalog and ownership contract | `OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate cargo test --locked -p opengrok-server --test against_plugin_registry` | 2 passed. One local HTTP registry adapter test and one Postgres/HTTP/turn test. |
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
until new route recordings are vendored. This evidence uses controlled HTTP fixtures; it does
not assert successful authentication to real external MCP services.

The source and verification ownership decisions, API contract and Grok Build reuse assessment
are in [`docs/plugin-registry.md`](../../plugin-registry.md). No Grok Build code was copied or
added as a dependency. Its desktop token-file store does not replace this account-scoped vault.
