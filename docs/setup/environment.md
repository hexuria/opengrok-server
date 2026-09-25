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
| `OG_BIND` | `0.0.0.0:1337` compiled, **use `0.0.0.0:1447`** — or `127.0.0.1:1447` behind Caddy | where everything listens: AG-UI, auth, `/mcp`, the console. 1337 clashes with grok-bot's local-docker box; 1447 is the convention everywhere (the gate, the smokes, the live dev server). With TLS in front (`setup/tls.md`) the server binds loopback and Caddy takes the LAN address on the same port |
| `OG_PUBLIC_GATEWAY_URL` | `http://<OG_BIND>` | the address this host advertises for itself (e.g. `http://192.168.100.24:1447`): the base of every emailed link and of the browser-login redirect (`crates/opengrok/src/main.rs:70`), and the MCP door's OAuth issuer + `resource` (`<url>/mcp`). Behind TLS it must be the HTTPS address clients actually reach. `HostState` also keeps it for the webhook mint, which has no door today — see `host_state.rs` |
| `OG_COOKIE_SECURE` | unset | `1` marks the console's auth cookies `Secure` — set it behind HTTPS |

## The model door

| Variable | Default | What it is |
|---|---|---|
| `OG_MODEL_DOOR` | gateway | **which door every model call leaves by — see the table below.** `mock` scripts a stream (CI, no spend); `mock-tools` asks for one shell call per turn (drives the tool path and consent cards deterministically); anything else — including unset — is the direct `GatewayDoor`. `mock-cards` is **gone**: it refuses to boot |
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
| `mock-cards` | — | **removed 20 Sep 2026.** It served the mock transcript catalogue, which was deleted with the desktop client's door it rendered into. The binary REFUSES TO START on it rather than falling through to a real, billed door, and names `mock` / `mock-tools` instead |
| `mock` / `mock-tools` | no | CI, and the consent-card path with no spend |

**The one-line check that tells you which you are on:** ask the gateway what it logged. A real
turn should say `reason=Passthrough` with your coworker's own pin. `reason=Classified` means the
model never arrived and the gateway is guessing — which is a door problem, not a routing one.

## Jev, the classifier

Not a model door. `POST /jev/ask` puts named questions to TypeSafe AI's Jev and returns typed
answers with a calibrated probability for each option — see `crates/opengrok-server/src/jev`.

| Variable | Default | What it is |
|---|---|---|
| `OG_JEV_API_KEY` | unset (refused) | TypeSafe's API key. Unset is a valid deployment: the route answers 503 with a sentence naming this variable rather than guessing an answer. Deliberately NOT the SDK's own `TYPESAFE_API_KEY` — every knob this server reads is `OG_`-prefixed, and two names for one setting means a server configured by a variable that is not in its own documentation. It is a **provider** key, the one place this server holds one; it never reaches a coworker's row, a client payload or a log line |
| `OG_JEV_BASE_URL` | `https://api.typesafe.ai` | where the questions go. Always sent explicitly, so `TYPESAFE_BASE_URL` in the environment cannot retarget a deployment's classifier. **Point it at open-ai-gateway once the gateway hosts Jev** (open-ai-gateway#83): a Jev call made straight to TypeSafe never enters the gateway's ledger, and that is the only way its tokens can be accounted for the way model tokens are |
| `OG_JEV_MODEL` | `jev-latest` | the Jev model a question is asked of when the request names none |
| `OG_JEV_TIMEOUT_MS` | `10000` | what ONE attempt may take |
| `OG_JEV_BUDGET_MS` | `30000` | the total budget across retries. The retrying is the SDK's own — two retries after the first attempt, 0.5 s to 5 s of backoff with jitter, honouring a `retry-after`; these two knobs change its numbers rather than wrapping a second loop around it. Zero is refused in both, not read as "off" |

## Computers

| Variable | Default | What it is |
|---|---|---|
| `OG_COMPUTER` | auto | `docker` \| `ascii` \| `none`; unset picks box.ascii.dev when `OG_BOX_API_KEY` is set, local Docker otherwise. `none` means none: a new coworker is hired computerless (and so is every hire in an integration test, which brings no provider). The ASCII adapter is `opengrok_box::ascii::Client` (shapes from `docs/box/`); `AsciiBoxes` is the `Computer` trait on top of it |
| `OG_BOX_RUN_TAG` | unset | a second label on every Docker box this process creates (`dev.opengrok.run=<tag>`). `scripts/gate.sh` sets one per run and removes every box carrying it on exit — the smokes used to leave a container per hire behind |
| `OG_BOX_API_KEY` | unset | box.ascii.dev (`box_…`), for computers that outlive this machine. A running box's desktop URL (`getForeverBoxStatus.vncUrl`) comes from `POST /boxes/{id}/desktop?vnc=1` — do not log it |
| `OG_BOX_IDLE_STOP_SECONDS` | `0` (off) | stop an idle box after this many seconds |
| `OG_RECIPE_OBSERVE` | `input` | how much of the desktop a recipe run asks the box to report back (hexuria/box#29): `off` \| `input` \| `page`. `input` is the window under each pointer step and where the keys were about to go — about ten X round trips on a connection the box already holds, bounded by the box at 400 ms and well under the pacing a click already pays, so it does not measurably change playback. `page` adds two loopback DevTools reads either side of every navigating step, each capped at 250 ms, up to 512 of them on a 256-step recipe — seconds to minutes on a long tape, which is why it is not the default. What comes back is summarised into the tool result a model reads and into a workflow's `last.observe` / `last.targets` / `last.focus` / `last.urls` facts; it is **not** kept in the run history, because a recipe's runs are read by everyone it was shared to and a run happens on the box of whoever played it. Read once at boot; an unrecognised word logs a warning and falls back to `input` |
| `OG_DOCKER_IMAGE` | `debian:stable-slim` | the image a Docker computer is built from; any image with a shell |
| `OG_EGRESS_TUNNEL_ENABLED` | unset (off) | `1` = host wants the box egress tunnel (NativeChat "Review an action"). Same word as Grok host `SAND_EGRESS_TUNNEL_ENABLED === "1"` and the in-app `egressTunnelEnabled` toggle: any one is *intent*. The verb `isEgressTunnelAvailable` is intent **and** guest `/v1/info` `capabilities.egress_tunnel.ready`. On a Docker desktop, intent at **create** also publishes `127.0.0.1::8790` and sets `BOX_EGRESS_TUNNEL=1` plus a long `BOX_EGRESS_TUNNEL_BEARER` — Docker cannot add that publish later. Existing containers without 8790 must be **recreated**. See attach steps below |
| `SAND_EGRESS_TUNNEL_ENABLED` | unset (off) | Grok host spelling of the same flag; `1` is on, `"true"` is not |
| `OG_HOSTED` | unset | `1` = hosted/multi-tenant: local Docker is never advertised or used (untrusted bot containers must not run on the API host) |

A Docker desktop created with egress on listens for the laptop client on the published
host port of guest 8790. Discover it and attach (bearer must match create's container env;
OpenGrok does not dial this WS):

```sh
docker port <box> 8790
# 127.0.0.1:NNNN
docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' <box> \
  | sed -n 's/^BOX_EGRESS_TUNNEL_BEARER=//p' > /tmp/box-egress.bearer
box-egress-tunnel client --url ws://127.0.0.1:NNNN --bearer-file /tmp/box-egress.bearer
```

The image must ship the `box-egress-tunnel` binary (`grok-box:local` rebuild). `ready` stays
false until that client is attached. Never publish 8791/8792.

## Identity, email, console

| Variable | Default | What it is |
|---|---|---|
| `OG_LOGIN_EMAIL` | `host@opengrok.local` | the host account a browser login binds to on a single-user deployment |
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
| `OG_PLUGIN_CONNECT_TIMEOUT_MS` | default `5000`: what one plugin server gets for `initialize` + `tools/list` together. Servers are dialled concurrently before a turn's first model call, so this is the most a dead one can delay a turn; a server that misses it is left out of that turn, named in a WARN, and the coworker is told it is unavailable. A server that failed is not tried again for 30 s, and a listed one is reused for 60 s (per principal, coworker and credential) |
| `OG_PLUGIN_CALL_TIMEOUT_MS` | default `60000`: what one plugin `tools/call` gets. On the deadline the server is sent `notifications/cancelled` and the model gets a result saying the call may still have taken effect. Zero or junk in either is refused with a WARN and the default used — an unbounded wait is the bug these exist to prevent |

## Diagnostics

| Variable | What it is |
|---|---|
| `RUST_LOG` | tracing filter, e.g. `opengrok=debug,opengrok_server=debug,opengrok_harness=debug` |
| `OG_TRACE_REQUESTS` | **on by default**: one INFO line per request (method, path, status, ms, request id, Origin presence, bearer *length*, never its value). `0` turns it off. Every request carries an `X-Request-Id` — the client's if it sent one, a UUID otherwise — echoed on the response and stamped on every log line the handler writes |
| `OG_TURN_TIMING` | unset (compact always) | Compact AG-UI CUSTOM `run-timing` is **always** emitted on `RUN_FINISHED` / `RUN_ERROR` (model_ms, tools, tool_wait_ms, auto_review_ms, total_ms, tool_rounds) plus one INFO line. `1` / `true` also logs the JSON body. NativeChat can hang the compact frame in a debug drawer |

Retired, read by nothing: `SAND_GATEWAY_TOKEN`, and — since the seam A/B deletion of 20 Sep 2026 — `OG_GATEWAY_BEARER`, `OG_GATEWAY_EMAIL`, `OG_GATEWAY_IDENTITY_FALLBACK` and `OG_GRPC_BIND`.
