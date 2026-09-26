# Reference documents

Everything a fresh session needs to know about the systems OpenGrok touches. Each was written by
reading the source, not from memory; each anchors its claims to file paths or primary-source URLs.

| Document | Covers | Read it before |
|---|---|---|
| [`client-nativechat.md`](client-nativechat.md) | **The client served today.** Every route the server mounts, grouped roster / conversation / approvals / attachments / MCP and skills / automations / computer, with where it is mounted, what it answers, and who is on record calling it. Written from the server's source; NativeChat's own source is **not yet read**, so its column says *unverified* until it is | touching any route, or connecting NativeChat |
| [`client-grok-bot.md`](client-grok-bot.md) | **Removed 20 Sep 2026 — the record.** The Grok Bot desktop client: architecture, the 123 `SAND_GATEWAY_COMMANDS`, transcript entry kinds and card types, the live activity stream, the box seam, and the first-boot call order | touching `opengrok-wire` or `opengrok-server` |
| [`gateway-open-ai-gateway.md`](gateway-open-ai-gateway.md) | open-ai-gateway: crate layout, the two listeners, auth, the model-pin dialect, endpoints, the `/v1/models` diagnostics envelope, and whether it can be embedded | touching `opengrok-harness` or planning the single-binary release |
| [`lessons-opensesame.md`](lessons-opensesame.md) | The previous product: what was tried, what broke, and the rules that came out — thread binding, the woven timeline, the fan-out collapse, the refresh bug, model pins, and the hosted-dependency trap | designing `opengrok-store` or `opengrok-harness` |
| [`sandbox-box-ascii-dev.md`](sandbox-box-ascii-dev.md) | box.ascii.dev: verdict, full endpoint table, pinned shapes (`box.id`, `X-Ascii-Confirm-Delete`), the typed `ascii::Client`, and the gaps (no streaming exec, no dir listing, VNC-only computer-use, hosted-only) | touching `opengrok-box` |
| [`../box/README.md`](../box/README.md) | Local copy of the Box Public API v1 pages + OpenAPI spec (vendor markdown, not ours) | fixing or extending `opengrok-box` against the live contract |
| [`connectors-open-connector.md`](connectors-open-connector.md) | open-connector: what is genuinely open vs hosted (an earlier audit was wrong here), the action schema, the executor problem, tenancy and security defaults | revisiting connectors; not adopted, slice 5 shipped them natively |

## Headline findings, so they are not buried

Each doc is long because the detail is the point. These four are the ones that change what you build:

- **The removed Grok Bot client refused a loopback gateway host** — serve on a non-loopback
  hostname or it failed with no useful error (`client-grok-bot.md`). Whether NativeChat has the
  same rule is unverified (`../setup/nativechat.md`).
- **The removed client's Sand gateway was 123 commands** (90 reachable from the renderer, 33 host-only) **plus an
  18-channel SSE stream, `/health` and `/avatars`** — and `api2.cursor.sh` is a *second* seam that is
  explicitly not ours (`client-grok-bot.md`).
- **The gateway embeds cleanly** — `oag_server::public_router()` gives a wired Axum router; the
  obstacles are process-global singletons, a private settings loader, and a catalogue refresh that
  fails silently if you forget it (`gateway-open-ai-gateway.md` §8).
- **open-connector's OAuth is open, not paywalled** — an earlier audit had this backwards, and
  believing it would have bought a hosted tier we do not need (`connectors-open-connector.md`).

The removed client's per-version protocol survey (0.18 to 0.30) is archived as
[`../archive/client-versions-0.18-0.30.md`](../archive/client-versions-0.18-0.30.md).

## Keeping them honest

A reference doc that has drifted is worse than none, because it is trusted. If you find a claim that
no longer holds, fix the document in the same commit as the code — and say in the commit body that
you did. Mark anything unverified as unverified rather than smoothing it over.

The two box.ascii.dev shapes that used to be flagged that way are now pinned (create id is
`box.id`; delete confirmation is `X-Ascii-Confirm-Delete` equal to the box id) — see
[`sandbox-box-ascii-dev.md`](sandbox-box-ascii-dev.md). Bulk upload is still unverified as a REST
endpoint. The typed client is `opengrok_box::ascii::Client`; vendor pages live in
[`docs/box/`](../box/README.md).
