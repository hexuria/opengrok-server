# Setting up OpenGrok

One file per topic, in the order you actually need them. Each step names real commands against
the real defaults; when a value here disagrees with `scripts/gate.sh` or `.env.example`, that is
a bug in the docs — fix the doc in the same commit as whatever you learned.

| Step | File | You are done when |
|---|---|---|
| 1 | [`postgres.md`](postgres.md) | `psql` reaches the dev Postgres and the `opengrok` database exists — on a named volume if its data should outlive a Docker restart |
| 2 | [`environment.md`](environment.md) | `.env` exists with the required secrets generated |
| 3 | [`running.md`](running.md) | `curl http://127.0.0.1:1447/health` answers `{"ok":true,…}` |
| 4 | [`first-run.md`](first-run.md) | the first admin signs in at `/console`, a hired coworker's Test says it can call tools, and one real turn shows `TOOL_CALL_*` events and `reason=Passthrough` at the gateway |
| 5 | [`gate.md`](gate.md) | `scripts/gate.sh --smoke` passes end to end |
| 6 | [`tls.md`](tls.md) | `curl https://<lan>:1447/health` answers through Caddy with a trusted certificate, with the server itself on loopback |
| 7 | [`site-logins.md`](site-logins.md) | `OG_CREDENTIAL_KEK` is backed up beside the database backup and `opengrok vault status` exits 0 — before anyone saves a login or passkey, or enrols their Mac |
| 8 | [`nativechat.md`](nativechat.md) | the client is signed in to this server, and a first chat, a consent card, the coworker's computer and a routine have each been seen — the demo runbook |

Prerequisites, once per machine: a Rust toolchain (edition 2024), Docker Desktop (for the dev
Postgres and local-Docker computers), `psql` and `jq`.

`desktop-client.md` is no longer a step: the client it describes is discontinued and the two
doors it used were deleted on 20 Sep 2026. It is kept as the record of how they were wired.
Its replacement is step 8, [`nativechat.md`](nativechat.md), for the client served today.

The web console needs no separate build step for development: build it once
(`cd web && bun install && bun run build`) and point `OG_WEB_CONSOLE_DIR=web/dist` at the
output; the server serves it at `/console`.
