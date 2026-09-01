# Request ids and /events open/close — captured 2 Sep 2026

Throwaway server, mock door, RUST_LOG=info, port 1490. Two /health calls (one with
`X-Request-Id: desk-0x1f`, one without) and one 2-second /events connect with `desk-sse-1`.
ANSI colour stripped; nothing else edited.

```
$ curl -D - /health -H 'x-request-id: desk-0x1f'   → x-request-id: desk-0x1f
$ curl -D - /health                                 → x-request-id: e1f94f05-4d70-4dbc-8de2-b3fdb06b8c25

INFO opengrok_server: request id=ab1c1a5c-937c-4433-9398-d006cd2a7117 method=GET uri=/health status=200 origin=false auth_len=9 ms=3
INFO opengrok_server: request id=desk-0x1f method=GET uri=/health status=200 origin=false auth_len=9 ms=3
INFO opengrok_server: request id=e1f94f05-4d70-4dbc-8de2-b3fdb06b8c25 method=GET uri=/health status=200 origin=false auth_len=9 ms=2
INFO http{id=desk-sse-1}: opengrok_server::gateway::routes: events: stream opened id=desk-sse-1 channels=agents subscribers=1
INFO opengrok_server: request id=desk-sse-1 method=GET uri=/events?channels=agents status=200 origin=false auth_len=9 ms=10
INFO opengrok_server::gateway::routes: events: stream closed id=desk-sse-1 channels=agents subscribers=0 open_secs=2
```
