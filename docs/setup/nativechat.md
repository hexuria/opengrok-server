# Connecting NativeChat — and the demo runbook

NativeChat is the client served today: a native desktop app in Rust + GPUI, in its own repository
(`hexuria/nativechat`, per #147). It replaced the Grok Bot desktop client, whose doors were deleted
on 20 Sep 2026 ([`../archive/desktop-client.md`](../archive/desktop-client.md) is that client's
record).

> **What this page can and cannot vouch for.** NativeChat's source is not in this checkout.
> Every statement about **the server** below names the file or smoke that proves it. Statements
> about **NativeChat itself** — its build, its screens, which sign-in it uses, whether it insists
> on HTTPS — are either cited from a document in this repo that recorded them, or marked
> *unverified*. Non-negotiable #1 and #10 forbid filling those in from memory. When
> `hexuria/nativechat` is read, replace each *unverified* with its file path. The routes it calls
> are mapped, with the same rule, in
> [`../research/client-nativechat.md`](../research/client-nativechat.md).

## Before you start

Steps 1–4 of this chain are done ([`first-run.md`](first-run.md)). An admin can sign in, the
gateway key is in `.env`, and a hired coworker's **Test** says it calls tools. A demo on a
coworker whose route only talks shows no card and no computer. That is exactly what the
`gpt-5.6-luna` default did.

## 1. Build or obtain NativeChat

*Unverified:* the build steps live in NativeChat's own repository and are not transcribed here.
Take them from its README, not from this page.

## 2. Point it at the server: host, TLS, token

**Host.** Everything, NativeChat's routes included, is served on one port (`OG_BIND`, `1447` by
convention; [`running.md`](running.md), "What is listening where"). A client on another machine
needs the server bound to the LAN (`OG_BIND=0.0.0.0:1447`), or Caddy in front with the server on
loopback ([`tls.md`](tls.md)). `OG_PUBLIC_GATEWAY_URL` must be the address clients actually reach.
Emailed links and the MCP door's OAuth issuer are built from it ([`environment.md`](environment.md)).

**TLS.** [`tls.md`](tls.md) puts Caddy and a locally trusted certificate in front. The server
never terminates TLS itself. Claude Code's MCP OAuth needs HTTPS. *Unverified:* whether NativeChat
requires `https://` or accepts the plain LAN address.

**Loopback rules.** Two different rules, and only one is known:

- **Server, verified:** the dev sign-in (`GET /auth/cursor_dev_session_token`) answers only when
  the `Host` header names a loopback address (`is_loopback`,
  `crates/opengrok-server/src/auth/routes.rs`). A client on another machine cannot use it, and
  nothing here should: it is for the smokes.
- **Client, unverified:** the removed Electron client **refused** a gateway host starting with
  `127.0.0.1` or `localhost`. That was the Electron client's rule. Whether NativeChat has one is
  not known from this repo, so do not carry it over. Try the address you mean to demo on.

**Token.** Every NativeChat call carries `Authorization: Bearer <access token>` for the signed-in
account. `POST /ag-ui` reads **only** that header. It does not read the console's cookie
(`principal_from_bearer`, `crates/opengrok-server/src/agui/routes.rs`). An access token is
refreshed through `POST /oauth/token`. The server offers two sign-ins that yield one:

- **PKCE browser sign-in, for desktop clients.** The app opens `/loginDeepControl` in the
  person's browser. The person signs in there with their email and password, and the app polls
  `GET /auth/poll` until it releases `{accessToken, refreshToken}`
  (`slice16-browser-login-smoke.sh`).
- **The console's cookie login** (`POST /auth/login`), for the browser only.

*Unverified:* which of these NativeChat uses. `ROADMAP.md` Phase 0 records that NativeChat
calls `GET/PUT /ag-ui/host-settings` "under the account token", so it holds one.

A **bot key** (`POST /coworkers/{id}/keys`, shown once) is the alternative for a client that
can hold only one static header. A bare `POST /ag-ui` with it runs **as that coworker**
(`slice14-botkey-smoke.sh`).

## 3. The demo runbook

Four things, in the order a person meets them. Each one has the server-side proof beside it, a
`curl` with the same `$OG` and `$TOKEN` as [`first-run.md`](first-run.md) step 5. Run the `curl`
first when a step misbehaves in the app: it tells you which side is wrong.

### A first chat

In NativeChat, open the coworker and say something (*unverified*: the screen names). On the
wire, that is `POST /ag-ui` with `forwardedProps.coworkerId`, answered as an AG-UI event stream:

```sh
curl -sN -X POST "$OG/ag-ui" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d "{\"threadId\":\"demo\",\"runId\":\"demo-$(date +%s)\",\"messages\":[{\"id\":\"m1\",\"role\":\"user\",\"content\":\"hello\"}],\"forwardedProps\":{\"coworkerId\":\"$CW\"}}" \
  | sed -n 's/^data: //p' | jq -r '.type' | uniq -c
```

A reconnecting client replays with `GET /ag-ui/threads/{thread_id}` (`?limit=&events=`) or
`GET /ag-ui/runs/{run_id}`. Nothing is lost when the app closes mid-answer: the run lives here.

### A consent card

Make `shell` need a person's yes on this coworker, then ask it to run a command:

```sh
curl -fsS -X POST "$OG/coworkers/$CW/approvals" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"tools":["shell"]}'      # → {"needsApproval":["shell"],…}
```

The run **suspends rather than fails**. The stream carries `CUSTOM` `run-awaiting-approval` and
then closes with `RUN_FINISHED`. The smoke's comment records that NativeChat keys its Waiting
chrome off that event (`scripts/slice8-approval-smoke.sh`). The waiting call is listed with its
arguments, so the person sees what they are approving, and it is answered exactly once:

```sh
curl -fsS "$OG/ag-ui/approvals" -H "authorization: Bearer $TOKEN" | jq '.[] | {runId, callId, tool, arguments}'
curl -fsS -X POST "$OG/ag-ui/runs/<runId>/answer" -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"call_id":"<callId>","approved":true}'
```

For a demo with **no model spend and a card every time**, run the server with
`OG_MODEL_DOOR=mock-tools OG_AUTO_REVIEW_MOCK_VERDICT=ask scripts/serve.sh`
([`running.md`](running.md), "Deterministic windows").

### The coworker's computer

A coworker is hired with a computer: local Docker by default, box.ascii.dev when
`OG_BOX_API_KEY` is set, none with `OG_COMPUTER=none` ([`environment.md`](environment.md),
"Computers"). The server's surface for it (`crates/opengrok-server/src/agui/routes.rs`):

| Route | What it does |
|---|---|
| `GET /coworkers/{id}/computer` | live box state and the noVNC URL; the handler's doc names it as what "NativeChat's Open button" reads |
| `POST /coworkers/{id}/computer` | makes sure the box is running, then answers the same status |
| `GET /coworkers/{id}/screen` | a screenshot |
| `POST /coworkers/{id}/computer/update` | rebuild on the newest image, keeping its files (202; the phases arrive on `GET …/computer`) |
| `POST /coworkers/{id}/computer/reset` | destroy it, data and all, and start fresh |

The card from the step above ran its command there.

### A routine

A routine is a schedule (`POST /schedules`) that wakes the coworker with a prompt, with no
client open:

```sh
curl -fsS -X POST "$OG/schedules" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d "{\"coworkerId\":\"$CW\",\"name\":\"Morning check\",\"cron\":\"0 9 * * 1-5\",\"prompt\":\"Check the disk and report.\"}"
# → {"id":…,"nextDueMs":…}
```

Five fields are read as minute-first and promoted to the parser's seconds-first form
(`normalized_cron`, `crates/opengrok-core/src/schedule.rs`), so `0 9 * * 1-5` means 09:00 on
weekdays. `nextDueMs` in the reply says when that is on the server's clock. Its runs land in a
thread whose id is the schedule's own. `GET /schedules` lists it, and `POST
/schedules/{id}/pause` or `/resume` and `DELETE /schedules/{id}` manage it
(`scripts/slice10-autonomy-smoke.sh`). *Unverified:* where NativeChat shows a routine's result.

Edit it in place, run it by hand, and read what it did:

```sh
curl -fsS -X PATCH "$OG/schedules/$ID" -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"prompt":"Check the disk and the backups."}'   # any of name, prompt, cron, coworkerId
curl -fsS -X POST "$OG/schedules/$ID/run" -H "authorization: Bearer $TOKEN"    # → 202 {"accepted":true,"runId":…}
curl -fsS "$OG/schedules/$ID/runs?limit=20" -H "authorization: Bearer $TOKEN"  # → [{"runId","cause","status",…}]
```

An edit keeps the routine's id, thread and history, and a field left out keeps what it had. A
webhook routine keeps its URL and key through an edit, and refuses a `cron`. Run now works on a
paused routine and leaves it paused: the clock and the hook stay off. The history lists only runs
the routine started (`cause` is `clock`, `manual` or `webhook`), newest first, with `status`
`running`, `waiting` (on a card), `ok` or `error`.

A monitor has the same three: `PATCH /monitors/{id}` (any of `watches`, `prompt`, `coworkerId`),
`POST /monitors/{id}/run` and `GET /monitors/{id}/runs`. An edit is checked the way create is, so
it cannot be pointed at `monitor-fired`. Run now works on a paused monitor and leaves it paused,
and the events of that run itself (`run/{id}`) never wake it again; another monitor, or an
event the run causes elsewhere (a computer assigned, say), can still match. Its history's `cause`
is `event` (the log woke it) or `manual`.

## Attachments: what the server accepts and what the model sees

The shape is the one NativeChat chose in hexuria/nativechat#90 (#229): upload the file to
`POST /artifacts` with `kind: "attachment"`, then name its `art_` id in an AG-UI 1.0 `image` or
`document` part of the user message. The numbers below are the server's, from
`crates/opengrok-server/src/artifacts.rs` and `crates/opengrok-server/src/agui/attachments.rs`.
Show them before a send, so a big file is not found out after it.

| What | Limit | Past it, or otherwise |
|---|---|---|
| Upload, any file | 25 MiB after base64 decoding (`MAX_ARTIFACT_BYTES`) | `413`, "artifacts must be under 25 MiB" |
| Upload types | `image/*`, `video/*`, `application/pdf`, `text/*` | `400`, "only images, videos, PDFs and text files are accepted" |
| Upload name and type | One line (no control character, U+2028 or U+2029), no `"`; the type a plain `type/subtype` | `400`, "the filename must be one line with no quotes, and the type a plain type/subtype" |
| A picture the model sees | `image/png`, `image/jpeg`, `image/gif`, `image/webp`, up to 10 MiB (`IMAGE_MIMES`, `MAX_IMAGE_BYTES`) | Named to the model, not shown to it |
| Pictures in one turn | 8 of them, and 20 MiB in all (`MAX_TURN_IMAGES`, `MAX_TURN_IMAGE_BYTES`) | The rest are named, saying the turn already carries as many as one request can |
| A text file the model reads | Its first 20,000 characters (`MAX_TEXT_CHARS`) | The model is told where it was cut |
| A PDF the model reads | The text of its first 100 pages (`MAX_PDF_PAGES`), then cut like a text file; read within 10 seconds (`PDF_TIMEOUT`) | A PDF that cannot be read, takes too long, or holds no text (a scan) is named, saying so |
| A file from an earlier message | Named to the model, not sent again | |
| An `art_` id the caller does not own, or a missing one | | The turn is refused before it starts: `404 {"error": "no such attachment: art_…"}` |

On replay, a message of files alone is `TEXT_MESSAGE_START` then `TEXT_MESSAGE_END` with the
`messageId` the client sent. Its files are the `GET /artifacts?threadId=` rows whose
`meta.messageId` is that id; each row carries `mime`, `filename` and `sizeBytes`. A row with no
`meta.messageId` was uploaded and never sent.

## When it does not connect

- `/health` from the NativeChat machine first: `curl http(s)://<address>:1447/health` answers
  `{"ok":true,…}`. No answer means host, bind or TLS, not the app.
- A `401` on every call is a token the server cannot verify. The server logs why:
  `a bearer access token did not verify`, with the token's *length*, never its value
  ([`running.md`](running.md), "Logs").
- A reply of text with no `TOOL_*` events is a route that cannot act. Use the console's
  **Test** ([`first-run.md`](first-run.md) step 4).
