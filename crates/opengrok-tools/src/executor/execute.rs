//! The dispatch half: `execute`, and the builtin tools it runs.

use super::*;

impl Executor {
    /// Run one call. Never returns `Err` for a *tool* failure: the model gets a result either way,
    /// and only the caller's own bugs propagate.
    ///
    /// THE ORDER IS THE SAFETY ARGUMENT. Identity first, for every tool. Then the PRIMARY gate for
    /// the tool — the machine's own policy for `user_machine_shell`, the coworker's grant for
    /// everything else. Then, once, the auto-review judge — skipped when the gate already denied
    /// (no spend on a refused command), when a person already answered this call (a resumed call
    /// is never re-judged), or when no active policy is attached. Then the ladder in
    /// `review::combine`, which is pure and table-tested. Only `Outcome::Run` reaches a dispatch.
    pub async fn execute(&self, context: &ToolContext, call: &ToolCall) -> ToolResult {
        // Text that would not parse arrives as null. Run with defaults it is a different call
        // from the one the model meant, so it is refused before identity, gates and cards.
        if !call.arguments.is_object() {
            return ToolResult::refused(&call.id, NOT_AN_OBJECT);
        }
        // The identity rule, applied once, before anything reads an argument — including the judge
        // and the card. Whatever the model wrote for these keys is discarded rather than checked.
        let arguments = overwrite_identity(&call.arguments, context);
        let tool_name = self.internal_tool_name(&call.name);
        // A plugin's form is the one built-in a Bot with no computer may use: it never touches a
        // box. Every other, and a sign-in form, is refused in words.
        let plugin_form = tool_name == REQUEST_USER_FORM
            && self.form_cards
            && user_form::form_mode::is_form_mode_source(&arguments);
        if self.boxless && Self::builtin_tool_names().contains(&tool_name.as_str()) && !plugin_form
        {
            return ToolResult::refused(&call.id, NO_COMPUTER);
        }
        // grok-box:local has neither binary (checked 22 Sep 2026). A box shell that names
        // them wakes the desktop, misses PATH, and then tries to fetch one. Refuse before
        // the egress card and before the box is started. The user machine is the other tool.
        if tool_name == "shell"
            && let Ok(args) = serde_json::from_value::<ShellArgs>(arguments.clone())
            && command_targets_absent_bir_binary(&args.command)
        {
            return ToolResult::refused(&call.id, BIR_NOT_ON_THE_BOX);
        }
        // A routine tool's arguments, and for a delete the routine it names, are asked BEFORE any
        // gate (#316): a call that could never run is told why now, never after a card.
        let routine = match (routine::is_routine_tool(&tool_name), self.routines.as_ref()) {
            (false, _) => None,
            (true, None) => return ToolResult::refused(&call.id, "routines are not on offer here"),
            (true, Some((desk, listing))) => {
                let call_of = (tool_name.as_str(), &arguments);
                match routine::admit(desk.as_ref(), *listing, context, call_of).await {
                    Ok(admitted) => Some((desk.clone(), admitted)),
                    Err(why) => return ToolResult::refused(&call.id, why),
                }
            }
        };
        // An office call's arguments are read BEFORE any gate the same way — its `document`
        // handle is just a string until the desk answers it.
        let office_call = match (
            office_desk::is_office_tool(&tool_name),
            self.office_desk.as_ref(),
        ) {
            (false, _) => None,
            (true, None) => {
                return ToolResult::refused(&call.id, "office documents are not on offer here");
            }
            (true, Some(desk)) => match office_desk::admit((tool_name.as_str(), &arguments)) {
                Ok(admitted) => Some((desk.clone(), admitted)),
                Err(why) => return ToolResult::refused(&call.id, why),
            },
        };
        // A plugin tool's arguments, and for one that asks first what it acts on, as stored, are
        // asked BEFORE any gate too (#359).
        let computer_call = match (
            computer_desk::is_computer_tool(&tool_name),
            self.computer_desk.as_ref(),
        ) {
            (false, _) => None,
            (true, None) => {
                return ToolResult::refused(&call.id, "this Bot's computer is not on offer here");
            }
            (true, Some(desk)) => {
                let call_of = (tool_name.as_str(), &arguments);
                match computer_desk::admit(desk.as_ref(), context, call_of).await {
                    Ok(admitted) => Some((desk.clone(), admitted)),
                    Err(why) => return ToolResult::refused(&call.id, why),
                }
            }
        };
        let plugin_call = match (
            plugin_desk::is_plugin_desk_tool(&tool_name),
            self.plugin_desk.as_ref(),
        ) {
            (false, _) => None,
            (true, None) => return ToolResult::refused(&call.id, "plugins are not on offer here"),
            (true, Some(desk)) => {
                let call_of = (tool_name.as_str(), &arguments);
                match plugin_desk::admit(desk.as_ref(), context, call_of).await {
                    Ok(admitted) => Some((desk.clone(), admitted)),
                    Err(why) => return ToolResult::refused(&call.id, why),
                }
            }
        };
        // Two different yeses. The gate's approval (the machine owner's or the policy's card)
        // releases the gate's ask AND skips the judge; a review approval skips only the judge.
        let gate_approved = self.approved_calls.contains(&call.id);
        let review_approved = gate_approved || self.review_approved_calls.contains(&call.id);

        // The reverse-exec tool is authorized by the LOCAL-EXEC policy (its sink judges the
        // command against never/ask/bypass + the standing rules), NOT the grant's profile — which
        // would Deny it for any coworker whose profile lists only the box tools — once its ceiling
        // lets it reach the machine at all. It runs on the USER'S machine, so it needs no box.
        let user_machine_command = if tool_name == USER_MACHINE_SHELL {
            match serde_json::from_value::<ShellArgs>(arguments.clone()) {
                Ok(args) => Some(args.command),
                Err(error) => {
                    // Say what would work: a model that sent nothing needs the shape, not
                    // just the complaint, or it sends nothing again.
                    return ToolResult::refused(
                        &call.id,
                        format!(
                            "bad arguments: {error}; call again as {{\"command\": \"<the shell command>\"}}"
                        ),
                    );
                }
            }
        } else {
            None
        };
        let gate = if let Some(command) = user_machine_command.as_deref() {
            let Some(sink) = self.user_machine.as_ref() else {
                return ToolResult::refused(
                    &call.id,
                    "no machine of yours is connected, so nothing can run there",
                );
            };
            if !self.reaches_the_machine() {
                return ToolResult::refused(&call.id, MACHINE_SWITCHED_OFF);
            }
            match sink.decide(&context.account_id, command).await {
                UserMachineVerdict::Allow => Gate::Allow,
                UserMachineVerdict::Ask => Gate::Ask(
                    AwaitingReason::ExecConsent,
                    "your machine's owner must approve this command".to_string(),
                ),
                UserMachineVerdict::Deny(why) => Gate::Deny(why),
            }
        } else {
            // POLICY BEFORE ANYTHING RUNS, AND BEFORE ANYTHING IS EVEN LOOKED UP. A refusal
            // reaches the model as a result it can reason about rather than an exception that
            // kills the run (CLAUDE.md #8), and it names the rule so a person can fix it.
            let decision = opengrok_policy::decide(
                &context.account_id,
                &context.coworker_id,
                opengrok_policy::Action::RunTool(&tool_name),
                &self.policy,
            );
            // A DELETE ALWAYS ASKS (#316), on the policy's card, in words naming the routine as
            // stored: an allow becomes the ask, and a grant's own ask says which routine. So does
            // a plugin tool that cannot be undone or reaches another Bot (#359).
            let delete = routine
                .as_ref()
                .and_then(|(_, (_, card))| card.clone())
                .or_else(|| plugin_call.as_ref().and_then(|(_, (_, card))| card.clone()))
                .or_else(|| {
                    computer_call
                        .as_ref()
                        .and_then(|(_, (_, card))| card.clone())
                });
            // The person's Always allow for such a tool (`+<tool>` in the ceiling, #359) lifts
            // that card; a grant's own ask still stands.
            let delete = delete.filter(|_| !self.asks_unasked(&tool_name));
            if decision.needs_approval() || (decision.is_allowed() && delete.is_some()) {
                let why = decision.reason().unwrap_or("a human yes").to_string();
                Gate::Ask(AwaitingReason::PolicyApproval, delete.unwrap_or(why))
            } else if let Some(reason) = decision.reason() {
                Gate::Deny(reason.to_string())
            } else {
                Gate::Allow
            }
        };

        // The browser tools are withheld when the network is off, so one arriving here is a
        // model that was offered them on an earlier turn. Refused in words BEFORE the two
        // hand-off cards below: a login card raised for a box that cannot browse would have
        // the person type a password that is then thrown away.
        if self.network_off() && BROWSER_TOOLS.contains(&tool_name.as_str()) {
            // A collect card and a plugin's form never touch the box. A login card on a
            // machine that cannot browse would take a password and throw it away.
            let collect = tool_name == REQUEST_USER_FORM
                && (user_form::is_chat_collection(&call.arguments)
                    || user_form::form_mode::is_form_mode_source(&call.arguments));
            if !collect {
                return ToolResult::refused(&call.id, NETWORK_OFF);
            }
        }

        // HITL wait, not approve-then-run — and BEFORE the judge. A form is not a tool that
        // then executes, so auto-review must not steal the card; a `computer` type while a
        // form is open must not send the secret to another model. Policy Deny still refuses.
        if tool_name == REQUEST_USER_FORM {
            if let Gate::Deny(why) = &gate {
                return ToolResult::refused(&call.id, why.as_str());
            }
            // NOT A PARK. A form-mode card is answered by the person's presses, each of which
            // runs on its own, so the turn ends at once and the server mints the card from the
            // plugin's card tool. It is not on the box either, so no screen hold stops it.
            if user_form::form_mode::is_form_mode_source(&call.arguments) {
                return match user_form::form_mode::open_answer(&call.arguments, context.form_mode) {
                    Ok(said) => ToolResult::ok(&call.id, said),
                    Err(why) => ToolResult::refused(&call.id, why),
                };
            }
            if context.screen_hold {
                return ToolResult::refused(
                    &call.id,
                    format!(
                        "a form or computer handoff is already open {}; wait for the person to \
                         finish it",
                        context.held_where()
                    ),
                );
            }
            if let Err(why) = user_form::validate_field_ids(&call.arguments) {
                return ToolResult::refused(&call.id, why);
            }
            return ToolResult::awaiting(&call.id, AwaitingReason::UserForm, "Waiting for you");
        }

        if context.screen_hold && matches!(tool_name.as_str(), "computer" | "open_url" | RUN_RECIPE)
        {
            return ToolResult::refused(
                &call.id,
                format!(
                    "a form or computer handoff is open {}; do not type, click, or open pages \
                     until the person has finished. Secrets must not be typed with `computer`",
                    context.held_where()
                ),
            );
        }

        // Prod traffic reroute: host wants the tunnel AND the box reports ready (laptop
        // client attached). Docker host-network is not that. With no standing auto-review
        // allow, leave-box tools raise the Review-an-action card. A primary-gate Ask
        // subsumes this (one card).
        let asks_the_tunnel = needs_egress_consent(&tool_name, &arguments);
        let review_inactive = self
            .auto_review
            .as_ref()
            .is_none_or(|review| !review.policy.is_active());
        // Consent to leave through the tunnel is given once per run, not once per click: "visit
        // facebook" used to cost a card for the page, another for the screenshot, another for the
        // click (21 Sep 2026). The resume paths set it from the answered card's own tool.
        let egress_consented = self.egress_consented();
        if self.egress_tunnel == EgressTunnelMode::On
            && asks_the_tunnel
            && !review_approved
            && !egress_consented
            && review_inactive
        {
            match &gate {
                Gate::Deny(why) => return ToolResult::refused(&call.id, why.clone()),
                Gate::Ask(_, _) => {}
                Gate::Allow if !gate_approved => {
                    return ToolResult::awaiting(
                        &call.id,
                        AwaitingReason::AutoReview,
                        review::EGRESS_TUNNEL_ASK_REASON,
                    );
                }
                Gate::Allow => {}
            }
        }

        // ONE judge call site, for every tool.
        let review = match (&gate, review_approved, self.auto_review.as_ref()) {
            (Gate::Deny(_), _, _) | (_, true, _) | (_, _, None) => None,
            (_, false, Some(review)) if !review.policy.is_active() => None,
            (_, false, Some(review)) => Some(
                review
                    .judge(&tool_name, &call.id, &arguments, &self.judge_failures)
                    .await,
            ),
        };

        match combine(gate, review, gate_approved) {
            Outcome::Refuse(why) => return ToolResult::refused(&call.id, why),
            // The machine's own ask is answered by the sink: `run` re-judges, writes the audit
            // row every other verdict gets, and replies NeedsApproval without dispatching. Going
            // through it keeps "one audit row per command that touched the channel" true.
            Outcome::Ask(AwaitingReason::ExecConsent, _) if user_machine_command.is_some() => {}
            Outcome::Ask(reason, why) => return ToolResult::awaiting(&call.id, reason, why),
            Outcome::Run => {}
        }

        if let Some(command) = user_machine_command.as_deref() {
            let Some(sink) = self.user_machine.as_ref() else {
                return ToolResult::refused(
                    &call.id,
                    "no machine of yours is connected, so nothing can run there",
                );
            };
            // The sink judges again as it runs (the gate is the single choke point on the way to
            // a machine). If the policy changed underneath us and it now asks, honour that — a
            // review approval must never stand in for the machine owner's consent.
            return match sink
                .run(&context.account_id, command, &call.id, gate_approved)
                .await
            {
                UserMachineReply::Ran(text) => ToolResult::ok(&call.id, text),
                UserMachineReply::Refused(why) => ToolResult::refused(&call.id, why),
                UserMachineReply::NeedsApproval => ToolResult::awaiting(
                    &call.id,
                    AwaitingReason::ExecConsent,
                    "your machine's owner must approve this command",
                ),
            };
        }
        // The server's own desk, as the context's account: no box is resolved or woken.
        if let Some((desk, (ask, _))) = routine {
            return routine::run(desk.as_ref(), context, &call.id, ask).await;
        }
        if let Some((desk, (ask, _))) = plugin_call {
            return plugin_desk::run(desk.as_ref(), context, &call.id, ask).await;
        }
        if let Some((desk, (ask, _))) = computer_call {
            return computer_desk::run(desk.as_ref(), context, &call.id, ask).await;
        }
        // `office_close` only ends a session row — no box bytes — so it answers with the
        // server's own desks, and a sleeping box can never keep a document stuck open.
        if let Some((desk, office_desk::Ask::Close { document })) = &office_call {
            return office_desk::run_close(desk.as_ref(), context, &call.id, document).await;
        }

        // `machine: "group"` aims the call at the room's shared computer; anything else is the
        // coworker's own box. The model chooses this one, so it is read from the arguments
        // rather than stamped by `overwrite_identity`.
        let on_group = arguments.get("machine").and_then(Value::as_str) == Some("group");
        // A room's computer has one screen, every member's; the Bot's own computer is on the
        // screen the Bot uses (#376).
        let screen = if on_group {
            Screen::Shared
        } else {
            context.screen()
        };
        let box_id = if on_group {
            match context.group_box.as_ref() {
                Some(group) => &group.box_id,
                None => {
                    return ToolResult::refused(
                        &call.id,
                        "this conversation has no shared group computer; leave `machine` out to use your own",
                    );
                }
            }
        } else {
            let Some(box_id) = context.box_id.as_ref() else {
                // A plugin's tool is the person's account at a service, not a program on a box
                // (#359): a Bot with no computer still runs it, and a plugin's form card (#220)
                // is nothing but such calls. Its gates have all been passed above.
                if !needs_the_box(&tool_name) {
                    return self.call_plugin_tool(&call.id, &tool_name, arguments).await;
                }
                return ToolResult::refused(
                    &call.id,
                    "this coworker has no computer yet, so nothing can be run",
                );
            };
            box_id
        };

        // The box is brought up here, by the first tool that needs it, not before the model was
        // asked: a turn that never touches the box never waits for it.
        if needs_the_box(&tool_name)
            && let Err(down) = self.ensure_awake(box_id.as_str()).await
        {
            return ToolResult::refused(&call.id, down);
        }
        // The box was asleep when the turn started, so the guest could not say whether the
        // tunnel is really there. It is awake now: ask it, once, and raise the card only if it
        // is — or, under a standing `never`, refuse in words: the tools were offered because
        // nobody could know, and the person's answer to a tunnel that IS there is no.
        if self.egress_tunnel == EgressTunnelMode::AskTheBoxAfterWake
            && BROWSER_TOOLS.contains(&tool_name.as_str())
            && self.egress_policy == EgressPolicy::Never
            && self.tunnel_is_there(box_id.as_str()).await
        {
            return ToolResult::refused(&call.id, NETWORK_OFF);
        }
        if self.egress_tunnel == EgressTunnelMode::AskTheBoxAfterWake
            && asks_the_tunnel
            && !review_approved
            && !egress_consented
            && review_inactive
            && !gate_approved
            && self.tunnel_is_there(box_id.as_str()).await
        {
            return ToolResult::awaiting(
                &call.id,
                AwaitingReason::AutoReview,
                review::EGRESS_TUNNEL_ASK_REASON,
            );
        }

        // Every other office verb runs here, box awake: reads, edits and exports are bytes on
        // it. The session row — not a live object — is what `document` names, so a proposal
        // staged last turn is still what `office_accept` is offered now.
        if let Some((desk, ask)) = office_call {
            return office_desk::run(
                desk.as_ref(),
                context,
                self.computer.as_ref(),
                box_id.as_str(),
                &call.id,
                ask,
            )
            .await;
        }

        match tool_name.as_str() {
            RUN_RECIPE => match serde_json::from_value::<RunRecipeArgs>(arguments) {
                Ok(args) => {
                    let values = args.values.unwrap_or_default();
                    self.run_recipe(box_id, &screen, context, &call.id, &args.recipe, &values)
                        .await
                }
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "shell" => match serde_json::from_value::<ShellArgs>(arguments) {
                Ok(args) => self.shell(box_id, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "read_file" => match serde_json::from_value::<ReadFileArgs>(arguments) {
                Ok(args) => match self.computer.read_file(box_id.as_str(), &args.path).await {
                    Ok(content) => ToolResult::ok(&call.id, content),
                    Err(error) => ToolResult::refused(&call.id, describe(error)),
                },
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "write_file" => match serde_json::from_value::<WriteFileArgs>(arguments) {
                Ok(args) => match self
                    .computer
                    .write_file(box_id.as_str(), &args.path, &args.content)
                    .await
                {
                    Ok(()) => ToolResult::ok(&call.id, format!("wrote {}", args.path)),
                    Err(error) => ToolResult::refused(&call.id, describe(error)),
                },
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "open_url" => match serde_json::from_value::<OpenUrlArgs>(arguments) {
                Ok(args) => self.open_url(box_id, &screen, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            "computer" => match serde_json::from_value::<ComputerArgs>(arguments) {
                Ok(args) => self.computer_use(box_id, &screen, &call.id, args).await,
                Err(error) => ToolResult::refused(&call.id, format!("bad arguments: {error}")),
            },
            // A plugin's tool. Reached only AFTER the policy check above, so a connector is
            // governed exactly like `shell` — the grant, the ceiling and the approval all apply
            // before a single byte leaves this process.
            other => self.call_plugin_tool(&call.id, other, arguments).await,
        }
    }

    /// A taught recipe, run as one box call. The receipt's end screenshot rides the result so
    /// the model sees where the screen ended up; a stopped run is a refusal with the step.
    ///
    /// The run also asks the box to say what it saw while it played, and that reading goes into
    /// the result's words. It is the only channel there is: a `ToolResult` carries text and one
    /// image, so a structured observation the model never sees would be a fact recorded for
    /// nobody. `crate::observe` has the level, what it costs, and why none of it is a verdict.
    async fn run_recipe(
        &self,
        box_id: &BoxId,
        screen: &Screen,
        context: &ToolContext,
        call_id: &str,
        recipe_id: &str,
        values: &opengrok_recipes::Values,
    ) -> ToolResult {
        let Some(offer) = self.recipes.iter().find(|recipe| recipe.id == recipe_id) else {
            return ToolResult::refused(
                call_id,
                format!("no recipe `{recipe_id}` is granted to this coworker"),
            );
        };
        // What the person typed beats what the model inferred. The model may have read a value
        // off the sentence, or invented one; a value typed into a named field is the only one
        // that was actually stated. Anything the person left blank, the model may still fill.
        let mut values = values.clone();
        if let Some((chosen, typed)) = &self.chosen_recipe
            && chosen == recipe_id
        {
            for (name, value) in typed {
                values.insert(name.clone(), value.clone());
            }
        }
        let values = &values;

        // Bind parameter values against the recipe's declaration. A refusal from binding is a
        // refusal in words the model can act on.
        if let Err(why) = opengrok_recipes::bind(&offer.parameters, values) {
            return ToolResult::refused(call_id, why);
        }
        let Some(source) = self.recipe_source.as_ref() else {
            return ToolResult::refused(call_id, "recipes are not available on this server");
        };
        if let Err(why) = source.still_granted(recipe_id, &context.coworker_id).await {
            return ToolResult::refused(call_id, why);
        }
        let (version, mut request) = match source.recipe_request(recipe_id, values).await {
            Ok(found) => found,
            Err(why) => return ToolResult::refused(call_id, why),
        };
        // The bot's screen is taken the way the page's Run button takes it (#227), before the box
        // is touched: a chat play on top of a page run was two recipes clicking on one screen.
        let claim = match source
            .claim_run(recipe_id, version, &context.coworker_id)
            .await
        {
            Ok(claim) => claim,
            Err(why) => return ToolResult::refused(call_id, why),
        };
        let claimed = claim.as_ref().map(|claim| claim.id.as_str());
        // ASK THE BOX WHAT IT SAW. A receipt on its own answers "did any step throw", and a model
        // that gets `ok` back for the twenty-fifth identical replay has been told the truth and
        // learnt nothing. The observation is what lets it tell a run that landed where the tape
        // was taped from one that did not. See `crate::observe` for the level and its cost.
        crate::observe::ask(&mut request, self.observe);
        let receipt = match self
            .computer
            .run_recipe(box_id.as_str(), screen, &request)
            .await
        {
            Ok(raw) => RecipeReceipt::from_value(raw),
            // The box may have played some of it before the connection went: a replay would
            // type on top of it. Counted as played, and the model is told to look first.
            Err(error @ BoxError::Interrupted(_)) => {
                return ToolResult::refused(
                    call_id,
                    format!(
                        "{}. The recipe may have played part way: look at the screen before \
                         doing anything else, and do not run it again. This bot's screen stays \
                         reserved for up to a minute while the interrupted run's claim runs out, \
                         so another recipe asked for in that time is refused",
                        describe(error)
                    ),
                )
                .part_way();
            }
            // A claimed row is ended as a failed run that says why, as the page does; left to its
            // lease it would read "interrupted", which is not what happened. An interrupted play
            // (above) is left to lapse, because part way is all that is known of it; the lapse
            // holds the bot for up to RUN_LEASE_MS, and the model is told so.
            Err(error) => {
                let why = describe(error);
                if claimed.is_some() {
                    let failed = RecipeReceipt::from_value(
                        serde_json::json!({ "ok": false, "ran": 0, "error": why }),
                    );
                    let _ = source
                        .record_run(recipe_id, version, &context.coworker_id, &failed, claimed)
                        .await;
                }
                return ToolResult::refused(call_id, why);
            }
        };
        let _ = source
            .record_run(recipe_id, version, &context.coworker_id, &receipt, claimed)
            .await;
        let mut said = if receipt.ok {
            format!(
                "ran recipe \"{}\" (v{version}): {} steps; screenshot of the screen afterwards attached",
                offer.name, receipt.ran
            )
        } else {
            format!(
                "recipe \"{}\" (v{version}) stopped at step {}: {}",
                offer.name,
                receipt.stopped_at.unwrap_or(receipt.ran),
                receipt.error.as_deref().unwrap_or("a step failed")
            )
        };
        // APPENDED, NOT WOVEN IN. A receipt that carries no observation — an old box, or a
        // deployment that asked for none — leaves these words exactly as a model has been reading
        // them since before any of this existed. It goes on the stopped result too: where a tape
        // stopped is most of the story, and what was under it is the rest.
        if let Some(seen) = crate::observe::Seen::read(&receipt.raw) {
            said.push_str(". ");
            said.push_str(&seen.sentence());
        }
        let mut result = if receipt.ok {
            ToolResult::ok(call_id, said)
        } else {
            ToolResult::refused(call_id, said).part_way()
        };
        if let Some(image) = receipt.image.clone() {
            result = result.with_image(image);
        }
        result
    }

    async fn open_url(
        &self,
        box_id: &BoxId,
        screen: &Screen,
        call_id: &str,
        args: OpenUrlArgs,
    ) -> ToolResult {
        if !self.has_screen() {
            return ToolResult::refused(call_id, "this computer has no screen");
        }
        match self
            .computer
            .open_url(box_id.as_str(), screen, &args.url)
            .await
        {
            Ok(()) => ToolResult::ok(
                call_id,
                format!(
                    "opened {} in the browser on your box; take a screenshot with `computer` to see it",
                    args.url
                ),
            ),
            Err(error) => ToolResult::refused(call_id, describe(error)),
        }
    }

    /// Act, then look: every action answers with a fresh screenshot, so the model sees what it
    /// did without a second call. A plain `screenshot` just looks.
    async fn computer_use(
        &self,
        box_id: &BoxId,
        screen: &Screen,
        call_id: &str,
        args: ComputerArgs,
    ) -> ToolResult {
        if !self.has_screen() {
            return ToolResult::refused(call_id, "this computer has no screen");
        }
        let action = match args.into_action() {
            Ok(action) => action,
            Err(why) => return ToolResult::refused(call_id, format!("bad arguments: {why}")),
        };
        let mut said = String::new();
        if let Some(action) = &action {
            if let Err(error) = self.computer.act(box_id.as_str(), screen, action).await {
                return ToolResult::refused(call_id, describe(error));
            }
            said = format!("{}; ", action.describe());
            // The display needs a moment to repaint after input before it is worth looking.
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        }
        match self.computer.screenshot(box_id.as_str(), screen).await {
            Ok(shot) => ToolResult::ok(
                call_id,
                format!(
                    "{said}screenshot of the {}x{} screen attached",
                    shot.width, shot.height
                ),
            )
            .with_image(ToolImage::from(shot)),
            Err(error) if action.is_some() => ToolResult::ok(
                call_id,
                format!(
                    "{said}but the screen could not be captured: {}",
                    describe(error)
                ),
            ),
            Err(error) => ToolResult::refused(call_id, describe(error)),
        }
    }

    /// Route a qualified name to the session that owns it.
    async fn call_plugin_tool(
        &self,
        call_id: &str,
        name: &str,
        arguments: serde_json::Value,
    ) -> ToolResult {
        let name = self.internal_tool_name(name);
        let Some((plugin, server, remote)) = crate::mcp::split_qualified(&name) else {
            // A server that did not answer offered no tools this turn, so the wire name of one the
            // model remembers from an earlier turn resolves to nothing. Refused with the server's
            // reason, not as a tool that never existed.
            let down = self.unavailable_plugins.iter().find(|(key, _)| {
                name.starts_with(&format!("{}_", crate::mcp::openai_safe_tool_name(key)))
            });
            return ToolResult::refused(
                call_id,
                match down {
                    Some((_, reason)) => format!("{name} cannot run now: {}", clip_reason(reason)),
                    None => format!("there is no tool called {name}"),
                },
            );
        };

        let key = format!("{plugin}.{server}");
        let Some(session) = self.sessions.get(&key) else {
            // Named precisely: "no such tool" and "that plugin is not connected right now" send a
            // person to different places.
            let why = self
                .unavailable_plugins
                .get(&key)
                .map(|reason| format!(" ({})", clip_reason(reason)))
                .unwrap_or_default();
            return ToolResult::refused(
                call_id,
                format!("{plugin} is not connected on this run{why}, so {name} cannot run"),
            );
        };

        // The identity-overwritten arguments go out, minus the keys that are ours rather than the
        // tool's — a remote server rejecting an unexpected `coworker_id` would fail the call for a
        // reason the model cannot act on.
        let arguments = strip_identity(arguments);

        match session.call(&remote, arguments).await {
            Ok(content) => ToolResult::ok(call_id, content),
            Err(error) => ToolResult::refused(call_id, error.to_string()),
        }
    }

    async fn shell(&self, box_id: &BoxId, call_id: &str, args: ShellArgs) -> ToolResult {
        match self
            .computer
            .run(box_id.as_str(), &args.command, args.timeout_seconds)
            .await
        {
            Ok(output) => {
                let mut content = String::new();
                if !output.stdout.is_empty() {
                    content.push_str(&output.stdout);
                }
                if !output.stderr.is_empty() {
                    content.push_str("\n[stderr]\n");
                    content.push_str(&output.stderr);
                }
                // Truncation is stated, never implied. A coworker reasoning over a silently
                // clipped log reaches confident wrong conclusions.
                if output.stdout_truncated || output.stderr_truncated {
                    content.push_str("\n[output was truncated by the box]");
                }
                if output.timed_out {
                    content.push_str(&format!(
                        "\n[timed out after {}s — it may still be running]",
                        args.timeout_seconds
                    ));
                }
                // A non-zero exit is a *result*, not a refusal: the command ran, and the model
                // needs to see what it said in order to fix it.
                if output.exit_code != 0 {
                    content.push_str(&format!("\n[exit code {}]", output.exit_code));
                }
                ToolResult::ok(call_id, content)
            }
            Err(error) => ToolResult::refused(call_id, describe(error)),
        }
    }
}
