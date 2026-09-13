# grok-box (`hexuria/box`) — the self-hosted computer

**Researched:** 13 Sep 2026 against [`hexuria/box` `docs/API.md`](https://github.com/hexuria/box/blob/main/docs/API.md)
and `docker-compose.yml`. **Role in OpenGrok:** the third `Computer` impl,
`opengrok_box::GrokBoxComputer` (`crates/opengrok-box/src/grok_box/`). Guest HTTP
stays in that crate. Setup and local verify: [`../setup/grok-box.md`](../setup/grok-box.md).

---

## Verdict

The guest is a Debian image with two HTTP daemons (exec `:1337`, host `:1340`), a
noVNC desktop on `:6080`, and a per-box `BOX_TOKEN`. That is a different computer
from `DockerComputer` (headless debian, `docker exec`, ports 3000/5173/8000/8080,
no screen) and from box.ascii.dev (rented VMs, org API key). The seam was already
the `Computer` trait; this is a third plug.

EnsureBox in hexuria/box demonstrates create / ready / stop / destroy. It is not
a control plane we adopt — sharing modes, idle-stop, and `/console` already exist
here.

## Guest wire (connect-only)

SDKs take `(execUrl, hostUrl, token)` and **ignore** `/v1/info.endpoints` (those are
container-local listen addresses). OpenGrok does the same: published `127.0.0.1`
ports from `docker port`, token from `docker inspect` `Config.Env`.

Auth: `Authorization: Bearer <BOX_TOKEN>` except `GET /v1/health`. `GET /v1/ready`
requires Bearer. 6080 / noVNC is **not** Bearer-authenticated; RFB stays loopback
inside the container. VNC password is `BOX_VNC_PASSWORD` (x11vnc: first 8
characters), independent of `BOX_TOKEN`.

| Need | Method + path | Notes |
|---|---|---|
| Ready | `GET {host}/v1/ready` | 200 when exec is up and, if `BOX_DESKTOP_REQUIRED=1`, X is up. 503 `not_ready`, 401 bad bearer |
| Exec | `POST {exec}/v1/exec` | `{command, timeout_ms, stdin?}`. `command` may be a string (`/bin/sh -c`) or argv. Timeout default 30s, max 10 min. On timeout: `timed_out: true`, `exit_code: null` |
| Read file | `GET {exec}/v1/files?path=&encoding=utf8` | jailed to `/workspace`; empty path lists the root |
| Write file | `PUT {exec}/v1/files` | `{path, content, encoding, create_dirs}` |
| Desktop viewer | published `:6080/vnc.html` | do not use `/v1/desktop.viewer.url` as a public URL (container-local) |
| CUA | `POST {exec}/v1/cua/*` | framebuffer 1280×800, origin top-left. **Not on `Computer` yet** |

Compose tags `grok-box:local`, `shm_size: 256mb`, binds exec/host on `0.0.0.0` inside
the container, publishes 1337/1340/6080 on **127.0.0.1**. OpenGrok publishes the
same three on loopback with **random host ports** so per-bot can be more than one
guest. 5900 and 9222 stay inside.

## What OpenGrok stores, and what it must not

- Vault / org computer secret for kind `grok-box`: the **image name** (or
  `grok-box:local`). Not `BOX_TOKEN`.
- Per-guest token: container env only.
- `getForeverBoxStatus.vncUrl`: viewer URL + VNC password. Never `BOX_TOKEN`.
- Seam-B `EnsureSandBox.vnc_url`: still empty (known gap; the live screen is the
  gateway verb).

## Lifecycle we actually run

`create` = named volumes `{id}-workspace` (`/workspace`) and `{id}-chrome`
(`/home/box/chrome-profile`) + `docker run -d` with minted token and VNC password.
TTL is ignored (the guest has its own entrypoint; idle-stop is OpenGrok's).
`state` is Docker's word until the container is `running`, then `provisioning`
until ready (so default `wake` waits), `running` when ready, `error` on 401/403.
`stop` keeps container and volumes. `destroy` `rm -f`s the container and the
volumes. CUA on the trait is a follow-up (`docs/ROADMAP.md` Later).
