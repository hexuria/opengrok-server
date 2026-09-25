//! `submitUserForm` / `dismissUserForm` / box-handoff resolve — gateway verbs and AG-UI REST twins.
//!
//! WHY THIS IS NOT A SECRET DROP. The desktop's `submitSecret` (deleted with seam A) stamped
//! `secretProvided` and DROPPED the value into the connector vault. This path types into the live
//! page. Mixing the two would either vault a Google password or fill a connector secret into
//! Chromium — the reason they were never one path, and the reason this one did not follow that
//! verb out.
//!
//! WHY ESCALATE IS NOT A RESUME. Official `dismissUserForm` mode `escalated` is Grok Bot
//! "Open the screen": emit a **separate** `sand://box` attachment with `boxRequestId` and keep
//! screen-hold until hand-back or decline. Resuming immediately (and clearing hold) races the
//! person on the computer — the Facebook hang after password when phone verify hits. This is
//! not OpenGrok Take over / I'm done / Skip, and it is not `handBackForeverBox` (lifecycle stop).
//!
//! SKIP MAY POST THE FORM ID. NativeChat KeepAlive prefers `handoffEntryId` from the escalate
//! response, then falls back to the form gateway `entryId`. That form never carries
//! `boxRequestId` (a stray one converts the card into a handoff). Resolve must therefore
//! find and settle live `sand://box` siblings when the posted id is the escalated form,
//! or chrome stays "Waiting for you" on a still-suspended UserForm run. Same for
//! `dismissUserForm` mode `dismissed` on an already-escalated form: that is abandon, not
//! an escalate retry.
//!
//! NativeChat talks AG-UI with an account bearer, not the gateway host bearer, so the REST
//! twins live on a router that has `HostState` (live emit + resume) but authenticates
//! like AG-UI (`account_from_bearer`), never through `refuse()`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_core::run::{RunCommand, RunView};
use opengrok_tools::user_form::{
    FieldOutcome, FormRequest, FormResolution, HAND_BACK_TOOL_RESULT, HANDOFF_DECLINED_TOOL_RESULT,
    HOLD_TIMED_OUT_TOOL_RESULT, audit_lengths, fill_into_focus, form_request_from,
    handoff_instruction, is_live_handoff, is_unresolved, is_user_form_entry, model_facing_result,
    overall_resolution, shared_values, submitted_values, tool_result_content,
};
use opengrok_wire::agui::{Event, EventType};
use serde_json::{Value, json};

use super::resume;
use crate::host_state::HostState;

/// How long an unanswered form or live handoff may block the turn, counted from the card's own
/// `timestampMs`. Tests pass a later `now` to the settlers rather than waiting this out. Facebook
/// hang: password fill reported submitted and the OTP wait never ended.
pub const FORM_HOLD_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// How far the deadline sweep's window trails the deadline. A run's park is written a moment
/// before its card, so by the time the park is this far past the deadline its cards are too.
const MINT_LAG_MS: i64 = 60_000;

fn hold_ms() -> i64 {
    i64::try_from(FORM_HOLD_TIMEOUT.as_millis()).unwrap_or(i64::MAX)
}

/// When a card was put up. A card with no stamp never reaches its deadline; if it is dead, its
/// run says so.
fn minted_at(entry: &Value) -> i64 {
    entry
        .get("timestampMs")
        .and_then(Value::as_i64)
        .unwrap_or(i64::MAX)
}

pub fn agui_router(state: HostState) -> Router {
    Router::new()
        .route("/ag-ui/user-form/submit", post(agui_submit))
        .route("/ag-ui/user-form/dismiss", post(agui_dismiss))
        .route("/ag-ui/box-handoff/resolve", post(agui_resolve_handoff))
        .with_state(state)
}

async fn agui_submit(
    State(state): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = submit_user_form(&state, &args, &account_id).await;
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

async fn agui_dismiss(
    State(state): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = dismiss_user_form(&state, &args, &account_id).await;
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

async fn agui_resolve_handoff(
    State(state): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = crate::agui::routes::account_from_bearer(&state.agui, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let (code, body) = resolve_box_handoff(&state, &args, &account_id).await;
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

/// `submitUserForm {entryId, values, agentId, platform?}`. Types into the box, settles
/// `formResolution` plus `formFieldOutcomes`, resumes with a secret-free tool result that says
/// the values were filled into the page — not that login succeeded.
pub async fn submit_user_form(
    state: &HostState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let (seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    if !is_user_form_entry(&entry) {
        return (400, json!({ "error": "that entry is not a user-form" }));
    }
    if !is_unresolved(&entry) {
        return heal_or_already(state, account_id, &coworker_id, &entry).await;
    }

    let form = form_request_from(&entry);
    let collect = opengrok_tools::user_form::is_chat_collection(&entry);
    // TYPED ONLY WHILE ITS RUN WAITS ON IT. A card outlives its run on screen — the run was
    // stopped, or a twin from the same completion was answered and the run moved on — and typing
    // then put an email into whatever field the page had focused. A card that names no call
    // cannot be tied to a run and types nothing either. A collect card types nothing anyway. A
    // stop landing between this check and the typing still types: the window is that interval,
    // no longer the card's whole life.
    if !collect && !waits_on(state, account_id, &coworker_id, call_id_of(&entry)).await {
        let settled = settle_entry(
            entry,
            FormResolution::FillFailed,
            &BTreeMap::new(),
            false,
            &nothing_filled(&form),
            false,
        );
        if let Err(error) = state
            .agui
            .auth
            .store
            .update_gateway_entry(&coworker_id, account_id, seq, &settled)
            .await
        {
            tracing::error!(%error, "could not settle a user-form nothing waits on");
            return (500, json!({ "error": "transcript unavailable" }));
        }
        return (200, settled);
    }
    let values = submitted_values(&form, args.get("values").unwrap_or(&Value::Null));
    audit_lengths(&form, &values);
    let saved_login = is_saved_login(args);
    // A passkey card has no fields to type: the person's passkey is loaded into the page (or
    // an empty holder is, for a site that offers to make one), and the bot is told to click.
    let passkey_card = form.challenge_kind.as_deref() == Some("passkey");
    if (saved_login || passkey_card)
        && !fills_a_dedicated_box(state, account_id, &coworker_id).await
    {
        // The card stays open: the person may still type by hand, or dismiss.
        return (
            403,
            json!({ "error": SHARED_COMPUTER, "message": SHARED_COMPUTER_MESSAGE }),
        );
    }
    // A saved login is secret whatever the model called its fields. The initial result
    // and persisted answers must use the same suppression, including on collect cards.
    let shared = if saved_login || passkey_card {
        BTreeMap::new()
    } else {
        shared_values(&form, &values)
    };
    let (outcomes, resolution, content) = if passkey_card {
        let told = if form.passkey_mode.as_deref() == Some("register") {
            let hint = args
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            super::passkeys::register_passkey(state, account_id, &coworker_id, &form, &hint).await
        } else {
            match args.get("savedLoginId").and_then(Value::as_str) {
                Some(login_id) => {
                    super::passkeys::use_passkey(state, account_id, &coworker_id, &form, login_id)
                        .await
                }
                None => Err("no passkey was chosen".to_string()),
            }
        };
        match told {
            Ok(sentence) => (Vec::new(), FormResolution::Submitted, sentence),
            Err(why) => (
                Vec::new(),
                FormResolution::FillFailed,
                format!(
                    "The passkey could not be readied: {why}. The person may pick another way \
                     in; do not type a password."
                ),
            ),
        }
    } else if collect {
        let content = opengrok_tools::user_form::collection_tool_result(&form, &shared);
        (Vec::new(), FormResolution::Submitted, content)
    } else {
        let outcomes = fill_on_box(state, account_id, &coworker_id, &form, &values).await;
        let resolution = overall_resolution(&outcomes);
        let content = tool_result_content(&form, resolution, &shared, false);
        (outcomes, resolution, content)
    };

    let settled = settle_entry(entry, resolution, &shared, false, &outcomes, false);
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, account_id, seq, &settled)
        .await
    {
        tracing::error!(%error, "could not settle a user-form entry");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    journal_settled_form(state, account_id, &coworker_id, &settled).await;
    // A passkey's use is stamped when the site's challenge is signed, not when it is loaded.
    if resolution == FormResolution::Submitted
        && saved_login
        && !passkey_card
        && let Some(login_id) = args.get("savedLoginId").and_then(Value::as_str)
        && let Err(error) = state
            .agui
            .auth
            .store
            .touch_site_login_used(account_id, login_id, chrono::Utc::now().timestamp_millis())
            .await
    {
        tracing::warn!(%error, "could not stamp a site login's last use");
    }
    // A login that came from the vault is not offered to the vault again, and a passkey card
    // typed nothing worth saving.
    if resolution == FormResolution::Submitted && !saved_login && !passkey_card && !collect {
        super::credential::offer_save_after_submit(
            state,
            account_id,
            &coworker_id,
            call_id_of(&settled),
            &entry_id,
            &form,
            &shared,
        )
        .await;
    }

    resume_user_form(
        state,
        account_id,
        &coworker_id,
        content,
        call_id_of(&settled),
    )
    .await;
    (200, settled)
}

/// `dismissUserForm {entryId, mode: dismissed|escalated, agentId, platform?}`. No fill.
/// `dismissed` resumes. `escalated` starts a box handoff and does **not** resume.
pub async fn dismiss_user_form(
    state: &HostState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or_default();
    let resolution = match mode {
        "dismissed" => FormResolution::Dismissed,
        "escalated" => FormResolution::Escalated,
        _ => {
            return (
                400,
                json!({ "error": "mode must be dismissed or escalated" }),
            );
        }
    };
    let (seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    if !is_user_form_entry(&entry) {
        return (400, json!({ "error": "that entry is not a user-form" }));
    }
    if !is_unresolved(&entry) {
        // Skip after Open the screen can land as dismissed on the *form* id while a
        // live sand://box sibling still holds the screen. heal_or_already would
        // no-op because the form is already escalated.
        if resolution == FormResolution::Dismissed && is_escalated_form(&entry) {
            return abandon_escalated_form(state, account_id, &coworker_id, entry).await;
        }
        return heal_or_already(state, account_id, &coworker_id, &entry).await;
    }

    let form = form_request_from(&entry);
    let settled = settle_entry(entry, resolution, &BTreeMap::new(), true, &[], false);
    if let Err(error) = state
        .agui
        .auth
        .store
        .update_gateway_entry(&coworker_id, account_id, seq, &settled)
        .await
    {
        tracing::error!(%error, "could not settle a user-form entry");
        return (500, json!({ "error": "transcript unavailable" }));
    }
    journal_settled_form(state, account_id, &coworker_id, &settled).await;

    if resolution == FormResolution::Escalated {
        let handoff = start_box_handoff(state, account_id, &coworker_id, &form).await;
        let mut response = settled;
        if let Some(id) = handoff
            .as_ref()
            .and_then(|card| card.get("id"))
            .and_then(Value::as_str)
        {
            // HTTP convenience for NativeChat. Not persisted on the user-form: a `boxRequestId`
            // on that card would convert it into a handoff.
            response["handoffEntryId"] = json!(id);
        }
        return (200, response);
    }

    let content = model_facing_result(&settled, &form, resolution, &BTreeMap::new(), false);
    resume_user_form(
        state,
        account_id,
        &coworker_id,
        content,
        call_id_of(&settled),
    )
    .await;
    (200, settled)
}

/// `resolveBoxHandoff {entryId, agentId, resolution: handed_back|declined|timed_out}`.
/// Stamps `boxResolution` on every live `sand://box` sibling and resumes the waiting
/// user-form run. Accepts the handoff entry id **or** the escalated form entry id
/// (NativeChat KeepAlive falls back to the form when `handoffEntryId` is missing).
/// Does **not** stop the box (`handBackForeverBox` is a lifecycle verb).
pub async fn resolve_box_handoff(
    state: &HostState,
    args: &Value,
    account_id: &AccountId,
) -> (u16, Value) {
    let Some((entry_id, coworker_id)) = named_entry(args) else {
        return (400, json!({ "error": "entryId and agentId are required" }));
    };
    let word = args
        .get("resolution")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (content, timed_out) = match word {
        "handed_back" => (HAND_BACK_TOOL_RESULT.to_string(), false),
        "declined" => (HANDOFF_DECLINED_TOOL_RESULT.to_string(), false),
        "timed_out" => (HOLD_TIMED_OUT_TOOL_RESULT.to_string(), true),
        _ => {
            return (
                400,
                json!({ "error": "resolution must be handed_back, declined, or timed_out" }),
            );
        }
    };
    let (_seq, entry) = match load_owned_entry(state, account_id, &coworker_id, &entry_id).await {
        Ok(row) => row,
        Err(reply) => return reply,
    };
    let posted_live = is_live_handoff(&entry);
    let posted_handoff = is_handoff_entry(&entry);
    let posted_escalated_form = is_escalated_form(&entry);
    if !posted_live && !posted_handoff && !posted_escalated_form {
        return (
            400,
            json!({ "error": "that entry is not a live box handoff" }),
        );
    }

    // Name the call this resume answers. Passing `None` lets it land on whichever
    // call happens to be parked, which for stacked forms is the sibling's -- the
    // twin then gets this form's tool result. A posted handoff carries no call, so its
    // escalated form's is used; without one a hand-back resumed another conversation's run.
    let answers = match call_id_of(&entry) {
        Some(call) => Some(Some(call.to_string())),
        None => match waiting_calls(state, account_id, &coworker_id).await {
            Some(waiting) => state
                .agui
                .auth
                .store
                .gateway_transcript(&coworker_id, account_id)
                .await
                .ok()
                .and_then(|entries| handoff_call(&entries, &waiting)),
            None => None,
        },
    };
    let settled_siblings =
        settle_live_handoffs(state, account_id, &coworker_id, word, timed_out).await;
    if let Some(call_id) = answers {
        resume_user_form(state, account_id, &coworker_id, content, call_id.as_deref()).await;
    }

    if posted_live {
        if let Some(card) = settled_siblings
            .into_iter()
            .find(|card| card.get("id") == entry.get("id"))
        {
            return (200, card);
        }
        return (200, json!({ "alreadyAnswered": true }));
    }
    if posted_escalated_form {
        if let Some(card) = settled_siblings.into_iter().next() {
            return (200, card);
        }
        return (200, json!({ "alreadyAnswered": true }));
    }
    (200, json!({ "alreadyAnswered": true }))
}

/// The durable half of the hold deadline: every `SWEEP_INTERVAL`, time out the forms and
/// handoffs of runs parked past `FORM_HOLD_TIMEOUT`, across every account.
///
/// IT USED TO BE A SLEEPING TASK PER CARD, and a deploy inside those ten minutes left the run
/// parked and the screen held with nothing left to wake either. The deadline is the card's own
/// `timestampMs`; the log only says which coworkers to look at. Each tick reads the runs whose
/// last write crossed the deadline since the last tick — the first tick after a start reads them
/// all, which is the restart's backlog — so a run a person is slow to answer is looked at once, not
/// on every tick for as long as it waits.
pub async fn hold_deadlines_forever(state: HostState) {
    let mut after_ms = i64::MIN;
    loop {
        let now = now_ms();
        let before_ms = now - hold_ms() - MINT_LAG_MS;
        match state
            .agui
            .auth
            .store
            .parked_between(after_ms, before_ms)
            .await
        {
            Ok(runs) => {
                expire_parked_forms(&state, &runs, now).await;
                after_ms = before_ms;
            }
            Err(error) => tracing::warn!(%error, "the form-hold sweep could not read the log"),
        }
        tokio::time::sleep(crate::recovery::SWEEP_INTERVAL).await;
    }
}

/// Time out what the coworkers of these parked runs have held past its deadline at `now_ms`. The
/// runs only say whom to look at; each card's own stamp decides.
pub async fn expire_parked_forms(
    state: &HostState,
    runs: &[(opengrok_core::id::RunId, AccountId)],
    now_ms: i64,
) {
    let mut looked = BTreeSet::new();
    for (run_id, account_id) in runs {
        let Ok((run, _)) = state.agui.auth.store.load_run(run_id).await else {
            continue;
        };
        let on_a_form = matches!(
            run.pending.as_ref().map(|pending| pending.reason),
            Some(opengrok_core::run::SuspendReason::UserForm)
        );
        let Some(coworker_id) = resume::coworker_of(&run).filter(|_| on_a_form) else {
            continue;
        };
        if looked.insert((account_id.to_string(), coworker_id.to_string())) {
            settle_dead_holds(state, account_id, &coworker_id, now_ms).await;
        }
    }
}

async fn start_box_handoff(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form: &FormRequest,
) -> Option<Value> {
    let card = crate::cards::computer_handoff_card(
        &format!("e_{}", uuid::Uuid::now_v7()),
        &format!("req_{}", uuid::Uuid::now_v7().simple()),
        &handoff_instruction(form),
        now_ms(),
    );
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_gateway_entry(coworker_id, account_id, &card, now_ms())
        .await
    {
        tracing::error!(%error, "could not append the box handoff entry");
        return None;
    }
    Some(card)
}

/// Settle what holds this coworker's screen that no run will ever answer, and time out what has
/// held it past its deadline at `now_ms`. `false` when it could not tell or could not write, so a
/// caller that promises a card is closed can say it is not.
///
/// DEAD IS DECIDED BY THE RUN, NOT THE CARD. A card is minted only once its run's suspension is in
/// the log, so an unresolved card whose `callId` no parked run waits on belongs to a run that was
/// stopped, failed, or answered past it (a twin from the same completion): nothing will answer it,
/// and it held the screen for good. A card another conversation's parked run waits on is that
/// conversation's and is left alone — settling every card of the coworker is how typing in one
/// conversation dismissed a sign-in waiting in another. A card with no `callId` (written before
/// cards carried one) cannot be tied to a run and is left alone too.
///
/// A handoff card carries no `callId` — the transcribed shape has none — so it lives as long as an
/// escalated form's run still waits.
///
/// A dead card is settled dismissed and nothing resumes: there is no run to resume. A card past
/// its deadline is settled dismissed and timed out, and its run resumes with that answer; a
/// handoff past its deadline is timed out and its escalated form's run resumes.
pub async fn settle_dead_holds(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    now_ms: i64,
) -> bool {
    let Some(waiting) = waiting_calls(state, account_id, coworker_id).await else {
        return false;
    };
    let Ok(entries) = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker_id, account_id)
        .await
    else {
        return false;
    };
    let expired = |entry: &Value| minted_at(entry) <= now_ms.saturating_sub(hold_ms());
    // Still to be answered when this pass began: an open sibling either times out below and
    // answers its own call or is the person's, and an escalated one is answered by its handoff.
    let open: BTreeSet<&str> = entries
        .iter()
        .filter(|entry| is_unresolved(entry) || is_escalated_form(entry))
        .filter_map(call_id_of)
        .collect();
    let mut written = true;
    for entry in &entries {
        let dead = call_id_of(entry).is_some_and(|call| !waiting.contains_key(call));
        if !is_unresolved(entry) || !(dead || expired(entry)) {
            continue;
        }
        match settle_open_form(state, account_id, coworker_id, entry, !dead).await {
            Ok(Some(settled)) if !dead => {
                let form = form_request_from(&settled);
                let none = BTreeMap::new();
                let content =
                    model_facing_result(&settled, &form, FormResolution::Dismissed, &none, true);
                // Its own call; or, when its run is parked on a sibling from the same completion
                // whose card is no longer open (so nothing else will answer it), the sibling's —
                // a timed-out form must not leave its run parked with nothing left to wake it.
                let call_id = call_id_of(&settled).map(|call| match waiting.get(call) {
                    Some(pending) if !open.contains(pending.as_str()) => pending.as_str(),
                    _ => call,
                });
                resume_user_form(state, account_id, coworker_id, content, call_id).await;
            }
            Ok(_) => {}
            Err(()) => written = false,
        }
    }
    let live: Vec<&Value> = entries
        .iter()
        .filter(|entry| is_live_handoff(entry))
        .collect();
    if let Some(first) = live.first() {
        if handoff_call(&entries, &waiting).is_none() {
            let settled =
                settle_live_handoffs(state, account_id, coworker_id, "declined", false).await;
            written &= settled.len() >= live.len();
        } else if live.iter().any(|handoff| expired(handoff)) {
            let args = json!({
                "entryId": first.get("id"),
                "agentId": coworker_id.as_str(),
                "resolution": "timed_out",
            });
            written &= resolve_box_handoff(state, &args, account_id).await.0 == 200;
        }
    }
    written
}

/// The call a live handoff answers, which is its escalated form's: `Some(Some(call))` for the
/// form whose run still waits, `Some(None)` for a form written before cards carried a call (the
/// first parked form run is all there is to go on), `None` when no escalated form's run waits.
fn handoff_call(entries: &[Value], waiting: &BTreeMap<String, String>) -> Option<Option<String>> {
    entries
        .iter()
        .rev()
        .filter(|entry| is_escalated_form(entry))
        .find_map(|form| match call_id_of(form) {
            Some(call) if waiting.contains_key(call) => Some(Some(call.to_string())),
            Some(_) => None,
            None => Some(None),
        })
}

/// Every call a parked run of this coworker waits on, with the call that run is parked on.
/// `None` when the log cannot be read: a caller deciding which cards are dead must then decide
/// nothing.
async fn waiting_calls(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Option<BTreeMap<String, String>> {
    let store = &state.agui.auth.store;
    let mut calls = BTreeMap::new();
    for run_id in store.awaiting_approval(account_id).await.ok()? {
        let (run, _) = store.load_run(&run_id).await.ok()?;
        if let Some(pending) = run.pending.as_ref()
            && resume::run_belongs_to(&run, coworker_id)
        {
            for call in resume::parked_calls(&run) {
                calls.insert(call, pending.call_id.clone());
            }
        }
    }
    Some(calls)
}

/// Settle one open form as dismissed, re-read first so an answer that landed meanwhile wins.
/// `Ok(None)` when it did; `Err` when the write failed.
async fn settle_open_form(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    entry: &Value,
    timed_out: bool,
) -> Result<Option<Value>, ()> {
    let Some(entry_id) = entry.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let store = &state.agui.auth.store;
    let Ok(found) = store
        .find_gateway_entry(coworker_id, account_id, entry_id)
        .await
    else {
        return Err(());
    };
    let Some((seq, current)) = found.filter(|(_, current)| is_unresolved(current)) else {
        return Ok(None);
    };
    let settled = settle_entry(
        current,
        FormResolution::Dismissed,
        &BTreeMap::new(),
        true,
        &[],
        timed_out,
    );
    if let Err(error) = store
        .update_gateway_entry(coworker_id, account_id, seq, &settled)
        .await
    {
        tracing::error!(%error, "could not settle a user-form nothing will answer");
        return Err(());
    }
    journal_settled_form(state, account_id, coworker_id, &settled).await;
    Ok(Some(settled))
}

fn call_id_of(entry: &Value) -> Option<&str> {
    entry
        .get("callId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

/// The wire word NativeChat reads when a saved login is refused.
pub const SHARED_COMPUTER: &str = "shared-computer";
pub const SHARED_COMPUTER_MESSAGE: &str = "This computer is shared with other bots or people, so a saved login is not used on it. Type the login by hand, or give this bot its own computer.";

/// `savedLogin: true` marks values NativeChat took from the person's saved logins after
/// Touch ID, as opposed to values they typed into the card just now.
fn is_saved_login(args: &Value) -> bool {
    args.get("savedLogin")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// A saved login is the person's own; it lands only on a box that is one bot's own, and only
/// when that bot is theirs and shown to nobody else. A box shared by the account, a group or
/// an org never receives it, and neither does an org-visible bot's (every member drives it,
/// so a session left there would be theirs too). Decided here, at fill time, from the box's
/// scope and the bot's record — not from what the app believes.
async fn fills_a_dedicated_box(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> bool {
    let (_, _, _, _, mode) =
        super::provision::scope_of(&state.agui, account_id, coworker_id.as_str()).await;
    if mode != opengrok_core::coworker::BoxMode::Dedicated {
        return false;
    }
    state
        .agui
        .auth
        .store
        .coworker_is_private_and_owned_by(account_id, coworker_id)
        .await
        .unwrap_or(false)
}

fn named_entry(args: &Value) -> Option<(String, CoworkerId)> {
    let entry_id = args.get("entryId").and_then(Value::as_str)?;
    let agent_id = args
        .get("agentId")
        .or_else(|| args.get("id"))
        .and_then(Value::as_str)?;
    if entry_id.is_empty() || agent_id.is_empty() {
        return None;
    }
    Some((entry_id.to_string(), CoworkerId::from_stored(agent_id)))
}

async fn may_use(state: &HostState, account_id: &AccountId, coworker: &CoworkerId) -> bool {
    state
        .agui
        .auth
        .store
        .may_use_coworker(account_id, coworker)
        .await
        .unwrap_or(false)
}

/// Null stays the disclosure answer for a coworker the caller may not use. A stamped
/// `entryId` that is missing from the transcript is an error — NativeChat collapses on Null.
async fn load_owned_entry(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    entry_id: &str,
) -> Result<(i64, Value), (u16, Value)> {
    if !may_use(state, account_id, coworker_id).await {
        return Err((200, Value::Null));
    }
    match state
        .agui
        .auth
        .store
        .find_gateway_entry(coworker_id, account_id, entry_id)
        .await
    {
        Ok(Some(row)) => Ok(row),
        Ok(None) => Err((404, json!({ "error": "form entry missing" }))),
        Err(error) => {
            tracing::error!(%error, "could not load a form or handoff entry");
            Err((500, json!({ "error": "transcript unavailable" })))
        }
    }
}

fn settle_entry(
    mut entry: Value,
    resolution: FormResolution,
    shared: &BTreeMap<String, String>,
    widget_dismissed: bool,
    outcomes: &[FieldOutcome],
    timed_out: bool,
) -> Value {
    if let Some(map) = entry.as_object_mut() {
        map.insert("formResolution".to_string(), json!(resolution.as_str()));
        map.remove("values");
        // Never a boxRequestId on the user-form: that converts the card into a handoff.
        map.remove("boxRequestId");
        if widget_dismissed {
            map.insert("widgetDismissed".to_string(), json!(true));
        }
        if timed_out {
            map.insert("timedOut".to_string(), json!(true));
        }
        if shared.is_empty() {
            map.remove("sharedValues");
        } else {
            map.insert("sharedValues".to_string(), json!(shared));
        }
        if outcomes.is_empty() {
            map.remove("formFieldOutcomes");
        } else {
            map.insert("formFieldOutcomes".to_string(), json!(outcomes));
        }
    }
    entry
}

fn settle_handoff(mut entry: Value, resolution: &str, timed_out: bool) -> Value {
    if let Some(map) = entry.as_object_mut() {
        map.insert("boxResolution".to_string(), json!(resolution));
        if timed_out {
            map.insert("timedOut".to_string(), json!(true));
        }
    }
    entry
}

fn is_handoff_entry(entry: &Value) -> bool {
    entry
        .get("boxRequestId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
}

fn is_escalated_form(entry: &Value) -> bool {
    is_user_form_entry(entry)
        && entry.get("formResolution").and_then(Value::as_str) == Some("escalated")
}

/// Stamp `boxResolution` on every still-live sand://box sibling. One Skip must not
/// leave another unanswered handoff holding "Waiting for you".
async fn settle_live_handoffs(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    resolution: &str,
    timed_out: bool,
) -> Vec<Value> {
    let Ok(entries) = state
        .agui
        .auth
        .store
        .gateway_transcript(coworker_id, account_id)
        .await
    else {
        return Vec::new();
    };
    let mut settled = Vec::new();
    for entry in entries {
        if !is_live_handoff(&entry) {
            continue;
        }
        let Some(entry_id) = entry.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        let Ok(Some((seq, current))) = state
            .agui
            .auth
            .store
            .find_gateway_entry(coworker_id, account_id, &entry_id)
            .await
        else {
            continue;
        };
        if !is_live_handoff(&current) {
            continue;
        }
        let card = settle_handoff(current, resolution, timed_out);
        if let Err(error) = state
            .agui
            .auth
            .store
            .update_gateway_entry(coworker_id, account_id, seq, &card)
            .await
        {
            tracing::error!(%error, "could not settle a box handoff");
            continue;
        }
        settled.push(card);
    }
    settled
}

/// NativeChat Skip after Open the screen: form is already `escalated`, live handoff
/// still unanswered. Settle siblings as declined and resume. Escalate itself never
/// comes here (`mode: escalated` on a settled form still hits heal_or_already).
async fn abandon_escalated_form(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    entry: Value,
) -> (u16, Value) {
    settle_live_handoffs(state, account_id, coworker_id, "declined", false).await;
    // Name the call this Skip answers; with `None` a stacked sibling's parked
    // call would take the declined result instead.
    resume_user_form(
        state,
        account_id,
        coworker_id,
        HANDOFF_DECLINED_TOOL_RESULT.to_string(),
        call_id_of(&entry),
    )
    .await;
    (200, entry)
}

async fn fill_on_box(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form: &FormRequest,
    values: &BTreeMap<String, String>,
) -> Vec<FieldOutcome> {
    let failed = || nothing_filled(form);
    let Some(runner) = crate::agui::routes::tools_for_coworker(
        &state.agui,
        account_id,
        coworker_id,
        &[],
        &[],
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await
    else {
        return failed();
    };
    // The fill types into the box's browser outside the executor, so the executor's withholding
    // of the browser tools does not reach it: the same rule is applied here.
    if runner.network_off() {
        return failed();
    }
    let Some((computer, box_id)) = runner.fill_target() else {
        return failed();
    };
    // The person may press Submit long after the box went to sleep; this path types straight
    // into the box, so it wakes it the way a tool call would — same memo, same in-use stamp.
    if runner.wake_fill_target().await.is_err() {
        return failed();
    }
    // And, awake, the guest can say whether a tunnel is attached: a standing `never` that
    // could not be decided while the box slept is decided here, before a password is typed.
    if runner.network_off_now().await {
        return failed();
    }
    fill_into_focus(computer.as_ref(), &box_id, form, values).await
}

/// Every field of the form, not typed.
fn nothing_filled(form: &FormRequest) -> Vec<FieldOutcome> {
    form.fields
        .iter()
        .map(|field| FieldOutcome {
            id: field.id.clone(),
            filled: false,
            fill_failed: true,
        })
        .collect()
}

/// Whether a parked run still waits on this call.
async fn waits_on(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    call_id: Option<&str>,
) -> bool {
    let Some(call_id) = call_id else {
        return false;
    };
    let reason = opengrok_core::run::SuspendReason::UserForm;
    pending_suspended(state, account_id, coworker_id, reason, Some(call_id))
        .await
        .is_some()
}

async fn heal_or_already(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    entry: &Value,
) -> (u16, Value) {
    let form = form_request_from(entry);
    let shared = entry
        .get("sharedValues")
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(id, value)| Some((id.clone(), value.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let word = entry.get("formResolution").and_then(Value::as_str);
    let resolution = match word {
        Some("submitted") => FormResolution::Submitted,
        Some("fill_failed") => FormResolution::FillFailed,
        Some("escalated") => FormResolution::Escalated,
        _ => FormResolution::Dismissed,
    };
    // Escalated means the person is on the computer. Resume is hand-back / decline / timeout,
    // never this retry — otherwise hold clears and the model types into their session.
    if resolution == FormResolution::Escalated {
        return (200, entry.clone());
    }
    let timed_out = entry.get("timedOut").and_then(Value::as_bool) == Some(true);
    let content = model_facing_result(entry, &form, resolution, &shared, timed_out);
    if resume_user_form(state, account_id, coworker_id, content, call_id_of(entry)).await {
        return (200, entry.clone());
    }
    (200, json!({ "alreadyAnswered": true }))
}

/// Answer a pending `UserForm` run and resume it with a synthesised result. `true` when a
/// pending run was found and answered (a retry can pick up a card that settled before the
/// run did).
async fn resume_user_form(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    content: String,
    call_id: Option<&str>,
) -> bool {
    resume_settled(
        state,
        account_id,
        coworker_id,
        opengrok_core::run::SuspendReason::UserForm,
        content,
        call_id,
    )
    .await
}

pub(crate) async fn resume_settled(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    reason: opengrok_core::run::SuspendReason,
    content: String,
    call_id: Option<&str>,
) -> bool {
    let Some((run_id, mut run, seq, pending)) =
        pending_suspended(state, account_id, coworker_id, reason, call_id).await
    else {
        return false;
    };
    if let Some(want) = call_id.filter(|id| !id.is_empty())
        && pending.call_id != want
    {
        // The run waits on this card among others from the same completion, and is parked on a
        // sibling's call: settle this card, leave the parked call for its own card.
        return false;
    }
    let resumed_seq = run.emitted.len() as u32;
    let at_ms = now_ms();
    let events = match run.decide(RunCommand::Answer {
        call_id: pending.call_id.clone(),
        approved: true,
        by: account_id.to_string(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(opengrok_core::run::RunError::AlreadyAnswered) => return false,
        Err(error) => {
            tracing::warn!(%error, "could not answer the pending run");
            return false;
        }
    };
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(account_id))
        .await
    {
        tracing::error!(%error, "could not append the answer");
        return false;
    }
    let in_room = resume::in_a_room(&run, coworker_id);
    let state = state.clone();
    let account_id = account_id.clone();
    let coworker_id = coworker_id.clone();
    tokio::spawn(resume::resume_where_it_lives(
        in_room,
        state,
        account_id,
        run_id,
        coworker_id,
        pending,
        resumed_seq,
        opengrok_harness::ResumeOutcome::Settled(content),
    ));
    true
}

/// The parked run a card answers.
///
/// NAMED BY THE CARD'S CALL when it has one. The first parked run of the coworker is whichever
/// moved least recently — often another conversation's — and answering into it left the card's
/// own run parked behind a card that already read as answered. `None` is for a card written
/// before cards carried their call, and takes the first, as every card once did.
pub(crate) async fn pending_suspended(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    reason: opengrok_core::run::SuspendReason,
    call_id: Option<&str>,
) -> Option<(
    opengrok_core::id::RunId,
    opengrok_core::run::Run,
    i64,
    opengrok_core::run::PendingApproval,
)> {
    let run_ids = state
        .agui
        .auth
        .store
        .awaiting_approval(account_id)
        .await
        .ok()?;
    for run_id in run_ids {
        let Ok((run, seq)) = state.agui.auth.store.load_run(&run_id).await else {
            continue;
        };
        if !resume::run_belongs_to(&run, coworker_id) {
            continue;
        }
        let Some(pending) = run.pending.clone() else {
            continue;
        };
        if pending.reason != reason {
            continue;
        }
        if let Some(want) = call_id.filter(|id| !id.is_empty())
            && !resume::parked_calls(&run).contains(want)
        {
            continue;
        }
        return Some((run_id, run, seq, pending));
    }
    None
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Journal a settled user-form onto the AG-UI run NativeChat replays.
///
/// Live HITL stays `CUSTOM name: run-awaiting-approval` / `reason: user-form`.
/// NativeChat's assembler already hydrates a later `CUSTOM name: user-form`
/// whose `value` is the gateway send-message envelope (`message.type:
/// user-form`, `formRequest`, sibling `formResolution`, no secrets). Without
/// this frame, `GET /ag-ui/threads/{id}` only has the mint-time CUSTOM and a
/// cold client cannot rebuild ✓ Submitted.
async fn journal_settled_form(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    settled: &Value,
) {
    journal_agui_custom(
        state,
        account_id,
        coworker_id,
        opengrok_core::run::SuspendReason::UserForm,
        call_id_of(settled),
        agui_user_form_frame(settled),
    )
    .await;
}

/// Append a CUSTOM onto the still-pending run waiting on `call_id` (NativeChat replays this).
/// Scrubs accidental password keys before the payload is stored. A run that no longer waits
/// takes no frame: a stopped run's log is closed, and journaling onto whichever run was parked
/// instead wrote one conversation's card into another's replay.
pub(crate) async fn journal_agui_custom(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    reason: opengrok_core::run::SuspendReason,
    call_id: Option<&str>,
    frame: Value,
) {
    let Some((run_id, mut run, seq, pending)) =
        pending_suspended(state, account_id, coworker_id, reason, call_id).await
    else {
        return;
    };
    let at_ms = now_ms();
    let scrubbed = opengrok_tools::credential::scrub_secret_keys(&frame);
    let mut frame = scrubbed;
    if let Some(map) = frame.as_object_mut() {
        map.insert("threadId".to_string(), json!(run.thread_id.clone()));
        map.insert("runId".to_string(), json!(run_id.as_str()));
        let has_call = map
            .get("callId")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty());
        if !has_call {
            map.insert("callId".to_string(), json!(pending.call_id.clone()));
        }
        map.insert("timestamp".to_string(), json!(at_ms));
    }
    let Ok(events) = run.decide(RunCommand::Emit {
        payload: frame,
        at_ms,
    }) else {
        return;
    };
    for event in &events {
        run.apply(event);
    }
    let view = RunView {
        id: run_id.clone(),
        thread_id: run.thread_id.clone(),
        status: run.status,
        event_count: run.emitted.len() as i64,
        updated_at_ms: at_ms,
    };
    if let Err(error) = state
        .agui
        .auth
        .store
        .append_run(&run_id, seq, &events, &view, Some(account_id))
        .await
    {
        tracing::error!(%error, "could not journal a custom frame onto a pending run");
    }
}

/// CUSTOM NativeChat already hydrates for settled / cold-load cards. Not live HITL.
pub(crate) fn agui_user_form_frame(entry: &Value) -> Value {
    let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    let message = entry
        .get("message")
        .cloned()
        .unwrap_or_else(|| json!({ "type": "user-form" }));
    let form_request = message.get("formRequest").cloned().unwrap_or(Value::Null);
    let resolution = entry.get("formResolution").cloned().unwrap_or(Value::Null);
    let call_id = entry.get("callId").cloned().unwrap_or(Value::Null);
    json!({
        "type": "CUSTOM",
        "name": "user-form",
        "entryId": entry_id,
        "callId": call_id,
        "formRequest": form_request,
        "formResolution": resolution,
        "message": message,
        "value": entry,
    })
}

fn overlay_form(event: &mut Value, form: &Value) {
    let Some(map) = event.as_object_mut() else {
        return;
    };
    if let Some(id) = form.get("id") {
        map.insert("entryId".to_string(), id.clone());
    }
    if map
        .get("callId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        && let Some(call_id) = form.get("callId")
    {
        map.insert("callId".to_string(), call_id.clone());
    }
    if let Some(message) = form.get("message") {
        map.insert("message".to_string(), message.clone());
        if let Some(request) = message.get("formRequest") {
            map.insert("formRequest".to_string(), request.clone());
        }
    }
    if let Some(resolution) = form.get("formResolution") {
        map.insert("formResolution".to_string(), resolution.clone());
    }
    map.insert("value".to_string(), form.clone());
}

fn user_form_event_id(event: &Value) -> Option<&str> {
    event
        .get("entryId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .or_else(|| event.pointer("/value/id").and_then(Value::as_str))
        .or_else(|| event.pointer("/value/entryId").and_then(Value::as_str))
}

fn is_user_form_agui(event: &Value) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("CUSTOM") {
        return false;
    }
    let name = event.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "user-form" {
        return true;
    }
    if name == "run-awaiting-approval"
        && event.get("reason").and_then(Value::as_str) == Some("user-form")
    {
        return true;
    }
    event
        .get("message")
        .and_then(|message| message.get("type"))
        .and_then(Value::as_str)
        == Some("user-form")
}

fn form_fingerprint(request: &Value) -> String {
    let title = request.get("title").and_then(Value::as_str).unwrap_or("");
    let ids = request
        .get("fields")
        .and_then(Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(|field| field.get("id").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    format!("{title}|{ids}")
}

fn event_fingerprint(event: &Value) -> Option<String> {
    let request = event
        .get("formRequest")
        .or_else(|| event.get("arguments"))
        .or_else(|| event.pointer("/message/formRequest"))
        .or_else(|| event.pointer("/value/message/formRequest"))?;
    Some(form_fingerprint(request))
}

fn entry_fingerprint(entry: &Value) -> Option<String> {
    entry.pointer("/message/formRequest").map(form_fingerprint)
}

/// Live HITL CUSTOM NativeChat keys Continue off — `run-awaiting-approval` / `user-form`.
pub(crate) fn is_live_user_form_custom(event: &Event) -> bool {
    event.event_type == EventType::Custom
        && event.extra.get("name").and_then(Value::as_str) == Some("run-awaiting-approval")
        && event.extra.get("reason").and_then(Value::as_str) == Some("user-form")
}

/// NativeChat paints a Website login card from live `TOOL_CALL` frames. It uses `entryId`
/// when present, otherwise the raw `toolCallId` (`call-…`). Those frames stream during the
/// model completion, *before* we mint the gateway card — so a second same-title form was
/// left as `call-*-1` and Continue could not `POST /ag-ui/user-form/submit`.
///
/// Hold every `request_user_form` TOOL_CALL until the matching CUSTOM is stamped with `e_*`,
/// then forward the call frames with that id.
#[derive(Default)]
pub(crate) struct UserFormSseHold {
    form_ids: HashSet<String>,
    held: Vec<Event>,
    /// A `run-stopped` frame has passed: the `RUN_FINISHED` behind it closes a stop, not a finish.
    stopped: bool,
}

impl UserFormSseHold {
    fn tool_call_id(event: &Event) -> Option<&str> {
        event
            .extra
            .get("toolCallId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
    }

    /// Hold a user-form TOOL_CALL; pass every other frame through.
    pub(crate) fn push(&mut self, event: Event) -> Option<Event> {
        match event.event_type {
            EventType::ToolCallStart => {
                let name = event
                    .extra
                    .get("toolCallName")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if name == opengrok_tools::REQUEST_USER_FORM {
                    if let Some(id) = Self::tool_call_id(&event) {
                        self.form_ids.insert(id.to_string());
                    }
                    self.held.push(event);
                    return None;
                }
                Some(event)
            }
            EventType::ToolCallArgs | EventType::ToolCallEnd | EventType::ToolCallResult => {
                if Self::tool_call_id(&event).is_some_and(|id| self.form_ids.contains(id)) {
                    self.held.push(event);
                    return None;
                }
                Some(event)
            }
            EventType::Custom => {
                if event.extra.get("name").and_then(Value::as_str) == Some("run-stopped") {
                    self.stopped = true;
                }
                Some(event)
            }
            _ => Some(event),
        }
    }

    /// Frames for this `toolCallId`, now carrying the gateway `entryId`.
    pub(crate) fn release_for(&mut self, call_id: &str, entry_id: Option<&str>) -> Vec<Event> {
        if call_id.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut rest = Vec::new();
        for mut event in std::mem::take(&mut self.held) {
            if Self::tool_call_id(&event) == Some(call_id) {
                if let Some(id) = entry_id.filter(|id| !id.is_empty()) {
                    event.extra.insert("entryId".to_string(), json!(id));
                }
                out.push(event);
            } else {
                rest.push(event);
            }
        }
        self.held = rest;
        self.form_ids.remove(call_id);
        // Every fragment of this call is in hand here, which the per-delta scrub in
        // the harness never has: assemble and scrub before anything reaches the wire.
        opengrok_harness::scrub_streamed_tool_args(out)
    }

    /// Stream is ending; leftover form TOOL_CALLs (refused, never awaiting) go out as-is.
    pub(crate) fn release_rest(&mut self) -> Vec<Event> {
        self.form_ids.clear();
        opengrok_harness::scrub_streamed_tool_args(std::mem::take(&mut self.held))
    }

    /// The stream's closer has arrived. After a finish's `RUN_FINISHED` the leftovers go out as
    /// `release_rest` sends them; after `RUN_ERROR`, or the `RUN_FINISHED` that closes a stop,
    /// they go nowhere. A run that failed or was stopped has no suspension behind any card, and
    /// NativeChat paints one from these frames — a button whose answer can only be a 409, most of
    /// all for a park whose write the log refused or that a Stop turned into a stop.
    pub(crate) fn release_at_end(&mut self, clean: bool) -> Vec<Event> {
        let rest = self.release_rest();
        if clean && !self.stopped {
            rest
        } else {
            Vec::new()
        }
    }
}

/// Fold current gateway user-form state into AG-UI replay events so a cold
/// NativeChat rebuilds ✓ Submitted (and idle cards) from `GET /ag-ui/threads/{id}`
/// / `GET /ag-ui/runs/{id}` — not only from live unresolved CUSTOMs.
pub(crate) fn hydrate_agui_events(
    mut events: Vec<Value>,
    forms: &[Value],
    started_at_ms: i64,
    updated_at_ms: i64,
) -> Vec<Value> {
    let forms: Vec<&Value> = forms
        .iter()
        .filter(|entry| is_user_form_entry(entry))
        .collect();
    if forms.is_empty() {
        return events;
    }
    let mut used = HashSet::new();
    for event in &mut events {
        if !is_user_form_agui(event) {
            continue;
        }
        if let Some(id) = user_form_event_id(event).map(str::to_string)
            && let Some(form) = forms
                .iter()
                .find(|entry| entry.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            overlay_form(event, form);
            used.insert(id);
            continue;
        }
        // Same-completion stacked Website login cards share a fingerprint. Join
        // on `callId` so replay does not stamp the last card onto the first TOOL_CALL.
        if let Some(call_id) = event
            .get("callId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            && let Some(form) = forms.iter().find(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                !used.contains(id) && entry.get("callId").and_then(Value::as_str) == Some(call_id)
            })
        {
            overlay_form(event, form);
            if let Some(id) = form.get("id").and_then(Value::as_str) {
                used.insert(id.to_string());
            }
            continue;
        }
        if let Some(fingerprint) = event_fingerprint(event)
            && let Some(form) = forms.iter().find(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                !used.contains(id)
                    && entry_fingerprint(entry).as_deref() == Some(fingerprint.as_str())
            })
        {
            overlay_form(event, form);
            if let Some(id) = form.get("id").and_then(Value::as_str) {
                used.insert(id.to_string());
            }
        }
    }
    // A card is appended only to the run that made its call. Two runs a few seconds apart in one
    // thread both have it in their window and each hydrates against the coworker's whole
    // transcript, so the window alone painted it once per run. A card that names no call (written
    // before cards carried one) has only the window to go on.
    let calls: HashSet<String> = events
        .iter()
        .flat_map(|event| [event.get("callId"), event.get("toolCallId")])
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    const SLACK_MS: i64 = 5_000;
    for form in forms {
        let Some(id) = form.get("id").and_then(Value::as_str) else {
            continue;
        };
        if used.contains(id) {
            continue;
        }
        if call_id_of(form).is_some_and(|call| !calls.contains(call)) {
            continue;
        }
        let at = form.get("timestampMs").and_then(Value::as_i64).unwrap_or(0);
        if at < started_at_ms.saturating_sub(SLACK_MS)
            || at > updated_at_ms.saturating_add(SLACK_MS)
        {
            continue;
        }
        events.push(agui_user_form_frame(form));
        used.insert(id.to_string());
    }
    stamp_tool_calls(&mut events);
    events
}

/// NativeChat paints TOOL_CALL frames as Website login cards. Live HITL stamps
/// `entryId` on the CUSTOM; replay overlays the same id onto matching TOOL_CALLs
/// so stacked cards stay Continue-able after a reconnect.
fn stamp_tool_calls(events: &mut [Value]) {
    let mut by_call: BTreeMap<String, String> = BTreeMap::new();
    for event in events.iter() {
        if !is_user_form_agui(event) {
            continue;
        }
        let Some(call_id) = event.get("callId").and_then(Value::as_str) else {
            continue;
        };
        if let Some(entry_id) = user_form_event_id(event) {
            by_call.insert(call_id.to_string(), entry_id.to_string());
        }
    }
    if by_call.is_empty() {
        return;
    }
    for event in events.iter_mut() {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "TOOL_CALL_START" && kind != "TOOL_CALL_ARGS" && kind != "TOOL_CALL_END" {
            continue;
        }
        let Some(call_id) = event.get("toolCallId").and_then(Value::as_str) else {
            continue;
        };
        let Some(entry_id) = by_call.get(call_id) else {
            continue;
        };
        if let Some(map) = event.as_object_mut() {
            map.insert("entryId".to_string(), json!(entry_id));
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../../tests/unit/user_form.rs"]
mod tests;
