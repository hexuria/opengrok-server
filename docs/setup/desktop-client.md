# Pointing the desktop client at this server

The packaged Grok Bot reconstruction (`/Volumes/goldcoders/OSS/opengrok`, installed as
`/Applications/Open Grok.app`) connects through its own **OpenGrok server mode** — an in-app
runtime, not an environment variable.

## How the connection works

Two client-side facts, one server-side:

1. **The runtime switch is `boxRuntime: "opengrok"`** with the gateway address in the
   `openGrokGatewayUrl` setting (`sand-data/settings.json` in the client's data root; set both
   through the app's Computer settings). Crossing into or out of server mode deliberately signs
   the account out — one account, never two (`source/electron-main/main-edge.ts:523-524`).
2. **`SAND_HOST_GATEWAY_URL` is a dead path.** Launching the app with that env var set
   deadlocks it before the window opens (re-verified 1 Sep 2026 —
   `../verification/real-client/README.md`). Do not debug a launch that uses it; use server
   mode.
3. **The server must be reachable on a non-loopback address** and the client must present the
   server's `OG_GATEWAY_BEARER`. The client throws when its gateway host starts with
   `127.0.0.1`/`localhost`, so use the LAN address (`http://192.168.100.24:1447` today — DHCP
   moves it; see the trap below).

## Traps that each cost a debugging session

- **The DHCP address goes stale.** `openGrokGatewayUrl` persists an absolute address; when the
  machine's LAN address moves, the app sits in `SYN_SENT` forever and the roster looks dead.
  Check `lsof -nP -i :1447` (a connection to the *old* address is the tell) and fix the
  setting.
- **The roster silently stops updating.** The client's coordinator drops every `agents` and
  `agent-upserted` SSE event when its persisted `inferenceProvider` is `claude-code`, `codex`,
  or `openrouter`. Check `settings.json` before suspecting the server.
- **An empty success is the dangerous reply.** `listAgents` returning `[]` is valid and paints
  a working app with no coworkers; reply *shapes* throw (`countAgents` must be a number,
  `getTrays` an array, `getForeverBoxStatus` null-or-record). Seed one coworker before judging
  the boot.

## Verifying against the packaged app

Every UI claim is verified in the packaged app over CDP, never in the client's recovered
source tree (which does not ship):

```sh
# in the client repo: package and install, then launch with a debug port
just package && just install
"/Applications/Open Grok.app/Contents/MacOS/Grok Bot" --remote-debugging-port=9223

# inspect and drive from the client repo's tools
node docs/research/tools/cdp-eval-main.mjs "document.body.innerText.slice(0,400)"
```

Server-side, `OG_TRACE_REQUESTS=1` shows each gateway call as the app makes it. The end-to-end
evidence pattern — client screenshot paired with the server's own rows — is what
`../verification/` holds; follow those examples when closing a roadmap box.
