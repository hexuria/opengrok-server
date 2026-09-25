# First run: a gateway key, the first admin, a first hire, a real turn

Steps 1–3 leave a server that answers `/health` and that **nobody can sign in to**. Signup only
redeems an invite, and an account made by signup lands `enabled=false` until an admin enables it
(`crates/opengrok-server/src/auth/identity.rs`). A fresh database has no admin, so the first one
is made from the operator's shell. On purpose: there is no HTTP surface for it, because a fresh
server has nobody to authorize one (`crates/opengrok/src/admin.rs`).

This page takes a fresh install to a signed-in admin, a hired coworker and one real, tool-calling
turn through open-ai-gateway. Every command is copied from the CLI's own usage text or from the
smokes that exercise it (`scripts/slice17-identity-smoke.sh`), not paraphrased.

## 1. An inference key from open-ai-gateway

Every model call exits through the gateway (CLAUDE.md #4). The server boots without a key, but
every real turn then fails. Mint an **inference** key in the open-ai-gateway checkout
(`docs/research/gateway-open-ai-gateway.md`, "Creating a key"):

```sh
# in open-ai-gateway, after its own cold start (`oag admin init`, a provider account, a route)
oag admin key create --email <you> --name opengrok      # or: just key name=opengrok
```

Put the printed `oag_live_…` key in `.env` as `OG_GATEWAY_TOKEN`. The key is shown once. Write it
straight into the file (mode 600) and never paste it into a transcript. Use the plain
**inference** key here, not an admin one (`--admin`): crossing the two fails in a way that looks
like a broken deployment (see the `OG_GATEWAY_ADMIN_TOKEN` row in
[`environment.md`](environment.md)). `OG_GATEWAY_URL` defaults to `http://127.0.0.1:29080`.

The default hire route is `xai/grok-4.6` (`OG_MODEL`, and see
[`environment.md`](environment.md) for why). The gateway's route needs a credential for the xAI
provider to serve it. If you pin coworkers elsewhere, the route needs a credential for each
provider you pin to.

Restart with `scripts/serve.sh` so the key is read.

## 2. The first org and its admin

The CLI talks to the database directly (it reads only `OG_DATABASE_URL` and applies the schema
under the same lock the server uses), so it works before, or while, the server runs:

```sh
set -a; . ./.env; set +a
target/debug/opengrok admin org create --name "Acme Inc" --admin-email you@acme.test --domain acme.test
```

It prints the org id and a **generated password once** — save it. Leave `--password` off on
purpose: a password on the command line lands in shell history and in the process table.
`--domain` is required, and the admin's own email domain must be one of its values (comma-separate
several), or the admin could not sign in. An unknown flag is refused rather than ignored.

The binary is `target/debug/opengrok` once `scripts/serve.sh` has built it. On an installed box it
is the release binary, with the same `admin …` arguments.

**Alternative:** `cargo run -p opengrok --bin bootstrap` does the same from environment variables
(`BOOTSTRAP_EMAIL`, `BOOTSTRAP_PASSWORD` of at least 8 characters, optional `BOOTSTRAP_ORG`,
`BOOTSTRAP_DOMAIN`), and also prints an invite code for the next member. Its doc comment is in
`crates/opengrok/src/bin/bootstrap.rs`. Use one or the other: both refuse an email that already
has an account.

Everyone after the admin joins by invite. Run `target/debug/opengrok admin invite --org <org_id>`,
or use the console's admin page. The new account then needs enabling, either on that page or with
`opengrok admin account enable --email <email>`.

## 3. Sign in to the console

The console is a built SPA the server serves at `/console`:

```sh
(cd web && bun install && bun run build)
# in .env:  OG_WEB_CONSOLE_DIR=web/dist
scripts/serve.sh
```

Open `http://<host>:1447/console/login` and sign in with the admin's email and the saved password.
Behind TLS ([`tls.md`](tls.md)), use the `https://` address and set `OG_COOKIE_SECURE=1`. The
session is a pair of httpOnly cookies; no token is ever held in the page.

## 4. Hire a coworker, and prove its route can act

On `/console/coworkers`, give it a name. Leave the route blank for the deployment's default, or
type one. Press **Test** before **Hire**. The probe asks the gateway for one real, tiny completion
that offers a single tool:

| The chip says | Meaning |
|---|---|
| `answered as …` | the route serves **and** called the tool — a coworker on it can use its computer |
| `answered as …, but did not call the offered tool …` | the route talks but cannot act: no tool call, so no policy gate or consent card ever fires. Pick another route |
| the gateway's own sentence | the route cannot be served on this key (e.g. no credential for that provider) |

Then press **Hire**.

## 5. One real turn, confirmed at the gateway

The console has no chat. Clients send turns to `POST /ag-ui` with a bearer token. This step does
the same with `curl`, using the access token from the console's own login:

```sh
OG=http://127.0.0.1:1447
curl -fsS -c /tmp/og.jar -H 'content-type: application/json' \
  -d '{"email":"you@acme.test","password":"<saved password>"}' "$OG/auth/login" >/dev/null
TOKEN=$(awk '$6=="og_access"{print $7}' /tmp/og.jar)
curl -fsS "$OG/coworkers" -H "authorization: Bearer $TOKEN" | jq -r '.[] | "\(.id)  \(.model)  \(.name)"'

CW=<the coworker id>
curl -sN -X POST "$OG/ag-ui" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d "{\"threadId\":\"first-run\",\"runId\":\"first-run-$(date +%s)\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"Run uname -a on your computer and tell me what it printed.\"}],\"forwardedProps\":{\"coworkerId\":\"$CW\"}}" \
  --max-time 120 | sed -n 's/^data: //p' | jq -r '.type' | uniq -c
rm -f /tmp/og.jar
```

What a working install shows:

- **`TOOL_CALL_START` … `TOOL_CALL_RESULT`** in the stream: the route called a tool and the
  coworker's computer ran it.
- **The gateway's log says `reason=Passthrough`** for this request, with the coworker's own pin
  (`xai/grok-4.6` on the default). That is the proof the pin reached the gateway. `reason=Classified`
  means the model never arrived and the gateway is guessing, which is a door problem
  ([`environment.md`](environment.md), "Which door to use when").

A `401 authentication failed` on every turn is the crossed-keys case from step 1. A reply of
nothing but text, with no `TOOL_*` events, is a route that cannot act. Test it in the console.

## What this page does not cover

- A database that survives a Docker restart, and backups: [`postgres.md`](postgres.md).
