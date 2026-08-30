# Plan — OpenGrok Web Console (Account + Admin dashboards)

**Status:** proposed, not started. Supersedes the Aug-29 Next.js scaffold in `web/`.
**Owner:** this session (opengrok-server).
**Date:** 2026-08-30.

## 1. What we are building and why

The backends are done and tested; there is **no UI**. Two authenticated pages, dark Open Grok
brand:

- **Account** (any signed-in user): change first/last name, set/replace/clear avatar (inline
  data-URL, ≤512 KB), change password. Email is read-only — it is the identity the org and the
  invite were bound to.
- **Admin** (the org's `admin` only): list org users with state, enable/disable a user, issue an
  invite (code + paste-or-click signup link), list outstanding invites and their state.

They render entirely against endpoints that **already exist** and do not change shape
(`crates/opengrok-server/src/account_api.rs`):

| Method & path | Purpose | Reply (camelCase) |
|---|---|---|
| `GET /account` | my profile | `{id,email,firstName,lastName,avatarUrl,orgId,verified,enabled}` |
| `POST /account/profile` | update name/avatar | updated profile |
| `POST /account/password` | change password (`{currentPassword,newPassword}`) | 204 |
| `GET /admin/users` | org members | `{users:[…]}` |
| `POST /admin/users/{id}/enable` \| `/disable` | toggle a user | updated profile |
| `GET /admin/invites` | outstanding codes | `{invites:[{code,state}]}` |
| `POST /admin/invites` | issue a code | `{code,link}` |

## 2. Stack (locked with Uriah, 2026-08-30)

- **Client:** Bun + Vite + React + **TanStack Router** (routes/guards) + **TanStack Query**
  (fetch/cache). A **pure client SPA** — no SSR runtime.
- **Serving:** **Axum serves the built assets** at the same origin (e.g. mounted under `/console`).
  One deployable, one origin, **no CORS**. In dev, `vite dev` proxies API + auth routes to the
  Rust server so cookie semantics match prod.
- The existing **Next.js** `web/` (create-next-app, npm) is replaced. Two files are salvaged as
  reference for the auth model: `web/lib/opengrok.ts` (JWT claim/expiry predicates transcribed
  from the desktop client) and its httpOnly-cookie rationale (moved server-side, see §3).

## 3. The one design refinement — browser auth via httpOnly cookies (not localStorage)

**Problem.** A browser page does not send a `Bearer` header on navigation, and a pure SPA that
stores tokens in `localStorage` exposes the **refresh token to any XSS** — exactly the risk the
Aug-29 `web/lib/opengrok.ts` was written to avoid ("one XSS away from being someone else's
session"). We keep that protection.

**Decision.** Because Axum serves the SPA **same-origin**, auth rides in **httpOnly cookies set by
the server**. The SPA never touches a token; it just calls same-origin endpoints and the cookie
goes along.

- New **`POST /auth/login`** (JSON `{email,password}`): authenticates via the existing
  `credential_login_ready()` + `verify_password()` + `mint_session()`, then sets `og_access`
  (httpOnly, SameSite=Lax, ~1 h) and `og_refresh` (httpOnly, SameSite=Lax, long-lived) cookies.
  Returns `{email}` (no token in the body). This is the browser's direct login leg — distinct
  from the desktop client's PKCE `/loginDeepControl`, which stays exactly as-is.
- New **`POST /auth/logout`**: clears both cookies (exp's the account session too).
- New **`POST /auth/refresh`**: reads `og_refresh`, rotates via the existing `oauth_token` logic,
  re-sets cookies. The SPA calls it on a 401 and retries once.
- **`caller()` in `account_api.rs`** learns to read the access token from **either** the
  `Authorization: Bearer` header (desktop/API clients — unchanged) **or** the `og_access` cookie
  (browser console). One extra branch; no behaviour change for existing callers.
- **CSRF:** cookies are SameSite=Lax and every mutation is a JSON `POST` (not a simple form), so a
  cross-site page cannot forge one without a preflight the server won't satisfy. Good enough for
  v1; note it, revisit if the console ever accepts form posts.

This keeps the SPA pure, keeps tokens out of JS, and needs no CORS.

## 4. Server-side work (Rust, opengrok-server)

1. `auth/routes.rs`: add `POST /auth/login`, `POST /auth/logout`, `POST /auth/refresh` (cookie
   variants of the token flow). Set-Cookie built by hand or via `axum-extra`'s `CookieJar` (pick
   the smaller dependency delta — `axum-extra` is likely already in the tree; check first).
2. `account_api.rs`: `caller()` accepts the `og_access` cookie as an alternative token source.
3. `lib.rs`: mount `ServeDir` (from `tower-http`, already a dep via axum stack — verify) on the
   built SPA dir under `/console`, with an SPA fallback to `index.html` so client routes deep-link.
   The dir path comes from an env/const so dev vs packaged both resolve.
4. Config: a `WEB_CONSOLE_DIR` (or reuse an existing static-dir convention) pointing at the built
   assets; documented in `.env.example`.

**Rust tests (added with the code, same commit):**
- `/auth/login` with good creds sets both cookies; bad creds → 401, no cookies; unverified /
  not-enabled → distinct 403 messages (mirror `login_submit`).
- `caller()` authorises from a cookie exactly as from a Bearer header (a request with only
  `og_access` reaches `GET /account` and gets the right profile).
- `/auth/refresh` rotates and re-sets cookies; `/auth/logout` clears them.
- Reuse the existing test harness/pattern in the crate; no new framework.

## 5. Client-side work (Bun + Vite + React + TanStack)

Replace `web/` with a Vite SPA (Bun as runtime/pkg manager). Structure:

- `web/src/api/` — a tiny typed client over the JSON endpoints (fetch with `credentials:"include"`,
  a 401→`/auth/refresh`→retry-once wrapper). Port the claim/expiry helpers from the old
  `lib/opengrok.ts` where still useful (mostly the server owns this now).
- `web/src/routes/` — TanStack Router:
  - `/login` — email + password → `POST /auth/login`; on success route to `/account`.
  - `/account` — name form; avatar picker (file → data-URL preview, client-side 512 KB guard
    matching the server cap, clear button); password form. Optimistic-ish via TanStack Query
    `invalidateQueries(['account'])`.
  - `/admin` — user table (email, name, state) with Enable/Disable buttons (mutations →
    invalidate `['admin','users']`); "Issue invite" → shows code + copyable signup link; list of
    invites with state. Guarded: if `GET /admin/users` returns 403, render "admins only".
  - Root guard: unauthenticated (`GET /account` 401) → redirect to `/login`.
- **Brand:** dark theme matching `auth/pages.rs` (`#0a0a0b` ground, `#141417` cards, `#5b6cff`
  focus ring, white pill buttons, inline smiley logo). A small shared CSS token set; Tailwind is
  fine if it stays out of the way, or plain CSS — keep it light.

**Client tests (vitest):**
- API client: 401 triggers exactly one refresh+retry; avatar guard rejects >512 KB and non-image.
- A couple of pure UI-logic units (invite-link render, enable/disable state mapping). Not
  exhaustive DOM testing — the browser pass (§6) is the real UI proof.

## 6. Browser verification — full flows + screenshots (Uriah's ask: "it lived in the browser")

Drive a **real Chrome via the Claude-in-Chrome MCP** against the running Rust server serving the
built SPA (not vite dev — verify the shipped path). Note: this tool has been **flaky under machine
load** in this project; if screenshots time out, retry when CPU is quiet and fall back to CDP
screenshot as a backup, but the flows themselves must be driven and confirmed.

Seed data via the admin CLI (`opengrok admin org create` + `account create` + `invite`) so there
is an admin and a member to act on.

Flows, screenshot each:
1. `/login` renders (dark brand) → sign in as the member → lands on `/account`.
2. Edit first/last name → save → reload shows the new name.
3. Pick an avatar image → preview appears → save → avatar persists on reload; oversize image is
   refused client-side.
4. Change password (wrong current → error; right current → success; re-login with new password).
5. Sign in as **admin** → `/admin` lists users including the member.
6. Disable the member → state flips; Enable → flips back.
7. Issue an invite → code + link shown and copyable; appears in the invite list as `open`.
8. Sign out → `/account` redirects to `/login`.

Store screenshots under `docs/verification/web-console/` (or attach in the report).

## 7. Slices (each: code + tests green + committed; verify-first before any PR)

- **Slice A — browser auth on the server.** `/auth/login|logout|refresh` cookie leg + `caller()`
  cookie support + `ServeDir` mount + Rust tests. `scripts/gate.sh` green. Commit.
- **Slice B — SPA shell + login + account page.** Scaffold, API client, `/login`, `/account`
  (name, avatar, password), served by Axum. Client tests green. Commit.
- **Slice C — admin page.** `/admin` users + invites. Client tests green. Commit.
- **Slice D — browser verification.** Full flows + screenshots (§6). Tick the roadmap
  ("Commands: `goal`/`plan`/… " area, add a **Web console** line), commit naming its verification.

## 8. Out of scope (named, so scope is honest)

- 2FA / passkeys (future), artifacts/upload store (avatar stays inline data-URL until then),
  per-org seat limits, changing one's own email, self-serve org creation (admin CLI owns
  bootstrap). `sand://` and the desktop PKCE `/loginDeepControl` are untouched.

## 9. Invariants that stay true

- No secrets committed; `.env` stays gitignored (`OG_RESEND_API_KEY` lives there).
- `unsafe_code` forbidden; no `unwrap/expect/panic` outside tests.
- Existing tests and public signatures keep working; existing smokes stay green.
- One tested slice at a time, simplest thing that works.
