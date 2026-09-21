# Site logins: the vault, the card, the rules

_21 Sep 2026. What the server keeps for a person's website logins, how a bot uses one, and what it never does._

## What a row is

`site_login` holds one row per (account, site, username): `kind` (`password`, `code`, `passkey`), `label`, `notes`, `created_at_ms`, `updated_at_ms`, `last_used_at_ms`, and for a passkey its public half (`passkey_credential_id`, `passkey_rp_id`, `passkey_user_handle`). The secrets are not on the row. They are sealed with the deployment's credential key (`OG_CREDENTIAL_KEK`) in `secret_store`, under keys that carry the account id so an account purge sweeps them:

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
- `POST /site-logins/{id}/reveal` — the password and the code seed, to the owner's own app, which asked after Touch ID on the Mac. Header bearer only: the console's cookie does not open this door. The passkey's key is never revealed; it is used on the server.
- `GET /site-logins/icon/{origin}` — the site's icon for the list, fetched by the server so the Mac does not announce the sites it has logins for. A public host only: no address, no local name, no private range once resolved, no redirect, no other site's link, a cap on the bytes, a day's cache of hits and misses.

## How a bot uses one

The bot never sees a secret. It raises a `request_user_form` card; NativeChat lists the person's rows for that site under the field; the person picks one and confirms with Touch ID; the app submits to `POST /ag-ui/user-form/submit` with `savedLogin: true` and `savedLoginId`.

- **A login:** the values carry the name and the password; the fill clicks each field at the position the bot gave and types. `sharedValues` is empty for a saved login whatever the model called its fields.
- **A code** (`challengeKind: "otp"`): the app mints the six digits on the Mac at the moment of sending (waiting for the next step when the current one is about to end) and they are typed like any secret field.
- **A passkey** (`challengeKind: "passkey"`, no fields): the server opens the row's key, holds the box's Chromium on its DevTools pipe (`opengrok_box::devtools`, started by the box's `box-chromium-pipe`), attaches the page, adds a platform-shaped virtual authenticator, loads the key, and tells the bot to click the site's passkey button; the key is removed after the site's challenge is signed or after two minutes. With `passkeyMode: "register"` an empty holder is added and the key the site makes is sealed into a new passkey row the moment it appears (`WebAuthn.credentialAdded`).

The order for a passkey is fixed: Chromium answers "no credentials" at once when the site asks before the key is loaded, so the person confirms first, the key is loaded, and only then does the bot click.

## The rules

- A saved login, code or passkey lands only on a computer that is one bot's own (`BoxMode::Dedicated`) and only when that bot is the caller's and private (`coworker_is_private_and_owned_by`). A box shared by the account, a group or an org, and an org-visible bot, get 403 `shared-computer`; the card stays open. Decided at fill time from the box's scope and the bot's record.
- Every use asks Touch ID on the Mac first. The server cannot verify that; it is the app's promise, and the reason the reveal door is bearer-only.
- No secret enters a journal, a tool result, or a transcript entry. `audit_lengths` logs field ids and lengths only.
- A fill from the vault stamps `last_used_at_ms` and is not offered to the vault again (`credential.offer_save` is for typed logins).
- The DevTools pipe is the only way into the box's browser: no port is opened, no socket made, so the bot's `shell` tool cannot reach the protocol. The private key is in Chromium's memory for the window of one sign-in and nowhere else in the box.

## What is not here

- Reading the person's Passwords app or iCloud Keychain: no app can. The bridge is import, once (see NativeChat's `docs/logins.md`).
- Apple's Credential Exchange: needs the signed app with a credential-provider extension; prepared in NativeChat's `macos/CredentialExchange/`, not registered by the dev build.
- SMS and email codes: nothing to hold; the person reads them or takes over the screen.
