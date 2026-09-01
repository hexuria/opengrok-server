# The gate

`scripts/gate.sh` runs everything CI runs — literally: since 1 Sep 2026 the workflow calls
this script instead of re-listing its steps, so the two cannot drift. The local run is the
pre-push ritual; CI is the public record. Nothing merges on a red `scripts/gate.sh --smoke`,
local or CI. (Two portability lessons are baked in: CI pins the same toolchain as
`rust-toolchain.toml` — a floating @stable failed lints nobody could reproduce at a desk — and
the smokes reach Postgres via a local `psql` when there is one, the dev container only as a
fallback.)

## Running it

```sh
# checks and tests only: fmt --check, cargo check, clippy -D warnings, cargo test
scripts/gate.sh

# everything, including the 19 smoke scripts (needs Postgres and the built binary)
cargo build -p opengrok        # the gate does NOT rebuild — stale binaries fail mysteriously
OG_PORT=1449 OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate \
  scripts/gate.sh --smoke
```

## What --smoke needs, and why

- **Its own database** (`opengrok_gate`), never a live server's: the autonomy sweeps claim work
  with `for update skip locked`, so a second opengrok on the same database races the smoke
  servers for schedule/monitor firings. The gate refuses to start if another opengrok is
  running on its database.
- **The fixed side databases** `opengrok_s17_gate`/`_s18_gate`/`_s19_gate` (identity, account
  admin, web console) — creation commands in [`postgres.md`](postgres.md).
- **The `oag-dev-postgres-1` container specifically**: several smokes `docker exec` into it by
  name, so a Postgres on another container or port fails with "cannot reach Postgres" even
  when the URL is right.
- **A port of its own** (`OG_PORT=1449` when a live server holds 1447): the gate frees its port
  by killing whatever is listening there. The default is 1447 — pointing it at your live
  server kills the live server.

## Shape

The gate stands up a shared mock-door server for nine smokes, restarts it with the tool-asking
door for two more, then hands over to the scripts that own their whole lifecycle (durability's
SIGKILL mid-run, recovery's planted rows, autonomy's kill-mid-schedule, seam B, browser login,
identity, account admin, web console — on `OG_PORT`+3…+6). Read the comments in
`scripts/gate.sh` itself; each guard in there is a bug that actually happened.
