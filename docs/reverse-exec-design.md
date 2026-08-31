# Design — Reverse-exec channel (a bot, or the user's phone, drives the user's own Mac)

**Status:** design for review. NOT approved to build. Uriah chose "design doc first,
build later"; nothing here ships until he approves this document.
**Date:** 2026-08-31. Spans `opengrok-server` (the transport — this doc's owner) and
the desktop client (`add-ascii-dev-remote-computer`'s session — the local daemon +
consent UI, already largely built and polling `/local-exec/*`).

## What it is, and why the bar is high

A coworker already has its OWN computer — a sandboxed, disposable box (Local VM or
box.ascii.dev). This channel is different: it lets a request reach the user's **real
machine** (his Mac), so the bot — or the user from his phone while the laptop is
open — can run a command there. The use case Uriah gave: reach his Mac from his
phone while the laptop is open.

The blast radius is the whole point. The bot's box is throwaway; the Mac is not. A
wrong command on the box costs a reset; a wrong command on the Mac is a wrong command
on the user's actual laptop. So every default is **closed**, consent is **explicit**,
and everything that runs is **auditable after the fact**. This is opt-in, never
opt-out.

The prerequisite — the model naming the two machines apart so a request is
unambiguous about Mac-vs-box — is already done (commit "Name the two computers
apart"). Without it, "my computer" would name two real machines and the bot could
choose wrong; now it can't confuse them.

## The security shape (agreed in writing with the client session)

Six points, all load-bearing:

1. **Mutual auth, with a per-MACHINE credential distinct from the account token.**
   The daemon on the Mac authenticates to the server with a *daemon token* minted for
   *that machine* — NOT the account access token. Consequences: a leaked account token
   cannot drive the Mac; revoking the channel (deleting the daemon token) does not log
   the user out; and each machine is individually revocable. The daemon also verifies
   the *server* (the token/response is signed), so a LAN attacker impersonating the
   server cannot feed the daemon commands.

2. **Consent enforced SERVER-SIDE, as a Claude-Code-style permission model** (Uriah's
   call, 31 Aug). Per machine there is a **mode** and two pattern lists, all checked on
   the server before a command is ever queued — a bypassed or compromised client cannot
   get a command through:
   - **mode** = `never` (the DEFAULT — channel off, every command denied) | `ask` |
     `bypass` (allow everything, deliberately turned on, like Claude Code's bypass).
   - an **allowlist** and a **denylist** of command patterns, added ON DEMAND: an `ask`
     prompt offers "run once", "always allow" (→ allowlist), or "always deny" (→
     denylist).
   - **precedence in `ask` mode:** denylist match → DENY; else allowlist match → ALLOW;
     else → ASK the user. `bypass` allows all and skips the lists; `never` denies all.
     Deny always wins over allow within the lists.
   The audit log (point 3) records EVERY command including under `bypass`, so there is
   always a record even when nothing prompts. The decision function is pure and
   closed-by-default (unknown ⇒ deny/ask, never ⇒ deny), tested in isolation before any
   transport can carry a command — gate first.

3. **A durable, user-readable audit log, server-side.** Every reverse-exec request and
   its result is recorded: which bot (or that it was the user from another device),
   the exact command, when, and the outcome. The user can read it afterward. Mandatory,
   not optional — the Mac is not disposable, so "what ran on my laptop, and when" must
   be answerable.

4. **Scope: a bot reaches ONLY its own account's user's daemon.** No cross-account, no
   driving someone else's machine. The daemon token binds a machine to one account; a
   request is refused unless the caller's account owns that daemon.

5. **A higher bar than the box, by design.** `ask` prompts **per command** (a one-off
   "run once" is one command, not a session). Broadening to always-allow a pattern is a
   deliberate act that writes an allowlist rule the user can see and remove; `bypass` is
   a deliberate machine-wide choice, never a default and never inferred. Where the box
   and the Mac differ, the Mac's rule is the stricter one.

6. **The naming prerequisite is done.** A reverse-exec request must be unambiguous
   about Mac-vs-box; the model already distinguishes them and says which it acted on.

7. **A REGISTERED DEVICE, proven by a passkey, or no remote control — period.** (Uriah,
   31 Aug.) On top of the per-machine daemon token, the CONTROL plane requires a
   WebAuthn passkey: a phishing-resistant, hardware-bound credential unlocked by
   biometric/PIN at the moment of the action. An UNREGISTERED device cannot remote-
   control at all — the request is refused before anything else, the same fail-closed
   default as `never`. A stolen account bearer token cannot produce a passkey assertion,
   so it cannot arm or drive the channel. The daemon token (point 1) is the MACHINE's
   identity for the data stream; the passkey is the PERSON-on-a-known-device's proof for
   the control plane — the two are complementary and both are required.

## Passkeys & devices (the control-plane identity)

A per-account registry of **registered devices**, each a WebAuthn credential. Only a
registered device may perform any reverse-exec control action; an unregistered one is
refused, full stop.

- **The registry.** Per account, N devices (the phone, each desktop on its own machine):
  `webauthn_credential(account_id, credential_id, public_key, sign_count, label,
  created_at_ms, last_used_at_ms)`. The private key never leaves the device's secure
  enclave; the server keeps only the public key + a monotonic sign-count (replay guard).
- **Register a device** = a WebAuthn *registration* ceremony from an already-authenticated
  session, on the web dashboard OR on the device's own app. That is how a new phone or a
  new desktop becomes an authorized controller. Nothing else adds a device.
- **Step-up = a WebAuthn *assertion*.** The high-risk control actions — enrolling a
  machine's daemon, turning the channel on, switching to `bypass`, and (per the user's
  choice) approving a command from the phone — issue a challenge the registered device
  signs; the server verifies origin, signature and sign-count before proceeding. No valid
  assertion ⇒ the action is refused.
- **Manage from BOTH the web dashboard AND the device.** List devices (label, created,
  last used), add a device (registration), and REVOKE one — from the console, or from any
  device's own app (a desktop can revoke a lost phone, and vice versa). Revoking a device
  removes its credential immediately; it can no longer satisfy step-up.
- **Multiple machines.** Several desktops on different machines each register their own
  passkey; the phone registers its own. Each is independent; each is independently
  revocable — distinct from the per-machine *daemon* tokens, which live on the Mac side.
- **Fail closed, and no backdoor.** With no registered device, the channel's control
  plane does nothing — you cannot enrol, enable, or drive anything until a device is
  registered. Losing every passkey disables the reverse-exec channel only (never the
  account, which is opt-in and non-essential); you re-register from an authenticated
  session. There is deliberately no recovery bypass — a bypass would defeat the passkey.
- **This is a NEW capability — the original WebAuthn does NOT do it** (client session
  read the source, 31 Aug). The shipped `webauthnProxyEnabled` / `/webauthn/*` is a
  security-key PROXY pointing the OTHER way: it relays a WebAuthn ceremony that
  ORIGINATED elsewhere (a web origin — a browser on the bot's box) to the machine where
  the user's authenticator is physically plugged in, so a headless box can borrow the
  person's local key. It is PER-CEREMONY (a `grant` stage then a `sign` stage, each with
  a requestId, 120s timeout), with NO credential registry and NO device-trust model. It
  never gated the daemon channel — the daemon just shares the transport.
- **What we take from it: the delivery primitive, not a registry.** The proxy is a
  working way to REACH the person's authenticator on demand — exactly what a step-up
  needs. Everything else is ours to build server-side: OpenGrok is the WebAuthn Relying
  Party (it issues the challenge, stores the public key, verifies the signature and the
  sign-count), and the DEVICE REGISTRY (point above) is new. For registration and for a
  step-up assertion, the server issues the ceremony and delivers it through the SAME
  proxy transport the client already speaks — which we must now SERVE (it 404s today,
  like `/local-exec/*`):
  `GET /webauthn/requests` (SSE, server pushes `welcome` / `ceremony{requestId,
  ceremony:{kind, origin, …opaque}}` / `cancel`); `POST /webauthn/responses` (daemon
  sends `hello` / `ping` / `stage{requestId, grant|sign, ok|declined|failed}` /
  `result{requestId, credentialJson}` / `error`). The server verifies the returned
  `credentialJson` as the RP; a step-up is honored only on a `grant`+`sign` success for a
  registered credential. Client half is real, darwin/win32, off by setting today.

## The transport — the REAL contract (read from the client's source, 31 Aug)

Corrected after the client session sent the actual daemon protocol; the earlier
"long-poll + {id, command}" sketch was wrong and would never have matched. The truth:

- **`/local-exec/requests` is an SSE STREAM, not a poll** (same shape as `/events`).
  The daemon opens it once; the server REGISTERS it as a provider and PUSHES
  newline-delimited JSON frames down it (`gateway-server.ts` `handleBridgeRequests` →
  `openSseStream(... bridge.registerProvider(frame => write(...)))`).
- **The command is an opaque message, not a shell string.** Server→daemon frames
  (`local-exec-provider.ts`): `welcome{providerId}`, `retire-approval{approvalId}`,
  `exec{requestId, approvalId?, serverMessage}`, `upload/download/messages-op{requestId,
  approvalId?, …}`, `cancel{requestId}`. Daemon→server (POST `/local-exec/responses`):
  `hello{localRoot, terminalsFolder, computerId, label, …}`, `ping`,
  `client|control{requestId, message}`, `file/file-error/messages-result/messages-error`.
  It is `requestId` (a string), and the exec payload rides `serverMessage` as an opaque
  `JsonValue`; results come back as `client`/`control` frames carrying an opaque
  `message`. The daemon speaks a MESSAGE protocol — it does not take a shell string and
  return an exit code.
- **Consequences for the gate + audit:** the gate matches on a COMMAND STRING, but the
  wire carries an opaque `serverMessage`. So the server must CONSTRUCT the exec frame
  (and derive the human-readable command for the gate + audit) at the point it builds
  the request — we own that shape, we do not read it back. This needs the exec
  `serverMessage` schema, agreed with the client, before slices 4–5 are built.
- **Enrolment moment:** the daemon sends `hello` on connect with `computerId` + `label`;
  and the APP (not the coordinator) enrols via `POST /local-exec/daemon` when the user
  turns the channel on, storing the returned token in the secrets store and handing it
  to the daemon through the same connection descriptor the gateway token travels in.
- **Approval already has a home on the wire:** `approvalId` is on every actionable
  frame and there is a `retire-approval` frame and a `cancel` frame — the ask/approve
  and cancel machinery should ride THOSE, not a parallel invention.
- **Auth today** is the shared gateway bearer (`isAuthorized`, timing-safe compare); the
  per-machine daemon token (slice 3, built) is the agreed addition that replaces it for
  this channel.

Slices 4–5 must be built to THIS, and they are additionally gated on the client
session's own owner approving the client half — server-side approval here does not carry
to that session. So the transport waits on: (a) the client owner's go, and (b) the exec
`serverMessage` schema settled together.

### The earlier (superseded) enqueue/gate sketch, still directionally right
The gate/consent flow below still holds; only the wire shape above changed.

- **Enqueue (a bot tool call, or the phone), account-authenticated.** A request to run
  something on the user's Mac hits the gate:
  - `never` (default) → refused with a readable reason; nothing is queued.
  - `ask` → the run SUSPENDS pending a per-command approval (reuse the existing
    tool-approval `NeedsApproval` machinery — this is the same "a person decides"
    suspension the policy layer already has), and only the approved command is queued.
  - `always` → queued directly, still logged.
  In every case an audit row is written at enqueue AND at result.

- **A distinct tool, not the box `shell`.** The model reaches the Mac through a
  SEPARATE tool (e.g. `user_machine_shell`) whose description says plainly it runs on
  the user's own computer, gated by consent — so "run on my box" and "run on the user's
  Mac" are different tool calls, never one ambiguous sentence. `shell`/`read_file`/
  `write_file` stay box-only.

## Data model (server side)

- `local_exec_daemon(account_id, machine_id, label, token_hash, enrolled_at_ms,
  revoked)` — enrolled machines.
- `local_exec_setting(account_id, machine_id, mode)` — `never|ask|always`, default
  `never` when absent.
- `local_exec_audit(id, account_id, machine_id, origin, command, requested_at_ms,
  decision, exit_code, finished_at_ms)` — the readable log. `origin` = which bot, or
  "user via <device>".
- The command queue itself can be in-memory per replica (a suspended run already
  survives via the journal); the audit log is durable.

## Explicitly NOT in v1

- No file transfer to/from the Mac, no port forwarding, no persistent agent on the
  Mac beyond the daemon — just command exec, so the audit log is a complete record.
- No `always` as a first-run default, ever. No cross-account. No driving a machine the
  account did not enrol.
- No bypass path: there is no server route that runs on the Mac without passing the
  gate and writing an audit row.

## Open questions for Uriah before any code

1. **Approve the shape above?** Especially: daemon token distinct from login, gate
   server-side with `never` default, mandatory audit log.
2. **`ask` granularity:** per-command (proposed) vs per-session. I recommend
   per-command for the Mac; confirm.
3. **Who may enqueue:** only the user's own bots, or also the user directly from
   another device (the phone use case needs the latter — the phone authenticates as
   the account and enqueues, gated the same way). Confirm both are wanted.
4. **Reconcile endpoint names** with the client's existing `/local-exec/requests` and
   `/local-exec/responses` before building, so nothing 404s.

Nothing here is built. On approval, this becomes a sliced plan (enrolment + token →
gate + audit → the two poll endpoints → the distinct tool), each slice tested and the
gate proven closed-by-default before the channel carries a single command.
