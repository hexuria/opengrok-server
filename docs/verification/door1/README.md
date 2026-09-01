# Door 1 — Claude Code through our gateway

Slice 15 (the model half) verified 1 Sep 2026 against open-ai-gateway on `127.0.0.1:29080`,
dev-bootstrapped (`just dev && just bootstrap`) after the 1 Sep Docker wipe. The dev provider is
the machine's own model door: an `openai` credential whose base URL is overridden to the local
opencodex proxy (`gateway.provider_base_urls.openai = http://127.0.0.1:8080/v1` — oag's
documented self-host hook), so no provider secret was invented and real models answer.

## What was proven, with the rows to show for it

**1. The Anthropic dialect streams.** `POST /v1/messages` with `stream: true` on route
`openai/gpt-5.5` returned the full Anthropic SSE choreography (`message_start` →
`content_block_delta`… → `message_stop`), text "door one is open", request id
`01a05a06-bc48-7650-9736-4df2e9e8f4b1`.

**2. The ledger recorded it** (`usage_event`, database `oag`):

```
request_id                            model_id        in    out  cost_usd   status streamed latency
01a05a07-4379-7b22-ba38-d7fae99ae9dc  openai/gpt-5.5  25199 11   0.10101600 200    t        1755ms
01a05a06-bc48-7650-9736-4df2e9e8f4b1  openai/gpt-5.5  14    19   0.00043600 200    t        695ms
01a05a05-eb0f-7093-adc3-801dd7f2cf0f  openai/gpt-5.5  0     0    0          502    t        0
```

The 502 row is the honest record of a misconfigured base URL (missing `/v1` — the openai
adapter posts `{base}/chat/completions`); it was metered as `EmptyResponse` rather than lost.

**3. Claude Code itself came through.** With nothing but the two environment variables —

```sh
ANTHROPIC_BASE_URL=http://127.0.0.1:29080 ANTHROPIC_AUTH_TOKEN=oag_live_… \
  claude -p "…" --model openai/gpt-5.5
```

— the reply streamed back ("claude code came through door one") and landed as ledger row
`01a05a07-…`: 25,199 input tokens (Claude Code's own system prompt — the real client,
unmistakably), 11 out, $0.101, status 200. Claude Code warns it doesn't recognise the model
name for context-window purposes; `modelOverrides` or `CLAUDE_CODE_MAX_CONTEXT_TOKENS` is the
polish, not a blocker.

**4. Discovery serves the Claude Code twins.** `GET /v1/models?claude_code=1` with the key
lists the ladder (`oag/auto`…`oag/frontier`), `openai/gpt-5.5`, and every id doubled under
`anthropic/` for Claude Code's cached model discovery.

## What this leans on (and what production changes)

- The dev credential chain is oag → opencodex → real upstreams. A production deployment
  replaces the base-URL override with real provider credentials
  (`oag admin account add --provider … --secret …`) — same route, same ledger, no code change.
- Budgets were not exercised here beyond existing gate coverage in the oag repo; per-key
  `quota_usd` and per-principal monthly budgets ship with the gateway.

## Slice 16 — the MCP door, validated on Claude Code itself

Built and verified 1 Sep 2026. The door is `POST /mcp` on opengrok-server: the bearer is a
slice-10 bot key naming the coworker, and every call runs through the same executor as a run.

**The full Door 1 loop, on the real client.** In one Claude Code invocation with both doors
configured — `ANTHROPIC_BASE_URL` → open-ai-gateway, plus
`claude mcp add --transport http opengrok http://192.168.100.24:<port>/mcp --header
"Authorization: Bearer <bot key>"` — the model answered through the gateway and called
`mcp__opengrok__shell`; the tool ran on the coworker's own Docker computer and Claude Code
repeated the output back:

```
door-one-complete
b70153ff2aa0        ← the hostname the tool printed IS the coworker's container id
```

`docker exec b70153ff2aa0 cat /tmp/cc-was-here` → `door-one-complete` — the marker is on the
coworker's box, nowhere else.

**Found live, fixed live:** Claude Code negotiates MCP protocol 2026-07-28 and rejected our
first `tools/list` — SEP-2549 makes `ttlMs`/`cacheScope` required, and rmcp leaves them unset.
The door now declares every listing `ttlMs: 0, cacheScope: "private"` (chosen, not defaulted:
the list is policy-filtered per key and policy is enforced on every action). `claude mcp list`
then reports the live `:1447` door **✔ Connected**.

**The rest of the acceptance is held by tests and the gate:**
- `tests/against_the_mcp_door.rs` — a HAND-WRITTEN JSON-RPC client (never rmcp-to-rmcp):
  handshake, empty toolbox for a computerless coworker, failed-closed call, person-token
  guidance, revoked-key refusal.
- `scripts/slice20-mcp-door-smoke.sh` (in the gate) — real Docker box: policy-filtered list, a
  command landing on the coworker's OWN computer with a foreign `coworkerId` argument
  overwritten (the slice-7 attack replayed through MCP), an ungranted tool refused naming the
  rule ("coworker … may never run gmail.workspace.send").

## Found while validating: the live deployment's box key is sealed under the lost KEK

On the live `:1447` server, every ascii-kind toolbox lists empty: the org's box.ascii.dev key
(`secret_store` id `org-computer:org_01a0551e-…:ascii`) was sealed under the KEK that was lost
in the 1 Sep reboot and regenerated — `open_credential` fails, so `provider_for` yields no
provider and `tools_for_coworker` yields no tools. **This affects live runs too, not just the
door.** The fix is operator action, not code: re-enter the box API key (org admin surface /
app) so it reseals under the current `OG_CREDENTIAL_KEK`, or switch the deployment's computers
to local Docker.
