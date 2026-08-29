# Reference documents

Everything a fresh session needs to know about the systems OpenGrok touches. Each was written by
reading the source, not from memory; each anchors its claims to file paths or primary-source URLs.

| Document | Covers | Read it before |
|---|---|---|
| [`client-grok-bot.md`](client-grok-bot.md) | The Grok Bot desktop client: architecture, the 123 `SAND_GATEWAY_COMMANDS`, transcript entry kinds and card types, the live activity stream, the box seam, and the first-boot call order | touching `og-wire` or `og-server` |
| [`gateway-open-ai-gateway.md`](gateway-open-ai-gateway.md) | open-ai-gateway: crate layout, the two listeners, auth, the model-pin dialect, endpoints, the `/v1/models` diagnostics envelope, and whether it can be embedded | touching `og-harness` or planning the single-binary release |
| [`lessons-opensesame.md`](lessons-opensesame.md) | The previous product: what was tried, what broke, and the rules that came out — thread binding, the woven timeline, the fan-out collapse, the refresh bug, model pins, and the hosted-dependency trap | designing `og-store` or `og-harness` |
| [`sandbox-box-ascii-dev.md`](sandbox-box-ascii-dev.md) | box.ascii.dev: verdict, full endpoint table, auth and session model, a minimal Rust flow, and the gaps (no streaming exec, no dir listing, VNC-only computer-use, hosted-only) | touching `og-box` |
| [`connectors-open-connector.md`](connectors-open-connector.md) | open-connector: what is genuinely open vs hosted (an earlier audit was wrong here), the action schema, the executor problem, tenancy and security defaults | touching `og-tools` or planning P6 |

## Headline findings, so they are not buried

Each doc is long because the detail is the point. These four are the ones that change what you build:

- **The client refuses a loopback gateway host** — serve on a non-loopback hostname or it fails with
  no useful error (`client-grok-bot.md`).
- **The Sand gateway is 123 commands** (90 reachable from the renderer, 33 host-only) **plus an
  18-channel SSE stream, `/health` and `/avatars`** — and `api2.cursor.sh` is a *second* seam that is
  explicitly not ours (`client-grok-bot.md`).
- **The gateway embeds cleanly** — `oag_server::public_router()` gives a wired Axum router; the
  obstacles are process-global singletons, a private settings loader, and a catalogue refresh that
  fails silently if you forget it (`gateway-open-ai-gateway.md` §8).
- **open-connector's OAuth is open, not paywalled** — an earlier audit had this backwards, and
  believing it would have bought a hosted tier we do not need (`connectors-open-connector.md`).

## Keeping them honest

A reference doc that has drifted is worse than none, because it is trusted. If you find a claim that
no longer holds, fix the document in the same commit as the code — and say in the commit body that
you did. Mark anything unverified as unverified rather than smoothing it over; two shapes in the
box.ascii.dev report are flagged exactly that way, and pinning them is a scheduled task, not a
lingering doubt.
