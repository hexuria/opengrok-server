//! Schema, applied in-process under an advisory lock.
//!
//! The lock is why several replicas can boot at once without racing each other into a half-applied
//! schema: whoever gets it migrates, the rest wait and then find the work already done. Matches
//! open-ai-gateway's pattern deliberately (`RUNBOOK.md` §2) — one database server for a developer,
//! and one habit to learn across the two services.

use sqlx::PgPool;

use crate::StoreResult;

/// Chosen once, arbitrary, and must never change: a different number is a different lock, which
/// would defeat the point on the one deploy where two versions overlap.
const MIGRATION_LOCK_KEY: i64 = 0x0_6E_67_72_6F_6B; // "ngrok" in hex, the tail of opengrok

const SCHEMA: &str = r#"
-- The log. Append-only: no UPDATE or DELETE is ever issued against this table.
create table if not exists events (
    id          bigserial primary key,
    stream_id   text        not null,
    stream_seq  bigint      not null,
    event_type  text        not null,
    payload     jsonb       not null,
    occurred_at timestamptz not null default now(),
    -- The optimistic-concurrency check, enforced by the database rather than by a read-then-write
    -- in application code, which would race.
    constraint events_stream_seq_unique unique (stream_id, stream_seq)
);

create index if not exists events_stream_idx on events (stream_id, stream_seq);

-- The lookup index for "whose refresh token is this". A projection like any other: derivable by
-- replaying `events`, and maintained in the same transaction as the append. A row exists only
-- while the token it names is live, so a rotated or revoked token simply has no row.
create table if not exists session_view (
    refresh_token_hash text   primary key,
    account_id         text   not null,
    session_id         text   not null
);

create index if not exists session_view_session_idx on session_view (session_id);

-- The roster. `box_id` lives here rather than in a request, because the computer a run uses is
-- read from the coworker's own row and never from a payload (CLAUDE.md #7).
create table if not exists coworker_view (
    id            text        primary key,
    account_id    text        not null,
    name          text        not null,
    model         text        not null,
    box_id        text,
    retired       boolean     not null default false,
    updated_at_ms bigint      not null
);
-- The standing role (`server/persona.rs`), read on the run path on every turn. A column rather
-- than a key in the seam-B profile blob: the blob holds the client's decoration, and a field the
-- model reads every turn is not decoration.
alter table coworker_view add column if not exists role text;
-- Who may see and talk to a coworker: 'private' (the default, and the safe one) or 'org'.
alter table coworker_view add column if not exists visibility text not null default 'private';

-- The account's ONE computer, shared by all its agents (1 account = 1 computer). Auto-provisioned
-- on the account's first agent, torn down when its last agent is deleted. A single row per account.
create table if not exists account_computer (
    account_id    text   primary key,
    box_id        text   not null,
    kind          text   not null,
    updated_at_ms bigint not null
);

-- A computer keyed by the SCOPE that shares it: 'org' (one box for the whole org), 'account' (one
-- per member, the default), or 'bot' (a dedicated box per bot). Supersedes account_computer, which
-- is the 'account' scope; kept above for the rows already written under it.
-- The last provisioning failure for an account's computer, so a boxless account can say WHY —
-- surfaced on listOpenGrokComputers (top-level) and stamped on the account's boxless agents.
-- Cleared when a computer is provisioned. {code, message}; code is one of the seven stable codes.
create table if not exists account_computer_error (
    account_id    text   primary key,
    code          text   not null,
    message       text   not null,
    updated_at_ms bigint not null
);

create table if not exists scoped_computer (
    scope         text   not null,
    scope_id      text   not null,
    box_id        text   not null,
    kind          text   not null,
    updated_at_ms bigint not null,
    primary key (scope, scope_id)
);
-- Idle-stop bookkeeping: when the box was last used, and whether it's currently stopped (disk kept,
-- billing paused). A box idle past the threshold is stopped by the sweep and resumed on next use.
alter table scoped_computer add column if not exists last_used_at_ms bigint;
alter table scoped_computer add column if not exists stopped boolean not null default false;
-- The org the box belongs to, so the idle sweep can rebuild the right provider (ascii key) to stop
-- or resume it. Null for a Local VM (needs no key).
alter table scoped_computer add column if not exists org_id text;

-- How an org shares computers, and per-account overrides. scope 'org' with the org id is the org
-- default; scope 'account' with an account id overrides it for that member. mode is
-- 'per-org' | 'per-account' | 'per-bot'. Absent ⇒ the built-in default (per-account).
create table if not exists computer_sharing (
    scope         text   not null,
    scope_id      text   not null,
    mode          text   not null,
    updated_at_ms bigint not null,
    primary key (scope, scope_id)
);

create index if not exists coworker_view_account_idx on coworker_view (account_id, updated_at_ms desc);

-- An authentication that happened. The token is NOT here — it is in `secret_store`, encrypted, and
-- this row only says who authenticated and what it opens.
create table if not exists connection_view (
    id            text   primary key,
    connector     text   not null,
    -- 'global' | 'user' | 'bot', with owner_id null for global.
    scope         text   not null,
    owner_id      text,
    -- Shown to a person: you@work.com, an org name. Never a secret.
    label         text   not null,
    disconnected  boolean not null default false,
    updated_at_ms bigint not null
);

-- When the access token stops working. NULL means it does not expire, which is a real answer and
-- not "already expired" — see ConnectionView::is_expiring.
alter table connection_view add column if not exists expires_at_ms bigint;

create index if not exists connection_view_owner_idx on connection_view (scope, owner_id, connector);

-- Who a connection has been lent to. Its own table because a loan is a separate decision from the
-- authentication, which is the whole reason a person need not sign in once per coworker.
create table if not exists connection_loan (
    connection_id text not null,
    coworker_id   text not null,
    updated_at_ms bigint not null,
    primary key (connection_id, coworker_id)
);

create index if not exists connection_loan_coworker_idx on connection_loan (coworker_id);

-- The ciphertext, and nothing that hints at what it opens. Separate from connection_view so a
-- credential can be shredded without deleting the record that it once existed.
create table if not exists secret_store (
    id            text   primary key,
    nonce         bytea  not null,
    ciphertext    bytea  not null,
    updated_at_ms bigint not null
);

-- Who may make which coworker do what. A row here is permission; its absence is refusal, which is
-- why nothing in the schema grants by default and why `policy_for` returns an empty context rather
-- than a permissive one when it finds nothing.
create table if not exists grant_view (
    principal_id text    not null,
    coworker_id  text    not null,
    profile      jsonb   not null,
    -- Tools that need a human yes — layer 5. Defaults to "none need approval"; the NARROW default
    -- for this column is the opposite of `profile`'s, because this one restricts rather than grants.
    needs_approval jsonb not null default '"none"'::jsonb,
    revoked      boolean not null default false,
    updated_at_ms bigint not null,
    primary key (principal_id, coworker_id)
);

-- See the note on run_view.account_id: `create table if not exists` does not evolve a table.
alter table grant_view add column if not exists needs_approval jsonb not null default '"none"'::jsonb;

-- What a coworker may EVER do, whoever asks. Separate from the grant on purpose: the two combine
-- by intersection, and a single table would invite someone to write a union.
create table if not exists ceiling_view (
    coworker_id  text   primary key,
    tools        jsonb  not null,
    updated_at_ms bigint not null
);

-- What a client asking "what happened in this run" is answered from. `status = running` with no
-- process behind it is the shape a restart leaves behind, and the reason this column exists: a
-- lost run must be findable, not merely absent.
create table if not exists run_view (
    id            text        primary key,
    thread_id     text        not null,
    status        text        not null,
    event_count   bigint      not null,
    updated_at_ms bigint      not null,
    -- Whose run this is. Nullable because a run may be started without a session (the endpoint is
    -- also just a way to talk to a model), and a NULL owner is readable by NOBODY rather than by
    -- everybody — see `run_owned_by`.
    account_id    text
);

-- `create table if not exists` does NOT evolve a table that already exists, so a column added to a
-- shipped table needs its own explicit ALTER. Learned the hard way: without this the index below
-- fails with `column "account_id" does not exist` on any database created before the column was.
alter table run_view add column if not exists account_id text;

-- A lease, so a run being worked on right now is not mistaken for one abandoned by a restart.
-- While a process is running a turn it holds the lease; when the process dies the lease simply
-- expires, which is the only signal that survives a SIGKILL. Without this, one replica would
-- "recover" runs another replica is actively serving.
alter table run_view add column if not exists leased_until_ms bigint;
-- When the run began: set on the first append and never moved, so a routine's run list can say
-- when each run started without replaying it.
alter table run_view add column if not exists started_at_ms bigint;

create index if not exists run_view_lease_idx on run_view (status, leased_until_ms);

create index if not exists run_view_account_idx on run_view (account_id);

create index if not exists run_view_thread_idx on run_view (thread_id);
create index if not exists run_view_status_idx on run_view (status);

-- A projection, not a source of truth: every column here is derivable by replaying `events`.
-- Written in the same transaction as the append that causes it.
create table if not exists account_view (
    id            text        primary key,
    email         text        not null unique,
    plan          text        not null,
    trial         boolean     not null,
    updated_at_ms bigint      not null
);

-- Autonomy (slice 6). Projections; the truth is the events stream, as everywhere.
create table if not exists schedule_view (
    id            text        primary key,
    account_id    text        not null,
    coworker_id   text        not null,
    cron          text        not null,
    prompt        text        not null,
    active        boolean     not null,
    -- When this next fires, epoch ms. NULL when inactive, or when the expression has no future
    -- occurrence left. The sweep claims by this column and ADVANCES IT IN THE CLAIMING UPDATE, so
    -- a crash between claim and fire skips one occurrence rather than firing it twice.
    next_due_ms   bigint,
    updated_at_ms bigint      not null
);
create index if not exists schedule_due_idx on schedule_view (next_due_ms) where active;
create index if not exists schedule_account_idx on schedule_view (account_id);

-- Routines (P9 wired to the desktop's pane): the name the person gave it, when it was made, and
-- when it last fired. Explicit ALTERs, because `create table if not exists` does not evolve a
-- table that already exists.
alter table schedule_view add column if not exists name text not null default '';
alter table schedule_view add column if not exists created_at_ms bigint;
alter table schedule_view add column if not exists last_fired_ms bigint;

create table if not exists monitor_view (
    id            text        primary key,
    account_id    text        not null,
    coworker_id   text        not null,
    watches       text        not null,
    prompt        text        not null,
    active        boolean     not null,
    updated_at_ms bigint      not null
);
create index if not exists monitor_account_idx on monitor_view (account_id);

-- The loop guard's memory: which runs each monitor started. A monitor never matches an event
-- from a run it fired. Written in the same transaction as the Fired event.
create table if not exists monitor_firing (
    monitor_id text not null,
    run_id     text not null,
    primary key (monitor_id, run_id)
);

-- Where the monitor sweep has read to in `events`. One row; advanced under a row lock so two
-- replicas never process the same span. Seeded at the log's current end on first use — a new
-- deployment must not replay history into freshly created monitors.
create table if not exists monitor_cursor (
    id            int    primary key,
    last_event_id bigint not null
);

-- Seam A (the desktop client's gateway), slice 8. The transcript the client renders: one row per
-- durable entry, sequenced per coworker. The entry itself is stored as the client-shaped JSON —
-- this is a wire-format projection, not domain truth; the runs journal remains the truth.
create table if not exists gateway_entry (
    coworker_id text   not null,
    seq         bigint not null,
    entry       jsonb  not null,
    at_ms       bigint not null,
    primary key (coworker_id, seq)
);

-- The prompt-acceptance ledger: (account slot, clientNonce) -> what was accepted. A repeated
-- nonce with the same digest answers accepted again; a different digest is refused. This is what
-- makes the client's retry safe instead of a duplicate send.
-- Bot keys: the durable credential a client Bot presents so its runs arrive AS a coworker.
-- A credential record, not a domain event — the same bargain as secret_store: the log records
-- what coworkers did, not which tokens exist. The row is what makes revocation real; a signed
-- key whose row is revoked (or missing) is refused.
create table if not exists bot_key_view (
    jti           text    primary key,
    account_id    text    not null,
    coworker_id   text    not null,
    label         text    not null,
    revoked       boolean not null default false,
    created_at_ms bigint  not null
);
create index if not exists bot_key_account_idx on bot_key_view (account_id);

-- Gateway keys: which open-ai-gateway key belongs to which org member. The SECRET IS NOT HERE —
-- the gateway holds its hash and we show the plaintext once, exactly like a bot key. This row is
-- attribution: it is what lets the console list an org's keys without reading every key in the
-- gateway, and what tells us whose key an id is before we ask the gateway to revoke it.
-- `revoked` mirrors the gateway's own flag; the gateway remains the authority on whether a key
-- still authenticates.
create table if not exists gateway_key_view (
    key_id            text    primary key,
    org_id            text    not null,
    member_account_id text    not null,
    key_prefix        text    not null,
    label             text    not null,
    revoked           boolean not null default false,
    created_at_ms     bigint  not null
);
create index if not exists gateway_key_org_idx on gateway_key_view (org_id);
-- 17.later: the console's mint carries a nonce, so a press whose reply was lost can be repeated
-- without minting a second real key. Unique per org; NULL for rows minted before nonces.
alter table gateway_key_view add column if not exists mint_nonce text;
create unique index if not exists gateway_key_nonce_idx
    on gateway_key_view (org_id, mint_nonce) where mint_nonce is not null;

-- 16.later Part B: OAuth clients that registered against the MCP door's authorization server
-- (RFC 7591). Public clients only — no secret is stored because none is issued. The row must
-- survive a restart: Claude Code keeps its client_id and would report "incompatible auth server"
-- if it vanished.
create table if not exists oauth_client (
    client_id     text  primary key,
    client_name   text  not null,
    redirect_uris jsonb not null,
    created_at_ms bigint not null
);

-- Refresh tokens the MCP door's authorization server issued: opaque, stored HASHED (a leaked
-- table yields nothing usable), one per access key (`jti`), rotated on every use. Revoking the
-- key from the coworker's list revokes these with it.
create table if not exists oauth_refresh_token (
    token_hash    text    primary key,
    jti           text    not null,
    client_id     text    not null,
    account_id    text    not null,
    coworker_id   text    not null,
    created_at_ms bigint  not null,
    expires_at_ms bigint  not null,
    revoked       boolean not null default false,
    -- The first access key's jti of this chain of rotations. A spent token presented again
    -- means somebody else holds the chain; the whole family goes.
    family        text    not null
);
create index if not exists oauth_refresh_jti_idx on oauth_refresh_token (jti);
-- `create table if not exists` does not evolve a table that already exists (a database that ran
-- the branch before the column did): the ALTER is what makes the index below possible.
alter table oauth_refresh_token add column if not exists family text not null default '';
-- A row from before the column has no chain of its own: it IS its own chain. Left as '' every
-- legacy token across every account would be one family, and one replay would end them all.
update oauth_refresh_token set family = jti where family = '';
create index if not exists oauth_refresh_family_idx on oauth_refresh_token (family);

-- Seam B keeps profile fields our aggregate does not model (description, title, avatar shape
-- and colour). A wire-format projection like gateway_entry: the client is the only reader.
create table if not exists seamb_profile (
    coworker_id text  primary key,
    profile     jsonb not null,
    updated_at_ms bigint not null
);

-- Identity (orgs + invites + credential accounts). Projections; the events stream is the truth.
create table if not exists org_view (
    id            text  primary key,
    name          text  not null,
    admin_id      text  not null,
    domains       jsonb not null,
    updated_at_ms bigint not null
);

-- One row per invite code, so signup can find the org a code belongs to and its state without
-- replaying every org. state: open | redeemed | revoked.
create table if not exists org_invite (
    code          text  primary key,
    org_id        text  not null,
    state         text  not null,
    updated_at_ms bigint not null
);
create index if not exists org_invite_org_idx on org_invite (org_id);

-- 12.later: domain claims awaiting their DNS TXT proof (domain → token). `domains` stays the
-- list that admits signups, so nothing that reads it needs to learn about pending state.
alter table org_view add column if not exists pending_domains jsonb not null default '{}'::jsonb;

-- Credential accounts: the login lookup reads password/verified/enabled/name without replaying
-- the whole account log. account_view stays the identity-agnostic projection; this augments it.
alter table account_view add column if not exists password_hash text;
alter table account_view add column if not exists first_name text not null default '';
alter table account_view add column if not exists last_name text not null default '';
alter table account_view add column if not exists org_id text;
alter table account_view add column if not exists verified boolean not null default false;
alter table account_view add column if not exists enabled boolean not null default false;
alter table account_view add column if not exists avatar_url text;

create table if not exists gateway_nonce (
    account_slot text   not null,
    nonce        text   not null,
    digest       text   not null,
    record       jsonb  not null,
    at_ms        bigint not null,
    primary key (account_slot, nonce)
);

-- Reverse-exec consent, per (account, machine). `mode` is 'never' (default, the channel off) |
-- 'ask' | 'bypass'. Absent ⇒ never — the channel does nothing until the user turns it on.
create table if not exists local_exec_policy (
    account_id    text   not null,
    machine_id    text   not null,
    mode          text   not null,
    updated_at_ms bigint not null,
    primary key (account_id, machine_id)
);

-- On-demand allow/deny rules for a machine's reverse-exec channel. `kind` is 'allow' | 'deny';
-- `pattern` is a command prefix matched on a word boundary. Deny beats allow (enforced in the gate).
create table if not exists local_exec_rule (
    account_id text   not null,
    machine_id text   not null,
    kind       text   not null,
    pattern    text   not null,
    added_at_ms bigint not null,
    primary key (account_id, machine_id, kind, pattern)
);

-- An enrolled machine's daemon. The token is NOT stored — only its `jti` (the token's id), so a
-- token can be verified as still-current and revoked without the token ever being at rest here.
-- One active daemon per (account, machine); re-enrolment replaces the jti, revoke flips `revoked`.
create table if not exists local_exec_daemon (
    account_id    text    not null,
    machine_id    text    not null,
    label         text    not null,
    jti           text    not null,
    enrolled_at_ms bigint not null,
    revoked       boolean not null default false,
    primary key (account_id, machine_id)
);

-- Auto-review policy: two tiers (global < coworker), one row per scope, every field TRI-STATE —
-- null inherits from the tier below, '' is an explicit "none" that stops inheritance. Override is
-- per field, never a merge. Precedence is decided in opengrok-server::auto_review (one place);
-- this table never pre-resolves. Design: docs/AUTO-REVIEW.md.
create table if not exists auto_review_policy (
    account_id         text    not null,
    scope_kind         text    not null,
    scope_id           text    not null,
    enabled            boolean,
    allow_instructions text,
    block_instructions text,
    updated_at_ms      bigint  not null,
    primary key (account_id, scope_kind, scope_id)
);

-- A device tier existed for one evening and was cut before any client wrote to it: "what on this
-- machine" is that machine's standing rules. A row nobody resolves is precisely the surprise a
-- policy store must not hold, so any that got in is removed here (idempotent).
delete from auto_review_policy where scope_kind = 'machine';

-- Every reverse-exec command and its outcome — the record the user can read afterward. Written at
-- enqueue (decision), updated when the daemon returns a result. `origin` names the bot, or the user.
create table if not exists local_exec_audit (
    id             text   not null primary key,
    account_id     text   not null,
    machine_id     text   not null,
    origin         text   not null,
    command        text   not null,
    decision       text   not null,
    requested_at_ms bigint not null,
    exit_code      integer,
    finished_at_ms bigint
);

create index if not exists local_exec_audit_acct_idx
    on local_exec_audit (account_id, machine_id, requested_at_ms desc);

-- The command's OUTCOME (the ShellResult oneof case: success / failure / timeout / rejected /
-- spawnError / permissionDenied), distinct from `decision` (the gate's verdict at enqueue). A
-- refusal is a case, not a non-zero exit, so the two are recorded separately.
alter table local_exec_audit add column if not exists outcome text;

-- Registered devices for the passkey step-up (reverse-exec slice 7). Each row is ONE WebAuthn
-- credential a person registered from an authenticated session; a step-up on a dangerous control
-- (enrol a machine, enable the channel, set bypass) is honored only for a credential that lives
-- here and is not revoked. The public key + sign_count are the RP's verification state; the private
-- key never leaves the authenticator. No credential material is secret enough to need the vault (a
-- public key is public), but the row is per-account and revocable — the whole point of the registry.
create table if not exists webauthn_credential (
    account_id      text   not null,
    credential_id   text   not null,   -- base64url, the authenticator's credential id
    public_key      text   not null,   -- serialized RP-side credential (webauthn-rs Passkey JSON)
    sign_count      bigint not null default 0,
    label           text   not null default '',
    created_at_ms    bigint not null,
    last_used_at_ms  bigint,
    revoked          boolean not null default false,
    primary key (account_id, credential_id)
);

create index if not exists webauthn_credential_acct_idx
    on webauthn_credential (account_id) where not revoked;

-- 16.r follow-up: every call through the MCP door, durable. A run journals its own tool calls;
-- a door call has no run (an Ask makes one — that is the card), so this is the only record that
-- a key was used to run a tool, with what, and what came of it. Arguments are stored REDACTED
-- (the judge's redaction), never raw: a shell command can carry a secret. `call_id` repeats
-- when a remembered yes is spent by a retry (one call, two rows: awaiting, then ok), hence the
-- serial key.
create table if not exists mcp_call_audit (
    id          bigserial primary key,
    account_id  text   not null,
    coworker_id text   not null,
    call_id     text   not null,
    tool        text   not null,
    arguments   jsonb  not null,
    outcome     text   not null,
    request_id  text   not null,
    at_ms       bigint not null
);
create index if not exists mcp_call_audit_coworker_idx
    on mcp_call_audit (coworker_id, at_ms desc);

-- Multi-replica: the three maps that lived in one process (`replica.rs`). Each row is taken
-- once with `delete … returning`; a TTL bounds every table. No index beyond the key: the
-- tables hold what is in flight in the last minutes, not history.
create table if not exists pending_login (
    uuid      text   primary key,
    challenge text   not null,
    email     text,
    at_ms     bigint not null
);
create table if not exists oauth_code (
    code           text   primary key,
    client_id      text   not null,
    client_name    text   not null,
    redirect_uri   text   not null,
    code_challenge text   not null,
    resource       text   not null,
    account_id     text   not null,
    coworker_id    text   not null,
    at_ms          bigint not null
);
create table if not exists mcp_allow_once (
    id          bigserial primary key,
    coworker_id text    not null,
    tool        text    not null,
    arguments   jsonb   not null,
    call_id     text    not null,
    gate        boolean not null,
    at_ms       bigint  not null
);
-- WHOSE consent this was. Without it, one member's "allow once" on a shared coworker would
-- authorise a DIFFERENT member's command — a consent record that fails open, which
-- non-negotiable 8 forbids. Nullable for rows written before sharing existed; a take matches on
-- it, so an old row can only be spent by a caller with no account, as before.
alter table mcp_allow_once add column if not exists account_id text;
create index if not exists mcp_allow_once_lookup_idx on mcp_allow_once (coworker_id, tool);

-- 18.later: a coworker's own gateway key, so its spend lands on its own cap. Attribution only —
-- the secret is sealed in secret_store under `coworker-gateway-key:{coworker_id}` and the
-- gateway keeps its hash. `quota_usd` mirrors the cap as we last set it; the gateway is the
-- authority on what is enforced.
create table if not exists coworker_gateway_key (
    coworker_id   text   primary key,
    account_id    text   not null,
    key_id        text   not null,
    key_prefix    text   not null,
    quota_usd     text,
    created_at_ms bigint not null
);
-- Spend limits as WE author them (`store/spend.rs`): three windows at three scopes. The
-- gateway keeps the ledger; the server evaluates these before each model call. Money as text
-- (up to six decimals), never a float; NULL means "this layer says nothing".
create table if not exists spend_limit (
    scope_kind    text   not null,
    scope_id      text   not null,
    five_hour_usd text,
    seven_day_usd text,
    month_usd     text,
    updated_at_ms bigint not null,
    primary key (scope_kind, scope_id)
);
-- Coworker templates (`store/templates.rs`): a coworker TYPE an org admin writes once — model
-- pin, tool ceiling, what needs a human yes, spend limits — that members hire from. What a
-- template says is COPIED to the coworker at hire (`coworker_template_use` remembers which);
-- editing a template changes no running coworker unless the admin applies it, and deleting
-- one leaves its coworkers exactly as hired.
create table if not exists coworker_template (
    id             text   primary key,
    org_id         text   not null,
    name           text   not null,
    description    text   not null default '',
    model          text,
    tool_ceiling   jsonb  not null,
    needs_approval jsonb  not null,
    five_hour_usd  text,
    seven_day_usd  text,
    month_usd      text,
    created_at_ms  bigint not null,
    updated_at_ms  bigint not null
);
create index if not exists coworker_template_org_idx on coworker_template (org_id, name);
create table if not exists coworker_template_use (
    coworker_id text   primary key,
    template_id text   not null,
    at_ms       bigint not null
);

-- A room paused on a member's card: where the round stood when a member's run suspended, so the
-- yes (or no) on the card resumes THAT member inside the room and then the rest of the round.
-- One per group: a new prompt to the room abandons an older pause (its card can still be
-- answered — the member then speaks — but the round it belonged to is not continued).
create table if not exists room_pause (
    group_id  text   primary key,
    run_id    text   not null,
    member_id text   not null,
    cursor    jsonb  not null,
    at_ms     bigint not null
);
create index if not exists room_pause_run_idx on room_pause (run_id);

-- Points limits (`docs/plan-spend-policy.md`): a member's monthly pool, set by the org admin;
-- a coworker's optional monthly cap and optional daily brake (a rolling 24 hours), set by its
-- owner. A point is one token at the gateway's reference price, so a subscription seat and an
-- API key count the same. The gateway meters; this table says what may be spent; the guard
-- refuses before each model call. NULL is "no limit here". The USD `spend_limit` table above
-- is no longer read and is dropped in a later cleanup, once points have run for a month.
create table if not exists points_limit (
    scope_kind    text   not null,
    scope_id      text   not null,
    month_points  bigint,
    day_points    bigint,
    set_by        text   not null,
    updated_at_ms bigint not null,
    primary key (scope_kind, scope_id)
);
-- A retired coworker's key row stays, marked: its month's points still count toward its
-- owner's pool, so retire-and-rehire does not reset a member's month.
alter table coworker_gateway_key add column if not exists revoked_at_ms bigint;
-- A key per (coworker, MEMBER), not per coworker. A shared coworker is talked to by people who
-- do not own it, and one key would bill every one of those turns to the hirer and count them
-- against the hirer's pool. The base table above still declares `coworker_id` alone as the
-- primary key because that is what every database created before this line has; these two
-- statements are what actually holds, on a fresh database too. Both are idempotent — do not
-- "simplify" them into an `add primary key`, which is not.
alter table coworker_gateway_key drop constraint if exists coworker_gateway_key_pkey;
create unique index if not exists coworker_gateway_key_pair
    on coworker_gateway_key (coworker_id, account_id);
-- Where this row's secret is sealed. The vault binds the secret id into the ciphertext as AAD
-- (`store/vault.rs`), so a secret CANNOT be moved to a new id by renaming the row — it would
-- stop opening. Rows written before the pair existed keep their secret at the per-coworker id
-- and say so here. This is a marker, not a guess: without it a member whose secret was missing
-- would fall back to the per-coworker id and quietly send somebody else's credential.
alter table coworker_gateway_key add column if not exists secret_scoped boolean not null default false;
-- Templates carry points, not USD windows; the USD columns stay unread until the cleanup.
alter table coworker_template add column if not exists month_points bigint;
alter table coworker_template add column if not exists day_points bigint;
-- The standing role a coworker hired from this template starts with (`server/persona.rs`).
alter table coworker_template add column if not exists role text;
create index if not exists coworker_template_use_template_idx
    on coworker_template_use (template_id);
-- Groups (`plan-rooms.md` §2): a coworker with members. The roster's isGroup/memberIds.
alter table coworker_view add column if not exists members jsonb not null default '[]'::jsonb;
"#;

/// Apply the schema. Safe to call on every boot and from every replica.
pub async fn run(pool: &PgPool) -> StoreResult<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let applied = sqlx::raw_sql(SCHEMA).execute(&mut *conn).await;

    // Release even when the migration failed, or the next boot deadlocks against our own lock.
    let released = sqlx::query("select pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await;

    applied?;
    released?;
    Ok(())
}
