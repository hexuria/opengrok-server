# box.ascii.dev — the coworker's computer

**Researched:** 29 Aug 2026, against the live docs. Verdict and endpoint table below are
primary-source; two shapes are explicitly marked unverified and must be pinned against a real box
before production structs are written.

**Role in OpenGrok:** the first implementation of the `Computer` trait in `crates/og-box`. A
coworker's computer is a *seam*, not a vendor — a local Docker implementation is expected to follow.

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
yourself. There is no Rust SDK and none is needed. Region is EU-only today. Hosted-only — no
open-source or self-host path found.

**Design consequence, already encoded in `og-box`:** `run` (single result) and `watch` (a poll) are
separate trait methods. Hiding the poll behind a nice `Stream` would conceal the latency from the
caller choosing a timeout.

---

## Endpoints

Base URL: `https://ascii.dev/api/box/v1` — reference index at
[docs.ascii.dev/box/api/v1](https://docs.ascii.dev/box/api/v1).

| Need | Method + Path | Notes |
|---|---|---|
| Create box | `POST /boxes` | body e.g. `{"ttlSeconds":3600}`; supports `Idempotency-Key` header |
| List boxes | `GET /boxes` | — |
| Get box | `GET /boxes/{boxId}` | states: `provisioning→provisioned→cloning→ready/idle→running→archiving→archived` / `error` |
| Update box | `PATCH /boxes/{boxId}` | — |
| Stop (pause billing) | `POST /boxes/{boxId}/stop` | archives with a snapshot |
| Resume | `POST /boxes/{boxId}/resume` | — |
| Fork (snapshot clone) | `POST /boxes/{boxId}/fork` | idempotency supported |
| Delete | `DELETE /boxes/{boxId}` | requires a confirmation header (**name/value unverified**) |
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
| SSH access | `POST /boxes/{boxId}/sshkey` + `box ssh` CLI | the escape hatch for a real PTY |
| Events | `GET /boxes/{boxId}/events` | agent-prompt/lifecycle events, **not** raw shell stdout |
| Prompt an in-box AI agent | `POST /boxes/{boxId}/prompt` — `{provider:"codex"\|"claude-code", model?, reasoningEffort?, prompt}`; status via `GET /boxes/{boxId}/prompts/{promptId}` | ASCII's own agent runner — *not* what OpenGrok uses; we bring our own harness |
| Snapshots | `GET /snapshots`, `GET /boxes/{boxId}/snapshots[/latest]`, `GET /snapshots/{id}/{tree,files,download}`, `DELETE /snapshots/{id}` | `fork` is the restore/clone primitive |
| Account / limits | `GET /me`, `GET /limits`, `GET /account/data-retention`, `GET /api-keys`, `GET /api-keys/{id}/usage` | — |

SDKs: Python `ascii-box-sdk`, TypeScript `@asciidev/box-sdk`
([overview](https://docs.ascii.dev/box/sdks/overview)). **No Rust SDK — not needed.**

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
- **Concurrency:** plan-gated, 100 → 1,500 concurrent boxes ($20/mo tier = 100 concurrent), with
  creation rate limits (e.g. 10/min, 50/hr, 150/day on the $20 plan).
  [billing.md](https://docs.ascii.dev/box/billing.md)
- **Idempotency:** `POST /boxes` and `POST /boxes/{id}/fork` accept `Idempotency-Key` (24h window).
- **Free trial:** 7 days, 2 concurrent boxes, 25 total hours, 2-hour auto-stop — not viable for
  continuous CI.

---

## Minimal Rust flow (verified endpoints only)

```rust
use serde_json::json;

const BASE: &str = "https://ascii.dev/api/box/v1";

async fn box_flow(api_key: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let auth = format!("Bearer {api_key}");

    // (a) create
    let created: serde_json::Value = client
        .post(format!("{BASE}/boxes"))
        .header("Authorization", &auth)
        .json(&json!({ "ttlSeconds": 3600 }))
        .send().await?.json().await?;
    // UNVERIFIED: the field name carrying the new box's id. Pin against a real box.
    let box_id = created["id"].as_str().unwrap_or_default().to_string();

    // (b) run to completion
    let ls: serde_json::Value = client
        .post(format!("{BASE}/boxes/{box_id}/commands"))
        .header("Authorization", &auth)
        .json(&json!({ "command": "ls -la", "timeoutSeconds": 30, "detached": false }))
        .send().await?.json().await?;
    println!("{}", ls["stdout"]);

    // (b2) long-running: detached + poll. There is no streaming socket.
    let started: serde_json::Value = client
        .post(format!("{BASE}/boxes/{box_id}/commands"))
        .header("Authorization", &auth)
        .json(&json!({ "command": "npm run build", "detached": true }))
        .send().await?.json().await?;
    let process_id = started["processId"].clone();
    loop {
        let status: serde_json::Value = client
            .get(format!("{BASE}/boxes/{box_id}/commands/{process_id}"))
            .header("Authorization", &auth)
            .send().await?.json().await?;
        if status["running"] == false { break; }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    // (c) write a file
    client.put(format!("{BASE}/boxes/{box_id}/files"))
        .header("Authorization", &auth)
        .json(&json!({ "path": "/home/user/hello.txt", "content": "hi", "encoding": "utf8" }))
        .send().await?;

    // (d) destroy — UNVERIFIED: the confirmation header's name/value.
    client.delete(format!("{BASE}/boxes/{box_id}"))
        .header("Authorization", &auth)
        .send().await?;

    Ok(())
}
```

---

## Gaps and risks

1. **No true streaming exec.** Sync (≤600s) or detached-with-polling; no WS/SSE for stdout. Live
   terminal output means polling `tailBytes` tails, or driving SSH/PTY yourself.
2. **No directory-listing endpoint** — shell out to `ls`.
3. **No computer-use action API** — a VNC URL, not click/screenshot primitives. Agentic computer-use
   means driving VNC ourselves.
4. **No Rust SDK** — immaterial; plain REST + bearer.
5. **Hosted-only, closed-source.** No self-host path found. *This is the argument for keeping
   `Computer` a trait and adding a local Docker implementation.*
6. **EU-only regions** (Germany, Finland, France) — latency and data-residency implications.
7. **Plan-gated concurrency and creation rate limits** — a server running many coworkers must budget
   against the tier.
8. **Two unpinned shapes** (created-box id field; delete confirmation header) — pull the raw OpenAPI
   spec and hit a real box before writing production structs. This is a P3 task.

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
