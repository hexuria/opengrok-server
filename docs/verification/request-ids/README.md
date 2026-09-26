# Request ids — captured 2 Sep 2026

Throwaway server, mock door, RUST_LOG=info, port 1490. Two /health calls (one with
`X-Request-Id: desk-0x1f`, one without). ANSI colour stripped; the `/events` lines of the original
capture are cut, because that stream was deleted with seam A (P0-E, 20 Sep 2026). The request-id
middleware is unchanged (`crates/opengrok-server/src/lib.rs`).

```
$ curl -D - /health -H 'x-request-id: desk-0x1f'   → x-request-id: desk-0x1f
$ curl -D - /health                                 → x-request-id: e1f94f05-4d70-4dbc-8de2-b3fdb06b8c25

INFO opengrok_server: request id=ab1c1a5c-937c-4433-9398-d006cd2a7117 method=GET uri=/health status=200 origin=false auth_len=9 ms=3
INFO opengrok_server: request id=desk-0x1f method=GET uri=/health status=200 origin=false auth_len=9 ms=3
INFO opengrok_server: request id=e1f94f05-4d70-4dbc-8de2-b3fdb06b8c25 method=GET uri=/health status=200 origin=false auth_len=9 ms=2
```
