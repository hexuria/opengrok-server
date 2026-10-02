//! Two requests that each find a scope without a computer end on ONE box (#302).
//!
//! `ensure_scope_box` reads the scope, finds no box, and asks the provider for one. The record
//! was an upsert, so of two such requests the later write won and the earlier box ran on with
//! nothing tracking it: never idle-stopped, never destroyed, billed on box.ascii.dev. The e2e
//! server hit it twice on 30 Sep 2026: two first `POST /coworkers/{id}/computer` calls on one
//! account, and a reset whose re-provision raced the client asking for a computer once it read
//! `absent`. The coworker the loser answered was left naming the orphan.
//!
//! The stand-in provider holds every create at a gate the test opens, so both requests are past
//! their read before either records, and the test picks which records first. It holds a rebuild
//! the same way, so an update can be raced against a heal, a second update and a failing store:
//! a rebuilt box runs on the disk of the box it replaces, and losing it must never lose that disk
//! (#311 review). The boot repair's tests are with the boot (`crates/opengrok/src/repair.rs`).
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, BoxId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::{live_boxes, update_scope_box};
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_server::host_state::HostState;
use opengrok_store::PgStore;
use serde_json::Value;
use tokio::sync::oneshot;

macro_rules! database_or_skip {
    () => {
        match std::env::var("OG_DATABASE_URL") {
            Ok(url) => opengrok_store::gate_database_or_panic(url),
            Err(_) => {
                eprintln!("skipping: OG_DATABASE_URL is not set");
                return;
            }
        }
    };
}

/// How long any one wait here may take IN ALL: a gate's arrivals, a record, a request, an update.
/// A gate the test never opens, or opens for the wrong request, then fails the test and names what
/// it waited for. Without it, the rebuild race below once waited on an update held at a gate it
/// opens only after that update ends, and the suite hung for 45 minutes and more.
const DEADLINE: Duration = Duration::from_secs(30);

/// `wait`, or a failure naming `what` once it has taken `DEADLINE`.
async fn within<T>(what: impl std::fmt::Display, wait: impl Future<Output = T>) -> T {
    let bounded = tokio::time::timeout(DEADLINE, wait).await;
    bounded.unwrap_or_else(|_| panic!("waited {} s for {what}", DEADLINE.as_secs()))
}

/// Where a held rebuild waits: before it removes the box it replaces, so a second rebuild of that
/// box can still start, or after, while the old box reads `absent` and nothing records the new one.
#[derive(Clone, Copy, PartialEq)]
enum Hold {
    BeforeRemovingOld,
    AfterRemovingOld,
}

/// A Local VM provider that remembers what it made, destroyed and discarded, and the disk each box
/// runs on: a rebuild runs on the disk of the box it replaces, as `recreate` does. It holds each
/// create (`gated`) or rebuild (`hold`) until the test opens its gate. A box it never made, or has
/// removed, is `absent`, and removing it again is refused, as Docker refuses, disk untouched.
#[derive(Default)]
struct Gated {
    gated: bool,
    hold: Option<Hold>,
    rebuilt_prefix: &'static str,
    arrived: Mutex<Vec<String>>,
    gates: Mutex<HashMap<String, oneshot::Sender<()>>>,
    destroyed: Mutex<Vec<String>>,
    discarded: Mutex<Vec<String>>,
    wiped: Mutex<Vec<String>>,
    disks: Mutex<HashMap<String, String>>,
    live: Mutex<HashSet<String>>,
}

impl Gated {
    fn holding_creates() -> Arc<Self> {
        Arc::new(Self {
            gated: true,
            ..Self::default()
        })
    }

    fn holding_rebuilds(hold: Hold) -> Arc<Self> {
        Arc::new(Self {
            hold: Some(hold),
            ..Self::default()
        })
    }

    /// The first `n` creates or rebuilds to reach the gate, in the order they reached it.
    async fn held(&self, n: usize) -> Vec<String> {
        let reached = async {
            loop {
                let arrived = self.arrived.lock().unwrap().clone();
                if arrived.len() >= n {
                    return arrived[..n].to_vec();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let what = format!("{n} creates or rebuilds to reach the gate");
        within(what, reached).await
    }

    fn open(&self, id: &str) {
        let gate = self
            .gates
            .lock()
            .unwrap()
            .remove(id)
            .expect("a held create");
        gate.send(()).expect("the create is still waiting");
    }

    async fn wait(&self, id: &str) {
        let (open, wait) = oneshot::channel();
        self.gates.lock().unwrap().insert(id.to_string(), open);
        self.arrived.lock().unwrap().push(id.to_string());
        wait.await.expect("the test opens every gate it holds");
    }

    fn made(&self, id: &str, disk: &str) {
        self.live.lock().unwrap().insert(id.to_string());
        let mut disks = self.disks.lock().unwrap();
        disks.insert(id.to_string(), disk.to_string());
    }

    fn disk(&self, id: &str) -> String {
        self.disks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .expect("a box made here")
    }

    fn destroyed(&self) -> Vec<String> {
        self.destroyed.lock().unwrap().clone()
    }

    fn discarded(&self) -> Vec<String> {
        self.discarded.lock().unwrap().clone()
    }

    fn wiped(&self) -> Vec<String> {
        self.wiped.lock().unwrap().clone()
    }

    fn running(&self) -> HashSet<String> {
        self.live.lock().unwrap().clone()
    }
}

#[async_trait]
impl Computer for Gated {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        let id = format!("bx_gate_{}", uuid::Uuid::now_v7().simple());
        self.made(&id, &format!("disk_{id}"));
        if self.gated {
            self.wait(&id).await;
        }
        Ok(id)
    }
    async fn recreate(&self, old_box_id: &str) -> BoxResult<String> {
        if !self.running().contains(old_box_id) {
            return Err(BoxError::NoSuchBox);
        }
        let prefix = match self.rebuilt_prefix {
            "" => "bx_rebuilt_",
            prefix => prefix,
        };
        let id = format!("{prefix}{}", uuid::Uuid::now_v7().simple());
        self.made(&id, &self.disk(old_box_id));
        if self.hold == Some(Hold::BeforeRemovingOld) {
            self.wait(&id).await;
        }
        self.live.lock().unwrap().remove(old_box_id);
        if self.hold == Some(Hold::AfterRemovingOld) {
            self.wait(&id).await;
        }
        Ok(id)
    }
    async fn run(&self, _box_id: &str, _command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
        Ok(CommandOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            timed_out: false,
        })
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Ok(StartedCommand {
            process_id: "p".to_string(),
            running: false,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
        })
    }
    async fn watch(&self, box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        self.start(box_id, "").await
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Ok("http://gated.invalid".to_string())
    }
    async fn stop(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _box_id: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, box_id: &str) -> BoxResult<()> {
        if !self.live.lock().unwrap().remove(box_id) {
            return Err(BoxError::NoSuchBox);
        }
        self.wiped.lock().unwrap().push(self.disk(box_id));
        self.destroyed.lock().unwrap().push(box_id.to_string());
        Ok(())
    }
    async fn discard(&self, box_id: &str) -> BoxResult<()> {
        if !self.live.lock().unwrap().remove(box_id) {
            return Err(BoxError::NoSuchBox);
        }
        self.discarded.lock().unwrap().push(box_id.to_string());
        Ok(())
    }
    async fn state(&self, box_id: &str) -> BoxResult<String> {
        let running = self.live.lock().unwrap().contains(box_id);
        Ok(if running { "running" } else { "absent" }.to_string())
    }
    /// An update waits for its rebuilt box's screen; these have one at once.
    async fn screen_url(&self, box_id: &str) -> BoxResult<Option<String>> {
        Ok(Some(format!("http://127.0.0.1:1/vnc.html?box={box_id}")))
    }
}

struct Server {
    base: String,
    state: AgUiState,
    stub: Arc<Gated>,
    client: reqwest::Client,
}

async fn server(database_url: &str, stub: Arc<Gated>) -> Server {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let state = AgUiState {
        auth: AuthState::new(
            PgStore::new(pool),
            Arc::new(TokenMinter::new(b"raced-computer-test-secret-raced")),
            "host@og.local".to_string(),
        ),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        // The deployment's own Local VM provider, the stand-in: with no vault there is no org
        // key, so every new box is asked of it.
        computer: Some(stub.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    let app = opengrok_server::router(state.clone(), HostState::new(state.clone(), None));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    Server {
        base: format!("http://127.0.0.1:{port}"),
        state,
        stub,
        client: reqwest::Client::new(),
    }
}

/// A person who signed in, under `org` when given, and their bearer token.
struct Member {
    id: AccountId,
    token: String,
}

impl Server {
    fn store(&self) -> &PgStore {
        &self.state.auth.store
    }

    async fn member(&self, org: Option<&str>) -> Member {
        let id = AccountId::new();
        let email = format!("raced-{}@og.local", uuid::Uuid::now_v7().simple());
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: "Raced".to_string(),
                last_name: String::new(),
                org_id: org.unwrap_or_default().to_string(),
                plan: Plan::Ultra,
                verified: true,
                enabled: true,
                at_ms,
            })
            .expect("register");
        let view = AccountView {
            id: id.clone(),
            email: email.clone(),
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: "Raced".to_string(),
            last_name: String::new(),
            org_id: org.map(str::to_string),
            verified: true,
            enabled: true,
            avatar_url: None,
        };
        self.store()
            .append_account(&id, 0, &events, &view)
            .await
            .expect("append account");
        let token = self
            .state
            .auth
            .minter
            .mint_access(
                id.as_str(),
                "sess-raced",
                &email,
                "ultra",
                chrono::Utc::now().timestamp(),
                3600,
            )
            .expect("mint access");
        Member { id, token }
    }

    async fn share(&self, scope: &str, scope_id: &str, mode: &str) {
        self.store()
            .set_sharing_mode(scope, scope_id, mode, 1)
            .await
            .expect("sharing mode");
    }

    /// A coworker of `account`, given `first` as its first box, or no box when `None`: a hire
    /// whose provisioning failed, which the client later asks a computer for.
    async fn coworker(&self, account: &AccountId, first: Option<&str>) -> CoworkerId {
        let id = CoworkerId::new();
        let mut coworker = Coworker::default();
        let mut events = coworker
            .decide(CoworkerCommand::Hire {
                name: "Raced".to_string(),
                model: "oag/cheap".to_string(),
                at_ms: 1,
            })
            .expect("hire");
        for event in &events {
            coworker.apply(event);
        }
        if let Some(first) = first {
            let assigned = coworker
                .decide(CoworkerCommand::AssignComputer {
                    box_id: BoxId::from_stored(first.to_string()),
                    mode: BoxMode::Shared,
                    at_ms: 1,
                })
                .expect("assign");
            for event in &assigned {
                coworker.apply(event);
            }
            events.extend(assigned);
        }
        let view = CoworkerView::of(id.clone(), &coworker, 1);
        self.store()
            .append_coworker(&id, account, 0, &events, &view)
            .await
            .expect("append coworker");
        id
    }

    /// `POST /coworkers/{id}/computer/update`: the update runs on after the reply. The 202 is the
    /// update as accepted, read before any of the work: its outcome, even an instant failure, is
    /// the status route's to tell, or the reply's shape changes from run to run (#311).
    async fn update(&self, member: &Member, coworker: &CoworkerId) {
        let url = format!(
            "{}/coworkers/{}/computer/update",
            self.base,
            coworker.as_str()
        );
        let request = self.client.post(url).bearer_auth(&member.token);
        let accepted = async {
            let reply = request.send().await.expect("update");
            let status = reply.status().as_u16();
            let body = reply.json::<Value>().await.expect("the accepted status");
            (status, body)
        };
        let (status, body) = within("the update's reply", accepted).await;
        assert_eq!(status, 202, "the update is accepted");
        assert_eq!(body["update"]["phase"], "pulling", "as accepted: {body}");
        assert_eq!(body["update"]["error"], Value::Null, "as accepted: {body}");
        assert_eq!(body["state"], "running", "the box before the work: {body}");
    }

    /// Wait for the scope's update to end: `None` once it succeeded and its record was cleared,
    /// or the failed record's reason.
    async fn update_ended(&self, scope: (&str, &str)) -> Option<Option<String>> {
        let ended = async {
            loop {
                match self
                    .store()
                    .box_update(scope.0, scope.1)
                    .await
                    .expect("update row")
                {
                    None => return None,
                    Some((phase, _, _, error)) if phase == "failed" => return Some(error),
                    Some(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        };
        within(format!("the update of {scope:?} to end"), ended).await
    }

    /// The account's first box, made by the client asking for a computer.
    async fn first_box(&self, member: &Member, coworker: &CoworkerId) -> String {
        assert_eq!(self.post(member, coworker, false).await, 200);
        let scope = member.id.as_str();
        self.scope_box("account", scope)
            .await
            .expect("the account's box")
    }

    /// `POST /coworkers/{id}/computer`, or `…/computer/reset`, sent in the background at once: a
    /// held request is in flight before the test awaits it. Answers its status.
    fn post(
        &self,
        member: &Member,
        coworker: &CoworkerId,
        reset: bool,
    ) -> impl Future<Output = u16> {
        let tail = if reset { "/reset" } else { "" };
        let path = format!("/coworkers/{}/computer{tail}", coworker.as_str());
        let request = self.client.post(format!("{}{path}", self.base));
        let request = request.bearer_auth(&member.token);
        let sent = tokio::spawn(async move { request.send().await.expect("post").status() });
        async move {
            let answered = within(format!("POST {path} to be answered"), sent).await;
            answered.expect("the request's task").as_u16()
        }
    }

    /// The roster's `boxId` for each of the member's own coworkers.
    async fn roster(&self, member: &Member) -> HashMap<String, Value> {
        let request = self.client.get(format!("{}/coworkers", self.base));
        let read = async {
            let reply = request.bearer_auth(&member.token).send().await;
            let rows = reply.expect("roster").json::<Vec<Value>>().await;
            rows.expect("roster body")
        };
        let rows = within("the roster", read).await;
        rows.into_iter()
            .map(|row| {
                (
                    row["id"].as_str().unwrap().to_string(),
                    row["boxId"].clone(),
                )
            })
            .collect()
    }

    /// The box a coworker's own record says it was given.
    async fn given(&self, coworker: &CoworkerId) -> Option<String> {
        let (loaded, _) = self.store().load_coworker(coworker).await.expect("load");
        loaded.computer().map(|id| id.as_str().to_string())
    }

    async fn scope_box(&self, scope: &str, scope_id: &str) -> Option<String> {
        self.store()
            .scoped_computer(scope, scope_id)
            .await
            .expect("scope row")
            .map(|(box_id, _)| box_id)
    }

    /// Wait until the scope records `box_id`: the request let through first has claimed it.
    async fn recorded(&self, scope: &str, scope_id: &str, box_id: &str) {
        let recorded = async {
            while self.scope_box(scope, scope_id).await.as_deref() != Some(box_id) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        within(format!("{scope}/{scope_id} to record {box_id}"), recorded).await;
    }

    /// Two members' first requests on one scope, each held after its read, let through one at a
    /// time when `in_turn`, both at once otherwise. Answers the box made by each, in the order
    /// the requests were sent.
    async fn two_first_requests(
        &self,
        requests: [(&Member, &CoworkerId); 2],
        scope: (&str, &str),
        in_turn: bool,
    ) -> Vec<String> {
        let first = self.post(requests[0].0, requests[0].1, false);
        let held = self.stub.held(1).await;
        let second = self.post(requests[1].0, requests[1].1, false);
        let held = [held, self.stub.held(2).await[1..].to_vec()].concat();
        if in_turn {
            self.stub.open(&held[0]);
            self.recorded(scope.0, scope.1, &held[0]).await;
            self.stub.open(&held[1]);
        } else {
            for id in &held {
                self.stub.open(id);
            }
        }
        assert_eq!(first.await, 200, "the first request");
        assert_eq!(second.await, 200, "the second request");
        held
    }

    /// One box kept, every other box this test made destroyed, and every coworker on the kept
    /// one: by its own assignment, and by the roster its member reads.
    async fn one_box_for_all(
        &self,
        made: &[String],
        scope: (&str, &str),
        everyone: &[(&Member, &CoworkerId)],
    ) -> String {
        let kept = self
            .scope_box(scope.0, scope.1)
            .await
            .expect("the scope has a box");
        assert!(
            made.contains(&kept),
            "the scope keeps a box made here: {kept}"
        );
        let others: HashSet<String> = made.iter().filter(|id| **id != kept).cloned().collect();
        let destroyed: HashSet<String> = self.stub.destroyed().into_iter().collect();
        assert_eq!(
            destroyed, others,
            "every box but the kept one is destroyed, and only those"
        );
        assert_eq!(
            self.stub.running(),
            HashSet::from([kept.clone()]),
            "exactly one box runs"
        );
        for (member, coworker) in everyone {
            let (loaded, _) = self.store().load_coworker(coworker).await.expect("load");
            assert!(
                loaded.computer().is_some(),
                "{coworker} was given a computer"
            );
            let view = CoworkerView::of((*coworker).clone(), &loaded, 1);
            let live = live_boxes(&self.state, &member.id, &[&view]).await;
            assert_eq!(
                live.get(*coworker),
                Some(&kept),
                "{coworker} is on the kept box"
            );
            let roster = self.roster(member).await;
            assert_eq!(
                roster.get(coworker.as_str()),
                Some(&Value::String(kept.clone())),
                "the roster names the kept box for {coworker}"
            );
        }
        kept
    }
}

#[tokio::test]
async fn two_first_requests_on_one_account_keep_one_box() {
    let database_url = database_or_skip!();
    let s = server(&database_url, Gated::holding_creates()).await;
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let one = s.coworker(&member.id, None).await;
    let two = s.coworker(&member.id, None).await;
    let scope = ("account", member.id.as_str());

    let made = s
        .two_first_requests([(&member, &one), (&member, &two)], scope, true)
        .await;

    let kept = s
        .one_box_for_all(&made, scope, &[(&member, &one), (&member, &two)])
        .await;
    assert_eq!(
        kept, made[0],
        "the request that recorded first keeps its box"
    );
    assert_eq!(
        s.stub.destroyed(),
        vec![made[1].clone()],
        "the other destroys its own"
    );
    // The coworker the loser answered is given the winner's box, not the one it destroyed: the
    // e2e loser's coworker was left naming the box nothing tracked.
    for coworker in [&one, &two] {
        assert_eq!(s.given(coworker).await, Some(kept.clone()));
    }
}

#[tokio::test]
async fn two_first_requests_let_through_at_once_still_keep_one_box() {
    let database_url = database_or_skip!();
    let s = server(&database_url, Gated::holding_creates()).await;
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let one = s.coworker(&member.id, None).await;
    let two = s.coworker(&member.id, None).await;
    let scope = ("account", member.id.as_str());

    let made = s
        .two_first_requests([(&member, &one), (&member, &two)], scope, false)
        .await;

    let kept = s
        .one_box_for_all(&made, scope, &[(&member, &one), (&member, &two)])
        .await;
    for coworker in [&one, &two] {
        assert_eq!(s.given(coworker).await, Some(kept.clone()));
    }
}

#[tokio::test]
async fn two_members_first_requests_on_one_org_keep_one_box() {
    let database_url = database_or_skip!();
    let s = server(&database_url, Gated::holding_creates()).await;
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    s.share("org", &org, "per-org").await;
    let (ann, bob) = (s.member(Some(&org)).await, s.member(Some(&org)).await);
    let ann_bot = s.coworker(&ann.id, None).await;
    let bob_bot = s.coworker(&bob.id, None).await;
    let scope = ("org", org.as_str());

    let made = s
        .two_first_requests([(&ann, &ann_bot), (&bob, &bob_bot)], scope, true)
        .await;

    let kept = s
        .one_box_for_all(&made, scope, &[(&ann, &ann_bot), (&bob, &bob_bot)])
        .await;
    assert_eq!(kept, made[0]);
    for coworker in [&ann_bot, &bob_bot] {
        assert_eq!(s.given(coworker).await, Some(kept.clone()));
    }
    assert_eq!(
        s.scope_box("account", ann.id.as_str()).await,
        None,
        "no member box was made"
    );
    assert_eq!(
        s.scope_box("account", bob.id.as_str()).await,
        None,
        "no member box was made"
    );
}

/// A reset destroys the box and forgets it, and while its own create is out, the client that
/// read `absent` asks for a computer: two creates on one empty scope. Whichever records first
/// is the box; the other is destroyed, the reset's own included.
async fn a_reset_racing_a_request(reset_records_first: bool) {
    let database_url = database_or_skip!();
    let s = server(&database_url, Gated::holding_creates()).await;
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let one = s.coworker(&member.id, None).await;
    let two = s.coworker(&member.id, None).await;
    let scope = ("account", member.id.as_str());
    let first = s.post(&member, &one, false);
    let old = s.stub.held(1).await.remove(0);
    s.stub.open(&old);
    assert_eq!(first.await, 200);
    assert_eq!(
        s.post(&member, &two, false).await,
        200,
        "the second shares it"
    );

    let reset = s.post(&member, &one, true);
    let by_reset = s.stub.held(2).await.remove(1);
    assert_eq!(
        s.scope_box(scope.0, scope.1).await,
        None,
        "the reset forgot the old box"
    );
    let request = s.post(&member, &one, false);
    let by_request = s.stub.held(3).await.remove(2);
    let (winner, loser) = if reset_records_first {
        (by_reset.clone(), by_request.clone())
    } else {
        (by_request.clone(), by_reset.clone())
    };
    s.stub.open(&winner);
    s.recorded(scope.0, scope.1, &winner).await;
    s.stub.open(&loser);
    assert_eq!(reset.await, 200, "the reset");
    assert_eq!(request.await, 200, "the request");

    let made = [old.clone(), by_reset, by_request];
    let kept = s
        .one_box_for_all(&made, scope, &[(&member, &one), (&member, &two)])
        .await;
    assert_eq!(kept, winner);
    assert_eq!(
        s.stub.destroyed(),
        vec![old, loser],
        "the old box by the reset, then the loser"
    );
}

#[tokio::test]
async fn a_reset_that_records_first_keeps_its_box_and_the_request_destroys_its_own() {
    a_reset_racing_a_request(true).await;
}

#[tokio::test]
async fn a_request_that_records_first_keeps_its_box_and_the_reset_destroys_its_own() {
    a_reset_racing_a_request(false).await;
}

/// An update removes the box it rebuilds before it records the new one, and the client that reads
/// `absent` meanwhile asks for a computer. That request healed the scope with a fresh box, the
/// update's record then missed, and the rebuilt box was destroyed with the disk it runs on: the
/// person's files (#311 review). Now the scope waits for the update.
#[tokio::test]
async fn an_update_racing_a_heal_keeps_the_rebuilt_box_and_its_files() {
    let database_url = database_or_skip!();
    let s = server(
        &database_url,
        Gated::holding_rebuilds(Hold::AfterRemovingOld),
    )
    .await;
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let one = s.coworker(&member.id, None).await;
    let scope = ("account", member.id.as_str());
    let old = s.first_box(&member, &one).await;
    let disk = s.stub.disk(&old);

    s.update(&member, &one).await;
    let rebuilt = s.stub.held(1).await.remove(0);
    assert!(!s.stub.running().contains(&old), "the old box reads absent");
    assert_eq!(s.post(&member, &one, false).await, 200, "asked");
    assert_eq!(s.scope_box(scope.0, scope.1).await, Some(old), "no heal");

    s.stub.open(&rebuilt);
    assert_eq!(s.update_ended(scope).await, None, "the update succeeded");
    assert_eq!(s.scope_box(scope.0, scope.1).await, Some(rebuilt.clone()));
    assert_eq!(
        s.stub.running(),
        HashSet::from([rebuilt.clone()]),
        "no second box"
    );
    assert_eq!(s.stub.disk(&rebuilt), disk, "on the old box's disk");
    assert!(
        s.stub.wiped().is_empty(),
        "no disk wiped: {:?}",
        s.stub.wiped()
    );
}

/// Refuse any write of a box id marked unrecordable: a store failing the write an update records
/// its rebuilt box with. Other tests' ids are untouched.
async fn refuse_unrecordable_boxes(s: &Server) {
    sqlx::raw_sql(
        "create or replace function refuse_unrecordable_box() returns trigger language plpgsql as $f$
         begin
             if new.box_id like 'bx_unrecordable_%' then
                 raise exception 'the store refused this write';
             end if;
             return new;
         end $f$;
         do $d$ begin
             if not exists (select 1 from pg_trigger where tgname = 'refuse_unrecordable_box') then
                 create trigger refuse_unrecordable_box before insert or update on scoped_computer
                     for each row execute function refuse_unrecordable_box();
             end if;
         end $d$;",
    )
    .execute(s.store().pool())
    .await
    .expect("install the refusing trigger");
}

/// A failed write may still have landed, so a rebuilt box whose record failed is never removed
/// blind: its disk is the person's files. Here the store refuses the write, the row still names
/// the old box, and the rebuilt one is left running, named in the update's failure.
#[tokio::test]
async fn a_rebuilt_box_whose_record_fails_is_left_running_with_its_files() {
    let database_url = database_or_skip!();
    let stub = Arc::new(Gated {
        rebuilt_prefix: "bx_unrecordable_",
        ..Gated::default()
    });
    let s = server(&database_url, stub).await;
    refuse_unrecordable_boxes(&s).await;
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let one = s.coworker(&member.id, None).await;
    let scope = ("account", member.id.as_str());
    let old = s.first_box(&member, &one).await;
    let disk = s.stub.disk(&old);

    s.update(&member, &one).await;
    let why = s.update_ended(scope).await.expect("the update failed");

    let running = s.stub.running();
    let rebuilt = running
        .iter()
        .find(|id| id.starts_with("bx_unrecordable_"))
        .expect("the rebuilt box still runs");
    assert!(
        why.unwrap_or_default().contains(rebuilt.as_str()),
        "the failure names it"
    );
    assert!(s.stub.destroyed().is_empty() && s.stub.discarded().is_empty());
    assert!(
        s.stub.wiped().is_empty(),
        "no disk wiped: {:?}",
        s.stub.wiped()
    );
    assert_eq!(s.stub.disk(rebuilt), disk);
    assert_eq!(
        s.scope_box(scope.0, scope.1).await,
        Some(old),
        "the write never landed"
    );
}

/// Two updates of one box (a second Update before the first one's record is in) rebuild it twice
/// on one disk. The one that records second loses, and only its container goes: destroying it
/// wiped the disk the winner runs on, the person's files (#311 review).
#[tokio::test]
async fn a_rebuilt_box_that_loses_its_scope_leaves_the_disk_the_winner_runs_on() {
    let database_url = database_or_skip!();
    let s = server(
        &database_url,
        Gated::holding_rebuilds(Hold::BeforeRemovingOld),
    )
    .await;
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let one = s.coworker(&member.id, None).await;
    let scope = ("account", member.id.as_str());
    let old = s.first_box(&member, &one).await;
    let disk = s.stub.disk(&old);

    let update = || {
        let (state, scope_id) = (s.state.clone(), member.id.as_str().to_string());
        tokio::spawn(update_scope_box(state, None, "account", scope_id))
    };
    // The second update is sent once the first is held, so the first rebuild held is the first
    // update's. Sent together, either could reach the gate first, as each reads the store on the
    // way: under load the second did, the test opened its gate and waited for the first update,
    // held at a gate the test opens only once that update has ended, and the run hung.
    let first = update();
    s.stub.held(1).await;
    let second = update();
    let held = s.stub.held(2).await;
    s.stub.open(&held[0]);
    within("the first update to end", first).await.unwrap();
    s.stub.open(&held[1]);
    within("the second update to end", second).await.unwrap();

    assert_eq!(s.scope_box(scope.0, scope.1).await, Some(held[0].clone()));
    assert!(
        s.stub.wiped().is_empty(),
        "no disk wiped: {:?}",
        s.stub.wiped()
    );
    assert_eq!(
        s.stub.discarded(),
        vec![held[1].clone()],
        "the loser's container goes"
    );
    assert_eq!(s.stub.running(), HashSet::from([held[0].clone()]));
    assert_eq!(s.stub.disk(&held[0]), disk, "the winner keeps the files");
}
