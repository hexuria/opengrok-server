# The gate

`scripts/gate.sh` runs everything CI runs — literally: since 1 Sep 2026 the workflow calls
this script instead of re-listing its steps, so the two cannot drift. The local run is the
pre-push ritual; CI is the public record. Nothing that changes code merges on a red
`scripts/gate.sh --smoke`, local or CI; which suites a run needs is below ("CI suites"). (Two portability lessons are baked in: CI pins the same toolchain as
`rust-toolchain.toml` — a floating @stable failed lints nobody could reproduce at a desk — and
the smokes reach Postgres via a local `psql` when there is one, the dev container only as a
fallback.)

## Running it

```sh
# checks and tests only: fmt --check, crate sizes, the architecture guard, no value echoed
# into head or grep -q (pipefail makes that a random failure), cargo deny, the formal models,
# cargo check (also --no-default-features), clippy -D warnings, the tests
scripts/gate.sh

# optional, pinned by sha256: what CI installs. Without them the gate skips cargo deny and
# formal.sh loudly and runs the tests with cargo test instead of nextest.
scripts/install-ci-tools.sh    # cargo-deny, cargo-nextest -> ~/.local/bin
scripts/install-tla.sh         # TLC (needs java)
scripts/install-lean.sh        # Lean 4

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
- **The fixed side databases** `opengrok_s17_gate`/`_s18_gate`/`_s19_gate`/`_s21_gate` (identity,
  account admin, web console, org keys) — creation commands in [`postgres.md`](postgres.md).
- **The `oag-dev-postgres-1` container specifically**: several smokes `docker exec` into it by
  name, so a Postgres on another container or port fails with "cannot reach Postgres" even
  when the URL is right.
- **A port of its own** (`OG_PORT=1449` when a live server holds 1447): the gate frees its port
  by killing whatever is listening there. The default is 1447 — pointing it at your live
  server kills the live server.

## The wire corpus NativeChat reads

NativeChat transcribes this server's wire by hand, and nothing else checks that the two still
agree (#255). `tests/fixtures/wire/` is the server's side of that check: every AG-UI frame and
every REST body NativeChat reads, recorded from this repo's own tests. NativeChat copies it in,
pinned by the `server_sha` in its `MANIFEST.json`, and parses every file.

- `scripts/record-wire.sh` records it and writes `tests/fixtures/wire/`. Commit the result when
  a change alters what a route answers or a turn streams.
- The gate's own test run records it too (when `OG_DATABASE_URL` is set and nextest is
  installed), then checks the committed copy. It fails on a frame type, a route and status, or a
  field that the committed copy does not show NativeChat. A field that was not seen this time is
  only noted: some tests read a routine while its run is still in flight, and whether a field
  is `null` or an object depends on timing.
- `MANIFEST.json`'s `emits` is every AG-UI type, CUSTOM name, approval reason and form resolution
  the server CAN send, and the five blocks of the account events stream (`events`, from
  `opengrok_wire::events::EVENTS`). It comes from `opengrok_wire::agui::SENT_TYPES` and
  `CUSTOM_NAMES` and from the enums, not from what the tests happened to reach.
  `the_wire_names_are_all_listed` scans the source and fails when a producer sends a name those
  lists do not hold. A word with no fixture is listed in `unrecorded`.
- The account events stream's blocks (`GET /ag-ui/events`, #348) are not AG-UI frames: their name
  is on the `event:` line, not in their data. They are kept whole, as `{id, event, data}`, under
  `events/<name>/`, one file per shape of each name (`routineId` there or not, `runId` an id or
  `null`), `reset` included. The recorder (`tests/support/wire_record.rs`) tells them by their
  `id:` and `event:` lines. The id is a placeholder: an account's ids say nothing alone.
- Ids and clocks are placeholders in the server's own formats. Secrets are `«redacted»`: keys
  named like tokens, keys and passwords, `Bearer` values, gateway keys, and any JWT anywhere in
  a string.

The recorder (`crates/opengrok-server/tests/support/wire_record.rs`) is compiled only with the
`record-wire` feature, which only the tests turn on, and runs only while `OG_RECORD_WIRE` names a
directory.

## CI suites

`.github/workflows/ci.yml` runs only the suites a change needs; `scripts/ci-scope.sh` decides.

| Suite | What it runs | Time |
|---|---|---|
| `server` | `scripts/gate.sh --smoke`: every check, every test, the smokes | ~7 min |
| `checks` | `scripts/gate.sh --checks`: fmt, crate sizes, architecture, echoed pipes, cargo deny, check, clippy | ~2 min |
| `formal` | `scripts/formal.sh --require`: TLC and Lean on `formal/` | ~1 min |
| `web` | the console: typecheck, test, build | seconds |
| `docs` | `crates/opengrok/tests/setup_docs.rs`, compiled with `rustc` alone | seconds |

| When | Suites |
|---|---|
| Pull request from `doc-*` / `docs-*` | `docs` |
| Pull request from `formal-*` / `tla-*` | `formal` |
| Pull request from `web-*` | `web` |
| Pull request from any other branch that changes code | `server`, `formal`, `web` |
| Pull request from any other branch that changes only docs | `docs` if a test reads one of them, else nothing |
| Push to `main` | `checks`, `formal` |
| Nightly | `server`, `formal`, `web` |
| Actions tab → ci → "Run workflow" | the suite you pick, or `all` |

**A prefix may narrow what runs, never skip tests for code.** A prefixed branch may change only
its own area (`formal/**` and the formal scripts; `web/**`) plus docs. Anything else fails the
`scope` job with the list of files; rename the branch (no prefix runs everything) or move them.
"Docs" means Markdown anywhere or anything under `docs/`.

A push to `main` runs the light set because its pull request already ran the tests on that same
merge result; two merges that each pass and break together are caught by the nightly run. A
skipped job reports success, so a required check stays green when its suite is not needed.

## Shape

The gate stands up a shared mock-door server for seven smokes, restarts it with the tool-asking
door for three more, then hands over to the scripts that own their whole lifecycle (durability's
SIGKILL mid-run, recovery's planted rows, autonomy's kill-mid-schedule, browser login, identity,
account admin, web console, org gateway keys — on `OG_PORT`+3…+7). Read the comments in
`scripts/gate.sh` itself; each guard in there is a bug that actually happened.

## Test databases

`OG_DATABASE_URL` names the gate's database, and each test binary works in its own database
beside it: `opengrok_gate` gives `opengrok_against_monitors_gate`, created on first use
(`gate_database_or_panic` in `opengrok-store`). nextest runs the binaries at once, and a test
that acts on the whole database (a purge, the monitor sweep's cursor) must not see another
binary's rows. The role needs CREATEDB, which `oag` has. They are ordinary `_gate` databases:
drop them whenever you like, and the next run recreates them.
