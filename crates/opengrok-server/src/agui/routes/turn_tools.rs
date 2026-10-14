//! Turn tool selection, connectors, and plugins.

use super::*;

/// What a run of `coworker` for `account` may spend, as the limits it captures: the server's
/// budget, under the ceiling of every org the two are in, under `narrower` (a routine's own
/// limits, or those a resumed run captured at its start). Read on every start and every resume,
/// so no run falls back to the server's budget while its org sets less: `None` when the ceiling
/// cannot be read, logged here once, and every caller refuses rather than run without it.
pub(crate) async fn run_limits(
    state: &AgUiState,
    account: &AccountId,
    coworker: Option<&CoworkerId>,
    narrower: RunLimits,
) -> Option<RunLimits> {
    let read = state.auth.store.org_run_ceiling(account, coworker).await;
    let ceiling = read
        .inspect_err(|error| tracing::error!(%error, "an org's run ceiling could not be read"))
        .ok()?;
    Some(RunBudget::default().limits().and(ceiling).and(narrower))
}

/// Build the tools for this run, bound to this coworker's own computer and this principal's grant.
///
/// THE GRANT IS READ HERE, ON THIS TURN. Not cached from sign-in and not carried in the request:
/// a grant revoked a second ago must stop this turn (CLAUDE.md #6).
/// The same binding, addressed by coworker rather than by request — because the scheduler and the
/// monitor fire runs with no `RunAgentInput` anywhere in sight.
/// `wake_patience` bounds how long the first box-bound tool call waits for a sleeping box before
/// answering that the computer is down or still starting: a turn can afford `TURN_WAKE_PATIENCE`;
/// the MCP door, whose caller (Claude Code) has its own request timeout, passes a shorter one and
/// lets the tool result say "still starting".
pub(crate) async fn tools_for_coworker(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    approved: &[String],
    review_approved: &[String],
    wake_patience: std::time::Duration,
) -> Option<ToolRunner> {
    let gates = (approved, review_approved);
    let turn = opengrok_integrations::turn::TurnPlugins::default();
    tools_for_turn(state, account_id, coworker_id, gates, wake_patience, &turn).await
}

/// [`tools_for_coworker`] for a turn whose message tagged plugins (`@cloudflare`): each one
/// `turn::mentioned` kept is switched on for this turn only (`with_mentioned`), with the account
/// the turn picked where it picked one.
pub(crate) async fn tools_for_turn(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    (approved, review_approved): (&[String], &[String]),
    wake_patience: std::time::Duration,
    turn: &opengrok_integrations::turn::TurnPlugins,
) -> Option<ToolRunner> {
    let coworker_id = coworker_id.clone();
    let (coworker, _) = state.auth.store.load_coworker(&coworker_id).await.ok()?;
    // A coworker with no computer gets no computer tools rather than tools that cannot run: a
    // tool the model is told about but that always refuses is a dead end it keeps trying. Its
    // plugins are another matter: they are the person's accounts at services, not programs on a
    // box, so a Bot without a computer still gets those (#359).
    let reach = (&state.auth.store, &*state.plugins);
    if coworker.computer().is_none() {
        let policy = opengrok_integrations::turn::tools_policy;
        let policy = policy(reach, account_id, &coworker_id, &turn.mentioned).await?;
        let (plugins, off) = connect_plugins(state, account_id, &coworker_id, &policy, turn).await;
        // A plugin's form needs no computer (#220).
        let offers = opengrok_integrations::turn::offers_form_cards;
        let form_cards = offers(reach, account_id, &coworker_id, turn).await;
        if plugins.tools.is_empty()
            && plugins.unavailable.is_empty()
            && off.is_empty()
            && !form_cards
        {
            return None;
        }
        let mut context = opengrok_tools::ToolContext::from_coworker(
            account_id.clone(),
            coworker_id.clone(),
            &coworker,
        );
        context.form_mode = turn.form_mode;
        let executor = opengrok_tools::Executor::without_a_computer(policy)
            .with_form_cards(form_cards)
            .with_plugins(plugins)
            .with_switched_off(off)
            .with_plugin_desk(plugin_desk(state))
            .with_approved(approved.iter().cloned())
            .with_review_approved(review_approved.iter().cloned());
        let executor = with_auto_review(state, account_id, &coworker_id, executor).await;
        return Some(ToolRunner::new(executor, context));
    }
    // Resolve the provider for this coworker's computer by its account's effective sharing mode and
    // scope (per-org / per-account / per-bot), then that scope's recorded kind — so tools run on the
    // same provider that created the box.
    let (mode, org_id) =
        super::provision::resolve_mode_for(state, account_id, coworker_id.as_str()).await;
    let (scope, scope_id, _) = super::provision::scope_for(
        &mode,
        account_id.as_str(),
        org_id.as_deref(),
        coworker_id.as_str(),
        coworker.is_group(),
    );
    let (mut box_id, kind, stopped) = state
        .auth
        .store
        .scoped_computer_full(scope, &scope_id)
        .await
        .ok()
        .flatten()?;
    let mut computer = super::provision::provider_for(state, org_id.as_deref(), &kind).await?;
    // The box is NOT woken here. A turn used to wait up to `wake_patience` for a sleeping box
    // before the model was even asked, and a plain "hi" paid for it (90 s with a dead box, 21 Sep
    // 2026). The executor wakes the box the first time a tool needs it, and the stream says so.
    // What stays is one cheap look at the box: a provider that refuses to say (401/403 — an ascii
    // key revoked, a computer this deployment may no longer reach) is taken over by local Docker
    // now — where this server runs Local VMs at all (`take_over_with_local_docker`).
    let _ = stopped;
    let mut running = false;
    match computer.state(&box_id).await {
        Ok(state) => running = state == "running",
        Err(error) => {
            let forbidden = matches!(
                &error,
                opengrok_box::BoxError::Refused {
                    status: 401 | 403,
                    ..
                }
            ) || error.to_string().contains("forbidden");
            let taken = if forbidden && kind != "local-docker" {
                super::provision::take_over_with_local_docker(
                    state,
                    account_id,
                    scope,
                    &scope_id,
                    org_id.as_deref(),
                    &error,
                    Some(box_id.as_str()),
                )
                .await
            } else {
                None
            };
            // No takeover keeps the refusing box rather than dropping every tool: the first
            // tool that needs it answers with the refusal, a result the model can relay, where
            // `None` here left it told it has no computer and never why (CLAUDE.md #8).
            match taken {
                Some((local, new_id)) => {
                    computer = local;
                    box_id = new_id;
                    running = true;
                }
                None => {
                    tracing::warn!(scope, scope_id = %scope_id, kind = %kind, code = %error.code(), box_id, "the box's state could not be read; a tool that needs it will say so")
                }
            }
        }
    }
    // The in-use stamp keeps the idle sweep off a box while it is used. A box that is asleep is
    // not in use by a turn that never touches it, so it is stamped only when running now — and by
    // the executor, through `on_woken`, the moment a tool (or a form fill) brings it up; a box
    // the executor merely finds running is not stamped twice.
    if running {
        let _ = state
            .auth
            .store
            .mark_scoped_used(scope, &scope_id, chrono::Utc::now().timestamp_millis())
            .await;
    }
    let stamp_store = state.auth.store.clone();
    let stamp_scope_id = scope_id.clone();
    let on_woken: opengrok_tools::OnWoken = std::sync::Arc::new(move |_| {
        let store = stamp_store.clone();
        let scope_id = stamp_scope_id.clone();
        tokio::spawn(async move {
            let _ = store
                .mark_scoped_used(scope, &scope_id, chrono::Utc::now().timestamp_millis())
                .await;
        });
    });

    // The policy a turn runs under, so a member of the org talking to a shared coworker gets the
    // tools its owner's grant allows — the same answer the run door's gate just gave. Callers
    // that must be the owner (routines, the tool listing) gate on that before they get here.
    let policy = opengrok_integrations::turn::tools_policy;
    let policy = policy(reach, account_id, &coworker_id, &turn.mentioned).await?;

    // The plugins this coworker may use, connected with its own credentials. On a shared
    // coworker that includes its `bot`-scoped connections, which its OWNER authorised: a member's
    // turn acts through them, while the owner's `user`-scoped ones stay the owner's (ROADMAP
    // 19.4). Narrowing that is a decision about what sharing lends, not a filter to add here.
    let (plugins, switched_off) =
        connect_plugins(state, account_id, &coworker_id, &policy, turn).await;

    // Bind the SCOPE's live box, not the coworker's frozen hire-time id. They match at hire, but a
    // reset or re-provision changes the account's box while the aggregate id stays put — and this is
    // the same box we just resumed above, so exec must run on it, or a reset would leave the bot
    // executing against a destroyed box.
    let mut context = opengrok_tools::ToolContext::from_coworker(
        account_id.clone(),
        coworker_id.clone(),
        &coworker,
    );
    // A box with a display gets the screen tools (`open_url`, `computer`); a headless one is
    // never told about them, so it cannot be sent down a dead end.
    // Whether the coworker has a screen is how its box is made, not whether the box happens to be
    // awake: the prompt and the tool list then say the same thing on every turn.
    let screen = computer.offers_a_screen(&box_id).await;
    // The tunnel probe asks the box's guest, which only answers when the box is up. For a box
    // that is asleep now, the executor asks the guest right after the first leave-box tool wakes
    // it, and raises the consent card only if the tunnel is really there.
    let egress_tunnel = if running {
        if state.egress_tunnel_for(computer.as_ref(), &box_id).await {
            opengrok_tools::EgressTunnelMode::On
        } else {
            opengrok_tools::EgressTunnelMode::Off
        }
    } else if state.egress_tunnel_enabled() {
        opengrok_tools::EgressTunnelMode::AskTheBoxAfterWake
    } else {
        opengrok_tools::EgressTunnelMode::Off
    };
    // The person's standing answer for THIS computer to the tunnel's card, keyed by the scope
    // the box lives under so a reset or takeover that changes the box id keeps the choice.
    // No row is `ask`: one card per run, as before there was a choice.
    let (egress_policy, egress_unconfirmed) =
        provision::egress_policy_for_turn(state, scope, &scope_id).await;
    let which = provision::screen_for(state, account_id, coworker_id.as_str()).await;
    // A Bot's own screen is started now, in the background, when its computer is already up:
    // started on first use, it kept the Bot's first action waiting and showed the person a black
    // screen while it came up. A computer asleep is not woken for it.
    if running && which.bot().is_some() {
        let opened = (computer.clone(), box_id.as_str(), &which);
        provision::open_own_screen(state, account_id, &coworker_id, opened);
    }
    context.set_screen(which);
    context.box_id = Some(opengrok_core::id::BoxId::from_stored(box_id));
    let transcript_hold = match state
        .auth
        .store
        .gateway_transcript(&coworker_id, account_id)
        .await
    {
        Ok(entries) => entries
            .iter()
            .any(opengrok_tools::user_form::holds_the_screen),
        // A transcript we cannot read must not freeze every screen tool; the form's own
        // submit path still refuses to log secrets.
        Err(_) => false,
    };
    // The coworker's one computer is held whichever conversation raised the form; the refusal
    // names that conversation when a parked run ties the hold to one.
    let held_in = form_hold_thread(state, account_id, &coworker_id).await;
    context.screen_hold = transcript_hold || held_in.is_some();
    context.screen_held_in = held_in;
    context.form_mode = turn.form_mode;

    // The recipes this bot was granted: offered as `run_recipe` only with a screen to run on.
    let recipes = if screen {
        crate::recipes::offers_for(state, &coworker_id).await
    } else {
        Vec::new()
    };
    let mut executor = opengrok_tools::Executor::with_policy(computer, policy)
        .with_wake_patience(wake_patience)
        .with_on_woken(on_woken)
        .with_screen(screen)
        .with_recipes(recipes, crate::recipes::source_for(state))
        .with_plugins(plugins)
        .with_switched_off(switched_off)
        .with_approved(approved.iter().cloned())
        .with_review_approved(review_approved.iter().cloned())
        .with_egress_tunnel_mode(egress_tunnel)
        .with_egress_policy(egress_policy)
        .with_egress_policy_unconfirmed(egress_unconfirmed)
        // This Bot's routines (#316), through the desk the routes use; the ceiling's row says
        // whether they are offered, and the context's account and Bot are all they answer for.
        .with_routines(Arc::new(crate::autonomy::desk::Tools {
            state: state.clone(),
        }))
        // The person's plugins and accounts (#359), through the desk the Plugins routes use.
        .with_plugin_desk(plugin_desk(state))
        // This Bot's own computer (7 Oct 2026), through the desk the Computer pane's routes use.
        .with_computer_desk(Arc::new(crate::computer_desk::Tools {
            state: state.clone(),
        }))
        // Its office documents on that same computer — attached only here, on the path that
        // HAS a box, so a boxless coworker is never offered tools that cannot run.
        .with_office_desk(crate::office_desk::desk(state.clone()));
    // The reverse-exec tool: offered ONLY when this account has an enrolled, enabled machine to
    // reach — otherwise the model is never told about a channel it cannot use. Bound to that
    // machine, and to this coworker for the audit origin. The executor offers it only where the
    // coworker's ceiling allows (#268).
    if let Some((machine_id, _label)) =
        crate::local_exec::enabled_machine(&state.auth.store, account_id.as_str()).await
    {
        executor =
            executor.with_user_machine(std::sync::Arc::new(crate::local_exec::ReverseExecSink {
                auth: state.auth.clone(),
                coworker_id: coworker_id.as_str().to_string(),
                machine_id,
            }));
    }

    let executor = with_auto_review(state, account_id, &coworker_id, executor).await;
    Some(ToolRunner::new(executor, context))
}

/// `executor` under this Bot's auto-review, when its account has it on: the judge every call
/// a Bot's tools make passes, with a computer or without one. A Bot without a computer still runs
/// plugin tools (#359, #220), from its turns and from the MCP door, and those are judged too.
async fn with_auto_review(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    executor: opengrok_tools::Executor,
) -> opengrok_tools::Executor {
    // THE TIER WALK HAPPENS HERE, ONCE PER RUN (docs/AUTO-REVIEW.md §3). Per tool call the check
    // is one in-memory test on the runner; a run that started before a PUT keeps the policy it
    // started with, and a resumed run rebuilds its runner through this function and re-resolves.
    // Nothing is attached when the policy is off or empty, so an unreviewed run costs nothing.
    let effective = crate::auto_review::load_effective(
        &state.auth.store,
        account_id.as_str(),
        Some(coworker_id.as_str()),
    )
    .await;
    if let Some(policy) = effective.review_policy() {
        // The judge is billed to the coworker whose turn raised it, and checked against that
        // coworker's limits. Resolved HERE because this function already holds the coworker and
        // runs once per run, which is exactly where the run path resolves its own key.
        //
        // A coworker over its cap now has its judge refused, and a refused judge is
        // `Unavailable`, which the executor turns into an Ask — so the tool call raises a card
        // for a person rather than proceeding unreviewed or spending past a limit.
        let judge =
            opengrok_harness::ModelJudge::new(state.door.clone(), state.auto_review_model.clone())
                .for_coworker(
                    coworker_id.as_str(),
                    account_id.as_str(),
                    crate::spend::key_for(state, coworker_id, account_id).await,
                );
        return executor.with_auto_review(policy, Arc::new(judge));
    }
    executor
}

/// The desk a turn's plugin tools answer through, on the registry the Plugins routes serve.
fn plugin_desk(state: &AgUiState) -> Arc<dyn opengrok_tools::plugin_desk::PluginDesk> {
    Arc::new(crate::plugin_desk::configured(
        state,
        crate::plugin_registry::registry(),
    ))
}

/// The conversation of a run suspended on a user-form: it holds the screen whether or not the
/// transcript has a card for it — the pending row is the truth, the card is chrome.
async fn form_hold_thread(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
) -> Option<String> {
    let Ok(run_ids) = state.auth.store.awaiting_approval(account_id).await else {
        return None;
    };
    for run_id in run_ids {
        let Ok((run, _)) = state.auth.store.load_run(&run_id).await else {
            continue;
        };
        if !crate::agui::resume::run_belongs_to(&run, coworker_id) {
            continue;
        }
        if matches!(
            run.pending.as_ref().map(|pending| pending.reason),
            Some(opengrok_core::run::SuspendReason::UserForm)
        ) {
            return Some(run.thread_id);
        }
    }
    None
}

/// The access token for a connection, refreshed first if it is about to expire.
///
/// REFRESHED BEFORE USE, NOT AFTER FAILURE. Waiting for a 401 means every expiry costs a person one
/// visibly failed tool call, and a model that reads that failure may go and do something else. The
/// leeway lives in `ConnectionView::is_expiring` so a token cannot expire mid-flight.
///
/// A refusal that means the person revoked access disconnects rather than retrying: `invalid_grant`
/// is a decision somebody made, and retrying it forever turns a revoked connection into a permanent
/// error loop.
async fn live_token(
    state: &AgUiState,
    vault: &opengrok_store::Vault,
    chosen: &opengrok_core::connection::ConnectionView,
) -> Option<String> {
    // Logged, not swallowed: a token sealed under a lost key used to read as "not connected", and
    // the plugin that needed it just quietly went missing.
    let stored = state
        .auth
        .store
        .open_credential(vault, &chosen.id)
        .await
        .inspect_err(|error| tracing::error!(%error, connection = %chosen.id, "a connection's token will not open"))
        .ok()
        .flatten();

    if !chosen.is_expiring(now_ms()) {
        return stored;
    }

    // Expiring. Without a provider or a refresh token there is nothing to do but use what we have
    // and let the provider say no — which is still better than refusing a call we might complete.
    let Some(config) = state.connectors.providers.get(&chosen.connector) else {
        return stored;
    };
    let Ok(Some(refresh_token)) = state
        .auth
        .store
        .open_credential(vault, &format!("{}_refresh", chosen.id))
        .await
    else {
        return stored;
    };

    // Bounded like a plugin's own connect: this runs before any server is dialled, and a token
    // endpoint that never answers would otherwise hold the turn just as a dead server did (#199).
    let within = opengrok_tools::mcp::Pool::global().deadlines().connect;
    let client = reqwest::Client::builder()
        .timeout(within)
        .build()
        .unwrap_or_default();
    match crate::connections::flow::refresh(&client, config, &refresh_token).await {
        Ok(token) => {
            let at_ms = now_ms();
            let expires_at = token.expires_at_ms(at_ms);

            // Sealed before it is returned, so a crash between here and the next request does not
            // leave the old token in the database and the new one only in memory.
            let access_sealed = vault.seal(&chosen.id, &token.access_token).ok();
            // Google omits the refresh token on a refresh, so the stored one is kept.
            let refresh_sealed = token
                .refresh_token_to_store(Some(&refresh_token))
                .filter(|rotated| rotated != &refresh_token)
                .and_then(|rotated| vault.seal(&format!("{}_refresh", chosen.id), &rotated).ok());
            if !crate::connections::routes::store_refreshed(
                &state.auth.store,
                &chosen.id,
                access_sealed.as_ref(),
                refresh_sealed.as_ref(),
                expires_at,
                at_ms,
            )
            .await
            {
                return None;
            }

            tracing::info!(
                connector = chosen.connector,
                "refreshed an expiring connection"
            );
            Some(token.access_token)
        }
        Err(error) => {
            if error.is_revoked() {
                tracing::warn!(
                    connector = chosen.connector,
                    "a connection was revoked at the provider; disconnecting it"
                );
                let _ = disconnect_revoked(state, &chosen.id).await;
                return None;
            }
            tracing::warn!(%error, connector = chosen.connector, "could not refresh; using what we have");
            stored
        }
    }
}

/// Record that a provider has revoked a connection.
///
/// Written down rather than merely logged: a person looking at their connections should see it is
/// gone, and the next run should not try again.
async fn disconnect_revoked(state: &AgUiState, id: &str) -> Result<(), opengrok_store::StoreError> {
    let mut tx = state.auth.store.pool().begin().await?;
    let remove = opengrok_integrations::accounts::remove_in;
    remove(&state.auth.store, &mut tx, id, now_ms()).await?;
    Ok(tx.commit().await?)
}

/// Open a session with every plugin server this coworker can both reach and be permitted to use.
///
/// TWO GATES, AND BOTH MATTER. A plugin must be in the coworker's ceiling — installing one on the
/// server is not the same as letting a coworker use it — and its credential must resolve, because
/// a connected tool without a token is a tool that fails at the moment of use rather than at the
/// moment of offer.
///
/// A server that will not connect in time is left out with a warning rather than failing the run,
/// and the turn is told it is unavailable: the other tools still work, and a turn that dies — or
/// waits forever — because one connector is down is worse than one that proceeds without it.
async fn connect_plugins(
    state: &AgUiState,
    account_id: &opengrok_core::id::AccountId,
    coworker_id: &CoworkerId,
    policy: &opengrok_policy::Context,
    turn: &opengrok_integrations::turn::TurnPlugins,
) -> (
    opengrok_tools::mcp::Dialled,
    Vec<opengrok_tools::mcp::McpTool>,
) {
    // Every credential this coworker can use, keyed the way a plugin's placeholders name them:
    // `GMAIL_TOKEN` for the `gmail` connector. Which account serves each service is the Bot's
    // pin or the only one it may use, never a guess between several (#359, `resolve`).
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    if let Some(vault) = state.vault.as_ref().filter(|_| !state.plugins.is_empty()) {
        let store = &state.auth.store;
        let chosen = opengrok_integrations::accounts::for_operator_plugins;
        for chosen in chosen(store, account_id, coworker_id).await {
            if let Some(token) = live_token(state, vault, &chosen).await {
                values.insert(opengrok_plugins::token_key(&chosen.connector), token);
            }
        }
    }

    let mut endpoints = Vec::new();
    // A plugin whose own name has a dot is never dialled. Its tools' names would read as another
    // plugin's server — a call is routed by the first segment (`split_qualified`) — and a ceiling's
    // `"<plugin>.*"` would admit them in that other plugin's name (#268).
    for plugin in state
        .plugins
        .values()
        .filter(|p| !p.manifest.name.contains('.'))
    {
        let (reachable, problems) = opengrok_tools::mcp::endpoints_for(plugin, &values);
        for problem in problems {
            tracing::debug!(%problem, plugin = plugin.manifest.name, "a plugin server is unavailable");
        }
        // A server the ceiling leaves nothing to offer is not dialled at all.
        endpoints.extend(reachable.into_iter().filter(|endpoint| {
            let prefix = endpoint.qualify("");
            opengrok_policy::may_run_any_under(account_id, coworker_id, &prefix, policy)
        }));
    }

    let (store, vault) = (&state.auth.store, state.vault.as_deref());
    let operator = |name: &str| state.plugins.contains_key(name);
    let installed = opengrok_integrations::turn::endpoints;
    let dialled = installed(
        store,
        vault,
        account_id,
        coworker_id,
        policy,
        turn,
        operator,
    );
    endpoints.extend(dialled.await);

    // The ceiling gate, per tool and on every turn, whether the listing is fresh or pooled. A tool
    // the coworker may not run is not offered at all — being told about a tool that always
    // refuses is a dead end a model retries.
    let permitted = |tool: &str| {
        let action = opengrok_policy::Action::RunTool(tool);
        let decision = opengrok_policy::decide(account_id, coworker_id, action, policy);
        decision.is_allowed() || decision.needs_approval()
    };
    // A tool the person switched off inside a plugin that is on (Never allow, `-<tool>`) is read
    // too, and then kept apart: never offered, only listed, so it can be switched back on.
    let switched_off = |tool: &str| match policy.ceiling.as_ref().map(|c| &c.tools) {
        Some(opengrok_policy::ToolSet::Only(names)) => {
            names.contains(&opengrok_policy::excluding(tool))
        }
        _ => false,
    };
    let scope = format!("{account_id}/{coworker_id}");
    let mut dialled = opengrok_tools::mcp::Pool::global()
        .dial(&scope, endpoints, |tool| {
            permitted(tool) || switched_off(tool)
        })
        .await;
    let (off, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut dialled.tools)
        .into_iter()
        .partition(|tool| switched_off(&tool.qualified_name));
    dialled.tools = kept;
    (dialled, off)
}
