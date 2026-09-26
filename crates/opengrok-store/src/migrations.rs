//! Schema, applied in-process under an advisory lock.
//!
//! The lock is why several replicas can boot at once without racing each other into a half-applied
//! schema: whoever gets it migrates, the rest wait and find the work done. Matches open-ai-gateway
//! (`RUNBOOK.md` §2). `SCHEMA` replays in full whenever it changes, so every statement in it runs
//! again on a database that already went through it; `EVERY_BOOT` runs on every boot. Read
//! docs/setup/postgres.md, "Data-transforming migrations", before writing either.

use sqlx::PgPool;

use crate::StoreResult;

/// Chosen once, arbitrary, and must never change: a different number is a different lock, which
/// would defeat the point on the one deploy where two versions overlap.
const MIGRATION_LOCK_KEY: i64 = 0x0_6E_67_72_6F_6B; // "ngrok" in hex, the tail of opengrok

const SCHEMA: &str = r#"
-- The log. Append-only in normal operation: no UPDATE is ever issued, and the only DELETE is
-- the operator purge (`purge_accounts_except`), which removes whole streams of aggregates that
-- no longer exist.
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
-- replaying `events`, and maintained in the same transaction as the append. The current hash
-- has grace_until_ms NULL (live until rotated). The just-rotated-away hash is kept until
-- grace_until_ms so a concurrent refresh with the old cookie can still find the session;
-- after that the lookup ignores it, and the next rotate deletes it. Never a history of hashes.
create table if not exists session_view (
    refresh_token_hash text   primary key,
    account_id         text   not null,
    session_id         text   not null
);
alter table session_view add column if not exists grace_until_ms bigint;

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
-- A viewer's hide-from-sidebar preference. Not a coworker event: hiding is the reader's
-- decoration, not a fact about the coworker, so another org member still sees it.
create table if not exists coworker_hidden (
    account_id    text   not null,
    coworker_id   text   not null,
    hidden_at_ms  bigint not null,
    primary key (account_id, coworker_id)
);

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

-- An update of a scope's box in flight (or the failure it ended in): the phase the pane shows.
-- One row per scope; cleared when the new box is up, kept as 'failed' with the reason until the
-- next attempt.
create table if not exists box_update (
    scope         text   not null,
    scope_id      text   not null,
    phase         text   not null,
    started_at_ms bigint not null,
    updated_at_ms bigint not null,
    error         text,
    primary key (scope, scope_id)
);

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

-- The person's standing answer, per computer, to the egress tunnel's Review-an-action card.
-- Keyed like `scoped_computer` and `computer_sharing`, so a takeover, update or reset that
-- changes the box id keeps the choice. mode is 'bypass' (always allow) | 'ask' | 'never'.
-- Absent ⇒ ask, which is what the card did before there was a choice: behaviour-preserving.
-- (The neighbouring `local_exec_policy` is absent ⇒ never because that channel starts off;
-- this one governs a card that already existed.)
create table if not exists egress_policy (
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
-- Which OG_CREDENTIAL_KEK sealed the row (a hash of the key, never the key). Null on rows sealed
-- before it existed: those are tried under every key held, and `opengrok vault reseal` fills it.
--
-- GUARDED, like `skill.approved_at_ms` below: a bare `add column if not exists` takes ACCESS
-- EXCLUSIVE before deciding there is nothing to do, and this file replays on every boot. Bare, it
-- deadlocked concurrent site-login saves against another harness's boot in CI (25 Sep 2026).
do $do$ begin
    if not exists (
        select 1 from information_schema.columns
         where table_schema = current_schema()
           and table_name = 'secret_store'
           and column_name = 'key_id'
    ) then
        alter table secret_store add column key_id text;
    end if;
end $do$;

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
-- When the person hid this turn, for a turn they hid.
--
-- A person deleting a turn in NativeChat is not asking for it to be destroyed: they are asking
-- not to see it again, on this machine or any other they sign in from. Nothing here is removed —
-- the run, its frames and everything the coworker was told stay exactly as they were, so the
-- coworker's own memory of the conversation is untouched. What changes is what a client is
-- offered when it asks a thread what happened, and what it is shown waiting on it: a hidden run
-- is in neither answer, so no client paints it and none asks the person to look at it again.
--
-- Asking for a run by name still answers with the whole of it. That is deliberate: a turn can be
-- hidden while it is still running, and the machine that hid it goes on following that run to its
-- end. Only the owner can ask, and only with a name they already hold.
alter table run_view add column if not exists hidden_at_ms bigint;

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
-- Inbound webhook wakes (routines). Cron rows keep kind='cron' and a NULL hook_id; the unique
-- index is partial so those NULLs do not collide. The sweep still claims by next_due_ms, which
-- stays NULL for webhooks, so they never fire on the clock.
alter table schedule_view add column if not exists kind text not null default 'cron';
alter table schedule_view add column if not exists hook_id text;
create unique index if not exists schedule_hook_idx
    on schedule_view (hook_id) where hook_id is not null;
-- The bearer's hash and the bearer itself, projected so the two hot paths read ONE ROW instead of
-- replaying a stream: an inbound POST checking a key, and an owner's listing showing them theirs.
-- NEITHER IS NEW EXPOSURE — both are already in `events` in plaintext (`ScheduleEvent::Created`,
-- `SecretRotated`), and this table is behind the same account check the listing is. Rows projected
-- before these columns existed carry '', which their readers take as "ask the aggregate" and not
-- as "this routine has no key".
alter table schedule_view add column if not exists secret_hash text not null default '';
alter table schedule_view add column if not exists webhook_key text not null default '';

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
-- When the firing was recorded, so the in-flight cap can count a run that was fired but has not
-- journaled its first frame yet — without counting forever one that never will. NULL on rows
-- written before the cap existed, which the cap reads as "long since settled".
--
-- Guarded like secret_store.key_id: a bare `add column if not exists` takes ACCESS EXCLUSIVE on
-- every boot, and the sweep writes this table while another harness boots.
do $do$ begin
    if not exists (
        select 1 from information_schema.columns
         where table_schema = current_schema()
           and table_name = 'monitor_firing'
           and column_name = 'fired_at_ms'
    ) then
        alter table monitor_firing add column fired_at_ms bigint;
    end if;
end $do$;

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
-- Whose conversation this entry is part of. A shared coworker is talked to by several people,
-- and one transcript per coworker would put them all in one thread — everybody reading
-- everybody's messages, which is the thing sharing must not do.
--
-- `seq` stays per COWORKER rather than per pair: the primary key is untouched, so no existing
-- row moves, and two members' entries simply interleave in one sequence that each of them reads
-- a subset of. Making seq per-pair would renumber every row that exists.
alter table gateway_entry add column if not exists account_id text;
-- Backfill: before this column, a coworker could only be reached by the person who hired it, so
-- every existing entry is that person's. Idempotent — after the first run nothing is NULL for a
-- coworker that has a row. Entries whose coworker has no view row keep NULL and are read by
-- nobody; no route can reach such a coworker, because every route loads it first.
update gateway_entry e set account_id = c.account_id
  from coworker_view c
 where e.coworker_id = c.id and e.account_id is null;
create index if not exists gateway_entry_reader
    on gateway_entry (coworker_id, account_id, seq);

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
-- spawnError / permissionDenied, or the server's `offline` for a machine with no daemon), distinct
-- from `decision` (the gate's verdict at enqueue). A refusal is a case, not a non-zero exit.
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
-- WHY THIS KEY LAST FAILED TO SERVE (a 401 the gateway still knows the key for, or a 503 naming a
-- credential), written by the model door and read by the console's spend, limit and usage
-- replies. Without it an uncapped coworker whose key reached no seat ran every turn on the
-- deployment's key and its console still read "metered" with nothing in it. Cleared by the next
-- call the key serves and by a re-mint; a row, not a replica's memory, so the replica answering
-- the console need not be the one that ran the turn.
-- Guarded like secret_store.key_id: a bare `add column if not exists` takes ACCESS EXCLUSIVE on
-- every boot, and the model door writes this table on every turn.
do $do$ begin
    if not exists (
        select 1 from information_schema.columns
         where table_schema = current_schema()
           and table_name = 'coworker_gateway_key'
           and column_name = 'refusal'
    ) then
        alter table coworker_gateway_key add column refusal text;
        alter table coworker_gateway_key add column refusal_at_ms bigint;
    end if;
end $do$;
-- Templates carry points, not USD windows; the USD columns stay unread until the cleanup.
alter table coworker_template add column if not exists month_points bigint;
alter table coworker_template add column if not exists day_points bigint;
-- The standing role a coworker hired from this template starts with (`server/persona.rs`).
alter table coworker_template add column if not exists role text;
create index if not exists coworker_template_use_template_idx
    on coworker_template_use (template_id);
-- Groups (`docs/archive/plan-rooms.md` §2): a coworker with members. The roster's isGroup/memberIds.
alter table coworker_view add column if not exists members jsonb not null default '[]'::jsonb;

-- WHEN THIS PERSON LAST LOOKED AT THIS COWORKER. The roster's `hasUnread`/`unreadCount` are read
-- from it, and until this table existed they were hard-coded false/0 — so the official renderer's
-- "New" separator, which anchors at `lastViewedAt` whenever `lastActivityAt` is greater, could
-- never appear.
--
-- PER PAIR, not per coworker: a shared coworker is read by several people and each of them is
-- somewhere different in it. Per coworker would mark a colleague's reading as yours.
--
-- A MISSING ROW MEANS NEVER VIEWED, which is why nothing is backfilled: the renderer already
-- treats a non-finite or non-positive `lastViewedAt` as never, so absence and "never" agree
-- without a migration inventing a moment that did not happen.
create table if not exists coworker_last_viewed (
    coworker_id  text   not null,
    account_id   text   not null,
    viewed_at_ms bigint not null,
    primary key (coworker_id, account_id)
);
-- A DELIBERATE "mark unread" MUST SURVIVE THE APP LOOKING AT THE ROW. The desktop records a view
-- when a coworker is opened or focused, so without this flag the sequence is: the person marks it
-- unread, the app's own view-on-open stamps last-viewed, and the badge they just asked for
-- disappears before they look away. Official keeps the same flag for the same reason
-- (`agent-db.ts`: markUnread sets it, markViewed takes `preserveManualUnread`).
alter table coworker_last_viewed add column if not exists manually_unread boolean not null default false;

-- Recipes: a task a person taught on a coworker's screen, kept in versions (1 raw tape,
-- 2 filtered into steps, 3+ edited), owned by the teacher, shared to people or an org, and
-- granted to the bots that may run it. A deleted recipe keeps its rows (soft delete) so the
-- runs it made stay readable.
--
-- AND WORKFLOWS, WHICH ARE THE SAME ROWS. A workflow is a decision tree that calls recipes as its
-- actions, and it is stored as a `recipe_version` of kind 'workflow' rather than in tables of its
-- own. Ownership, versioning, sharing, grants, run history and artifacts are built and tested
-- here and none of them care whether a body is clicks or a tree; a second set of tables would be
-- every one of these queries written a second time, to be got wrong once. Splitting them out
-- later is a migration; writing them twice now is a permanent tax.
create table if not exists recipe (
    id            text   primary key,
    owner_id      text   not null,
    org_id        text,
    name          text   not null,
    description   text   not null default '',
    screen_w      int    not null default 1280,
    screen_h      int    not null default 800,
    created_at_ms bigint not null,
    updated_at_ms bigint not null,
    deleted_at_ms bigint
);
create index if not exists recipe_owner_idx on recipe (owner_id);
create index if not exists recipe_org_idx on recipe (org_id);

create table if not exists recipe_version (
    recipe_id     text   not null references recipe (id),
    version       int    not null,
    -- FOUR VALUES: 'raw' (the tape as taught), 'filtered' (that tape as steps), 'edited' (a
    -- person's edit of those steps) and 'workflow' (a decision tree, `opengrok-tools/workflow.rs`).
    -- Deliberately NOT a check constraint or an enum type: the values are read by name in Rust,
    -- and a constraint here would make adding the fifth a lock on a table that several replicas
    -- migrate at once for no protection the code does not already give.
    --
    -- A ROW IS ALL ONE OR ALL THE OTHER. Mixing a 'workflow' version into a taped recipe would
    -- make `recipe_runnable_version` hand a tree to the box's recipe runner; the routes refuse the
    -- mix, which is where a refusal can say why in a sentence.
    kind          text   not null,
    body          jsonb  not null,
    note          text   not null default '',
    created_by    text   not null,
    created_at_ms bigint not null,
    primary key (recipe_id, version)
);

-- scope 'account' with an account id, or 'org' with an org id. An org share is accepted per
-- member: accepting writes an 'account' row for that member, so acceptance is always a person's.
create table if not exists recipe_share (
    recipe_id      text   not null references recipe (id),
    scope          text   not null,
    scope_id       text   not null,
    granted_by     text   not null,
    granted_at_ms  bigint not null,
    accepted_at_ms bigint,
    declined_at_ms bigint,
    primary key (recipe_id, scope, scope_id)
);
create index if not exists recipe_share_scope_idx on recipe_share (scope, scope_id);

create table if not exists recipe_grant (
    recipe_id     text   not null references recipe (id),
    coworker_id   text   not null,
    granted_by    text   not null,
    granted_at_ms bigint not null,
    primary key (recipe_id, coworker_id)
);
create index if not exists recipe_grant_coworker_idx on recipe_grant (coworker_id);

create table if not exists recipe_run (
    id          text   primary key,
    recipe_id   text   not null references recipe (id),
    version     int    not null,
    coworker_id text   not null,
    run_id      text,
    ok          boolean not null,
    stopped_at  int,
    receipt     jsonb  not null,
    at_ms       bigint not null
);
create index if not exists recipe_run_recipe_idx on recipe_run (recipe_id, at_ms);

-- Bytes a run or a person produced: a screenshot off a bot's screen, a screen recording of a
-- recipe playing, an image someone attached to a message.
--
-- Stored PLAINLY, unlike `secret_store`. The vault exists for credentials — things that open
-- other systems — and pays a decrypt on every read to keep them out of a database dump. A
-- screenshot is not that: the conversation it belongs to is already plaintext jsonb two tables
-- up in `events`, so encrypting the picture while the words beside it are in the clear buys
-- nothing against the same compromise and costs a decrypt every time a page of thumbnails is
-- drawn. If message payloads are ever sealed, these should be sealed in the same change.
--
-- No foreign keys, like every other table here: this schema is append-mostly and a row that
-- outlives its parent is readable history rather than an error.
create table if not exists artifact (
    id            text   primary key,
    -- Whose bytes these are. Every read checks it; nothing is served across accounts.
    account_id    text   not null,
    -- `screenshot` | `recording` | `attachment`.
    kind          text   not null,
    mime          text   not null,
    -- What to call it when someone saves it.
    filename      text   not null,
    size_bytes    bigint not null,
    bytes         bytea  not null,
    -- Where it came from. A recipe run fills the first three; a message fills the thread.
    recipe_id     text,
    run_id        text,
    step_index    int,
    thread_id     text,
    -- Width, height, duration: what a viewer needs before it has the bytes.
    meta          jsonb  not null default '{}'::jsonb,
    created_at_ms bigint not null,
    deleted_at_ms bigint
);
create index if not exists artifact_run_idx
    on artifact (recipe_id, run_id, step_index) where deleted_at_ms is null;
create index if not exists artifact_thread_idx
    on artifact (account_id, thread_id) where deleted_at_ms is null;

-- A person's saved site logins, so the same rows follow them to every Mac. The password
-- is sealed in secret_store under `site-login:<account>:<id>` (the account id in the key is
-- what the purge finds it by); only the owner's own app ever opens it, over the bearer door.
create table if not exists site_login (
    id             text   primary key,
    account_id     text   not null,
    origin         text   not null,
    username       text   not null,
    label          text   not null default '',
    created_at_ms  bigint not null,
    updated_at_ms  bigint not null,
    unique (account_id, origin, username)
);
create index if not exists site_login_account on site_login (account_id);
-- What kind of thing the row is (a password, an authenticator code, a passkey), the
-- person's notes, and when a bot last used it. A code's seed is sealed beside the password
-- under `site-login-otp:<account>:<id>`.
alter table site_login add column if not exists kind text not null default 'password';
alter table site_login add column if not exists notes text not null default '';
alter table site_login add column if not exists last_used_at_ms bigint;
-- A passkey row: the credential id and user handle (base64) and the relying party the key
-- answers for. The private key is sealed under `site-login-passkey:<account>:<id>`.
alter table site_login add column if not exists passkey_credential_id text;
alter table site_login add column if not exists passkey_rp_id text;
alter table site_login add column if not exists passkey_user_handle text;
-- A password and a passkey for one username on one site are two rows: the kind is part of
-- the key, so saving one never turns the other into it.
alter table site_login drop constraint if exists site_login_account_id_origin_username_key;
create unique index if not exists site_login_owner_site_name_kind
    on site_login (account_id, origin, username, kind);

-- SKILLS. A named, versioned bundle of instructions a person invokes for one turn by typing
-- `/name`: a SKILL.md body, plus whatever small files sit beside it. Owned by an account, visible
-- to the owner's org, soft-deleted so a run that cited one can still say what it cited.
--
-- MODELLED ON `recipe` DELIBERATELY. Ownership, versioning, soft delete and org visibility are
-- the same problem there, and the shapes that solved it are worth repeating rather than
-- re-inventing a second time to be got wrong once.
create table if not exists skill (
    id            text    primary key,
    owner_id      text    not null,
    org_id        text,
    name          text    not null,
    description   text    not null default '',
    -- Where this skill came from: 'authored', 'uploaded', or 'taught'. Descriptive, written by
    -- the server from how the row was made, and never a permission.
    source        text    not null default 'authored',
    -- The owner's switch, and it is not only about turns: a disabled skill leaves the ORG's
    -- listing and stops reading for a colleague (`server/skills.rs`, `relation_to`), because a
    -- colleague seeing one would be seeing something they cannot use. Its OWNER still lists and
    -- reads it — switching a skill off is not hiding it from yourself. The turn path is a later
    -- PR and reads this column rather than a request.
    enabled       boolean not null default true,
    created_at_ms bigint  not null,
    updated_at_ms bigint  not null,
    deleted_at_ms bigint
);
create index if not exists skill_owner_idx on skill (owner_id);
create index if not exists skill_org_idx on skill (org_id);
-- WHEN SOMEBODY SAID THIS BODY WAS FIT TO USE. Null means nobody has, which is where a skill a
-- MODEL wrote from a recording starts (`server/skills.rs`, `from_tape`). A skill a person wrote
-- or uploaded is stamped as it is inserted, because writing it IS reading it.
--
-- A FACT, NOT THE ENFORCEMENT POINT. `enabled` decides what a turn may use and stays the only
-- thing that decides it. This column exists because that switch cannot tell "nobody has read
-- this" from "read, approved, and switched off again a month later" — the two rows are
-- byte-identical — so a client drawing a review queue had to guess from `source = 'taught'` plus
-- a switch position, a heuristic the server never promised and could quietly break.
--
-- THE CATALOGUE IS CHECKED FIRST, for the reason spelled out on `skill_file_version_fk` below and
-- learned again here: `alter table` takes an ACCESS EXCLUSIVE lock on the table BEFORE it decides
-- there is nothing to do, so a bare `add column if not exists` in a file replayed on every boot
-- fights every write in flight. Written that way it deadlocked against a concurrent
-- `add_skill_version` the first time the gate ran it. Guarded, the steady state reads one
-- catalogue row and takes no lock at all.
--
-- The backfill sits inside the same guard and therefore runs exactly once. `or enabled` is the
-- half that is easy to leave out and wrong to: a taught skill that is already switched on was
-- approved by somebody — that is the only way it can be on — and leaving it null would make an
-- already-reviewed skill look unreviewed, which is the exact state this column exists to tell
-- apart. The dev database is the one most likely to hold such rows.
--
-- AND IT HOLDS THE LOCK FOR THE REST OF THE FILE. The schema is executed as one batch, so the
-- ACCESS EXCLUSIVE this ALTER takes is held until the last statement below commits — once, on the
-- boot that adds the column, on a table with no long transactions against it. Worth knowing
-- before adding anything slow after this point.
--
-- `table_schema` is named because `skill` is a common word: a stale row in another schema on the
-- search path would otherwise answer this question for us and leave the column unmade.
do $do$ begin
    if not exists (
        select 1 from information_schema.columns
         where table_schema = current_schema()
           and table_name = 'skill'
           and column_name = 'approved_at_ms'
    ) then
        alter table skill add column approved_at_ms bigint;
        update skill set approved_at_ms = created_at_ms
         where source <> 'taught' or enabled;
    end if;
end $do$;
-- ONE `/name` MEANS ONE SKILL. Two live skills called `review` under one account make the
-- invocation ambiguous, and the ambiguity would be resolved by whichever row sorted first.
-- Partial, so a deleted row never blocks the name it used to hold.
create unique index if not exists skill_owner_name_idx
    on skill (owner_id, name) where deleted_at_ms is null;

create table if not exists skill_version (
    skill_id      text   not null references skill (id),
    version       int    not null,
    -- 'authored' (a person wrote it here), 'uploaded' (a SKILL.md came in) or 'taught' (a turn
    -- wrote it down). Read by name in Rust, and deliberately NOT a check constraint or an enum
    -- type, for the reason spelled out on `recipe_version.kind` above: adding the fourth value
    -- would be a lock on a table several replicas migrate at once, for no protection the code
    -- does not already give.
    kind          text   not null,
    -- Text, not jsonb: a skill body is Markdown a model reads, and jsonb would re-order and
    -- re-escape it. Capped in the server (`skills::MAX_SKILL_BODY_CHARS`) rather than here,
    -- because a refusal has to be able to say what the limit was and what arrived.
    body          text   not null,
    note          text   not null default '',
    created_by    text   not null,
    created_at_ms bigint not null,
    primary key (skill_id, version)
);

-- The small files that came with a version. Per VERSION, not per skill: a new body that drops a
-- reference file must not leave the old body reading a file that is no longer beside it.
create table if not exists skill_file (
    skill_id text  not null references skill (id),
    version  int   not null,
    path     text  not null,
    bytes    bytea not null,
    primary key (skill_id, version, path)
);
-- A file belongs to a VERSION that exists. `add_skill_version` writes both in one transaction so
-- it cannot be otherwise today, but the invariant belongs in the schema: a file row naming a
-- version nobody can open is bytes with no page, and it would be invisible until somebody read
-- the table by hand.
--
-- THE CATALOGUE IS CHECKED FIRST, and that guard is the whole point of the block rather than
-- tidiness. Postgres has no `add constraint if not exists`, and the obvious spelling —
-- `drop constraint if exists` followed by `add constraint` — is wrong HERE, where the file is
-- replayed on every boot: `alter table ... add constraint` takes an ACCESS EXCLUSIVE lock on the
-- table before it does anything else, so every restart would fight the writes already in flight.
-- It does not merely slow things down; written that way it deadlocked against a concurrent
-- insert the first time the gate ran it. Guarded, the steady-state path issues no ALTER and takes
-- no lock at all.
do $do$ begin
    if not exists (select 1 from pg_constraint where conname = 'skill_file_version_fk') then
        alter table skill_file add constraint skill_file_version_fk
            foreign key (skill_id, version) references skill_version (skill_id, version)
            on delete cascade;
    end if;
end $do$;

-- A NativeChat follow-up that has not yet become a run. Mutable on purpose: the product is
-- cancel and edit *before* drain, and an append-only stream would make those two writes a
-- tombstone dance for a row that should simply go away. Identity is (thread, account) — a
-- shared coworker does not share this queue (CLAUDE.md #5, one transcript per person).
--
-- `status` is a word, not an enum type, for the same reason as `skill_version.kind`: adding a
-- third value must not take an ACCESS EXCLUSIVE lock on every boot. The only writers are the
-- functions in `pending.rs`; they spell `pending` and `drained`.
create table if not exists pending_user_message (
    id                 text        primary key,
    thread_id          text        not null,
    account_id         text        not null,
    content            text        not null,
    reply_to           jsonb,
    recipe_id          text,
    recipe_values      jsonb,
    skill_id           text,
    client_message_id  text,
    status             text        not null,
    created_at_ms      bigint      not null,
    updated_at_ms      bigint      not null,
    drained_at_ms      bigint,
    drained_run_id     text
);
create index if not exists pending_user_message_thread_idx
    on pending_user_message (account_id, thread_id, created_at_ms)
    where status = 'pending';
-- Idempotent enqueue: the client's bubble id is the natural key. Drained rows KEEP the key so
-- a second POST cannot re-queue a send that already became a run. Cancelled rows are deleted,
-- so the same bubble can be queued again after the person takes it back.
create unique index if not exists pending_user_message_client_idx
    on pending_user_message (account_id, thread_id, client_message_id)
    where client_message_id is not null;

-- Unused since the `credential.request` broker flow was deleted: nothing writes or reads it.
-- Kept only so a boot does not drop rows an older build wrote. It never held a password.
create table if not exists credential_hint (
    account_id     text   not null,
    coworker_id    text   not null,
    origin         text   not null,
    username       text   not null default '',
    credential_id  text   not null,
    updated_at_ms  bigint not null,
    primary key (account_id, coworker_id, origin)
);

-- A RECIPE RUN IS A ROW BEFORE THE BOX IS TOUCHED, and this is how the row says it is still
-- playing. A run used to be written only after the box answered, inside the request that asked for
-- it — so a closed tab dropped the handler, the box finished clicking, and the server kept no run,
-- no screenshots and no error. Now the row comes first and the work is detached from the request.
--
-- A LEASE, NOT A STATUS, for the reason `recovery.rs` gives: a live process pushes the expiry out
-- as it works and a dead one cannot, so a row whose lease has passed had nobody finishing it. Read
-- that way, a restart needs no sweep and two replicas cannot disagree. Null is a finished run —
-- every row written before this column existed, and every row once its receipt lands.
--
-- Guarded like `skill.approved_at_ms` above, and for the same deadlock: a bare `add column if not
-- exists` takes ACCESS EXCLUSIVE on every boot before deciding there is nothing to do. The index
-- is inside the guard so it, too, is built once; it serves "is this bot already playing?".
do $do$ begin
    if not exists (
        select 1 from information_schema.columns
         where table_schema = current_schema()
           and table_name = 'recipe_run'
           and column_name = 'lease_until_ms'
    ) then
        alter table recipe_run add column lease_until_ms bigint;
        create index recipe_run_live_idx on recipe_run (coworker_id)
            where lease_until_ms is not null;
    end if;
end $do$;
"#;

/// Run on EVERY boot, after `SCHEMA`, whether or not the schema itself was replayed.
///
/// A grant or ceiling written as exactly an older built-in set follows the built-ins. During a
/// deploy an older replica can still write the older set after a newer one has migrated, and the
/// next boot is what brings that row along (`old_grants_follow_the_builtins.rs`). These are
/// UPDATEs over rows that match, so they take row locks only, never the table locks that made
/// replaying `SCHEMA` on every boot deadlock against live reads.
const EVERY_BOOT: &str = r#"
-- The screen tools (open_url, computer) joined the built-ins. A grant or ceiling written as
-- EXACTLY the previous built-in set was "everything this server implements" when it was written,
-- so it follows the built-ins; a narrower or wider list was chosen on purpose and is left alone.
-- Idempotent: once widened, the row no longer matches.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["read_file", "shell", "write_file"]}'::jsonb;
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb
 where tools = '{"only": ["read_file", "shell", "write_file"]}'::jsonb;
-- `run_recipe` joined the built-ins the same way: a row that is exactly the five-tool set
-- follows; the two statements chain, so a three-tool row widens twice in one boot.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb;
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb
 where tools = '{"only": ["computer", "open_url", "read_file", "shell", "write_file"]}'::jsonb;
-- `request_user_form` joined the built-ins the same way: a row that is exactly today's
-- previous set follows; a narrower list was chosen on purpose and is left alone.
update grant_view
   set profile = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb;
update ceiling_view
   set tools = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where tools = '{"only": ["computer", "open_url", "read_file", "run_recipe", "shell", "write_file"]}'::jsonb;
-- `credential.request` joined the built-ins the same way. Site passwords are NOT stored;
-- this tool only asks the client to fill a saved login.
update grant_view
   set profile = '{"only": ["computer", "credential.request", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where profile = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb;
update ceiling_view
   set tools = '{"only": ["computer", "credential.request", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb
 where tools = '{"only": ["computer", "open_url", "read_file", "request_user_form", "run_recipe", "shell", "write_file"]}'::jsonb;
-- `credential.request` left with the broker (Sep 2026): the saved login is offered on the
-- ordinary form card. The widening just above still matches today's default grant, so it
-- would put the dead name on every fresh bot at every boot; this takes it out again.
update grant_view
   set profile = jsonb_set(profile, '{only}', (profile->'only') - 'credential.request')
 where profile->'only' ? 'credential.request';
update ceiling_view
   set tools = jsonb_set(tools, '{only}', (tools->'only') - 'credential.request')
 where tools->'only' ? 'credential.request';
"#;

/// Apply the schema. Safe to call on every boot and from every replica.
pub async fn run(pool: &PgPool) -> StoreResult<()> {
    let mut conn = pool.acquire().await?;
    sqlx::query("select pg_advisory_lock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await?;

    let applied = apply_unless_current(&mut conn).await;

    // Release even when the migration failed, or the next boot deadlocks against our own lock.
    let released = sqlx::query("select pg_advisory_unlock($1)")
        .bind(MIGRATION_LOCK_KEY)
        .execute(&mut *conn)
        .await;

    applied?;
    released?;
    Ok(())
}

/// Replay the schema only when THIS schema has not been applied here before.
///
/// A REPLAY IS NOT FREE, even when every statement is `if not exists`. The schema is one
/// transaction, and a bare `alter table … add column if not exists` takes ACCESS EXCLUSIVE before
/// it decides there is nothing to do, then holds it to the end. Every server and every test
/// harness boots through here, so any read that locked those tables in the other order
/// deadlocked against a boot: `policy_to_use` refused a turn over a grant that was fine, the
/// site-login saves died (#239), and a recipe's history prune was killed, leaving six runs where
/// five belong. Guarding each statement one at a time left the next one to be found in CI.
///
/// Keyed by the schema's digest, so any edit to `SCHEMA` replays it once, exactly as before;
/// only an unchanged schema is skipped. The check runs under the advisory lock, so two replicas
/// booting a new schema still apply it once and the second finds the digest. What must run on
/// every boot regardless lives in `EVERY_BOOT`, which never takes a table lock.
async fn apply_unless_current(conn: &mut sqlx::PgConnection) -> StoreResult<()> {
    use sha2::Digest as _;
    let digest: String = sha2::Sha256::digest(SCHEMA.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    // Created on its own, before the schema: it is the one table the skip decision reads.
    sqlx::query(
        "create table if not exists schema_applied (
             digest     text primary key,
             applied_at bigint not null
         )",
    )
    .execute(&mut *conn)
    .await?;
    let current: Option<(i32,)> = sqlx::query_as("select 1 from schema_applied where digest = $1")
        .bind(&digest)
        .fetch_optional(&mut *conn)
        .await?;
    if current.is_none() {
        sqlx::raw_sql(SCHEMA).execute(&mut *conn).await?;
        record_applied(conn, &digest).await?;
    }
    sqlx::raw_sql(EVERY_BOOT).execute(&mut *conn).await?;
    Ok(())
}

async fn record_applied(conn: &mut sqlx::PgConnection, digest: &str) -> StoreResult<()> {
    sqlx::query(
        "insert into schema_applied (digest, applied_at)
         values ($1, (extract(epoch from now()) * 1000)::bigint)
         on conflict (digest) do nothing",
    )
    .bind(digest)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
