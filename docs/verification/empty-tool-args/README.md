# A replayed call's arguments must be an object — captured 11 Oct 2026

Two live turns ended with the same 400 after a tool that takes no arguments ran (a server-folder
plugin, `bir`):

- after `bir_list_apps`: `messages.9.content.0.tool_use.input: Input should be an object`
  (`req_011Cfv7ugHNNaMEUUvKhucRA`, run `01a12a09-c301-776e-8505-ea892d2f0321`);
- after `bir_form_context` + `bir_form_fields` in parallel: `messages.27.content.0.tool_use.input:
  Input should be an object` (`req_011CfvAC3ie4jGjYebv7prR3`, run
  `01a12a25-0a4b-7653-aa7b-a34767fbaa67`).

## The gateway, asked directly

`POST $OG_GATEWAY_URL/v1/chat/completions` (the local Merge gateway, `stream: false`,
`max_tokens: 16`), model `anthropic/claude-haiku-5-5`. Three messages: the user's words, an
assistant message with one call `{"id":"call_1","type":"function","function":{"name":"bir_list_apps",
"arguments": A}}`, and its `tool` result. One tool, `bir_list_apps`, with an empty object schema.
Only `A` changes. Captured 2026-10-11T12:46Z; bearer token not shown.

| `arguments` sent | Answer |
| --- | --- |
| `""` | 200 |
| `"   "` | 200 |
| `"not json"` | 200 |
| `"{}"` | 200 |
| `"null"` | **400** `messages.1.content.0.tool_use.input: Input should be an object` (`req_011CfvUPUgyMfQRFmKok2sUv`) |
| `"[1]"` | **400** `messages.1.content.0.tool_use.input: Input should be an object` (`req_011CfvURRBwG3Z9DLYbvrMru`) |

So this gateway repairs argument text it cannot parse, but passes any valid JSON through as
`tool_use.input`, and Anthropic refuses one that is not an object. Before the fix, a call with no
argument text was collected as `null` (`collect_tool_calls`) and written back as the text `"null"`
(`tool_call_ref`): exactly the failing row. After it, the call is `{}`, and `chat_messages` sends
anything that is not a JSON object as `{}`, so neither the gateway's repair nor its absence matters.

Full error body of the `"null"` row:

```json
{"type": "upstream_error", "message": "upstream anthropic returned 400", "upstream_status": 400,
 "upstream": {"type": "error", "error": {"type": "invalid_request_error",
 "message": "messages.1.content.0.tool_use.input: Input should be an object"},
 "request_id": "req_011CfvUPUgyMfQRFmKok2sUv"}}
```
