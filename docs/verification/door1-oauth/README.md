# Door 1 — OAuth: Claude Code signs in from the browser and the door accepts its key

Verified 2 Sep 2026 against the dev server on `127.0.0.1:1447`, reached over TLS at
`https://192.168.100.24:1447` through Caddy (`docs/setup/tls.md`), built from main through #30.
The client is the real Claude Code CLI, registered with

```
claude mcp add --scope user --transport http opengrok https://192.168.100.24:1447/mcp
claude mcp login opengrok
```

Claude Code is a Node process, so it needs the local CA before either command:
`NODE_EXTRA_CA_CERTS=~/opengrok/caddy-root.crt`. Without it the metadata fetch fails with
`UNABLE_TO_GET_ISSUER_CERT_LOCALLY` and the flow never starts.

`--scope user` matters: added without it the server lands in the project entry for the directory
it was run from, and `claude mcp login` from anywhere else answers "No MCP server named opengrok".

## What was proven, with the rows to show for it

**1. The unauthenticated door challenges, and names where to go.** `POST /mcp` with no bearer:

```
HTTP/2 401
www-authenticate: Bearer resource_metadata="https://192.168.100.24:1447/.well-known/oauth-protected-resource/mcp", scope="mcp:tools"
```

**2. Both metadata documents answer.** The protected-resource document names the resource
`https://192.168.100.24:1447/mcp`, its authorization server, `scopes_supported: ["mcp:tools"]`
and `resource_name: "Open Grok MCP door"`. The authorization-server document names the issuer,
the three endpoints under `/oauth/mcp/`, and `code_challenge_methods_supported: ["S256"]`.

**3. The client registered itself.** `POST /oauth/mcp/register` → 201, client id
`mc_01a06057f7bc7590908048bbf357230a`, request id `a4cdbbda-90ef-4874-8dd4-61fa07ac7dec`.
No credential of any kind was configured by hand.

**4. A person signed in and chose a coworker.** The browser opened the server's own sign-in page;
the operator signed in as `signin@acme.test` and answered the consent card. The card offers that
account's own coworkers and nothing else.

```
mcp oauth: consent given client_id=mc_01a06057f7bc7590908048bbf357230a
    account=acct_01a0551e-29ad-74b3-b1d6-236a8122d6d8 coworker=cw_01a058dd-0052-7db2-a116-4c62276a9113
```

**5. The token is an ordinary bot key.** `POST /oauth/mcp/token` → 200:

```
mcp oauth: key issued client_id=mc_01a06057f7bc7590908048bbf357230a
    coworker=cw_01a058dd-0052-7db2-a116-4c62276a9113 jti=bk_01a06058-ee12-7ad0-97d3-2763a9c65da0
```

and it appears on that coworker's key list, revocable like any other:

```json
{ "jti": "bk_01a06058-ee12-7ad0-97d3-2763a9c65da0",
  "coworker_id": "cw_01a058dd-0052-7db2-a116-4c62276a9113",
  "label": "Claude Code (opengrok) via OAuth", "revoked": false,
  "created_at_ms": 1788322835986 }
```

**6. The door accepts it.** Two later `POST /mcp` requests carried a 388-character bearer and
answered 200 (request ids `c54fda01-…` and `f077c79d-…`), and `claude mcp list` reports
`opengrok: https://192.168.100.24:1447/mcp (HTTP) - ✔ Connected`.

`server-log.txt` in this directory is the server's own account of the whole flow, 17 lines from
the challenge to the calls, taken from `/tmp/opengrok-serve.log`.

## What this run does NOT show

**No audited `tools/call` row.** `mcp_call_audit` records tool calls, and the two successful
requests above are the protocol handshake, so the table is empty for this run. Producing a row
needs a Claude Code session that has the door's tools loaded — one started after the login. The
capturing session predated the registration, and the token lives in the operator's keychain
rather than anywhere a script can read it, which is the correct place for it. Add the row here
when someone makes a call.

## Two things that surprised the capture

- **Dev sign-in is loopback-only.** `/auth/cursor_dev_session_token` refuses over the LAN
  address with "dev sign-in is loopback-only; use the browser login". Reading the coworker's key
  list needs `http://127.0.0.1:1447`. From another machine, browser sign-in is the only path.
- **Caddy's root is not where the docs guessed.** On this Mac Caddy keeps its data under
  `~/.local/share/caddy`, not `~/Library/Application Support/Caddy`. Fetching the root from the
  admin API is the reliable way: `curl -s localhost:2019/pki/ca/local | jq -r .root_certificate`.
