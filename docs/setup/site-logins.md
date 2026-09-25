# Site logins, passkeys, and a person's own Mac

What an operator sets up before people save website logins in NativeChat, let a bot sign in with a
passkey, or enrol their Mac for commands. How the pieces behave is in
[`docs/site-logins.md`](../site-logins.md) and `crates/opengrok-server/src/local_exec.rs`; this page
is what has to be true on the server, and what to do when it stops being true.

## 1. The credential key

Every saved password, authenticator-code seed and passkey private key is sealed in `secret_store`
under `OG_CREDENTIAL_KEK` — the same key that seals connector tokens and the org's computer keys
([`environment.md`](environment.md#the-credential-key) has the full list).

- **Set it** before anyone saves a login: `openssl rand -base64 32` into `OG_CREDENTIAL_KEK`. Without
  it, a save and a reveal answer `503` "the credential vault is not configured on this server (set
  OG_CREDENTIAL_KEK)"; nothing is stored in the clear instead.
- **Back it up** wherever the database backup goes. Lose it and every saved login and passkey is
  unrecoverable; the rows stay, and nothing opens them.
- **Never replace it** — rotate it ([`environment.md`](environment.md#the-credential-key)): the old
  value goes to `OG_CREDENTIAL_KEK_OLD`, then `opengrok vault reseal`.
- **Check it:** `opengrok vault status` (with the server's environment) exits 0 when every row opens.
  At boot the same check runs and logs an `ERROR` naming any key id the server does not hold, and
  `GET /health` carries it as `vault.ok` / `vault.reason` (the top-level `ok` is unaffected).

## 2. The NativeChat flow

1. **Save.** The person saves a password (or an `otpauth://` seed) in NativeChat's settings:
   `POST /site-logins`. The server seals the secret; the row it returns, and every list, carries no
   secret.
2. **Fill.** A bot that needs to sign in raises a `request_user_form` card. NativeChat lists the
   person's rows for that site, the person picks one and passes Touch ID on the Mac, and the app asks
   `POST /site-logins/{id}/reveal` (bearer only — the console's cookie does not open it), then
   submits with `savedLogin: true`. The bot never sees the secret; the server types it into the box.
3. **Passkeys.** Made and used on the server, in the box's browser: registering one seals the key the
   site makes into a passkey row; using one loads it into a virtual authenticator for one sign-in. A
   passkey needs a computer with a browser pipe (a Docker box with a desktop). The private key never
   leaves the server for the Mac.

Saved logins land only on a bot's own dedicated computer, for its own private bot; a shared box
answers `403 shared-computer`.

## 3. When the key is lost anyway

- **Reveal** answers `409` with "this credential was sealed with a key this server no longer has …",
  not `500 store unavailable`. A bot's passkey sign-in fails with the same sentence.
- **The list** still shows the rows: it reads no secrets. (Marking them unusable needs a field
  NativeChat reads; none exists yet.)
- **The fix**, in order of preference: find the old key, put it in `OG_CREDENTIAL_KEK_OLD`, restart,
  `opengrok vault reseal`. Otherwise each person saves the login again — a save seals what it carries under
  the current key — and a passkey is registered again on the site.

## 4. Enrolling a person's Mac (reverse-exec)

Reverse-exec runs commands on the person's **own** machine, so it is closed by default and
per-machine. It does **not** use the credential key: a machine holds a daemon token signed with
`OG_TOKEN_SECRET`, which is why changing that secret signs out every desktop **and** every enrolled
daemon.

- **Enrol:** NativeChat calls `POST /local-exec/daemon` with `{ label, machineId? }` when the person
  turns the channel on. The reply's `token` is shown once; the app hands it to the daemon. Enrolling
  again with the same `machineId` rotates the token — the old one stops working at once.
- **Revoke:** `DELETE /local-exec/daemon/{machineId}`. Sign-in is untouched. `GET /local-exec/daemon`
  lists the machines and whether each daemon is connected.
- **Modes** (`PUT /local-exec/policy` with `{ machineId, mode }`):
  - `never` — the default. The channel is off; every command is refused.
  - `ask` — a deny rule refuses, an allow rule runs it, anything else asks the person on a card.
  - `bypass` — everything runs, still audited. A deliberate, machine-wide choice.

  Rules are `POST`/`DELETE /local-exec/policy/rule` with `{ machineId, kind: "allow" | "deny",
  pattern }`; a pattern matches a whole command or its leading words, deny beats allow, and `sudo` is
  never accepted as a standing allow.
- **Who drives it:** a bot through `user_machine_shell` (the full gate, on the account's first
  enrolled, unrevoked machine whose mode is not `never`), or the person directly with `POST /local-exec/run`,
  where sending the command is the approval (`ask` is skipped; `never` and deny rules still refuse).
  It is never offered over the MCP door.
- **Audit:** every command, allowed or not, is a row; `GET /local-exec/audit` lists them.
