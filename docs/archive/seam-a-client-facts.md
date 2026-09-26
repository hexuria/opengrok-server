# The Grok Bot client's facts, from CLAUDE.md

CLAUDE.md's "Three facts that each cost a day" were written while the Grok Bot desktop client was
the client this server served. Facts #1 and #3 are about that client and the doors built for it.
Both doors, seam A and seam B, were deleted on 20 Sep 2026 (`ROADMAP.md`, Phase 0, P0-E). The
client is discontinued. Fact #2, about the gateway, is not client-specific and stays in
CLAUDE.md.

They are recorded here word for word, as they stood on `main` at `d73042b`, because each cost a
day to learn and the reasoning can outlive the client. None of it describes NativeChat. The
loopback refusal in #1 was the Electron client's rule, and whether NativeChat has one is unknown
until its source is read (`setup/nativechat.md`). The verbs in #3 (`listAgents`, `countAgents`,
`getTrays`) now exist only as transcribed shapes in `crates/opengrok-wire`.

Moving them out of CLAUDE.md itself (issue #202) is a change to the project's agent
instructions, left for the operator to make.

## Fact #1, as written

1. **The client refuses a loopback gateway, and the env-var repoint is dead.** The desktop app
   connects through its own OpenGrok server mode (`boxRuntime: "opengrok"` + the
   `openGrokGatewayUrl` setting); launching it with `SAND_HOST_GATEWAY_URL` deadlocks it before
   the window opens. Either way it **throws if the gateway host starts with `127.0.0.1` or
   `localhost`** — serve on a non-loopback address. `docs/setup/desktop-client.md`.

## Fact #3, as written

3. **An empty success is the dangerous reply.** `listAgents` returning `[]` is *valid* — the client
   paints an empty sidebar and the person blames the app. Reply shapes matter as much as replies:
   `countAgents` must be a number, `getTrays` an array, or the renderer diverts or throws.
   And if the roster silently stops updating, check the client's `inferenceProvider` setting —
   and its persisted gateway address against the machine's current LAN address — before
   suspecting us. `docs/setup/desktop-client.md`.

## What still holds

The principle of #3 outlives its examples: **an empty success is the dangerous reply.** A route
that answers `200 []` when it should have answered something makes the client paint nothing, and
the person blames the app. For NativeChat, the shapes that matter are the ones its source reads.
`research/client-nativechat.md` is where they get recorded, each with the NativeChat file that
reads it.
