//! A recipe's registry rules, against the store: who sees it, who has accepted, which bot may run
//! what, and what a runnable version is.
//!
//! The routes refuse by `recipes::may`; this file checks the rows those refusals read. Needs
//! Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_store::PgStore;
use serde_json::json;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn connect() -> Option<PgStore> {
    let database_url = std::env::var("OG_DATABASE_URL").ok()?;
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

#[tokio::test]
async fn a_shared_recipe_is_seen_accepted_and_granted_per_person() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let owner = format!("acct_owner_{stamp}");
    let colleague = format!("acct_colleague_{stamp}");
    let stranger = format!("acct_stranger_{stamp}");
    let org = format!("org_{stamp}");
    let bot = format!("cw_bot_{stamp}");
    let id = format!("rcp_{stamp}");

    store
        .create_recipe(
            &id,
            &owner,
            Some(&org),
            "Open Gmail",
            "inbox, fast",
            (1280, 800),
            stamp,
        )
        .await
        .expect("create");

    // ---- versions: raw first, then filtered; the runnable one is never the raw tape ----
    let v1 = store
        .add_recipe_version(
            &id,
            "raw",
            &json!([{"kind": "down", "x": 1, "y": 1, "at": 0}]),
            "taped",
            &owner,
            stamp,
        )
        .await
        .expect("v1");
    assert_eq!(v1, 1);
    assert!(
        store
            .recipe_runnable_version(&id)
            .await
            .expect("read")
            .is_none(),
        "a raw tape alone is not runnable"
    );
    let v2 = store
        .add_recipe_version(
            &id,
            "filtered",
            &json!({"steps": [{"op": "click", "x": 1, "y": 1}]}),
            "filtered",
            &owner,
            stamp,
        )
        .await
        .expect("v2");
    assert_eq!(v2, 2);
    let v3 = store
        .add_recipe_version(
            &id,
            "edited",
            &json!({"steps": [{"op": "wait", "ms": 5}]}),
            "edited",
            &owner,
            stamp,
        )
        .await
        .expect("v3");
    assert_eq!(v3, 3);
    let runnable = store
        .recipe_runnable_version(&id)
        .await
        .expect("read")
        .expect("v3");
    assert_eq!(runnable.version, 3, "the newest non-raw version runs");
    assert_eq!(
        store
            .recipe(&id)
            .await
            .expect("read")
            .expect("row")
            .latest_version,
        3
    );

    // ---- mine ----
    let mine = store.recipes_owned_by(&owner).await.expect("mine");
    assert!(mine.iter().any(|row| row.id == id));
    assert!(
        store
            .recipes_owned_by(&colleague)
            .await
            .expect("theirs")
            .iter()
            .all(|row| row.id != id)
    );

    // ---- an org share: every member sees it pending; acceptance is per person ----
    store
        .share_recipe(&id, "org", &org, &owner, stamp)
        .await
        .expect("share to org");
    let seen = store
        .recipes_shared_with(&colleague, Some(&org))
        .await
        .expect("shared");
    let (_, share) = seen
        .iter()
        .find(|(row, _)| row.id == id)
        .expect("the colleague sees the org share");
    assert_eq!(share.scope, "org");
    assert!(share.accepted_at_ms.is_none(), "not accepted yet");
    assert!(
        store
            .recipes_shared_with(&stranger, None)
            .await
            .expect("shared")
            .iter()
            .all(|(row, _)| row.id != id),
        "outside the org nothing is shared"
    );
    assert!(
        !store
            .recipe_accepted_by(&id, &colleague)
            .await
            .expect("read")
    );

    assert!(
        store
            .answer_recipe_share(&id, &colleague, Some(&org), true, stamp)
            .await
            .expect("accept"),
        "an org member may accept the org share"
    );
    assert!(
        store
            .recipe_accepted_by(&id, &colleague)
            .await
            .expect("read")
    );
    assert!(
        !store
            .recipe_accepted_by(&id, &stranger)
            .await
            .expect("read"),
        "one member's acceptance is not another's"
    );
    assert!(
        !store
            .answer_recipe_share(&id, &stranger, None, true, stamp)
            .await
            .expect("answer"),
        "nothing was shared with the stranger, so there is nothing to accept"
    );

    // ---- a declined direct share is not accepted ----
    store
        .share_recipe(&id, "account", &stranger, &owner, stamp)
        .await
        .expect("share to a person");
    assert!(
        store
            .answer_recipe_share(&id, &stranger, None, false, stamp)
            .await
            .expect("decline")
    );
    assert!(
        !store
            .recipe_accepted_by(&id, &stranger)
            .await
            .expect("read")
    );
    store
        .unshare_recipe(&id, "account", &stranger)
        .await
        .expect("unshare");
    assert!(
        store
            .recipe_shares(&id)
            .await
            .expect("shares")
            .iter()
            .all(|s| s.scope_id != stranger)
    );

    // ---- grants decide what a bot is offered ----
    assert!(
        store
            .recipes_granted_to(&bot)
            .await
            .expect("granted")
            .is_empty()
    );
    store
        .grant_recipe(&id, &bot, &colleague, stamp)
        .await
        .expect("grant");
    let offered = store.recipes_granted_to(&bot).await.expect("granted");
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].name, "Open Gmail");
    store.revoke_recipe_grant(&id, &bot).await.expect("revoke");
    assert!(
        store
            .recipes_granted_to(&bot)
            .await
            .expect("granted")
            .is_empty()
    );

    // ---- runs are history ----
    store
        .grant_recipe(&id, &bot, &colleague, stamp)
        .await
        .expect("grant again");
    store
        .record_recipe_run(
            &format!("rrun_{stamp}"),
            &id,
            3,
            &bot,
            None,
            false,
            Some(0),
            &json!({"ok": false}),
            stamp,
        )
        .await
        .expect("record");
    let runs = store.recipe_runs(&id, 10).await.expect("runs");
    assert_eq!(runs.len(), 1);

    // History keeps the newest five of a version: the page shows five, so five is what is kept.
    // One run of v3 is already recorded above, and one of v2 proves a version is pruned alone.
    store
        .record_recipe_run(
            &format!("rrun_{stamp}_v2"),
            &id,
            2,
            &bot,
            None,
            true,
            None,
            &json!({"ok": true}),
            stamp,
        )
        .await
        .expect("record");
    for n in 0..7 {
        store
            .record_recipe_run(
                &format!("rrun_{stamp}_{n}"),
                &id,
                3,
                &bot,
                None,
                true,
                None,
                &json!({"ok": true}),
                stamp + n,
            )
            .await
            .expect("record");
    }
    let dropped = store.prune_recipe_runs(&id, 3, 5).await.expect("prune");
    assert_eq!(dropped, 3, "eight runs of v3, five kept");
    let kept: Vec<_> = store
        .recipe_runs(&id, 50)
        .await
        .expect("runs")
        .into_iter()
        .filter(|run| run.version == 3)
        .collect();
    assert_eq!(kept.len(), 5);
    assert_eq!(kept[0].at_ms, stamp + 6, "the newest survives");
    assert!(
        store
            .recipe_runs(&id, 50)
            .await
            .expect("runs")
            .iter()
            .any(|run| run.version == 2),
        "another version's history is untouched"
    );
    assert!(!runs[0].ok);
    assert_eq!(runs[0].stopped_at, Some(0));

    // ---- soft delete: gone from lists and from the bot, history still readable ----
    store
        .soft_delete_recipe(&id, stamp + 1)
        .await
        .expect("delete");
    assert!(
        store
            .recipes_owned_by(&owner)
            .await
            .expect("mine")
            .iter()
            .all(|row| row.id != id)
    );
    assert!(
        store
            .recipes_shared_with(&colleague, Some(&org))
            .await
            .expect("shared")
            .iter()
            .all(|(row, _)| row.id != id)
    );
    assert!(
        store
            .recipes_granted_to(&bot)
            .await
            .expect("granted")
            .is_empty(),
        "a deleted recipe is not offered"
    );
    assert!(
        store
            .recipe(&id)
            .await
            .expect("read")
            .expect("the row stays")
            .deleted_at_ms
            .is_some()
    );
    // Deleting the recipe does not delete what it did: five kept runs of v3 and one of v2.
    assert_eq!(store.recipe_runs(&id, 50).await.expect("runs").len(), 6);
}
