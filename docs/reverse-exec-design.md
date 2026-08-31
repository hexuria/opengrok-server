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

2. **Consent enforced SERVER-SIDE, before a command is ever queued.** The
   `never | ask | always` setting is checked on the server; it never hands a command to
   the daemon unless the setting permits. A bypassed or compromised client cannot get a
   command through. Default is **never** — the channel does nothing until the user
   deliberately turns it on.

3. **A durable, user-readable audit log, server-side.** Every reverse-exec request and
   its result is recorded: which bot (or that it was the user from another device),
   the exact command, when, and the outcome. The user can read it afterward. Mandatory,
   not optional — the Mac is not disposable, so "what ran on my laptop, and when" must
   be answerable.

4. **Scope: a bot reaches ONLY its own account's user's daemon.** No cross-account, no
   driving someone else's machine. The daemon token binds a machine to one account; a
   request is refused unless the caller's account owns that daemon.

5. **A higher bar than the box, by design.** `ask` is **per-command**, never a blanket
   session yes. `always` is a deliberate, scoped choice a person turns on, never a
   default and never inferred. Where the box and the Mac differ, the Mac's rule is the
   stricter one.

6. **The naming prerequisite is done.** A reverse-exec request must be unambiguous
   about Mac-vs-box; the model already distinguishes them and says which it acted on.

## The transport (server side — the part that is ours)

The client already runs a local-exec daemon that polls two endpoints (hence today's
`/local-exec/*` 404s). The server side is a small, auditable queue with a strict
gate. Proposed shape (names to be reconciled with the client's existing calls before
building):

- **Daemon enrolment.** `POST /local-exec/daemon` (authenticated as the account) mints
  a **daemon token** bound to `{account_id, machine_id, machine_label}`, shown once,
  stored hashed. `DELETE /local-exec/daemon/{machine_id}` revokes it. A daemon token
  is a new credential kind in `auth::token`, distinct from access/refresh/bot keys, and
  it authorises ONLY the two poll endpoints below — nothing else on the server.

- **Daemon poll (long-poll), daemon-authenticated.**
  `GET /local-exec/requests` returns the next approved command for THIS machine, or
  waits. The server only ever enqueues a command here after the consent gate said yes;
  the daemon never sees a command the gate refused.
  `POST /local-exec/responses` returns a command's stdout/stderr/exit, which the server
  writes to the audit log and hands back to the caller (the bot's turn, or the phone).

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
