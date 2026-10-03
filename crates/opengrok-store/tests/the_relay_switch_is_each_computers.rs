//! The relay switch is each computer's (`local_exec_daemon.relay_enabled`), and every machine
//! enrolled before it reads as on. #332 kept one switch per account, in its inference-source
//! events: an account whose last one says off has its machines switched off ONCE, by the boot that
//! adds the column, so a relay its person turned off is not turned back on by an upgrade; and no
//! later replay of the schema switches off a machine they switched on since.
//!
//! Its own test binary, so its own database: it drops the column and makes the schema replay,
//! which takes the table locks a replay takes, and nothing else may be running beside it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_store::PgStore;
use serde_json::json;

/// Replay the schema as the next change to it will, with or without the pass already recorded.
async fn replay(store: &PgStore, forget_the_pass: bool) {
    if forget_the_pass {
        let forget =
            "delete from schema_migrations where name = 'the-relay-switch-is-each-computers'";
        sqlx::query(forget)
            .execute(store.pool())
            .await
            .expect("forget the pass");
    }
    let forget = "delete from schema_applied";
    sqlx::query(forget)
        .execute(store.pool())
        .await
        .expect("forget the schema");
    opengrok_store::migrations::run(store.pool())
        .await
        .expect("replay");
}

/// An account's event as #332 wrote it: its setting whole, the switch logged only when off.
async fn logged(store: &PgStore, account: &str, seq: i64, event: serde_json::Value) {
    let kind = format!("account-{}", event["type"].as_str().unwrap());
    let insert = "insert into events (stream_id, stream_seq, event_type, payload)
                  values ($1, $2, $3, $4)";
    let query = sqlx::query(insert)
        .bind(format!("account/{account}"))
        .bind(seq);
    query
        .bind(kind)
        .bind(event)
        .execute(store.pool())
        .await
        .expect("an event from #332");
}

fn setting(relay_off: bool) -> serde_json::Value {
    let mut source = json!({ "kind": "local_proxy", "via": "mac", "has_key": false });
    if relay_off {
        source["relay_off"] = json!(true);
    }
    json!({ "type": "inference-source-set", "source": source, "at_ms": 1 })
}

/// Whether each machine's relay is on, in the order named.
async fn switches(store: &PgStore, account: &str, machines: &[&str]) -> Vec<bool> {
    let mut said = Vec::new();
    for machine in machines {
        let row = store.daemon_jti(account, machine).await.expect("read");
        said.push(row.expect("enrolled").2);
    }
    said
}

#[tokio::test]
async fn an_account_whose_relay_was_off_has_its_computers_switched_off_once() {
    let url = match std::env::var("OG_DATABASE_URL") {
        Ok(url) => opengrok_store::gate_database_or_panic(url),
        Err(_) => {
            eprintln!("skipping: OG_DATABASE_URL is not set");
            return;
        }
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect");
    opengrok_store::migrations::run(&pool).await.expect("boot");
    let store = PgStore::new(pool);
    let suffix = uuid::Uuid::now_v7().simple().to_string();
    let [off, back_on, untouched] =
        ["off", "on", "never"].map(|who| format!("acct_{who}_{suffix}"));
    // Its last setting says off, with an event of another kind after it.
    logged(&store, &off, 1, setting(true)).await;
    let zone = json!({ "type": "time-zone-set", "time_zone": "Asia/Manila", "at_ms": 2 });
    logged(&store, &off, 2, zone).await;
    // Off, then on again: its last setting logs no switch, which is on.
    logged(&store, &back_on, 1, setting(true)).await;
    logged(&store, &back_on, 2, setting(false)).await;
    for (account, machine) in [(&off, "mac-a"), (&off, "mac-b"), (&back_on, "mac-a")] {
        store
            .enrol_daemon(account, machine, "Mac", "jti", 1)
            .await
            .expect("enrol");
    }
    store
        .enrol_daemon(&untouched, "mac-a", "Mac", "jti", 1)
        .await
        .expect("enrol");
    let drop = "alter table local_exec_daemon drop column relay_enabled";
    sqlx::query(drop)
        .execute(store.pool())
        .await
        .expect("as before the switch");

    replay(&store, true).await;

    assert_eq!(
        switches(&store, &off, &["mac-a", "mac-b"]).await,
        [false, false]
    );
    assert_eq!(switches(&store, &back_on, &["mac-a"]).await, [true]);
    assert_eq!(switches(&store, &untouched, &["mac-a"]).await, [true]);

    // Its person switches one on. The next change to the schema replays it, and it stays on.
    let row = store
        .set_relay(&off, "mac-a", true)
        .await
        .expect("switch")
        .expect("theirs");
    assert!(row.relay_enabled, "{row:?}");
    replay(&store, false).await;
    assert_eq!(
        switches(&store, &off, &["mac-a", "mac-b"]).await,
        [true, false]
    );
}
