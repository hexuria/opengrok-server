//! Coworker membership, access, hiring, and listing.

use super::*;

/// `GET /models` — the routes this deployment's gateway advertises.
///
/// Signed in is enough: this is the list of things a person may pin their own coworker to, and it
/// carries no secret. The gateway's key stays here; the browser only ever learns ids.
pub async fn list_models(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let source = query.get("source").map(|word| serde_json::json!(word));
    let asked = match crate::inference::named(source.as_ref(), "source") {
        Ok(asked) => asked.map(|asked| asked.kind),
        Err(refusal) => return *refusal,
    };
    let mut listing = match (asked, state.auth.model_catalogue.clone()) {
        (Some(SourceKind::LocalProxy), _) => serde_json::json!({ "models": [], "note": null }),
        // A mock door has no gateway to ask. Say that, rather than answering [] as though the
        // gateway had told us it serves nothing.
        (_, None) => serde_json::json!({
            "models": [],
            "note": "this deployment has no gateway configured (OG_MODEL_DOOR is a mock), so a \
                     pin must be typed by hand",
        }),
        (_, Some(catalogue)) => {
            let listing = catalogue.list().await;
            let points = crate::points::models_points(&state).await;
            // Points multipliers (`points.rs`): null on a gateway with no reference price or older
            // than open-ai-gateway #52; the picker shows ×N after the id.
            let entry = |model: &opengrok_core::catalogue::Model| {
                let base = crate::points::base_model(&model.id);
                let points = crate::points::points_json(points.as_ref().and_then(|p| p.get(base)));
                model.entry(points, SourceKind::Gateway, None)
            };
            let models: Vec<_> = listing.models.iter().map(entry).collect();
            serde_json::json!({ "models": models, "note": listing.note })
        }
    };
    // Whenever an address is stored or a Mac is there to ask, whatever the setting's kind: their
    // models beside the gateway's, and `localProxy`, so a picker can say why they are missing.
    let listing_local = asked != Some(SourceKind::Gateway);
    if let Some((said, local)) = local_proxy::listed(&state, &account, listing_local).await {
        let plan = local.iter().map(|(via, model)| model.on_the_plan(*via));
        if let Some(models) = listing["models"].as_array_mut() {
            models.extend(plan);
        }
        listing["localProxy"] = said;
    }
    Json(listing).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub model: String,
}

/// `POST /models/probe` — ask the gateway to answer one tiny prompt on a candidate pin.
///
/// This is how a pin is proven BEFORE it is saved. It is also how we learned that `oag/auto` is
/// refused on a route with no matching credential — a fact no amount of reading the catalogue
/// would have revealed, because the id IS advertised.
pub async fn probe_model(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<ProbeRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let model = request.model.trim();
    if model.is_empty() {
        return (StatusCode::BAD_REQUEST, "a model is required").into_response();
    }
    let Some(catalogue) = state.auth.model_catalogue.clone() else {
        return Json(serde_json::json!({
            "ok": false,
            "detail": "this deployment has no gateway configured, so a pin cannot be proven here",
        }))
        .into_response();
    };
    // A probe is a REAL, billed completion on the deployment's own key. One person clicking Test
    // needs a handful; a loop wants thousands of somebody else's money.
    if !catalogue.may_probe(account_id.as_str()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "wait a moment before testing another route",
        )
            .into_response();
    }
    match catalogue.probe(model).await {
        Ok(probed) => Json(serde_json::json!({
            "ok": true, "served": probed.served, "toolCalls": probed.tool_calls,
        }))
        .into_response(),
        // The gateway's own words. A paraphrase would lose the part that says what to do.
        Err(detail) => Json(serde_json::json!({ "ok": false, "detail": detail })).into_response(),
    }
}

/// `PATCH /coworkers/{id}` — change this coworker's name, its route, how hard it thinks, its
/// standing role, its decoration, or several at once. A field absent is left alone; `role: null`
/// or a blank string clears it, and `effort: null` is `inherit`. Taking the body as a `Value`
/// rather than a struct of `Option`s is what makes "absent" and "null" different, which a
/// nullable field needs.
///
/// EVERY FIELD THE CLIENT SENDS IS READ HERE. The app's Save button puts the whole card in one
/// body — name, title and role together — so a field this route quietly skipped was an edit the
/// person watched succeed and lost: a rename to "Greendale" was dropped on the floor while the
/// role beside it was stored, and the coworker went on introducing itself as "New Bot".
///
/// The fields land in two different homes, as `persona.rs` explains: the name, model, role and
/// visibility are the aggregate's, and the title, avatar shape and colour are the client's
/// decoration in the coworker's profile blob. `notifyOnUpdates` has no home at all — see below.
///
/// A coworker not on your roster answers 404, like every other per-coworker route here: an id
/// you cannot use must not be distinguishable from one that does not exist. One an org-mate
/// shared with you is on your roster, so you already know it exists; what you may change on it is
/// your own sidebar, and anything else is a 403 that says so — management stays with the owner.
/// A body naming no field is a 400 rather than a silent no-op, because a caller who sent one
/// meant something.
pub async fn repin_coworker(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let refuse = |sentence: String| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": sentence })),
        )
            .into_response()
    };
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    // The caller's own roster row, so the reply below can be exactly what the roster will list.
    let (listed, owner) = match state.auth.store.roster_for(&account_id).await {
        Ok(roster) => match roster.into_iter().find(|(view, _)| view.id == coworker_id) {
            Some(seat) => seat,
            None => return (StatusCode::NOT_FOUND, "no such coworker").into_response(),
        },
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let mine = owner.id == account_id;

    // A name is trimmed and must survive it. Null is a wrong type here rather than "clear it":
    // the role is nullable and a name is not, because `persona::system_message` has no identity
    // line to write without one and the coworker would stop knowing what it is called.
    let name = match body.get("name") {
        None => None,
        Some(serde_json::Value::String(name)) => match crate::persona::validate_name(name) {
            Ok(name) => Some(name),
            Err(sentence) => return refuse(sentence),
        },
        Some(_) => return refuse("name: expected a string".to_string()),
    };
    let model = match body.get("model") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(model)) => Some(model.clone()),
        Some(_) => return refuse("model: expected a route id".to_string()),
    };
    // Absent leaves the role alone; null or blank clears it. Both are meaningful, so they cannot
    // collapse into one `Option`.
    let role = match body.get("role") {
        None => None,
        Some(serde_json::Value::Null) => Some(None),
        Some(serde_json::Value::String(role)) => match crate::persona::validate_role(Some(role)) {
            Ok(role) => Some(role),
            Err(sentence) => return refuse(sentence),
        },
        Some(_) => return refuse("role: expected a string, or null to clear it".to_string()),
    };
    // "private" | "org". An unrecognised word is refused rather than defaulted: a caller who
    // wrote "public" meant something we do not offer, and quietly storing "private" would tell
    // them they had shared a coworker they had not. `org` is honoured by the roster
    // (`roster_for`) and the run door (`policy_to_use`). Until #175 nothing on this door read
    // it, and a 200 here reported a sharing that did nothing; an owner in no org, for whom that
    // is still true, is refused below.
    let visibility = match body.get("visibility") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => {
            match opengrok_core::coworker::Visibility::parse(text) {
                Some(visibility) => Some(visibility),
                None => {
                    return refuse(format!("visibility: '{text}' is not one of private, org"));
                }
            }
        }
        Some(_) => return refuse("visibility: expected \"private\" or \"org\"".to_string()),
    };
    // One of the words agreed with the client (#271); null is inherit. Anything else refuses the
    // WHOLE body here, before a field of it is written: a name beside "loud" is not renamed.
    let effort = match Effort::named(body.get("effort")) {
        Ok(effort) => effort,
        Err(why) => return refuse(format!("effort {why}")),
    };
    // Absent leaves its door, null hands it back to the person's own setting, any other word 400s.
    let source = body.get("source").map(|word| SourceKind::named(Some(word)));
    let source = match source.transpose() {
        Ok(source) => source,
        Err(why) => return refuse(format!("source {why}")),
    };
    let hidden = match body.get("hiddenFromSidebar") {
        None => None,
        Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Bool(hidden)) => Some(*hidden),
        Some(_) => return refuse("hiddenFromSidebar: expected a boolean".to_string()),
    };
    // The decoration, type-checked at the door and merged into the profile blob below. A key
    // absent leaves the stored value alone; an empty string is stored as one, which is how the
    // client clears a field and how the desktop's own update already behaves.
    //
    // `description` is in the blob too but no client sends it here, so this door does not read
    // it — the key list itself lives in `persona.rs`, where the blob's shape is decided.
    let mut decoration = serde_json::Map::new();
    for key in ["title", "avatarShape", "avatarColor"] {
        match body.get(key) {
            None => {}
            Some(serde_json::Value::String(text)) => {
                decoration.insert(key.to_string(), serde_json::Value::String(text.clone()));
            }
            Some(_) => return refuse(format!("{key}: expected a string")),
        }
    }
    // `notifyOnUpdates` arrives from the app and is READ NOWHERE, deliberately. Nothing on this
    // server stores it, `coworker_row` never answers it, and the desktop client keeps the real
    // answer in its own settings file (`docs/research/client-grok-bot.md` §8.1). Accepting it here would need a table, and
    // inventing one to make a toggle look persistent is worse than the toggle not persisting.
    //
    // So a body naming nothing this route can change — including one carrying only that toggle —
    // is a 400 rather than a silent no-op, and the sentence lists what it could have named.
    let manages = name.is_some()
        || model.is_some()
        || effort.is_some()
        || source.is_some()
        || role.is_some()
        || visibility.is_some()
        || !decoration.is_empty();
    if !manages && hidden.is_none() {
        return refuse(
            "nothing to change: send a name, a model, a role, a title, an avatar shape or \
             colour, a visibility, an effort, a source, hiddenFromSidebar, or several"
                .to_string(),
        );
    }
    if !mine {
        return match (manages, hidden) {
            (false, Some(hidden)) => {
                hide_shared(&state, &account_id, &listed, &owner, hidden).await
            }
            _ => (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "only the person who hired this coworker can change it; you can \
                              hide it from your own sidebar"
                })),
            )
                .into_response(),
        };
    }

    let Ok((loaded, seq)) = state.auth.store.load_coworker(&coworker_id).await else {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    };
    // Read before anything is written, and a failure refuses the whole PATCH: the blob is merged
    // into and written back whole, so an unreadable one taken as `{}` would overwrite the stored
    // title and avatar with nothing — and even a PATCH that touches no decoration answers it.
    let mut profile = match state.auth.store.seamb_profile(&coworker_id).await {
        Ok(profile) => profile.unwrap_or_else(|| serde_json::json!({})),
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    // The stored hide flag, read here for the same reason and with the same refusal: the reply
    // is the roster row the app overwrites its own from, so a failed read answered as `false`
    // would un-hide the coworker on the sidebar of the person who hid it. `GET /coworkers`
    // answers the same failure 503.
    let hidden_from_sidebar = match hidden {
        Some(hidden) => hidden,
        None => match state.auth.store.hidden_coworker_ids(&account_id).await {
            Ok(ids) => ids.contains(coworker_id.as_str()),
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        },
    };
    let at_ms = now_ms();
    let mut events = Vec::new();
    // One command per decision, as the aggregate defines them: renaming, repinning and describing
    // are different things and a caller that meant one must not do the other.
    if let Some(name) = name {
        match loaded.decide(CoworkerCommand::Rename { name, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    if let Some(model) = model {
        match loaded.decide(CoworkerCommand::Repin { model, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    if let Some(effort) = effort {
        match loaded.decide(CoworkerCommand::SetEffort { effort, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    if let Some(source) = source {
        match loaded.decide(CoworkerCommand::SetSource { source, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    if let Some(role) = role {
        match loaded.decide(CoworkerCommand::SetRole { role, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    // Sharing with the org when there is no org would store a word that reaches nobody — the
    // 200 would tell the person their coworker was shared when it was not. Refused only on the
    // way IN, so a coworker already marked `org` (its owner has since left the org) can still be
    // saved from a card that sends its visibility back unchanged.
    if visibility == Some(opengrok_core::coworker::Visibility::Org)
        && loaded.visibility != opengrok_core::coworker::Visibility::Org
        && owner.org_id.is_none()
    {
        return refuse(
            "visibility: this account is in no org, so there is nobody to share this coworker \
             with; it stays private"
                .to_string(),
        );
    }
    if let Some(visibility) = visibility {
        match loaded.decide(CoworkerCommand::SetVisibility { visibility, at_ms }) {
            Ok(more) => events.extend(more),
            Err(error) => return refuse(error.to_string()),
        }
    }
    let mut after = loaded.clone();
    for event in &events {
        after.apply(event);
    }
    // ON ITS OWN PLAN ITS PIN IS WHAT IS ASKED: a body putting it there, or repinning it there,
    // leaves one a subscription may answer or is refused before any write, as a `localModel` is.
    if (source.is_some() || after.model != loaded.model)
        && after.source == Some(SourceKind::LocalProxy)
        && let Err(why) = opengrok_core::inference::subscription_model(&after.model)
    {
        return refuse(format!("model: {why}"));
    }
    let chosen = (after.source, after.model.as_str());
    if let Some(effort) = effort
        && let Some(why) = effort_refused(&state, &account_id, chosen, effort).await
    {
        return refuse(format!("effort: {why}"));
    }
    // The stored stamp when nothing is appended (a decoration- or hide-only PATCH): the
    // projection is only rewritten with events, and a reply stamped `now` over an unchanged
    // row would move the coworker in the sidebar until the next roster read moved it back.
    let stamp = if events.is_empty() {
        listed.updated_at_ms
    } else {
        at_ms
    };
    let view = CoworkerView::of(coworker_id.clone(), &after, stamp);
    if !events.is_empty()
        && state
            .auth
            .store
            .append_coworker(&coworker_id, &account_id, seq, &events, &view)
            .await
            .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    if let Some(hidden) = hidden
        && state
            .auth
            .store
            .set_coworker_hidden(&account_id, &coworker_id, hidden, at_ms)
            .await
            .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    if !decoration.is_empty() {
        crate::persona::merge_profile_text(&mut profile, &serde_json::Value::Object(decoration));
        // A 500 rather than the removed seam-B path's silent `let _`: this door exists because an
        // edit that is accepted and not stored is the bug being fixed, and a reply saying the title
        // changed when the write failed would be that bug again.
        if state
            .auth
            .store
            .put_seamb_profile(&coworker_id, &profile, at_ms)
            .await
            .is_err()
        {
            return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
        }
    }
    // The same row the roster lists, by construction: the app overwrites its row from this reply
    // and relaunches onto `GET /coworkers`, so two spellings of one coworker is a coworker that
    // changes shape on restart.
    let live = provision::live_boxes(&state, &account_id, &[&view]).await;
    let box_id = live.get(&view.id).map(String::as_str);
    let row = coworker_row(
        &view,
        Some(&profile),
        hidden_from_sidebar,
        &owner,
        &account_id,
        box_id,
    );
    Json(row).into_response()
}

/// A member hiding a coworker an org-mate shared: the one change that is theirs to make, because
/// the sidebar it changes is their own (`coworker_hidden` is keyed by the viewer). Without it a
/// shared coworker is a row somebody can never put away.
async fn hide_shared(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    listed: &opengrok_core::coworker::CoworkerView,
    owner: &opengrok_store::RosterOwner,
    hidden: bool,
) -> Response {
    let profile = match state.auth.store.seamb_profile(&listed.id).await {
        Ok(profile) => profile,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    if state
        .auth
        .store
        .set_coworker_hidden(account_id, &listed.id, hidden, now_ms())
        .await
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    Json(coworker_row(
        listed,
        profile.as_ref(),
        hidden,
        owner,
        account_id,
        None,
    ))
    .into_response()
}

/// One coworker, as every route that answers with one spells it — the roster, the hire reply
/// (which adds `computerError` and `templateNote`) and the PATCH reply.
///
/// camelCase throughout, because that is what the app's `Coworker` deserialises: a snake_case
/// key is a field it silently reads as absent, which is how #27 lost a whole reply, and how the
/// roster (which serialized the core `CoworkerView` as-is) lost the sort key, the title and the
/// avatar on every relaunch.
///
/// Provenance, key by key. The decoder of this row is NativeChat's `Coworker`, hexuria/nativechat
/// `src/opengrok/types.rs:54-79` (at 2af9e60), `#[serde(rename_all = "camelCase")]`; the roster is
/// its `Vec<Coworker>` and the hire and PATCH replies one `Coworker` (`src/opengrok/client.rs`
/// `list_coworkers`, `hire`, `patch_coworker`). `id` is its one required key. `name`, `model`
/// (`String`), `updatedAtMs` (`i64`, aliases `updated_at_ms` and `updatedAt`) and
/// `hiddenFromSidebar` (`bool`) are `#[serde(default)]`, which covers a missing key and not a null
/// one: a null in any of them fails the decode (on the roster, one row's null fails the whole
/// array), so the row never sends one. `hiddenFromSidebar`'s only alias is its own spelling, so
/// `hidden_from_sidebar` or §8.1's `isHiddenFromSidebar` would be skipped and read as false — every
/// hidden coworker back in the sidebar. `role`, `title`, `avatarShape`, `avatarColor` and `boxId`
/// (aliases `boxId` and `box_id`) are `Option<String>`, null reading as unset. The struct has no
/// `deny_unknown_fields`, so `visibility`, `isGroup`, `memberIds`, `mine`, `canManage` and `owner`
/// (and the hire reply's extras) are ignored by it, not refused. `title`, `avatarShape`,
/// `avatarColor`, `isGroup` and `memberIds` are also the Electron host's roster names
/// (`docs/research/client-grok-bot.md` §8.1, transcribed from
/// `source/host/extensions/session/session-summaries.ts:13-16`). §8.1 spells the sort key
/// `updatedAt` and the hide flag `isHiddenFromSidebar`; this row keeps NativeChat's primary
/// spellings, because the second of those is not an alias its decoder accepts. The permission keys
/// are server precedent (below).
///
/// Every key is always present, null when unset: a key that is sometimes missing is a shape the
/// app has to guess about. `retired` is not a key because a retired coworker is never a row.
/// `notifyOnUpdates` is absent on purpose: nothing stores it, and echoing a constant would
/// overwrite the toggle the person just moved.
///
/// The permission fields are decided here, per viewer, on every row (ROADMAP 19.2; the shape the
/// pre-deletion roster answered, `git show 0cc3487^:crates/opengrok-server/src/gateway/live.rs`).
/// `mine` is ownership; `canManage` follows it exactly, because management stays with the owner
/// when a coworker is shared — without it a member's client offers edit controls that answer
/// 403; `owner` names the hirer so a shared row can say whose it is.
pub(crate) fn coworker_row(
    view: &opengrok_core::coworker::CoworkerView,
    profile: Option<&serde_json::Value>,
    hidden_from_sidebar: bool,
    owner: &opengrok_store::RosterOwner,
    viewer: &opengrok_core::id::AccountId,
    box_id: Option<&str>,
) -> serde_json::Value {
    let mine = owner.id == *viewer;
    // Blank reads as absent, the way `Persona::compose` reads the same blob: a cleared title is
    // a coworker with no title, not one called "".
    let decorated = |key: &str| {
        profile
            .and_then(|profile| profile.get(key))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    serde_json::json!({
        "id": view.id.as_str(),
        "name": view.name,
        "model": view.model,
        "effort": view.effort.as_str(),
        // Null is the person's own setting; never left out, as an app reads that as an older server.
        "source": view.source.map(SourceKind::as_str),
        "role": view.role,
        "title": decorated("title"),
        "avatarShape": decorated("avatarShape"),
        "avatarColor": decorated("avatarColor"),
        "visibility": view.visibility.as_str(),
        "hiddenFromSidebar": hidden_from_sidebar,
        "updatedAtMs": view.updated_at_ms,
        // The hirer's computer, so null on a shared row: a member's turns resolve a box from
        // the member's own scope, and every computer route answers them 404, so the owner's id
        // here would name a machine the row's reader can neither open nor work on. Null is
        // already the row's word for "no computer" (a hire with none answers it). The SCOPE'S
        // box (`provision::live_boxes`), never the one frozen on the coworker's row (#302).
        "boxId": box_id.filter(|_| mine),
        "isGroup": !view.members.is_empty(),
        "memberIds": view.members.iter().map(CoworkerId::as_str).collect::<Vec<_>>(),
        "mine": mine,
        "canManage": mine,
        "owner": {
            "id": owner.id.as_str(),
            "name": format!("{} {}", owner.first_name, owner.last_name).trim(),
        },
    })
}

/// `DELETE /coworkers/{id}` — retire this coworker. Same ownership 404 as every other
/// per-coworker route: an id that is not yours is indistinguishable from one that does not exist.
pub async fn delete_coworker(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    let owns = state
        .auth
        .store
        .coworkers_for(&account_id)
        .await
        .map(|roster| roster.iter().any(|view| view.id == coworker_id))
        .unwrap_or(false);
    if !owns {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }
    let Ok((loaded, seq)) = state.auth.store.load_coworker(&coworker_id).await else {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    };
    let at_ms = now_ms();
    let mut after = loaded;
    let Ok(events) = after.decide(CoworkerCommand::Retire { at_ms }) else {
        return (StatusCode::CONFLICT, "that coworker has retired").into_response();
    };
    for event in &events {
        after.apply(event);
    }
    let view = CoworkerView::of(coworker_id.clone(), &after, at_ms);
    if state
        .auth
        .store
        .append_coworker(&coworker_id, &account_id, seq, &events, &view)
        .await
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not save").into_response();
    }
    let _ = state
        .auth
        .store
        .set_coworker_hidden(&account_id, &coworker_id, false, at_ms)
        .await;
    crate::agui::provision::teardown_computer_for(&state, &account_id, &coworker_id).await;
    crate::spend::revoke_for(&state, &coworker_id).await;
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Debug, Deserialize)]
pub struct HireRequest {
    pub name: String,
    /// A route through the gateway, never a key.
    #[serde(default)]
    pub model: Option<String>,
    /// One of the org's coworker templates (`templates.rs`): its model when none is given here,
    /// its tool ceiling and approval set, its spend limits — copied at hire. camelCase on the
    /// wire like the rest of this API; the snake_case name once made the picker a silent no-op.
    #[serde(default, rename = "templateId")]
    pub template_id: Option<String>,
}

/// Hire a coworker, and optionally give it a computer.
///
/// The account comes from the bearer token, never from the body: a client that could name an
/// account could hire into somebody else's roster.
pub async fn hire(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Json(request): Json<HireRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };

    // The hirer as the roster will name them, read BEFORE anything is written: the reply is a
    // roster row, and a read failing after the hire committed could only answer 503 over a
    // coworker that exists — which a client retries into a second hire.
    let owner = match state.auth.store.roster_owner(&account_id).await {
        Ok(owner) => owner,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };

    let coworker_id = CoworkerId::new();
    let at_ms = now_ms();
    // A template, when named, must be the hirer's org's: somebody else's reads as "no such
    // template", never as a hire on the deployment's defaults.
    let template = match request.template_id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => {
            match crate::templates::for_account(&state, &account_id, id).await {
                Ok(Some(template)) => Some(template),
                Ok(None) => return (StatusCode::NOT_FOUND, "no such template").into_response(),
                Err(error) => {
                    return (StatusCode::SERVICE_UNAVAILABLE, error).into_response();
                }
            }
        }
        _ => None,
    };
    // Absent OR blank falls back: `unwrap_or_else` alone let `"model": ""` through, and the
    // aggregate would (now) refuse it rather than the caller getting the default they meant.
    // The template's pin sits between the request's and the hirer's default for new bots (#318).
    let named = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .or_else(|| template.as_ref().and_then(|t| t.model.clone()));
    let (model, default) = match crate::inference::hire_model(&state, &account_id, named).await {
        Ok(chosen) => chosen,
        Err(refusal) => return refusal,
    };

    let mut coworker = opengrok_core::coworker::Coworker::default();
    let mut events = match coworker.decide(CoworkerCommand::Hire {
        name: request.name.clone(),
        model: model.clone(),
        at_ms,
    }) {
        Ok(events) => events,
        Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
    };
    for event in &events {
        coworker.apply(event);
    }
    // The template's standing role, in the SAME append as the hire: a second write could fail
    // after the coworker exists, leaving a coworker the template promised a role and did not get.
    if let Some(role) = template.as_ref().and_then(|t| t.role.clone()) {
        let more = match coworker.decide(CoworkerCommand::SetRole {
            role: Some(role),
            at_ms,
        }) {
            Ok(more) => more,
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        };
        for event in &more {
            coworker.apply(event);
        }
        events.extend(more);
    }
    // Born on the whole default when it gave the pin: its door and effort, in this same append.
    if let Some(default) = &default {
        match coworker.born_on(default, at_ms) {
            Ok(more) => events.extend(more),
            Err(error) => return (StatusCode::BAD_REQUEST, error.to_string()).into_response(),
        }
    }

    // A computer, if asked for — via the shared helper (every create path's, before P0-E left only
    // this one). A failure leaves a boxless-but-hired coworker; the reason is in the reply.
    // 1 account = 1 computer: the account's first agent creates it, later agents share it.
    let provisioned =
        provision::ensure_computer_for(&state, &account_id, &coworker_id, &mut coworker, at_ms)
            .await;
    events.extend(provisioned.events);
    // Stamped with NOW, because this one is fresh by construction — it is the error from the
    // provisioning attempt this very request just made. Every `computerError` on the wire carries
    // `updatedAtMs` or the client cannot tell which of them it may trust; a field that is
    // sometimes present is worse than one that never is.
    let computer_error = provisioned
        .error
        .map(|(code, message)| (code, message, at_ms));
    // A key of its own, so a cap can be written on it. Never fails the hire; the console says
    // why when it could not be minted.
    let _key =
        crate::spend::ensure_key_for(&state, &account_id, &coworker_id, &coworker.name).await;

    let view = CoworkerView::of(coworker_id.clone(), &coworker, at_ms);

    if let Err(error) = state
        .auth
        .store
        .append_coworker(&coworker_id, &account_id, 0, &events, &view)
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }

    // Hiring grants the hirer access to what they hired. Written explicitly rather than implied by
    // ownership: "the owner may do anything" is the rule that has no seam to narrow later, and
    // coworker-to-coworker delegation will need one.
    //
    // The ceiling starts at the tools this server implements without any plugin, not `All`: a
    // coworker's limits should be a list somebody can read, and `All` would silently include
    // whatever is added next. The person's machine is among them, as every coworker could reach it
    // before its ceiling governed it (#268). A plugin must widen it later, so "install a plugin"
    // and "let this coworker use it" stay two decisions rather than one.
    let tools = opengrok_policy::ToolSet::only(opengrok_tools::Executor::every_builtin());
    let mut template_note: Option<String> = None;
    let granted = match template.as_ref() {
        // Hired from a template: the template's ceiling, approval set and limits, copied. A
        // limit that could not be copied comes back as a note for the hirer.
        Some(template) => match crate::templates::apply_at_hire(
            &state,
            &account_id,
            &coworker_id,
            template,
            at_ms,
        )
        .await
        {
            Ok(note) => {
                template_note = note;
                Ok(())
            }
            Err(error) => Err(opengrok_store::StoreError::Corrupt(error)),
        },
        None => {
            state
                .auth
                .store
                // Nothing needs approval by default. A person who wants a second pair of eyes on
                // `shell` sets it deliberately; defaulting to "approve everything" would make the
                // prompt noise and teach people to click yes.
                .grant_access(
                    &account_id,
                    &coworker_id,
                    &tools,
                    &tools,
                    &opengrok_policy::ToolSet::None,
                    at_ms,
                )
                .await
        }
    };
    if let Err(error) = granted {
        // A coworker nobody may use is worse than no coworker: fail the hire rather than leave one
        // that silently refuses everything.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("the coworker was created but could not be granted: {error}"),
        )
            .into_response();
    }

    // The roster's row, so a hired coworker does not change shape the first time the app
    // relaunches onto `GET /coworkers`; the two keys only a hire can answer ride on top of it.
    // Not hidden: nobody can have hidden an id minted a moment ago.
    let profile = template.as_ref().and_then(crate::templates::hire_profile);
    // The box the claim just gave it, which is the scope's.
    let box_id = view.box_id.as_ref().map(|id| id.as_str());
    let mut row = coworker_row(&view, profile.as_ref(), false, &owner, &account_id, box_id);
    if let Some(row) = row.as_object_mut() {
        row.insert(
            "computerError".to_string(),
            provision::error_json_at(&computer_error),
        );
        // A sentence when something the template promised did not land; null otherwise.
        row.insert("templateNote".to_string(), serde_json::json!(template_note));
    }
    (StatusCode::CREATED, Json(row)).into_response()
}

#[derive(Debug, Deserialize)]
pub struct ApprovalsRequest {
    /// Tools this coworker may only run with a human yes. An empty list means none.
    #[serde(default)]
    pub tools: Vec<String>,
}

/// Say which of a coworker's tools need a person to approve them — PLAN §4.5 layer 5.
///
/// Set by the person who holds the grant, on their own grant: approval is about what *they* may
/// have done without asking, so it is a property of the grant and not of the coworker.
pub async fn set_approvals(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(coworker_id): Path<String>,
    Json(request): Json<ApprovalsRequest>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);

    // Only somebody who already holds a grant may change its approval list — otherwise this would
    // be a way to create a grant, which is a different permission entirely.
    let policy = match state.auth.store.policy_for(&account_id, &coworker_id).await {
        Ok(policy) => policy,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let decision = opengrok_policy::decide(
        &account_id,
        &coworker_id,
        opengrok_policy::Action::UseCoworker,
        &policy,
    );
    if let Some(reason) = decision.reason() {
        return refuse_use(&state, &account_id, &coworker_id, reason)
            .await
            .into_response();
    }

    // `use_skill` NEVER NEEDS A YES, and listing it would promise a card that never comes: it only
    // reads skills its owner attached, is answered before any gate (`ToolRunner::run_one`), and
    // raises no card. Refused, not dropped, so the list a client saved is the list that stands.
    if request
        .tools
        .iter()
        .any(|tool| tool == opengrok_tools::skill::USE_SKILL)
    {
        let why = "use_skill never needs a yes: it only reads skills its owner attached";
        return crate::health::refusal(422, why);
    }
    let needs_approval = if request.tools.is_empty() {
        opengrok_policy::ToolSet::None
    } else {
        opengrok_policy::ToolSet::only(request.tools.clone())
    };

    // ONLY what needs a yes is written, on the grant as it is now. This used to write back the
    // profile and the ceiling it had just read, and a ceiling save that landed in between was
    // undone: a tool its owner switched off came back on (#268).
    let store = &state.auth.store;
    match store
        .set_needs_approval(&account_id, &coworker_id, &needs_approval, now_ms())
        .await
    {
        Ok(true) => {}
        Ok(false) => return (StatusCode::FORBIDDEN, "no grant to change").into_response(),
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }

    Json(serde_json::json!({
        "coworkerId": coworker_id.as_str(),
        "needsApproval": request.tools,
    }))
    .into_response()
}

/// A refused use of a coworker, answered the way every per-coworker route answers: 404 when it
/// is not on the caller's roster, so an outsider's probe reads the same for a coworker that
/// exists and one that does not (#175) — and 403 with the rule's reason when it is on their
/// roster, because somebody who can see it already knows it exists, and a refusal they can read
/// is one they can act on (CLAUDE.md #8).
///
/// Asked only after the policy has refused, so it can never turn a refusal into an allow. A
/// store error keeps the 403: its reason is the no-grant sentence an unknown id gets too, so it
/// confirms nothing either.
pub(crate) async fn refuse_use(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    reason: &str,
) -> (StatusCode, String) {
    let may = state.auth.store.may_use_coworker(account_id, coworker_id);
    match may.await {
        Ok(false) => (StatusCode::NOT_FOUND, "no such coworker".to_string()),
        Ok(true) | Err(_) => (StatusCode::FORBIDDEN, reason.to_string()),
    }
}

/// The roster, newest first — the order the client sorts by.
pub async fn list_coworkers(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    // `roster_for`, not `coworkers_for`: the roster is what this person may TALK to, including
    // what an org-mate shared; `coworkers_for` is what they may manage, and stays owner-only.
    let coworkers = match state.auth.store.roster_for(&account_id).await {
        Ok(coworkers) => coworkers,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let hidden = match state.auth.store.hidden_coworker_ids(&account_id).await {
        Ok(ids) => ids,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    // A 503 rather than rows without their decoration: a roster that answers 200 with every
    // title and avatar quietly gone is the app repainting a correct sidebar as a wrong one.
    let ids: Vec<CoworkerId> = coworkers.iter().map(|(view, _)| view.id.clone()).collect();
    let profiles = match state.auth.store.seamb_profiles(&ids).await {
        Ok(profiles) => profiles,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let own: Vec<&CoworkerView> = coworkers
        .iter()
        .filter_map(|(view, owner)| (owner.id == account_id).then_some(view))
        .collect();
    let live = provision::live_boxes(&state, &account_id, &own).await;
    // An ARRAY, always. An empty roster is a valid answer and must not become null or an
    // object — the desktop client throws on a malformed array reply (RUNBOOK §4).
    let rows: Vec<serde_json::Value> = coworkers
        .iter()
        .map(|(view, owner)| {
            coworker_row(
                view,
                profiles.get(view.id.as_str()),
                hidden.contains(view.id.as_str()),
                owner,
                &account_id,
                live.get(&view.id).map(String::as_str),
            )
        })
        .collect();
    Json(rows).into_response()
}

/// Whose account this is, from the bearer token. Never from the body.
pub(crate) fn account_from_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Option<opengrok_core::id::AccountId> {
    // Header OR the console's httpOnly cookie. The browser cannot send an Authorization header on
    // its own and must never hold a token in JS, so a cookie is the only way it can reach these —
    // it is the same access token, verified the same way, which is what account_api already does.
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_string)
        .or_else(|| {
            crate::auth::cookies::read_cookie(headers, crate::auth::cookies::ACCESS_COOKIE)
        })?;
    // Name the failure, as the seam-A path does. `.ok()?` here meant a mint refused with
    // "a signed access token is required" and no record of WHY — ExpiredSignature and
    // InvalidSignature are different bugs belonging to different people, and the caller cannot
    // tell you which because it only sees the 401.
    match state.auth.minter.verify_access(&token) {
        Ok(claims) => Some(opengrok_core::id::AccountId::from_stored(claims.sub)),
        Err(error) => {
            tracing::warn!(%error, token_len = token.len(), "a bearer access token did not verify");
            None
        }
    }
}

/// Whose account this is, from an `Authorization: Bearer` header and nothing else: the
/// console's cookie does not count here. For the doors only the person's own app may open.
pub(crate) fn account_from_header_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Option<opengrok_core::id::AccountId> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    match state.auth.minter.verify_access(token) {
        Ok(claims) => Some(opengrok_core::id::AccountId::from_stored(claims.sub)),
        Err(error) => {
            tracing::warn!(%error, token_len = token.len(), "a bearer access token did not verify");
            None
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Mint a durable key that lets a client Bot run AS this coworker.
///
/// Access tokens live an hour; a Bot registered in a client's vault with a static header dies
/// hourly. This key is signed like everything else but LONG-lived, because its real lifecycle
/// control is the revocable row — showing the token once at mint is the only time it exists in
/// a reply, the same bargain every credential here makes.
pub(super) async fn mint_bot_key(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(coworker_id): axum::extract::Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    // Owner-and-permitted, or a 404 that does not confirm the coworker exists.
    let owns = state
        .auth
        .store
        .coworkers_for(&account_id)
        .await
        .map(|roster| {
            roster
                .iter()
                .any(|view| view.id == coworker_id && !view.retired)
        })
        .unwrap_or(false);
    if !owns {
        return (StatusCode::NOT_FOUND, "no such coworker").into_response();
    }

    let minted = match crate::auth::bot_keys::mint(
        &state.auth.store,
        &state.auth.minter,
        &account_id,
        &coworker_id,
        "bot key",
        None,
        crate::auth::bot_keys::HAND_MINTED_TTL_SECS,
    )
    .await
    {
        Ok(minted) => minted,
        Err(error) => {
            tracing::error!(%error, "could not mint a bot key");
            return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response();
        }
    };
    let jti = minted.jti;
    let token = minted.token;
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "jti": jti,
            "coworkerId": coworker_id.as_str(),
            // Shown exactly once. The row keeps the jti; the token is the caller's to keep.
            "key": token,
        })),
    )
        .into_response()
}

/// `GET /templates` — the caller's org's coworker templates; `[]` outside any org.
pub(super) async fn list_templates(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let org_id = match state.auth.store.load_account(&account_id).await {
        Ok((account, _)) => account.org_id.filter(|org| !org.is_empty()),
        Err(error) => {
            tracing::error!(%error, "could not load the account");
            return (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response();
        }
    };
    let Some(org_id) = org_id else {
        return Json(serde_json::json!({ "templates": [] })).into_response();
    };
    match state.auth.store.templates_for_org(&org_id).await {
        Ok(templates) => Json(serde_json::json!({
            "templates": templates.iter().map(crate::templates::template_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(error) => {
            tracing::error!(%error, "could not list templates");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response()
        }
    }
}

/// Is this coworker the caller's? `None` ⇒ 404: another account's coworker id must read as
/// "no such coworker", never as an empty or refused one.
pub(crate) async fn owned_coworker(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
) -> Result<bool, Response> {
    match state.auth.store.coworkers_for(account_id).await {
        Ok(coworkers) => Ok(coworkers.iter().any(|c| c.id == *coworker_id)),
        Err(error) => {
            tracing::error!(%error, "could not list coworkers");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "storage failed").into_response())
        }
    }
}

/// The caller and the coworker in the path, when it is the caller's own: 401 without a bearer,
/// 404 for anyone else's — the opening of every owner-only coworker route.
pub(super) async fn owner_of(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
    coworker_id: String,
) -> Result<(AccountId, CoworkerId), Response> {
    let Some(account_id) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let coworker_id = CoworkerId::from_stored(coworker_id);
    match owned_coworker(state, &account_id, &coworker_id).await? {
        true => Ok((account_id, coworker_id)),
        false => Err((StatusCode::NOT_FOUND, "no such coworker").into_response()),
    }
}
