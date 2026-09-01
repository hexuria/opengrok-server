# Slice 16.later — a PolicyApproval card, and OAuth 2.1 on the MCP door

Status: **Part A implemented (ROADMAP 16.policy); Part B not started.** Written 2 Sep 2026 from
the code and the specs cited below; peer-reviewed the same day against the recovered client
bundle (findings folded in, marked *review*). Part B waits on TLS in front of the dev server
(`setup/tls.md`, the operator's install) and takes the token endpoint under `/oauth/mcp/*` (§2.2).
The consent page offers the signed-in account's own coworkers (open question 2, decided).

**The ask** (`ROADMAP.md` 16.later): OAuth 2.1 metadata on `/mcp`; a transcribed desktop card
for PolicyApproval (AutoReview Ask already has one, 16.cards); reverse-exec stays excluded.

---

## 0. What is true today

**PolicyApproval is a stuck run, not a missing nicety.** A coworker's policy grant can mark a
tool `needs_approval` (`opengrok-policy` `Decision::NeedsApproval`). The executor turns that into
`Gate::Ask(AwaitingReason::PolicyApproval)` (`opengrok-tools/src/lib.rs:489`), the run suspends
with `SuspendReason::PolicyApproval`, and then:

- `card_for` in `gateway/conversation.rs:720` returns `None` for that reason — no card is
  appended, the agent is paused, and nothing is ever answerable. Recovery skips awaiting runs.
- `resolveAutoReviewApproval` (`conversation.rs:1406`) only matches a pending suspension whose
  reason is `AutoReview`, so even a hand-crafted answer would not find it.
- On the MCP door (`mcp_door.rs:518`) the same reason fails closed with a message and no card.

So today a `needs_approval` grant behaves like a deny that also leaves a suspended run behind.
The resume machinery for a *gate* yes already exists and is reason-aware: `conversation.rs:1584`
and `agui/routes.rs:1539` route any non-AutoReview reason to `gate_yes`. Only the card and the
resolve gate are missing.

**The MCP door authenticates with a bot key and nothing else.** `guard` (`mcp_door.rs:108`)
resolves the bearer with `principal_from_bearer`; the bearer must be a `BotKeyClaims` token
(`use=bot-key`, `sub`, `coworker`, `jti`, `exp` ten years; the row is the lifecycle, revoke is
`DELETE /coworkers/{id}/keys/{jti}`). A refusal is `401` with `WWW-Authenticate: Bearer` and no
`resource_metadata`. Claude Code is wired with a static header
(`docs/verification/door1/README.md:60`: `claude mcp add --transport http … --header
"Authorization: Bearer <bot key>"`).

**The client's card inventory is closed.** `TRANSCRIPT_CARD_ENTRY_TYPES` lists twelve types and
the projector rejects the whole entry on an unknown one (`research/client-grok-bot.md` §3.2).
The `auto-review-approval` card carries `approval.requestId`, `summary`,
`status ∈ {pending, approved, always, denied, expired}`, and optional `surface`, `reason`,
`command`, `proposedRule`. It is answered with `resolveAutoReviewApproval`, and on the wire the
resolution is `approved | denied` only — the client's `AutoReviewTransportResolution` excludes
`always` (`frontend/src/recovered/features/conversation/cards/transcript-card/auto-review-actions.ts:9`);
"Always" sends `approved`, and writes a client-side auto-review instruction only when the card
carried a `proposedRule` (`auto-review-actions.ts:149-150`).

---

## 1. Part A — the PolicyApproval card (first: smaller, and it fixes the stuck run)

**Decision: reuse the `auto-review-approval` card verbatim for a policy approval.** It is the
client's own shape (non-negotiable #1), it is the one consent surface (`AUTO-REVIEW.md` §0), it is
answerable from any device, and its optional fields already say what a policy ask needs to say:

| field | policy ask value |
|---|---|
| `requestId` | the tool call id, as for every card |
| `summary` | `cards::summary_for(tool, args)` |
| `surface` | `cards::surface_for(tool)` |
| `command` | `cards::command_for(tool, args)` when the tool has one |
| `reason` | the grant's reason (`Decision::reason()`, "a human yes" by default) — the one field that differs |
| `proposedRule` | **omitted**. A policy grant is widened in policy, never from a card. |

The server tells the two asks apart by the run's `SuspendReason`, not by the card.

*Review, confirmed against the recovered client:* the card view renders `approval.reason` as a
paragraph under the summary and shows its three buttons unconditionally
(`frontend/src/recovered/features/conversation/cards/transcript-card/views/auto-review-approval.tsx:117-124`).
With `proposedRule` absent, "Always allow" is a plain approve: `auto-review-actions.ts:149-150`
returns `{transport: "approved", settled: "approved"}` and writes **no** instruction — the "rule
added" note exists only for a settled `always`, which cannot happen without a rule. So omitting
`proposedRule` makes the card behave as allow-once with nothing hidden and nothing written.

Changes, in order:

1. `gateway/cards.rs`: `auto_review_card` gains the reason text as a parameter it already takes
   (`Some(REVIEW_ASK_REASON)` today) — no new card builder. `card_for` grows a
   `SuspendReason::PolicyApproval` arm that calls it with the grant's reason and no proposed rule.
   `Suspension` (`conversation.rs:676`) carries only the reason *enum* today, not the grant's
   text (*review*); the awaiting `ToolResult` has the text, so thread it through the
   `run-awaiting-approval` event payload into the suspension.
2. `resolve_auto_review_approval`: match `pending.reason ∈ {AutoReview, PolicyApproval}`. The
   resume already maps PolicyApproval to `gate_yes` for that one call id. `approved` releases the
   gate for that call only; `denied` finishes the run with the tool result refused naming the
   rule; neither writes a standing rule — the policy stays what the admin set. Settled card
   status is `approved` / `denied`, never `always`.
3. `mcp_door.rs` `ask`: the PolicyApproval arm synthesises the same run + card the AutoReview arm
   does (`raise_card` generalised by reason). The remembered allow-once for the client's retry
   must be recorded as a *gate* approval, so `run_one` passes it as `gate_yes`, not `review_yes` —
   check the pending-approval table in `mcp_door.rs:220` carries the reason.
4. The module docs and the 16.cards line stop saying "PolicyApproval has no card".

Tests:

- unit: `card_for(PolicyApproval)` produces an `auto-review-approval` entry with the grant's
  reason and no `proposedRule`.
- Postgres: a coworker with a `needs_approval` grant → run suspends → card in the transcript →
  `resolveAutoReviewApproval approved` → the tool runs on the mock-tools door; `denied` → refused
  result names the rule; a second answer says `alreadyAnswered`.
- MCP door: `tests/against_the_mcp_door.rs:541` already builds a PolicyApproval awaiting result;
  extend it to card raised → approve → retry executes.
- smoke: extend `slice8-approval-smoke.sh` (or `slice20-mcp-door-smoke.sh`) with the policy ask.

Evidence to file: the packaged Open Grok.app renders the card with the reason text and its
buttons, and answering it resumes the run — screenshot + the client file path for how the card's
buttons are chosen when `proposedRule` is absent (`docs/verification/policy-card/`).

No wrinkle on "Always": see the review note above — without a proposed rule it is an approve
and writes nothing on either side.

---

## 2. Part B — OAuth 2.1 on `/mcp`

**Scope decision: metadata alone is worse than nothing.** An OAuth-capable client that finds
`resource_metadata` on a 401 starts the flow; if no authorization server answers, it fails where
the static header used to work. Three options:

1. keep the static header only (works today; the person mints a bot key with curl);
2. embed a minimal OAuth 2.1 authorization server in opengrok-server whose access token **is a
   bot key** — the flow becomes "mint a bot key from the browser";
3. point `resource_metadata` at an external authorization server.

**Recommendation: 2.** The pieces exist: accounts, a browser credential page in the `pages`
shell, the token minter, bot keys with a revocation list. Option 3 would need an identity we do
not have (17.later's SSO). The user-visible result is `claude mcp add --transport http opengrok
<url>/mcp` with no header, then `/mcp` → browser → sign in → pick a coworker → done; revoke from
the coworker's key list as today.

### 2.1 What the specs require (cited, 2 Sep 2026)

MCP authorization, protocol revision 2026-07-28
(`https://modelcontextprotocol.io/specification/latest/basic/authorization`):

- MCP servers **MUST** implement OAuth 2.0 Protected Resource Metadata (RFC 9728); clients
  discover the authorization server from it. The 401 carries
  `WWW-Authenticate: Bearer resource_metadata="…/.well-known/oauth-protected-resource…", scope="…"`
  (scope **SHOULD**).
- The authorization server **MUST** serve RFC 8414 metadata or OpenID Discovery; clients support
  both.
- Client registration: servers **SHOULD** support Client ID Metadata Documents; Dynamic Client
  Registration (RFC 7591) is **MAY** and "deprecated, retained for backwards compatibility".
- PKCE is OAuth 2.1 (S256). Clients **MUST** send `resource` (RFC 8707) in both requests; the
  server **MUST** validate the token was issued for it as audience.
- `iss` in the authorization response **SHOULD** be included (RFC 9207) and, if so, advertised
  with `authorization_response_iss_parameter_supported: true`.
- Invalid or expired tokens **MUST** get 401; insufficient scope 403.

Claude Code (`https://code.claude.com/docs/en/mcp`): Dynamic Client Registration is the
**default**; without it the server needs pre-configured `--client-id`. The callback is
`http://localhost:PORT/callback` on a random port (fixable with `--callback-port`). Tokens are
stored and refreshed automatically; on a 401 it refreshes once and retries. If `headers.Authorization`
is configured, OAuth is not used — so the static-header path keeps working unchanged.

Consequence: we implement **DCR** (that is what Claude Code speaks) and note CIMD as the
follow-up the spec prefers.

### 2.2 Endpoints

| route | serves |
|---|---|
| `GET /.well-known/oauth-protected-resource/mcp` **and** the root form `GET /.well-known/oauth-protected-resource` (*review*: clients probe both) | RFC 9728: `resource` = `<OG_PUBLIC_GATEWAY_URL>/mcp`, `authorization_servers: [<OG_PUBLIC_GATEWAY_URL>]`, `bearer_methods_supported: ["header"]`, `scopes_supported: ["mcp:tools"]` |
| `GET /.well-known/oauth-authorization-server` | RFC 8414: `issuer`, `authorization_endpoint`, `token_endpoint`, `registration_endpoint`, `response_types_supported: ["code"]`, `grant_types_supported: ["authorization_code"]`, `code_challenge_methods_supported: ["S256"]`, `token_endpoint_auth_methods_supported: ["none"]`, `scopes_supported`, `authorization_response_iss_parameter_supported: true` |
| `POST /oauth/mcp/register` | RFC 7591, public clients only: stores `client_id` (`mc_…`), `client_name`, `redirect_uris`; no secret. Redirect URIs accepted: `http://localhost:*/callback`, `http://127.0.0.1:*/callback`, any `https://` — nothing else. |
| `GET /oauth/mcp/authorize` | the consent page in the `pages` shell. Validates `client_id`, exact `redirect_uri`, `code_challenge` + `S256`, `resource` == our canonical `/mcp`, `state`. If the browser has no `og_access` cookie, the same card shows the credential form (the `loginDeepControl` form, same gates: verified + enabled). Then a coworker picker over the signed-in account's own coworkers (UseCoworker layer) and "Allow *client_name* to use *coworker*'s tools". |
| `POST /oauth/mcp/authorize` | consent submit (form carries the pending request id): mints a bot key for (account, coworker) labelled with the client name, issues a one-shot code (`ac_…`, 10 min, bound to client_id, redirect_uri, challenge, resource, jti), 302 to `redirect_uri?code&state&iss`. |
| `POST /oauth/mcp/token` | `grant_type=authorization_code` (form-encoded, per OAuth 2.1): verify code (unused, unexpired, same client/redirect/resource), PKCE verifier, then answer `{access_token: <the bot key JWT>, token_type: "Bearer", expires_in, scope}`. **Not `/oauth/token`** (*review*): that path is the desktop client's refresh — `electron-main/account/cursor-auth.ts:450` POSTs `{client_id, grant_type: "refresh_token", refresh_token}` as JSON — and dispatching on `grant_type` would couple two unrelated contracts on one path and break the moment either side adds a grant. RFC 8414 lets the metadata name any `token_endpoint` and Claude Code follows it, so the AS lives under `/oauth/mcp/*` and `/oauth/token` stays byte-for-byte as it is (locked by a test). |
| `/mcp` guard | **Every** unauthenticated `/mcp` response — the initial `POST initialize` included, not only a bad token — carries `WWW-Authenticate: Bearer resource_metadata="…", scope="mcp:tools"` (*review*). Token audience: OAuth-minted bot keys carry `aud = <public_url>/mcp` and the guard checks it; hand-minted keys (no `aud`) stay accepted — they are ours, the header path is unchanged, and the MCP smoke uses them. |

Refresh tokens: **not in v1.** The access token is a bot key, but an OAuth-minted one gets a
**90-day** `exp` rather than the hand-minted ten years (*review*), with `expires_in` honest.
Claude Code holds no refresh token, so on the 401 it simply re-runs the browser flow — the right
behaviour for a key that leaked or a machine that changed hands. Revocation from the coworker's
key list stays the real control; "Clear authentication" forgets the key client-side. Add refresh
only if re-consent every quarter proves annoying.

### 2.3 Storage

- `oauth_client (client_id pk, client_name, redirect_uris jsonb, created_at_ms)` — a table,
  because a registration must survive a restart or Claude Code reports "incompatible auth server".
- Authorization codes: in memory with a TTL, the same way `logins` holds `loginDeepControl`
  challenges. Single replica today; a multi-replica deployment moves both to a table together.
- The DCR rate cap needs a home (*review*): the same in-memory map keyed by peer address, with a
  ceiling on registrations per hour and on total live registrations.
- Bot keys: unchanged (`insert_bot_key` with the client name as label; `aud` in the claims).

### 2.4 Security, stated up front

- PKCE S256 mandatory; `redirect_uri` exact-match against the registration; code single-use,
  ten minutes, bound to client + redirect + challenge + resource; `resource` must equal the
  canonical `/mcp` URL or the request is refused (no token for another server).
- The consent POST carries the pending request id and the cookie; the browser-Origin refusal on
  `/mcp` itself is untouched (the authorize page is a browser page; `/mcp` is not).
- DCR is unauthenticated by design: cap registrations (per-IP rate + a table size ceiling) so it
  cannot be used to fill the database.
- Reverse-exec stays excluded on the door no matter how the key was minted (#12 below).
- The token is a bot key: a leaked one widens to exactly what a leaked hand-minted key does today,
  nothing more.

### 2.5 Tests and evidence

- Hand-written HTTP test (never rmcp-to-rmcp): register → authorize with a cookie → code →
  token → `/mcp` `initialize` succeeds; PKCE mismatch, unregistered redirect, wrong `resource`,
  reused code, expired code each refused with the right OAuth error; a token minted for `/mcp`
  is refused on nothing else and a hand-minted key still opens the door.
- The desktop's `/oauth/token` body is answered byte-for-byte as before.
- The `aud` check does not break the MCP smoke (`slice20-mcp-door-smoke.sh`), which uses
  hand-minted keys (*review*).
- The door's browser-Origin refusal is re-tested after the 401 change, because browsers will
  now fetch the metadata URLs (*review*).
- Live: Claude Code `claude mcp add --transport http opengrok http://<lan>:1447/mcp` with **no
  header**, `/mcp` → browser → consent → a tool call runs on the coworker's box. Filed under
  `docs/verification/door1-oauth/` with the exact commands, the browser screens, and the server
  log lines (request ids from PR #13 make this one grep).

---

## 3. Part C — reverse-exec stays excluded

No change. `user_machine_shell` is excluded from the listing and refused on call regardless of
credential; an OAuth-minted key does not reopen the question. Restate it in the door's module
doc when Part B lands.

---

## 4. Order and size

0. **Decision, not a spike** (*review*): Claude Code's MCP docs state the authorization-server
   metadata URL "must use https://", carving out only `http://localhost:PORT/callback` for the
   redirect, and the MCP spec assumes TLS. The dev server is `http://192.168.x.x:1447`, so a
   plain-http spike is expected to fail. Part B therefore needs TLS on the dev server first — a
   self-signed CA added to the Mac keychain, or a Caddy in front of `:1447` — and the static
   header stays the documented LAN path either way. That is the operator's call, made now.
1. **Part A** — about a day: card arm, resolve gate, door arm, three tests, client evidence.
   **Ready to start.**
2. **Part B** — two to three days plus half a day of TLS setup: metadata (both forms), DCR
   with its cap, consent page, `/oauth/mcp/token`, audience, tests, live evidence.
3. Docs: tick 16.later; HANDOVER one line; `setup/environment.md` says `OG_PUBLIC_GATEWAY_URL`
   is also the OAuth issuer (no new variable); `verification/door1/README.md` gets the
   no-header command as the new default.

## 5. Not in this slice

Client ID Metadata Documents (add when Claude Code moves to them); refresh tokens; more than one
scope; per-tool scopes; a durable audit of every allowed door call (16.r's open item); hiding the
client's "Always" button on a policy card (client change).

## 6. Open questions for the operator

1. TLS on the dev server (§4 step 0): self-signed CA in the keychain, Caddy in front, or Part B
   waits. Decides whether Part B ships now.
2. Which coworkers a consent page may offer: the signed-in account's own only (proposed, and the
   reviewer's vote — an org admin gets the roster later with SSO), or an admin's whole roster.
