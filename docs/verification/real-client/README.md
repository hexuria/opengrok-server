# The real client against this server — what is proven, and by which path

Re-checked 1 Sep 2026 against the installed packaged app (`/Applications/Open Grok.app`) and the
live server on `:1447` (pid 10650, `{"ok":true}` on `/health`).

## The B1 re-check: the env-var repoint is still broken, and no longer matters

`docs/archive/port-blockers.md` B1 (30 Aug) reported that launching the app with
`SAND_HOST_GATEWAY_URL` set deadlocks it before its window opens. Re-run today with the current
LAN address:

```sh
env SAND_HOST_GATEWAY_URL="http://192.168.100.24:1447" SAND_HOST_GATEWAY_TOKEN=… \
  "/Applications/Open Grok.app/Contents/MacOS/Grok Bot" --remote-debugging-port=9223
```

Result, observed over 30+ seconds: CDP answers on `:9223` with **zero targets**, window count
**0**, and **no connection to `:1447` is ever attempted** (`lsof`). B1 reproduces unchanged.

It is moot rather than fixed: the client now carries its own **OpenGrok server mode** — the
`boxRuntime: "opengrok"` runtime plus the persisted `openGrokGatewayUrl` setting
(`source/electron-main/main-edge.ts:158,523-524`; crossing into or out of that mode deliberately
signs the account out, "one account, never two"). That is the supported repoint; the env var is a
dead path on this build and the port ladder no longer leans on it.

## 7.v / 8.v — the populated sidebar and the streamed conversation

Proven 31 Aug 2026 through the server-mode path, packaged app on `:1447`:

- **Client half:** the peer session's CDP-driven acceptance run —
  `opengrok/docs/consent-model-B5-acceptance.md` and the screenshots in
  `opengrok/docs/consent-model-evidence/` (computer tab, general tab, per-agent auto-review,
  the auto-review card raised in the live app).
- **Server half:** [`../auto-review/README.md`](../auto-review/README.md) — prompts sent from
  that app landing as real runs on this server (streams `run_01a057f0-…`, `run_01a057f5-…`,
  `run_01a057f6-…`), answers and cards streamed back to it, transcript rows in `gateway_entry`.

A booted sidebar, a sent prompt and a streamed answer are prerequisites of every one of those
flows, so 7.v and 8.v are covered by the same evidence.

Additional live check today: with the persisted gateway address corrected to `.24`, the packaged
app boots to its full UI (CDP page target + rendered transcript). The machine's DHCP address had
moved from `.21` to `.24` and the persisted setting had gone stale with it — worth remembering
when the roster "mysteriously" stops loading in server mode: check `openGrokGatewayUrl` in
`sand-data/settings.json` against the machine's current address.

## 9.v — still open

"The client mints its own connection through us" needs a sign-in run in server mode (the
credential form at `/loginDeepControl`, then `EnsureSandBox` handing back
`OG_PUBLIC_GATEWAY_URL` + the bearer). Not exercised end-to-end from the packaged app yet; the
contract is held by `scripts/slice13-seamb-smoke.sh` and the tonic round-trip test meanwhile.
The old framing ("remove `SAND_HOST_GATEWAY_URL`") is obsolete — there is nothing to remove;
the run is: switch the runtime to OpenGrok server mode, sign in, and watch the mint.
