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
//! their read before either records, and the test picks which records first. The boot repair is
//! driven on seeded rows: it reports #302's strays unless told to destroy them, and never touches
//! a box recorded only on a coworker's row, which is how a box hired before 31 Aug 2026 was kept.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, BoxId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::{live_boxes, repair_destroys, repair_stray_boxes};
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

/// A Local VM provider that remembers what it made and destroyed, and, when gated, holds each
/// create until the test opens its gate. A box it never made, or has destroyed, is `absent`.
#[derive(Default)]
struct Gated {
    gated: bool,
    arrived: Mutex<Vec<String>>,
    gates: Mutex<HashMap<String, oneshot::Sender<()>>>,
    destroyed: Mutex<Vec<String>>,
    live: Mutex<HashSet<String>>,
}

impl Gated {
    fn holding_creates() -> Arc<Self> {
        Arc::new(Self {
            gated: true,
            ..Self::default()
        })
    }

    /// The first `n` creates to reach the gate, in the order they reached it.
    async fn held(&self, n: usize) -> Vec<String> {
        for _ in 0..1500 {
            let arrived = self.arrived.lock().unwrap().clone();
            if arrived.len() >= n {
                return arrived[..n].to_vec();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{n} creates never reached the gate");
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

    fn made(&self, id: &str) {
        self.live.lock().unwrap().insert(id.to_string());
    }

    fn destroyed(&self) -> Vec<String> {
        self.destroyed.lock().unwrap().clone()
    }

    fn running(&self) -> HashSet<String> {
        self.live.lock().unwrap().clone()
    }
}

#[async_trait]
impl Computer for Gated {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        let id = format!("bx_gate_{}", uuid::Uuid::now_v7().simple());
        self.made(&id);
        if self.gated {
            let (open, wait) = oneshot::channel();
            self.gates.lock().unwrap().insert(id.clone(), open);
            self.arrived.lock().unwrap().push(id.clone());
            wait.await.expect("the test opens every gate it holds");
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
        self.destroyed.lock().unwrap().push(box_id.to_string());
        self.live.lock().unwrap().remove(box_id);
        Ok(())
    }
    async fn state(&self, box_id: &str) -> BoxResult<String> {
        let running = self.live.lock().unwrap().contains(box_id);
        Ok(if running { "running" } else { "absent" }.to_string())
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

    /// A coworker of `account` given `first` as a SHARED box, as scope provisioning gives one, or
    /// no box when `None`: a hire whose provisioning failed, which the client later asks a
    /// computer for.
    async fn coworker(&self, account: &AccountId, first: Option<&str>) -> CoworkerId {
        self.hired(account, first.map(|id| (id, BoxMode::Shared)))
            .await
    }

    async fn hired(&self, account: &AccountId, first: Option<(&str, BoxMode)>) -> CoworkerId {
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
        if let Some((first, mode)) = first {
            let assigned = coworker
                .decide(CoworkerCommand::AssignComputer {
                    box_id: BoxId::from_stored(first.to_string()),
                    mode,
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

    /// `POST /coworkers/{id}/computer`, or `…/computer/reset`, in the background.
    fn post(
        &self,
        member: &Member,
        coworker: &CoworkerId,
        reset: bool,
    ) -> tokio::task::JoinHandle<u16> {
        let tail = if reset { "/reset" } else { "" };
        let url = format!(
            "{}/coworkers/{}/computer{tail}",
            self.base,
            coworker.as_str()
        );
        let request = self.client.post(url).bearer_auth(&member.token);
        tokio::spawn(async move { request.send().await.expect("post").status().as_u16() })
    }

    /// The roster's `boxId` for each of the member's own coworkers.
    async fn roster(&self, member: &Member) -> HashMap<String, Value> {
        let rows: Vec<Value> = self
            .client
            .get(format!("{}/coworkers", self.base))
            .bearer_auth(&member.token)
            .send()
            .await
            .expect("roster")
            .json()
            .await
            .expect("roster body");
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
        for _ in 0..1500 {
            if self.scope_box(scope, scope_id).await.as_deref() == Some(box_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{scope}/{scope_id} never recorded {box_id}");
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
        assert_eq!(first.await.unwrap(), 200, "the first request");
        assert_eq!(second.await.unwrap(), 200, "the second request");
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
    assert_eq!(first.await.unwrap(), 200);
    assert_eq!(
        s.post(&member, &two, false).await.unwrap(),
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
    assert_eq!(reset.await.unwrap(), 200, "the reset");
    assert_eq!(request.await.unwrap(), 200, "the request");

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

/// Told to destroy, the repair destroys a stray box (one no record names, whose coworkers are on a
/// shared box of an account or org that records another) on its scope's provider; a box a record
/// names is never touched, nor is one already gone; and a second pass changes nothing.
#[tokio::test]
async fn told_to_destroy_the_repair_destroys_a_stray_box_and_never_a_recorded_one() {
    let database_url = database_or_skip!();
    let s = server(&database_url, Arc::new(Gated::default())).await;
    let unique = || format!("bx_{}", uuid::Uuid::now_v7().simple());
    let (kept, stray, gone, elsewhere, org_kept, org_stray) =
        (unique(), unique(), unique(), unique(), unique(), unique());
    for id in [&kept, &stray, &elsewhere, &org_kept, &org_stray] {
        s.stub.made(id);
    }

    // An account whose box is `kept`: one coworker on it, one left naming `stray` (the race's
    // loser), one naming `gone` (a box a reset destroyed), one naming `elsewhere`, which is
    // another account's recorded box.
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    let other = s.member(None).await;
    let store = s.store();
    for (scope_id, box_id) in [(member.id.as_str(), &kept), (other.id.as_str(), &elsewhere)] {
        store
            .set_scoped_computer("account", scope_id, box_id, "local-docker", None, 1)
            .await
            .expect("record");
    }
    let on_kept = s.coworker(&member.id, Some(&kept)).await;
    let on_stray = s.coworker(&member.id, Some(&stray)).await;
    s.coworker(&member.id, Some(&gone)).await;
    s.coworker(&member.id, Some(&elsewhere)).await;

    // The same shape on an org's shared box.
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    s.share("org", &org, "per-org").await;
    let org_member = s.member(Some(&org)).await;
    store
        .set_scoped_computer("org", &org, &org_kept, "local-docker", Some(&org), 1)
        .await
        .expect("record");
    s.coworker(&org_member.id, Some(&org_kept)).await;
    let on_org_stray = s.coworker(&org_member.id, Some(&org_stray)).await;

    let found: HashSet<String> = repair_stray_boxes(&s.state, true)
        .await
        .into_iter()
        .collect();

    assert_eq!(found, HashSet::from([stray.clone(), org_stray.clone()]));
    let destroyed: HashSet<String> = s.stub.destroyed().into_iter().collect();
    assert_eq!(
        destroyed,
        HashSet::from([stray.clone(), org_stray.clone()]),
        "the strays go; recorded boxes and one already gone are left alone"
    );
    for id in [&kept, &elsewhere, &org_kept] {
        assert!(
            s.stub.running().contains(id),
            "{id} is recorded and still runs"
        );
    }
    // What the stray's coworkers are on is their scope's box, whatever their own row says.
    for (member, coworker, box_id) in [
        (&member, &on_stray, &kept),
        (&member, &on_kept, &kept),
        (&org_member, &on_org_stray, &org_kept),
    ] {
        let roster = s.roster(member).await;
        assert_eq!(
            roster.get(coworker.as_str()),
            Some(&Value::String(box_id.clone()))
        );
    }

    assert!(
        repair_stray_boxes(&s.state, true).await.is_empty(),
        "nothing left to find"
    );
    assert_eq!(
        s.stub.destroyed().len(),
        2,
        "a second pass destroys nothing more: {:?}",
        s.stub.destroyed()
    );
}

/// By default the repair only reports: the boot reads `OG_REPAIR_STRAY_BOXES` through
/// `repair_destroys`, which destroys on the one word `destroy` and on nothing else, unset included.
#[tokio::test]
async fn by_default_the_repair_reports_a_stray_and_destroys_nothing() {
    assert!(!repair_destroys(None), "unset reports");
    for word in ["", "1", "true", "yes", "DESTROY", " destroy", "destroy "] {
        assert!(!repair_destroys(Some(word)), "{word:?} reports");
    }
    assert!(repair_destroys(Some("destroy")));

    let database_url = database_or_skip!();
    let s = server(&database_url, Arc::new(Gated::default())).await;
    let (kept, stray) = (
        format!("bx_{}", uuid::Uuid::now_v7().simple()),
        format!("bx_{}", uuid::Uuid::now_v7().simple()),
    );
    s.stub.made(&kept);
    s.stub.made(&stray);
    let member = s.member(None).await;
    s.share("account", member.id.as_str(), "per-account").await;
    s.store()
        .set_scoped_computer(
            "account",
            member.id.as_str(),
            &kept,
            "local-docker",
            None,
            1,
        )
        .await
        .expect("record");
    s.coworker(&member.id, Some(&stray)).await;

    for _boot in 0..2 {
        let found = repair_stray_boxes(&s.state, repair_destroys(None)).await;
        assert_eq!(found, vec![stray.clone()], "each boot reports the stray");
        assert!(s.stub.destroyed().is_empty(), "and destroys nothing");
        assert!(s.stub.running().contains(&stray));
    }
    let found = repair_stray_boxes(&s.state, repair_destroys(Some("destroy"))).await;
    assert_eq!(found, vec![stray.clone()]);
    assert_eq!(s.stub.destroyed(), vec![stray], "only the word destroys");
}

/// A box recorded only on a coworker's row is never destroyed, even told to destroy: a dedicated
/// box hired before 31 Aug 2026 (`817ec33`), in an account with no computer row or in one that has
/// since been given one; a shared box in an account with no computer row; and a stray that a
/// dedicated coworker also names.
#[tokio::test]
async fn a_box_recorded_only_on_a_coworkers_row_is_never_destroyed() {
    let database_url = database_or_skip!();
    let s = server(&database_url, Arc::new(Gated::default())).await;
    let unique = || format!("bx_{}", uuid::Uuid::now_v7().simple());
    let (kept, old_dedicated, later_dedicated, unscoped_shared, also_dedicated) =
        (unique(), unique(), unique(), unique(), unique());
    for id in [
        &kept,
        &old_dedicated,
        &later_dedicated,
        &unscoped_shared,
        &also_dedicated,
    ] {
        s.stub.made(id);
    }

    let unscoped = s.member(None).await;
    s.share("account", unscoped.id.as_str(), "per-account")
        .await;
    s.hired(&unscoped.id, Some((&old_dedicated, BoxMode::Dedicated)))
        .await;
    s.coworker(&unscoped.id, Some(&unscoped_shared)).await;

    let scoped = s.member(None).await;
    s.share("account", scoped.id.as_str(), "per-account").await;
    s.store()
        .set_scoped_computer(
            "account",
            scoped.id.as_str(),
            &kept,
            "local-docker",
            None,
            1,
        )
        .await
        .expect("record");
    s.hired(&scoped.id, Some((&later_dedicated, BoxMode::Dedicated)))
        .await;
    s.coworker(&scoped.id, Some(&also_dedicated)).await;
    s.hired(&scoped.id, Some((&also_dedicated, BoxMode::Dedicated)))
        .await;

    let found = repair_stray_boxes(&s.state, true).await;

    assert!(found.is_empty(), "none of them is #302's stray: {found:?}");
    assert!(s.stub.destroyed().is_empty(), "{:?}", s.stub.destroyed());
    for id in [
        &kept,
        &old_dedicated,
        &later_dedicated,
        &unscoped_shared,
        &also_dedicated,
    ] {
        assert!(s.stub.running().contains(id), "{id} still runs");
    }
}
