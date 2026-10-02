//! The one-time pin (#318) on seeded bots: each with no source is pinned to the door its owner's
//! setting gave it, in real coworker events and at its roster stamp; a source somebody chose is
//! never touched; and once the pass has run it never runs again, so a bot hired afterwards with
//! none, or handed back to none, keeps none. Needs Postgres; skips loudly without OG_DATABASE_URL.

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView, Effort, Visibility};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_core::inference::{InferenceSource, SourceKind, Via};
use opengrok_store::PgStore;

use super::{door, pin_once};

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

/// The roster stamp every seeded bot carries: the sidebar's sort key, which a pin must not move.
const STAMP: i64 = 1_000;

const PIN: &str = "xai/grok-4.6";

async fn store(database_url: &str) -> PgStore {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    PgStore::new(pool)
}

/// THE TWO PASSES HERE TAKE TURNS. A pass pins every bot in the database, so one test's pass could
/// pin the bot the other hires after its own pass to show that a second pass leaves it alone. The
/// lock is this session's, and dropping the connection, a failed assertion included, ends it.
async fn one_at_a_time(store: &PgStore) -> sqlx::PgConnection {
    let mut held = store.pool().acquire().await.expect("connect").detach();
    sqlx::query("select pg_advisory_lock(318318)")
        .execute(&mut held)
        .await
        .expect("take turns");
    held
}

/// A setting on the person's own plan by the loopback, answered by `model`.
fn on_the_plan(model: &str) -> InferenceSource {
    InferenceSource {
        kind: SourceKind::LocalProxy,
        base_url: Some("http://127.0.0.1:9".to_string()),
        local_model: Some(model.to_string()),
        ..InferenceSource::default()
    }
}

/// A person with `setting` in their log, as `PUT /account/inference-source` writes it, or none.
async fn person(store: &PgStore, setting: Option<InferenceSource>) -> AccountId {
    let id = AccountId::new();
    let email = format!("pin-{}@og.local", uuid::Uuid::now_v7().simple());
    let register = AccountCommand::Register {
        email: email.clone(),
        password_hash: "x".to_string(),
        first_name: "Pin".to_string(),
        last_name: String::new(),
        org_id: String::new(),
        plan: Plan::Ultra,
        verified: true,
        enabled: true,
        at_ms: 1,
    };
    let mut events = Account::default().decide(register).expect("register");
    if let Some(source) = setting {
        let set = AccountCommand::SetInferenceSource { source, at_ms: 2 };
        events.extend(Account::replay(&events).decide(set).expect("a setting"));
    }
    let view = AccountView::session_only(id.clone(), email, Plan::Ultra, false, 1);
    store
        .append_account(&id, 0, &events, &view)
        .await
        .expect("append account");
    id
}

/// `owner`'s bot on `model`, with the `source` and `effort` its owner gave it: no `SourceSet` at
/// all for none, as every bot hired before a bot had a door of its own.
async fn bot(
    store: &PgStore,
    owner: &AccountId,
    model: &str,
    source: Option<SourceKind>,
    effort: Effort,
) -> CoworkerId {
    let id = CoworkerId::new();
    let mut bot = Coworker::default();
    let hire = CoworkerCommand::Hire {
        name: "Pinned".to_string(),
        model: model.to_string(),
        at_ms: 1,
    };
    let mut events = bot.decide(hire).expect("hire");
    for event in &events {
        bot.apply(event);
    }
    let mut commands = vec![CoworkerCommand::SetEffort { effort, at_ms: 1 }];
    if source.is_some() {
        commands.push(CoworkerCommand::SetSource { source, at_ms: 1 });
    }
    for command in commands {
        let more = bot.decide(command).expect("decide");
        for event in &more {
            bot.apply(event);
        }
        events.extend(more);
    }
    let view = CoworkerView::of(id.clone(), &bot, STAMP);
    store
        .append_coworker(&id, owner, 0, &events, &view)
        .await
        .expect("append coworker");
    id
}

/// Decide `command` on `id` as its owner and write it.
async fn decide(store: &PgStore, owner: &AccountId, id: &CoworkerId, command: CoworkerCommand) {
    let (mut bot, seq) = store.load_coworker(id).await.expect("load");
    let events = bot.decide(command).expect("decide");
    for event in &events {
        bot.apply(event);
    }
    let view = CoworkerView::of(id.clone(), &bot, STAMP);
    store
        .append_coworker(id, owner, seq, &events, &view)
        .await
        .expect("append");
}

/// A bot as its log has it: its door, its pin, how hard it thinks, and how many events it has.
async fn read(store: &PgStore, id: &CoworkerId) -> (Option<SourceKind>, String, Effort, i64) {
    let (bot, seq) = store.load_coworker(id).await.expect("load");
    (bot.source, bot.model, bot.effort, seq)
}

fn marker() -> String {
    format!("pin-test-{}", uuid::Uuid::now_v7().simple())
}

/// The door is the owner's setting resolved as a turn that picks nothing: the gateway on the bot's
/// own pin, or the plan on the setting's model for the way its turns went. A plan model that
/// cannot be pinned (none for that way, or one the allowlist refuses) is the gateway on its pin.
#[test]
fn a_bots_door_is_its_owners_setting_resolved_as_a_turn_that_picks_nothing() {
    use SourceKind::{Gateway, LocalProxy};
    assert_eq!(
        door(&InferenceSource::default(), PIN),
        (Gateway, PIN.to_string(), None),
        "no setting is the gateway, on the pin it has"
    );
    let gateway_with_a_model = InferenceSource {
        kind: Gateway,
        ..on_the_plan("gpt-5.5")
    };
    assert_eq!(door(&gateway_with_a_model, PIN).0, Gateway);
    assert_eq!(
        door(&on_the_plan(" gpt-5.5 "), PIN),
        (LocalProxy, "gpt-5.5".to_string(), None)
    );
    let by_mac = InferenceSource {
        via: Some(Via::Mac),
        relay_model: Some("gpt-6-sol--fast".to_string()),
        ..on_the_plan("gpt-5.5")
    };
    assert_eq!(
        door(&by_mac, PIN),
        (LocalProxy, "gpt-6-sol--fast".to_string(), None),
        "by the Mac, the Mac's own model"
    );
    let mac_with_none = InferenceSource {
        relay_model: None,
        ..by_mac
    };
    let mut unpinnable = vec![mac_with_none];
    for model in ["my-codex-thing", "claude-sonnet-4.5", ""] {
        unpinnable.push(on_the_plan(model));
    }
    for setting in unpinnable {
        let (source, model, why) = door(&setting, PIN);
        assert_eq!((source, model.as_str()), (Gateway, PIN), "{setting:?}");
        assert!(why.is_some_and(|why| !why.is_empty()), "{setting:?}");
    }
}

#[tokio::test]
async fn each_bot_with_no_source_is_pinned_once_to_the_door_its_owners_setting_gave_it() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let _turn = one_at_a_time(&store).await;
    let on_gateway = person(&store, None).await;
    let on_plan = person(&store, Some(on_the_plan("gpt-5.5"))).await;
    let by_mac = InferenceSource {
        via: Some(Via::Mac),
        relay_model: Some("gpt-6-sol--fast".to_string()),
        ..on_the_plan("gpt-5.5")
    };
    let by_mac = person(&store, Some(by_mac)).await;
    // Saved before #296 anchored the allowlist, which no longer takes it.
    let unpinnable = person(&store, Some(on_the_plan("my-codex-thing"))).await;

    let gateway = bot(&store, &on_gateway, PIN, None, Effort::High).await;
    let plan = bot(&store, &on_plan, PIN, None, Effort::Low).await;
    let mac = bot(&store, &by_mac, PIN, None, Effort::Inherit).await;
    let refused = bot(&store, &unpinnable, "xai/grok-4.6@sub", None, Effort::Max).await;
    // Shared with its owner's org: pinned by its owner's setting, as every bot is.
    let shared = bot(&store, &on_plan, PIN, None, Effort::Inherit).await;
    let org = CoworkerCommand::SetVisibility {
        visibility: Visibility::Org,
        at_ms: 2,
    };
    decide(&store, &on_plan, &shared, org).await;

    let pinned = pin_once(&store, &marker()).await.expect("the pass");
    assert!(pinned.is_some_and(|pinned| pinned >= 5), "{pinned:?}");

    use SourceKind::{Gateway, LocalProxy};
    let (source, model, effort, _) = read(&store, &gateway).await;
    assert_eq!(
        (source, model.as_str(), effort),
        (Some(Gateway), PIN, Effort::High)
    );
    let (source, model, effort, _) = read(&store, &plan).await;
    assert_eq!(
        (source, model.as_str(), effort),
        (Some(LocalProxy), "gpt-5.5", Effort::Low),
        "its owner's own plan, on the model that plan answered it with"
    );
    let (source, model, _, _) = read(&store, &mac).await;
    assert_eq!(
        (source, model.as_str()),
        (Some(LocalProxy), "gpt-6-sol--fast")
    );
    let (source, model, effort, _) = read(&store, &refused).await;
    assert_eq!(
        (source, model.as_str(), effort),
        (Some(Gateway), "xai/grok-4.6@sub", Effort::Max),
        "a plan model the allowlist refuses is never pinned: the gateway, on its own pin"
    );
    let (source, model, _, _) = read(&store, &shared).await;
    assert_eq!((source, model.as_str()), (Some(LocalProxy), "gpt-5.5"));

    // Real events, the ones an owner's PATCH writes, so the aggregate stays the truth.
    let logged: Vec<String> = sqlx::query_scalar(
        "select event_type from events where stream_id = $1 order by stream_seq",
    )
    .bind(opengrok_store::coworker_stream(&plan))
    .fetch_all(store.pool())
    .await
    .expect("the bot's log");
    assert_eq!(
        logged,
        [
            "coworker-hired",
            "coworker-effort-set",
            "coworker-repinned",
            "coworker-source-set"
        ]
    );
    // The roster reads the pin, and lists the bot where it was.
    let roster = store.coworkers_for(&on_plan).await.expect("roster");
    let row = roster.iter().find(|row| row.id == plan).expect("listed");
    assert_eq!(
        (row.source, row.model.as_str()),
        (Some(LocalProxy), "gpt-5.5")
    );
    assert_eq!(
        row.updated_at_ms, STAMP,
        "a pin does not move a bot in the sidebar"
    );
}

#[tokio::test]
async fn the_pin_leaves_a_chosen_source_alone_and_never_runs_twice() {
    let database_url = database_or_skip!();
    let store = store(&database_url).await;
    let _turn = one_at_a_time(&store).await;
    let owner = person(&store, Some(on_the_plan("gpt-5.5"))).await;
    let chose_gateway = bot(
        &store,
        &owner,
        PIN,
        Some(SourceKind::Gateway),
        Effort::Inherit,
    )
    .await;
    let local = Some(SourceKind::LocalProxy);
    let chose_plan = bot(&store, &owner, "gpt-6-luna", local, Effort::High).await;
    let unset = bot(&store, &owner, PIN, None, Effort::Inherit).await;
    let retired = bot(&store, &owner, PIN, None, Effort::Inherit).await;
    decide(
        &store,
        &owner,
        &retired,
        CoworkerCommand::Retire { at_ms: 2 },
    )
    .await;
    // A group takes no model call of its own, so it has no door to pin, and its sentinel stays.
    let group = CoworkerId::new();
    let hire = CoworkerCommand::HireGroup {
        name: "Pair".to_string(),
        members: vec![unset.clone()],
        at_ms: 1,
    };
    let events = Coworker::default().decide(hire).expect("a group");
    let view = CoworkerView::of(group.clone(), &Coworker::replay(&events), STAMP);
    store
        .append_coworker(&group, &owner, 0, &events, &view)
        .await
        .expect("append group");
    let untouched = [&chose_gateway, &chose_plan, &retired, &group];
    let mut before = Vec::new();
    for id in untouched {
        before.push(read(&store, id).await);
    }

    let marker = marker();
    assert!(pin_once(&store, &marker).await.expect("the pass").is_some());
    let mut after = Vec::new();
    for id in untouched {
        after.push(read(&store, id).await);
    }
    assert_eq!(
        after, before,
        "a chosen source, a retired bot and a group are left alone"
    );
    let (source, model, _, _) = read(&store, &unset).await;
    assert_eq!(
        (source, model.as_str()),
        (Some(SourceKind::LocalProxy), "gpt-5.5")
    );

    // After the pass, a bot hired on a model of its own has no source, and so does one its owner
    // hands back to their setting. The pass ran; running again must not take either choice away.
    let hired_later = bot(&store, &owner, PIN, None, Effort::Inherit).await;
    let hand_back = CoworkerCommand::SetSource {
        source: None,
        at_ms: 3,
    };
    decide(&store, &owner, &unset, hand_back).await;
    let every = [
        &chose_gateway,
        &chose_plan,
        &retired,
        &group,
        &unset,
        &hired_later,
    ];
    let mut once = Vec::new();
    for id in every {
        once.push(read(&store, id).await);
    }
    assert_eq!(
        pin_once(&store, &marker).await.expect("the second boot"),
        None,
        "its row says it ran"
    );
    let mut twice = Vec::new();
    for id in every {
        twice.push(read(&store, id).await);
    }
    assert_eq!(twice, once, "a second run changes nothing");
    assert_eq!(twice[4].0, None, "handed back to none, and kept there");
    assert_eq!(
        twice[5].0, None,
        "hired after the pass with none, and kept there"
    );
}
