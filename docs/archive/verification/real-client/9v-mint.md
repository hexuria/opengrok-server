# 9.v — the packaged client minted its own connection

Verified 1 Sep 2026. Throwaway Open Grok.app profile (`--user-data-dir=/tmp/og-9v`,
CDP `:9333`). Live server pid 93629, `OG_TRACE_REQUESTS=1`,
`OG_PUBLIC_GATEWAY_URL=http://192.168.100.24:1447`. A fresh account
`9v-1788256936@acme.test` / `acct_01a05c6b-647a-7050-a9ef-0aeac1608105` — not
`signin@acme.test`, so the live desktop session and `OG_GATEWAY_EMAIL` were left alone.

## The path the client actually ran

`window.desktop.agent.signInToOpenGrokServer("http://192.168.100.24:1447")`
(`opengrok-signin.ts`: PKCE → `GET /loginDeepControl` via `shell.openExternal` →
`GET /auth/poll` until 200 → `POST …/EnsureSandBox`).

TRACE, same uuid `c56ef824-b018-4890-8cf8-75ab533663f9`:

```
GET  /loginDeepControl?challenge=…&uuid=c56ef824-…&mode=login&redirectTarget=sand  200
GET  /auth/poll?uuid=c56ef824-…&verifier=…                                        404  (pending)
POST /loginDeepControl   credentials bound email=9v-1788256936@acme.test           200
GET  /auth/poll?uuid=c56ef824-…&verifier=…                                        200
POST /aiserver.v1.GrokBotService/EnsureSandBox   auth_len=311                      200
```

Poll treated 404 as "not yet" and completed only after the form bound. EnsureSandBox
ran on the account JWT (311-byte Authorization), not a bot key.

## What the RPC handed back (tokens not logged)

```
gatewayUrl = http://192.168.100.24:1447    # equals OG_PUBLIC_GATEWAY_URL
signedIn   = true
email      = 9v-1788256936@acme.test
accountId  = acct_01a05c6b-647a-7050-a9ef-0aeac1608105
```

`status.ok` was still false (`Not connected yet`) at return — the coordinator restart
is async. The mint and the persisted settings are the proof, not that status field.

`/tmp/og-9v/sand-data/settings.json` after the RPC:

```
boxRuntime:         opengrok
openGrokGatewayUrl: http://192.168.100.24:1447
```

The gateway bearer is in the secret store, not in this file, and was not printed.
