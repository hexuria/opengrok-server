//! `purge_accounts_except` on a real Postgres: two accounts with a coworker, a run with frames,
//! a schedule, a gateway entry and an org each; keep one; the other is gone from every table,
//! the kept one is untouched, and an allowlist that names nobody deletes nothing.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.
#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
use opengrok_core::id::{AccountId, CoworkerId, OrgId, RunId, ScheduleId};
use opengrok_core::org::{Org, OrgCommand};
use opengrok_core::run::{Run, RunCommand, RunView};
use opengrok_core::schedule::{Schedule, ScheduleCommand, Wake};
use opengrok_server::auth::password::hash_password;
use opengrok_store::PgStore;
use serde_json::json;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn connect() -> Option<PgStore> {
    let database_url =
        opengrok_store::gate_database_or_panic(std::env::var("OG_DATABASE_URL").ok()?);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    Some(PgStore::new(pool))
}

struct Seeded {
    account: AccountId,
    org: OrgId,
    coworker: CoworkerId,
    run: RunId,
    schedule: ScheduleId,
}

/// One account with everything the purge has to reach: its org, a coworker, a run with frames
/// on that coworker's thread, a schedule, and a gateway entry.
async fn seed(store: &PgStore, email: &str) -> Seeded {
    let at_ms = now_ms();
    let org = OrgId::new();
    let account = AccountId::new();

    let hash = hash_password("password1").expect("hash");
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: "Purge".to_string(),
            last_name: "Fixture".to_string(),
            org_id: org.to_string(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: account.clone(),
        email: email.to_string(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(hash),
        first_name: "Purge".to_string(),
        last_name: "Fixture".to_string(),
        org_id: Some(org.to_string()),
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&account, 0, &events, &view)
        .await
        .expect("append account");

    let domain = email.rsplit('@').next().expect("domain").to_string();
    let events = Org::default()
        .decide(OrgCommand::Create {
            name: format!("org of {email}"),
            admin: account.clone(),
            domains: vec![domain],
            at_ms,
        })
        .expect("create org");
    let state = Org::replay(&events);
    store
        .append_org(&org, 0, &events, &state, at_ms)
        .await
        .expect("append org");

    let coworker = CoworkerId::new();
    let mut hired = Coworker::default();
    let events = hired
        .decide(CoworkerCommand::Hire {
            name: "Fixture".to_string(),
            model: "oag/cheap".to_string(),
            at_ms,
        })
        .expect("hire");
    for event in &events {
        hired.apply(event);
    }
    let view = CoworkerView {
        id: coworker.clone(),
        name: hired.name.clone(),
        model: hired.model.clone(),
        box_id: None,
        retired: false,
        members: Vec::new(),
        updated_at_ms: at_ms,
        role: None,
        visibility: Default::default(),
    };
    store
        .append_coworker(&coworker, &account, 0, &events, &view)
        .await
        .expect("append coworker");

    let run = RunId::new();
    let mut started = Run::default();
    let events = started
        .decide(RunCommand::Start {
            thread_id: coworker.to_string(),
            coworker_id: Some(coworker.clone()),
            model: Some("oag/cheap".to_string()),
            system: None,
            at_ms,
        })
        .expect("start");
    for event in &events {
        started.apply(event);
    }
    let view = RunView {
        id: run.clone(),
        thread_id: coworker.to_string(),
        status: started.status,
        event_count: 0,
        updated_at_ms: at_ms,
    };
    store
        .append_run(&run, 0, &events, &view, Some(&account))
        .await
        .expect("append run");

    let schedule = ScheduleId::new();
    let events = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: coworker.clone(),
            prompt: "check the queue".to_string(),
            name: "fixture".to_string(),
            wake: Wake::Cron {
                cron: "0 */15 * * * *".to_string(),
            },
            at_ms,
        })
        .expect("create schedule");
    let state = Schedule::replay(&events);
    store
        .append_schedule(&schedule, &account, 0, &events, &state, at_ms)
        .await
        .expect("append schedule");

    store
        .append_gateway_entry(
            &coworker,
            &account,
            &json!({ "id": "e_fixture", "message": { "type": "text", "text": "hello" } }),
            at_ms,
        )
        .await
        .expect("append gateway entry");

    // A saved site login: the row, and the password sealed under a key that carries the
    // account id.
    let vault =
        opengrok_store::Vault::from_base64_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .expect("vault");
    store
        .upsert_site_login(
            &vault,
            &account,
            &opengrok_store::SiteLoginWrite {
                origin: "example.com",
                username: "ada",
                label: "",
                kind: "password",
                notes: "",
                password: Some("pw"),
                otpauth: Some("otpauth://totp/x?secret=ABCDEFGHIJKLMNOP"),
            },
            at_ms,
        )
        .await
        .expect("site login");

    Seeded {
        account,
        org,
        coworker,
        run,
        schedule,
    }
}

/// Rows that name this account, org, coworker, run or schedule, table by table.
async fn footprint(store: &PgStore, seeded: &Seeded) -> Vec<(&'static str, i64)> {
    let account = seeded.account.to_string();
    let org = seeded.org.to_string();
    let coworker = seeded.coworker.to_string();
    let run = seeded.run.to_string();
    let schedule = seeded.schedule.to_string();
    let streams = vec![
        format!("account/{account}"),
        format!("org/{org}"),
        format!("coworker/{coworker}"),
        format!("run/{run}"),
        format!("schedule/{schedule}"),
    ];
    let mut out = Vec::new();
    for (table, sql, key) in [
        (
            "account_view",
            "select count(*) from account_view where id = $1",
            &account,
        ),
        (
            "org_view",
            "select count(*) from org_view where id = $1",
            &org,
        ),
        (
            "coworker_view",
            "select count(*) from coworker_view where id = $1",
            &coworker,
        ),
        (
            "run_view",
            "select count(*) from run_view where id = $1",
            &run,
        ),
        (
            "schedule_view",
            "select count(*) from schedule_view where id = $1",
            &schedule,
        ),
        (
            "gateway_entry",
            "select count(*) from gateway_entry where coworker_id = $1",
            &coworker,
        ),
        (
            "site_login",
            "select count(*) from site_login where account_id = $1",
            &account,
        ),
        (
            "secret_store (site logins)",
            "select count(*) from secret_store where id like 'site-login:' || $1 || ':%' or id like 'site-login-otp:' || $1 || ':%'",
            &account,
        ),
    ] {
        let count: i64 = sqlx::query_scalar(sql)
            .bind(key)
            .fetch_one(store.pool())
            .await
            .expect("count");
        out.push((table, count));
    }
    let events: i64 = sqlx::query_scalar("select count(*) from events where stream_id = any($1)")
        .bind(&streams)
        .fetch_one(store.pool())
        .await
        .expect("count events");
    out.push(("events", events));
    out
}

/// Everyone the database already holds is on the allowlist, plus the account seeded to be kept;
/// only the account seeded to go is off it. So the purge deletes exactly one account, whatever
/// else the database holds and whatever runs beside this test.
async fn everyone_but(store: &PgStore, gone: &str) -> Vec<String> {
    sqlx::query_scalar("select email from account_view where email <> $1")
        .bind(gone)
        .fetch_all(store.pool())
        .await
        .expect("emails")
}

/// One test, in order: the refusals and the dry run first, the real purge last.
#[tokio::test]
async fn everyone_but_the_allowlist_goes_and_the_allowlist_keeps_everything() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let kept_email = format!("purge-keep-{stamp}@og.local");
    let gone_email = format!("purge-gone-{stamp}@og.local");
    let kept = seed(&store, &kept_email).await;
    let gone = seed(&store, &gone_email).await;

    let before_kept = footprint(&store, &kept).await;
    let before_gone = footprint(&store, &gone).await;
    for (table, count) in before_gone.iter().chain(before_kept.iter()) {
        assert!(*count >= 1, "seeding left nothing in {table}");
    }

    // An allowlist that names nobody refuses before touching a row: empty, or a typo.
    let empty = store.purge_accounts_except(&[], false).await;
    assert!(empty.is_err(), "an empty allowlist must refuse");
    let typo = store
        .purge_accounts_except(&[format!("nobody-{stamp}@og.local")], false)
        .await;
    let message = typo.expect_err("an unknown email must refuse").to_string();
    assert!(message.contains("does not exist"), "{message}");
    assert_eq!(
        footprint(&store, &gone).await,
        before_gone,
        "a refused purge deleted rows"
    );

    let keep = everyone_but(&store, &gone_email).await;
    assert!(
        keep.contains(&kept_email),
        "the kept fixture is on the allowlist"
    );

    // A dry run reports the work and changes nothing.
    let rehearsal = store
        .purge_accounts_except(&keep, true)
        .await
        .expect("dry run");
    assert_eq!(rehearsal.accounts_deleted, 1, "{rehearsal:?}");
    assert_eq!(
        footprint(&store, &gone).await,
        before_gone,
        "a dry run deleted rows"
    );

    let report = store
        .purge_accounts_except(&keep, false)
        .await
        .expect("purge");
    assert!(
        report
            .kept
            .contains(&(kept_email.clone(), kept.account.to_string())),
        "{report:?}"
    );
    assert_eq!(report.accounts_deleted, 1, "{report:?}");
    assert_eq!(report.coworkers_deleted, 1, "{report:?}");
    assert_eq!(report.orgs_deleted, 1, "{report:?}");
    assert!(report.rows["events"] >= 5, "{report:?}");

    for (table, count) in footprint(&store, &gone).await {
        assert_eq!(count, 0, "{table} still holds the purged account's rows");
    }
    assert_eq!(
        footprint(&store, &kept).await,
        before_kept,
        "the kept account lost rows"
    );
}
