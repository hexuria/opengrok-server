//! The Executor: what runs a tool call on the caller's computer, and the catalogue of
//! tools it offers.

use super::*;

mod execute;

/// Runs tool calls on the caller's own computer, if policy allows.
pub struct Executor {
    computer: Arc<dyn Computer>,
    /// How long the first box-bound call of a turn waits for a sleeping box to come up. A turn
    /// no longer waits before the model is asked; it waits here, once, when a tool needs the box.
    wake_patience: std::time::Duration,
    /// What this turn learnt about each box it needed: `Ok` once found running (or woken), `Err`
    /// with the sentence to answer once it would not come up. Asked once per box per turn, not
    /// once per call — a box that is down costs one wait, not one per call.
    woken: std::sync::Mutex<std::collections::BTreeMap<String, Result<(), String>>>,
    /// Told the box id after this executor woke a box, so the server can stamp it as in use.
    on_woken: Option<OnWoken>,
    /// What this principal may make this coworker do. Consulted before EVERY call, never once at
    /// the start: a grant revoked mid-conversation must stop the next tool, not the next session
    /// (CLAUDE.md #6).
    policy: opengrok_policy::Context,
    /// Calls a person has already said yes to.
    ///
    /// PER CALL, NEVER PER TOOL. Approving `shell` once must not approve every later `shell`: the
    /// person approved *that command*, and a set of call ids is the only shape that says so. A
    /// resumed run carries exactly the id that was answered, so nothing else slips through with it.
    approved_calls: std::collections::BTreeSet<String>,
    /// Calls a person answered on an AUTO-REVIEW card. Kept apart from `approved_calls` on
    /// purpose: a review approval skips the judge and nothing else. It must never release the
    /// machine's own consent gate — if the machine's mode flipped to `ask` between the card and
    /// the answer, the owner still gets their own card.
    review_approved_calls: std::collections::BTreeSet<String>,
    /// Live sessions with the MCP servers this coworker's plugins bring, keyed by
    /// `<plugin>.<server>`.
    ///
    /// Connected once per request rather than per call — and pooled across requests by
    /// `mcp::Pool` — so a turn that reaches for three tools on one server hand-shakes at most once.
    sessions: BTreeMap<String, Arc<crate::mcp::Session>>,
    /// Every plugin tool on offer, in the order a model is told about them.
    plugin_tools: Vec<crate::mcp::McpTool>,
    /// Plugin servers that should have been reached this request and were not, with the reason.
    unavailable_plugins: BTreeMap<String, String>,
    /// The reverse-exec bridge, present ONLY when this account has an enrolled, enabled machine.
    /// Its presence, with a ceiling that allows it, is what advertises `user_machine_shell` — the
    /// tool exists iff a machine can actually be reached.
    user_machine: Option<Arc<dyn UserMachineSink>>,
    /// Auto-review, when the run's effective policy is on: the instruction texts (resolved once
    /// per run by the server) and the judge that reads them. `None` is the cheapest short-circuit.
    auto_review: Option<AutoReview>,
    /// The judge's failures in a row in this run. Seeded by the server from the run's journal
    /// on a resume: every failure parks the run on a card and the executor is rebuilt, so a
    /// count that began at zero here would never pass one.
    pub(crate) judge_failures: std::sync::atomic::AtomicU32,
    /// The box has a display. Only then are `open_url` and `computer` offered: a headless box
    /// would refuse every call, and a tool that always refuses is a dead end the model retries.
    screen: bool,
    /// The name of the group whose shared computer this turn may use, when there is one; it
    /// puts `machine` on the box tools' schemas.
    group_box_name: Option<String>,
    /// The taught recipes this bot may run; `run_recipe` is offered only when there are any.
    recipes: Vec<RecipeOffer>,
    recipe_source: Option<Arc<dyn RecipeSource>>,
    /// The recipe the PERSON chose in the composer this turn, and the values they typed for it.
    /// Kept apart from whatever the model passes, because when the two disagree the person wins.
    chosen_recipe: Option<(String, opengrok_recipes::Values)>,
    /// How much of the desktop a recipe run asks the box to report back. Held here rather than
    /// read at the call, so the level a turn runs at is decided once when the turn is built and
    /// cannot change between two calls of the same conversation.
    observe: crate::observe::Observe,
    /// Prod user-network path: the box agent's egress tunnel. Docker host-network is not this.
    /// When on, leave-box screen tools raise the Review-an-action card unless a standing
    /// auto-review allow is already attached.
    egress_tunnel: EgressTunnelMode,
    /// Boxes whose guest, asked once a tool woke it, said the tunnel is there (the
    /// `AskTheBoxAfterWake` mode). Only a yes is kept: right after a wake the guest is usually
    /// not answering yet, and a remembered "no" would skip the consent card for the whole run.
    egress_after_wake: std::sync::Mutex<std::collections::BTreeSet<String>>,
    /// The person already said yes, in this run, to a leave-box action — the tunnel's card, or a
    /// judge's card on the same kind of action — so the tunnel is not asked about again.
    egress_consented: bool,
    /// The person's standing answer for this computer. `Always` is consent given in advance;
    /// `Never`, with the tunnel on, takes the leave-box tools off the offer (see `has_screen`).
    egress_policy: EgressPolicy,
    /// The policy could not be read this turn and `Never` is the fail-closed stand-in, not the
    /// person's word: the prompt must not say they chose it.
    egress_policy_unconfirmed: bool,
    /// The person's routines (#316), and whether this run may only list them: a run a routine
    /// started may not make one, or a routine could breed routines. `None` offers none.
    routines: Option<(Arc<dyn routine::RoutineDesk>, bool)>,
    /// The person's plugins and accounts (#359), through the desk the Plugins routes use. `None`
    /// offers none.
    plugin_desk: Option<Arc<dyn plugin_desk::PluginDesk>>,
    /// This Bot's own computer (7 Oct 2026), through the desk the Computer pane's routes use.
    /// `None` offers none.
    computer_desk: Option<Arc<dyn computer_desk::ComputerDesk>>,
    /// The office documents on it, through the desk `doc_session` rows and box bytes answer.
    /// `None` offers none.
    office_desk: Option<Arc<dyn office_desk::OfficeDesk>>,
    /// A switched-on plugin's tools the person switched off for this Bot (`with_switched_off`).
    switched_off: Vec<crate::mcp::McpTool>,
    /// A Bot with no computer that still has plugin tools (`without_a_computer`): no built-in is
    /// offered, and one called anyway is refused in words rather than sent to a box that is not
    /// there.
    boxless: bool,
    /// On a Bot with no computer: offer `request_user_form` for a plugin's form only (#220),
    /// which never touches a box. Set when the turn's app draws the card and a plugin has one.
    form_cards: bool,
}

/// The box a Bot with no computer is given so its plugin tools can run: every box call is
/// refused, and none is ever offered (`Executor::without_a_computer`).
struct NoComputer;

#[async_trait::async_trait]
impl Computer for NoComputer {
    async fn create(&self, _ttl: Option<u64>) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn run(
        &self,
        _box_id: &str,
        _command: &str,
        _timeout: u32,
    ) -> opengrok_box::BoxResult<opengrok_box::CommandOutput> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn start(
        &self,
        _box_id: &str,
        _command: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(
        &self,
        _box_id: &str,
        _process: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn write_file(
        &self,
        _box_id: &str,
        _path: &str,
        _content: &str,
    ) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn expose_port(
        &self,
        _box_id: &str,
        _port: u16,
        _title: &str,
    ) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn stop(&self, _box_id: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn resume(&self, _box_id: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn destroy(&self, _box_id: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn state(&self, _box_id: &str) -> opengrok_box::BoxResult<String> {
        Ok("absent".to_string())
    }
}

/// Why a call whose arguments are not a JSON object (malformed JSON, an array, a number) is refused.
pub const NOT_AN_OBJECT: &str = "your arguments were not a JSON object (malformed JSON, or not \
     an object); call the tool again with one JSON object of its parameters";

/// What a Bot with no computer is told when it calls a computer tool anyway.
pub const NO_COMPUTER: &str =
    "This Bot has no computer, so it cannot run computer tools. Its plugins' tools still work.";

/// The built-ins that need a display.
const SCREEN_TOOLS: &[&str] = &["open_url", "computer"];
/// The built-ins that exist to work a web page in the box's browser: the screen tools, and the
/// one that hands a page's login to the person. Withheld together when the computer's use of the
/// person's network is switched off, so none of them is an advertised dead end.
pub(crate) const BROWSER_TOOLS: &[&str] = &["open_url", "computer", RUN_RECIPE, REQUEST_USER_FORM];
/// The recipe tool's name; offered next to the screen tools, gated the same way.
pub const RUN_RECIPE: &str = "run_recipe";
/// What `run_recipe` does; its schema lists the recipes this bot may run after these words.
const RUN_RECIPE_DESCRIPTION: &str = "Run a task a person taught on THIS BOT'S OWN computer, \
     as one step, instead of looking and clicking your way through it. Use one when the request \
     matches its description, and say which you used.";

/// The auto-review pair a run carries.
struct AutoReview {
    policy: ReviewPolicy,
    judge: Arc<dyn ReviewJudge>,
}

impl AutoReview {
    /// One question, one word back, rendered into what the ladder needs. The judge sees the
    /// arguments AFTER identity overwrite and redaction, never as the model wrote them.
    ///
    /// `failures` is the run's count of judge failures in a row. Once it reaches
    /// `JUDGE_DOWN_AFTER` the judge is not asked again this run: the call is refused in words,
    /// which the model passes on once, instead of one more card and one more billed attempt.
    async fn judge(
        &self,
        tool: &str,
        call_id: &str,
        arguments: &Value,
        failures: &std::sync::atomic::AtomicU32,
    ) -> ReviewOutcome {
        use std::sync::atomic::Ordering;
        let streak = failures.load(Ordering::Relaxed);
        if streak >= review::JUDGE_DOWN_AFTER {
            tracing::warn!(
                call = call_id,
                tool,
                failures = streak,
                "auto-review judge is down for this run; refusing the call without asking it"
            );
            return ReviewOutcome::Block(review::judge_down_reason(streak));
        }
        let redacted = redact_arguments(arguments);
        let verdict = self
            .judge
            .judge(ReviewAsk {
                call_id,
                tool,
                arguments: &redacted,
                allow_instructions: &self.policy.allow_instructions,
                ask_instructions: &self.policy.ask_instructions,
                block_instructions: &self.policy.block_instructions,
            })
            .await;
        if matches!(verdict, ReviewVerdict::Unavailable(_)) {
            failures.fetch_add(1, Ordering::Relaxed);
        } else {
            failures.store(0, Ordering::Relaxed);
        }
        let (ask, block) = (
            &self.policy.ask_instructions,
            &self.policy.block_instructions,
        );
        match verdict {
            ReviewVerdict::Allow => ReviewOutcome::Allow,
            // A refusal names a rule the person wrote. A "block" or "ask" with no such list
            // behind it is the judge's guess, and a guess asks; it never refuses.
            ReviewVerdict::Block if !block.trim().is_empty() => {
                ReviewOutcome::Block(review::block_refusal(block))
            }
            ReviewVerdict::Ask if !ask.trim().is_empty() => {
                ReviewOutcome::Ask(ask_first_reason(ask))
            }
            ReviewVerdict::Block | ReviewVerdict::Ask | ReviewVerdict::Unsure => {
                ReviewOutcome::Ask(review::REVIEW_ASK_REASON.to_string())
            }
            ReviewVerdict::Unavailable(cause) => {
                ReviewOutcome::Ask(review::unavailable_reason(cause))
            }
        }
    }
}

impl Executor {
    /// An executor that allows nothing. The default is deliberately useless: an executor built
    /// without policy should refuse everything rather than quietly permit it.
    pub fn new(computer: Arc<dyn Computer>) -> Self {
        Self {
            computer,
            policy: opengrok_policy::Context::default(),
            wake_patience: DEFAULT_WAKE_PATIENCE,
            woken: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            on_woken: None,
            approved_calls: std::collections::BTreeSet::new(),
            sessions: BTreeMap::new(),
            plugin_tools: Vec::new(),
            unavailable_plugins: BTreeMap::new(),
            user_machine: None,
            auto_review: None,
            judge_failures: std::sync::atomic::AtomicU32::new(0),
            review_approved_calls: std::collections::BTreeSet::new(),
            screen: false,
            group_box_name: None,
            recipes: Vec::new(),
            recipe_source: None,
            chosen_recipe: None,
            observe: crate::observe::wanted(),
            egress_tunnel: EgressTunnelMode::Off,
            egress_after_wake: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            egress_consented: false,
            egress_policy: EgressPolicy::default(),
            egress_policy_unconfirmed: false,
            routines: None,
            plugin_desk: None,
            computer_desk: None,
            office_desk: None,
            switched_off: Vec::new(),
            boxless: false,
            form_cards: false,
        }
    }

    /// The executor a real request builds: a computer, and what this principal may do with it.
    pub fn with_policy(computer: Arc<dyn Computer>, policy: opengrok_policy::Context) -> Self {
        Self {
            policy,
            ..Self::new(computer)
        }
    }

    /// An executor for a Bot with no computer: only what is attached to it (its plugins) is
    /// offered. A plugin is the person's account at a service, not something that runs on a box,
    /// so a Bot without one still uses it (#359).
    pub fn without_a_computer(policy: opengrok_policy::Context) -> Self {
        Self {
            boxless: true,
            ..Self::with_policy(Arc::new(NoComputer), policy)
        }
    }

    /// Offer a Bot with no computer `request_user_form` for plugin forms (`form_cards`).
    #[must_use]
    pub fn with_form_cards(mut self, offered: bool) -> Self {
        self.form_cards = offered;
        self
    }

    /// Whether a box stands behind this executor.
    pub fn has_computer(&self) -> bool {
        !self.boxless
    }

    /// Offer the routine tools over `desk`, where the ceiling and the grant allow each (#316).
    #[must_use]
    pub fn with_routines(mut self, desk: Arc<dyn routine::RoutineDesk>) -> Self {
        self.routines = Some((desk, false));
        self
    }

    /// Offer the plugin tools over `desk`, where the ceiling and the grant allow each (#359).
    #[must_use]
    pub fn with_plugin_desk(mut self, desk: Arc<dyn plugin_desk::PluginDesk>) -> Self {
        self.plugin_desk = Some(desk);
        self
    }

    /// Offer the computer tools over `desk`, where the ceiling and the grant allow each.
    #[must_use]
    pub fn with_computer_desk(mut self, desk: Arc<dyn computer_desk::ComputerDesk>) -> Self {
        self.computer_desk = Some(desk);
        self
    }

    /// Offer the office tools over `desk` — only ever attached where a box is, since every
    /// verb but `office_close` reads or writes box bytes.
    #[must_use]
    pub fn with_office_desk(mut self, desk: Arc<dyn office_desk::OfficeDesk>) -> Self {
        self.office_desk = Some(desk);
        self
    }

    /// The office tools this run is offered: all of them with a desk, none without.
    fn office_desk_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let take = if self.office_desk.is_some() {
            office_desk::TOOLS.len()
        } else {
            0
        };
        office_desk::TOOLS.into_iter().take(take)
    }

    /// The computer tools this run is offered: all of them with a desk, none without.
    fn computer_desk_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let take = if self.computer_desk.is_some() {
            computer_desk::TOOLS.len()
        } else {
            0
        };
        computer_desk::TOOLS.into_iter().take(take)
    }

    /// The plugin tools this run is offered: all of them with a desk, none without.
    fn plugin_desk_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let take = if self.plugin_desk.is_some() {
            plugin_desk::TOOLS.len()
        } else {
            0
        };
        plugin_desk::TOOLS.into_iter().take(take)
    }

    /// Only `list_routines`, for a run a routine started: it may not make or change one.
    #[must_use]
    pub fn with_routines_listing_only(mut self) -> Self {
        self.routines = self.routines.map(|(desk, _)| (desk, true));
        // Nor any plugin tool (#359): nobody is watching such a run to answer a card or finish a
        // sign-in, and a routine must not install or switch on what its person never chose.
        self.plugin_desk = None;
        // Nor a computer tool: nobody is watching to answer a reset's card.
        self.computer_desk = None;
        self.office_desk = None;
        self
    }

    /// The routine tools this run is offered: none without a desk, only the listing for a run a
    /// routine started.
    fn routine_tools(&self) -> impl Iterator<Item = &'static str> + '_ {
        let listing = self.routines.as_ref().map(|(_, listing)| *listing);
        let take = listing.map_or(0, |listing| if listing { 1 } else { routine::TOOLS.len() });
        routine::TOOLS.into_iter().take(take)
    }

    /// Say the box has a display, so the screen tools are offered and run.
    #[must_use]
    pub fn with_screen(mut self, screen: bool) -> Self {
        self.screen = screen;
        self
    }

    /// Whether the screen tools are on offer — the prompt must say the same thing the offering does.
    /// A box whose network use is switched off has its screen tools withheld too: every one of
    /// them leaves the box for the web, and a tool the model is told about but that always
    /// refuses is a dead end it keeps trying.
    pub fn has_screen(&self) -> bool {
        self.screen && !self.network_off()
    }

    /// The computer may not use the person's network, and the box's guest says a tunnel is
    /// attached that would carry it: the browser tools are withheld and the prompt says why.
    /// Host intent alone (`AskTheBoxAfterWake`, the box asleep) is not enough — `ask` raises
    /// no card then either; a `never` that withheld more than `ask` asks about would say the
    /// person switched off something that was not carrying anything. The after-wake check
    /// refuses in words instead, once the guest has been asked.
    pub fn network_off(&self) -> bool {
        self.egress_policy == EgressPolicy::Never && self.egress_tunnel == EgressTunnelMode::On
    }

    /// `network_off`, asked once the box is awake: under a standing `never` with the tunnel
    /// mode undecided at turn start (`AskTheBoxAfterWake`), the woken guest's word settles it.
    /// For the paths that wake a box and act on it outside `execute` (the user-form fill).
    pub async fn network_off_now(&self, box_id: &str) -> bool {
        self.network_off()
            || (self.egress_policy == EgressPolicy::Never
                && self.egress_tunnel == EgressTunnelMode::AskTheBoxAfterWake
                && self.tunnel_is_there(box_id).await)
    }

    /// The person's standing answer for this computer to the tunnel's card.
    #[must_use]
    pub fn with_egress_policy(mut self, policy: EgressPolicy) -> Self {
        self.egress_policy = policy;
        self
    }

    /// The policy is a fail-closed stand-in this turn, not something the person chose.
    #[must_use]
    pub fn with_egress_policy_unconfirmed(mut self, unconfirmed: bool) -> Self {
        self.egress_policy_unconfirmed = unconfirmed;
        self
    }

    /// The browser is withheld this turn because the policy could not be read, not because
    /// the person said no — the prompt says so in those words.
    pub fn network_unconfirmed(&self) -> bool {
        self.egress_policy_unconfirmed && self.network_off()
    }

    /// How long the first box-bound tool call of a turn waits for a sleeping box.
    #[must_use]
    pub fn with_wake_patience(mut self, patience: std::time::Duration) -> Self {
        self.wake_patience = patience;
        self
    }

    /// Whether running `call` would first have to wake the box it targets: the call is box-bound,
    /// the box is known, and it is not running yet as far as this turn has seen. The harness asks
    /// before running a round so it can say "waking the computer" on the stream.
    pub async fn box_needs_wake(&self, context: &ToolContext, call: &ToolCall) -> bool {
        let tool_name = self.internal_tool_name(&call.name);
        if self.boxless {
            return false;
        }
        if !needs_the_box(&tool_name) || !self.would_reach_the_box(context, call, &tool_name) {
            return false;
        }
        let Some(box_id) = target_box(context, &call.arguments) else {
            return false;
        };
        if self.box_outcome(box_id.as_str()).is_some() {
            return false;
        }
        match self.computer.state(box_id.as_str()).await {
            Ok(state) if state == "running" => {
                self.remember_box(box_id.as_str(), Ok(()));
                false
            }
            Ok(state) if state == "absent" => false,
            _ => true,
        }
    }

    /// The cheap half of `execute`'s admission, for the frame that says "waking": a call the
    /// policy denies or parks on a card, a screen tool while a form holds the screen, or a screen
    /// tool the egress tunnel will ask about first, never reaches the box, so nothing wakes.
    fn would_reach_the_box(&self, context: &ToolContext, call: &ToolCall, tool_name: &str) -> bool {
        // A routine is the server's (#316), and so is a plugin tool (#359): neither touches the
        // box, so nothing wakes.
        if routine::is_routine_tool(tool_name)
            || plugin_desk::is_plugin_desk_tool(tool_name)
            || computer_desk::is_computer_tool(tool_name)
        {
            return false;
        }
        let decision = opengrok_policy::decide(
            &context.account_id,
            &context.coworker_id,
            opengrok_policy::Action::RunTool(tool_name),
            &self.policy,
        );
        let gate_approved = self.approved_calls.contains(&call.id);
        let review_approved = gate_approved || self.review_approved_calls.contains(&call.id);
        // A deny is a deny, approved or not; an ask is released by the gate's own yes.
        if decision.reason().is_some() && !decision.needs_approval() {
            return false;
        }
        if decision.needs_approval() && !gate_approved {
            return false;
        }
        let screen_tool = leaves_the_box(tool_name);
        if context.screen_hold && screen_tool {
            return false;
        }
        if self.network_off() && screen_tool {
            return false;
        }
        let review_inactive = self
            .auto_review
            .as_ref()
            .is_none_or(|review| !review.policy.is_active());
        // In the after-wake mode the box is woken before the tunnel is asked about, so the frame
        // is right either way.
        if self.egress_tunnel == EgressTunnelMode::On
            && needs_egress_consent(tool_name, &call.arguments)
            && review_inactive
            && !review_approved
            && !self.egress_consented()
        {
            return false;
        }
        true
    }

    /// Consent to leave through the tunnel for this run: given on a card in this run, or given
    /// in advance for this computer (`EgressPolicy::Always`).
    fn egress_consented(&self) -> bool {
        self.egress_consented || self.egress_policy == EgressPolicy::Always
    }

    fn box_outcome(&self, box_id: &str) -> Option<Result<(), String>> {
        self.woken
            .lock()
            .ok()
            .and_then(|woken| woken.get(box_id).cloned())
    }

    fn remember_box(&self, box_id: &str, outcome: Result<(), String>) {
        if let Ok(mut woken) = self.woken.lock() {
            woken.insert(box_id.to_string(), outcome);
        }
    }

    /// The box was seen running, or woken, by the time this returns `Ok` — once per box per
    /// turn; the answer is remembered either way, so a box that is down costs one wait and every
    /// later call in the turn gets the same sentence at once. `Err` is what the model is told and
    /// relays: the computer is down (only the person can look at it), or still starting (retry).
    async fn ensure_awake(&self, box_id: &str) -> Result<(), String> {
        if let Some(outcome) = self.box_outcome(box_id) {
            return outcome;
        }
        let (outcome, woke) = self.wake_now(box_id).await;
        // "Still starting" invites a retry, so it is the one answer not remembered: the next
        // call asks the provider again and finds the box up, instead of being told the same
        // sentence at once and giving up on it.
        if !outcome
            .as_ref()
            .is_err_and(|why| why.starts_with(COMPUTER_STARTING))
        {
            self.remember_box(box_id, outcome.clone());
        }
        if woke && let Some(on_woken) = &self.on_woken {
            on_woken(box_id);
        }
        outcome
    }

    /// The outcome, and whether this call actually brought the box up (as opposed to finding it
    /// running), which is what the in-use stamp is for.
    async fn wake_now(&self, box_id: &str) -> (Result<(), String>, bool) {
        let state = match self.computer.state(box_id).await {
            Ok(state) => state,
            Err(error) => return (Err(format!("{COMPUTER_DOWN} ({error})")), false),
        };
        if state == "running" {
            return (Ok(()), false);
        }
        let reached = match self.computer.wake(box_id, self.wake_patience).await {
            Ok(reached) => reached,
            Err(error) => return (Err(format!("{COMPUTER_DOWN} ({error})")), false),
        };
        if reached == "running" {
            (Ok(()), true)
        } else if opengrok_box::is_starting(&reached) {
            (Err(format!("{COMPUTER_STARTING} (it is {reached})")), false)
        } else {
            (Err(format!("{COMPUTER_DOWN} (it is {reached})")), false)
        }
    }

    /// Bring the coworker's own box up for something that types into it outside `execute` — the
    /// user-form fill — through the same memo and in-use stamp as a tool call.
    pub async fn wake_own_box(&self, context: &ToolContext) -> Result<(), String> {
        let Some(box_id) = context.box_id.as_ref() else {
            return Err("this coworker has no computer yet".to_string());
        };
        self.ensure_awake(box_id.as_str()).await
    }

    /// Whether the (now awake) box's guest reports the egress tunnel up with a client attached.
    /// A yes is remembered for the turn; a no is asked again next time, because right after a
    /// wake the guest and the laptop client are usually still coming back.
    async fn tunnel_is_there(&self, box_id: &str) -> bool {
        if self
            .egress_after_wake
            .lock()
            .is_ok_and(|known| known.contains(box_id))
        {
            return true;
        }
        let there =
            opengrok_box::EgressTunnel::advertised(true, self.computer.egress_tunnel(box_id).await);
        if there && let Ok(mut known) = self.egress_after_wake.lock() {
            known.insert(box_id.to_string());
        }
        there
    }

    /// Called with the box id after this executor brought a box up (or found it up), so the
    /// server can stamp it as in use for the idle sweep.
    #[must_use]
    pub fn with_on_woken(mut self, on_woken: OnWoken) -> Self {
        self.on_woken = Some(on_woken);
        self
    }

    /// The box this executor talks to. Fill (`user-form`) types here outside `computer_use`.
    #[must_use]
    pub fn computer(&self) -> Arc<dyn Computer> {
        self.computer.clone()
    }

    /// How much of the desktop a recipe run asks the box to report back, when it should not be
    /// the deployment's own setting. The level is a cost paid in playback time, so it is
    /// settable rather than fixed.
    #[must_use]
    pub fn with_observe(mut self, observe: crate::observe::Observe) -> Self {
        self.observe = observe;
        self
    }

    /// What the person picked in the composer: a recipe, and the values they filled in for it.
    ///
    /// A model that is told a recipe was chosen still has to call it, and it may supply values of
    /// its own — read off the sentence, or guessed. These are neither: they were typed into named
    /// fields, so they override.
    #[must_use]
    pub fn with_chosen_recipe(
        mut self,
        recipe_id: impl Into<String>,
        values: opengrok_recipes::Values,
    ) -> Self {
        self.chosen_recipe = Some((recipe_id.into(), values));
        self
    }

    /// The recipes this bot was granted, and where their steps come from.
    #[must_use]
    pub fn with_recipes(
        mut self,
        recipes: Vec<RecipeOffer>,
        source: Arc<dyn RecipeSource>,
    ) -> Self {
        self.recipes = recipes;
        self.recipe_source = Some(source);
        self
    }

    pub fn has_recipes(&self) -> bool {
        self.has_screen() && !self.recipes.is_empty()
    }

    /// Offer `machine: "group"` on the box tools, naming the room.
    pub fn set_group_box_name(&mut self, name: &str) {
        self.group_box_name = Some(name.to_string());
    }

    /// Carry the calls a person has already answered yes to.
    #[must_use]
    pub fn with_approved(mut self, approved: impl IntoIterator<Item = String>) -> Self {
        self.approved_calls = approved.into_iter().collect();
        self
    }

    /// Carry the calls a person approved on an auto-review card: the judge is skipped for them,
    /// and NOTHING else is released (see `review_approved_calls`).
    #[must_use]
    pub fn with_review_approved(mut self, approved: impl IntoIterator<Item = String>) -> Self {
        self.review_approved_calls = approved.into_iter().collect();
        self
    }

    /// Attach the reverse-exec bridge — the server does this only when the account has an enrolled,
    /// enabled machine; `user_machine_shell` is then offered where the coworker's ceiling allows.
    #[must_use]
    pub fn with_user_machine(mut self, sink: Arc<dyn UserMachineSink>) -> Self {
        self.user_machine = Some(sink);
        self
    }

    /// Attach auto-review for this run: the effective instruction texts and the judge. The server
    /// resolves the texts once per run (`docs/AUTO-REVIEW.md` §3) and attaches only when the
    /// policy is on; an inactive policy attached here is still a no-op per call.
    #[must_use]
    pub fn with_auto_review(mut self, policy: ReviewPolicy, judge: Arc<dyn ReviewJudge>) -> Self {
        self.auto_review = Some(AutoReview { policy, judge });
        self
    }

    /// Prod traffic reroute: leave-box tools (`computer`, `open_url`, `run_recipe`) raise the
    /// Review-an-action card before they run. Docker host-network is not this path.
    #[must_use]
    pub fn with_egress_tunnel(mut self, on: bool) -> Self {
        self.egress_tunnel = if on {
            EgressTunnelMode::On
        } else {
            EgressTunnelMode::Off
        };
        self
    }

    /// See [`EgressTunnelMode`].
    #[must_use]
    pub fn with_egress_tunnel_mode(mut self, mode: EgressTunnelMode) -> Self {
        self.egress_tunnel = mode;
        self
    }

    /// The person already said yes, in this run, to a leave-box action; the tunnel is not asked
    /// about again. Set by the resume paths from the answered card's own tool, not inferred.
    #[must_use]
    pub fn with_egress_consented(mut self, consented: bool) -> Self {
        self.egress_consented = consented;
        self
    }

    /// The judge's failures in a row so far in this run, read back from its journal by the
    /// resume paths. See `judge_failures`.
    #[must_use]
    pub fn with_judge_failures(self, failures: u32) -> Self {
        self.judge_failures
            .store(failures, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Attach live MCP sessions and the tools they offer.
    ///
    /// Taken together because a tool nobody can reach is worse than a tool nobody was offered: the
    /// model would call it, wait, and be refused for a reason it cannot fix.
    #[must_use]
    pub fn with_plugin_tools(
        mut self,
        sessions: BTreeMap<String, Arc<crate::mcp::Session>>,
        tools: Vec<crate::mcp::McpTool>,
    ) -> Self {
        self.sessions = sessions;
        self.plugin_tools = tools;
        self
    }

    /// Whether the person chose Always allow for `tool`, which otherwise asks first by rule (a
    /// delete, an uninstall, a removal): `+<tool>` in this Bot's ceiling. It admits nothing by
    /// itself; the tool must still be allowed.
    pub fn asks_unasked(&self, tool: &str) -> bool {
        match self.policy.ceiling.as_ref().map(|ceiling| &ceiling.tools) {
            Some(opengrok_policy::ToolSet::Only(names)) => names.contains(&format!("+{tool}")),
            _ => false,
        }
    }

    /// The tools that ask first by rule, whatever the grant says, unless the person chose Always
    /// allow (`asks_unasked`).
    pub const ASK_BY_RULE: [&'static str; 6] = [
        routine::DELETE_ROUTINE,
        plugin_desk::UNINSTALL_PLUGIN,
        plugin_desk::REMOVE_PLUGIN_ACCOUNT,
        computer_desk::RESET_COMPUTER,
        computer_desk::UPDATE_COMPUTER,
        computer_desk::SET_NETWORK,
    ];

    /// The label a decided tool gets: an Allow still asks first on the ask-by-rule list unless
    /// the person chose Always allow for it.
    fn mode_label(&self, decision: &opengrok_policy::Decision, tool: &str) -> &'static str {
        if decision.needs_approval() {
            "ask"
        } else if !decision.is_allowed() {
            "never"
        } else if Self::ASK_BY_RULE.contains(&tool) && !self.asks_unasked(tool) {
            "ask"
        } else {
            "always"
        }
    }

    /// A tool's choice for this Bot as a person reads it: `ask` (a card first), `always`, or
    /// `never` (not offered). `tool` is the dotted name for a plugin's tool.
    pub fn mode_of(&self, account: &AccountId, coworker: &CoworkerId, tool: &str) -> &'static str {
        let decision = opengrok_policy::decide(
            account,
            coworker,
            opengrok_policy::Action::RunTool(tool),
            &self.policy,
        );
        self.mode_label(&decision, tool)
    }

    /// Whether `group`'s own switch is off for this Bot (7 Oct 2026): switched off as a whole,
    /// or none of its tools in the ceiling at all.
    pub fn group_off(&self, group: &str) -> bool {
        let Some(ceiling) = self.policy.ceiling.as_ref() else {
            return true;
        };
        ceiling.tools.group_off(group)
            || opengrok_policy::TOOL_GROUPS
                .iter()
                .find(|(g, _)| *g == group)
                .is_some_and(|(_, members)| {
                    !members.iter().any(|t| ceiling.tools.allows_alone(t))
                        && !members.iter().any(|t| ceiling.tools.names_never(t))
                })
    }

    /// A tool's choice for the Bot as its own entries say it, whatever its group's switch: what a
    /// group switched off keeps for each of its tools, and shows (7 Oct 2026).
    pub fn mode_of_alone(
        &self,
        account: &AccountId,
        coworker: &CoworkerId,
        tool: &str,
    ) -> &'static str {
        let off = opengrok_policy::group_of(tool).filter(|group| {
            self.policy
                .ceiling
                .as_ref()
                .is_some_and(|ceiling| ceiling.tools.group_off(group))
        });
        let Some(group) = off else {
            // A group left out entirely keeps no choices of its own: switching it on gives each
            // tool its default, which is what it shows meanwhile.
            if let Some(group) = opengrok_policy::group_of(tool)
                && self.group_off(group)
                && !self
                    .policy
                    .ceiling
                    .as_ref()
                    .is_some_and(|c| c.tools.names_never(tool))
            {
                return if Self::ASK_BY_RULE.contains(&tool) {
                    "ask"
                } else {
                    "always"
                };
            }
            return self.mode_of(account, coworker, tool);
        };
        // The switch is written into both layers (the ceiling and the owner's grant), so both
        // are read as if it were on.
        let mut policy = self.policy.clone();
        if let Some(ceiling) = policy.ceiling.as_mut() {
            ceiling.tools = ceiling.tools.with_group_on(group);
        }
        if let Some(grant) = policy.grant.as_mut() {
            grant.profile = grant.profile.with_group_on(group);
        }
        let decision = opengrok_policy::decide(
            account,
            coworker,
            opengrok_policy::Action::RunTool(tool),
            &policy,
        );
        self.mode_label(&decision, tool)
    }

    /// The tools of a switched-on plugin that the person switched off for this Bot (Never allow):
    /// never offered, kept so a listing can show them and they can be switched back on.
    #[must_use]
    pub fn with_switched_off(mut self, tools: Vec<crate::mcp::McpTool>) -> Self {
        self.switched_off = tools;
        self
    }

    pub fn switched_off(&self) -> &[crate::mcp::McpTool] {
        &self.switched_off
    }

    /// Attach what a dial produced: the sessions, the tools, and the servers that did not answer.
    #[must_use]
    pub fn with_plugins(mut self, dialled: crate::mcp::Dialled) -> Self {
        self.unavailable_plugins = dialled.unavailable;
        self.with_plugin_tools(dialled.sessions, dialled.tools)
    }

    /// One sentence for the system message naming the plugin servers that could not be reached
    /// this request, or nothing.
    ///
    /// SAID UP FRONT BECAUSE THEIR TOOLS ARE NOT OFFERED. Without it a person asking for GitHub
    /// while GitHub's server is down hears that the coworker has no GitHub, which sends them to
    /// the wrong place (#199).
    pub fn unavailable_plugins_line(&self) -> String {
        if self.unavailable_plugins.is_empty() {
            return String::new();
        }
        let reasons = self
            .unavailable_plugins
            .values()
            .map(|reason| clip_reason(reason))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            " Some of your connected tools are unavailable on this turn, so they are not offered: \
             {reasons}. If asked for one, say it could not be reached right now and may work on a \
             later message — not that you do not have it."
        )
    }

    /// The tools that need no plugin. `open_url` and `computer` are in the default grant but are
    /// OFFERED only when the box has a display (`with_screen`); `run_recipe` only with a display
    /// and at least one granted recipe (`with_recipes`). A grant that omits a name here denies it.
    pub fn builtin_tool_names() -> &'static [&'static str] {
        &[
            "shell",
            "read_file",
            "write_file",
            "open_url",
            "computer",
            REQUEST_USER_FORM,
            RUN_RECIPE,
        ]
    }

    /// The built-ins this executor can actually run right now.
    fn offered_builtins(&self) -> impl Iterator<Item = &'static str> + '_ {
        Self::builtin_tool_names()
            .iter()
            .copied()
            .filter(move |name| !self.boxless || (self.form_cards && *name == REQUEST_USER_FORM))
            .filter(move |name| self.screen || !SCREEN_TOOLS.contains(name))
            .filter(move |name| {
                !self.network_off() || !BROWSER_TOOLS.contains(name) || *name == REQUEST_USER_FORM
            })
            // Offered by `tool_names` / `tool_schemas` on its own terms: a screen AND a grant.
            .filter(|name| *name != RUN_RECIPE)
    }

    /// EVERY tool a model is offered on THIS request — built-ins plus whatever this coworker's
    /// plugins brought.
    ///
    /// An instance method now rather than a constant, because the answer depends on which plugins
    /// this coworker has. The invariant it protects is unchanged and load-bearing: the offered set
    /// must equal the executed set, or the model is told about a dead end and keeps trying it.
    pub fn tool_names(&self) -> Vec<String> {
        self.offered_builtins()
            .map(str::to_string)
            .chain(self.has_recipes().then(|| RUN_RECIPE.to_string()))
            .chain(
                self.reaches_the_machine()
                    .then(|| USER_MACHINE_SHELL.to_string()),
            )
            .chain(self.routine_tools().map(str::to_string))
            .chain(self.plugin_desk_tools().map(str::to_string))
            .chain(self.computer_desk_tools().map(str::to_string))
            .chain(self.office_desk_tools().map(str::to_string))
            .chain(
                self.plugin_tools
                    .iter()
                    .map(|tool| tool.qualified_name.clone()),
            )
            .collect()
    }

    /// A machine attached, and a CEILING that lets this coworker reach it — its owner's switch
    /// (#268). Not the grant's profile: each command is the local-exec policy's to allow.
    fn reaches_the_machine(&self) -> bool {
        let ceiling = self.policy.ceiling.as_ref().map(|ceiling| &ceiling.tools);
        self.user_machine.is_some() && ceiling.is_some_and(|tools| tools.allows(USER_MACHINE_SHELL))
    }

    /// Every built-in, the person's machine, `message_bot` and the routine tools too: names no
    /// plugin may take, and what a new hire's ceiling holds (#268, #314, #316). The machine and
    /// `message_bot` are not the executor's to run: the server offers each where its ceiling
    /// allows (`reaches_the_machine`, `ToolRunner::with_bots`).
    pub fn every_builtin() -> impl Iterator<Item = &'static str> {
        Self::builtin_tool_names()
            .iter()
            .copied()
            .chain([USER_MACHINE_SHELL, message_bot::MESSAGE_BOT])
            .chain(routine::TOOLS)
            .chain(plugin_desk::TOOLS)
            .chain(computer_desk::TOOLS)
            .chain(office_desk::TOOLS)
    }

    /// What a person reads about a built-in tool or tool group, beside what the model reads
    /// (`builtin_description`, written for the model, in its capitals): a short label and one
    /// sentence. Apps list these; the model is never shown them (#359).
    pub fn builtin_for_people(name: &str) -> Option<(&'static str, &'static str)> {
        Some(match name {
            "shell" => ("Shell", "Runs commands on the Bot's own computer."),
            "read_file" => ("Read file", "Reads a file on the Bot's own computer."),
            "write_file" => (
                "Write file",
                "Creates or changes a file on the Bot's own computer.",
            ),
            "open_url" => ("Open page", "Opens a web page in the Bot's own browser."),
            "computer" => ("Computer", "Looks at the Bot's screen, clicks and types."),
            REQUEST_USER_FORM => ("Ask with a form", "Asks you to fill in a form in chat."),
            RUN_RECIPE => (
                "Run recipe",
                "Runs a task you taught on the Bot's computer.",
            ),
            USER_MACHINE_SHELL => (
                "Your computer's shell",
                "Runs commands on this Mac, when you allow it.",
            ),
            message_bot::MESSAGE_BOT => {
                ("Message a Bot", "Sends a message to another of your Bots.")
            }
            routine::ROW => (
                routine::ROW_LABEL,
                "Lists, makes, edits, deletes and runs this Bot's routines.",
            ),
            routine::LIST_ROUTINES => ("List routines", "Lists this Bot's routines."),
            routine::CREATE_ROUTINE => (
                "Create routine",
                "Makes a routine that wakes this Bot on a schedule.",
            ),
            routine::UPDATE_ROUTINE => ("Update routine", "Changes one of this Bot's routines."),
            routine::DELETE_ROUTINE => (
                "Delete routine",
                "Deletes one of this Bot's routines for good.",
            ),
            routine::RUN_ROUTINE => ("Run routine", "Runs one of this Bot's routines now."),
            computer_desk::ROW => (
                computer_desk::ROW_LABEL,
                "Checks, starts, stops, restarts, resets and updates the Bot's own computer, and \
                 looks at your other Bots' screens.",
            ),
            computer_desk::COMPUTER_STATUS => (
                "Computer status",
                "Reads whether the Bot's computer is running.",
            ),
            computer_desk::START_COMPUTER => ("Start computer", "Starts the Bot's computer."),
            computer_desk::SHUTDOWN_COMPUTER => (
                "Shut down computer",
                "Shuts the Bot's computer down, keeping its files.",
            ),
            computer_desk::RESTART_COMPUTER => ("Restart computer", "Restarts the Bot's computer."),
            computer_desk::LOOK_AT_SCREEN => (
                "Look at another Bot's screen",
                "Shows the Bot a picture of another of your Bots' screens, without touching it.",
            ),
            computer_desk::RESET_COMPUTER => (
                "Reset computer",
                "Deletes everything on the Bot's computer for good.",
            ),
            computer_desk::UPDATE_COMPUTER => (
                "Update computer",
                "Rebuilds the Bot's computer on the newest image.",
            ),
            computer_desk::SET_NETWORK => (
                "Set network",
                "Sets what the Bot's computer may reach on your network.",
            ),
            office_desk::ROW => (office_desk::ROW_LABEL, office_desk::ROW_DESCRIPTION),
            office_desk::OFFICE_FILES => (
                "Browse documents",
                "Lists the .docx, .xlsx and .pptx files on the Bot's computer.",
            ),
            office_desk::OFFICE_OPEN => (
                "Open document",
                "Opens an Office file and keeps a session you can watch live.",
            ),
            office_desk::OFFICE_CREATE => (
                "Create document",
                "Makes a new .docx, .xlsx or .pptx file on the Bot's computer.",
            ),
            office_desk::OFFICE_OUTLINE => (
                "Document outline",
                "Reads a document's headings, sheets or slides.",
            ),
            office_desk::OFFICE_GREP => ("Search document", "Finds text inside an open document."),
            office_desk::OFFICE_READ => {
                ("Read document", "Reads a span of an open document's text.")
            }
            office_desk::OFFICE_CELLS => ("Read cells", "Reads a range of a spreadsheet's cells."),
            office_desk::OFFICE_RENDER => (
                "Render page",
                "Rasterizes one page, slide or sheet to an image.",
            ),
            office_desk::OFFICE_VERIFY => (
                "Verify document",
                "Saves and reopens a document to prove the file is still valid.",
            ),
            office_desk::OFFICE_CLOSE => ("Close document", "Ends an open document's session."),
            office_desk::OFFICE_PROPOSE => (
                "Propose edits",
                "Stages text edits you can review before they are written.",
            ),
            office_desk::OFFICE_PROPOSE_CELLS => (
                "Propose cell edits",
                "Stages spreadsheet edits you can review before they are written.",
            ),
            office_desk::OFFICE_REVIEW => {
                ("Review proposals", "Shows what a staged edit would change.")
            }
            office_desk::OFFICE_ACCEPT => (
                "Accept edits",
                "Writes a reviewed proposal to the document.",
            ),
            office_desk::OFFICE_REJECT => (
                "Reject edits",
                "Discards a staged proposal without writing.",
            ),
            office_desk::OFFICE_EXPORT => (
                "Export document",
                "Saves the document to a new file and attaches it to the reply.",
            ),
            plugin_desk::ROW => (
                plugin_desk::ROW_LABEL,
                "Finds, installs and removes plugins and manages their accounts.",
            ),
            plugin_desk::LIST_PLUGINS => ("List plugins", "Lists the plugins you can use."),
            plugin_desk::PLUGIN_DETAILS => ("Plugin details", "Reads one plugin in full."),
            plugin_desk::INSTALL_PLUGIN => ("Install plugin", "Installs a plugin for you."),
            plugin_desk::UNINSTALL_PLUGIN => (
                "Uninstall plugin",
                "Uninstalls one of your plugins and its accounts.",
            ),
            plugin_desk::LIST_PLUGIN_ACCOUNTS => ("List accounts", "Lists your plugin accounts."),
            plugin_desk::ADD_PLUGIN_ACCOUNT => {
                ("Add account", "Shows you a card to add a plugin account.")
            }
            plugin_desk::RENAME_PLUGIN_ACCOUNT => {
                ("Rename account", "Renames one of your plugin accounts.")
            }
            plugin_desk::REMOVE_PLUGIN_ACCOUNT => (
                "Remove account",
                "Removes one of your plugin accounts for good.",
            ),
            plugin_desk::SET_PLUGIN_FOR_BOT => (
                "Switch plugin for a Bot",
                "Switches a plugin on or off for a Bot.",
            ),
            plugin_desk::PICK_PLUGIN_ACCOUNT => (
                "Pick account for a Bot",
                "Chooses which account a Bot uses for a plugin.",
            ),
            _ => return None,
        })
    }

    /// A built-in's words as `tool_schemas` offers them, less what a turn adds (a recipe list);
    /// and the routines' row's (#316).
    pub fn builtin_description(name: &str) -> Option<&'static str> {
        if name == RUN_RECIPE {
            return Some(RUN_RECIPE_DESCRIPTION);
        }
        if name == message_bot::MESSAGE_BOT {
            return Some(message_bot::MESSAGE_BOT_DESCRIPTION);
        }
        routine::description(name)
            .or(plugin_desk::description(name))
            .or(computer_desk::description(name))
            .or(office_desk::description(name))
            .or(builtin_tool_spec(name).map(|(description, _)| description))
    }

    /// Internal dotted `qualified_name` ↔ OpenAI-safe wire name for this coworker's plugins.
    /// Sorted so two names that sanitise the same way get the same `_2` suffix
    /// regardless of plugin-list order.
    fn plugin_wire_names(&self) -> Vec<(String, String)> {
        let mut qualified: Vec<&str> = self
            .plugin_tools
            .iter()
            .map(|tool| tool.qualified_name.as_str())
            .collect();
        qualified.sort_unstable();
        crate::mcp::openai_unique_tool_names(Self::every_builtin(), qualified)
    }

    /// A plugin tool offered under `wire_name`: its dotted `<plugin>.<server>.<tool>` name, and the
    /// title its server's annotations give it, for a person to read (never for a decision).
    pub fn plugin_tool_named(&self, wire_name: &str) -> Option<(String, Option<String>)> {
        let tool = self.lookup_plugin_tool(wire_name)?;
        let title = tool
            .annotations
            .as_ref()
            .and_then(|notes| notes.get("title"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_string);
        Some((tool.qualified_name.clone(), title))
    }

    /// Accept the model's OpenAI-safe name or a legacy dotted qualify. Policy, sessions and
    /// `split_qualified` keep the dotted form. Every builtin is already a legal OpenAI name,
    /// so only a plugin tool can arrive under a name other than its own.
    fn internal_tool_name(&self, call_name: &str) -> String {
        if let Some(tool) = self.lookup_plugin_tool(call_name) {
            return tool.qualified_name.clone();
        }
        call_name.to_string()
    }

    fn lookup_plugin_tool(&self, name: &str) -> Option<&crate::mcp::McpTool> {
        self.plugin_tools
            .iter()
            .find(|tool| tool.qualified_name == name)
            .or_else(|| {
                let qualified = self
                    .plugin_wire_names()
                    .into_iter()
                    .find(|(_, wire)| wire == name)
                    .map(|(qualified, _)| qualified)?;
                self.plugin_tools
                    .iter()
                    .find(|tool| tool.qualified_name == qualified)
            })
    }

    /// The OpenAI function-calling tool definitions this coworker is OFFERED for a request: built-ins
    /// the policy permits (Allow or NeedsApproval — a `Deny` is never advertised, so the model is not
    /// told about a dead end it would keep trying) plus its plugin tools. This is the OFFERING half
    /// that must pair with `run`; without it the model is never told the tools exist and answers "I
    /// can't run commands" even with a computer attached. Empty when the coworker may run nothing.
    pub fn tool_schemas(&self, account_id: &AccountId, coworker_id: &CoworkerId) -> Vec<Value> {
        let permitted = |name: &str| {
            !matches!(
                opengrok_policy::decide(
                    account_id,
                    coworker_id,
                    opengrok_policy::Action::RunTool(name),
                    &self.policy,
                ),
                opengrok_policy::Decision::Deny(_)
            )
        };
        let mut schemas = Vec::new();
        for name in self.offered_builtins() {
            let spec = match self.boxless {
                true => user_form::form_mode::boxless_spec(),
                false => builtin_tool_spec(name),
            };
            if permitted(name)
                && let Some((description, mut parameters)) = spec
            {
                if let Some(group) = &self.group_box_name
                    && let Some(properties) = parameters
                        .get_mut("properties")
                        .and_then(Value::as_object_mut)
                {
                    properties.insert(
                        "machine".to_string(),
                        serde_json::json!({
                            "type": "string",
                            "enum": ["mine", "group"],
                            "description": format!(
                                "Which computer: \"mine\" (your own box, the default) or \"group\" (the shared computer of the {group} group, which every member can see and use)."
                            ),
                        }),
                    );
                }
                schemas.push(serde_json::json!({
                    "type": "function",
                    "function": { "name": crate::mcp::openai_safe_tool_name(name), "description": description, "parameters": parameters },
                }));
            }
        }
        // Taught recipes: one tool whose description is the list, so the model reads what each
        // one does and picks by id. Gated by the grant like the other box tools, and only where
        // there is a screen to run them on.
        if self.has_recipes() && permitted(RUN_RECIPE) {
            let listing = self
                .recipes
                .iter()
                .map(|recipe| {
                    let mut entry =
                        format!("`{}` — {}: {}", recipe.id, recipe.name, recipe.description);
                    if !recipe.parameters.is_empty() {
                        let params = recipe
                            .parameters
                            .iter()
                            .map(|p| {
                                format!(
                                    "{} ({}text)",
                                    p.name,
                                    if p.required { "required " } else { "optional " }
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        entry.push_str(&format!("; parameters: {}", params));
                    }
                    entry
                })
                .collect::<Vec<_>>()
                .join("\n");
            let ids: Vec<&str> = self
                .recipes
                .iter()
                .map(|recipe| recipe.id.as_str())
                .collect();

            // Build properties that include parameters for each recipe.
            let mut properties = serde_json::json!({
                "recipe": { "type": "string", "enum": ids, "description": "The recipe's id." }
            });

            // Add a values object property that describes the parameters of the recipes.
            let recipe_params_info = self
                .recipes
                .iter()
                .filter(|r| !r.parameters.is_empty())
                .map(|r| {
                    let param_info = r
                        .parameters
                        .iter()
                        .map(|p| {
                            format!(
                                "{}: {} ({}required)",
                                p.name,
                                format!("{:?}", p.kind).to_lowercase(),
                                if p.required { "" } else { "not " }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("`{}`: {}", r.id, param_info)
                })
                .collect::<Vec<_>>();

            if !recipe_params_info.is_empty() {
                let values_description = format!(
                    "Parameter values for recipes that require them. For recipes with parameters, pass `values` as a map of parameter names to values. Recipes with parameters:\n{}",
                    recipe_params_info.join("\n")
                );
                if let Some(props) = properties.as_object_mut() {
                    props.insert(
                        "values".to_string(),
                        serde_json::json!({
                            "type": "object",
                            "description": values_description
                        }),
                    );
                }
            }

            schemas.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": crate::mcp::openai_safe_tool_name(RUN_RECIPE),
                    "description": format!("{RUN_RECIPE_DESCRIPTION} Recipes you may run:\n{listing}"),
                    "parameters": {
                        "type": "object",
                        "properties": properties,
                        "required": ["recipe"],
                    },
                },
            }));
        }
        // The reverse-exec tool is NOT gated by the grant's profile: each command is authorized by
        // the account's local-exec policy (never/ask/bypass) and the machine's own consent, inside
        // the sink, and a profile written before the machine existed would deny every coworker a
        // capability the account enabled. Whether this coworker may reach the machine at all is
        // its ceiling's switch (#268), so it is offered when a machine is attached AND that allows.
        if self.reaches_the_machine()
            && let Some((description, parameters)) = builtin_tool_spec(USER_MACHINE_SHELL)
        {
            schemas.push(serde_json::json!({
                "type": "function",
                "function": { "name": crate::mcp::openai_safe_tool_name(USER_MACHINE_SHELL), "description": description, "parameters": parameters },
            }));
        }
        let routines = self.routine_tools().filter(|name| permitted(name));
        schemas.extend(routines.filter_map(routine::schema));
        let plugin_desk = self.plugin_desk_tools().filter(|name| permitted(name));
        schemas.extend(plugin_desk.filter_map(plugin_desk::schema));
        let computer = self.computer_desk_tools().filter(|name| permitted(name));
        schemas.extend(computer.filter_map(computer_desk::schema));
        let office = self.office_desk_tools().filter(|name| permitted(name));
        schemas.extend(office.filter_map(office_desk::schema));
        let plugin_wires = self.plugin_wire_names();
        let mut schema_budget = crate::mcp::MAX_ADVERTISED_SCHEMAS_BYTES;
        for tool in &self.plugin_tools {
            if permitted(&tool.qualified_name) {
                let wire = plugin_wires
                    .iter()
                    .find(|(qualified, _)| qualified == &tool.qualified_name)
                    .map(|(_, wire)| wire.clone())
                    .unwrap_or_else(|| crate::mcp::openai_safe_tool_name(&tool.qualified_name));
                schemas.push(serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": wire,
                        "description": tool.description.clone().unwrap_or_default(),
                        // The server's own schema, cleaned: an open object here had the model
                        // guess argument names, and every wrong guess cost a round (#196).
                        "parameters": crate::mcp::within_budget(tool.parameters(), &mut schema_budget),
                    },
                }));
            }
        }
        schemas
    }
}

/// Replace every identity-bearing argument with the session's own.
///
/// Additive on purpose: a key the model did not send is still set, so a tool cannot be reached
/// with an absent identity either. The list is small and explicit — a new identity-bearing
/// argument must be added here, and the test below is what notices when one is not.
/// The description and JSON-Schema parameters a builtin tool is advertised to the model with, or
/// `None` if the name is not a builtin. `box_id` is deliberately ABSENT from every schema — the
/// computer is not the model's to choose; `overwrite_identity` injects it server-side.
fn builtin_tool_spec(name: &str) -> Option<(&'static str, Value)> {
    // Every description names the target unambiguously: THIS BOT'S OWN sandboxed box on the server,
    // which is NOT the user's own machine. A bot that runs `write_file` has written to its box, and
    // must never describe that as touching the user's computer. (When a reverse channel to the user's
    // machine exists, "my computer" will name two real machines; this wording keeps them apart.)
    match name {
        "shell" => Some((
            "Run a shell command on THIS BOT'S OWN computer — a sandboxed box on the server, \
             not the user's own machine — and return its stdout, stderr and exit code.",
            serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The shell command to run on the bot's own box." } },
                "required": ["command"],
            }),
        )),
        "read_file" => Some((
            "Read a file from THIS BOT'S OWN computer (the sandboxed box on the server, not the \
             user's machine) and return its contents.",
            serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Absolute path on the bot's own box." } },
                "required": ["path"],
            }),
        )),
        "write_file" => Some((
            "Create or overwrite a file on THIS BOT'S OWN computer (the sandboxed box on the server, \
             not the user's machine).",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute path on the bot's own box." },
                    "content": { "type": "string", "description": "The file's full new contents." },
                },
                "required": ["path", "content"],
            }),
        )),
        "open_url" => Some((
            "Open a web page in the browser on THIS BOT'S OWN computer — its sandboxed box, which \
             has a screen. The page appears on that display; use `computer` with \
             action=screenshot to see it, then click and type on it.",
            serde_json::json!({
                "type": "object",
                "properties": { "url": { "type": "string", "description": "The page to open, with its scheme (https://…)." } },
                "required": ["url"],
            }),
        )),
        "computer" => Some((
            "Use the screen of THIS BOT'S OWN computer: a 1280x800 display with a desktop, a dock \
             (Terminal, Chromium, Files) and whatever windows are open. `screenshot` returns the \
             display as an image. `click`, `right_click`, `double_click`, `move`, `drag`, `type`, \
             `key` and `scroll` act at pixel coordinates and return a fresh screenshot. Look before \
             you act, do one step at a time, and check each result before the next.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["screenshot", "click", "right_click", "double_click", "move", "drag", "type", "key", "scroll"] },
                    "coordinate": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "[x, y] in display pixels, 0,0 top-left." },
                    "to": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "For drag: where to drop, [x, y]." },
                    "text": { "type": "string", "description": "For type: the text to type." },
                    "key": { "type": "string", "description": "For key: a key or chord, e.g. Return, Escape, ctrl+l, alt+F4." },
                    "scroll": { "type": "array", "items": { "type": "integer" }, "minItems": 2, "maxItems": 2, "description": "For scroll: [dx, dy]; positive dy scrolls down." },
                    "button": { "type": "integer", "description": "For click: 1 left (default), 2 middle, 3 right." }
                },
                "required": ["action"],
            }),
        )),
        REQUEST_USER_FORM => Some(user_form::tool_spec()),
        USER_MACHINE_SHELL => Some((
            "Run a shell command on the USER'S OWN machine — the real computer they enrolled,              NOT this bot's sandboxed box. It runs only with the user's consent under their              reverse-exec policy: a command may run, be refused, or be held for the user to approve              (in which case you should wait rather than retry). Use this ONLY when the task is about              the user's own machine; for your own work use `shell`.",
            serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The shell command to run on the USER's own machine." } },
                "required": ["command"],
            }),
        )),
        _ => None,
    }
}

pub fn overwrite_identity(arguments: &Value, context: &ToolContext) -> Value {
    let mut object = match arguments {
        Value::Object(map) => map.clone(),
        // A non-object argument carries no identity to overwrite, but must still not be able to
        // smuggle one — it is replaced by an object with only ours. `execute` refuses it first.
        _ => serde_json::Map::new(),
    };

    // Strip EVERY identity alias the model may have written — both spellings of account/coworker/
    // box — BEFORE inserting ours. Overwriting only `coworker_id`/`box_id` left the camelCase
    // twins (`coworkerId`/`boxId`) to ride through untouched, and for a plugin tool the whole
    // argument map is forwarded to a remote server: an attacker-chosen `coworkerId` reaching a
    // connector is exactly what "overwrite, never validate" (CLAUDE.md #7) forbids. The alias list
    // is the same one the judge redacts (`review::IDENTITY_KEYS`), so the two cannot drift.
    for key in crate::review::IDENTITY_KEYS {
        object.remove(*key);
    }

    object.insert(
        "coworker_id".to_string(),
        Value::String(context.coworker_id.to_string()),
    );
    // A box is inserted when the session has one; when it does not, the key stays removed rather
    // than carrying whatever the model wrote (the strip above already took every spelling).
    if let Some(box_id) = &context.box_id {
        object.insert("box_id".to_string(), Value::String(box_id.to_string()));
    }

    Value::Object(object)
}

/// Remove the identity keys before handing arguments to a remote server.
///
/// They exist so a LOCAL tool cannot be aimed elsewhere. A remote MCP server never sees them: it
/// did not ask for them, its schema does not have them, and a strict server would reject the call
/// over a field the model never wrote. Every alias `overwrite_identity` guards against is removed
/// here too — a value the model must not choose must not leave the process under any spelling.
pub(crate) fn strip_identity(arguments: Value) -> Value {
    match arguments {
        Value::Object(mut map) => {
            for key in crate::review::IDENTITY_KEYS {
                map.remove(*key);
            }
            Value::Object(map)
        }
        other => other,
    }
}

/// A remote server's error text, cut to what a prompt can afford: it is the server's words, and a
/// server must not be able to fill the system message with them.
fn clip_reason(reason: &str) -> String {
    const MOST: usize = 240;
    match reason.char_indices().nth(MOST) {
        Some((cut, _)) => format!("{}…", &reason[..cut]),
        None => reason.to_string(),
    }
}

/// A box failure, in words a model can act on.
fn describe(error: BoxError) -> String {
    match error {
        BoxError::NoSuchBox => "that computer no longer exists".to_string(),
        BoxError::Secret(reason) => reason.clone(),
        BoxError::Unreachable(detail) => format!("the computer is unreachable: {detail}"),
        BoxError::Interrupted(detail) => {
            format!("the connection to the computer was lost before it answered: {detail}")
        }
        BoxError::Refused { status, body } => {
            format!("the computer refused the request ({status}): {body}")
        }
    }
}
