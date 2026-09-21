# Site logins: the vault, the card, the rules

_21 Sep 2026. What the server keeps for a person's website logins, how a bot uses one, and what it never does._

## What a row is

`site_login` holds one row per (account, site, username, kind): `kind` is `password`, `code` or `passkey`, so a password and a passkey for one name on one site are two rows and saving one never turns the other into it. Each row carries `label`, `notes`, `created_at_ms`, `updated_at_ms`, `last_used_at_ms`, and for a passkey its public half (`passkey_credential_id`, `passkey_rp_id`, `passkey_user_handle`). The secrets are not on the row. They are sealed with the deployment's credential key (`OG_CREDENTIAL_KEK`) in `secret_store`, under keys that carry the account id so an account purge sweeps them:

| secret | key |
|---|---|
| the password | `site-login:<account>:<id>` |
| the authenticator-code seed (an `otpauth://` URI) | `site-login-otp:<account>:<id>` |
| a passkey's private key (PKCS#8, base64) | `site-login-passkey:<account>:<id>` |

## The routes

All read the account from the bearer, never from the body; another account's id is "no such row".

- `GET /site-logins` — the rows, never a secret.
- `POST /site-logins` — `{ origin, username, password?, otpauth?, label?, notes?, kind? }`. A `password` row needs a password; a `code` row needs an `otpauth://` seed. A passkey row is never posted: the site makes it (below). The origin is stored as the bare host, lowercase.
- `PATCH /site-logins/{id}` — `{ label?, notes? }`.
- `DELETE /site-logins/{id}` — the row and all three secrets.
- `POST /site-logins/{id}/reveal` — the password and the code seed, to the owner's own app, which asked after Touch ID on the Mac. The account is read from `Authorization: Bearer` and nothing else: the console's cookie does not open this door, whatever else the request carries. The passkey's key is never revealed; it is used on the server.
- `GET /site-logins/icon/{origin}` — the site's icon for the list, fetched by the server so the Mac does not announce the sites it has logins for. A public host only: no address, no local name, no private range once resolved (and the fetch goes to the vetted addresses, not a second lookup), no redirect, no other site's link, a cap on the bytes. Only a raster image is served (PNG, ICO, JPEG, GIF, WebP; never SVG), with `nosniff` and a `default-src 'none'` policy, so nothing fetched can run on this origin. The cache is per account and bounded: a day for an icon, six hours for "none", five minutes for a fetch that failed.

## How a bot uses one

The bot never sees a secret. It raises a `request_user_form` card; NativeChat lists the person's rows for that site under the field; the person picks one and confirms with Touch ID; the app submits to `POST /ag-ui/user-form/submit` with `savedLogin: true` and `savedLoginId`.

- **A login:** the values carry the name and the password; the fill clicks each field at the position the bot gave and types. `sharedValues` is empty for a saved login whatever the model called its fields.
- **A code** (`challengeKind: "otp"`): the app mints the six digits on the Mac at the moment of sending (waiting for the next step when the current one is about to end) and they are typed like any secret field.
- **A passkey** (`challengeKind: "passkey"`, no fields): the server holds the box's Chromium on its DevTools pipe (`opengrok_box::devtools`, started by the box's `box-chromium-pipe`), attaches the newest tab on the site (or opens one at its front page when none is), then opens the row's key, adds a platform-shaped virtual authenticator, loads the key, and tells the bot to click the site's passkey button; the key is removed after the site's challenge is signed or after two minutes, and the row's last use is stamped then, not at load. When the browser had to be replaced or a tab opened, the bot is told to go back to the sign-in page first. With `passkeyMode: "register"` an empty holder is added and the key the site makes is sealed into a new passkey row the moment it appears (`WebAuthn.credentialAdded`). Only a computer that offers a pipe (`Computer::offers_a_pipe`, Docker) is touched at all; any other answers that it has no pipe without killing anything.

The order for a passkey is fixed: Chromium answers "no credentials" at once when the site asks before the key is loaded, so the person confirms first, the key is loaded, and only then does the bot click.

## The rules

- A saved login, code or passkey lands only on a computer that is one bot's own (`BoxMode::Dedicated`) and only when that bot is the caller's and private (`coworker_is_private_and_owned_by`). A box shared by the account, a group or an org, and an org-visible bot, get 403 `shared-computer`; the card stays open. Decided at fill time from the box's scope and the bot's record.
- Every use asks Touch ID on the Mac first. The server cannot verify that; it is the app's promise, and the reason the reveal door is bearer-only.
- No secret enters a journal, a tool result, or a transcript entry. `audit_lengths` logs field ids and lengths only.
- A fill from the vault stamps `last_used_at_ms` and is not offered to the vault again (`credential.offer_save` is for typed logins).
- The DevTools pipe is the only way into the box's browser: no port is opened, no socket made, so the bot's `shell` tool cannot reach the protocol. The server creates its boxes with `BOX_CHROME=0`, so the box's own port-bound Chromium is never started. The private key is in Chromium's memory for the window of one sign-in and nowhere else in the box.
- A passkey row is keyed by the host it was made on; a site whose relying party spans hosts (`accounts.google.com` making a key for `google.com`) is offered on that host. Two passkeys for one name on one site are one row: the newer key replaces the older.

## What is not here

- Reading the person's Passwords app or iCloud Keychain: no app can. The bridge is import, once (see NativeChat's `docs/logins.md`).
- Apple's Credential Exchange: needs the signed app with a credential-provider extension; prepared in NativeChat's `macos/CredentialExchange/`, not registered by the dev build.
- SMS and email codes: nothing to hold; the person reads them or takes over the screen.
