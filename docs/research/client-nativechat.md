# NativeChat's routes on this server

The route map for the client served today. NativeChat is a native desktop app in Rust + GPUI
(`hexuria/nativechat`, per #147). It replaced the Grok Bot desktop client, whose two doors were
deleted on 20 Sep 2026 ([`client-grok-bot.md`](client-grok-bot.md) is that client's record).
Setting it up: [`../setup/nativechat.md`](../setup/nativechat.md).

## Status: the server half is complete; the client half is not yet read

**Every route this server mounts is below**, grouped the way a person meets them. The
`Mounted at` column is where the router registers the route. It is the server's own provenance,
and `crates/opengrok/tests/setup_docs.rs` fails when a mounted route has no row here. The
`Reply` column is what the server sends. Where it says "see the mount", the handler has not been
transcribed into this table yet. Read the handler rather than guessing.

**The client half is where non-negotiable #1 applies, and it is not met yet.** "The client
contract is transcribed, never invented": a route's place in the contract is proven by the
NativeChat file that calls it. NativeChat's source is not in this checkout. So the last column
only records **callers on record**, meaning what a document or issue in *this* repo already
said, with that source named:

- `*unverified*` means nobody has read whether NativeChat calls this route. It does **not** mean
  "NativeChat does not call it". A blank would read as the second, so the test refuses a blank.
- A citation of #147 names NativeChat files as they stood on 19 Sep 2026 (`main` of
  `hexuria/nativechat`), quoted by that issue and not re-read here.
- "not NativeChat: the console" names the web console's own file in `web/src/api/`, which *is*
  read. It still does not prove NativeChat never calls the route.

**To finish it:** with `hexuria/nativechat` checked out, grep its HTTP client (#147 names
`src/opengrok/client.rs` and `src/opengrok/local_exec.rs`) for every path. Replace each
`*unverified*` with `path/to/file.rs:line`, or with "not called (grep: none)". Add the reply
fields NativeChat actually reads. Fields it reads that this table does not list are the ones that
break silently when renamed (CLAUDE.md, "An empty success is the dangerous reply").

## Roster

Who the coworkers are, their routes, their keys and their limits.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/ag-ui/host-settings` | GET PUT | `agui/routes.rs:895` | the host settings record (+ `egressTunnelAvailable` with `?coworker=`); PUT merges and answers the whole record | calls it — `ROADMAP.md` Phase 0 re-homed the three seam-A verbs NativeChat still called onto this route. NativeChat file: *unverified* |
| `/coworkers` | POST GET | `agui/routes.rs:899` | GET: JSON array of roster rows, newest first; POST: the hired row (`id`, `model`, `name`, `boxId`, …) | *unverified* |
| `/coworkers/{coworker_id}` | PATCH DELETE | `agui/routes.rs:905` | PATCH: the updated row; DELETE: retires it (409 if already retired); 404 if not yours | *unverified* |
| `/coworkers/{coworker_id}/keys` | POST GET | `agui/routes.rs:920` | POST: `{key, jti, …}` — the key shown once; GET: JSON array, never the key | *unverified* |
| `/coworkers/{coworker_id}/keys/{jti}` | DELETE | `agui/routes.rs:924` | 204; 404 "no such key" | *unverified* |
| `/coworkers/{coworker_id}/limit` | GET PUT | `agui/routes.rs:916` | JSON `{cap, dayCap, …}`; PUT refuses a cap above the pool with the numbers | not NativeChat: the console, `web/src/api/coworkers.ts`. NativeChat: *unverified* |
| `/coworkers/{coworker_id}/mcp-calls` | GET | `agui/routes.rs:928` | JSON: the MCP door's audit for this coworker | not NativeChat: the console, `web/src/api/coworkers.ts`. NativeChat: *unverified* |
| `/coworkers/{coworker_id}/spend` | GET | `agui/routes.rs:912` | JSON; see the mount | not NativeChat: the console, `web/src/api/coworkers.ts`. NativeChat: *unverified* |
| `/coworkers/{coworker_id}/usage` | GET | `agui/routes.rs:915` | JSON (`?window=`); see the mount | *unverified* |
| `/models` | GET | `agui/routes.rs:900` | `{models: [{id, points?}], note}` — `note` says why the list is empty, when it is | *unverified* |
| `/models/probe` | POST | `agui/routes.rs:904` | `{ok: true, served, toolCalls}` or `{ok: false, detail}`; 429 when probed again too soon | not NativeChat: the console, `web/src/api/coworkers.ts`. NativeChat: *unverified* |
| `/templates` | GET | `agui/routes.rs:903` | JSON: the templates the caller's org offers | not NativeChat: the console, `web/src/api/coworkers.ts`. NativeChat: *unverified* |

## Conversation

A turn, its replay, its queue, stopping and hiding it.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/ag-ui` | POST | `agui/routes.rs:883` | `text/event-stream` of AG-UI events (`RUN_STARTED` … `TEXT_MESSAGE_*`, `TOOL_CALL_*`, `CUSTOM`, `RUN_FINISHED`/`RUN_ERROR`) | calls it — `scripts/slice8-approval-smoke.sh` records that NativeChat keys its Waiting chrome off `RUN_FINISHED`; `environment.md` (`OG_TURN_TIMING`) that it can show `run-timing`. NativeChat file: *unverified* |
| `/ag-ui/runs/{run_id}` | GET | `agui/routes.rs:890` | the run replayed from the log: JSON with `status` (`finished`, `awaiting-approval`, …), `pending` when suspended, and its events; 404 if not yours | *unverified* |
| `/ag-ui/runs/{run_id}/hide` | POST | `agui/routes.rs:892` | 204; 404 "no such run" | *unverified* |
| `/ag-ui/runs/{run_id}/stop` | POST | `agui/routes.rs:891` | 404 "no such run" if not yours; otherwise see the mount | *unverified* |
| `/ag-ui/threads/{thread_id}` | GET | `agui/routes.rs:893` | the thread replayed from the log (`?limit=&events=`); 404 "no such thread" | `?events=true` on a cold load — `docs/archive/findings-og139.md`. NativeChat file: *unverified* |
| `/ag-ui/threads/{thread_id}/pending` | GET POST | `agui/pending.rs:49` | see the mount | *unverified* |
| `/ag-ui/threads/{thread_id}/pending/{id}` | PATCH DELETE | `agui/pending.rs:50` | DELETE: the entry marked `canceled`; otherwise see the mount | *unverified* |

## Approvals and cards

A suspended run, the person's answer, forms and the policy behind them.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/ag-ui/approvals` | GET | `agui/routes.rs:894` | JSON array: `{runId, callId, tool, arguments, …}` per waiting call | *unverified* |
| `/ag-ui/box-handoff/resolve` | POST | `agui/user_form.rs:58` | JSON; see the mount | *unverified* |
| `/ag-ui/runs/{run_id}/answer` | POST | `agui/routes.rs:884` | `{alreadyAnswered, …}` — exactly once; a repeat reports the settled state | *unverified* |
| `/ag-ui/user-form/dismiss` | POST | `agui/user_form.rs:57` | JSON; see the mount | *unverified* |
| `/ag-ui/user-form/submit` | POST | `agui/user_form.rs:56` | JSON; see the mount | calls it with `savedLogin`/`savedLoginId` — `docs/site-logins.md`. NativeChat file: *unverified* |
| `/auto-review/effective` | GET | `auto_review.rs:152` | JSON: the policy in force | *unverified* |
| `/auto-review/policy` | GET PUT DELETE | `auto_review.rs:148` | PUT/DELETE: 204; GET: see the mount | *unverified* |
| `/coworkers/{coworker_id}/approvals` | POST | `agui/routes.rs:909` | the grant view: `{needsApproval: […], …}` | *unverified* |
| `/site-logins` | GET POST | `agui/site_logins.rs:33` | GET: JSON array of rows (id, origin, username, label, kind, notes, timestamps, passkey ids; the secrets come back only from `/reveal`); POST: 400 `{error}` with the reason | lists the person's rows under a form field — `docs/site-logins.md`. NativeChat file: *unverified* |
| `/site-logins/icon/{origin}` | GET | `agui/site_logins.rs:36` | the site's icon; 400 `{error}` for a non-site | *unverified* |
| `/site-logins/{id}` | DELETE PATCH | `agui/site_logins.rs:34` | see the mount | *unverified* |
| `/site-logins/{id}/reveal` | POST | `agui/site_logins.rs:35` | `{id, password, otpauth}`; 404 `{error}` | *unverified* |

## Attachments

What a run produces and what a person attaches.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/artifacts` | POST | `artifacts.rs:26` | the stored artifact's record (no bytes); 25 MiB cap | *unverified* |
| `/artifacts/{id}` | GET DELETE | `artifacts.rs:36` | GET: the record; DELETE: removes it; the same 404 for missing, deleted or not yours | *unverified* |
| `/artifacts/{id}/bytes` | GET | `artifacts.rs:37` | the bytes; the same 404 rule | *unverified* |

## MCP and skills

Skills, recipes, connectors and the tools a coworker is offered.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/connections` | GET | `connections/routes.rs:53` | JSON: what a person has connected and to whom it is lent | *unverified* |
| `/connections/callback` | GET | `connections/routes.rs:55` | the provider's return leg (the signed state is the only trusted input) | *unverified* |
| `/connections/{connector}/authorize` | GET | `connections/routes.rs:54` | a redirect to the provider | *unverified* |
| `/connections/{id}` | DELETE | `connections/routes.rs:58` | see the mount | *unverified* |
| `/connections/{id}/lend` | POST | `connections/routes.rs:56` | see the mount | *unverified* |
| `/connections/{id}/revoke` | POST | `connections/routes.rs:57` | see the mount | *unverified* |
| `/coworkers/{coworker_id}/tools` | GET | `agui/routes.rs:934` | see the mount | *unverified* |
| `/recipes` | GET POST | `recipes.rs:71` | see the mount | *unverified* |
| `/recipes/{id}` | GET PUT DELETE | `recipes.rs:77` | PUT: 400 "a recipe needs a name" for a blank one; otherwise see the mount | *unverified* |
| `/recipes/{id}/accept` | POST | `recipes.rs:82` | see the mount | *unverified* |
| `/recipes/{id}/decline` | POST | `recipes.rs:83` | see the mount | *unverified* |
| `/recipes/{id}/grants` | POST | `recipes.rs:84` | 403 for somebody else's bot; otherwise see the mount | *unverified* |
| `/recipes/{id}/grants/{coworker_id}` | DELETE | `recipes.rs:85` | 403 for somebody else's bot; otherwise see the mount | *unverified* |
| `/recipes/{id}/run` | POST | `recipes.rs:86` | see the mount | *unverified* |
| `/recipes/{id}/share` | POST | `recipes.rs:80` | see the mount | *unverified* |
| `/recipes/{id}/share/{scope}/{scope_id}` | DELETE | `recipes.rs:81` | see the mount | *unverified* |
| `/recipes/{id}/versions` | POST | `recipes.rs:78` | see the mount | *unverified* |
| `/recipes/{id}/versions/{version}` | DELETE | `recipes.rs:79` | 404 "no such version"; otherwise see the mount | *unverified* |
| `/skills` | GET POST | `skills.rs:91` | see the mount | *unverified* |
| `/skills/from-tape` | POST | `skills.rs:110` | see the mount | *unverified* |
| `/skills/{id}` | GET PUT DELETE | `skills.rs:119` | see the mount | *unverified* |
| `/skills/{id}/versions` | POST | `skills.rs:120` | see the mount | *unverified* |

## Automations

Work that starts with no client open.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/hooks/{hook_id}` | POST | `hooks.rs:49` | `{accepted: true, runId}`; `{error}` for a bad token | a third party's webhook, with the hook's own key — not a client |
| `/monitors` | POST GET | `autonomy/routes.rs:59` | POST: JSON; GET: JSON array | *unverified* |
| `/monitors/{id}` | DELETE | `autonomy/routes.rs:62` | see the mount | *unverified* |
| `/monitors/{id}/pause` | POST | `autonomy/routes.rs:60` | see the mount | *unverified* |
| `/monitors/{id}/resume` | POST | `autonomy/routes.rs:61` | see the mount | *unverified* |
| `/schedules` | POST GET | `autonomy/routes.rs:49` | POST: `{id, nextDueMs, …}` (422 with the reason for a bad cron); GET: JSON array, `no-store` | *unverified* |
| `/schedules/{id}` | DELETE | `autonomy/routes.rs:53` | see the mount | *unverified* |
| `/schedules/{id}/pause` | POST | `autonomy/routes.rs:50` | see the mount | *unverified* |
| `/schedules/{id}/resume` | POST | `autonomy/routes.rs:51` | see the mount | *unverified* |
| `/schedules/{id}/rotate-key` | POST | `autonomy/routes.rs:52` | see the mount | *unverified* |
| `/workflows` | POST | `workflows.rs:60` | see the mount | *unverified* |
| `/workflows/{id}/run` | POST | `workflows.rs:62` | see the mount | *unverified* |
| `/workflows/{id}/versions` | POST | `workflows.rs:61` | see the mount | *unverified* |

## Computer

The coworker's computer, and the person's own machine (reverse-exec).

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/coworkers/{coworker_id}/computer` | GET POST | `agui/routes.rs:929` | the computer status record (live state, noVNC URL) | the handler's own doc names "NativeChat's Open button". NativeChat file: *unverified* |
| `/coworkers/{coworker_id}/computer/egress-policy` | GET PUT | `agui/routes.rs:939` | GET: JSON policy; PUT: 204 (422 "unknown mode") | *unverified* |
| `/coworkers/{coworker_id}/computer/reset` | POST | `agui/routes.rs:943` | the status record after the reset | *unverified* |
| `/coworkers/{coworker_id}/computer/update` | POST | `agui/routes.rs:935` | 202 + the status record | *unverified* |
| `/coworkers/{coworker_id}/computer/vnc/{ticket}/{*rest}` | GET | `agui/routes.rs:942` | the box's noVNC page, its files and its websocket, proxied for one ticket; every answer carries `Content-Security-Policy: sandbox allow-scripts allow-pointer-lock`, and a redirect from the box is refused with a 502 | the `vncUrl` of the computer status record points here (`docs/setup/environment.md`, `OG_DOCKER_IMAGE`). NativeChat file: *unverified* |
| `/coworkers/{coworker_id}/screen` | GET | `agui/routes.rs:933` | JSON carrying a screenshot | *unverified* |
| `/local-exec/audit` | GET | `local_exec.rs:244` | `{entries: […]}` | *unverified* |
| `/local-exec/daemon` | POST GET | `local_exec.rs:239` | POST: `{machineId, token}` (one token per machine); GET: `{machines: […]}` | `ensure_daemon` enrols the Mac — `src/opengrok/local_exec.rs` (#147; the route is inferred) |
| `/local-exec/daemon/{machine_id}` | DELETE | `local_exec.rs:240` | 204 | *unverified* |
| `/local-exec/policy` | GET PUT | `local_exec.rs:234` | GET: JSON mode and rules; PUT: 204 (422 "unknown mode") | `ensure_daemon` writes `ask` on first enrol; the card's Always/Never write `bypass`/`never` — `src/opengrok/local_exec.rs`, `src/state.rs:6001–6004` (#147, 19 Sep 2026; not re-read) |
| `/local-exec/policy/rule` | POST DELETE | `local_exec.rs:235` | 204; 422 with the reason (a `sudo` standing allow is refused) | `add_local_exec_rule`, `src/opengrok/client.rs:1313`, **never called** (#147, 19 Sep 2026; not re-read) |
| `/local-exec/requests` | GET | `local_exec.rs:251` | the machine's request stream (`exec` and `cancel` frames); 401 "enrol this machine first" | `run_request_stream` handles `exec` frames only and drops `cancel` — `src/opengrok/local_exec.rs:143` (#147; that it reads this route is inferred) |
| `/local-exec/responses` | POST | `local_exec.rs:252` | 204; 401 "enrol this machine first" | posts results; posts no `hello` (#147 stage 5). NativeChat file: *unverified* |
| `/local-exec/run` | POST | `local_exec.rs:248` | JSON outcome; 403 with the refusal | **never called** — no client (#147) |

## Sign-in

How a client gets and keeps an account token.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/auth/cursor_dev_session_token` | GET | `auth/routes.rs:176` | see the mount | the smokes; answers only on loopback (`is_loopback`). Not for a client on another machine |
| `/auth/poll` | GET | `auth/routes.rs:184` | see the mount | the PKCE sign-in built for desktop clients (`slice16-browser-login-smoke.sh`). Whether NativeChat uses it: *unverified* |
| `/auth/signup` | POST | `auth/routes.rs:190` | see the mount | JSON signup with an invite code. Whether NativeChat calls it: *unverified* |
| `/auth/verify` | GET | `auth/routes.rs:191` | see the mount | the emailed verification link, opened in a browser |
| `/loginDeepControl` | GET POST | `auth/routes.rs:180` | see the mount | the browser half of the same PKCE sign-in. Whether NativeChat uses it: *unverified* |
| `/oauth/token` | POST | `auth/routes.rs:177` | see the mount | token rotation for a bearer client. Whether NativeChat calls it: *unverified* |

## Not a NativeChat door

The web console, browser pages, the MCP door and its OAuth, probes.

| Route | Methods | Mounted at (`crates/opengrok-server/src/`) | Reply (the server's side) | Callers on record |
|---|---|---|---|---|
| `/.well-known/oauth-authorization-server` | GET | `auth/oauth_mcp.rs:135` | OAuth metadata / registration / consent page / tokens, per the MCP authorization spec | MCP clients during OAuth (`against_mcp_oauth.rs`) — not NativeChat |
| `/.well-known/oauth-protected-resource` | GET | `auth/oauth_mcp.rs:127` | OAuth metadata / registration / consent page / tokens, per the MCP authorization spec | MCP clients during OAuth (`against_mcp_oauth.rs`) — not NativeChat |
| `/.well-known/oauth-protected-resource/mcp` | GET | `auth/oauth_mcp.rs:131` | OAuth metadata / registration / consent page / tokens, per the MCP authorization spec | MCP clients during OAuth (`against_mcp_oauth.rs`) — not NativeChat |
| `/account` | GET | `account_api.rs:32` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/account/password` | POST | `account_api.rs:34` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/account/profile` | POST | `account_api.rs:33` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/admin/computers` | GET | `computers.rs:33` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/computers/docker` | GET | `computers.rs:34` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/computers/docker/update-all` | POST | `computers.rs:35` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/computers/mode` | GET PUT | `computers.rs:41` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/computers/mode/account/{id}` | PUT DELETE | `computers.rs:42` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/computers/{kind}` | POST DELETE | `computers.rs:39` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/computers/{kind}/test` | POST | `computers.rs:40` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/domains` | GET POST | `account_api.rs:41` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/domains/{domain}` | DELETE | `account_api.rs:42` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/domains/{domain}/verify` | POST | `account_api.rs:43` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/gateway/budget` | PUT | `account_api.rs:51` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/gateway/keys` | GET POST | `account_api.rs:45` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/gateway/keys/{id}` | DELETE | `account_api.rs:49` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/gateway/keys/{id}/quota` | PUT | `account_api.rs:50` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/gateway/usage` | GET | `account_api.rs:52` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/invites` | GET POST | `account_api.rs:38` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/points` | GET | `account_api.rs:56` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/points/members/{account_id}` | PUT | `account_api.rs:58` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/points/reference` | PUT | `account_api.rs:57` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/recipes` | GET | `recipes.rs:87` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/templates` | GET POST | `account_api.rs:60` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/templates/{id}` | PUT DELETE | `account_api.rs:64` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/users` | GET | `account_api.rs:35` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/users/{id}/disable` | POST | `account_api.rs:37` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/users/{id}/enable` | POST | `account_api.rs:36` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/admin/users/{id}/verify` | POST | `account_api.rs:39` | JSON for the console; see the mount | not NativeChat: the console, `web/src/api/admin.ts` |
| `/auth/login` | POST | `auth/routes.rs:187` | see the mount | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/auth/logout` | POST | `auth/routes.rs:188` | see the mount | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/auth/password/forgot` | POST | `auth/routes.rs:202` | see the mount | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/auth/refresh` | POST | `auth/routes.rs:189` | see the mount | not NativeChat: the console, `web/src/api/client.ts`. NativeChat: *unverified* |
| `/auth/verify/resend` | POST | `auth/routes.rs:242` | `202 {accepted, mailer}`, the same for an unknown, a verified and a waiting address | not NativeChat: the console, `web/src/api/account.ts`. NativeChat: *unverified* |
| `/console` | GET | `lib.rs:228` | the built SPA (`OG_WEB_CONSOLE_DIR`) | a browser: the SPA |
| `/console/` | GET | `lib.rs:229` | the built SPA (`OG_WEB_CONSOLE_DIR`) | a browser: the SPA |
| `/console/{*rest}` | GET | `lib.rs:230` | the built SPA (`OG_WEB_CONSOLE_DIR`) | a browser: the SPA |
| `/forgot-password` | GET POST | `auth/routes.rs:198` | see the mount | a browser page |
| `/health` | GET | `health.rs:21` | `{ok: true, pid, isBusy, activeAgentId, startedAt, lastBusyAtMs}` — `ok === true` is all a probe reads | every probe and script; `ok === true` is all any reads. NativeChat: *unverified* |
| `/jev/ask` | POST | `jev/routes.rs:54` | typed answers with a probability per option; 503 naming `OG_JEV_API_KEY` when unset | no client on record |
| `/mcp` | (nested MCP service) | `lib.rs:80` | the MCP door (streamable HTTP), for MCP clients such as Claude Code | MCP clients (Claude Code), with a bot key or the door's OAuth — not NativeChat |
| `/oauth/mcp/authorize` | GET POST | `auth/oauth_mcp.rs:140` | OAuth metadata / registration / consent page / tokens, per the MCP authorization spec | MCP clients during OAuth (`against_mcp_oauth.rs`) — not NativeChat |
| `/oauth/mcp/register` | POST | `auth/oauth_mcp.rs:139` | OAuth metadata / registration / consent page / tokens, per the MCP authorization spec | MCP clients during OAuth (`against_mcp_oauth.rs`) — not NativeChat |
| `/oauth/mcp/token` | POST | `auth/oauth_mcp.rs:144` | OAuth metadata / registration / consent page / tokens, per the MCP authorization spec | MCP clients during OAuth (`against_mcp_oauth.rs`) — not NativeChat |
| `/ready` | GET | `health.rs:22` | `{ok, store, gateway, gatewayStatus}`, 503 when not ok; `gateway` is `ok`, `refused`, `unreachable` or `unused` | readiness probes (`docs/setup/environment.md`, `OG_GATEWAY_TOKEN`). NativeChat: *unverified* |
| `/resend-verification` | GET POST | `auth/routes.rs:244` | see the mount | a browser page: the "Resend verification email" link on the sign-in card |
| `/reset-password` | GET POST | `auth/routes.rs:206` | see the mount | a browser page: the emailed link |
| `/signup` | GET POST | `auth/routes.rs:192` | see the mount | a browser: the invite link's page |
