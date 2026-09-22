//! Recipes: tasks a person taught on a coworker's screen, kept in versions, owned by the
//! teacher, shared to people or an org, granted to the bots that may run them.
//!
//! Who may do what (`may`), in one place, so every route and the agent's tool refuse alike:
//! - the OWNER reads, renames, adds versions, shares, unshares, grants to their own bots, runs
//!   on their own bots, deletes;
//! - a RECIPIENT (a person who accepted a share, directly or through their org) reads, grants
//!   to their own bots, runs on their own bots — and never shares onward, edits or deletes;
//! - an org member who has not accepted yet sees the pending share and may accept or decline.
//!
//! WORKFLOWS ARE THESE ROWS, AND THESE RULES (`crate::workflows`). A decision tree is a
//! `recipe_version` of kind `workflow` on an ordinary `recipe` row, so reading one, renaming it,
//! sharing it, granting it to a bot, deleting it and reading its run history are these routes,
//! unchanged. `permitted` and `may` below are `pub(crate)` for exactly that: the workflow routes
//! call them rather than growing a second set that could refuse differently.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_recipes::{Parameter, Screen, Step, TapeEvent, Values};
use opengrok_store::{PgStore, RecipeRow};
use opengrok_tools::{RecipeReceipt, RecipeSource};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::agui::routes::{account_from_bearer, owned_coworker};

/// The raw tape is capped so a runaway teach cannot fill the table.
const MAX_RAW_BYTES: usize = 5 * 1024 * 1024;

/// How much of a raw tape a detail response carries. Enough to read what was taped, few
/// enough that a long teach does not push a megabyte of pointer moves through the page.
const RAW_EVENTS_SENT: usize = 2_000;

/// How many runs of one version the history keeps. Older ones are dropped as new ones land,
/// so the page's five rows per version are the whole table, not a window onto an endless one.
const RUNS_KEPT_PER_VERSION: i64 = 5;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/recipes", get(list).post(create))
        .route("/recipes/{id}", get(detail).put(rename).delete(remove))
        .route("/recipes/{id}/versions", post(add_version))
        .route("/recipes/{id}/versions/{version}", delete(remove_version))
        .route("/recipes/{id}/share", post(share))
        .route("/recipes/{id}/share/{scope}/{scope_id}", delete(unshare))
        .route("/recipes/{id}/accept", post(accept))
        .route("/recipes/{id}/decline", post(decline))
        .route("/recipes/{id}/grants", post(grant))
        .route("/recipes/{id}/grants/{coworker_id}", delete(revoke))
        .route("/recipes/{id}/run", post(run))
        .route("/admin/recipes", get(admin_list))
        .with_state(state)
}

/// What a caller is to a recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relation {
    Owner,
    /// Accepted a share, directly or through the org.
    Recipient,
    /// The share is there, unanswered.
    Invited,
    None,
}

/// What a caller wants to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Read,
    Edit,
    Share,
    Grant,
    Run,
    Delete,
    Answer,
}

async fn relation(
    state: &AgUiState,
    account: &AccountId,
    org: Option<&str>,
    recipe: &RecipeRow,
) -> Relation {
    if recipe.owner_id == account.as_str() {
        return Relation::Owner;
    }
    let store = &state.auth.store;
    if store
        .recipe_accepted_by(&recipe.id, account.as_str())
        .await
        .unwrap_or(false)
    {
        return Relation::Recipient;
    }
    let shares = store.recipe_shares(&recipe.id).await.unwrap_or_default();
    let invited = shares.iter().any(|share| {
        share.declined_at_ms.is_none()
            && ((share.scope == "account" && share.scope_id == account.as_str())
                || (share.scope == "org" && Some(share.scope_id.as_str()) == org))
    });
    if invited {
        Relation::Invited
    } else {
        Relation::None
    }
}

/// The one answer to "may this person do that to this recipe".
pub(crate) fn may(relation: Relation, action: Action) -> Result<(), &'static str> {
    use Action::*;
    use Relation::*;
    let ok = matches!(
        (relation, action),
        (Owner, _) | (Recipient, Read | Grant | Run | Answer) | (Invited, Read | Answer)
    );
    if ok {
        Ok(())
    } else {
        Err(match relation {
            None => "no such recipe",
            Invited => "accept the share first",
            Recipient => "only the recipe's owner may do that",
            Owner => "refused",
        })
    }
}

/// The org this person belongs to, or `None`.
///
/// AN EMPTY ORG IS NO ORG, and the filter is load-bearing rather than tidiness: `Register` takes
/// `org_id` as a plain `String`, so an account created without one replays to `Some("")` rather
/// than `None`. Two such people then have equal orgs, and anything that decides "same org, may
/// read" from that comparison makes every person in no org a colleague of every other. Caught by
/// `against_another_persons_skill`, where a stranger read a stranger's skill.
pub(crate) async fn org_of(state: &AgUiState, account: &AccountId) -> Option<String> {
    state
        .auth
        .store
        .load_account(account)
        .await
        .ok()
        .and_then(|(account, _)| account.org_id)
        .filter(|org| !org.is_empty())
}

/// Loads the recipe and checks the action; a deleted recipe is a 404 to everyone but its owner.
pub(crate) async fn permitted(
    state: &AgUiState,
    headers: &HeaderMap,
    id: &str,
    action: Action,
) -> Result<(AccountId, Option<String>, RecipeRow, Relation), Response> {
    let Some(account) = account_from_bearer(state, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "sign in first").into_response());
    };
    let org = org_of(state, &account).await;
    let recipe = match state.auth.store.recipe(id).await {
        Ok(Some(recipe)) => recipe,
        Ok(None) => return Err((StatusCode::NOT_FOUND, "no such recipe").into_response()),
        Err(error) => {
            return Err((StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response());
        }
    };
    let relation = relation(state, &account, org.as_deref(), &recipe).await;
    if recipe.deleted_at_ms.is_some() && relation != Relation::Owner {
        return Err((StatusCode::NOT_FOUND, "no such recipe").into_response());
    }
    if let Err(why) = may(relation, action) {
        let status = if relation == Relation::None {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::FORBIDDEN
        };
        return Err((status, why).into_response());
    }
    Ok((account, org, recipe, relation))
}

fn relation_word(relation: Relation) -> &'static str {
    match relation {
        Relation::Owner => "mine",
        Relation::Recipient => "shared",
        Relation::Invited => "invited",
        Relation::None => "none",
    }
}

/// What the version a run would actually play IS, and what it asks for.
///
/// Read from the runnable version rather than the newest, because that is the one a run binds
/// against: a draft edit that declares a third parameter must not make a client ask for
/// something the run will not use.
///
/// One read for both answers on purpose. The kind and the parameters come off the same row, and a
/// listing that fetched them separately would be two round trips per recipe on every page open.
pub(crate) async fn runnable_shape(
    state: &AgUiState,
    recipe_id: &str,
) -> (&'static str, Vec<Parameter>) {
    let Ok(Some(version)) = state.auth.store.recipe_runnable_version(recipe_id).await else {
        return (TAPE, Vec::new());
    };
    let parameters = serde_json::from_value(
        version
            .body
            .get("parameters")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .unwrap_or_default();
    let kind = if version.kind == opengrok_tools::workflow::KIND {
        opengrok_tools::workflow::KIND
    } else {
        TAPE
    };
    (kind, parameters)
}

/// Refuse a row that is the other shape.
///
/// A ROW IS ALL TAPE OR ALL TREE, and it is checked from the versions rather than from a column
/// because there is no column: the kind lives on each version, and "what is this row" is "what
/// have its versions been". A row with no versions yet is whichever the caller says it is, which
/// is the moment between creating the row and writing its first version.
pub(crate) async fn expect_kind(
    state: &AgUiState,
    recipe_id: &str,
    workflow: bool,
) -> Result<(), Response> {
    let versions = state
        .auth
        .store
        .recipe_versions(recipe_id)
        .await
        .unwrap_or_default();
    let has_tree = versions
        .iter()
        .any(|row| row.kind == opengrok_tools::workflow::KIND);
    let has_tape = versions
        .iter()
        .any(|row| row.kind != opengrok_tools::workflow::KIND);
    if workflow && has_tape {
        return Err((
            StatusCode::CONFLICT,
            "that is a taught recipe, not a workflow: a recipe is a taped sequence and a workflow \
             is a decision tree, and one row cannot be both",
        )
            .into_response());
    }
    if !workflow && has_tree {
        return Err((
            StatusCode::CONFLICT,
            "that is a workflow, not a taught recipe: a workflow is a decision tree and a recipe \
             is a taped sequence, and one row cannot be both",
        )
            .into_response());
    }
    Ok(())
}

/// What a row is when it is not a workflow. A word for the wire, not a `recipe_version.kind` —
/// the three tape kinds are versions of one another and a client only ever needs to know which of
/// the two things it is holding.
pub(crate) const TAPE: &str = "recipe";

pub(crate) fn summary(
    recipe: &RecipeRow,
    relation: Relation,
    share_state: Option<&str>,
    kind: &str,
    parameters: &[Parameter],
) -> Value {
    json!({
        "id": recipe.id,
        "ownerId": recipe.owner_id,
        "orgId": recipe.org_id,
        "name": recipe.name,
        "description": recipe.description,
        "screen": { "width": recipe.screen_w, "height": recipe.screen_h },
        "createdAtMs": recipe.created_at_ms,
        "updatedAtMs": recipe.updated_at_ms,
        "deletedAtMs": recipe.deleted_at_ms,
        "latestVersion": recipe.latest_version,
        "relation": relation_word(relation),
        "shareState": share_state,
        // `recipe` or `workflow`. A page has to be able to tell a taped sequence from a decision
        // tree before it offers to edit one, and the two live on the same table.
        "kind": kind,
        // On the listing, not only the detail: a composer that has just been handed a recipe has
        // to know what it needs before it can ask for it, and a second fetch per row to find out
        // is a round trip per recipe on every open.
        "parameters": parameters,
    })
}

/// What `filter` may say. A typo used to fall through every arm and answer an empty list, which
/// is indistinguishable from "nothing has been shared with you" — the reply a person blames their
/// own account for. Shared in spirit with `skills::FILTERS`, not in code: the two listings are
/// free to grow different words.
const FILTERS: [&str; 4] = ["mine", "shared", "org", "all"];

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// `mine` | `shared` | `org` | (absent: everything visible)
    filter: Option<String>,
    /// `recipe` | `workflow` | (absent: both). A QUERY RATHER THAN A SECOND LISTING ROUTE: the
    /// rows, the relations and the share states are identical, and `GET /workflows` would be this
    /// handler with one line different and its own bugs.
    kind: Option<String>,
}

/// `GET /recipes?filter=mine|shared|org&kind=recipe|workflow`
async fn list(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let org = org_of(&state, &account).await;
    let store = &state.auth.store;
    let filter = query
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|filter| !filter.is_empty())
        .unwrap_or("all");
    if !FILTERS.contains(&filter) {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "{filter:?} is not a filter; it is one of {}, or leave it off for everything",
                FILTERS.join(", ")
            ),
        )
            .into_response();
    }
    let wanted = query
        .kind
        .as_deref()
        .map(str::trim)
        .filter(|kind| !kind.is_empty());
    let mut out = Vec::new();
    if matches!(filter, "mine" | "all") {
        match store.recipes_owned_by(account.as_str()).await {
            Ok(rows) => {
                for row in &rows {
                    let (kind, params) = runnable_shape(&state, &row.id).await;
                    if wanted.is_some_and(|wanted| wanted != kind) {
                        continue;
                    }
                    out.push(summary(row, Relation::Owner, None, kind, &params));
                }
            }
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        }
    }
    if matches!(filter, "shared" | "org" | "all") {
        // NOT gated on the caller having an org, deliberately: this call answers BOTH the shares
        // made to them by name and the ones made to their org, and a person in no org can still
        // hold the first kind. The org arm is gated inside the query instead, where a NULL org
        // matches no row — see `recipes_shared_with`.
        match store
            .recipes_shared_with(account.as_str(), org.as_deref())
            .await
        {
            Ok(rows) => {
                let mut seen = std::collections::HashSet::new();
                for (row, share) in rows {
                    let via_org = share.scope == "org";
                    if filter == "org" && !via_org {
                        continue;
                    }
                    if filter == "shared" && via_org {
                        continue;
                    }
                    if !seen.insert(row.id.clone()) {
                        continue;
                    }
                    // A person's own answer wins over the org-wide row.
                    let accepted = store
                        .recipe_accepted_by(&row.id, account.as_str())
                        .await
                        .unwrap_or(false);
                    let (relation, share_state) = if accepted {
                        (Relation::Recipient, "accepted")
                    } else if share.declined_at_ms.is_some() {
                        (Relation::None, "declined")
                    } else {
                        (Relation::Invited, "pending")
                    };
                    let (kind, params) = runnable_shape(&state, &row.id).await;
                    if wanted.is_some_and(|wanted| wanted != kind) {
                        continue;
                    }
                    out.push(summary(&row, relation, Some(share_state), kind, &params));
                }
            }
            Err(error) => {
                return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
            }
        }
    }
    Json(json!({ "recipes": out })).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequest {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    screen: Option<Screen>,
    /// The raw tape (v1).
    raw: Vec<TapeEvent>,
}

/// A raw tape, bounded, filtered and linted: the tape as it was taped, and the steps read off
/// it. The one road from a tape to steps.
///
/// `pub(crate)` AND SHARED WITH `skills::from_tape`, which takes the same tape off the same
/// desktop recorder and makes prose of it instead of a recipe. Two copies of this would be two
/// answers to "is this tape usable" — and the copy that drifted would be the one that fed a
/// model, which is the reader least able to complain about a malformed tape.
///
/// The serialised tape is handed back rather than re-derived, because `create` stores it as the
/// `raw` version and serialising a 5 MB tape twice is the kind of waste nothing downstream sees.
///
/// The refusal is a status and a sentence rather than a built `Response`, so a caller can add to
/// it — and so this stays a function about tapes rather than about HTTP.
pub(crate) fn tape_into_steps(
    raw: &[TapeEvent],
    screen: Screen,
) -> Result<(Value, Vec<Step>), (StatusCode, String)> {
    let value =
        serde_json::to_value(raw).map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    if value.to_string().len() > MAX_RAW_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "the tape is over 5 MB; teach a shorter task".to_string(),
        ));
    }
    let steps = opengrok_recipes::filter(raw, screen);
    if let Err(why) = opengrok_recipes::lint(&steps, screen) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            // NOT "did not filter into a recipe": the same sentence now reaches somebody who
            // asked for a skill, and naming the other outcome would send them looking at the
            // recipe they never asked for.
            format!("the tape did not filter into usable steps: {why}"),
        ));
    }
    Ok((value, steps))
}

/// `POST /recipes` — the tape in, v1 and v2 written, the recipe out.
async fn create(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(request): Json<CreateRequest>,
) -> Response {
    let Some(account) = account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let name = request.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "a recipe needs a name").into_response();
    }
    let screen = request.screen.unwrap_or_default();
    let (raw, steps) = match tape_into_steps(&request.raw, screen) {
        Ok(both) => both,
        Err(refusal) => return refusal.into_response(),
    };
    let org = org_of(&state, &account).await;
    let id = format!("rcp_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();
    let store = &state.auth.store;
    if let Err(error) = store
        .create_recipe(
            &id,
            account.as_str(),
            org.as_deref(),
            name,
            request.description.trim(),
            (screen.width, screen.height),
            at_ms,
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    let _ = store
        .add_recipe_version(
            &id,
            "raw",
            &raw,
            "the tape as taught",
            account.as_str(),
            at_ms,
        )
        .await;
    let body = json!({ "steps": steps, "stop_on_error": true, "screenshot": "end" });
    let _ = store
        .add_recipe_version(
            &id,
            "filtered",
            &body,
            "the tape filtered into steps",
            account.as_str(),
            at_ms,
        )
        .await;
    detail_body(&state, &account, org.as_deref(), &id).await
}

pub(crate) async fn detail_body(
    state: &AgUiState,
    account: &AccountId,
    org: Option<&str>,
    id: &str,
) -> Response {
    let store = &state.auth.store;
    let Ok(Some(recipe)) = store.recipe(id).await else {
        return (StatusCode::NOT_FOUND, "no such recipe").into_response();
    };
    let relation = relation(state, account, org, &recipe).await;
    let versions = store.recipe_versions(id).await.unwrap_or_default();
    let grants = store.recipe_grants(id).await.unwrap_or_default();
    let runs = store.recipe_runs(id, 50).await.unwrap_or_default();
    let shares = if relation == Relation::Owner {
        store.recipe_shares(id).await.unwrap_or_default()
    } else {
        Vec::new()
    };
    // The caller's own bots, so the page can offer "grant to…" without a second call.
    let my_bots: Vec<Value> = store
        .coworkers_for(account)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|view| !view.retired && view.members.is_empty())
        .map(|view| json!({ "id": view.id.as_str(), "name": view.name }))
        .collect();
    // A run carries what it produced, so the history can show a step's picture and play a
    // recording without a second round trip per row. The bytes themselves are never in here —
    // these are ids the page turns into URLs.
    let mut runs_with_artifacts = Vec::with_capacity(runs.len());
    for run in &runs {
        let of_run = store
            .artifacts_for_run(id, &run.id)
            .await
            .unwrap_or_default();
        let mut row = serde_json::to_value(run).unwrap_or_else(|_| json!({}));
        if let Some(object) = row.as_object_mut() {
            object.insert(
                "artifacts".to_string(),
                json!(
                    of_run
                        .iter()
                        .map(|artifact| json!({
                            "id": artifact.id,
                            "kind": artifact.kind,
                            "mime": artifact.mime,
                            "stepIndex": artifact.step_index,
                            "sizeBytes": artifact.size_bytes,
                            "meta": artifact.meta,
                        }))
                        .collect::<Vec<_>>()
                ),
            );
        }
        runs_with_artifacts.push(row);
    }

    let (kind, parameters) = runnable_shape(state, id).await;
    Json(json!({
        "recipe": summary(&recipe, relation, None, kind, &parameters),
        "versions": versions.iter().map(|version| json!({
            "version": version.version,
            "kind": version.kind,
            "note": version.note,
            "createdBy": version.created_by,
            "createdAtMs": version.created_at_ms,
            // The raw tape travels with its count, so a page can show what was taped and not
            // merely how much of it there was. A tape is capped at 5 MB on the way in but can
            // still be tens of thousands of moves, which is neither readable nor worth sending,
            // so a long one is cut and says so; the whole tape stays in the store either way.
            "body": if version.kind == "raw" {
                let events = version.body.as_array().cloned().unwrap_or_default();
                let total = events.len();
                let shown: Vec<Value> = events.into_iter().take(RAW_EVENTS_SENT).collect();
                json!({
                    "events": total,
                    "tape": shown,
                    "truncated": total > RAW_EVENTS_SENT,
                })
            } else {
                version.body.clone()
            },
        })).collect::<Vec<_>>(),
        "shares": shares,
        "grants": grants,
        "runs": runs_with_artifacts,
        "myBots": my_bots,
    }))
    .into_response()
}

/// `GET /recipes/{id}`
async fn detail(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    match permitted(&state, &headers, &id, Action::Read).await {
        Ok((account, org, _, _)) => detail_body(&state, &account, org.as_deref(), &id).await,
        Err(refusal) => refusal,
    }
}

#[derive(Debug, Deserialize)]
struct RenameRequest {
    name: String,
    #[serde(default)]
    description: String,
}

/// `PUT /recipes/{id}` — name and description; the owner's.
async fn rename(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<RenameRequest>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Edit).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let name = request.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "a recipe needs a name").into_response();
    }
    if let Err(error) = state
        .auth
        .store
        .rename_recipe(&id, name, request.description.trim(), now_ms())
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionRequest {
    steps: Vec<Step>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    parameters: Option<Vec<Parameter>>,
}

/// `POST /recipes/{id}/versions` — an edited version; the owner's.
async fn add_version(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<VersionRequest>,
) -> Response {
    let (account, org, recipe, _) = match permitted(&state, &headers, &id, Action::Edit).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    // A ROW IS ALL TAPE OR ALL TREE. `recipe_runnable_version` is "the newest version that is not
    // the raw tape", so a `filtered` version added on top of a workflow would quietly become what
    // every run of that workflow plays. The refusal says which it is rather than letting the next
    // run hand a decision tree to the box's step runner.
    if let Err(refusal) = expect_kind(&state, &id, false).await {
        return refusal;
    }
    let screen = Screen {
        width: recipe.screen_w,
        height: recipe.screen_h,
    };
    if let Err(why) = opengrok_recipes::lint(&request.steps, screen) {
        return (StatusCode::UNPROCESSABLE_ENTITY, why.to_string()).into_response();
    }
    // The DECLARATION is judged here, not the values: `bind` answers "may this run?", and a
    // required parameter with nothing supplied rightly fails that — which made declaring one
    // impossible. Values are judged at run time, where they exist.
    let params = request.parameters.as_deref().unwrap_or(&[]);
    if let Err(why) = opengrok_recipes::check(params) {
        return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response();
    }
    let mut body = json!({ "steps": request.steps, "stop_on_error": true, "screenshot": "end" });
    // Only written when there are some, so a version with no parameters keeps the exact body
    // shape it has today and nothing downstream has to special-case an empty list.
    if let Some(params) = &request.parameters
        && let Some(object) = body.as_object_mut()
    {
        object.insert(
            "parameters".to_string(),
            serde_json::to_value(params).unwrap_or(json!([])),
        );
    }
    let note = if request.note.trim().is_empty() {
        "edited"
    } else {
        request.note.trim()
    };
    if let Err(error) = state
        .auth
        .store
        .add_recipe_version(&id, "edited", &body, note, account.as_str(), now_ms())
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

/// `DELETE /recipes/{id}/versions/{version}` — one edited version; the owner's.
///
/// The tape and the filtered steps read off it are what the recipe IS: the tape is the record
/// of what a person did, and the filtered version is that tape turned into steps. Everything
/// after them is an edit, and an edit is the only thing there is to take back. Deleting the
/// newest edit leaves the one before it running, which is what falling back means here.
async fn remove_version(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, i32)>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Edit).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let store = &state.auth.store;
    let found = match store.recipe_version(&id, version).await {
        Ok(Some(found)) => found,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such version").into_response(),
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    // A WORKFLOW'S VERSIONS FALL BACK THE SAME WAY AN EDIT DOES — deleting the newest tree leaves
    // the one before it running — EXCEPT THE LAST ONE, which is not an edit of anything: taking it
    // away leaves a row with nothing to run and no tape to fall back to.
    if found.kind == opengrok_tools::workflow::KIND {
        let trees = store
            .recipe_versions(&id)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|row| row.kind == opengrok_tools::workflow::KIND)
            .count();
        if trees <= 1 {
            return (
                StatusCode::CONFLICT,
                format!(
                    "v{version} is the only version of this workflow; delete the workflow instead"
                ),
            )
                .into_response();
        }
    } else if found.kind != "edited" {
        return (
            StatusCode::CONFLICT,
            format!(
                "v{version} is the {}, which is what this recipe was taught; only an edited version can be deleted",
                if found.kind == "raw" { "tape as taught" } else { "tape filtered into steps" }
            ),
        )
            .into_response();
    }
    if let Err(error) = store.delete_recipe_version(&id, version).await {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

/// `DELETE /recipes/{id}` — soft; the owner's.
async fn remove(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(refusal) = permitted(&state, &headers, &id, Action::Delete).await {
        return refusal;
    }
    match state.auth.store.soft_delete_recipe(&id, now_ms()).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShareRequest {
    /// `account` | `org`
    scope: String,
    /// The account id, or an email in the owner's org, for `account`; ignored for `org`.
    #[serde(default)]
    scope_id: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

/// `POST /recipes/{id}/share` — to a person in the owner's org, or to the org; the owner's.
async fn share(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<ShareRequest>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Share).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let store = &state.auth.store;
    let (scope, scope_id) = match request.scope.as_str() {
        "org" => match org.clone() {
            Some(org) => ("org", org),
            None => return (StatusCode::BAD_REQUEST, "you are not in an org").into_response(),
        },
        "account" => {
            // The target as (id, org): the two facts the policy needs.
            let (target_id, target_org): (AccountId, Option<String>) = if let Some(email) = request
                .email
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
            {
                match store.account_by_email(email).await {
                    Ok(Some(view)) => (view.id, view.org_id),
                    Ok(None) => {
                        return (StatusCode::NOT_FOUND, "no account with that email")
                            .into_response();
                    }
                    Err(error) => {
                        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string())
                            .into_response();
                    }
                }
            } else if let Some(scope_id) = request.scope_id.as_deref() {
                let id = AccountId::from_stored(scope_id.to_string());
                match store.load_account(&id).await {
                    Ok((target, _)) => (id, target.org_id),
                    Err(_) => return (StatusCode::NOT_FOUND, "no such account").into_response(),
                }
            } else {
                return (StatusCode::BAD_REQUEST, "say who: scopeId or email").into_response();
            };
            // Same org only: a recipe carries a person's own screens and habits.
            if target_org.is_none() || target_org != org {
                return (StatusCode::FORBIDDEN, "recipes are shared inside your org")
                    .into_response();
            }
            if target_id == account {
                return (StatusCode::BAD_REQUEST, "that is you").into_response();
            }
            ("account", target_id.as_str().to_string())
        }
        other => {
            return (StatusCode::BAD_REQUEST, format!("unknown scope `{other}`")).into_response();
        }
    };
    if let Err(error) = store
        .share_recipe(&id, scope, &scope_id, account.as_str(), now_ms())
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

/// `DELETE /recipes/{id}/share/{scope}/{scope_id}` — the owner takes a share back.
async fn unshare(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((id, scope, scope_id)): Path<(String, String, String)>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Share).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    if let Err(error) = state
        .auth
        .store
        .unshare_recipe(&id, &scope, &scope_id)
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

async fn answer(state: AgUiState, headers: HeaderMap, id: String, accept: bool) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Answer).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    match state
        .auth
        .store
        .answer_recipe_share(&id, account.as_str(), org.as_deref(), accept, now_ms())
        .await
    {
        Ok(true) => detail_body(&state, &account, org.as_deref(), &id).await,
        Ok(false) => (StatusCode::NOT_FOUND, "nothing was shared with you").into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

/// `POST /recipes/{id}/accept`
async fn accept(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    answer(state, headers, id, true).await
}

/// `POST /recipes/{id}/decline`
async fn decline(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    answer(state, headers, id, false).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrantRequest {
    coworker_id: String,
}

/// `POST /recipes/{id}/grants` — let one of the caller's own bots run it.
async fn grant(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<GrantRequest>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Grant).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let coworker = CoworkerId::from_stored(request.coworker_id.clone());
    match owned_coworker(&state, &account, &coworker).await {
        Ok(true) => {}
        Ok(false) => {
            return (StatusCode::FORBIDDEN, "you may only grant to your own bots").into_response();
        }
        Err(refusal) => return refusal,
    }
    if let Err(error) = state
        .auth
        .store
        .grant_recipe(&id, coworker.as_str(), account.as_str(), now_ms())
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

/// `DELETE /recipes/{id}/grants/{coworker_id}`
async fn revoke(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path((id, coworker_id)): Path<(String, String)>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Grant).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let coworker = CoworkerId::from_stored(coworker_id);
    match owned_coworker(&state, &account, &coworker).await {
        Ok(true) => {}
        Ok(false) => {
            return (StatusCode::FORBIDDEN, "you may only change your own bots").into_response();
        }
        Err(refusal) => return refusal,
    }
    if let Err(error) = state
        .auth
        .store
        .revoke_recipe_grant(&id, coworker.as_str())
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    detail_body(&state, &account, org.as_deref(), &id).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRequest {
    coworker_id: String,
    #[serde(default)]
    values: Option<Values>,
}

/// `POST /recipes/{id}/run` — run it now on one of the caller's bots; the receipt comes back
/// and is written to the recipe's history.
async fn run(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<RunRequest>,
) -> Response {
    let (account, _org, recipe, _) = match permitted(&state, &headers, &id, Action::Run).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let coworker = CoworkerId::from_stored(request.coworker_id.clone());
    match owned_coworker(&state, &account, &coworker).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::FORBIDDEN,
                "you may only run recipes on your own bots",
            )
                .into_response();
        }
        Err(refusal) => return refusal,
    }
    // The bot's box, the way a turn finds it.
    let (_mode, org_id, scope, scope_id, _) =
        crate::agui::provision::scope_of(&state, &account, coworker.as_str()).await;
    let Ok(Some((box_id, kind, _))) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
    else {
        return (StatusCode::CONFLICT, "this bot has no computer yet").into_response();
    };
    let Some(provider) =
        crate::agui::provision::provider_for(&state, org_id.as_deref(), &kind).await
    else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "the computer's provider is not available",
        )
            .into_response();
    };
    let source = StoreRecipes {
        store: state.auth.store.clone(),
    };
    let values = request.values.unwrap_or_default();
    let (version, mut body) = match source.recipe_request(&id, &values).await {
        Ok(found) => found,
        Err(why) => return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response(),
    };

    // The run's id is minted BEFORE the run, because the box writes its files into a directory
    // named after it: a recording and eleven screenshots from two runs of the same recipe would
    // otherwise land on top of each other.
    let run_id = format!("rrun_{}", uuid::Uuid::now_v7());
    // A person watching a run wants to see what it did, so this asks for a picture of every step
    // and a recording of the whole thing. An agent's run does not: it needs the end screenshot as
    // its tool result, and a video of every turn would be bytes nobody opens.
    //
    // Setting `artifact_dir` also makes the box write the pictures to disk INSTEAD of inlining
    // them, so the receipt comes back without `png_base64`. That is the trade: the page gets its
    // images from the artifact routes rather than out of this response.
    //
    // AND IT ASKS THE BOX FOR NO OBSERVATION, unlike a run played for a bot. A person watching
    // gets a picture of every step and a recording of the whole thing, which is strictly more
    // than "which window was under step three" — and the one place an observation could be kept
    // for them to read later, the run history, is the one place it must not be kept (`write_run`).
    // Paying for a reading that is thrown away is not a trade worth making.
    if let Some(object) = body.as_object_mut() {
        object.insert("screenshot".to_string(), json!("each"));
        object.insert("record".to_string(), json!(true));
        object.insert(
            "artifact_dir".to_string(),
            json!(format!("/workspace/.recipe-runs/{run_id}")),
        );
    }

    let receipt = match provider.run_recipe(&box_id, &body).await {
        Ok(raw) => RecipeReceipt::from_value(raw),
        Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
    };

    // Pull what the box made onto the server while the box is still around. A box can be reset
    // or stopped a moment later, and then the files are gone with it.
    let (kept, missed) = crate::artifacts::keep_run_artifacts(
        &state,
        &account,
        &provider,
        &box_id,
        &id,
        &run_id,
        &receipt.raw,
    )
    .await;

    source
        .record_run_as(&run_id, &id, version, &coworker, &receipt)
        .await;
    // History is for reading, not for keeping: five runs per version is what the page shows.
    let _ = state
        .auth
        .store
        .prune_recipe_runs(&id, version, RUNS_KEPT_PER_VERSION)
        .await;
    // Whatever those runs produced goes with them; an artifact with no run has no page.
    let _ = state
        .auth
        .store
        .orphan_artifacts_of_gone_runs(&id, now_ms())
        .await;
    Json(json!({
        "recipe": recipe.id,
        "version": version,
        "runId": run_id,
        "ok": receipt.ok,
        "ran": receipt.ran,
        "stoppedAt": receipt.stopped_at,
        "error": receipt.error,
        // Present only when the box inlined one, which it stops doing once it is writing files.
        "image": receipt.image.as_ref().map(|image| json!({
            "mime": image.mime, "base64": image.base64, "width": image.width, "height": image.height,
        })),
        "artifacts": kept,
        // Said out loud rather than swallowed: a run whose recording did not survive looks
        // identical to one that was never recorded, and the difference matters when debugging.
        "artifactsMissed": missed,
    }))
    .into_response()
}

/// The executor's view of the registry: steps out, runs in.
#[derive(Clone)]
pub struct StoreRecipes {
    pub store: PgStore,
}

#[async_trait::async_trait]
impl RecipeSource for StoreRecipes {
    async fn recipe_request(
        &self,
        recipe_id: &str,
        values: &Values,
    ) -> Result<(i32, Value), String> {
        let recipe = match self.store.recipe(recipe_id).await {
            Ok(Some(recipe)) if recipe.deleted_at_ms.is_none() => recipe,
            Ok(_) => return Err(format!("recipe `{recipe_id}` is gone")),
            Err(error) => return Err(error.to_string()),
        };
        let version = match self.store.recipe_runnable_version(recipe_id).await {
            Ok(Some(version)) => version,
            Ok(None) => return Err(format!("recipe `{recipe_id}` has no runnable version")),
            Err(error) => return Err(error.to_string()),
        };
        // Said in words rather than failing as "unreadable". A workflow body has no `steps`, so
        // without this the model (and the run route) would be told a tree was a corrupt recipe.
        if version.kind == opengrok_tools::workflow::KIND {
            return Err(format!(
                "`{recipe_id}` is a workflow, not a recipe: it is a decision tree that calls \
                 recipes, and it is run from the workflow route rather than played as a tape"
            ));
        }
        let steps: Vec<Step> =
            serde_json::from_value(version.body.get("steps").cloned().unwrap_or(Value::Null))
                .map_err(|error| {
                    format!(
                        "recipe `{recipe_id}` v{} is unreadable: {error}",
                        version.version
                    )
                })?;
        let params: Vec<Parameter> = serde_json::from_value(
            version
                .body
                .get("parameters")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .unwrap_or_default();
        let bound = opengrok_recipes::bind(&params, values)?;
        let filled = opengrok_recipes::fill(&steps, &bound)?;
        Ok((
            version.version,
            opengrok_recipes::recipe_request(&recipe.name, &params, &filled, &bound),
        ))
    }

    async fn record_run(
        &self,
        recipe_id: &str,
        version: i32,
        coworker_id: &CoworkerId,
        receipt: &RecipeReceipt,
    ) -> Option<String> {
        let id = format!("rrun_{}", uuid::Uuid::now_v7());
        self.write_run(&id, recipe_id, version, coworker_id, receipt)
            .await;
        Some(id)
    }
}

impl StoreRecipes {
    /// Record a run under an id the caller already minted, because the artifacts were filed
    /// against that id before the run finished.
    pub async fn record_run_as(
        &self,
        run_id: &str,
        recipe_id: &str,
        version: i32,
        coworker_id: &CoworkerId,
        receipt: &RecipeReceipt,
    ) {
        self.write_run(run_id, recipe_id, version, coworker_id, receipt)
            .await;
    }

    async fn write_run(
        &self,
        id: &str,
        recipe_id: &str,
        version: i32,
        coworker_id: &CoworkerId,
        receipt: &RecipeReceipt,
    ) {
        // The receipt is kept without its picture: the picture rode the tool result; the
        // history shows what happened, not a gallery.
        let mut receipt_json = receipt.raw.clone();
        if let Some(object) = receipt_json.as_object_mut() {
            object.remove("screenshot");
        }
        // And without what the box saw. Every run of a recipe is handed back to everyone who may
        // read that recipe (`detail_body`), but a run happens on the box of whoever played it —
        // and a recipient may run a recipe they were shared. Keeping the window titles and page
        // URLs would make sharing a recipe a way to read a colleague's screen back to its author.
        // What it cost to look is kept; what it saw is not.
        opengrok_tools::observe::strip_observations(&mut receipt_json);
        let _ = self
            .store
            .record_recipe_run(
                id,
                recipe_id,
                version,
                coworker_id.as_str(),
                Some(id),
                receipt.ok,
                receipt.stopped_at.map(|n| n as i32),
                &receipt_json,
                now_ms(),
            )
            .await;
    }
}

/// The recipes a bot may run, for the executor.
pub async fn offers_for(
    state: &AgUiState,
    coworker_id: &CoworkerId,
) -> Vec<opengrok_tools::RecipeOffer> {
    let store = &state.auth.store;
    let mut offers = Vec::new();
    for recipe in store
        .recipes_granted_to(coworker_id.as_str())
        .await
        .unwrap_or_default()
    {
        let (kind, params) = runnable_shape(state, &recipe.id).await;
        // A GRANTED WORKFLOW IS NOT A `run_recipe` OPTION. The tool plays a tape in one box call;
        // a tree is walked by the engine, which is not something the model can be handed here. It
        // would arrive in the tool's enum and refuse on every call.
        if kind != TAPE {
            continue;
        }
        offers.push(opengrok_tools::RecipeOffer {
            id: recipe.id,
            name: recipe.name,
            description: recipe.description,
            parameters: params,
        });
    }
    offers
}

pub fn source_for(state: &AgUiState) -> Arc<dyn RecipeSource> {
    Arc::new(StoreRecipes {
        store: state.auth.store.clone(),
    })
}

/// `GET /admin/recipes` — the org's view: what has been shared org-wide, by whom, and how many
/// members took it up. Read-only; an admin manages people and computers, not recipes.
async fn admin_list(State(state): State<AgUiState>, headers: HeaderMap) -> Response {
    let (org_id, _, _) = match crate::account_api::admin_org(&state.auth, &headers).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let store = &state.auth.store;
    let rows = match store.recipes_shared_to_org(org_id.as_str()).await {
        Ok(rows) => rows,
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    };
    let members = store
        .accounts_by_org(org_id.as_str())
        .await
        .unwrap_or_default();
    let email_of = |id: &str| {
        members
            .iter()
            .find(|view| view.id.as_str() == id)
            .map(|view| view.email.clone())
    };
    let recipes: Vec<Value> = rows
        .iter()
        .map(|(recipe, accepted)| {
            json!({
                "id": recipe.id,
                "name": recipe.name,
                "description": recipe.description,
                "ownerId": recipe.owner_id,
                "ownerEmail": email_of(&recipe.owner_id),
                "latestVersion": recipe.latest_version,
                "updatedAtMs": recipe.updated_at_ms,
                "accepted": accepted,
                "members": members.len(),
            })
        })
        .collect();
    Json(json!({ "recipes": recipes })).into_response()
}
