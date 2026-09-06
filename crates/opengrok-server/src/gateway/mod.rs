//! Seam A: the gateway the desktop client actually lives on.
//!
//! This is the surface `docs/PORT-PRIORITY.md` measured — 123 JSON+SSE commands, not protobuf —
//! and this module is its slice 7: enough for the real, unmodified Grok Bot app to boot against
//! us (`/health`, `/events`, and the roster/settings commands), pointed here by nothing more than
//! `SAND_HOST_GATEWAY_URL`.
//!
//! EVERY WIRE RULE HERE IS TRANSCRIBED, NOT DESIGNED. Shapes, headers, status codes and timings
//! come from `docs/research/client-grok-bot.md` §2.0 and §9, each read from the client's own
//! source. Where this module has an opinion the client does not check, it keeps none.
//!
//! THE AUTH MODEL IS THE SHIPPED HOST'S. One shared bearer, compared in constant time; any
//! request carrying an `Origin` header is refused (a browser page must never be able to drive
//! the gateway, even with the token); and with no token configured, only loopback `Host`s are
//! served — exactly `gateway-server.ts`'s posture, so a deployment that forgets the token fails
//! closed instead of open.

pub mod cards;
pub mod conversation;
pub mod group;
pub mod lifecycle;
pub mod live;
// THE MOCK CATALOGUE IS A BUILD-TIME CHOICE, not just a runtime one. `mock_fixtures` carries
// ~65 KB of embedded fixture files and a filesystem read verb; both are development surface and
// neither belongs in a production binary, where the only thing standing between them and a caller
// would be an environment variable. Off by default, so shipping it is something a build has to
// ask for rather than something a release has to remember to remove. `mock_fixtures_absent`
// answers the same five functions honestly, so no call site knows which one it got.
#[cfg(feature = "mock-fixtures")]
pub mod mock_fixtures;
#[cfg(not(feature = "mock-fixtures"))]
#[path = "mock_fixtures_absent.rs"]
pub mod mock_fixtures;
pub mod routes;
pub mod summaries;

use std::sync::{Arc, Mutex};

use subtle::ConstantTimeEq;

use crate::agui::routes::AgUiState;

/// The header carrying the CALLER's account access token on gateway calls — the per-account pivot.
///
/// The gateway's `Authorization` header is the shared TRANSPORT bearer (one secret for the whole
/// host, checked by `refuse`), so it cannot also identify the person. The desktop therefore sends
/// its signed account access token — the same JWT seam B authenticates with — in this separate
/// header, and the gateway resolves WHOSE roster/coworkers to serve from it. Signed, so a caller
/// cannot forge another account; verified exactly like seam B (`minter.verify_access`).
pub const ACCOUNT_HEADER: &str = "x-opengrok-account";

/// Who a seam-A request is for, or why we will not guess.
///
/// THIS USED TO FAIL OPEN, AND IT AUTHENTICATED PEOPLE AS SOMEBODY ELSE. The old shape returned
/// `state.email` whenever the header was missing or unreadable. That fallback was written to keep
/// a client that had not yet learned to send the header working — additive, not a flag day — and
/// it is a reasonable migration choice right up until you notice what it means: a request with no
/// identity was served AS the account named by `OG_GATEWAY_EMAIL`, which on a dev box is the
/// admin. On 5 Sep 2026 that is exactly what happened. The desktop attaches the header once per
/// CONNECTION; one empty read of its token secret at connect time dropped the header for the whole
/// life of that connection, and every call over it — reads and writes, `sendPrompt` included —
/// ran as the admin. Nothing degraded, nothing retried, nothing logged. The person saw another
/// account's coworkers in their sidebar and could have written to that account's transcripts.
///
/// Both ends failed open, each citing the other's fallback as its justification: the client's
/// comment said a failure here was fine because "the server falls back to its configured account",
/// and ours said the fallback kept older clients working. Each is defensible alone. Together they
/// authenticate as the admin, and neither side's review catches it, because the bug is in the seam.
///
/// So: a missing identity is now a refusal the caller can act on (CLAUDE.md #8), and `Fallback` is
/// reachable only when a deployment opts in with `OG_GATEWAY_IDENTITY_FALLBACK=1`.
pub enum Caller {
    /// The account named by a verified `ACCOUNT_HEADER` token.
    Account(String),
    /// No usable header, and the deployment opted back in to the old behaviour.
    Fallback(String),
    /// No header at all. `account_identity_required` — the client should REBUILD its connection
    /// (which re-reads the token secret) rather than retry the call, because on this seam the
    /// header is attached per connect and a retry carries the same absence.
    Missing,
    /// A header that did not verify — expired, wrong signature, or malformed.
    /// `account_identity_invalid` — a rebuild will not fix it, so the client must surface it.
    Invalid,
}

impl Caller {
    /// The email to act as, for a caller we accepted.
    pub fn email(&self) -> Option<&str> {
        match self {
            Self::Account(email) | Self::Fallback(email) => Some(email),
            Self::Missing | Self::Invalid => None,
        }
    }

    /// The stable code the client branches on. Prose is for people and may be reworded; this is
    /// the contract, compared with `===` on the other side.
    pub fn refusal_code(&self) -> Option<&'static str> {
        match self {
            Self::Missing => Some("account_identity_required"),
            Self::Invalid => Some("account_identity_invalid"),
            Self::Account(_) | Self::Fallback(_) => None,
        }
    }

    /// What to log this request as. Never the token, only the resolved identity — the request line
    /// recorded `auth_len` and nothing else, and because that bearer is SHARED and identical for
    /// every caller, three separate readings of a cross-account bug were drawn from a log that
    /// could not have distinguished them. This is that gap closed.
    pub fn logged(&self) -> &str {
        match self {
            Self::Account(email) | Self::Fallback(email) => email,
            Self::Missing => "<no-identity>",
            Self::Invalid => "<bad-identity>",
        }
    }
}

/// Resolve who a seam-A request is for. See `Caller`.
pub async fn caller_of(state: &GatewayState, headers: &axum::http::HeaderMap) -> Caller {
    let present = headers.get(ACCOUNT_HEADER).is_some();
    // THE EMAIL COMES FROM THE TOKEN, not from the store. An earlier shape loaded the account by
    // `sub` and answered `Invalid` when the load failed — but `load_account` replays a log and
    // cannot fail for a missing account (it replays to a blank one); the only way it fails is the
    // database not answering. So that branch turned a store outage into "sign in again" for every
    // request that carried a valid identity, on the one seam that must open through an outage
    // (`against_events_when_the_store_is_down`). The mint signed `email` beside `sub`
    // (`AccessClaims`) and there is no event that changes an account's email, so the token's copy
    // is as authoritative as the row's for as long as the token lives.
    if let Some((_, email)) = account_from_header(state, headers) {
        return Caller::Account(email);
    }
    if present {
        // A header that did not verify is never a fallback candidate, opted in or not: it is an
        // active claim we rejected, and answering it as somebody else would be worse than the
        // absent case, not better.
        return Caller::Invalid;
    }
    if state.identity_fallback {
        tracing::warn!(
            account = %state.email,
            "seam-A request carried no identity; serving it as OG_GATEWAY_EMAIL because \
             OG_GATEWAY_IDENTITY_FALLBACK=1. This is the pre-5-Sep-2026 behaviour and it serves \
             one account's data to whoever asks without one."
        );
        return Caller::Fallback(state.email.clone());
    }
    Caller::Missing
}

/// The account id and email from a valid `ACCOUNT_HEADER` token, or `None`. Accepts the raw JWT
/// or a `Bearer <jwt>` value, so the client may reuse its Authorization-shaped token verbatim.
fn account_from_header(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Option<(opengrok_core::id::AccountId, String)> {
    let raw = headers
        .get(ACCOUNT_HEADER)
        .and_then(|value| value.to_str().ok())?;
    let token = raw.strip_prefix("Bearer ").unwrap_or(raw).trim();
    // NAME THE FAILURE. This used to be `.ok()?`, and the silence was half the bug: a token that
    // did not verify became "no identity", which became the deployment account, with nothing said.
    // `jsonwebtoken` distinguishes InvalidSignature from ExpiredSignature from InvalidToken, and
    // which one it is decides whose bug it is — so log it rather than make the next person guess.
    match state.agui.auth.minter.verify_access(token) {
        Ok(claims) => Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            claims.email,
        )),
        Err(error) => {
            tracing::warn!(
                %error,
                token_len = token.len(),
                "an account identity header did not verify"
            );
            None
        }
    }
}

/// One frame on the live bus: the channel it belongs to, the payload, and the account it is for.
///
/// `audience: None` is a frame that names nobody and may go to every stream. `Some(account)`
/// carries one person's data and is delivered only to streams that person opened — the check
/// that used to be missing, and that the desktop client was inadvertently doing for us by
/// ignoring agents it did not know.
#[derive(Clone, Debug)]
pub struct LiveFrame {
    pub channel: String,
    pub payload: serde_json::Value,
    pub audience: Option<opengrok_core::id::AccountId>,
}

/// What the gateway knows beyond the shared server state.
#[derive(Clone)]
pub struct GatewayState {
    pub agui: AgUiState,
    /// The shared bearer, compared timing-safe. `None` = loopback-only, the shipped default.
    pub bearer: Option<String>,
    /// Whose coworkers this gateway serves as its roster. The desktop is a one-person surface;
    /// this names the person.
    pub email: String,
    /// Serve a request that carries NO account identity as `email`, the pre-5-Sep-2026 behaviour.
    /// Read once at startup from `OG_GATEWAY_IDENTITY_FALLBACK`, never per request. Off by
    /// default because on it is how a headerless connection read and wrote as the admin; see
    /// `Caller`. Tests that mean to exercise the deployment identity opt in explicitly with
    /// `allowing_identity_fallback`, which also makes them greppable.
    pub identity_fallback: bool,
    /// Host settings, echoed back the way the resync chain expects. In memory on purpose: the
    /// client rewrites every field of interest on every `transport-connected`, so persisting
    /// them would only preserve values the next connect immediately overwrites.
    pub settings: Arc<Mutex<serde_json::Value>>,
    /// When this process started — `/health`'s `startedAt`.
    pub started_at_ms: i64,
    /// The live stream: every SSE frame goes through here, and each `/events` subscriber holds
    /// a receiver. Slow subscribers lag and are dropped by `broadcast`'s own rules — a stalled
    /// client must not be able to wedge the host.
    /// Every live frame: the channel, the payload, and WHO IT IS FOR.
    ///
    /// The audience is the third element because the bus is a broadcast: without it, a frame
    /// built from one person's data reached every open stream, and the only thing standing
    /// between that and a disclosure was the client choosing not to render an agent it did not
    /// recognise. `None` means "everybody", which is the honest answer for a frame whose payload
    /// names nobody. `Some(account)` is delivered to that account's streams alone.
    pub events_tx: tokio::sync::broadcast::Sender<LiveFrame>,
    /// This process's ordering epoch. A client that sees the epoch change treats the replica as
    /// restarted and resyncs — which is exactly what a restart means.
    pub epoch: String,
    /// Monotonic sequence per replica key (`roster`, `transcript:<agentId>`), for the `ordered`
    /// stamp every roster and transcript event carries.
    pub seqs: Arc<Mutex<std::collections::HashMap<String, i64>>>,
    /// The address `EnsureSandBox` hands out — the mint. Non-loopback or the client refuses it;
    /// `None` means the mint answers failed_precondition rather than inventing an address.
    pub public_gateway_url: Option<String>,
    /// Which agent the client last opened. `getTranscript` and `sendPrompt` fall back to it.
    pub active_agent: Arc<Mutex<Option<String>>>,
    /// Coworkers with a turn in flight right now — the roster's `isRunning`.
    pub running: Arc<Mutex<std::collections::HashSet<String>>>,
    /// The abort handle of each in-flight turn, keyed by agent id, so `stopAgentTurn` can cancel a
    /// running turn. Inserted when a turn is spawned, removed when it ends; a bot with a phantom
    /// `running` flag simply has no entry here, and stopping it just clears the flag.
    pub cancels: Arc<Mutex<std::collections::HashMap<String, tokio::task::AbortHandle>>>,
}

impl GatewayState {
    pub fn new(
        agui: AgUiState,
        bearer: Option<String>,
        email: String,
        public_gateway_url: Option<String>,
    ) -> Self {
        Self {
            agui,
            bearer,
            email,
            public_gateway_url,
            identity_fallback: std::env::var("OG_GATEWAY_IDENTITY_FALLBACK").as_deref() == Ok("1"),
            settings: Arc::new(Mutex::new(default_settings())),
            started_at_ms: chrono::Utc::now().timestamp_millis(),
            events_tx: tokio::sync::broadcast::channel(256).0,
            epoch: uuid::Uuid::now_v7().to_string(),
            seqs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            active_agent: Arc::new(Mutex::new(None)),
            running: Arc::new(Mutex::new(std::collections::HashSet::new())),
            cancels: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Serve headerless requests as `email`. For tests whose subject is not identity and which
    /// therefore speak as the deployment account, and for a deployment that has knowingly opted
    /// back in. Never reach for this to make a failing identity test pass.
    #[must_use]
    pub fn allowing_identity_fallback(mut self) -> Self {
        self.identity_fallback = true;
        self
    }
}

/// The `getHostSettings` record the client reads back — every field from
/// `client-grok-bot.md` §9, with the defaults the shipped host starts from. Fields the shape
/// marks "omitted when undefined" are omitted.
pub fn default_settings() -> serde_json::Value {
    serde_json::json!({
        "notifications": {
            "isEnabled": false, "allowedApps": [], "minIntervalMs": 5000,
            "maxPerWindow": 10, "windowMs": 300_000
        },
        "mcpCustomInstructions": {},
        "mcpCustomInstructionsByServerId": {},
        "mcpDisabledToolsByServerId": {},
        "mcpBoxServers": [],
        "autoReviewInstructions": null,
        "localToolPermission": null,
        "webauthnProxyEnabled": false,
        // Ours, not Cursor's — but the field must exist, the resync chain reads the record whole.
        "inferenceProvider": "cursor",
        "inferenceRouterUsage": null,
        "sidebarSections": [],
        "hasSeenOnboarding": true
    })
}

/// The gateway's whole access decision, in the order the shipped host applies it.
///
/// Returns the refusal to send, or `None` for "come in".
pub fn refuse(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
) -> Option<(u16, &'static str)> {
    // A browser origin is refused before anything else — with or without the token. A web page
    // that has somehow learned the bearer must still not be able to drive a desktop's gateway.
    if headers.get(axum::http::header::ORIGIN).is_some() {
        return Some((403, "browser origins are not served"));
    }

    match &state.bearer {
        Some(expected) => {
            let presented = headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default();
            // Constant-time on the bytes; the length leak is unavoidable and harmless for a
            // random token.
            let matches = presented.len() == expected.len()
                && presented.as_bytes().ct_eq(expected.as_bytes()).into();
            if matches {
                None
            } else {
                Some((401, "bad token"))
            }
        }
        None => {
            // No token pinned: loopback hosts only, exactly the shipped host's fallback. This is
            // what keeps "forgot to configure auth" from meaning "open on the LAN".
            let host = headers
                .get(axum::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
            let loopback = matches!(name, "127.0.0.1" | "localhost" | "[::1]" | "::1");
            if loopback {
                None
            } else {
                Some((403, "no gateway token is configured; loopback only"))
            }
        }
    }
}
