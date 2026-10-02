//! The boot pass on seeded rows (#302): it reports a stray box unless told to destroy it, destroys
//! only #302's strays when told, and never touches a box recorded only on a coworker's row, which
//! is how a box hired before 31 Aug 2026 was kept. Needs Postgres; skips loudly without
//! OG_DATABASE_URL. And what boot says about the Local VM image (#301), which needs neither.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opengrok_box::{BoxError, BoxResult, CommandOutput, Computer, StartedCommand};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{BoxMode, Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, BoxId, CoworkerId};
use opengrok_harness::MockDoor;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::provision::live_boxes;
use opengrok_server::auth::{AuthState, TokenMinter};
use opengrok_server::connections::routes::Connectors;
use opengrok_store::PgStore;

use super::{destroys, say_image, stray_boxes};

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

/// A Local VM provider that knows which boxes it still has and remembers what it destroyed.
#[derive(Default)]
struct Boxes {
    live: Mutex<HashSet<String>>,
    destroyed: Mutex<Vec<String>>,
}

impl Boxes {
    fn running(&self) -> HashSet<String> {
        self.live.lock().unwrap().clone()
    }

    fn destroyed(&self) -> Vec<String> {
        self.destroyed.lock().unwrap().clone()
    }
}

#[async_trait]
impl Computer for Boxes {
    async fn create(&self, _ttl_seconds: Option<u64>) -> BoxResult<String> {
        let id = format!("bx_{}", uuid::Uuid::now_v7().simple());
        self.live.lock().unwrap().insert(id.clone());
        Ok(id)
    }
    async fn run(&self, _box_id: &str, _command: &str, _timeout: u32) -> BoxResult<CommandOutput> {
        Err(BoxError::NoSuchBox)
    }
    async fn start(&self, _box_id: &str, _command: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn watch(&self, _box_id: &str, _process_id: &str) -> BoxResult<StartedCommand> {
        Err(BoxError::NoSuchBox)
    }
    async fn read_file(&self, _box_id: &str, _path: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
    }
    async fn write_file(&self, _box_id: &str, _path: &str, _content: &str) -> BoxResult<()> {
        Err(BoxError::NoSuchBox)
    }
    async fn expose_port(&self, _box_id: &str, _port: u16, _title: &str) -> BoxResult<String> {
        Err(BoxError::NoSuchBox)
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
        self.destroyed.lock().unwrap().push(box_id.to_string());
        Ok(())
    }
    async fn state(&self, box_id: &str) -> BoxResult<String> {
        let running = self.live.lock().unwrap().contains(box_id);
        Ok(if running { "running" } else { "absent" }.to_string())
    }
}

struct Harness {
    state: AgUiState,
    boxes: Arc<Boxes>,
}

async fn harness(database_url: &str) -> Harness {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let boxes = Arc::new(Boxes::default());
    let state = AgUiState {
        auth: AuthState::new(
            PgStore::new(pool),
            Arc::new(TokenMinter::new(b"stray-boxes-repair-test-secret!")),
            "host@og.local".to_string(),
        ),
        door: Arc::new(MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        // The deployment's own Local VM provider: every seeded box is one of its.
        computer: Some(boxes.clone()),
        vault: None,
        connectors: Connectors {
            providers: Arc::new(BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: Arc::new(BTreeMap::new()),
        host_settings: None,
    };
    Harness { state, boxes }
}

fn unique() -> String {
    format!("bx_{}", uuid::Uuid::now_v7().simple())
}

impl Harness {
    fn store(&self) -> &PgStore {
        &self.state.auth.store
    }

    /// Boxes the provider has, running.
    fn made(&self, ids: &[&String]) {
        let mut live = self.boxes.live.lock().unwrap();
        live.extend(ids.iter().map(|id| (*id).clone()));
    }

    /// A member, under `org` when given; one with no org is put on per-account, so the test
    /// does not depend on `OG_BOX_SHARE`.
    async fn member(&self, org: Option<&str>) -> AccountId {
        let id = AccountId::new();
        let email = format!("stray-{}@og.local", uuid::Uuid::now_v7().simple());
        let at_ms = chrono::Utc::now().timestamp_millis();
        let events = Account::default()
            .decide(AccountCommand::Register {
                email: email.clone(),
                password_hash: "x".to_string(),
                first_name: "Stray".to_string(),
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
            email,
            plan: Plan::Ultra,
            trial: false,
            updated_at_ms: at_ms,
            password_hash: Some("x".to_string()),
            first_name: "Stray".to_string(),
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
        if org.is_none() {
            self.share("account", id.as_str(), "per-account").await;
        }
        id
    }

    async fn share(&self, scope: &str, scope_id: &str, mode: &str) {
        self.store()
            .set_sharing_mode(scope, scope_id, mode, 1)
            .await
            .expect("sharing mode");
    }

    /// The scope's record of its box, as scope provisioning writes it.
    async fn record(&self, scope: &str, scope_id: &str, box_id: &str, org: Option<&str>) {
        self.store()
            .set_scoped_computer(scope, scope_id, box_id, "local-docker", org, 1)
            .await
            .expect("record the scope's box");
    }

    /// A coworker of `account` given `box_id` as its first box, in `mode`.
    async fn coworker(&self, account: &AccountId, box_id: &str, mode: BoxMode) -> CoworkerId {
        let id = CoworkerId::new();
        let mut coworker = Coworker::default();
        let mut events = coworker
            .decide(CoworkerCommand::Hire {
                name: "Stray".to_string(),
                model: "oag/cheap".to_string(),
                at_ms: 1,
            })
            .expect("hire");
        for event in &events {
            coworker.apply(event);
        }
        let assigned = coworker
            .decide(CoworkerCommand::AssignComputer {
                box_id: BoxId::from_stored(box_id.to_string()),
                mode,
                at_ms: 1,
            })
            .expect("assign");
        for event in &assigned {
            coworker.apply(event);
        }
        events.extend(assigned);
        let view = CoworkerView::of(id.clone(), &coworker, 1);
        self.store()
            .append_coworker(&id, account, 0, &events, &view)
            .await
            .expect("append coworker");
        id
    }

    /// The box a coworker is on, as the roster and every turn find it: its scope's record.
    async fn on(&self, account: &AccountId, coworker: &CoworkerId) -> Option<String> {
        let (loaded, _) = self.store().load_coworker(coworker).await.expect("load");
        let view = CoworkerView::of(coworker.clone(), &loaded, 1);
        live_boxes(&self.state, account, &[&view])
            .await
            .remove(coworker)
    }
}

/// By default the pass only reports: the boot reads `OG_REPAIR_STRAY_BOXES` through `destroys`,
/// which destroys on the one word `destroy` and on nothing else, unset included.
#[tokio::test]
async fn by_default_the_pass_reports_a_stray_and_destroys_nothing() {
    assert!(!destroys(None), "unset reports");
    for word in ["", "1", "true", "yes", "DESTROY", " destroy", "destroy "] {
        assert!(!destroys(Some(word)), "{word:?} reports");
    }
    assert!(destroys(Some("destroy")));

    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (kept, stray) = (unique(), unique());
    h.made(&[&kept, &stray]);
    let member = h.member(None).await;
    h.record("account", member.as_str(), &kept, None).await;
    h.coworker(&member, &stray, BoxMode::Shared).await;

    for _boot in 0..2 {
        let found = stray_boxes(&h.state, destroys(None)).await;
        assert_eq!(found, vec![stray.clone()], "each boot reports the stray");
        assert!(h.boxes.destroyed().is_empty(), "and destroys nothing");
        assert!(h.boxes.running().contains(&stray));
    }
    let found = stray_boxes(&h.state, destroys(Some("destroy"))).await;
    assert_eq!(found, vec![stray.clone()]);
    assert_eq!(h.boxes.destroyed(), vec![stray], "only the word destroys");
}

/// Told to destroy, the pass destroys a stray box (one no record names, whose coworkers are on a
/// shared box of an account or org that records another) on its scope's provider; a box a record
/// names is never touched, nor is one already gone; and a second pass changes nothing.
#[tokio::test]
async fn told_to_destroy_the_pass_destroys_a_stray_and_never_a_recorded_box() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (kept, stray, gone, elsewhere, org_kept, org_stray) =
        (unique(), unique(), unique(), unique(), unique(), unique());
    h.made(&[&kept, &stray, &elsewhere, &org_kept, &org_stray]);

    // An account whose box is `kept`: one coworker on it, one left naming `stray` (the race's
    // loser), one naming `gone` (a box a reset destroyed), one naming `elsewhere`, which is
    // another account's recorded box.
    let member = h.member(None).await;
    let other = h.member(None).await;
    h.record("account", member.as_str(), &kept, None).await;
    h.record("account", other.as_str(), &elsewhere, None).await;
    let on_kept = h.coworker(&member, &kept, BoxMode::Shared).await;
    let on_stray = h.coworker(&member, &stray, BoxMode::Shared).await;
    h.coworker(&member, &gone, BoxMode::Shared).await;
    h.coworker(&member, &elsewhere, BoxMode::Shared).await;

    // The same shape on an org's shared box.
    let org = format!("org_{}", uuid::Uuid::now_v7().simple());
    h.share("org", &org, "per-org").await;
    let org_member = h.member(Some(&org)).await;
    h.record("org", &org, &org_kept, Some(&org)).await;
    h.coworker(&org_member, &org_kept, BoxMode::Shared).await;
    let on_org_stray = h.coworker(&org_member, &org_stray, BoxMode::Shared).await;

    let found: HashSet<String> = stray_boxes(&h.state, true).await.into_iter().collect();

    assert_eq!(found, HashSet::from([stray.clone(), org_stray.clone()]));
    let destroyed: HashSet<String> = h.boxes.destroyed().into_iter().collect();
    assert_eq!(
        destroyed,
        HashSet::from([stray.clone(), org_stray.clone()]),
        "the strays go; recorded boxes and one already gone are left alone"
    );
    for id in [&kept, &elsewhere, &org_kept] {
        assert!(
            h.boxes.running().contains(id),
            "{id} is recorded and still runs"
        );
    }
    // What the stray's coworkers are on is their scope's box, whatever their own row says.
    assert_eq!(h.on(&member, &on_stray).await, Some(kept.clone()));
    assert_eq!(h.on(&member, &on_kept).await, Some(kept.clone()));
    assert_eq!(h.on(&org_member, &on_org_stray).await, Some(org_kept));

    assert!(
        stray_boxes(&h.state, true).await.is_empty(),
        "nothing left to find"
    );
    assert_eq!(
        h.boxes.destroyed().len(),
        2,
        "a second pass destroys nothing"
    );
}

/// A box recorded only on a coworker's row is never destroyed, even told to: a dedicated box
/// hired before 31 Aug 2026 (`817ec33`), in an account with no computer row or in one that has
/// since been given one; a shared box in an account with no computer row; and a stray that a
/// dedicated coworker also names.
#[tokio::test]
async fn a_box_recorded_only_on_a_coworkers_row_is_never_destroyed() {
    let database_url = database_or_skip!();
    let h = harness(&database_url).await;
    let (kept, old_dedicated, later_dedicated, unscoped_shared, also_dedicated) =
        (unique(), unique(), unique(), unique(), unique());
    let every = [
        &kept,
        &old_dedicated,
        &later_dedicated,
        &unscoped_shared,
        &also_dedicated,
    ];
    h.made(&every);

    let unscoped = h.member(None).await;
    h.coworker(&unscoped, &old_dedicated, BoxMode::Dedicated)
        .await;
    h.coworker(&unscoped, &unscoped_shared, BoxMode::Shared)
        .await;

    let scoped = h.member(None).await;
    h.record("account", scoped.as_str(), &kept, None).await;
    h.coworker(&scoped, &later_dedicated, BoxMode::Dedicated)
        .await;
    h.coworker(&scoped, &also_dedicated, BoxMode::Shared).await;
    h.coworker(&scoped, &also_dedicated, BoxMode::Dedicated)
        .await;

    let found = stray_boxes(&h.state, true).await;

    assert!(found.is_empty(), "none of them is #302's stray: {found:?}");
    assert!(h.boxes.destroyed().is_empty(), "{:?}", h.boxes.destroyed());
    for id in every {
        assert!(h.boxes.running().contains(id), "{id} still runs");
    }
}

/// What `say` logs, as the boot's own formatter writes it, without colour.
fn logged(say: impl FnOnce()) -> String {
    let written = Log::default();
    let sink = written.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, say);
    String::from_utf8_lossy(&written.0.lock().unwrap()).into_owned()
}

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Log {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A `:local` image this host never built is said once, at warn, with the command that builds
/// it (#301). Until then the first sign was a refused hire.
#[test]
fn a_local_image_never_built_is_warned_about_with_the_command_that_builds_it() {
    let said = logged(|| say_image("grok-box:local", Ok(false)));
    assert_eq!(said.lines().count(), 1, "{said}");
    assert!(said.contains(" WARN "), "{said}");
    assert!(said.contains("hexuria/box"), "{said}");
    let build = "docker build -f docker/Dockerfile -t grok-box:local .";
    assert!(said.contains(build), "{said}");
}

/// A Docker that cannot answer is warned about too: no Local VM can be made without it.
#[test]
fn a_docker_that_cannot_answer_is_warned_about() {
    let down = BoxError::Unreachable("could not run docker: not found".to_string());
    let said = logged(|| say_image("grok-box:local", Err(down)));
    assert!(
        said.contains(" WARN ") && said.contains("not found"),
        "{said}"
    );
}

/// An image Docker has says nothing, and one it can pull is no fault: the first Local VM pulls it.
#[test]
fn an_image_docker_has_or_can_pull_is_not_warned_about() {
    assert_eq!(logged(|| say_image("grok-box:local", Ok(true))), "");
    let pulled = logged(|| say_image("debian:stable-slim", Ok(false)));
    assert!(
        !pulled.contains(" WARN ") && pulled.contains("pulls it"),
        "{pulled}"
    );
}
