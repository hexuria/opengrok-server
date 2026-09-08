# Environment

Copy `.env.example` to `.env` and fill the required values. Every knob the server reads is
listed here — the list is grep-verified against `crates/` (`OG_*`/`SAND_*`/`RESEND_*` string
literals), and a variable that exists in code but not here is a documentation bug.

## Required

| Variable | What it is |
|---|---|
| `OG_DATABASE_URL` | Postgres — see [`postgres.md`](postgres.md) |
| `OG_TOKEN_SECRET` | signs our access tokens. No default, so two deployments can never share a key by accident. `openssl rand -hex 32`. **Changing it signs out every desktop at once**, and the recovery is a sign-in window that dies 180 s after it opens, with the person at the keyboard — so when editing `.env` for any other reason, fingerprint this value before and after (`grep -m1 '^OG_TOKEN_SECRET=' .env \| cut -d= -f2- \| shasum -a 256 \| cut -c1-12`) and change lines in place rather than rewriting the file |
| `OG_CREDENTIAL_KEK` | encrypts connector credentials at rest. Deliberately no default. `openssl rand -base64 32` |

## The listeners

| Variable | Default | What it is |
|---|---|---|
| `OG_BIND` | `0.0.0.0:1337` compiled, **use `0.0.0.0:1447`** — or `127.0.0.1:1447` behind Caddy | where everything listens: the Sand gateway, AG-UI, auth, the console. 1337 clashes with grok-bot's local-docker box; 1447 is the convention everywhere (the gate, the smokes, the live dev server). With TLS in front (`setup/tls.md`) the server binds loopback and Caddy takes the LAN address on the same port |
| `OG_GRPC_BIND` | unset (off) | opt-in tonic listener for seam-B gRPC. Unset means no gRPC socket — an unused open port is a liability |
| `OG_PUBLIC_GATEWAY_URL` | unset | the address `EnsureSandBox` mints to clients, and the MCP door's OAuth issuer + resource (`<url>/mcp`) (e.g. `http://192.168.100.24:1447`). Unset ⇒ the mint refuses. Must be non-loopback because the *client* refuses a loopback host — the mint itself does not check that (`seamb.rs`; `slice13-seamb-smoke.sh` is the loopback assertion) |
| `OG_GATEWAY_BEARER` | unset | the shared bearer the desktop client presents on every gateway call. The client-side counterpart is the token field beside its OpenGrok gateway URL setting |
| `OG_COOKIE_SECURE` | unset | `1` marks the console's auth cookies `Secure` — set it behind HTTPS |

## The model door

| Variable | Default | What it is |
|---|---|---|
| `OG_MODEL_DOOR` | gateway | **which door every model call leaves by — see the table below.** `mock` scripts a stream (CI, no spend); `mock-cards` serves the fixture catalogue; `mock-tools` asks for one shell call per turn (drives the tool path and consent cards deterministically); `rig` goes through rig-core; anything else — including unset — is the direct `GatewayDoor` |
| `OG_GATEWAY_URL` | `http://127.0.0.1:29080` | open-ai-gateway's inference listener |
| `OG_GATEWAY_TOKEN` | — | an `oag_live_…` key. **Never a provider key** — a pin is a route, not a credential (CLAUDE.md #4) |
| `OG_MODEL` | `gpt-5.6-luna` | the route a NEW coworker is hired on when none is named. Each coworker then keeps its own pin (changeable in the console at `/console/coworkers`), so changing this retargets nothing existing. Dialect: `provider/model` (`openai/gpt-5.5`), `@api`/`@sub`, or a ladder id (`oag/auto`); a bare name works on a passthrough route. **Servable and advertised are independent, in both directions.** An advertised id is not necessarily servable — `oag/auto` is refused on a route with no credential for the rung it picks; `POST /models/probe` proves a pin before it is saved. And the reverse: a **servable id need not be advertised** — `/v1/models` is built from each provider's own model listing, and xAI's returns quota with no model list, so `xai/grok-4.6` serves perfectly while never appearing in the picker. Do not "fix" a working default because the picker does not list it |
| `OG_AUTO_REVIEW_MODEL` | `OG_MODEL` | the auto-review judge's route — deliberately not the coworker's own route (the reviewer must not be the reviewed) |
| `OG_AUTO_REVIEW_MOCK_VERDICT` | unset | on the mock doors only: the judge's canned one-word verdict (`allow`/`ask`/`block`), for driving consent cards with no spend |
| `OG_GATEWAY_ADMIN_URL` | unset | open-ai-gateway's **admin** listener (`:29081`), for minting org members' keys from the console |
| `OG_GATEWAY_ADMIN_TOKEN` | unset | an **admin** key (`oag admin key create --email <you> --admin`) — NOT the inference key above. Unset ⇒ the console's "Gateway access" card is off, and `mint_late` cannot give a coworker a key of its own. **Crossing this with `OG_GATEWAY_TOKEN` fails in a way that reads like a broken deployment**: admin calls keep working while every inference call answers `401 authentication failed`, because an admin key is not valid on the inference route. `spend.rs:226` records the inversion from 2 Sep. Both values arrive in a mode-600 file and are never pasted into a terminal or a transcript; check them by prefix, never by value |

### Which door to use when

| `OG_MODEL_DOOR` | Real models? | Use it for |
|---|---|---|
| unset / `gateway` | yes | **the default, and the one to use.** Speaks the gateway's OpenAI-compatible route directly and sends `"model": request.model`, so a coworker's pin is honoured and the gateway logs `reason=Passthrough` |
| `rig` | yes | **avoid.** Goes through rig-core and does NOT transmit the model, so the gateway sees a modelless request, classifies it by policy, and answers on whatever rung the classifier picks. Every pin is silently ignored. It cost a night on 8 Sep 2026: turns "worked" while a coworker pinned to xAI was being answered by whatever tier the prompt happened to classify into |
| `mock-cards` | no | UI and card-rendering work. Serves the fixture catalogue (`help` lists it). Needs the `mock-fixtures` feature or it refuses to boot |
| `mock` / `mock-tools` | no | CI, and the consent-card path with no spend |

**The one-line check that tells you which you are on:** ask the gateway what it logged. A real
turn should say `reason=Passthrough` with your coworker's own pin. `reason=Classified` means the
model never arrived and the gateway is guessing — which is a door problem, not a routing one.

## Computers

| Variable | Default | What it is |
|---|---|---|
| `OG_COMPUTER` | auto | `docker` \| `ascii` \| `none`; unset picks box.ascii.dev when `OG_BOX_API_KEY` is set, local Docker otherwise. `none` means none: a new coworker is hired computerless (and so is every hire in an integration test, which brings no provider). The ASCII adapter is `opengrok_box::ascii::Client` (shapes from `docs/box/`); `AsciiBoxes` is the `Computer` trait on top of it |
| `OG_BOX_RUN_TAG` | unset | a second label on every Docker box this process creates (`dev.opengrok.run=<tag>`). `scripts/gate.sh` sets one per run and removes every box carrying it on exit — the smokes used to leave a container per hire behind |
| `OG_BOX_API_KEY` | unset | box.ascii.dev (`box_…`), for computers that outlive this machine. A running box's desktop URL (`getForeverBoxStatus.vncUrl`) comes from `POST /boxes/{id}/desktop?vnc=1` — do not log it |
| `OG_BOX_IDLE_STOP_SECONDS` | `0` (off) | stop an idle box after this many seconds |
| `OG_DOCKER_IMAGE` | `debian:stable-slim` | the image a Docker computer is built from; any image with a shell |
| `OG_HOSTED` | unset | `1` = hosted/multi-tenant: local Docker is never advertised or used (untrusted bot containers must not run on the API host) |

## Identity, email, console

| Variable | Default | What it is |
|---|---|---|
| `OG_LOGIN_EMAIL` | `OG_GATEWAY_EMAIL`, then `host@opengrok.local` | the host account the desktop roster and sign-in bind to on a single-user deployment |
| `OG_RESEND_API_KEY` | unset (auto-verify) | Resend key; set ⇒ signup sends a verification email and requires it (`RESEND_API` is accepted as a legacy alias) |
| `RESEND_FROM_EMAIL` / `RESEND_FROM_NAME` | — | the sender identity; the domain must be verified in the Resend account |

No variable configures DNS: domain-ownership proof (`/admin/domains/{d}/verify`) resolves the
`_opengrok-verify.<domain>` TXT record through hickory-resolver using the box's own resolver
configuration (`/etc/resolv.conf`). A box with none logs it at boot and that one endpoint answers
503 until it is fixed; nothing else is affected. Password reset needs the Resend key above —
without it `/forgot-password` says so and the operator resets with
`opengrok admin account password --email <email>`. Run the smoke gate with the Resend key UNSET:
`slice17-identity-smoke.sh` asserts the no-mailer path, and a set key sends real mail to the
throwaway signup addresses.
| `OG_WEB_CONSOLE_DIR` | unset (no console route) | directory holding the built SPA's `index.html`, normally `web/dist`; served at `/console` |

## Connectors and plugins

| Variable | What it is |
|---|---|
| `OG_CONNECTORS` | JSON list of OAuth provider configurations (holds client secrets — file permissions are the guard) |
| `OG_OAUTH_REDIRECT_URI` | where a provider sends the browser back; must match the app registration byte for byte |
| `OG_PLUGINS_DIR` | Agent Plugins installed on this server, one directory each |

## Diagnostics

| Variable | What it is |
|---|---|
| `RUST_LOG` | tracing filter, e.g. `opengrok=debug,opengrok_server=debug,opengrok_harness=debug` |
| `OG_TRACE_REQUESTS` | **on by default**: one INFO line per request (method, path, status, ms, request id, Origin presence, bearer *length*, never its value), plus `/events` stream open/close with the subscriber count. `0` turns it off. Every request carries an `X-Request-Id` — the client's if it sent one, a UUID otherwise — echoed on the response and stamped on every log line the handler writes |

Retired: `SAND_GATEWAY_TOKEN` is read by nothing — the client bearer is `OG_GATEWAY_BEARER`.
