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

Slice 16 (the `/mcp` door on opengrok-server) is the other half of Door 1; its evidence lands
below when built.
