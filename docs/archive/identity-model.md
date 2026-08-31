# Identity model — LOCKED 30 Aug 2026

> **Archived 1 Sep 2026 — executed.** Shipped as ROADMAP slice 12 (`796bf61`); the deferred items live in ROADMAP 12.later.

The desktop integration surfaced a need the port ladder never had: real accounts. This is the
consolidated model from everything Uriah said this session, with the conflicts and blockers named
rather than guessed. **No code until this is locked** — the model changed three times in one turn,
so building now would build the wrong thing.

## What is settled (Uriah's words, reconciled)

- **Multi-tenant by organization.** Each org has **one main admin**. Accounts belong to an org.
- **Invitation, not open signup.** A stranger's `@gmail.com` cannot create an account. The admin
  issues **invite codes**; a user signs up under their org by redeeming a code.
- **Email verification IS in — conditionally.** If a Resend (resend.com) API key is configured,
  signup sends a verification email and the address must be verified before login. If no key is
  configured, verification is skipped (accounts active immediately). This is Uriah's own
  "if we have set resend api ... if not skip it."
- **Password reset — same condition.** With Resend configured, a forgot-password flow; without it,
  operator-only (CLI). Not v1-critical.
- **Real name at signup** (firstName/lastName) so the Account card shows a person, not "host".
- **Enablement step.** An account can be created-but-not-yet-usable; an admin **enables** it before
  it can log in. Login must refuse a not-enabled or not-verified account with a *distinguishable*
  error, so the client says "not enabled yet" / "verify your email" rather than "wrong password".
- **No user limit** for now.
- **The login FLOW is unchanged** — the PKCE `/loginDeepControl` + `/auth/poll` leg built in 9.1b
  stays. What changes: `/loginDeepControl` presents an email+password form and authenticates before
  binding the uuid, instead of treating the opener as the host. Poll's contract does not change;
  only *which account* the uuid binds to.
- **Testability:** a way to create accounts under a different name/email — today everything is the
  single `host@opengrok.local`, which makes multi-account untestable. This falls out of signup.

## Proposed aggregates (grounded in the existing event-sourced store)

- **`org`**: id, name, main-admin account id, its members. Event-sourced like every aggregate;
  projection `org_view`.
- **`invite`**: a code belonging to an org, its state (issued / redeemed / revoked). Redeemed at
  signup, binds the new account to the org.
- **`account`** (extends today's aggregate, which already keys on email): + password hash (argon2),
  first/last name, `verified` (driven by the Resend round-trip), and a lifecycle state
  (invited → pending-verification → enabled → disabled). Login checks all three: password,
  verified, enabled.
- Password hashing: **argon2** (new workspace dep; the existing crypto is ChaCha20/SHA-256, neither
  is a password KDF).

## The two blockers — LOCKED by Uriah

1. **Bootstrap: a CLI command.** `opengrok admin org create --email you@org.com --name Org
   --domain org.com` mints the first org and its main admin from the terminal, mirroring the oag
   gateway's key-mint. The admin account is created enabled + verified. No web surface for it.
2. **Eligibility: invite code AND org domain.** Signup requires a valid invite code belonging to an
   org AND an email whose domain matches that org's registered domain. Two gates. (An org's domain
   is asserted at `org create` for v1; proving ownership is a later slice.)

## Defaults I will take unless told otherwise (stated, not assumed silently)

- Invite codes: **single-use, no expiry** for v1 (simplest honest thing; revocable by the admin).
- Resend: gated behind `OG_RESEND_API_KEY`; unset ⇒ verification auto-passes (Uriah's conditional).
  I can build and unit-test the verification path with a stand-in mailer, but the *real* email send
  needs the key — an operator dependency, like the OAuth apps and the luna credits.
- **Domain MATCHING at signup is required now** — the email's domain must equal one of the org's
  registered domains. What is deferred is domain **OWNERSHIP** proof (a DNS challenge that the org
  actually controls the domain); v1 takes the admin's word at `org create`. These are different:
  matching is in v1, ownership is a later slice.
- **Testability under code+domain:** a throwaway `@gmail` cannot pass signup, so the CLI also mints
  accounts directly — `opengrok admin account create --org --email --name` creates an enabled,
  verified account bypassing the invite+domain gate (admin power, already how the admin itself is
  made). This is how Uriah makes a second identity to prove multi-account works.

## Suggested slicing (so this ships one tested piece at a time, per the standing rule)

- **A — accounts + passwords + signup-by-invite + login refusing the right way.** Unblocks the
  peer's "test under a different name/email" immediately once bootstrap exists.
- **B — orgs + invites + the admin enable step + the bootstrap path.** (A and B may merge since A
  needs an invite to redeem, which needs B's invite issuance — likely one slice with the bootstrap
  as its first move.)
- **C — Resend: verification email + address verification + password reset.**
