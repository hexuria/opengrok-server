//! The account's ONE shared computer, and its teardown.
//!
//! 1 account = 1 computer (`docs/plan-bots-computers-channels.md`). The account's first agent
//! creates the box; every later agent of that account SHARES it — provisioning is automatic, with
//! no per-agent choice and no "connect" step. Deleting an agent does not destroy the box (others
//! may share it); when the account's LAST agent is deleted, the box is destroyed and the mapping
//! cleared, so nothing runs unused.
//!
//! A provisioning failure is never fatal to the hire: a boxless coworker is still a coworker, and
//! the reason is reported for the client to show.

use std::sync::Arc;

use opengrok_box::{Computer, EgressTunnel};
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerError, CoworkerEvent};
use opengrok_core::id::{AccountId, BoxId, CoworkerId};

use serde_json::{Value, json};

use crate::agui::AgUiState;

/// The org that owns an account, if any — the key to which computer credentials apply.
async fn account_org(state: &AgUiState, account_id: &AccountId) -> Option<String> {
    let (account, _) = state.auth.store.load_account(account_id).await.ok()?;
    account.org_id
}

/// Why `lookup_provider` could not build a computer. `invalid_key` is a saved ascii secret this
/// process cannot open (KEK rotated); `no_org_key` is no secret at all; `not_supported` is a kind
/// this deployment does not serve. The client already has copy for these codes.
pub struct ProviderLookup {
    pub computer: Option<Arc<dyn Computer>>,
    pub error: Option<(String, String)>,
}

fn ascii_unreadable() -> ProviderLookup {
    ProviderLookup {
        computer: None,
        error: Some((
            "invalid_key".into(),
            "The saved box.ascii.dev key cannot be opened. An admin can paste it again on the dashboard.".into(),
        )),
    }
}

fn ascii_missing() -> ProviderLookup {
    ProviderLookup {
        computer: None,
        error: Some((
            "no_org_key".into(),
            "no computer is configured for your organization — an admin must set up box.ascii.dev on the dashboard".into(),
        )),
    }
}

/// The provider for a computer of `kind` in this org, plus why it is missing when it is.
/// Decrypt failure is `invalid_key`, not "no computer" — a sealed blob the current KEK cannot
/// open is how a live box looked absent after a reboot regenerated `OG_CREDENTIAL_KEK`.
pub async fn lookup_provider(
    state: &AgUiState,
    org_id: Option<&str>,
    kind: &str,
) -> ProviderLookup {
    // "local-docker" is served ONLY by the deployment's own Docker provider (OG_COMPUTER at boot;
    // a stand-in in tests). Until 25 Sep 2026 this built a DockerComputer unconditionally, so a
    // hosted server (where boot installs none) or `OG_COMPUTER=none` still ran a bot container on
    // the API host whenever box.ascii.dev refused and the takeover asked for one. "Hosted",
    // "none" and "ascii-only" are now one fact a test can hand in; `OG_HOSTED` is not settable
    // from a test (`set_var` is unsafe). NEVER the boot provider for "ascii": that one is built
    // from the ORG's sealed key, and OG_BOX_API_KEY would run one org's boxes on another's account.
    if kind == "local-docker" {
        return match state.computer.as_ref() {
            Some(computer) if computer.kind() == kind && local_docker_allowed() => ProviderLookup {
                computer: Some(computer.clone()),
                error: None,
            },
            _ => ProviderLookup {
                computer: None,
                error: Some(("not_supported".into(), NO_LOCAL_VM.into())),
            },
        };
    }
    match kind {
        "ascii" => lookup_ascii(state, org_id).await,
        _ => ProviderLookup {
            computer: None,
            error: Some((
                "not_supported".into(),
                "this deployment does not offer that computer".into(),
            )),
        },
    }
}

/// Why a Local VM is unavailable: the pane shows it for a row recorded before the server stopped
/// serving them, and the model reads it when a refused box has nothing to fall back on.
pub const NO_LOCAL_VM: &str = "this server does not run Local VMs on its own host, so this \
computer cannot be used here — an admin can set up box.ascii.dev on the dashboard";

async fn lookup_ascii(state: &AgUiState, org_id: Option<&str>) -> ProviderLookup {
    let Some(org) = org_id else {
        return ascii_missing();
    };
    let has_row = state
        .auth
        .store
        .org_computer_kinds(org)
        .await
        .ok()
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "ascii"));
    let Some(vault) = state.vault.as_ref() else {
        return if has_row {
            ascii_unreadable()
        } else {
            ascii_missing()
        };
    };
    match state
        .auth
        .store
        .org_computer_secret(vault, org, "ascii")
        .await
    {
        Ok(Some(key)) => ProviderLookup {
            computer: Some(Arc::new(
                opengrok_box::AsciiBoxes::new(key).with_base_url(state.auth.ascii_base_url.clone()),
            )),
            error: None,
        },
        Ok(None) => ascii_missing(),
        Err(_) => ascii_unreadable(),
    }
}

/// The provider for a computer of `kind` in this org: an AsciiBoxes built from the org's sealed
/// box.ascii.dev key for `"ascii"`, or the deployment's own Docker for `"local-docker"`. `None`
/// when the kind cannot be served (e.g. `"ascii"` but the org has no key or the vault is absent).
/// The SAME provider must create and run a box, so both paths call this.
pub async fn provider_for(
    state: &AgUiState,
    org_id: Option<&str>,
    kind: &str,
) -> Option<Arc<dyn Computer>> {
    lookup_provider(state, org_id, kind).await.computer
}

/// When box.ascii.dev refuses or fails, give the scope a Local VM instead — a SELF-HOST
/// convenience (NativeChat on a laptop still gets Shell/Read), so only where the deployment
/// brought a Docker provider: never hosted, never `OG_COMPUTER=none` (see `lookup_provider`).
///
/// THE SWAP IS STAMPED on the account's `computerError`, with the upstream code. It moves the
/// coworker from its desktop to a different, often headless, machine: files seem to vanish and
/// screen tools disappear, and until this stamp nothing said why.
pub async fn take_over_with_local_docker(
    state: &AgUiState,
    account_id: &AccountId,
    scope: &str,
    scope_id: &str,
    org_id: Option<&str>,
    refused: &opengrok_box::BoxError,
) -> Option<(Arc<dyn Computer>, String)> {
    let Some(computer) = provider_for(state, org_id, "local-docker").await else {
        tracing::warn!(scope, scope_id, %refused, "computer: the provider refused and this server runs no Local VM to fall back on");
        return None;
    };
    tracing::warn!(scope, scope_id, %refused, "computer: the provider refused; asking local Docker");
    let box_id = computer.create(None).await.ok()?;
    let at_ms = chrono::Utc::now().timestamp_millis();
    let store = &state.auth.store;
    store
        .set_scoped_computer(scope, scope_id, &box_id, "local-docker", org_id, at_ms)
        .await
        .ok()?;
    let why = format!("{FELL_BACK} ({refused})");
    // The stamp is the only thing that tells the person the box changed; a lost one is logged
    // rather than failing a takeover that already happened.
    if let Err(error) = store
        .set_account_computer_error(account_id.as_str(), refused.code(), &why, at_ms)
        .await
    {
        tracing::warn!(scope, scope_id, %error, "computer: the takeover could not be stamped on the account; the pane will not say the box changed");
    }
    tracing::warn!(scope, scope_id, box_id = %box_id, "computer: local Docker box is this scope's computer");
    Some((computer, box_id))
}

/// What the stamp says after a takeover. The pane shows it beside the new box.
pub const FELL_BACK: &str = "box.ascii.dev refused this computer, so it now runs as a Local VM \
on this server — files on the old computer are not on this one";

/// The provider for an account's existing computer of `kind`, resolving the account's org itself.
/// The run path uses this so tools execute on the same provider that created the box.
pub async fn provider_for_account(
    state: &AgUiState,
    account_id: &AccountId,
    kind: &str,
) -> Option<Arc<dyn Computer>> {
    let org_id = account_org(state, account_id).await;
    provider_for(state, org_id.as_deref(), kind).await
}

/// Local VM (server-host Docker) is a SELF-HOST / dev convenience only. A hosted, multi-tenant
/// deployment (`OG_HOSTED=1`) must never run untrusted bot containers on the API host — a container
/// escape lands on the machine holding the token secret and the org vault — so it is neither
/// advertised nor used there; production computers are box.ascii.dev / Windows 365 / (later) cloud.
pub fn local_docker_allowed() -> bool {
    std::env::var("OG_HOSTED").as_deref() != Ok("1")
}

/// The kind a NEW account computer should be, from the org's CURRENT config: a box.ascii.dev box
/// when the org has configured a key; else a Local VM on the server host when allowed (dev /
/// self-host); else `"none"` — no provider, and the hire says so readably.
pub async fn kind_for_new(state: &AgUiState, org_id: Option<&str>) -> &'static str {
    if let (Some(vault), Some(org)) = (state.vault.as_ref(), org_id)
        && state
            .auth
            .store
            .org_computer_secret(vault, org, "ascii")
            .await
            .ok()
            .flatten()
            .is_some()
    {
        "ascii"
    } else if local_docker_allowed()
        && state
            .computer
            .as_ref()
            .is_some_and(|computer| computer.kind() == "local-docker")
    {
        // The deployment brought a Docker provider (OG_COMPUTER, or the default when no ascii
        // key is set). Without one there is nothing to give: `OG_COMPUTER=none` now means what
        // the boot log says, and an integration test with no provider hires computerless —
        // which is how test runs stopped leaving hundreds of `sleep infinity` containers on the
        // dev Mac (192 found on 2 Sep 2026, none mapped to any scope).
        "local-docker"
    } else {
        "none"
    }
}

/// The outcome of assigning the account's computer to a freshly-hired coworker.
pub struct Provisioned {
    /// The `ComputerAssigned` events to persist alongside the hire (empty when none/failed).
    pub events: Vec<CoworkerEvent>,
    /// The account's box id, for the coworker view (`None` when none/failed).
    pub box_id: Option<BoxId>,
    /// Why the box could not be given, as (code, message) — never fatal to the hire. `code` is one
    /// of the seven stable codes; `message` is the human-readable reason.
    pub error: Option<(String, String)>,
}

/// Record a provisioning failure at the ACCOUNT level (so a boxless account can say why before any
/// agent exists) and return it. Never fatal — the hire stands, boxless.
async fn record_error(
    state: &AgUiState,
    account_id: &AccountId,
    code: &str,
    message: &str,
    at_ms: i64,
) -> Provisioned {
    let _ = state
        .auth
        .store
        .set_account_computer_error(account_id.as_str(), code, message, at_ms)
        .await;
    Provisioned {
        events: Vec::new(),
        box_id: None,
        error: Some((code.to_string(), message.to_string())),
    }
}

/// A provisioning error as the client contract `{code, message, updatedAtMs}`, or null. The same
/// shape on every surface: create responses, listOpenGrokComputers, and agent rows.
///
/// THE STAMP IS PART OF THE ANSWER. On 5 Sep 2026 a `quota_exceeded` recorded at 13:44 was shown
/// as the live state of a box at 20:11, and nothing in the payload could tell the two apart —
/// both sessions reasoned from a six-hour-old row for an hour before anyone checked the
/// timestamp in the database. A client that cannot say "since 13:44" will say "now" by omission.
///
/// There is deliberately no unstamped variant. `{code, message}` was the shape until this landed,
/// and leaving both would let a caller emit the ambiguous one by accident — the field is only
/// worth having if it is always there. It is additive, so a client that ignores the third key is
/// unaffected.
pub fn error_json_at(error: &Option<(String, String, i64)>) -> Value {
    match error {
        Some((code, message, at_ms)) => {
            json!({ "code": code, "message": message, "updatedAtMs": at_ms })
        }
        None => Value::Null,
    }
}

/// Ensure the account has its one computer and assign it (shared) to `coworker`. The account's
/// first agent creates the box and records it; every later agent reuses the same box. Applies the
/// assignment to `coworker`; returns the events to persist, the box id, and any error. Never raises.
/// The effective sharing mode for an account and its org: the account's OVERRIDE if set, else the
/// org DEFAULT, else the built-in default (per-account). Returns (mode, org_id).
pub async fn resolve_mode(state: &AgUiState, account_id: &AccountId) -> (String, Option<String>) {
    let org_id = account_org(state, account_id).await;
    if let Ok(Some(mode)) = state
        .auth
        .store
        .sharing_mode("account", account_id.as_str())
        .await
    {
        return (mode, org_id);
    }
    if let Some(org) = &org_id
        && let Ok(Some(mode)) = state.auth.store.sharing_mode("org", org).await
    {
        return (mode, org_id);
    }
    if let Ok(mode) = std::env::var("OG_BOX_SHARE") {
        match mode.as_str() {
            "per-bot" | "per-account" | "per-org" => return (mode, org_id),
            _ => {}
        }
    }
    ("per-account".to_string(), org_id)
}

/// The (scope, scope_id, box mode) a mode maps to: per-org shares one org box, per-account one box
/// per member, per-bot a dedicated box each. An account with no org falls back to account scope.
pub fn scope_for(
    mode: &str,
    account_id: &str,
    org_id: Option<&str>,
    coworker_id: &str,
    group: bool,
) -> (&'static str, String, BoxMode) {
    // A group has a computer of its own whatever the sharing mode: the room's shared desk, which
    // every member can reach next to its own box.
    if group {
        return ("group", coworker_id.to_string(), BoxMode::Shared);
    }
    match mode {
        "per-org" => match org_id {
            Some(org) => ("org", org.to_string(), BoxMode::Shared),
            None => ("account", account_id.to_string(), BoxMode::Shared),
        },
        "per-bot" => ("bot", coworker_id.to_string(), BoxMode::Dedicated),
        _ => ("account", account_id.to_string(), BoxMode::Shared),
    }
}

/// The scope a coworker's computer lives under, with everything `scope_for` needs looked up:
/// the account's mode and org, and whether the coworker is a group. The one call sites should
/// make when they have only an id.
pub async fn scope_of(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &str,
) -> (String, Option<String>, &'static str, String, BoxMode) {
    let (mode, org_id) = resolve_mode(state, account_id).await;
    let group = state
        .auth
        .store
        .load_coworker(&CoworkerId::from_stored(coworker_id.to_string()))
        .await
        .map(|(coworker, _)| coworker.is_group())
        .unwrap_or(false);
    let (scope, scope_id, box_mode) = scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id,
        group,
    );
    (mode, org_id, scope, scope_id, box_mode)
}

/// Whether a recorded box is gone from its provider — removed by hand, by a `docker system
/// prune`, or by a host that was rebuilt. Such a row is a dead pointer; the caller clears it and
/// provisions again. Only `absent` counts: a stopped box is asleep, not gone.
async fn box_is_gone(state: &AgUiState, org_id: Option<&str>, kind: &str, box_id: &str) -> bool {
    match provider_for(state, org_id, kind).await {
        Some(provider) => matches!(provider.state(box_id).await.as_deref(), Ok("absent")),
        None => false,
    }
}

/// Warm the box an account's coworkers will share, as soon as the account exists: per-org the
/// org's one box, per-account the member's own. Per-bot boxes come at hire. Best effort — a
/// signup never fails over a computer.
pub async fn warm_scope_for_account(state: &AgUiState, account_id: &AccountId) {
    let (mode, org_id) = resolve_mode(state, account_id).await;
    let (scope, scope_id) = match (mode.as_str(), org_id.as_deref()) {
        ("per-org", Some(org)) => ("org", org.to_string()),
        ("per-account", _) | ("per-org", None) => ("account", account_id.as_str().to_string()),
        _ => return,
    };
    let at_ms = chrono::Utc::now().timestamp_millis();
    match ensure_scope_box(state, org_id.as_deref(), scope, &scope_id, at_ms).await {
        Ok(box_id) => tracing::info!(
            scope,
            scope_id,
            box_id,
            "computer: warmed for a new account"
        ),
        Err((code, message)) => {
            tracing::warn!(
                scope,
                scope_id,
                code,
                message,
                "computer: could not warm for a new account"
            )
        }
    }
}

/// Ensure the computer for a coworker under its account's effective sharing mode, and assign it.
/// per-org: the whole org shares one box; per-account: one box per member; per-bot: a dedicated box.
/// The box is created on first need for its scope and reused after. Non-fatal — a failure leaves a
/// boxless coworker carrying the account-level error.
pub async fn ensure_computer_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    coworker: &mut Coworker,
    at_ms: i64,
) -> Provisioned {
    let store = &state.auth.store;
    let (mode, org_id) = resolve_mode(state, account_id).await;
    let (scope, scope_id, box_mode) = scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id.as_str(),
        coworker.is_group(),
    );

    // A recorded box whose container no longer exists is healed here rather than reported: the
    // row is cleared and the scope gets a new box below, so a bot whose computer vanished comes
    // back with one instead of a black screen forever.
    let recorded = match store.scoped_computer(scope, &scope_id).await {
        Ok(Some((box_id, kind))) => {
            if box_is_gone(state, org_id.as_deref(), &kind, &box_id).await {
                tracing::warn!(scope, scope_id = %scope_id, box_id, "computer: the recorded box is gone; provisioning a new one");
                let _ = store.clear_scoped_computer(scope, &scope_id).await;
                Ok(None)
            } else {
                Ok(Some(box_id))
            }
        }
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    };
    let mut fell_back = false;
    let box_id = match recorded {
        Ok(Some(box_id)) => box_id,
        Ok(None) => {
            let kind = kind_for_new(state, org_id.as_deref()).await;
            let Some(provider) = provider_for(state, org_id.as_deref(), kind).await else {
                let code = if kind == "none" {
                    "no_org_key"
                } else {
                    "not_supported"
                };
                return record_error(
                    state,
                    account_id,
                    code,
                    "no computer is configured for your organization — an admin must set up box.ascii.dev on the dashboard",
                    at_ms,
                )
                .await;
            };
            // NAME THE PROVIDER CALL. Until 6 Sep 2026 a create left no trace at all: the only
            // record was the error row it wrote on failure, so "did we ask box.ascii.dev today,
            // and what did it say" had no answer and an hours-old row got read as live. One line
            // before, one after, with the upstream code — the same gap as a request line that
            // logged only `auth_len`.
            tracing::info!(scope, scope_id = %scope_id, kind, "computer: asking the provider for a box");
            match provider.create(None).await {
                Ok(box_id) => {
                    tracing::info!(scope, scope_id = %scope_id, kind, box_id = %box_id, "computer: the provider gave us a box");
                    if let Err(error) = store
                        .set_scoped_computer(
                            scope,
                            &scope_id,
                            &box_id,
                            kind,
                            org_id.as_deref(),
                            at_ms,
                        )
                        .await
                    {
                        return record_error(
                            state,
                            account_id,
                            "unknown",
                            &error.to_string(),
                            at_ms,
                        )
                        .await;
                    }
                    box_id
                }
                Err(error) => {
                    if kind == "ascii"
                        && let Some((_, docker_id)) = take_over_with_local_docker(
                            state,
                            account_id,
                            scope,
                            &scope_id,
                            org_id.as_deref(),
                            &error,
                        )
                        .await
                    {
                        fell_back = true;
                        docker_id
                    } else {
                        return record_error(
                            state,
                            account_id,
                            error.code(),
                            &error.to_string(),
                            at_ms,
                        )
                        .await;
                    }
                }
            }
        }
        Err(error) => {
            return record_error(state, account_id, "unknown", &error.to_string(), at_ms).await;
        }
    };

    let box_id = BoxId::from_stored(box_id);
    let events = match coworker.decide(CoworkerCommand::AssignComputer {
        box_id: box_id.clone(),
        mode: box_mode,
        at_ms,
    }) {
        Ok(events) => {
            for event in &events {
                coworker.apply(event);
            }
            events
        }
        // A coworker that ALREADY has a box (a re-provision after reset) cannot be re-assigned — the
        // aggregate forbids it so a previous box is never silently stranded. That is not a failure
        // here: the scope's box was just (re)created and recorded, and the run path binds the SCOPE's
        // live box, not this frozen aggregate id, so the coworker follows the new box regardless. Keep
        // the existing assignment, report success with the scope's box.
        Err(CoworkerError::AlreadyHasComputer) => Vec::new(),
        Err(error) => {
            return record_error(state, account_id, "unknown", &error.to_string(), at_ms).await;
        }
    };
    // A takeover's stamp IS the news about this box; clearing it on success kept the swap silent.
    if !fell_back {
        let _ = store
            .clear_account_computer_error(account_id.as_str())
            .await;
    }
    Provisioned {
        events,
        box_id: Some(box_id),
        error: None,
    }
}

/// Ensure a scope's box EXISTS (create + record if absent), with no coworker assignment. This is the
/// eager-provisioning primitive: warm the one shared org box the moment an admin selects per-org, so
/// it is ready before anyone's first bot. Returns the box id, or `(code, message)` when it could not
/// be provisioned (e.g. the org has no key). Idempotent — an already-provisioned scope returns its
/// existing box.
pub async fn ensure_scope_box(
    state: &AgUiState,
    org_id: Option<&str>,
    scope: &str,
    scope_id: &str,
    at_ms: i64,
) -> Result<String, (String, String)> {
    let store = &state.auth.store;
    if let Ok(Some((box_id, _kind))) = store.scoped_computer(scope, scope_id).await {
        return Ok(box_id);
    }
    let kind = kind_for_new(state, org_id).await;
    let Some(provider) = provider_for(state, org_id, kind).await else {
        let code = if kind == "none" {
            "no_org_key"
        } else {
            "not_supported"
        };
        return Err((
            code.to_string(),
            "no computer is configured for your organization — set up box.ascii.dev first"
                .to_string(),
        ));
    };
    tracing::info!(
        scope,
        scope_id,
        kind,
        "computer: asking the provider for a box"
    );
    match provider.create(None).await {
        Ok(box_id) => {
            tracing::info!(scope, scope_id, kind, box_id = %box_id, "computer: the provider gave us a box");
            match store
                .set_scoped_computer(scope, scope_id, &box_id, kind, org_id, at_ms)
                .await
            {
                Ok(()) => Ok(box_id),
                Err(error) => Err(("unknown".to_string(), error.to_string())),
            }
        }
        Err(error) => {
            // The upstream refusal, in full, at WARN. This is the line whose absence meant a
            // `quota_exceeded` from hours earlier could not be told from one from a second ago.
            tracing::warn!(scope, scope_id, kind, code = %error.code(), %error, "computer: the provider refused");
            Err((error.code().to_string(), error.to_string()))
        }
    }
}

/// Destroy a scope's box (best-effort, on the provider that made it) and clear its mapping.
async fn destroy_and_clear(
    state: &AgUiState,
    org_id: Option<&str>,
    scope: &str,
    scope_id: &str,
    box_id: &str,
    kind: &str,
) {
    // A Local VM recorded before this server stopped serving them (OG_HOSTED=1 set later, or a
    // switch to box.ascii.dev) is still a container on this host; removing it is the one Docker
    // call such a server still makes, so teardown does not strand it running and unmanaged.
    let provider = match provider_for(state, org_id, kind).await {
        None if kind == "local-docker" => {
            Some(Arc::new(opengrok_box::DockerComputer::new()) as Arc<dyn Computer>)
        }
        provider => provider,
    };
    if let Some(provider) = provider
        && let Err(error) = provider.destroy(box_id).await
    {
        tracing::warn!(%error, box_id, "could not destroy a box on teardown; clearing the mapping anyway");
    }
    let _ = state
        .auth
        .store
        .clear_scoped_computer(scope, scope_id)
        .await;
}

/// Tear down a deleted coworker's computer according to its account's mode. per-bot: destroy the
/// bot's own box. per-account: destroy the account box once the account's last agent is gone.
/// per-org: leave the shared org box (idle-stop / an admin manages its lifetime). Call AFTER the
/// deletion is persisted.
pub async fn teardown_computer_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) {
    let store = &state.auth.store;
    let (mode, org_id) = resolve_mode(state, account_id).await;
    // A group's box is nobody else's; it goes when the group does.
    if let Ok((coworker, _)) = store.load_coworker(coworker_id).await
        && coworker.is_group()
    {
        if let Ok(Some((box_id, kind))) = store.scoped_computer("group", coworker_id.as_str()).await
        {
            destroy_and_clear(
                state,
                org_id.as_deref(),
                "group",
                coworker_id.as_str(),
                &box_id,
                &kind,
            )
            .await;
        }
        return;
    }
    match mode.as_str() {
        "per-bot" => {
            if let Ok(Some((box_id, kind))) =
                store.scoped_computer("bot", coworker_id.as_str()).await
            {
                destroy_and_clear(
                    state,
                    org_id.as_deref(),
                    "bot",
                    coworker_id.as_str(),
                    &box_id,
                    &kind,
                )
                .await;
            }
        }
        // The org box is shared org-wide; a single member's delete must not pull it out from under
        // everyone. Its lifetime is idle-stop and admin action, not agent deletion.
        "per-org" => {}
        _ => {
            let empty = store
                .coworkers_for(account_id)
                .await
                .map(|rows| rows.is_empty())
                .unwrap_or(false);
            if empty
                && let Ok(Some((box_id, kind))) =
                    store.scoped_computer("account", account_id.as_str()).await
            {
                destroy_and_clear(
                    state,
                    org_id.as_deref(),
                    "account",
                    account_id.as_str(),
                    &box_id,
                    &kind,
                )
                .await;
            }
        }
    }
}

/// NativeChat (tip `729bdd9`) gates Route traffic from **only** this JSON —
/// `isEgressTunnelAvailable` or nested `egress_tunnel.ready`. It does not call
/// Box `/v1/info`. Probe the scoped live `boxId` already in the payload (never
/// a frozen coworker-row id) on every status read. Do not invent `ready`.
fn stamp_egress_fields(screen: &mut Value, host_wants: bool, cap: Option<EgressTunnel>) {
    screen["isEgressTunnelAvailable"] = json!(EgressTunnel::advertised(host_wants, cap));
    if let Some(cap) = cap {
        screen["egress_tunnel"] = json!({
            "enabled": cap.enabled,
            "ready": cap.ready,
        });
    }
}

/// NativeChat places Route traffic chrome by computer **share**, not on every
/// bot pane. Internal `scope_for` stays `bot|account|group|org`; sharing-mode
/// policy stays `per-bot|per-account|per-org`. This is the client-facing
/// placement name (`dedicated` = bot sidebar, `user` = Settings→Computer,
/// `group` = group sidebar, `org` = admin console).
pub fn share_scope_of(scope: &str) -> &'static str {
    match scope {
        "bot" => "dedicated",
        "account" => "user",
        "group" => "group",
        "org" => "org",
        // Unknown internal scope must not paint dedicated bot-pane chrome.
        _ => "user",
    }
}

/// The standing answer for a computer scope; no row is `ask`. `Err` when the store could not
/// answer — the caller decides what that means where it stands: the run path fails closed,
/// the pane says nothing, the GET says 503.
pub async fn egress_policy_read(
    state: &AgUiState,
    scope: &str,
    scope_id: &str,
) -> Result<opengrok_tools::EgressPolicy, ()> {
    match state.auth.store.egress_policy_mode(scope, scope_id).await {
        Ok(mode) => Ok(mode
            .map(|mode| opengrok_tools::EgressPolicy::from_stored(&mode))
            .unwrap_or_default()),
        Err(error) => {
            tracing::warn!(%error, scope, scope_id, "could not read the egress policy");
            Err(())
        }
    }
}

/// The policy for a turn, and whether it is a stand-in. A store that cannot answer fails
/// CLOSED: `never` for this turn. `ask` would still put the tunnel card in front of a person
/// for the screen tools, but the two login hand-offs have no card of their own — under `ask`
/// a stored `never` would let a person type a site password into a box whose traffic then
/// leaves through their network. One turn of a withheld browser during a store error is the
/// cheaper wrong, and the `unconfirmed` flag keeps the prompt from calling it the person's
/// choice.
pub async fn egress_policy_for_turn(
    state: &AgUiState,
    scope: &str,
    scope_id: &str,
) -> (opengrok_tools::EgressPolicy, bool) {
    match egress_policy_read(state, scope, scope_id).await {
        Ok(policy) => (policy, false),
        Err(()) => (opengrok_tools::EgressPolicy::Never, true),
    }
}

/// The Computer pane carries the choice next to `shareScope`, so the client can paint the
/// control without a second request. Nothing is stamped when the store cannot answer: the
/// client hides the control rather than showing a word nobody chose.
async fn stamp_egress_policy(state: &AgUiState, screen: &mut Value, scope: &str, scope_id: &str) {
    if let Ok(policy) = egress_policy_read(state, scope, scope_id).await {
        screen["egressPolicy"] = json!(policy.as_stored());
    }
}

fn stamp_share_scope(screen: &mut Value, scope: &str, scope_id: &str) {
    screen["shareScope"] = json!(share_scope_of(scope));
    if scope == "group" {
        screen["groupId"] = json!(scope_id);
    }
}

/// A coworker's live box, found the one way every door must find it: the account's sharing
/// mode, then the scope that mode puts this coworker in, then that scope's recorded box and the
/// provider of its kind. The id frozen on the coworker's own row is NOT consulted: after an
/// update, a heal or a takeover it names a container that no longer exists (four of the dev
/// account's coworkers carried `6c21ce5cd833` while the account's box was `box-box-1`, 21 Sep
/// 2026), and host-settings, which read it, said the tunnel was off for a box whose guest said
/// it was on.
pub struct ScopedBox {
    pub scope: &'static str,
    pub scope_id: String,
    pub box_id: String,
    pub kind: String,
    pub stopped: bool,
    pub org_id: Option<String>,
    pub computer: Arc<dyn Computer>,
}

/// `None` when the coworker has no box in its scope or its kind has no provider here. The
/// Computer pane (`coworker_screen`) walks the same steps itself because it has to say WHY
/// each one failed; the callers here (host-settings, the egress policy) only need the box.
pub async fn scoped_box_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Option<ScopedBox> {
    let row = scoped_box_row_for(state, account_id, coworker_id).await?;
    let computer = lookup_provider(state, row.org_id.as_deref(), &row.kind)
        .await
        .computer?;
    Some(ScopedBox {
        scope: row.scope,
        scope_id: row.scope_id,
        box_id: row.box_id,
        kind: row.kind,
        stopped: row.stopped,
        org_id: row.org_id,
        computer,
    })
}

/// The recorded box of a coworker's scope, without its provider. What a setting keyed by the
/// scope needs: a preference about a box is still writable when the box's provider cannot be
/// built right now (an org key sealed under a rotated KEK), and the Computer pane paints the
/// control in exactly that state.
pub struct ScopedBoxRow {
    pub scope: &'static str,
    pub scope_id: String,
    pub box_id: String,
    pub kind: String,
    pub stopped: bool,
    pub org_id: Option<String>,
}

pub async fn scoped_box_row_for(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Option<ScopedBoxRow> {
    let (_, org_id, scope, scope_id, _) = scope_of(state, account_id, coworker_id.as_str()).await;
    let (box_id, kind, stopped) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
        .ok()
        .flatten()?;
    Some(ScopedBoxRow {
        scope,
        scope_id,
        box_id,
        kind,
        stopped,
        org_id,
    })
}

/// Live screen NativeChat paints. Same facts as gateway `getForeverBoxStatus`, on the AG-UI
/// cookie session so the desktop client does not need the host gateway bearer — plus live
/// egress capability and `shareScope` (where Route traffic chrome belongs). `shareScope` is
/// omitted on pure `state: absent` (no scoped computer); NativeChat hides the control then.
pub async fn coworker_screen(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Value {
    let agent_id = coworker_id.as_str();
    let host_wants = state.egress_tunnel_enabled();
    let absent = || {
        let mut body = json!({
            "agentId": agent_id,
            "state": "absent",
            "vncUrl": Value::Null,
        });
        stamp_egress_fields(&mut body, host_wants, None);
        body
    };
    let group = match state.auth.store.load_coworker(coworker_id).await {
        Ok((coworker, _)) if !coworker.name.is_empty() => coworker.is_group(),
        _ => return absent(),
    };
    let (mode, org_id) = resolve_mode(state, account_id).await;
    let (scope, scope_id, _) = scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        agent_id,
        group,
    );
    let Ok(Some((box_id, kind, stopped))) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
    else {
        if let Ok(Some((code, message, at_ms))) = state
            .auth
            .store
            .account_computer_error(account_id.as_str())
            .await
        {
            let mut body = json!({
                "agentId": agent_id,
                "state": "absent",
                "vncUrl": Value::Null,
                "computerError": { "code": code, "message": message, "updatedAtMs": at_ms },
            });
            stamp_egress_fields(&mut body, host_wants, None);
            return body;
        }
        return absent();
    };
    let lookup = lookup_provider(state, org_id.as_deref(), &kind).await;
    let Some(provider) = lookup.computer else {
        let (code, message) = lookup.error.unwrap_or_else(|| {
            (
                "unknown".into(),
                "the computer's provider is not available".into(),
            )
        });
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as i64)
            .unwrap_or(0);
        let mut body = json!({
            "agentId": agent_id,
            "state": if stopped { "stopped" } else { "unknown" },
            "vncUrl": Value::Null,
            "computerError": { "code": code, "message": message, "updatedAtMs": at_ms },
        });
        stamp_egress_fields(&mut body, host_wants, None);
        stamp_share_scope(&mut body, scope, &scope_id);
        stamp_egress_policy(state, &mut body, scope, &scope_id).await;
        return body;
    };
    let live_state = provider
        .state(&box_id)
        .await
        .unwrap_or_else(|_| "unknown".to_string());
    let (mut vnc_url, image) = if live_state == "running" {
        (
            provider.screen_url(&box_id).await.ok().flatten(),
            provider.image_status(&box_id).await.ok(),
        )
    } else {
        (None, None)
    };
    // A Local VM's page is on this host's loopback, which the person's app — on another machine
    // whenever the gateway is not loopback — cannot open; it is served through this server. No
    // reachable origin means no live screen rather than a URL that cannot load.
    if kind == "local-docker" {
        vnc_url = vnc_url.and_then(|local| {
            let origin = super::screen_proxy::public_origin(&state.auth.public_url, headers)?;
            let now = chrono::Utc::now().timestamp();
            super::screen_proxy::proxied_page(
                &state.auth.minter,
                &origin,
                account_id,
                coworker_id,
                &box_id,
                &local,
                now,
            )
        });
    }
    // Live `/v1/info` of THIS scoped box — NativeChat never probes the guest itself.
    let cap = provider.egress_tunnel(&box_id).await;
    let mut screen = json!({
        "agentId": agent_id,
        "state": live_state,
        "vncUrl": vnc_url,
        // The scope's live box, not the id frozen on the coworker's row: after an update or a
        // heal they differ, and the live one is the one a person is looking at.
        "boxId": box_id,
        "update": update_status(state, scope, &scope_id).await,
        "image": image.map(|image| json!({
            "running": image.running,
            "latest": image.latest,
            "stale": image.stale(),
        })),
    });
    // A Local VM with a TAKEOVER stamp beside it is a takeover's box: the stamp says the box
    // changed and why. Only that stamp — the account's error row also holds other scopes' failed
    // hires (a per-bot quota refusal), which beside a healthy Local VM would read as its fault.
    // Additive — `agentId`/`state`/`vncUrl` are what the renderer validates.
    if kind == "local-docker"
        && let Ok(Some((code, message, at_ms))) = state
            .auth
            .store
            .account_computer_error(account_id.as_str())
            .await
        && message.starts_with(FELL_BACK)
    {
        screen["computerError"] = json!({ "code": code, "message": message, "updatedAtMs": at_ms });
    }
    stamp_egress_fields(&mut screen, host_wants, cap);
    stamp_share_scope(&mut screen, scope, &scope_id);
    stamp_egress_policy(state, &mut screen, scope, &scope_id).await;
    screen
}

/// The coworker's screen right now, as the box's PNG — what the Computer pane paints in its
/// tile. Resolved the same way as `coworker_screen`; a box without a display is a 404 the
/// client can stop asking about, not a failure.
pub async fn coworker_screenshot(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<opengrok_box::Screenshot, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let group = match state.auth.store.load_coworker(coworker_id).await {
        Ok((coworker, _)) if !coworker.name.is_empty() => coworker.is_group(),
        _ => return Err((StatusCode::NOT_FOUND, "no such coworker".into())),
    };
    let (mode, org_id) = resolve_mode(state, account_id).await;
    let (scope, scope_id, _) = scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id.as_str(),
        group,
    );
    let Ok(Some((box_id, kind, _stopped))) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
    else {
        return Err((
            StatusCode::NOT_FOUND,
            "this coworker has no computer".into(),
        ));
    };
    let Some(provider) = lookup_provider(state, org_id.as_deref(), &kind)
        .await
        .computer
    else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "the computer's provider is not available".into(),
        ));
    };
    provider
        .screenshot(&box_id)
        .await
        .map_err(|error| match error {
            opengrok_box::BoxError::Refused { status: 501, .. } => {
                (StatusCode::NOT_FOUND, "this computer has no screen".into())
            }
            other => (StatusCode::SERVICE_UNAVAILABLE, other.to_string()),
        })
}

/// How long an update waits for the new box to come up and show a screen.
const UPDATE_WAKE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(90);
const UPDATE_SCREEN_PATIENCE: std::time::Duration = std::time::Duration::from_secs(30);

/// An update older than this with no progress is a crashed one; a new attempt may replace it.
const UPDATE_STALE_AFTER_MS: i64 = 15 * 60 * 1000;

/// The phases an update passes through, as the pane shows them. `failed` keeps its reason.
pub const UPDATE_PULLING: &str = "pulling";
pub const UPDATE_TRANSFERRING: &str = "transferring";
pub const UPDATE_STARTING: &str = "starting";
pub const UPDATE_FAILED: &str = "failed";

/// Poll until the box's screen is reachable or patience runs out; the caller reports either way.
pub async fn wait_for_screen(provider: &dyn Computer, box_id: &str, patience: std::time::Duration) {
    let started = std::time::Instant::now();
    loop {
        if let Ok(Some(_)) = provider.screen_url(box_id).await {
            return;
        }
        if started.elapsed() >= patience {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

/// The update record as the client sees it, or `Null` when nothing is in flight.
pub async fn update_status(state: &AgUiState, scope: &str, scope_id: &str) -> Value {
    match state.auth.store.box_update(scope, scope_id).await {
        Ok(Some((phase, started_at_ms, updated_at_ms, error))) => json!({
            "phase": phase,
            "startedAtMs": started_at_ms,
            "updatedAtMs": updated_at_ms,
            "error": error,
        }),
        _ => Value::Null,
    }
}

/// Rebuild a scope's box on the provider's newest image, keeping its data: pull, recreate on the
/// same volumes, record the new id, wait for it to come up. Every step writes its phase so the
/// pane can say what is happening; a failure is recorded with its reason and the old box — still
/// there, stopped at worst — stays the scope's computer.
pub async fn update_scope_box(
    state: AgUiState,
    org_id: Option<String>,
    scope: &'static str,
    scope_id: String,
) {
    let store = state.auth.store.clone();
    let fail = |why: String| {
        let store = store.clone();
        let scope_id = scope_id.clone();
        async move {
            tracing::warn!(scope, scope_id = %scope_id, why, "computer: update failed");
            let _ = store
                .set_box_update_phase(
                    scope,
                    &scope_id,
                    UPDATE_FAILED,
                    Some(&why),
                    chrono::Utc::now().timestamp_millis(),
                )
                .await;
        }
    };
    let Ok(Some((old_box_id, kind, _stopped))) = store.scoped_computer_full(scope, &scope_id).await
    else {
        return fail("this scope has no computer to update".into()).await;
    };
    let Some(provider) = provider_for(&state, org_id.as_deref(), &kind).await else {
        return fail("the computer's provider is not available".into()).await;
    };

    if let Err(error) = provider.pull_latest().await {
        return fail(format!("could not fetch the newest image: {error}")).await;
    }

    let _ = store
        .set_box_update_phase(
            scope,
            &scope_id,
            UPDATE_TRANSFERRING,
            None,
            chrono::Utc::now().timestamp_millis(),
        )
        .await;
    let new_box_id = match provider.recreate(&old_box_id).await {
        Ok(id) => id,
        Err(error) => {
            // The old box was stopped for the copy; bring it back so the person is not left
            // with nothing.
            let _ = provider.resume(&old_box_id).await;
            return fail(format!("could not rebuild the computer: {error}")).await;
        }
    };
    let at_ms = chrono::Utc::now().timestamp_millis();
    if let Err(error) = store
        .set_scoped_computer(
            scope,
            &scope_id,
            &new_box_id,
            &kind,
            org_id.as_deref(),
            at_ms,
        )
        .await
    {
        return fail(format!("the new computer could not be recorded: {error}")).await;
    }
    tracing::info!(scope, scope_id = %scope_id, old = %old_box_id, new = %new_box_id, "computer: rebuilt on the newest image");

    let _ = store
        .set_box_update_phase(scope, &scope_id, UPDATE_STARTING, None, at_ms)
        .await;
    match provider.wake(&new_box_id, UPDATE_WAKE_PATIENCE).await {
        Ok(reached) if reached == "running" => {
            wait_for_screen(provider.as_ref(), &new_box_id, UPDATE_SCREEN_PATIENCE).await;
        }
        Ok(reached) => {
            return fail(format!("the new computer is {reached}, not running")).await;
        }
        Err(error) => return fail(format!("the new computer did not start: {error}")).await,
    }
    let _ = store.clear_box_update(scope, &scope_id).await;
}

/// Start an update of a coworker's computer in the background and answer at once; the status
/// route carries the phases. Refuses while one is already running.
pub async fn begin_update_for_coworker(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let (_mode, org_id, scope, scope_id, _) =
        scope_of(state, account_id, coworker_id.as_str()).await;
    let store = &state.auth.store;
    if !matches!(store.scoped_computer(scope, &scope_id).await, Ok(Some(_))) {
        return Err((
            StatusCode::NOT_FOUND,
            "this coworker has no computer to update".into(),
        ));
    }
    let now = chrono::Utc::now().timestamp_millis();
    if let Ok(Some((phase, _started, updated_at_ms, _))) = store.box_update(scope, &scope_id).await
        && phase != UPDATE_FAILED
        && now - updated_at_ms < UPDATE_STALE_AFTER_MS
    {
        return Err((
            StatusCode::CONFLICT,
            "this computer is already being updated".into(),
        ));
    }
    if let Err(error) = store
        .begin_box_update(scope, &scope_id, UPDATE_PULLING, now)
        .await
    {
        return Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()));
    }
    tokio::spawn(update_scope_box(state.clone(), org_id, scope, scope_id));
    Ok(())
}

/// Destroy a coworker's computer, data and all, and provision a fresh one in its place.
pub async fn reset_for_coworker(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), (String, String)> {
    let (_mode, org_id, scope, scope_id, _) =
        scope_of(state, account_id, coworker_id.as_str()).await;
    let store = &state.auth.store;
    if let Ok(Some((box_id, kind, _))) = store.scoped_computer_full(scope, &scope_id).await
        && let Some(provider) = provider_for(state, org_id.as_deref(), &kind).await
    {
        let _ = provider.destroy(&box_id).await;
    }
    let _ = store.clear_scoped_computer(scope, &scope_id).await;
    let _ = store.clear_box_update(scope, &scope_id).await;
    reprovision_coworker(state, account_id, coworker_id).await
}

/// Provision (or re-provision) a coworker's box and PERSIST the assignment to its aggregate, so
/// a fresh coworker row points at a box even though the run path binds the scope's live one.
pub async fn reprovision_coworker(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<(), (String, String)> {
    use opengrok_core::coworker::CoworkerView;

    let Ok((mut coworker, seq)) = state.auth.store.load_coworker(coworker_id).await else {
        return Err(("unknown".into(), "could not load the coworker".into()));
    };
    let at_ms = chrono::Utc::now().timestamp_millis();
    let provisioned =
        ensure_computer_for(state, account_id, coworker_id, &mut coworker, at_ms).await;
    if let Some(error) = provisioned.error {
        return Err(error);
    }
    if provisioned.events.is_empty() {
        return Ok(());
    }
    let view = CoworkerView {
        id: coworker_id.clone(),
        name: coworker.name.clone(),
        model: coworker.model.clone(),
        box_id: coworker.computer().cloned(),
        retired: false,
        // Carried, not blanked: a group reprovisioned after hire keeps its members.
        members: coworker.members.clone(),
        updated_at_ms: at_ms,
        role: coworker.role.clone(),
        visibility: coworker.visibility,
    };
    let _ = state
        .auth
        .store
        .append_coworker(coworker_id, account_id, seq, &provisioned.events, &view)
        .await;
    Ok(())
}

/// Wake a stopped scope box so Open can show its screen. Best-effort: a missing mapping is
/// `coworker_screen`'s absent, not a failure here.
pub async fn wake_coworker_computer(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) {
    let (_mode, org_id, scope, scope_id, _) =
        scope_of(state, account_id, coworker_id.as_str()).await;
    let Ok(Some((box_id, kind, _stopped))) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
    else {
        return;
    };
    let Some(provider) = lookup_provider(state, org_id.as_deref(), &kind)
        .await
        .computer
    else {
        return;
    };
    let _ = provider
        .wake(&box_id, std::time::Duration::from_secs(90))
        .await;
}

/// How long a box may sit idle before the sweep stops it (disk kept, billing paused). Read from
/// `OG_BOX_IDLE_STOP_SECONDS`; `0` (the default) disables idle-stop entirely.
fn idle_stop_seconds() -> i64 {
    std::env::var("OG_BOX_IDLE_STOP_SECONDS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
}

/// Stop boxes idle past the threshold, forever. A stopped box keeps its disk and pauses billing; the
/// run path (`tools_for_coworker`) resumes it on next use and refreshes its last-used stamp. Active
/// only when `OG_BOX_IDLE_STOP_SECONDS > 0`; the sweep interval is a quarter of the threshold,
/// clamped to [30s, 300s]. A box never used yet (no last-used stamp) is left alone. Each box is
/// stopped on the SAME provider that made it, rebuilt from the org id recorded on its row.
pub async fn idle_stop_forever(state: AgUiState) {
    let idle_seconds = idle_stop_seconds();
    if idle_seconds <= 0 {
        tracing::info!("idle-stop is off (set OG_BOX_IDLE_STOP_SECONDS to enable)");
        return;
    }
    let interval = (idle_seconds / 4).clamp(30, 300) as u64;
    tracing::info!(
        idle_seconds,
        interval,
        "idle-stop sweep running: idle boxes will be stopped (disk kept)"
    );
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval));
    loop {
        ticker.tick().await;
        let before = chrono::Utc::now().timestamp_millis() - idle_seconds * 1000;
        idle_stop_once(&state, before).await;
    }
}

/// One idle-stop pass: stop every box idle since before `before_ms` and mark it stopped. Returns how
/// many were stopped. Factored out of the forever loop so a test can drive a single deterministic
/// sweep. Best-effort per box — a stop failure is logged and the box left running for the next pass.
pub async fn idle_stop_once(state: &AgUiState, before_ms: i64) -> usize {
    let idle = match state.auth.store.idle_scoped_computers(before_ms).await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "idle-stop sweep could not list idle boxes");
            return 0;
        }
    };
    let mut stopped = 0;
    for (scope, scope_id, box_id, kind, org_id) in idle {
        let Some(provider) = provider_for(state, org_id.as_deref(), &kind).await else {
            continue;
        };
        match provider.stop(&box_id).await {
            Ok(()) => {
                let _ = state
                    .auth
                    .store
                    .mark_scoped_stopped(&scope, &scope_id)
                    .await;
                stopped += 1;
                tracing::info!(
                    box_id,
                    scope,
                    "stopped an idle box (disk kept, billing paused)"
                );
            }
            Err(error) => {
                tracing::warn!(%error, box_id, "could not stop an idle box");
            }
        }
    }
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_its_own_scope_whatever_the_sharing_mode() {
        for mode in ["per-org", "per-account", "per-bot"] {
            let (scope, id, box_mode) = scope_for(mode, "acct_1", Some("org_1"), "cw_room", true);
            assert_eq!(
                (scope, id.as_str(), box_mode),
                ("group", "cw_room", BoxMode::Shared),
                "{mode}"
            );
        }
    }

    #[test]
    fn a_plain_coworker_follows_the_mode() {
        assert_eq!(
            scope_for("per-bot", "acct_1", Some("org_1"), "cw_1", false),
            ("bot", "cw_1".to_string(), BoxMode::Dedicated)
        );
        assert_eq!(
            scope_for("per-account", "acct_1", Some("org_1"), "cw_1", false),
            ("account", "acct_1".to_string(), BoxMode::Shared)
        );
        assert_eq!(
            scope_for("per-org", "acct_1", Some("org_1"), "cw_1", false),
            ("org", "org_1".to_string(), BoxMode::Shared)
        );
        // No org to share: the member's own box stands in.
        assert_eq!(
            scope_for("per-org", "acct_1", None, "cw_1", false),
            ("account", "acct_1".to_string(), BoxMode::Shared)
        );
    }

    #[test]
    fn computer_json_stamps_live_egress_when_the_guest_is_ready() {
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_egress_fields(
            &mut screen,
            true,
            Some(EgressTunnel {
                enabled: true,
                ready: true,
            }),
        );
        assert_eq!(screen["isEgressTunnelAvailable"], true);
        assert_eq!(screen["egress_tunnel"]["enabled"], true);
        assert_eq!(screen["egress_tunnel"]["ready"], true);
    }

    #[test]
    fn computer_json_available_is_false_when_the_guest_is_not_ready() {
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_egress_fields(
            &mut screen,
            true,
            Some(EgressTunnel {
                enabled: true,
                ready: false,
            }),
        );
        assert_eq!(screen["isEgressTunnelAvailable"], false);
        assert_eq!(screen["egress_tunnel"]["ready"], false);
        assert_eq!(screen["egress_tunnel"]["enabled"], true);
    }

    #[test]
    fn computer_json_does_not_invent_ready_when_info_is_missing() {
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_egress_fields(&mut screen, true, None);
        assert_eq!(screen["isEgressTunnelAvailable"], false);
        assert!(
            screen.get("egress_tunnel").is_none(),
            "missing /v1/info must not invent a nested capability: {screen}"
        );
    }

    #[test]
    fn share_scope_maps_internal_scope_to_client_placement() {
        assert_eq!(share_scope_of("bot"), "dedicated");
        assert_eq!(share_scope_of("account"), "user");
        assert_eq!(share_scope_of("group"), "group");
        assert_eq!(share_scope_of("org"), "org");
        let (scope, id, _) = scope_for("per-bot", "acct_1", Some("org_1"), "cw_1", false);
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_share_scope(&mut screen, scope, &id);
        assert_eq!(screen["shareScope"], "dedicated");
        assert!(screen.get("groupId").is_none(), "{screen}");

        let (scope, id, _) = scope_for("per-account", "acct_1", Some("org_1"), "cw_1", false);
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_share_scope(&mut screen, scope, &id);
        assert_eq!(screen["shareScope"], "user");
        assert!(screen.get("groupId").is_none(), "{screen}");

        let (scope, id, _) = scope_for("per-org", "acct_1", Some("org_1"), "cw_1", false);
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_share_scope(&mut screen, scope, &id);
        assert_eq!(screen["shareScope"], "org");
        assert!(screen.get("groupId").is_none(), "{screen}");

        let (scope, id, _) = scope_for("per-org", "acct_1", None, "cw_1", false);
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_share_scope(&mut screen, scope, &id);
        assert_eq!(
            screen["shareScope"], "user",
            "per-org with no org falls back to account/user, not org: {screen}"
        );

        let (scope, id, _) = scope_for("per-account", "acct_1", Some("org_1"), "cw_room", true);
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_share_scope(&mut screen, scope, &id);
        assert_eq!(screen["shareScope"], "group");
        assert_eq!(screen["groupId"], "cw_room");
    }

    #[test]
    fn absent_computer_json_omits_share_scope_and_does_not_fake_ready() {
        let mut absent = json!({
            "agentId": "cw_1",
            "state": "absent",
            "vncUrl": Value::Null,
        });
        stamp_egress_fields(&mut absent, true, None);
        assert_eq!(absent["isEgressTunnelAvailable"], false);
        assert!(absent.get("egress_tunnel").is_none(), "{absent}");
        assert!(
            absent.get("shareScope").is_none(),
            "unprovisioned omits shareScope; client hides the control: {absent}"
        );
        assert!(absent.get("groupId").is_none(), "{absent}");
    }

    #[test]
    fn share_scope_stamp_leaves_egress_fields_in_place() {
        let mut screen = json!({ "boxId": "bx_live" });
        stamp_egress_fields(
            &mut screen,
            true,
            Some(EgressTunnel {
                enabled: true,
                ready: true,
            }),
        );
        stamp_share_scope(&mut screen, "account", "acct_1");
        assert_eq!(screen["isEgressTunnelAvailable"], true);
        assert_eq!(screen["egress_tunnel"]["ready"], true);
        assert_eq!(screen["shareScope"], "user");
    }
}
