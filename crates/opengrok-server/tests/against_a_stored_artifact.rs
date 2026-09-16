//! Bytes go in and come back the same, a listing never carries them, and one account's artifact
//! is not another's. Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use opengrok_store::{ArtifactRow, PgStore};
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

fn row(id: &str, account: &str, kind: &str, at_ms: i64) -> ArtifactRow {
    ArtifactRow {
        id: id.to_string(),
        account_id: account.to_string(),
        kind: kind.to_string(),
        mime: "image/png".to_string(),
        filename: "shot.png".to_string(),
        // Deliberately a lie: put_artifact writes the real length rather than trusting a caller.
        size_bytes: 0,
        recipe_id: None,
        run_id: None,
        step_index: None,
        thread_id: None,
        meta: json!({}),
        created_at_ms: at_ms,
        deleted_at_ms: None,
    }
}

#[tokio::test]
async fn an_artifact_round_trips_and_belongs_to_one_account() {
    let Some(store) = connect().await else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let stamp = now_ms();
    let mine = format!("acct_mine_{stamp}");
    let theirs = format!("acct_theirs_{stamp}");

    // A PNG header and a NUL: bytes that do not survive a trip through a String.
    let bytes: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x00, 0x1A, 0x0A, 0xFF];
    let id = format!("art_{stamp}");
    store
        .put_artifact(&row(&id, &mine, "attachment", stamp), &bytes)
        .await
        .expect("store");

    let listed = store
        .artifact(&id)
        .await
        .expect("read")
        .expect("it is there");
    assert_eq!(listed.account_id, mine);
    assert_eq!(
        listed.size_bytes,
        bytes.len() as i64,
        "the stored length is the real one, not the caller's claim"
    );

    let (again, back) = store
        .artifact_bytes(&id)
        .await
        .expect("read")
        .expect("it is there");
    assert_eq!(back, bytes, "a PNG header survives the round trip");
    assert_eq!(again.id, id);

    // Empty is refused: a zero-byte artifact is a bug upstream, not a thing to keep.
    assert!(
        store
            .put_artifact(
                &row(&format!("art_empty_{stamp}"), &mine, "attachment", stamp),
                &[]
            )
            .await
            .is_err(),
        "an empty artifact is refused"
    );

    // ---- a run's artifacts come back in step order ----
    let recipe = format!("rcp_{stamp}");
    let run = format!("rrun_{stamp}");
    for step in [2i32, 0, 1] {
        let mut shot = row(&format!("art_{stamp}_{step}"), &mine, "screenshot", stamp);
        shot.recipe_id = Some(recipe.clone());
        shot.run_id = Some(run.clone());
        shot.step_index = Some(step);
        store.put_artifact(&shot, &bytes).await.expect("store");
    }
    let of_run = store.artifacts_for_run(&recipe, &run).await.expect("read");
    assert_eq!(
        of_run.iter().map(|a| a.step_index).collect::<Vec<_>>(),
        vec![Some(0), Some(1), Some(2)],
        "a steps table draws them in step order"
    );

    // ---- a thread's artifacts are that account's only ----
    let thread = format!("thread_{stamp}");
    let mut ours = row(&format!("art_ours_{stamp}"), &mine, "attachment", stamp);
    ours.thread_id = Some(thread.clone());
    store.put_artifact(&ours, &bytes).await.expect("store");
    let mut nosy = row(&format!("art_nosy_{stamp}"), &theirs, "attachment", stamp);
    nosy.thread_id = Some(thread.clone());
    store.put_artifact(&nosy, &bytes).await.expect("store");

    let seen = store
        .artifacts_for_thread(&mine, &thread)
        .await
        .expect("read");
    assert_eq!(
        seen.len(),
        1,
        "the same thread id in another account is not mine"
    );
    assert_eq!(seen[0].account_id, mine);

    // ---- soft delete hides it from every read ----
    store
        .soft_delete_artifact(&id, stamp + 1)
        .await
        .expect("delete");
    assert!(store.artifact(&id).await.expect("read").is_none());
    assert!(store.artifact_bytes(&id).await.expect("read").is_none());

    // ---- an artifact whose run is gone goes with it ----
    let gone_run = format!("rrun_gone_{stamp}");
    let mut orphan = row(&format!("art_orphan_{stamp}"), &mine, "recording", stamp);
    orphan.recipe_id = Some(recipe.clone());
    orphan.run_id = Some(gone_run.clone());
    store.put_artifact(&orphan, &bytes).await.expect("store");
    assert_eq!(
        store
            .artifacts_for_run(&recipe, &gone_run)
            .await
            .expect("read")
            .len(),
        1
    );

    // No recipe_run row was ever written for that id, so pruning finds it orphaned. The three
    // step screenshots above are orphaned too — none of them has a run row either.
    let dropped = store
        .orphan_artifacts_of_gone_runs(&recipe, stamp + 2)
        .await
        .expect("orphan");
    assert_eq!(
        dropped, 4,
        "the recording and the three step shots all had no run"
    );
    assert!(
        store
            .artifacts_for_run(&recipe, &gone_run)
            .await
            .expect("read")
            .is_empty(),
        "an artifact with no run has no page, so it is gone"
    );
    // Another recipe's artifacts are not touched: the sweep is per recipe.
    assert_eq!(
        store
            .artifacts_for_thread(&mine, &thread)
            .await
            .expect("read")
            .len(),
        1,
        "an attachment has no run and must survive the sweep"
    );
}
