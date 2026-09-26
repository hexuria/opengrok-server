# Archive

Documents that finished their job. Each is kept because its reasoning is still worth reading;
none is current, and nothing links here for instructions. One line each:

| File | What it was | Superseded by |
|---|---|---|
| [`handover-2026-08-29.md`](handover-2026-08-29.md) | the P0-era handover ("nothing serves yet") | [`../HANDOVER.md`](../HANDOVER.md) |
| [`runbook-p1.md`](runbook-p1.md) | standing P1 up against port 1337 and the seam-B mock | [`../setup/`](../setup/README.md) |
| [`plan-web-console.md`](plan-web-console.md) | the web-console build plan | executed as ROADMAP slice 13; evidence in [`../verification/web-console/`](../verification/web-console/README.md) |
| [`identity-model.md`](identity-model.md) | the locked identity model (orgs, invites, credentials) | executed as ROADMAP slice 12; the deferred bits live in ROADMAP 12.later |
| [`reverse-exec-design.md`](reverse-exec-design.md) | the reverse-exec channel design | built and merged; the final consent model is [`../AUTO-REVIEW.md`](../AUTO-REVIEW.md); its passkey step-up is parked in ROADMAP "Later" |
| [`plan-bots-computers-channels.md`](plan-bots-computers-channels.md) | bots + computers + channels plan | provisioning shipped (`41245b5`); the channels half is parked in ROADMAP "Later" |
| [`port-blockers.md`](port-blockers.md) | B1: the `SAND_HOST_GATEWAY_URL` launch deadlock | moot — the client's OpenGrok server mode is the supported repoint; [`verification/real-client/README.md`](verification/real-client/README.md) |
| [`pr-queue-2026-09-04.md`](pr-queue-2026-09-04.md) | how the six-PR queue (#52–#57) actually reached `main` on 4 Sep 2026, wrong turns left in | all six merged; the gaps it lists as left open became issues #58 (closed) and #59 (closed by PR #63), the rest ROADMAP entries |
| [`seam-a-client-facts.md`](seam-a-client-facts.md) | CLAUDE.md's "Three facts" #1 and #3, about the Grok Bot client and seam A, word for word | the client and its doors were removed 20 Sep 2026; NativeChat is [`../setup/nativechat.md`](../setup/nativechat.md) |
| [`handover-2026-09-01.md`](handover-2026-09-01.md) | the 1 Sep handover for ROADMAP 9.v and 10.3 (seam B, the Grok Bot client) | [`../HANDOVER.md`](../HANDOVER.md) |
| [`plan-rooms.md`](plan-rooms.md) | groups and shared rooms | groups were built for the Grok Bot client and removed with it on 20 Sep 2026; shared rooms are parked in ROADMAP |
| [`plan-coworker-model-pins.md`](plan-coworker-model-pins.md) | the model-pins investigation | built as ROADMAP slice 18, which corrects several of its claims |
| [`plan-slice16-later.md`](plan-slice16-later.md) | the PolicyApproval card and OAuth 2.1 on `/mcp` | built as ROADMAP 16.policy and 16.oauth; `auth/oauth_mcp.rs` still cites its §2.1 spec digest |
| [`findings-og139.md`](findings-og139.md) | review findings on PR #139 | the fixed ones are marked; its paths name the `gateway/` modules deleted 20 Sep 2026 |
| [`desktop-client.md`](desktop-client.md) | connecting the Grok Bot desktop client | the client and its doors were removed 20 Sep 2026; [`../setup/nativechat.md`](../setup/nativechat.md) |
| [`client-versions-0.18-0.30.md`](client-versions-0.18-0.30.md) | the Grok Bot client's protocol across versions 0.18 to 0.30 | the client was removed 20 Sep 2026; [`../research/client-nativechat.md`](../research/client-nativechat.md) |
| [`verification/real-client/`](verification/real-client/README.md) | the packaged Grok Bot app against this server: the dead env-var repoint, the `EnsureSandBox` mint | removed with the client, 20 Sep 2026 |
| [`verification/plan-mode-wire/`](verification/plan-mode-wire/README.md) | capture showing the packaged app's `sendPrompt` sends no `mode` | removed with the client, 20 Sep 2026 |
| [`artifacts/two-doors.html`](artifacts/two-doors.html) | diagram №2, the two doors into the Grok Bot client | both doors removed 20 Sep 2026; [`../DIAGRAMS.md`](../DIAGRAMS.md) №3 is the AG-UI desk |
