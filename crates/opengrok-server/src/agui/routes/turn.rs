//! Run authorization, start, turn handling, and requests.

use super::*;

/// What a bot key says about itself. The `use` claim is the discriminator the minter's own
/// documentation demands: without it, a stolen access token would verify here too.
pub(crate) use crate::auth::bot_keys::BotKeyClaims;

/// Why a bearer that was PRESENT did not name anybody.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BearerRefusal {
    /// A bot key that verifies but has been revoked.
    Revoked,
    /// Expired, signed by somebody else, or not one of our credentials at all.
    NotOurs,
}

impl BearerRefusal {
    pub(crate) fn sentence(self) -> &'static str {
        match self {
            Self::Revoked => "this bot key has been revoked",
            Self::NotOurs => {
                "this sign-in has expired or was not issued by this server; sign in again"
            }
        }
    }
}

/// Who is calling, and — when the credential is a bot key — AS which coworker.
///
/// `Ok(None)` is ONLY "no bearer at all". A bearer that is present and does not name anybody is
/// an `Err`, never a fall-through to anonymous: a revoked Bot must not keep talking on the
/// deployment's model with nobody noticing the revocation did nothing, and a stale key must not
/// look like success — on 1 Sep a Bot whose key had been minted for another account sent a turn
/// that "worked" as an anonymous caller owning nothing (ROADMAP 10.3).
pub(crate) async fn principal_from_bearer(
    state: &AgUiState,
    headers: &axum::http::HeaderMap,
) -> Result<Option<(opengrok_core::id::AccountId, Option<CoworkerId>)>, BearerRefusal> {
    let Some(token) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return Ok(None);
    };
    if let Ok(claims) = state.auth.minter.verify_access(token) {
        return Ok(Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            None,
        )));
    }
    if let Ok(claims) = state.auth.minter.verify_claims::<BotKeyClaims>(token) {
        if claims.purpose != "bot-key" {
            return Err(BearerRefusal::NotOurs);
        }
        let live = state
            .auth
            .store
            .bot_key_live(&claims.jti)
            .await
            .unwrap_or(false);
        if !live {
            return Err(BearerRefusal::Revoked);
        }
        return Ok(Some((
            opengrok_core::id::AccountId::from_stored(claims.sub),
            Some(CoworkerId::from_stored(claims.coworker)),
        )));
    }
    Err(BearerRefusal::NotOurs)
}

/// Start a run and stream its events.
pub async fn run(
    State(gateway): State<crate::host_state::HostState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<RunAgentInput>,
) -> Response {
    // `AgUiState` has no path to the live bus (`HostState` owns it). This handler lives on
    // `HostState` so a UserForm CUSTOM can mint the card and stamp `entryId` before the
    // SSE frame is sent; the rest of the turn still reads `agui` the same way every other
    // AG-UI path does.
    //
    // Who is asking. Established first, because the permission check, the run's ownership and the
    // model it thinks with all depend on it.
    //
    // Layer 1, every turn: may this principal talk to this coworker at all?
    let (account_id, key_coworker) = match principal_from_bearer(&gateway.agui, &headers).await {
        Ok(Some((account, coworker))) => (Some(account), coworker),
        Ok(None) => (None, None),
        // A bad bearer refuses; downgrading to anonymous would make revocation invisible and a
        // stale key look like success.
        Err(refusal) => return unauthorized(refusal.sentence()),
    };
    turn(gateway, (account_id, key_coworker), input).await
}

/// A turn for whoever the bearer named: `run` for a client's POST, and the server itself for a
/// queued send it delivers once the person's Mac is back (`pending::drain_held`).
pub(crate) async fn turn(
    gateway: crate::host_state::HostState,
    (account_id, key_coworker): (Option<AccountId>, Option<CoworkerId>),
    input: RunAgentInput,
) -> Response {
    let state = gateway.agui.clone();
    // A BOT KEY NAMES THE COWORKER. barok-works registers a Bot with an endpoint and a header —
    // it has no forwardedProps to send — so the key itself carries which coworker the Bot IS.
    // An explicit forwardedProps still wins: a client that says what it means is believed.
    let run_coworker = coworker_id_from(&input).or(key_coworker);

    // A COWORKER NAMED BY A CALLER WE CANNOT NAME BACK IS REFUSED HERE, NOT LATER. `coworker_id_from`
    // reads `forwardedProps`, which anyone can send, so this pair is reachable from outside: a turn
    // that names a coworker and carries no credential.
    //
    // It could only ever fail, and it failed late and in the wrong words. Every gate below is keyed
    // on having BOTH — the policy check, the coworker's own model and role, its tools and its
    // persona are all skipped — and the turn then reached the spend guard carrying a scope with no
    // payer, which held it with "This turn does not say whose spend it is … This is a server bug".
    // The guard was right, and the sentence was ours to prevent: it named our defect in a place the
    // person could only read as a limit they had hit.
    //
    // NO TURN IS ANONYMOUS ANY MORE, EITHER (25 Sep 2026). One naming nobody ran on the
    // deployment's gateway key with no payer, so `GuardedDoor` — which meters a coworker's own key
    // — never saw it: anyone who could reach the host spent its model credit, one run row per
    // request. Refused here, before any journal write.
    let Some(caller) = account_id.clone() else {
        return unauthorized(if run_coworker.is_some() {
            "that turn names a coworker, so it needs a signed-in caller; sign in and send it again"
        } else {
            "a turn needs a signed-in caller; sign in and send it again"
        });
    };
    // A PAIR'S THREAD IS ITS TWO BOTS' (#314): nothing a person POSTs runs in one, WHATEVER ITS
    // MESSAGES. Refusing only new words let an empty body, a tool result or words before the
    // last answer start a turn there, or stop a run parked on a card (review of #325). No client
    // POSTs into one: its runs are the server's, read by polling, and a card's answer comes by
    // its own route (`/ag-ui/runs/{id}/answer`, `/ag-ui/user-form/…`), which this never sees.
    if opengrok_wire::pair::is_pair_thread(&input.thread_id) {
        return read_only();
    }
    // A source the turn names that cannot be honoured refuses before anything is charged or drained.
    let named = input.forwarded_props.get("inferenceSource");
    let chosen_source = match crate::inference::named(named, "inferenceSource") {
        Ok(chosen) => chosen,
        Err(refusal) => return *refusal,
    };
    // A named caller with no coworker has a payer but no key of its own to meter it, so it runs
    // on the deployment's key: bounded per account instead. Charged BEFORE the queued send below
    // is drained, so a refusal here never costs the person a queued message — and refunded when
    // the POST turns out to start nothing (a stale queued send, a retry that reattaches), so a
    // client reconnecting to its own run does not spend the hour's turns doing it.
    let unscoped_charge = run_coworker
        .is_none()
        .then(|| format!("account:{}", caller.as_str()));
    if let Some(charge) = &unscoped_charge
        && let Err(spent) = state
            .auth
            .budgets
            .take(&crate::auth::budget::AGUI_UNSCOPED, charge)
    {
        // A spent budget still lets the person reach a run they already started: a dropped
        // stream re-POSTs its own run id, and answering that 429 would strand the turn it paid
        // for. Nothing new starts on this path, so nothing is charged.
        if let Some(answer) =
            answer_for_existing_run(&state, account_id.as_ref(), &input.run_id).await
        {
            return answer;
        }
        return crate::auth::budget::too_many(
            spent,
            "too many turns without a coworker this hour; hire or pick a coworker, whose turns \
             are metered on its own key, or wait",
        );
    }

    // The deployment's model is the default, not the answer: a named coworker overrides it below.
    let mut model = state.model.clone();
    let mut effort = Effort::Inherit;
    // Its own door and pin, only once a coworker loads: never the deployment's model as a pin.
    let mut own = (None, None);
    let mut coworker_name = String::new();
    let mut coworker_role: Option<String> = None;

    if let (Some(account_id), Some(coworker_id)) = (&account_id, run_coworker.clone()) {
        // `policy_to_use`, not `policy_for`: a coworker an org-mate shared is one this person may
        // talk to under the owner's grant, read now. A store error refuses too, but as a 503
        // that says so: an empty context would deny with "no grant lets …", which sends the
        // person off to repair a grant that is fine.
        let policy = match state
            .auth
            .store
            .policy_to_use(account_id, &coworker_id)
            .await
        {
            Ok(policy) => policy,
            Err(error) => {
                tracing::error!(%error, coworker = %coworker_id.as_str(), "the run door could not read the policy; the turn is refused");
                let why = "the permission check could not be read right now, so nothing ran; \
                           send it again in a moment";
                return (StatusCode::SERVICE_UNAVAILABLE, why).into_response();
            }
        };
        let using = opengrok_policy::Action::UseCoworker;
        let decision = opengrok_policy::decide(account_id, &coworker_id, using, &policy);
        if let Some(reason) = decision.reason() {
            return refuse_use(&state, account_id, &coworker_id, reason)
                .await
                .into_response();
        }

        // WHICH MODEL A COWORKER THINKS WITH IS THE COWORKER'S, NOT THE DEPLOYMENT'S. Hiring takes
        // a model and stores it, and the roster reports it; a run that read past it left every one
        // of those answers describing a choice that never happened — a coworker hired on one model
        // silently answered on another, and the only visible symptom was the bill.
        //
        // AFTER the policy check and only for a named principal: a caller that check refuses must
        // not learn a coworker's configuration by noticing which model replies.
        //
        // A coworker that cannot be loaded keeps the default rather than failing the run: the model
        // is how the turn is answered, not whether it is allowed, and that question was just asked.
        if let Ok((coworker, _)) = state.auth.store.load_coworker(&coworker_id).await {
            model = coworker.model.clone();
            effort = coworker.effort;
            own = (coworker.source, Some(coworker.model));
            coworker_name = coworker.name;
            coworker_role = coworker.role;
        }
    }
    // Before any queued send is drained, as the policy is: a ceiling that cannot be read refuses
    // the turn rather than hand it the server's whole budget.
    let limits = run_limits(&state, &caller, run_coworker.as_ref(), RunLimits::default());
    let Some(limits) = limits.await else {
        refund_unscoped(&state, unscoped_charge.as_deref());
        let why = "the organization's run limits could not be read right now, so nothing ran; send \
                   it again in a moment";
        return (StatusCode::SERVICE_UNAVAILABLE, why).into_response();
    };

    // A FILE THE CALLER DOES NOT OWN REFUSES THE TURN before anything runs or any queued send is
    // drained (#229): answered like a missing one, so an id tells nobody whose file it was.
    if let Some(refusal) =
        super::attachments::refuse_unowned(&state, account_id.as_ref(), &input).await
    {
        refund_unscoped(&state, unscoped_charge.as_deref());
        return refusal;
    }

    // Refuse stale sends before interrupting a parked turn or preparing any model work. A queued
    // send's own source pick wins: it is what the person chose when they queued it. One its Mac
    // would carry while no Mac is connected stays queued, and is answered so (`consume_for_turn`).
    let picks = (chosen_source, own.0);
    let queued = crate::agui::pending::consume_for_turn(&state, &caller, &input, picks);
    let chosen_source = match queued.await {
        Ok(queued) => queued.or(chosen_source),
        Err(refusal) => {
            refund_unscoped(&state, unscoped_charge.as_deref());
            return refusal;
        }
    };

    // PAST THE CLAIM, A HANG-UP MUST NOT CANCEL THE TURN. The queued send is drained now, and the
    // setup below waits on a box wake, the store and the door. Run inline, a client that timed
    // out there dropped this future and left the row drained under a run that never started. The
    // turn itself was always spawned; now the setup that leads to it is too.
    let turn = tokio::spawn(start_claimed_turn(
        gateway,
        input,
        account_id,
        run_coworker,
        (model, effort, chosen_source),
        own,
        coworker_name,
        coworker_role,
        unscoped_charge,
        limits,
    ));
    turn.await.unwrap_or_else(|error| {
        tracing::error!(%error, "a claimed turn's setup did not finish");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "this turn could not be started",
        )
            .into_response()
    })
}

/// The 401 every refusal of an unnamed or unrecognised caller answers with: `{"error": …}`, the
/// shape the removed `/api/{method}` seam guaranteed and the removed desktop client's error helper
/// read first (`docs/known-gaps.md` §2), so the sentence reaches the person rather than "failed
/// (401)".
pub(super) fn unauthorized(sentence: &str) -> Response {
    let said = Json(serde_json::json!({ "error": sentence }));
    (StatusCode::UNAUTHORIZED, said).into_response()
}

/// The 403 a person's words into a pair's thread get, the contract's body (#314).
pub(crate) fn read_only() -> Response {
    let body = Json(opengrok_wire::pair::read_only_body());
    (StatusCode::FORBIDDEN, body).into_response()
}

/// A store that did not answer: a 503 with its words.
pub(crate) fn unavailable(error: impl std::fmt::Display) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
}

/// Give back an `AGUI_UNSCOPED` hit taken for a POST that started no turn.
fn refund_unscoped(state: &AgUiState, charge: Option<&str>) {
    if let Some(charge) = charge {
        let budgets = &state.auth.budgets;
        budgets.refund(&crate::auth::budget::AGUI_UNSCOPED, charge);
    }
}

/// Everything a turn does once any queued send it fires has been claimed.
#[allow(clippy::too_many_arguments)]
async fn start_claimed_turn(
    gateway: crate::host_state::HostState,
    input: RunAgentInput,
    account_id: Option<opengrok_core::id::AccountId>,
    run_coworker: Option<CoworkerId>,
    // What the turn thinks with: the coworker's pin and effort, and the source it named.
    (model, effort, chosen_source): (String, Effort, Option<TurnSource>),
    own: (Option<SourceKind>, Option<String>),
    coworker_name: String,
    coworker_role: Option<String>,
    unscoped_charge: Option<String>,
    limits: RunLimits,
) -> Response {
    let state = gateway.agui.clone();
    // A RUN ID THAT ALREADY HAS A RUN IS NOT A NEW TURN. A client retrying its POST — its stream
    // dropped, or it sent the same body twice — used to start a second loop on the same run: a
    // second model call, every tool twice, one transcript interleaving both. Answered here,
    // before anything below has a side effect (a parked card stopped, a box woken). AFTER the
    // queued-send check, deliberately: a same-run retry whose words changed gets that check's
    // own refusal (`stale-pending-message`), which says more than being handed the old run.
    // The claim further down is what makes it exact: two POSTs at once can both get past this.
    if let Some(answer) = answer_for_existing_run(&state, account_id.as_ref(), &input.run_id).await
    {
        refund_unscoped(&state, unscoped_charge.as_deref());
        return answer;
    }
    if let (Some(account_id), Some(coworker_id)) = (&account_id, &run_coworker) {
        crate::agui::resume::interrupt_parked_hitl(
            &gateway,
            account_id,
            coworker_id,
            &input.thread_id,
            account_id.as_str(),
        )
        .await;
    }

    // Read once. The tool runner needs it to bind the run, and the system prompt needs it to say
    // so; reading it twice would let the two disagree about what the person chose.
    let chosen = chosen_recipe_from(&input);
    // The plugins this message named, kept to those its driving account installed on a Bot it
    // owns; read once, so the tools and the sentence naming them cannot disagree.
    let asked_plugins = mentioned_plugins_from(&input);
    // Provenance: an app that draws form-mode cards sends `forwardedProps.formMode: true` (#220,
    // Tier 1); without it no form-mode card is ever minted.
    let drawn = input.forwarded_props.get("formMode") == Some(&serde_json::json!(true));
    let mut turn_plugins = opengrok_integrations::turn::TurnPlugins {
        form_mode: Some(drawn),
        chosen: plugin_accounts_from(&input),
        ..Default::default()
    };
    if let (Some(account), Some(coworker)) = (&account_id, &run_coworker) {
        let store = &state.auth.store;
        let operator = &state.plugins;
        let kept = opengrok_integrations::turn::mentioned(
            store,
            account,
            coworker,
            &asked_plugins,
            operator,
        );
        turn_plugins.mentioned = kept.await;
        // A tagged plugin this turn could not use yet is asked about BEFORE the model is: the
        // turn answers with the card and ends, and the card's answer sends the same message again
        // (#360). Parking instead would resume without the tags, which live only on this request.
        let vault = state.vault.as_deref();
        let asked = (&asked_plugins, &turn_plugins);
        let needs =
            opengrok_integrations::turn::needs(store, vault, account, coworker, asked.0, asked.1)
                .await;
        if !needs.is_empty() {
            return plugin_needs_answer(&input, needs);
        }
    }
    let mentioned = turn_plugins.mentioned.clone();
    let plugins_off = match (&account_id, &run_coworker) {
        (Some(account), Some(coworker)) => {
            let store = &state.auth.store;
            opengrok_integrations::turn::switched_off(store, account, coworker, &mentioned).await
        }
        _ => Vec::new(),
    };
    // What this conversation's form-mode cards came to (#220): a card's call answered before
    // the card had an outcome, so the Bot hears it here, on its next turn.
    let form_lines = match (&account_id, &run_coworker) {
        (Some(account), Some(coworker)) => {
            opengrok_integrations::form_mode::turn_line(
                &state.auth.store,
                account,
                coworker,
                &input.thread_id,
            )
            .await
        }
        _ => String::new(),
    };
    // No bearer, no identity, and therefore no computer tools.
    let tools = match (&account_id, &run_coworker) {
        (Some(account), Some(coworker)) => {
            let patience = TURN_WAKE_PATIENCE;
            let gates: (&[String], &[String]) = (&[], &[]);
            let runner =
                tools_for_turn(&state, account, coworker, gates, patience, &turn_plugins).await;
            // The turn's ids onto the context now — an office export's artifact attaches under
            // them, so the reply that reports the file can hand it over.
            let runner =
                runner.map(|r| r.with_turn(input.thread_id.as_str(), input.run_id.as_str()));
            match (runner, chosen.clone()) {
                (Some(runner), Some((recipe, values))) => {
                    Some(runner.with_chosen_recipe(recipe, values))
                }
                (runner, _) => runner,
            }
        }
        _ => None,
    };
    // bar_chart / form are painted by the client from TOOL_CALL frames. Offer them on
    // every AG-UI turn, including coworkers with no computer, so a chart request is a
    // tool call rather than streamed markdown. And `use_skill`, for the skills its owner attached
    // that this person may use (#270): the system message lists them from this same runner.
    let tools = super::chat_ui::attach(tools);
    // The live stream, made here so `message_bot` can put its `messaged` row on it (#314).
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let tools = match (&account_id, &run_coworker) {
        (Some(account), Some(coworker)) => {
            let tools = crate::skills::onto(&state, account, coworker, tools).await;
            let (run, live) = (Some(input.run_id.as_str()), Some(tx.clone()));
            crate::pairs::onto(&state, (account, coworker), run, live, Some(tools)).await
        }
        _ => Some(tools),
    };

    // Where it asks (`turn_request` says what follows), before the system message that names it.
    let (run, account) = (&input.run_id, account_id.as_ref());
    let route = local_proxy::route(&state, account, chosen_source, None, run, own).await;

    // Who this coworker is, plus whose computer its tools touch. Desktop `sendPrompt` already
    // composes this; AG-UI used to send `system: None`, so a Description saved as the standing
    // role never reached the model. A run with no coworker still composes nothing — there is
    // nobody to introduce.
    let mut recorded_skill: Option<String> = None;
    let system = match (account_id.as_ref(), run_coworker.as_ref()) {
        (Some(account_id), Some(coworker_id)) => {
            let persona = crate::persona::of(&state, coworker_id, coworker_role).await;
            let has_computer = tools.as_ref().is_some_and(|runner| {
                runner.executor().is_some_and(|(_, context)| {
                    context.box_id.is_some() || context.group_box.is_some()
                })
            });
            let reaches_user_machine = tools.as_ref().is_some_and(|runner| {
                runner
                    .tool_schemas()
                    .iter()
                    .any(|schema| schema["function"]["name"] == opengrok_tools::USER_MACHINE_SHELL)
            });
            let user_machine_label = if reaches_user_machine {
                crate::local_exec::enabled_machine(&state.auth.store, account_id.as_str())
                    .await
                    .map(|(_id, label)| label)
                    .filter(|label| !label.trim().is_empty())
            } else {
                None
            };
            let has_screen = tools.as_ref().is_some_and(|runner| runner.has_screen());
            let network_off = tools.as_ref().is_some_and(|runner| runner.network_off());
            let network_unconfirmed = tools
                .as_ref()
                .is_some_and(|runner| runner.network_unconfirmed());
            let has_recipes = tools.as_ref().is_some_and(|runner| runner.has_recipes());
            // What the person named this turn, kept to what this bot can actually run.
            let preferred = honour_preferences(&preferred_tools_from(&input), tools.as_ref());
            // The recipe is named to the model by its NAME, not its id: the sentence is read by
            // something that has to repeat it back to a person, and `rcp_01a0ac0a-…` is not a
            // thing anybody chose. A recipe the store cannot produce says nothing at all rather
            // than naming an id, because a prompt that cites what the person cannot see is worse
            // than a prompt that stays quiet.
            let chosen_line = match (&chosen, has_recipes) {
                (Some((recipe_id, values)), true) => {
                    match state.auth.store.recipe(recipe_id).await {
                        Ok(Some(row)) => crate::persona::chosen_recipe_line(&row.name, values),
                        _ => String::new(),
                    }
                }
                _ => String::new(),
            };
            // The skill the person chose, read against the account the token named and never
            // against anything in the body. A skill that cannot be given does not cost the turn —
            // it costs the skill, and the coworker is told to say so rather than answer as though
            // it had followed instructions it never saw (CLAUDE.md #8). No id on this request
            // reuses the skill from a prior run on this thread.
            let who = (account_id, coworker_id);
            let (skill_line, skill_id) =
                skill_line_for_turn(&state, who, &input.thread_id, &input, tools.as_ref()).await;
            recorded_skill = skill_id;
            // Who is speaking and what day it is, from the token's account: FIRST in the tail,
            // and so never after the skill line, which must stay last.
            let speaker = crate::persona::speaker_line(
                &crate::persona::caller(&state, account_id).await,
                chrono::Utc::now().date_naive(),
                "UTC",
            );
            let text = crate::persona::system_message(
                &coworker_name,
                &persona,
                route.asks(&model),
                Some(&format!(
                    "{speaker}\n\n{}{}{}{}{}{}{}{}{}{}",
                    crate::persona::computer_system_prompt(
                        has_computer,
                        has_screen,
                        has_recipes,
                        reaches_user_machine,
                        user_machine_label.as_deref(),
                    ),
                    crate::persona::network_off_line(network_off, network_unconfirmed),
                    tools
                        .as_ref()
                        .map(ToolRunner::unavailable_plugins_line)
                        .unwrap_or_default(),
                    crate::persona::preferred_tools_line(&preferred),
                    crate::persona::mentioned_plugins_line(&mentioned, &asked_plugins),
                    crate::persona::plugins_off_line(&plugins_off),
                    form_lines,
                    // After what it may do, as the author's words they are, and before this
                    // message's own choices: a skill it may reach for on any turn.
                    tools
                        .as_ref()
                        .map(ToolRunner::skills_line)
                        .unwrap_or_default(),
                    chosen_line,
                    // LAST, AFTER EVERY SEGMENT THAT SAYS WHAT THIS COWORKER MAY DO. A skill body
                    // is prose a person wrote; it must not be able to read as granting itself
                    // something the segments above just withheld. `persona::fenced_skill`
                    // carries the rest of the reason, and an empty one adds nothing at all — a
                    // turn with no skill is byte-for-byte the turn we had before.
                    skill_line,
                )),
            );
            if text.is_empty() { None } else { Some(text) }
        }
        // A turn with no coworker composes no persona — there is nobody to introduce, and loading
        // a named coworker's role without a principal would leak configuration by the shape of the
        // reply. A SKILL CHOSEN ON SUCH A TURN IS STILL SOMETHING THE PERSON ASKED FOR AND IS NOT
        // GETTING, and dropping it here without a word was exactly the silence the refusal line
        // exists to prevent. It is the generic sentence because there is no account to resolve the
        // skill against, and trimmed because there is no paragraph above it to join.
        _ => match chosen_skill_from(&input) {
            Some(_) => {
                tracing::warn!("a turn carried a chosen skill with no coworker to give it to");
                Some(crate::persona::SKILL_UNAVAILABLE_LINE.trim().to_string())
            }
            None => None,
        },
    };

    // The thread's own log, with this turn's new messages at the end — or the client's copy, on
    // a thread the log cannot tell whole (`history`'s module note).
    // This turn's files, read and marked as sent before the turn is asked (#229).
    let mut attached = match &account_id {
        Some(account) => {
            super::attachments::resolve(&state, account, &input, run_coworker.as_ref()).await
        }
        None => super::attachments::Attached::default(),
    };
    // A staged Office file announces itself with the run's opening, not with a tool call: the
    // frames ride the request into the harness.
    let staged = std::mem::take(&mut attached.customs);
    let asked = super::history::for_turn(&state, account_id.as_ref(), &input, &attached).await;
    let mut messages = asked.messages;
    // ONE system message. A client-supplied `system` in the AG-UI body would be a second claim
    // about the same coworker; drop it when we composed one.
    if system.is_some() {
        messages.retain(|message| message.role != "system");
    }
    // NativeChat steer is stop, then a new run whose body is chat bubbles. A stopped
    // turn often has tool results and no assistant text, so the next run repeats the
    // work. Splice those results in front of the new message. A finished answer is
    // already in the bubbles. A parked card is a fresh turn, not a continuation. A history
    // composed from the log already carries the stopped turn's tools, so only the client's copy
    // is spliced.
    if asked.from_client
        && let Some(account) = &account_id
    {
        continue_stopped_turn(
            &state,
            account,
            &input.thread_id,
            &input.run_id,
            &mut messages,
        )
        .await;
    }

    let source = route.source();
    let who = (run_coworker.as_ref(), account_id.as_ref());
    let thinks = (route, model, effort);
    let mut request = turn_request(&state, who, thinks, system.clone(), messages).await;
    request.customs = staged;

    // The journal writes each round to Postgres before the next model call, and stamps the run's
    // owner so only they can read it back. A run that cannot be recorded fails inside the loop
    // rather than being streamed (CLAUDE.md #5).
    let journal = StoreJournal {
        state: state.clone(),
        thread_id: input.thread_id.clone(),
        account_id: account_id.clone(),
        coworker_id: run_coworker,
        model: Some(request.model.clone()),
        effort: request.effort,
        inference_source: source,
        system,
        skill_id: recorded_skill,
        offered_skills: tools.iter().flat_map(ToolRunner::offered_skills).collect(),
        prompt: Some(asked.prompt),
        limits,
        generation: 0,
    };

    // THE CLAIM. Exactly one POST per run id appends the run's `Started` at the first seq, under
    // the unique `(stream_id, stream_seq)`, and only that one runs a loop
    // (`formal/tla/RunLifecycle.tla` OneDriver). One that lost to a POST racing it is answered
    // the way the check at the top answers a retry.
    match journal.claim(&input.run_id).await {
        Ok(true) => {}
        Ok(false) => {
            refund_unscoped(&state, unscoped_charge.as_deref());
            return answer_for_existing_run(&state, account_id.as_ref(), &input.run_id)
                .await
                .unwrap_or_else(run_taken);
        }
        Err(error) => return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }

    // Hold the run while we serve it, so a recovery sweep does not mistake a slow model call for
    // an abandoned run. Released when the spawned turn drops — including when the process dies,
    // which is exactly the case the lease exists for.
    let thread_id = input.thread_id.clone();
    let run_id = input.run_id.clone();
    let at_ms = now_ms();
    let door = state.door.clone();
    let lease = crate::recovery::Lease::new(crate::recovery::hold(
        state.clone(),
        RunId::from_stored(run_id.clone()),
    ));
    // The HTTP body is the live sink. Awaiting the conversation first, then wrapping the
    // Vec in `stream::iter`, is what made NativeChat paint the whole reply at once.
    let tagged = turn_plugins.mentioned.clone();
    tokio::spawn(async move {
        let _lease = lease;
        // Card mint + `entryId` stamp happen inside the sink, **before** the CUSTOM frame
        // is forwarded, so NativeChat sees the id on the AG-UI stream.
        let sink = AgUiSink {
            tx,
            gateway,
            coworker_id: journal.coworker_id.clone(),
            account_id: journal.account_id.clone(),
            thread_id: journal.thread_id.clone(),
            tagged,
            form_hold: Mutex::new(crate::agui::user_form::UserFormSseHold::default()),
        };
        let _ = run_conversation_within(
            door.as_ref(),
            tools.as_ref(),
            &journal,
            request,
            RunContext::new(thread_id, run_id, at_ms),
            RunBudget::held_to(&limits),
            Some(&sink),
        )
        .await;
    });
    sse(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|event| (Ok::<_, std::io::Error>(event), rx))
    }))
}

/// A turn's request on the route it asks (`local_proxy::route`): on a person's own proxy the
/// proxy's model and NO gateway key, not even minted late for a turn the gateway never sees; else
/// the pin, and the key whose cap it counts against. `who` is the coworker whose spend it is and
/// the account that pays.
pub(crate) async fn turn_request(
    state: &AgUiState,
    (coworker, account): (Option<&CoworkerId>, Option<&AccountId>),
    (route, pin, effort): (local_proxy::Route, String, Effort),
    system: Option<String>,
    messages: Vec<ChatMessage>,
) -> ModelRequest {
    let (effort, fallback_for) = route.fallback(effort);
    let (model, endpoint) = route.asked(pin);
    let gateway_key = match (&endpoint, coworker, account) {
        (None, Some(coworker), Some(account)) => {
            crate::spend::key_for(state, coworker, account).await
        }
        // No coworker, or nobody named: the deployment's key, unmetered. `/ag-ui` refuses an
        // unsigned turn and bounds a coworker-less one per account (`budget::AGUI_UNSCOPED`);
        // inventing a payer would put a stranger's turn on somebody's pool.
        _ => None,
    };
    ModelRequest {
        gateway_key,
        spend_scope: coworker.map(|c| c.as_str().to_string()),
        spend_actor: account.map(|a| a.as_str().to_string()),
        context_tokens: state.context_for(&model).await,
        model,
        effort,
        system,
        messages,
        tools: Vec::new(),
        endpoint,
        fallback_for,
        customs: Vec::new(),
    }
}
