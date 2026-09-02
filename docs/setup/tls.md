# TLS in front of the dev server

Status: installed on the dev Mac 2 Sep 2026 (by the peer session, with the operator's ok — trusting
a certificate authority is a change to the Mac, not to this repo). The desktop app talks to the
server through it. This page describes the layout as installed; if the Caddyfile on the Mac and
this page disagree, the Mac is right and this page needs the fix.

## Why

Claude Code's MCP client runs the OAuth flow only against an `https://` authorization-server
metadata URL (`code.claude.com/docs/en/mcp`: "The URL must use https://"; only the
`http://localhost:PORT/callback` redirect is carved out), and the MCP authorization spec assumes
TLS. The dev server is plain HTTP on the LAN (`http://192.168.100.24:1447`), so OAuth on `/mcp`
cannot ship until something in front of it speaks HTTPS. The server does **not** terminate TLS
itself — it never has to know: a reverse proxy does it, and `OG_PUBLIC_GATEWAY_URL` says which
address the outside world uses.

The static-header path (`claude mcp add … --header "Authorization: Bearer <bot key>"`) keeps
working over plain HTTP either way; this is only for the OAuth flow and for any client that
insists on HTTPS.

## The shape: one port, two addresses

```
desktop app / Claude Code ──https://192.168.100.24:1447──▶ Caddy ──http://127.0.0.1:1447──▶ opengrok
```

The server binds **loopback only** (`OG_BIND=127.0.0.1:1447`); Caddy binds the **LAN address** on
the same port and speaks HTTPS. Two listeners, one port number, no clash — and nothing that used
`127.0.0.1:1447` changes: the smokes, the gate and `scripts/serve.sh` (which health-checks
loopback) are unaffected. Only the address the outside world uses gained a scheme. Caddy issues
the certificate from its own local CA (`tls internal`), and `caddy trust` installs that CA's root
into the Mac's keychain so Chromium (the desktop app, Claude Code's browser step) and `curl`
trust it.

## Install (once per machine)

```sh
brew install caddy
```

`~/opengrok/Caddyfile` (any path; `scripts/serve.sh` does not manage it):

```caddyfile
# HTTPS front for the OpenGrok dev server, on the LAN address only; the server itself listens on
# loopback at the same port. Certificate from Caddy's local CA; the CA root is trusted into the
# login keychain by `caddy trust` below. The LAN address is DHCP — when it moves, change it here
# (twice), in OG_PUBLIC_GATEWAY_URL and in the app's openGrokGatewayUrl.
{
    # No plain-http listener redirecting to https: :80 is not ours to take, and the http side of
    # this port is the server's own loopback listener.
    auto_https disable_redirects
}

https://192.168.100.24:1447 {
    bind 192.168.100.24
    tls internal
    reverse_proxy 127.0.0.1:1447 {
        # /events is a server-sent stream and /ag-ui streams too: never buffer a response.
        flush_interval -1
    }
}
```

Start it and trust the CA:

```sh
caddy run --config ~/opengrok/Caddyfile      # foreground; or `caddy start` to background it
caddy trust                                  # prompts for the Mac password: installs Caddy's local
                                             # CA root into the login keychain. One time, and PER
                                             # LOGIN KEYCHAIN: another macOS user runs it again.
```

Check from the Mac:

```sh
curl -sS https://192.168.100.24:1447/health -H "authorization: Bearer $OG_GATEWAY_BEARER"
curl -sSN --max-time 2 "https://192.168.100.24:1447/events?channels=agents" \
  -H "authorization: Bearer $OG_GATEWAY_BEARER" -H 'accept: text/event-stream' | head -1
# → retry: 1000
```

If `curl` refuses the certificate, `caddy trust` did not land in the keychain the shell uses;
`security find-certificate -c "Caddy Local Authority"` shows whether it is there.

## Server side

`.env`:

```
OG_BIND=127.0.0.1:1447
OG_PUBLIC_GATEWAY_URL=https://192.168.100.24:1447
```

`OG_BIND` moves to loopback so Caddy can take the LAN address on the same port. The public URL is
the address `EnsureSandBox` mints to clients, the base of every emailed link, and — once Part B
lands — the OAuth issuer and the `resource` a token is issued for; it must be the HTTPS address
the clients actually reach. Restart with `scripts/serve.sh`.

The smokes and the gate need nothing: they run their own server on their own port and check
`OG_PUBLIC_GATEWAY_URL` only for being non-loopback (`slice13-seamb-smoke.sh`), which an HTTPS LAN
address satisfies. Keep running the gate with a clean environment as before.

## Desktop side

Do this BEFORE any OAuth work: the app must already be on the https address, or the OAuth
issuer (`OG_PUBLIC_GATEWAY_URL`) and the address the app talks to disagree.

1. Quit the app. In the client's data root, `sand-data/settings.json`: set `openGrokGatewayUrl`
   to `https://192.168.100.24:1447` (both places the desktop-client doc names).
2. The CA root from `caddy trust` is what the app's Chromium checks; nothing else to install.
3. Relaunch `/Applications/Open Grok.app` and confirm the roster paints and `/events` opens on
   the new address — the server's request log shows every call with its `X-Request-Id` and
   `events: stream opened`.
4. Sign in through the browser. The dev sign-in shortcut
   (`GET /auth/cursor_dev_session_token`) answers only when the request's `Host` is a loopback
   address (`auth/routes.rs::is_loopback`); through the HTTPS LAN address, or from any other
   machine, it refuses with "dev sign-in is loopback-only; use the browser login". This
   surprised a capture mid-flow on 2 Sep 2026 — it is the intended posture, not a fault.

Claude Code, once Part B ships:

```sh
claude mcp add --transport http opengrok https://192.168.100.24:1447/mcp
claude mcp login opengrok        # browser: sign in, pick the coworker, done
```

## When the LAN address changes

Four places, together: the `https://…` site address and the `bind` line in the Caddyfile,
`OG_PUBLIC_GATEWAY_URL`, and the app's `openGrokGatewayUrl`. The desktop-client doc's note on the stale DHCP address
applies to the HTTPS address the same way.
