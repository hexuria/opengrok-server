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

    // ---- one version at a time: an edit can be taken back, the tape cannot ----
    assert!(
        store
            .delete_recipe_version(&id, 9)
            .await
            .expect("delete")
            .is_none(),
        "a version that was never there deletes nothing"
    );
    let v4 = store
        .add_recipe_version(&id, "edited", &json!({"steps": []}), "later", &owner, stamp)
        .await
        .expect("v4");
    assert_eq!(v4, 4);
    assert_eq!(
        store
            .recipe_runnable_version(&id)
            .await
            .expect("read")
            .expect("v4")
            .version,
        4
    );
    let gone = store
        .delete_recipe_version(&id, 4)
        .await
        .expect("delete")
        .expect("v4 was there");
    assert_eq!(gone.kind, "edited");
    assert_eq!(
        store
            .recipe_runnable_version(&id)
            .await
            .expect("read")
            .expect("v3")
            .version,
        3,
        "deleting the newest edit leaves the one before it running"
    );
    assert!(
        store
            .recipe_runs(&id, 50)
            .await
            .expect("runs")
            .iter()
            .all(|run| run.version != 4),
        "a version's runs go with it, since history is read a version at a time"
    );

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

#[tokio::test]
async fn recipe_parameters_declare_and_bind() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let owner = format!("acct_owner_{stamp}");
    let org = format!("org_{stamp}");
    let id = format!("rcp_{stamp}");

    store
        .create_recipe(
            &id,
            &owner,
            Some(&org),
            "Search YouTube",
            "teaches a search on YouTube",
            (1280, 800),
            stamp,
        )
        .await
        .expect("create");

    let raw_version = store
        .add_recipe_version(&id, "raw", &json!([]), "taped", &owner, stamp)
        .await
        .expect("v1");
    assert_eq!(raw_version, 1);

    // Add a version with parameters
    let params = json!([
        {
            "name": "search_term",
            "description": "What to search for",
            "required": true,
            "kind": "text",
            "default": null,
            "values": null,
        }
    ]);
    let body = json!({
        "steps": [
            {"op": "type", "text": "search {{search_term}}"},
            {"op": "key", "key": "Return"},
        ],
        "stop_on_error": true,
        "screenshot": "end",
        "parameters": params,
    });

    let v2 = store
        .add_recipe_version(&id, "edited", &body, "with parameters", &owner, stamp)
        .await
        .expect("v2");
    assert_eq!(v2, 2);

    // Verify the parameters round-trip through storage
    let version = store
        .recipe_version(&id, v2)
        .await
        .expect("read")
        .expect("found");
    let stored_params = version
        .body
        .get("parameters")
        .cloned()
        .unwrap_or(json!(null));
    assert_eq!(
        stored_params, params,
        "parameters round-trip through storage"
    );

    // Verify steps are also present
    let stored_steps = version
        .body
        .get("steps")
        .and_then(|s| s.as_array())
        .expect("steps array");
    assert_eq!(stored_steps.len(), 2, "both steps are stored");
}

/// TWO PEOPLE IN NO ORG ARE NOT COLLEAGUES.
///
/// `Register` carries `org_id` as a plain `String`, so an account made without one used to replay
/// to `Some("")`. The share handler's `Some` arm then accepted `{"scope":"org"}` from an orgless
/// owner and wrote `recipe_share(scope='org', scope_id='')` — and every other orgless account
/// matched it, became `Invited`, could accept, and could then run a recipe carrying somebody
/// else's taped screens and keystrokes.
///
/// The fix is in three places and this test stands on the last of them: the aggregate no longer
/// produces `Some("")` (`opengrok-core`), `org_of` filters it (`server/recipes.rs`), and the
/// queries here refuse an absent or empty org outright — which is what still has to hold for a
/// row an older build already wrote.
#[tokio::test]
async fn two_people_in_no_org_are_not_colleagues() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let owner = format!("acct_orgless_owner_{stamp}");
    let stranger = format!("acct_orgless_stranger_{stamp}");
    let id = format!("rcp_orgless_{stamp}");

    store
        .create_recipe(&id, &owner, None, "Open the bank", "", (1280, 800), stamp)
        .await
        .expect("create");
    store
        .add_recipe_version(
            &id,
            "filtered",
            &json!({"steps": [{"op": "click", "x": 1, "y": 1}]}),
            "",
            &owner,
            stamp,
        )
        .await
        .expect("version");

    // The row an older build would have written: an org share whose org is the empty string.
    store
        .share_recipe(&id, "org", "", &owner, stamp)
        .await
        .expect("share");

    // No org at all sees nothing...
    assert!(
        store
            .recipes_shared_with(&stranger, None)
            .await
            .expect("read")
            .iter()
            .all(|(row, _)| row.id != id),
        "a person in no org is not in everyone's org"
    );
    // ...and neither does one whose org somehow still arrives as the empty string.
    assert!(
        store
            .recipes_shared_with(&stranger, Some(""))
            .await
            .expect("read")
            .iter()
            .all(|(row, _)| row.id != id),
        "an empty org id must match no share row, including one already stored"
    );
    // The owner's own listing is not what this is about, and must be untouched.
    assert!(
        store
            .recipes_owned_by(&owner)
            .await
            .expect("read")
            .iter()
            .any(|row| row.id == id),
        "the owner still owns it"
    );

    // And accepting is refused, so the stranger cannot become a Recipient either.
    for org in [None, Some("")] {
        assert!(
            !store
                .answer_recipe_share(&id, &stranger, org, true, stamp)
                .await
                .expect("answer"),
            "there is no share here to accept (org {org:?})"
        );
    }
    assert!(
        !store
            .recipe_accepted_by(&id, &stranger)
            .await
            .expect("read"),
        "nothing was accepted, so nothing may run"
    );

    // A REAL org still works: the fix must refuse the empty string, not org sharing.
    let org = format!("org_real_{stamp}");
    let colleague = format!("acct_colleague_{stamp}");
    let real = format!("rcp_real_{stamp}");
    store
        .create_recipe(
            &real,
            &owner,
            Some(&org),
            "Open Gmail",
            "",
            (1280, 800),
            stamp,
        )
        .await
        .expect("create");
    store
        .share_recipe(&real, "org", &org, &owner, stamp)
        .await
        .expect("share");
    assert!(
        store
            .recipes_shared_with(&colleague, Some(&org))
            .await
            .expect("read")
            .iter()
            .any(|(row, _)| row.id == real),
        "a real org share still reaches a real colleague"
    );
    assert!(
        store
            .answer_recipe_share(&real, &colleague, Some(&org), true, stamp)
            .await
            .expect("answer"),
        "and can still be accepted"
    );
}

/// A SHARE TAKEN BACK TAKES THE BOTS' GRANTS WITH IT.
///
/// A recipient may grant a shared recipe to their own bots, and those grants are what a turn offers
/// as `run_recipe`. Unsharing and declining used to delete only the share row, so the bot kept
/// being offered the recipe — and kept playing the owner's newest version, edits made after the
/// unshare included. Access was checked once, at grant time.
///
/// Two halves, both asserted: the paths that end access delete the grants, and the read that
/// builds a turn's offers re-checks that whoever granted still holds the recipe, so a grant a
/// cleanup missed fails closed instead of running.
#[tokio::test]
async fn unsharing_or_declining_takes_the_recipe_back_from_the_recipients_bots() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let owner = format!("acct_owner_{stamp}_w");
    let colleague = format!("acct_colleague_{stamp}_w");
    let org = format!("org_{stamp}_w");
    let bot_c = format!("cw_colleagues_{stamp}");
    let bot_o = format!("cw_owners_{stamp}");
    let id = format!("rcp_{stamp}_w");
    store
        .create_recipe(
            &id,
            &owner,
            Some(&org),
            "Pay the rent",
            "",
            (1280, 800),
            stamp,
        )
        .await
        .expect("create");
    store
        .add_recipe_version(
            &id,
            "filtered",
            &json!({"steps": [{"op": "click", "x": 1, "y": 1}]}),
            "filtered",
            &owner,
            stamp,
        )
        .await
        .expect("version");
    let offered = |bot: String| {
        let store = store.clone();
        async move { store.recipes_granted_to(&bot).await.expect("granted").len() }
    };
    let listed = |bot: String| {
        let store = store.clone();
        let id = id.clone();
        async move {
            store
                .recipe_grants(&id)
                .await
                .expect("grants")
                .iter()
                .any(|grant| grant.coworker_id == bot)
        }
    };

    // ---- a direct share, withdrawn ----
    store
        .share_recipe(&id, "account", &colleague, &owner, stamp)
        .await
        .expect("share");
    assert!(
        store
            .answer_recipe_share(&id, &colleague, Some(&org), true, stamp)
            .await
            .expect("accept")
    );
    store
        .grant_recipe(&id, &bot_c, &colleague, stamp)
        .await
        .expect("grant to the colleague's bot");
    store
        .grant_recipe(&id, &bot_o, &owner, stamp)
        .await
        .expect("grant to the owner's bot");
    assert_eq!(offered(bot_c.clone()).await, 1, "accepted and granted");
    store
        .unshare_recipe(&id, "account", &colleague)
        .await
        .expect("unshare");
    assert_eq!(
        offered(bot_c.clone()).await,
        0,
        "a withdrawn share is not offered to the bot it was granted to"
    );
    assert!(
        !listed(bot_c.clone()).await,
        "and the owner's page no longer lists that grant"
    );
    assert_eq!(
        offered(bot_o.clone()).await,
        1,
        "the owner's own grant is not the colleague's to lose"
    );

    // ---- an org share, withdrawn: everyone who accepted through it loses their grants ----
    store
        .share_recipe(&id, "org", &org, &owner, stamp)
        .await
        .expect("share to the org");
    assert!(
        store
            .answer_recipe_share(&id, &colleague, Some(&org), true, stamp)
            .await
            .expect("accept through the org")
    );
    store
        .grant_recipe(&id, &bot_c, &colleague, stamp)
        .await
        .expect("grant again");
    assert_eq!(offered(bot_c.clone()).await, 1);
    store
        .unshare_recipe(&id, "org", &org)
        .await
        .expect("unshare the org");
    assert_eq!(
        offered(bot_c.clone()).await,
        0,
        "an org share withdrawn is withdrawn from every member's bots"
    );
    assert!(!listed(bot_c.clone()).await);

    // ---- declined after granting ----
    store
        .share_recipe(&id, "account", &colleague, &owner, stamp)
        .await
        .expect("share again");
    assert!(
        store
            .answer_recipe_share(&id, &colleague, Some(&org), true, stamp)
            .await
            .expect("accept")
    );
    store
        .grant_recipe(&id, &bot_c, &colleague, stamp)
        .await
        .expect("grant");
    assert_eq!(offered(bot_c.clone()).await, 1);
    assert!(
        store
            .answer_recipe_share(&id, &colleague, Some(&org), false, stamp)
            .await
            .expect("decline")
    );
    assert_eq!(
        offered(bot_c.clone()).await,
        0,
        "a person who declined has no bot running it"
    );
    assert!(!listed(bot_c.clone()).await);

    // ---- a cleanup that never ran still fails closed ----
    assert!(
        store
            .answer_recipe_share(&id, &colleague, Some(&org), true, stamp)
            .await
            .expect("accept again")
    );
    store
        .grant_recipe(&id, &bot_c, &colleague, stamp)
        .await
        .expect("grant");
    assert_eq!(offered(bot_c.clone()).await, 1);
    // The share row goes without passing through `unshare_recipe`: a row lost some other way, or
    // a path added later that forgets the grants.
    sqlx::query("delete from recipe_share where recipe_id = $1 and scope_id = $2")
        .bind(&id)
        .bind(&colleague)
        .execute(store.pool())
        .await
        .expect("delete the share by hand");
    assert_eq!(
        offered(bot_c.clone()).await,
        0,
        "a grant whose granter no longer holds the recipe is not offered, whoever forgot it"
    );
    assert!(!listed(bot_c).await);
    assert_eq!(offered(bot_o).await, 1, "the owner still holds their own");
}
