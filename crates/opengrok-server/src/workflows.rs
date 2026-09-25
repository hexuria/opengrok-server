//! Workflows: the decision tree that calls recipes, and the door Jev is asked through.
//!
//! The tree itself is `opengrok_tools::workflow` — the body shape, the lint and the engine, none
//! of which knows about Postgres or HTTP. This file is the three things that only a server can do:
//! store a tree on a `recipe` row, decide who may run one, and put its questions to Jev.
//!
//! THREE ROUTES, AND DELIBERATELY NOT THIRTEEN. A workflow is a `recipe` row whose runnable
//! version is of kind `workflow`, so reading one, renaming it, deleting it, sharing it, accepting
//! or declining that share, granting it to a bot, revoking the grant, dropping a version and
//! reading its run history are ALREADY SERVED, correctly and with tests, by `/recipes/{id}/…`.
//! Copying those ten routes here would be copying ten refusals, and the second copy is the one
//! that quietly stops matching. What a workflow genuinely cannot borrow is the three places the
//! BODY differs:
//!
//! - `POST /workflows` — creating one, because a recipe is created from a taped tape and a
//!   workflow has no tape to filter;
//! - `POST /workflows/{id}/versions` — a new version, because the body is a tree rather than a
//!   list of steps and is linted by different rules;
//! - `POST /workflows/{id}/run` — running one, because a run is a walk over several recipes rather
//!   than one call to the box.
//!
//! `GET /workflows` is missing for the same reason: `GET /recipes?kind=workflow` is that listing,
//! and the rows, relations and share states in it are identical.
//!
//! WHO MAY DO WHAT IS `recipes::may`, CALLED, NOT RE-IMPLEMENTED. Every handler here goes through
//! `recipes::permitted`, so a workflow refuses exactly the way the recipe on the row beside it
//! does, in the same words and with the same status.

use std::collections::BTreeSet;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use opengrok_core::id::CoworkerId;
use opengrok_recipes::Values;
use opengrok_tools::workflow::{Ending, Judging, Walker, Workflow};
#[cfg(feature = "jev")]
use opengrok_tools::workflow::{Judge, JudgeAsk, JudgeError, Verdict};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::agui::AgUiState;
use crate::agui::routes::owned_coworker;
#[cfg(feature = "jev")]
use crate::jev::{Answer, Ask, JevError, JsonContent, NoulCriteria, Question};
use crate::recipes::{Action, StoreRecipes, org_of, permitted};

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn router(state: AgUiState) -> Router {
    Router::new()
        .route("/workflows", post(create))
        .route("/workflows/{id}/versions", post(add_version))
        .route("/workflows/{id}/run", post(run))
        .with_state(state)
}

// -------------------------------------------------------------------------------------------
// Jev, through the engine's seam
// -------------------------------------------------------------------------------------------

/// The engine's `Judge`, answered by the classifier.
///
/// A SEPARATE TYPE FROM `JevDoor` BECAUSE THEY ANSWER DIFFERENT QUESTIONS. The door's job is to
/// put a question to TypeSafe and bring back what was said, with the four ways it can fail kept
/// apart; the engine's job is to take one branch. This is the translation between them, and it is
/// the only place the three answer kinds are turned into one word.
#[cfg(feature = "jev")]
pub struct JevJudge {
    pub jev: crate::jev::SharedJev,
    /// The Jev model for this run's questions. `None` uses the deployment's configured default,
    /// which is what every caller here wants.
    pub model: Option<String>,
}

#[cfg(feature = "jev")]
#[async_trait::async_trait]
impl Judge for JevJudge {
    async fn judge(&self, ask: JudgeAsk<'_>) -> Result<Verdict, JudgeError> {
        let state = JsonContent::from_value(ask.state.clone()).map_err(|error| {
            // Ours: the engine built a state Jev's content type cannot carry.
            JudgeError::Malformed(error.to_string())
        })?;
        let question = question_for(ask.question).map_err(JudgeError::Malformed)?;
        let judgement = self
            .jev
            .ask(Ask {
                state,
                questions: vec![(ask.name.to_string(), question)],
                model: self.model.clone(),
            })
            .await
            .map_err(|error| match error {
                // THE FOUR FAILURES BECOME TWO HERE, AND ONLY HERE. `Asked` is a question that
                // could never have been put — our bug, never retried, and never answered on our
                // behalf. The other three are the service being away, which is what the agreed
                // fallbacks exist for. Collapsing them earlier would make an expired key look
                // like a blip; collapsing them later would make the engine carry four cases it
                // has only two answers for.
                JevError::Asked(why) => JudgeError::Malformed(why),
                other => JudgeError::Unavailable(other.to_string()),
            })?;

        let Some((_, answer)) = judgement
            .answers
            .into_iter()
            .find(|(name, _)| name == ask.name)
        else {
            // Not malformed: the question was fine and something upstream dropped it, so a second
            // attempt could well succeed. It falls back, and the run says so.
            return Err(JudgeError::Unavailable(format!(
                "Jev answered nothing under \"{}\"",
                ask.name
            )));
        };
        verdict_for(ask.question, answer)
    }
}

/// One of the engine's questions as the SDK's. The refusals are the sentences `/jev/ask` gives for
/// the same shapes; the tree's lint has already applied them at write time, so reaching one of
/// these means a body got past the lint and that is worth stopping the walk for.
#[cfg(feature = "jev")]
fn question_for(question: &opengrok_tools::workflow::Question) -> Result<Question, String> {
    use opengrok_tools::workflow::Question as Asked;
    let instructions = question.instructions().trim();
    if instructions.is_empty() {
        return Err("the question has no words to it".to_string());
    }
    Ok(match question {
        Asked::Noul {
            yes_means,
            no_means,
            ..
        } => {
            let noul = Question::noul(instructions);
            match (yes_means, no_means) {
                (None, None) => noul,
                (yes, no) => {
                    let mut criteria = NoulCriteria::new();
                    if let Some(yes) = yes {
                        criteria = criteria.yes(yes.as_str());
                    }
                    if let Some(no) = no {
                        criteria = criteria.no(no.as_str());
                    }
                    noul.with_noul_criteria(criteria)
                }
            }
        }
        Asked::Choice { choices, .. } => {
            if choices.len() < 2 {
                return Err("a choice needs at least two options to choose between".to_string());
            }
            let labels: Vec<(String, Option<JsonContent>)> = choices
                .iter()
                .map(|choice| {
                    (
                        choice.label().to_string(),
                        choice
                            .means()
                            .map(|means| JsonContent::String(means.to_string())),
                    )
                })
                .collect();
            Question::choice(instructions, labels)
        }
        Asked::Score { levels, .. } => {
            if levels.is_empty() {
                return Err("a score needs levels to score against".to_string());
            }
            Question::score(instructions, levels.iter().map(String::as_str))
        }
    })
}

/// What Jev said, as the one word the engine branches on.
#[cfg(feature = "jev")]
fn verdict_for(
    question: &opengrok_tools::workflow::Question,
    answer: Answer,
) -> Result<Verdict, JudgeError> {
    use opengrok_tools::workflow::Question as Asked;
    match (question, answer) {
        (Asked::Noul { .. }, Answer::Noul(noul)) => {
            let (yes, confidence) = crate::jev::routes::noul_reading(noul.noul);
            Ok(Verdict {
                answer: if yes { "yes" } else { "no" }.to_string(),
                confidence,
            })
        }
        (Asked::Choice { .. }, Answer::Choice(choice)) => Ok(Verdict {
            answer: choice.choice,
            confidence: choice.confidence,
        }),
        (Asked::Score { levels, .. }, Answer::Score(score)) => {
            // THE RUNG IS READ AGAINST OUR OWN LEVELS, NOT AGAINST JEV'S LEGEND. The branches are
            // keyed by the words the body wrote, and the legend is those words echoed back — so
            // going through it adds a way for a branch to miss (an echo that rewrapped a level,
            // a legend with a gap) and buys nothing.
            let rung = crate::jev::routes::rung_of(score.score)
                .and_then(|rung| usize::try_from(rung).ok())
                .and_then(|rung| levels.get(rung));
            match rung {
                Some(level) => Ok(Verdict {
                    answer: level.clone(),
                    confidence: score.confidence,
                }),
                None => Err(JudgeError::Unavailable(format!(
                    "Jev scored {}, which is not a rung of the {} this question offered",
                    score.score,
                    levels.len()
                ))),
            }
        }
        // Upstream answered a different kind of question than the one that was asked. Not our
        // body's fault, so it falls back with the reason rather than stopping the walk.
        (question, answer) => Err(JudgeError::Unavailable(format!(
            "Jev answered a {} question with {}",
            question.kind_word(),
            match answer {
                Answer::Noul(_) => "a yes-or-no",
                Answer::Choice(_) => "a choice",
                Answer::Score(_) => "a score",
            }
        ))),
    }
}

#[cfg(not(feature = "jev"))]
fn ask_refused_without_jev() -> &'static str {
    "this build was compiled without the cargo feature `jev`, so there is nobody to ask"
}

/// `Ok` only when the caller switched Jev off. Any other switch would have asked the classifier,
/// and this build has no classifier to ask.
#[cfg(not(feature = "jev"))]
fn jev_wanted_on_this_build(jev_switch: Option<bool>) -> Result<(), &'static str> {
    match jev_switch {
        Some(false) => Ok(()),
        _ => Err(ask_refused_without_jev()),
    }
}

// -------------------------------------------------------------------------------------------
// The routes
// -------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequest {
    name: String,
    #[serde(default)]
    description: String,
    /// The tree, as `opengrok_tools::workflow` reads it.
    workflow: Value,
    #[serde(default)]
    note: String,
}

/// `POST /workflows` — the tree in, v1 written, the row out.
async fn create(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Json(request): Json<CreateRequest>,
) -> Response {
    let Some(account) = crate::agui::routes::account_from_bearer(&state, &headers) else {
        return (StatusCode::UNAUTHORIZED, "sign in first").into_response();
    };
    let name = request.name.trim();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, "a workflow needs a name").into_response();
    }
    let workflow = match Workflow::parse(&request.workflow) {
        Ok(workflow) => workflow,
        Err(why) => return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response(),
    };
    let org = org_of(&state, &account).await;
    let id = format!("rcp_{}", uuid::Uuid::now_v7());
    let at_ms = now_ms();
    let store = &state.auth.store;
    // The same `recipe` row a taught task gets. The screen size is the box's default rather than a
    // taught one: a tree is not pixels, and the column is not nullable.
    let screen = opengrok_recipes::Screen::default();
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
    let note = if request.note.trim().is_empty() {
        "the tree as written"
    } else {
        request.note.trim()
    };
    if let Err(error) = store
        .add_recipe_version(
            &id,
            opengrok_tools::workflow::KIND,
            // Stored as the engine writes it rather than as it arrived, so the shape number and
            // the clamped budget are in the row and a reader never has to guess which defaults
            // were in force the day it was saved.
            &workflow.to_body(),
            note,
            account.as_str(),
            at_ms,
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    crate::recipes::detail_body(&state, &account, org.as_deref(), &id).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VersionRequest {
    workflow: Value,
    #[serde(default)]
    note: String,
}

/// `POST /workflows/{id}/versions` — a new tree on the same row; the owner's.
async fn add_version(
    State(state): State<AgUiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<VersionRequest>,
) -> Response {
    let (account, org, _, _) = match permitted(&state, &headers, &id, Action::Edit).await {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    if let Err(refusal) = crate::recipes::expect_kind(&state, &id, true).await {
        return refusal;
    }
    let workflow = match Workflow::parse(&request.workflow) {
        Ok(workflow) => workflow,
        Err(why) => return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response(),
    };
    let note = if request.note.trim().is_empty() {
        "edited"
    } else {
        request.note.trim()
    };
    if let Err(error) = state
        .auth
        .store
        .add_recipe_version(
            &id,
            opengrok_tools::workflow::KIND,
            &workflow.to_body(),
            note,
            account.as_str(),
            now_ms(),
        )
        .await
    {
        return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
    }
    crate::recipes::detail_body(&state, &account, org.as_deref(), &id).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRequest {
    coworker_id: String,
    #[serde(default)]
    values: Option<Values>,
    /// THE ONE SWITCH THAT TURNS JEV OFF. Absent is on. `false` runs the tree deterministically on
    /// purpose: every `ask` step takes its agreed fallback and the run says it was off rather than
    /// absent, which is the difference between a workflow somebody chose to run without a judge
    /// and one that lost its judge halfway through.
    #[serde(default)]
    jev: Option<bool>,
}

/// `POST /workflows/{id}/run` — walk the tree now on one of the caller's bots.
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
    let store = state.auth.store.clone();
    let Ok(Some(version)) = store.recipe_runnable_version(&id).await else {
        return (
            StatusCode::CONFLICT,
            "this workflow has no runnable version",
        )
            .into_response();
    };
    if version.kind != opengrok_tools::workflow::KIND {
        return (
            StatusCode::CONFLICT,
            "that is a taught recipe, not a workflow; run it from the recipe route",
        )
            .into_response();
    }
    let workflow = match Workflow::parse(&version.body) {
        Ok(workflow) => workflow,
        // A STORED BODY THAT NO LONGER PARSES IS A 409, NOT A 500. Bodies are immutable, so this
        // is a version written by an older shape of this engine, and the person who owns it can
        // fix it by writing a new one — which is a conflict with the state of the row, not a
        // failure of the request.
        Err(why) => {
            return (
                StatusCode::CONFLICT,
                format!("this workflow (v{}) cannot be run: {why}", version.version),
            )
                .into_response();
        }
    };
    // Bound before anything else happens, so a value that does not bind is a refusal to the person
    // who typed it rather than a run row that failed at its third step.
    let bound =
        match opengrok_recipes::bind(&workflow.parameters, &request.values.unwrap_or_default()) {
            Ok(bound) => bound,
            Err(why) => return (StatusCode::UNPROCESSABLE_ENTITY, why).into_response(),
        };

    let coworker = CoworkerId::from_stored(request.coworker_id.clone());
    match owned_coworker(&state, &account, &coworker).await {
        Ok(true) => {}
        Ok(false) => {
            return (
                StatusCode::FORBIDDEN,
                "you may only run workflows on your own bots",
            )
                .into_response();
        }
        Err(refusal) => return refusal,
    }

    // EVERY RECIPE THE TREE CAN PLAY IS CHECKED BEFORE THE FIRST STEP, not when it is reached.
    // A workflow that stops half way because its fourth branch turned out to be a recipe the
    // caller may not run has already clicked three recipes' worth of things on a screen.
    let allowed = match permitted_recipes(&state, &headers, &workflow.recipes()).await {
        Ok(allowed) => allowed,
        Err(refusal) => return refusal,
    };

    // The bot's box, the way a recipe run finds it.
    let (_mode, org_id, scope, scope_id, _) =
        crate::agui::provision::scope_of(&state, &account, coworker.as_str()).await;
    let Ok(Some((box_id, kind, _))) = store.scoped_computer_full(scope, &scope_id).await else {
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

    // Whether anybody is asked. Two ways to arrive at "nobody", and they are NOT the same
    // sentence: one is a decision somebody made for this run, the other is a deployment with no
    // key at all, and a person reading a run full of default answers has to be able to tell which
    // — so the reason travels into the trail rather than a bare "no judge".
    #[cfg(feature = "jev")]
    let judge = match (request.jev, state.auth.jev.clone()) {
        (Some(false), _) => Err("Jev was switched off for this run".to_string()),
        // `None` for the model: the deployment's own (`OG_JEV_MODEL`, read once at boot into the
        // client's config) is the right answer, and a per-request read of the environment is the
        // habit this server does not have.
        (_, Some(jev)) => Ok(JevJudge { jev, model: None }),
        (_, None) => Err(
            "this deployment has no Jev key configured (OG_JEV_API_KEY), so there was nobody to ask"
                .to_string(),
        ),
    };
    #[cfg(not(feature = "jev"))]
    let judge: Result<(), String> = match jev_wanted_on_this_build(request.jev) {
        Ok(()) => Err("Jev was switched off for this run".to_string()),
        Err(why) => {
            return (StatusCode::SERVICE_UNAVAILABLE, why).into_response();
        }
    };

    // The walk is written down as a run of this row, under the same table a recipe's runs use, so
    // one history shows both and the artifact and pruning rules already written apply unchanged.
    // Minted and WRITTEN before the first step, for the reason `recipes::begin_run` gives.
    let run_id = format!("rrun_{}", uuid::Uuid::now_v7());
    if let Err(refusal) =
        crate::recipes::begin_run(&store, &run_id, &id, version.version, &coworker).await
    {
        return refusal;
    }
    let walking = tokio::spawn(walk(Walked {
        store,
        provider,
        box_id,
        workflow_id: id.clone(),
        run_id: run_id.clone(),
        version: version.version,
        coworker,
        allowed,
        judge,
        name: recipe.name.clone(),
        workflow,
        bound,
    }));
    if crate::recipes::wants_async(&headers) {
        return crate::recipes::accepted(json!({
            "workflow": id, "version": version.version, "runId": run_id, "state": "running",
        }));
    }
    crate::recipes::joined(walking, &run_id).await
}

/// Everything a detached walk owns: `Walker` borrows, and a walk outlives the request.
struct Walked {
    store: opengrok_store::PgStore,
    provider: std::sync::Arc<dyn opengrok_box::Computer>,
    box_id: String,
    workflow_id: String,
    run_id: String,
    version: i32,
    coworker: CoworkerId,
    allowed: BTreeSet<String>,
    #[cfg(feature = "jev")]
    judge: Result<JevJudge, String>,
    #[cfg(not(feature = "jev"))]
    judge: Result<(), String>,
    name: String,
    workflow: Workflow,
    bound: Values,
}

/// Walk the tree and finish its row, detached from the request for the reason `recipes::play`
/// gives: a closed tab used to take the walk's history with it while the box kept clicking.
async fn walk(walked: Walked) -> Response {
    let Walked {
        store,
        provider,
        box_id,
        workflow_id: id,
        run_id,
        version,
        coworker,
        allowed,
        judge,
        name,
        workflow,
        bound,
    } = walked;
    let _lease = crate::recipes::hold_run(store.clone(), run_id.clone());
    #[cfg(feature = "jev")]
    let judging = match &judge {
        Ok(judge) => Judging::Ask(judge),
        Err(because) => Judging::Off(because.clone()),
    };
    #[cfg(not(feature = "jev"))]
    let judging = Judging::Off(judge.err().unwrap_or_default());
    let recipes = StoreRecipes {
        store: store.clone(),
    };
    let walker = Walker {
        computer: provider.as_ref(),
        box_id: &box_id,
        recipes: &recipes,
        coworker: &coworker,
        allowed: &allowed,
        judging,
        name: &name,
        // The deployment's level, read once at boot rather than per walk; a `run` step that needs
        // the page as well says so in the body and overrides this for itself.
        observe: opengrok_tools::observe::wanted(),
    };
    let walk = walker.walk(&workflow, &bound).await;

    let receipt = walk.receipt();
    let written = store
        .record_recipe_run(
            &run_id,
            &id,
            version,
            coworker.as_str(),
            Some(&run_id),
            walk.ok(),
            // The step count, for a walk that ended without the tree saying so. A clean ending
            // leaves it null, the way a recipe that played to the end does.
            if walk.ok() {
                None
            } else {
                i32::try_from(walk.steps).ok()
            },
            &receipt,
            now_ms(),
        )
        .await;
    crate::recipes::tidy_history(&store, &id, version, &coworker).await;

    let mut body = json!({
        "workflow": id,
        "version": version,
        "runId": run_id,
        "at": ending_step(&walk.ending),
    });
    if let (Some(body), Some(receipt)) = (body.as_object_mut(), receipt.as_object()) {
        for (key, value) in receipt {
            body.insert(key.clone(), value.clone());
        }
    }
    match written {
        Ok(()) => Json(body).into_response(),
        // The walk happened and its history does not say so: an error to the caller, with the
        // receipt riding along, as `recipes::play` does for one recipe.
        Err(error) => {
            tracing::warn!(run = %run_id, %error, "a workflow walked but was not written down");
            if let Some(object) = body.as_object_mut() {
                object.insert(
                    "historyMissed".to_string(),
                    json!(format!(
                        "the walk ran but could not be written to the workflow's history: {error}"
                    )),
                );
            }
            (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
        }
    }
}

/// The step a walk was on when it ended, for the endings that did not get to take one. `None` for
/// a `stop`, whose step is the last line of the trail.
fn ending_step(ending: &Ending) -> Option<&str> {
    match ending {
        Ending::Stopped { .. } => None,
        Ending::OutOfSteps { at, .. }
        | Ending::OutOfTime { at, .. }
        | Ending::GoingInCircles { at, .. }
        | Ending::RecipeFailed { at, .. }
        | Ending::Broken { at, .. } => Some(at),
    }
}

/// The recipes this caller may play, or the refusal naming the first one they may not.
///
/// THE SAME QUESTION `POST /recipes/{id}/run` ASKS, ASKED ONCE PER RECIPE THE TREE NAMES. A tree
/// is not a way to run somebody else's recipe: being allowed to run the workflow says nothing
/// about the recipes inside it, which have their own owners, their own shares and their own
/// grants.
async fn permitted_recipes(
    state: &AgUiState,
    headers: &HeaderMap,
    wanted: &BTreeSet<String>,
) -> Result<BTreeSet<String>, Response> {
    let mut allowed = BTreeSet::new();
    for recipe_id in wanted {
        permitted(state, headers, recipe_id, Action::Run)
            .await
            .map_err(|_| {
                // The recipe route's own refusal would say "no such recipe" with no clue which
                // one; this run is refused before it starts and names it.
                (
                    StatusCode::FORBIDDEN,
                    format!(
                        "this workflow plays `{recipe_id}`, and you may not run that recipe — \
                         accept the share, or have its owner share it with you"
                    ),
                )
                    .into_response()
            })?;
        // A WORKFLOW DOES NOT CALL A WORKFLOW, yet. The budgets are per walk, so a tree calling a
        // tree would have two of them and no accounting between; the refusal says so rather than
        // handing a decision tree to the box's step runner.
        let (kind, _) = crate::recipes::runnable_shape(state, recipe_id).await;
        if kind != crate::recipes::TAPE {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("`{recipe_id}` is a workflow; a workflow cannot call another workflow"),
            )
                .into_response());
        }
        allowed.insert(recipe_id.clone());
    }
    Ok(allowed)
}

#[cfg(all(test, not(feature = "jev")))]
mod without_jev {
    #[test]
    fn an_ask_with_kind_noul_refuses_when_jev_is_not_compiled() {
        let why = super::ask_refused_without_jev();
        assert!(
            why.contains("cargo feature `jev`"),
            "the refusal must name the missing feature, got {why}"
        );
        assert!(
            matches!(super::jev_wanted_on_this_build(None), Err(sentence) if sentence == why),
            "an ask that needs Jev must refuse"
        );
        assert!(
            matches!(
                super::jev_wanted_on_this_build(Some(true)),
                Err(sentence) if sentence == why
            ),
            "jev true still needs the feature"
        );
        assert!(
            super::jev_wanted_on_this_build(Some(false)).is_ok(),
            "an explicit off stays a walk without a judge"
        );
    }
}
