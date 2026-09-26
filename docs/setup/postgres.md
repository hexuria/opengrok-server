# Postgres

OpenGrok uses its own database on the gateway's existing dev Postgres instance, so a developer
runs one database server rather than two.

## Standing it up

```sh
# the gateway's dev compose provides Postgres on host port 5452 (container oag-dev-postgres-1)
cd /Volumes/goldcoders/OSS/open-ai-gateway && just dev

# OpenGrok's database
docker exec oag-dev-postgres-1 psql -U oag -d postgres -c 'create database opengrok'
```

```
OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok
```

That database is **scratch**: it is gone after the next Docker restart (the trap, below). For a
demo, or anything else whose data somebody will miss, use
[a database that survives a restart](#a-database-that-survives-a-restart) instead.

Migrations run **in-process at startup under a Postgres advisory lock** — no migration command
to run, and a second replica starting at the same moment waits rather than racing.

## The databases the gate expects

`scripts/gate.sh --smoke` owns its own database and refuses to share it with a running server
(the autonomy sweeps would race). Create these once, all on the same instance:

```sh
docker exec oag-dev-postgres-1 psql -U oag -d postgres -c "create database opengrok_gate" 2>/dev/null || true
for db in opengrok_s17_gate opengrok_s18_gate opengrok_s19_gate opengrok_s21_gate; do
  docker exec oag-dev-postgres-1 psql -U oag -d postgres -c "create database $db" 2>/dev/null || true
done
```

The gate is run with `OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate`; the
identity, account, console and org-keys smokes derive `…_s17/_s18/_s19/_s21_gate` from it
themselves. The loop is the same list `.github/workflows/ci.yml` creates, and
`crates/opengrok/tests/setup_docs.rs` fails when the two disagree.

## The trap: the dev Postgres has no volume

**Found the hard way, 29 Aug 2026.** `oag-dev-postgres-1` runs with no volume mount, so PGDATA
lives in the container's writable layer. Restarting Docker Desktop recreates the container and
**every database on it is gone** — the gateway's and ours.

What that looks like, so it is recognised rather than debugged:

| Symptom | Actually |
|---|---|
| OpenGrok hangs at boot with no log line and no port | Postgres unreachable (the pool times out in 10 s and names the host) |
| `FATAL: database "opengrok" does not exist` | the database was wiped; recreate it as above |
| open-ai-gateway serves `/health/live` but drops every authenticated request | its `api_key`/`account`/`model_catalog` tables are gone; its migrations must re-run and its provider credentials be re-added (the operator's) |

OpenGrok itself recovers by restarting — it re-applies its own schema on boot. Treat everything
on that container as scratch, never as somewhere a demo's data can live. The gate's databases
belong there (several smokes `docker exec` into it by name); data somebody will miss does not.

## A database that survives a restart

Give OpenGrok's data its own container, on a **named volume**, and leave the gateway's container
for the gate:

```sh
docker volume create opengrok-pgdata
docker run -d --name opengrok-postgres --restart unless-stopped \
  -e POSTGRES_USER=oag -e POSTGRES_PASSWORD=oag -e POSTGRES_DB=opengrok \
  -p 127.0.0.1:5453:5432 \
  -v opengrok-pgdata:/var/lib/postgresql \
  postgres:18
```

```
OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5453/opengrok
```

**The mount path depends on the image's major version, and the wrong one looks like it works.**
From 18, the official image's `VOLUME` is `/var/lib/postgresql` and `PGDATA` is
`/var/lib/postgresql/18/docker`. On 17 and earlier, the mount goes at `/var/lib/postgresql/data`,
and the image's own documentation warns that a mount at the parent path does not persist the
data there. Check with `docker inspect -f '{{json .Config.Volumes}}' postgres:<tag>` before
choosing. The port is published on loopback only: nothing off this machine needs the database.

Moving an existing install onto it loses nothing if you **dump first**. The old container's data
lives in its writable layer and goes with it. Take a backup (below), start the new container,
and restore into it.

A **managed Postgres** works the same way: point `OG_DATABASE_URL` at a database OpenGrok owns.
The schema step takes a session-level `pg_advisory_lock`, applies the schema and unlocks, as three
statements on one connection (`migrations.rs`). So the connection must be a real session. Behind
a transaction-mode pooler, those three statements can land on different server connections.

The gateway's own tables (keys, accounts, the catalogue) still live on `oag-dev-postgres-1` and
are still wiped by a restart. Giving that container a volume is a change to open-ai-gateway's
compose, not to this repo. Until it is made there, back its database up alongside ours
(below).

## Backup and restore

A backup is **the whole `opengrok` database**, not just `events`. Not every table is a
projection a replay could rebuild: `pending_user_message`, `monitor_cursor` and the
`local_exec_*` tables are written directly (`crates/opengrok-store/src/pending.rs`,
`autonomy.rs`, `postgres.rs`). Run the dump **inside** the container. A host `pg_dump` older than
the server refuses to dump it.

```sh
# a running server is fine: pg_dump reads one consistent snapshot
docker exec opengrok-postgres pg_dump -U oag -Fc opengrok > opengrok-$(date +%Y%m%d-%H%M).dump
# the gateway's database at the same moment: coworker keys live there
docker exec oag-dev-postgres-1 pg_dump -U oag -Fc oag > oag-$(date +%Y%m%d-%H%M).dump
```

No `-t` on `docker exec`: a pseudo-terminal rewrites line endings in the binary dump.

**Keep these beside the dump, or it restores into something unusable:**

- `OG_CREDENTIAL_KEK`. Connector credentials are sealed under it, and a different KEK cannot open
  them.
- `OG_TOKEN_SECRET`. With a different value, every session and every bot key minted before the
  dump stops verifying.

Both are secrets. Store them the way `.env` is stored (mode 600), never in the dump's filename,
a ticket or a transcript.

**Not in the dump:** the coworkers' computers. Docker containers and box.ascii.dev boxes live
outside Postgres, so restored rows can name a box that no longer exists.

**Restore with the server stopped.** A running server's recovery and autonomy sweeps claim work
(`for update skip locked`), the same race [`gate.md`](gate.md) guards against, and would act on
a half-restored database. Restore into a **fresh** database, then boot:

```sh
pgrep -x opengrok      # must print nothing; stop the server first (running.md)
docker exec opengrok-postgres psql -U oag -d postgres -c 'drop database if exists opengrok'
docker exec opengrok-postgres psql -U oag -d postgres -c 'create database opengrok'
docker exec -i opengrok-postgres pg_restore -U oag -d opengrok --no-owner < opengrok-<stamp>.dump
scripts/serve.sh
```

Booting re-applies the schema over the restored tables. Every statement is idempotent (below),
so a dump taken by an older binary comes up on the newer schema. The reverse is not promised:
restoring a newer dump under an older binary can leave columns that the old code does not know.

Rehearsed 25 Sep 2026 on a local Postgres 16 with the host's own `pg_dump`/`pg_restore`: a dump
of a database holding a CLI-made admin, restored into a freshly created database. The
`opengrok admin` CLI then re-applied the schema over it and refused to recreate that admin
("already exists"), which is the proof the rows came back. The Docker commands above are the same
tools, run inside the container.

## Data-transforming migrations

Today's schema (`crates/opengrok-store/src/migrations.rs`) is one script, `SCHEMA`, of `create …
if not exists` and `alter … add column if not exists`. Every boot, from every replica, takes one
advisory lock (`MIGRATION_LOCK_KEY`, which must never change, or two versions overlapping on one
deploy take different locks) and replays `SCHEMA` **when its SHA-256 is not yet in
`schema_applied`**: once per change, in full, so every statement in it runs again on a database
that already went through it and must be safe to. An unchanged schema is not replayed, because a
bare `alter` takes ACCESS EXCLUSIVE before finding nothing to do and held it for the whole script,
which deadlocked live reads against every boot. `EVERY_BOOT` runs after it on every boot: the
UPDATEs that must also catch rows an older replica writes mid-deploy, which take row locks only.

A migration that **changes data** follows the same rule. It must be correct on its hundredth run
and on a database that already went through it:

1. **Guard it with its own predicate**, so a second run finds nothing to do. For example,
   `update coworkers set x = … where x is null` after `alter … add column if not exists x`.
   Never "update everything", which relies on running once.
2. **Keep it in `SCHEMA`, after the statement it depends on.** The script goes to Postgres as
   one multi-statement query under the lock, which Postgres runs as a single implicit
   transaction. A later statement sees an earlier one's result, and a failing statement rolls
   back the whole step, so the next boot tries it again. The same fact rules out anything that
   refuses to run inside a transaction, such as `create index concurrently`.
3. **Keep the old shape readable until every replica runs the new code.** Two versions overlap
   during a deploy, so add a column and backfill it first, and drop the old column in a later
   release. A transform that must also catch rows the older replica keeps writing (a grant of
   the old built-in set) goes in `EVERY_BOOT`, not `SCHEMA`, and must take row locks only: an
   UPDATE with its own predicate, never DDL.
4. **Rehearse it on a restored dump** (above) before it runs anywhere real.

The first transform that **cannot** be made idempotent (one that must run exactly once) is the
point to add a `schema_migrations (name text primary key, applied_at timestamptz)` table. That
table would be created in `SCHEMA` like everything else, written under the same lock, and a
missing row would mean "not yet run". An existing deployment is the baseline, because every
statement before it was idempotent. Until that transform exists, the table would be a mechanism
with nothing to guard.
