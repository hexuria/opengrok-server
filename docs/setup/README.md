# Setting up OpenGrok

One file per topic, in the order you actually need them. Each step names real commands against
the real defaults; when a value here disagrees with `scripts/gate.sh` or `.env.example`, that is
a bug in the docs — fix the doc in the same commit as whatever you learned.

| Step | File | You are done when |
|---|---|---|
| 1 | [`postgres.md`](postgres.md) | `psql` reaches the dev Postgres and the `opengrok` database exists |
| 2 | [`environment.md`](environment.md) | `.env` exists with the required secrets generated |
| 3 | [`running.md`](running.md) | `curl http://127.0.0.1:1447/health` answers `{"ok":true,…}` |
| 4 | [`gate.md`](gate.md) | `scripts/gate.sh --smoke` passes end to end |
| 5 | [`desktop-client.md`](desktop-client.md) | the packaged app boots against your server in OpenGrok server mode |
| 6 | [`tls.md`](tls.md) | (only for the MCP door's OAuth) `curl https://<lan>:1448/health` answers through Caddy with a trusted certificate |

Prerequisites, once per machine: a Rust toolchain (edition 2024), Docker Desktop (for the dev
Postgres and local-Docker computers), `psql`, `jq`, and — only for desktop-client work — the
packaged client from `/Volumes/goldcoders/OSS/opengrok`.

The web console needs no separate build step for development: build it once
(`cd web && bun install && bun run build`) and point `OG_WEB_CONSOLE_DIR=web/dist` at the
output; the server serves it at `/console`.
