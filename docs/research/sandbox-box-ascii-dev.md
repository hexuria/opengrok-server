# box.ascii.dev — the coworker's computer

**Researched:** 29 Aug 2026, against the live docs. Pinned 31 Aug 2026 (create id, delete
header) against a real box. Typed client 1–2 Sep 2026 from the vendor pages in
[`docs/box/`](../box/README.md) (fetched 1 Sep 2026). Live site wins if the local copy drifts.

**Role in OpenGrok:** the first implementation of the `Computer` trait in `crates/opengrok-box`.
A coworker's computer is a *seam*, not a vendor — local Docker is the other adapter, and the
default when no `OG_BOX_API_KEY` is set.

---

## Verdict

Strong candidate, with one real gap: **no native live-streaming exec.** It is a plain-HTTP,
bearer-token REST API fronting persistent Ubuntu VMs — trivially consumable from `reqwest` with no
proprietary transport, no gRPC, no required SDK. It gives exactly what an autonomous coding agent
needs: a real persistent filesystem, SSH/Docker inside the VM, port exposure with public preview
URLs, disk-level snapshot/fork, and a VNC desktop for computer-use.

The weak spot: command execution is either **synchronous** (blocks up to 600s) or **detached with
poll-only status** (log-tail polling — no streaming socket or SSE). True live stdout/stderr for a
Rust server must be built by polling `GET /boxes/{boxId}/commands/{processId}`, or by driving SSH
yourself. Vendor SDKs are Python and TypeScript; **ours is** `opengrok_box::ascii::Client`
(plain REST + bearer, shapes from [`docs/box/`](../box/README.md)). Region is EU-only today.
Hosted-only — no open-source or self-host path found.

**Design consequence, already encoded in `opengrok-box`:** `run` (single result) and `watch` (a poll) are
separate trait methods. Hiding the poll behind a nice `Stream` would conceal the latency from the
caller choosing a timeout.

---

## Endpoints

Base URL: `https://ascii.dev/api/box/v1` — vendor pages mirrored locally in
[`docs/box/`](../box/README.md) (fetched 1 Sep 2026 from
[docs.ascii.dev/box/api/v1](https://docs.ascii.dev/box/api/v1)). Live site wins if they disagree.

| Need | Method + Path | Notes |
|---|---|---|
| Create box | `POST /boxes` | body e.g. `{"ttlSeconds":3600}`; supports `Idempotency-Key` header |
| List boxes | `GET /boxes` | — |
| Get box | `GET /boxes/{boxId}` | states: `provisioning→provisioned→cloning→ready/idle→running→archiving→archived` / `error` |
| Update box | `PATCH /boxes/{boxId}` | — |
| Stop (pause billing) | `POST /boxes/{boxId}/stop` | archives with a snapshot |
| Resume | `POST /boxes/{boxId}/resume` | — |
| Fork (snapshot clone) | `POST /boxes/{boxId}/fork` | idempotency supported |
| Delete | `DELETE /boxes/{boxId}` | confirmation header `X-Ascii-Confirm-Delete` equal to the box id (OpenAPI `ConfirmDelete`; live 31 Aug 2026) |
| **Run command (sync)** | `POST /boxes/{boxId}/commands` — `{command, cwd?, timeoutSeconds (1–600, default 30), detached:false}` → `{type:"command.finished", success, exitCode, stdout, stderr, stdoutTruncated, stderrTruncated, timedOut, startedAt, finishedAt}` | [execute-box-command](https://docs.ascii.dev/box/api/reference/agent/execute-box-command.md) |
| **Run command (detached)** | same endpoint, `detached:true` → `{type:"command.started", processId, pid, logPath, errLogPath}` | — |
| **Poll command status** | `GET /boxes/{boxId}/commands/{processId}?tailBytes=` (default 512KiB, max 524288) → `{processId, pid, status, running, exitCode, signal, command, cwd, startedAt, finishedAt, stdout, stderr, …}` | **poll only, log tail** — [get-command-status](https://docs.ascii.dev/box/api/reference/agent/get-command-status.md) |
| Interrupt running work | `POST /boxes/{boxId}/interrupt` | — |
| **Read file** | `GET /boxes/{boxId}/files?path=&encoding=utf8\|base64` → `{type:"file.read", success, path, encoding, size, content}` | [read-box-file](https://docs.ascii.dev/box/api/reference/agent/read-box-file.md) |
| **Write file** | `PUT /boxes/{boxId}/files` — `{path, content, encoding?}` → `{type:"file.written", success, path, encoding, size}` | [write-box-file](https://docs.ascii.dev/box/api/reference/agent/write-box-file.md) |
| List directory | *no dedicated endpoint found* | use shell `ls` through `/commands` |
| Download artifact | `GET /boxes/{boxId}/artifacts?path=` → binary `application/octet-stream` | [download-box-artifact](https://docs.ascii.dev/box/api/reference/agent/download-box-artifact.md) |
| Bulk upload | not documented as a REST endpoint | `PUT /files` with base64 for small files; SCP (`box scp` CLI) for larger — **unverified** |
| **Expose port / preview URL** | `POST /boxes/{boxId}/host` — `{port, title}` → `https://<box-subdomain>-<port>.on.ascii.dev?_token=<token>` | tokenised by default; `--public` drops the token. [hosting.md](https://docs.ascii.dev/box/hosting.md) |
| **Desktop / computer-use** | `POST /boxes/{boxId}/desktop?vnc=1&theme=` → `{type:"desktop.url", desktopUrl, ip, mode, provisioning, message}` | a **noVNC/VNC URL only** — no REST screenshot/click primitives. [get-desktop-streaming-url](https://docs.ascii.dev/box/api/reference/agent/get-desktop-streaming-url.md) |
| SSH access | `POST /boxes/{boxId}/sshkey` + `box ssh` CLI | body field is OpenAPI `key`, not `publicKey` |
| Events | `GET /boxes/{boxId}/events` | agent-prompt/lifecycle events, **not** raw shell stdout |
| Prompt an in-box AI agent | `POST /boxes/{boxId}/prompt` — `{provider:"codex"\|"claude-code", model?, reasoningEffort?, prompt}`; status via `GET /boxes/{boxId}/prompts/{promptId}` | ASCII's own agent runner — *not* what OpenGrok uses; we bring our own harness |
| Snapshots | `GET /snapshots`, `GET /boxes/{boxId}/snapshots[/latest]`, `GET /snapshots/{id}/{tree,files,download}`, `DELETE /snapshots/{id}` | `fork` is the restore/clone primitive |
| Account / limits | `GET /me`, `GET /limits`, `GET /account/data-retention`, `GET /api-keys`, `GET /api-keys/{id}/usage` | — |

SDKs: Python `ascii-box-sdk`, TypeScript `@asciidev/box-sdk`
([overview](https://docs.ascii.dev/box/sdks/overview)). **Our Rust client** is
`opengrok_box::ascii::Client`, typed from [`docs/box/`](../box/README.md). The `Computer` trait
(`AsciiBoxes`) is a thin adapter over it.

---

## Auth and session model

- **Auth:** `Authorization: Bearer $BOX_API_KEY`, key format `box_…`. Created via `box api-key
  create` or the dashboard. No OAuth for API use (OAuth login exists only for interactive CLI /
  dashboard sign-in). No per-box control-plane token — one account key governs all boxes; hosted
  preview URLs get their own short-lived `_token` query param.
- **Lifecycle:** created with optional `ttlSeconds` (quickstart uses 3600). `stop` archives with a
  snapshot and pauses billing; `resume` restarts; `fork` clones via fast disk snapshot. The
  **filesystem persists** across stop/resume and fork — it is VM-disk based, not container-ephemeral.
  `DELETE` is permanent.
- **Sleep and wake, as observed live (bx_ncfmdpem, 2 Sep 2026):** the sleeping state is
  `archived`, never `stopped` — both `POST /stop` and the TTL auto-stop snapshot the disk and leave
  the box `archived` (`archiving` on the way there; `POST /stop` on a fresh box landed on `archived`
  within seconds). `POST /resume` answers **202** with `status: resuming` and `box.state:
  provisioning`; a single 202 is not a running box. `GET /boxes/{id}` then reads
  `provisioning → provisioned → ready/idle/running` in about 10–15s. A command sent before
  `ready` is refused **409 `box_starting`** (retryable). `POST /desktop?vnc=1` answers
  `provisioning: true` with no URL until noVNC is up; the `desktopUrl` followed `running` by 5–6s.
  Resuming a box that is already resuming is a 409; `archiving` cannot be resumed until it has
  landed on `archived`. `opengrok_box::Computer::wake` encodes exactly this (resume once, poll
  `state`, wait through `archiving`), and `ensureForeverBox` / a turn's `tools_for_coworker` use
  it; `resume` alone is only the request. Resume counts as a machine start against the plan's
  per-minute rate limit.
- **Concurrency:** plan-gated, 100 → 1,500 concurrent boxes ($20/mo tier = 100 concurrent), with
  creation rate limits (e.g. 10/min, 50/hr, 150/day on the $20 plan).
  [billing.md](https://docs.ascii.dev/box/billing.md)
- **Idempotency:** `POST /boxes` and `POST /boxes/{id}/fork` accept `Idempotency-Key` (24h window).
- **Free trial:** 7 days, 2 concurrent boxes, 25 total hours, 2-hour auto-stop — not viable for
  continuous CI.

---

## Minimal Rust flow

Do not copy the 29 Aug raw-`reqwest` sketch — it guessed `created["id"]` and omitted the
delete header. Use `opengrok_box::ascii::Client` (`crates/opengrok-box/src/ascii/client.rs`).
`AsciiBoxes` is the `Computer` adapter on top of it.

Pinned shapes (do not "simplify"):

- Create id is documented `box.id` (`bx_…`). `CreateBoxResponse::id()` also accepts a bare
  `id` / `boxId` so a stand-in that has not been updated still works. Rust 2024 reserves
  `box`, so the nested field is `box_` with `#[serde(rename = "box")]`.
- Delete sends `X-Ascii-Confirm-Delete` equal to the box id (`CONFIRM_DELETE_HEADER`).
- Desktop: `POST /boxes/{id}/desktop?vnc=1` → `desktopUrl`. First call after ensure can
  return no URL while the desktop is still provisioning; a later poll carries the link.
  Do not log the URL — it carries a password / `_token`.
- SSH key body field is OpenAPI `key`, not `publicKey`.

Covered by `Client` today: create (optional `Idempotency-Key`) / get / list, stop / resume /
fork / delete, run_command / start_command / command_status, read / write files, host_port,
desktop, interrupt, configure_ssh_key, update_box. Not yet: snapshots, environments,
webhooks, ASCII's in-box prompt agent, secrets, repos, artifacts, events, `/me` — add when
a coworker path needs them.

---

## Gaps and risks

1. **No true streaming exec.** Sync (≤600s) or detached-with-polling; no WS/SSE for stdout. Live
   terminal output means polling `tailBytes` tails, or driving SSH/PTY yourself. `run` and
   `watch` stay separate trait methods for this reason.
2. **No directory-listing endpoint** — shell out to `ls`.
3. **No computer-use action API** — a VNC URL, not click/screenshot primitives. The desktop
   client's right-sidebar screen is that URL in an Electron `<webview>` (`*.on.ascii.dev/vnc.html`).
   `Page.captureScreenshot` of the renderer does **not** composite `<webview>` pixels — an empty
   thumbnail in a CDP capture is not proof the screen is blank; click the preview.
4. **Hosted-only, closed-source.** No self-host path found. *This is the argument for keeping
   `Computer` a trait and shipping a local Docker implementation* — which we did.
5. **EU-only regions** (Germany, Finland, France) — latency and data-residency implications.
6. **Plan-gated concurrency and creation rate limits** — a server running many coworkers must budget
   against the tier.
7. **Client coverage.** Snapshots / environments / webhooks / prompt-agent / secrets / repos /
   artifacts / events / `/me` are documented in `docs/box/` and not in `ascii::Client` yet.

The two shapes that were unpinned here (create id; delete header) are pinned — see above.

---

## Sources

- [box.ascii.dev](https://box.ascii.dev/) · [compare/pricing](https://box.ascii.dev/compare)
- [API v1 index](https://docs.ascii.dev/box/api/v1) · [quickstart](https://docs.ascii.dev/box/quickstart)
- [execute-box-command](https://docs.ascii.dev/box/api/reference/agent/execute-box-command.md) ·
  [get-command-status](https://docs.ascii.dev/box/api/reference/agent/get-command-status.md)
- [read-box-file](https://docs.ascii.dev/box/api/reference/agent/read-box-file.md) ·
  [write-box-file](https://docs.ascii.dev/box/api/reference/agent/write-box-file.md) ·
  [download-box-artifact](https://docs.ascii.dev/box/api/reference/agent/download-box-artifact.md)
- [get-desktop-streaming-url](https://docs.ascii.dev/box/api/reference/agent/get-desktop-streaming-url.md)
- [hosting](https://docs.ascii.dev/box/hosting.md) · [snapshots](https://docs.ascii.dev/box/snapshots.md) ·
  [billing](https://docs.ascii.dev/box/billing.md) · [production use](https://docs.ascii.dev/box/use-in-production.md) ·
  [CLI reference](https://docs.ascii.dev/box/cli-reference.md) · [SDKs](https://docs.ascii.dev/box/sdks/overview)
- Local mirror: [`docs/box/`](../box/README.md) (fetched 1 Sep 2026). Live site wins if they disagree.
