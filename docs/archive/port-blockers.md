# Port blockers — measured, not guessed

> **Archived 1 Sep 2026 — moot.** B1 still reproduces, but nothing leans on the env var any more: the client connects through its own OpenGrok server mode. `../verification/real-client/README.md` is the re-check record.

## B1 — `SAND_HOST_GATEWAY_URL` deadlocks the reconstructed app before its window (30 Aug 2026)

**The repoint the whole port ladder leans on does not currently work in the reconstructed
client.** Slice 7's server side is built and smoke-verified; its live verification (7.v) is
blocked on this client-side bug.

Isolation, each step observed on this machine:

| Launch | Result |
|---|---|
| no env vars, `boxRuntime: local-docker` | window in ~6 s (the everyday workflow) |
| `SAND_HOST_GATEWAY_URL` + token, `boxRuntime: remote` | **no window, ever** |
| `SAND_HOST_GATEWAY_URL` + token, `boxRuntime: local-docker` | **no window, ever** — so the runtime setting is not the trigger; the env var is |

State of the hung process (`kill -USR1`, inspector on 9229, `Runtime.evaluate`):

- `process._getActiveHandles()` → **zero handles.** No sockets, no timers, no child
  processes. The main process is not waiting on I/O; it is awaiting a JS promise that
  nothing will ever resolve.
- It never dialed the gateway: zero connections to our listener, no line appended to
  `sand-data/host-connector.log`, so the hang precedes `createSettingsRoutedHostConnector`.
- `main.ts:403` awaits `initializeServices(...)` before `createWindow()` at `:430` — the
  deadlock is inside service init.

Where to look first (readers of the env var during init, from `grep`):

- `account/production-account-authorization.ts:43` — env presence switches on
  `createEnvDescriptorAccountBinding` + `createDesktopAccountAuthorizer`, the account-
  authorization machinery. This is the branch that exists **only** when the env var is set,
  which matches the trigger exactly.
- `dev/dev-box-recreate-plane.ts:45` and `dev/dev-controls-window.ts:24` read it too, but
  are dev-plane paths.

Until this is fixed in the client, 7.v/8.v (real-app verification) cannot run; the wire
contract is held by `scripts/slice11-gateway-smoke.sh` instead. The server side needs no
change — the same launch against `http://192.168.100.21:1447` is the test to re-run once
the client boots with the env var set.

Repro (client repo, this machine):

```sh
pkill -9 -f "Open Grok.app/Contents"
env SAND_HOST_GATEWAY_URL="http://192.168.100.21:1447" SAND_HOST_GATEWAY_TOKEN=x \
  "/Applications/Open Grok.app/Contents/MacOS/Grok Bot" --remote-debugging-port=9223
# window never appears; osascript window count stays 0; /json/list stays []
```
