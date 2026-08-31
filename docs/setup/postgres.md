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

Migrations run **in-process at startup under a Postgres advisory lock** — no migration command
to run, and a second replica starting at the same moment waits rather than racing.

## The databases the gate expects

`scripts/gate.sh --smoke` owns its own database and refuses to share it with a running server
(the autonomy sweeps would race). Create these once, all on the same instance:

```sh
for db in opengrok_gate opengrok_s17_gate opengrok_s18_gate opengrok_s19_gate; do
  docker exec oag-dev-postgres-1 psql -U oag -d postgres -c "create database $db" 2>/dev/null || true
done
```

The gate is run with `OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok_gate`; the
identity/account/console smokes derive `…_s17/_s18/_s19_gate` from it themselves.

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
on that container as scratch, never as somewhere a demo's data can live. The fix worth making
once: give the container a named volume.
