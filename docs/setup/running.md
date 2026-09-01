# Running the server

## The short version

```sh
scripts/serve.sh          # build, stop whatever is running, start from .env
curl -fsS http://127.0.0.1:1447/health   # {"ok":true,…}
```

`serve.sh` sources `.env`, builds `target/debug/opengrok`, safely stops an already-running
server, and starts the new binary. It refuses to run when `OG_DATABASE_URL` points at the
gate's database (`…/opengrok_gate`) — a dev server there would race the smoke suite's sweeps.

## Restarting by hand — the three gotchas, learned the hard way

If you manage the process yourself instead of using `serve.sh`:

1. **Find it with `pgrep -x opengrok`, never `pgrep -f`.** `-f` matches your own shell wrapper
   (and then you kill yourself); a script that `exec`s the binary keeps the wrapper's pid, so
   the pid you remember may not be the pid that matters. `-x` matches the process name alone.
2. **SIGTERM drops the listener but the process can outlive it** — graceful shutdown holds open
   SSE connections (a connected daemon or client keeps it draining). Wait a moment, then
   `kill -9` what `pgrep -x opengrok` still shows.
3. **The port outlives the kill by a moment.** Poll `/health` until it stops answering before
   starting the next binary, or the new process fails to bind while the old one drains.

## Deterministic windows — driving consent cards with no spend

For demonstrating or verifying the tool/consent path without a provider call, run a window with
the tool-asking mock door and a canned judge verdict:

```sh
OG_MODEL_DOOR=mock-tools OG_AUTO_REVIEW_MOCK_VERDICT=ask scripts/serve.sh
```

Every turn then asks for exactly one `shell` call on the coworker's box, and the auto-review
judge answers `ask` — one card per prompt, deterministically. Restore the real window by
running `scripts/serve.sh` again without the overrides.

## What is listening where

| Port | What | Why this number |
|---|---|---|
| `1447` | everything: Sand gateway, AG-UI, auth, `/console` | 1337 (the compiled default) clashes with grok-bot's local-docker box host |
| `1449`+ | the gate's smoke servers (`OG_PORT=1449` recommended when a live server holds 1447) | the gate **kills whatever holds its port** before starting — point it away from a server you care about |
| `29080` | open-ai-gateway's inference listener (a separate process today; embedding it is designed, `PLAN.md` §3) | its own default |
| `5452` | the dev Postgres | the gateway's compose |

## Logs

Tracing goes to stdout under `RUST_LOG`. Request-level visibility is on by default: one line per
request (path, status, ms, and the ones that can never match — a 0-length bearer, an Origin the
gateway refuses), and one line each when an `/events` stream opens and closes, with how many
subscribers are left. `OG_TRACE_REQUESTS=0` turns it off.

Every request has an `X-Request-Id`. The desktop client sends one per gateway call and per SSE
connect; the server mints a UUID when a caller does not, and echoes it on the response either way.
The handler runs inside a span carrying it, so the request line and everything logged while
serving it share one key:

```sh
grep 'id=desk-0x1f' server.log          # one call, start to finish
grep 'events: stream' server.log        # was the stream up at 03:16, and for whom
```
