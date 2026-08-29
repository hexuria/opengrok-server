# Standing P1 up

How to point the Grok Bot desktop client at a local OpenGrok and prove P1 is done. Written because
the prerequisite chain is four separate things and none of them is guessable.

**Read [`PLAN.md`](PLAN.md) §2 first** — especially the loopback trap. It is the failure that looks
like a bug in your code and is not.

---

## 0. Go / no-go: do you have the client?

grok-bot's shipped renderer is **git-ignored**. It is hydrated by `npm run bootstrap` from a
checksum-verified Grok Bot 0.18.0 DMG that the operator must already possess
(`opengrok/PROVENANCE.md`; reference §1.2).

```sh
cd /Volumes/goldcoders/OSS/opengrok && ls frontend/dist 2>/dev/null || echo "renderer not hydrated"
```

**If you cannot hydrate the renderer, stop and ask the operator.** Do not improvise a client. There
is a fallback that still lets you finish and verify P1 — the curl transcript in §5 — but the
"desktop app lists coworkers" demo is not available without it, and the plan should say so out loud
rather than have you discover it at the end.

---

## 1. The four environment facts

| Variable | Set on | Value | Why |
|---|---|---|---|
| `SAND_HOST_GATEWAY_URL` | the client | `http://opengrok.local:1337` | the repoint. **Must not be loopback** — see below |
| `SAND_HOST_GATEWAY_TOKEN` | the client | any shared secret | must equal the server's token |
| `SAND_GATEWAY_TOKEN` | OpenGrok | the same secret | the pair authenticates the host |
| `SAND_BACKEND_URL` | the client | `http://127.0.0.1:8787` | points seam B at the repo's mock, not `api2.cursor.sh` |

### The loopback trap, concretely

`createSettingsRoutedHostConnector` (`local-docker-host-connector.ts:437-443`) **throws** when the
resolved gateway host starts with `127.0.0.1` or `localhost`, unless `boxRuntime === "local-docker"`
— which then ignores `SAND_HOST_GATEWAY_URL` entirely and spawns its own host on port 1350. So a
hostname alias is required. **This step needs sudo and is the operator's to run:**

```sh
echo "127.0.0.1 opengrok.local" | sudo tee -a /etc/hosts
```

Then bind OpenGrok to `0.0.0.0:1337` and address it as `opengrok.local`.

### Why seam B needs the mock

The renderer refuses to call `listAgents` at all unless the account state is `logged-in`
(reference §7). That state comes from the Cursor ConnectRPC backend — seam B, which is **not ours**.
grok-bot ships a mock for exactly this:

```sh
cd /Volumes/goldcoders/OSS/opengrok && npm run mock   # serves seam B on :8787 — check package.json for the exact script name
```

---

## 2. Postgres

**Chosen default** (an engineering default, not an operator decision — change it if you disagree,
but change it *here*): OpenGrok uses its own database on the gateway's existing dev Postgres
instance, so a developer runs one database server rather than two.

```sh
# the gateway's dev compose already provides Postgres on host port 5452
cd /Volumes/goldcoders/OSS/open-ai-gateway && just dev
createdb -h 127.0.0.1 -p 5452 -U openbot opengrok   # or psql -c 'create database opengrok'
```

```
OG_DATABASE_URL=postgres://oag:oag@127.0.0.1:5452/opengrok
```

Migrations live in `crates/opengrok-store/migrations/` and run **in-process at startup under a Postgres
advisory lock**, matching the gateway's pattern — so a second replica starting at the same moment
waits rather than racing. `.sqlx` offline data is committed, so CI needs no database.

---

## 3. OpenGrok's own configuration

Every knob, so nothing has to be invented. Copy `.env.example` to `.env`.

| Variable | Default | What it is |
|---|---|---|
| `OG_BIND` | `0.0.0.0:1337` | where the Sand gateway listens. Not loopback — see §1 |
| `OG_DATABASE_URL` | — | Postgres. Required |
| `SAND_GATEWAY_TOKEN` | — | the shared secret with the client |
| `OG_GATEWAY_URL` | `http://127.0.0.1:29080` | open-ai-gateway's inference listener |
| `OG_GATEWAY_TOKEN` | — | an `oag_live_…` key. **Never a provider key** |
| `OG_BOX_API_KEY` | — | box.ascii.dev (`box_…`). P3; unset is fine before then |
| `RUST_LOG` | `opengrok=debug,opengrok_server=debug` | tracing |

---

## 4. What P1 actually implements

The client's own boot sequence, in order. Sources: `host-supervisor.ts:135-168`,
`node-agent-coordinator/main.ts:157-172`, `coordinator-resync.ts:7-8`, and the renderer effects —
full table at reference §9.

| # | The client calls | You must answer | If you skip it |
|---|---|---|---|
| 1 | `GET /health` (**1500 ms deadline**, 5 s TTL) | `{ok:true, pid, isBusy:false, activeAgentId, startedAt, lastBusyAtMs}` | the supervisor discards the connection and re-resolves forever |
| 2 | `GET /events` (`Accept: text/event-stream`) | 200, then `retry: 1000\n\n`, then `:ping\n\n` at **≤15 s** | a 35 s watchdog aborts → reconnect loop, `transport-down` |
| 3 | `listAgents` (on `transport-connected`) | a JSON **array** of roster summaries | the sidebar never seeds |
| 4–11 | the resync chain: `setHostSettings` × 7, `getHostSettings` once | any record; `getHostSettings` returns the full settings record (reference §9) | logged failures only, except MCP never reconciles |
| 12 | `setWindowFocused` | void | — |
| 13 | `listAgents` again (renderer `refreshRoster`) | the same array | **empty sidebar** |
| 14 | `countAgents` | a **number** | the onboarding screen instead of the app |
| 15 | `isAgentNetworkEnabled` | boolean | agent-network UI hidden (fails closed, harmless) |
| 16 | `getTrays` | an **array** | `getTrays returned a malformed array reply` — a throw |
| 17 | `isGlobalSearchEnabled` | boolean | search hidden |
| 18 | `getForeverBoxStatus` | `null` **or** `{agentId, state, …}` | `malformed box status` — a throw |
| 19 | click a coworker → `openAgentTail {id, limit:200}` | `{entries:[…], nextBeforeSeq?}` | transcript load error banner |

Everything after this is P2 (`sendPrompt`, then streaming the answer over the SSE `transcript`
channel).

> **The reply shape is as load-bearing as the reply.** A number that should be an array throws; an
> empty array that should have a row renders as a working app with no coworkers. See Trap 2 in the
> reference, and the comment on `P1_COMMANDS` in `crates/opengrok-wire/src/command.rs`.

---

## 5. Proving it — the acceptance check

CLAUDE.md #10 says evidence or it doesn't ship, so P1 ends with a script, not a screenshot.
`scripts/p1-smoke.sh` (write it as part of P1) must assert:

```sh
# 1. health, within the client's own deadline
curl -fsS --max-time 1.5 http://opengrok.local:1337/health | jq -e '.ok == true'

# 2. the event stream opens, announces its retry, and pings inside 15s
curl -fsS -N --max-time 20 -H 'Accept: text/event-stream' http://opengrok.local:1337/events \
  | head -c 400 | grep -q 'retry: 1000'

# 3. the shapes the renderer will throw on
curl -fsS -X POST http://opengrok.local:1337/api/listAgents  | jq -e 'type == "array" and length > 0'
curl -fsS -X POST http://opengrok.local:1337/api/countAgents | jq -e 'type == "number"'
curl -fsS -X POST http://opengrok.local:1337/api/getTrays    | jq -e 'type == "array"'
curl -fsS -X POST http://opengrok.local:1337/api/getForeverBoxStatus | jq -e '. == null or (.agentId | type == "string")'

# 4. the seeded coworker came from Postgres, not a constant
psql "$OG_DATABASE_URL" -tAc "select count(*) from coworkers" | grep -qv '^0$'
```

**P1 is done when that script exits 0 and the desktop app shows the seeded coworker in its
sidebar.** The script is the part that must keep passing; the screenshot is the part that convinces
a person.

---

## 5b. The dev Postgres has no volume — a Docker restart wipes it

**Found the hard way, 29 Aug 2026.** `oag-dev-postgres-1` runs with **no volume mount**
(`docker inspect oag-dev-postgres-1 --format '{{len .Mounts}}'` → `0`), so PGDATA lives in the
container's writable layer. Restarting Docker Desktop recreates the container and **every database
on it is gone** — the gateway's and ours.

What that looks like when it happens, so it is recognised rather than debugged:

| Symptom | Actually |
|---|---|
| OpenGrok hangs at boot with no log line and no port | Postgres unreachable. Fixed: the pool now times out in 10s and names the host. |
| `FATAL: database "opengrok" does not exist` | the database was wiped; recreate it (below) |
| open-ai-gateway serves `/health/live` but **drops** every authenticated request | its `api_key`, `account` and `model_catalog` tables are gone; a long-running process also holds connections from before the restart |

Recovering:

```sh
docker start oag-dev-postgres-1
docker exec oag-dev-postgres-1 psql -U oag -d postgres -c 'create database opengrok'
# OpenGrok re-applies its own schema on boot; nothing else to do on our side.
# open-ai-gateway needs its migrations re-run AND its provider credentials re-added — those are
# the operator's, and no amount of restarting brings them back.
```

**The fix worth making once:** give that container a named volume. Until then, treat everything in
it as scratch, and never as somewhere a demo's data can live.

## 6. When the roster mysteriously stops updating

Before suspecting OpenGrok: `source/node-agent-coordinator/main.ts:136` **drops every `agents` and
`agent-upserted` SSE event** when the persisted `inferenceProvider` in the client's data root is
`claude-code`, `codex`, or `openrouter` (`usesLocalInference`, `:34-37`). Check `settings.json` in
the data root first. (Reference Trap 9.)
