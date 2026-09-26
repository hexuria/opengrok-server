# Plan-mode wire capture — 1 Sep 2026

Slice 19a was going to honour the composer's plan mode on `sendPrompt`. Capture first,
because the contract is transcribed (CLAUDE.md #1). Finding: **there is no field to
transcribe.**

## What the packaged app actually POSTs

Open Grok.app (`/Applications/Open Grok.app`), renderer bundle
`app.asar` → `dist/renderer/assets/index-UbX-y3il.js`.

The composer send (offset ~5679995) builds:

```
sendPrompt({
  agentId,
  prompt,
  richText?,          // omitted when empty
  attachments: [{path, name}],
  replyToId?,
  isFork?,
  consumedDraft?,
  trace: { traceparent?, enterEpochMs },
  onJournaled,        // renderer callback; not a wire field
})
```

A second call site (~5845618, launcher/create) is the same shape minus richText/reply.
Neither object includes `mode`. A window of 400+800 characters around every
`sendPrompt` occurrence contains no `mode` token.

`host-gateway-api.ts:214-228` (recovered source, the *host* gateway, not OpenGrok
server mode) also forwards a fixed list with no `mode`. In OpenGrok server mode the
coordinator JSON-stringifies the renderer args onto `POST /sendPrompt`
(`node-agent-coordinator/gateway/gateway-client.ts:376`).

## The picker is not in this app

The same bundle has **zero** occurrences of `Plan mode` / `plan mode`. `AgentMode`
appears only as protobuf leftover (`agent.v1.AgentMode` enum). Plan mode in Cursor's
local agent loop (`packages/agent/mode-processing.ts`) is not a surface the packaged
Grok Bot exposes, and OpenGrok server mode bypasses that loop anyway.

## What this means

Honouring a `mode` field on our server would be inventing a contract the client does
not speak. A `/plan` prompt prefix would be the same invention. The next honest step
is a *client* change (composer control + field on `sendPrompt`), then a server that
transcribes that field — two repos, and not this capture.
