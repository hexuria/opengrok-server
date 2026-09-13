# grok-box — a self-hosted computer

The third `Computer` impl. Docker-runs the grok-box guest from
[`hexuria/box`](https://github.com/hexuria/box) and speaks its HTTP wire
(`POST /v1/exec`, `GET|PUT /v1/files`, `GET /v1/ready`) instead of `docker exec`.
The guest has a noVNC desktop on `:6080`. `DockerComputer` stays the headless
debian path; box.ascii.dev stays the rented path. Enabling grok-box does not
remove either.

EnsureBox in that repo is a **demo** of create / ready / stop / destroy. OpenGrok
already owns sharing, idle-stop, and `/console`. We ported the docker-run + poll-ready
+ guest-HTTP shape, not the Node control plane.

`BOX_TOKEN` is minted per guest, injected as container env, recovered later via
`docker inspect`, and **never** stored in Postgres, returned to `/console`, or put
on `getForeverBoxStatus.vncUrl`. The VNC password in that URL is independent
(x11vnc uses the first 8 characters).

`OG_HOSTED=1` refuses grok-box the same way it refuses local Docker: untrusted guest
containers must not run on a multi-tenant API host.

## Build the guest image

From a checkout of `hexuria/box`:

```sh
docker compose build
docker image inspect grok-box:local
```

Compose tags `grok-box:local` (`docker-compose.yml` `image:`). A different tag is
`OG_GROK_BOX_IMAGE`. You do **not** `docker compose up` for OpenGrok — the server
`docker run`s its own copies so per-bot isolation can be more than one container,
and so host ports are random (`127.0.0.1::1337` etc.) rather than Compose's fixed
1337/1340/6080.

Confirm the image, then:

```sh
# optional: make every new hire a grok-box even before the console Enable
OG_COMPUTER=grok-box
OG_GROK_BOX_IMAGE=grok-box:local
# OG_GROK_BOX_SCREEN_HOST=127.0.0.1   # default; rewrite only the URL host, not the bind
scripts/serve.sh
```

The usual path is to leave `OG_COMPUTER` unset (local Docker at boot) and **Enable**
grok-box on `/console` — that is the org preference `kind_for_new` reads, and it
wins over a saved box.ascii.dev key for *new* computers. Existing ascii /
local-docker mappings keep their kind until you reset that computer.

## Enable it on `/console`

1. Build the console if you have not (`cd web && bun install && bun run build`) and
   set `OG_WEB_CONSOLE_DIR=web/dist`.
2. Sign in as the org admin → **Computers**.
3. **grok-box (self-hosted)** → Guest image `grok-box:local` (blank uses that
   default) → **Enable**.
4. **Test connection** — create, wait up to 90s for `GET /v1/ready`, destroy.
   Success copy may include a screen URL. It must not mention `BOX_TOKEN`.
   Test is allowed before Enable (it uses the deployment image).

The sealed vault value is the **image name** (or `grok-box:local` when the field is
blank), never a token. Remove goes back to ascii if that key is still saved, else
local Docker when the process booted that way.

`OG_COMPUTER=grok-box` makes the boot provider grok-box even without an org secret.
Then the **image** is `OG_GROK_BOX_IMAGE`, not a later console paste — the boot
handle is what the run path uses. Enable still records the org preference for a
process that booted as local Docker.

## Sharing modes (same scopes as ascii)

Default sharing lives on the Computers card. Per-member override is the dropdown
on each row in **Users**.

| Mode | Scope | What you should see locally |
|---|---|---|
| **per-org** | one guest for the whole org | two members' bots share one `/workspace` |
| **per-account** (default) | one guest per person | two bots of the same person share; another person gets another guest |
| **per-bot** | one guest per bot | two bots of the same person do **not** share a filesystem |

The override on a user row wins over the org default (`resolve_mode`: account row,
else org row, else `per-account`).

Existing mappings are kept when you flip the mode. A new hire (or
`resetForeverBox` in the desktop) is what creates a guest under the *current* mode
and kind. Per-org also eager-provisions when you select it, so the shared guest can
exist before anyone's first bot.

### Local isolation check

With the image present:

```sh
cargo test -p opengrok-box --test against_a_real_grok_box -- --nocapture
cargo test -p opengrok-server --test against_grok_box --features opengrok-server/mock-fixtures
```

The first skips without Docker or `grok-box:local`. It proves HTTP exec/files, a
`screen_url` that is noVNC and not a `BOX_TOKEN`, stop keeping volumes, destroy
removing them, and that two creates do not share a workspace (the per-bot shape).

The server test needs `OG_DATABASE_URL`. It uses a recording stand-in (no image)
and proves `kind_for_new` prefers grok-box when enabled (ascii still builds),
per-account / per-bot / per-org / a per-member override, and
`getForeverBoxStatus.vncUrl`.

Hand check after Enable:

1. Set the org mode to **per-bot**. Hire two coworkers on one account. Each
   container is `og-gb-*`; `docker exec` is the wrong tool — write a file through
   one bot and confirm the other cannot read it (or `GET` that path on the other
   guest's published 1337 with its own token — you will not have the token; use the
   bots).
2. Flip to **per-account**, reset both computers, hire again: one `og-gb-*` for
   that person.
3. Two accounts, **per-org**, reset: one `og-gb-*` for the org. A file written by
   A's bot is in B's workspace.
4. Leave per-org, set B's Users dropdown to **per-bot**, reset B's bots: B no
   longer shares A's filesystem.

`docker ps --filter label=dev.opengrok.kind=grok-box` lists guests. Ports are
published on **loopback only** — 6080 is not Bearer-authenticated.

## Screen URL

`getForeverBoxStatus.vncUrl` is `http://{OG_GROK_BOX_SCREEN_HOST|127.0.0.1}:{host-port}/vnc.html?autoconnect=1&resize=scale&password=…`
once `GET /v1/ready` succeeds. Open it on the **same machine** as Docker. The
desktop client's loopback refusal is the **gateway** host, not this viewer.

`OG_GROK_BOX_SCREEN_HOST` rewrites only the host in that URL. Published ports stay
on `127.0.0.1`. A viewer on another machine needs an SSH tunnel to the published
6080, not a public bind — 6080 has no Bearer.

Seam-B `EnsureSandBox.vnc_url` is still the empty string. That is a known gap; the
packaged app's live screen is `getForeverBoxStatus`, not the grpc mint.

## What this PR does not do

Computer Use (`POST /v1/cua/screenshot|click|type|…`) is implemented **in the
guest** and not yet on the `Computer` trait. Shell, files, and `screen_url` are
the first verify. Follow-up: default-unsupported CUA methods on the trait so
AsciiBoxes / DockerComputer keep compiling.

## Env

| Variable | Default | What it is |
|---|---|---|
| `OG_COMPUTER` | auto | `grok-box` selects this provider at boot; see [`environment.md`](environment.md) |
| `OG_GROK_BOX_IMAGE` | `grok-box:local` | guest image when the org has not saved one (or saved `enabled`) |
| `OG_GROK_BOX_SCREEN_HOST` | `127.0.0.1` | host written into `screen_url` |
| `OG_BOX_RUN_TAG` | unset | also labelled on grok-box containers **and** volumes; `scripts/gate.sh` removes both |
| `OG_HOSTED` | unset | `1` refuses grok-box and local Docker |
