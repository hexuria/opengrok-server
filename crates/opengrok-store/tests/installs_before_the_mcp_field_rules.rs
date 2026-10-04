//! An install saved before #367's MCP field rules was never judged by them, and its stored servers
//! no longer carry the raw fields (`disabled`, `oauth`, the unknown ones) that would let a dial
//! judge it now. So the boot that brings the rules clears such installs ONCE, with their credential
//! rows and the sealed secrets those point at, and they are reinstalled under the rules.
//!
//! What it must get right: every install from before goes, its secrets with it and nobody else's,
//! and no later replay of the schema deletes an install made since.
//!
//! One test in its own binary, so its own database: it makes the schema replay, which takes the
//! table locks a replay takes, and nothing else may be running beside it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_store::PgStore;

const PASS: &str = "installs-before-the-mcp-field-rules";

/// Replay the schema as the next change to it will, with or without the pass already recorded.
async fn replay(store: &PgStore, forget_the_pass: bool) {
    if forget_the_pass {
        sqlx::query("delete from schema_migrations where name = $1")
            .bind(PASS)
            .execute(store.pool())
            .await
            .expect("forget the pass");
    }
    sqlx::query("delete from schema_applied")
        .execute(store.pool())
        .await
        .expect("forget the schema");
    opengrok_store::migrations::run(store.pool())
        .await
        .expect("replay");
}

/// An install with one saved credential, and the sealed secret it points at.
async fn installed(store: &PgStore, account: &str, plugin: &str) {
    sqlx::query(
        "insert into plugin_installation (account_id, name, registry, registry_revision,
           repository, revision, bundle, installed_at_ms)
         values ($1, $2, 'o/r', repeat('a', 40), 'o/r', repeat('a', 40), '{}'::jsonb, 1)",
    )
    .bind(account)
    .bind(plugin)
    .execute(store.pool())
    .await
    .expect("an install");
    let secret = format!("plugin/{account}/{plugin}/{plugin}");
    sqlx::query(
        "insert into secret_store (id, nonce, ciphertext, key_id, updated_at_ms)
         values ($1, '\\x00'::bytea, '\\x00'::bytea, 'k', 1)",
    )
    .bind(&secret)
    .execute(store.pool())
    .await
    .expect("its sealed secret");
    sqlx::query(
        "insert into plugin_credential (account_id, plugin_name, connector, secret_id)
         values ($1, $2, $2, $3)",
    )
    .bind(account)
    .bind(plugin)
    .bind(&secret)
    .execute(store.pool())
    .await
    .expect("its credential");
}

async fn count(store: &PgStore, sql: &'static str, key: &str) -> i64 {
    sqlx::query_scalar(sql)
        .bind(key)
        .fetch_one(store.pool())
        .await
        .expect("count")
}

/// Installs, credentials and plugin secrets this account still has.
async fn footprint(store: &PgStore, account: &str) -> (i64, i64, i64) {
    (
        count(
            store,
            "select count(*) from plugin_installation where account_id = $1",
            account,
        )
        .await,
        count(
            store,
            "select count(*) from plugin_credential where account_id = $1",
            account,
        )
        .await,
        count(
            store,
            "select count(*) from secret_store where id like 'plugin/' || $1 || '/%'",
            account,
        )
        .await,
    )
}

#[tokio::test]
async fn installs_from_before_the_rules_go_once_and_installs_since_stay() {
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
    let account = format!("acct_installs_{suffix}");
    // A secret that is not a plugin's: the pass must not reach it.
    let unrelated = format!("site-login:{account}:example");
    sqlx::query(
        "insert into secret_store (id, nonce, ciphertext, key_id, updated_at_ms)
         values ($1, '\\x00'::bytea, '\\x00'::bytea, 'k', 1)",
    )
    .bind(&unrelated)
    .execute(store.pool())
    .await
    .expect("an unrelated secret");

    // Saved by a build from before the rules, then the boot that brings them.
    installed(&store, &account, "demo").await;
    installed(&store, &account, "other").await;
    assert_eq!(footprint(&store, &account).await, (2, 2, 2));
    replay(&store, true).await;
    assert_eq!(
        footprint(&store, &account).await,
        (0, 0, 0),
        "every install from before the rules is cleared, its credentials and secrets with it"
    );
    let kept = count(
        &store,
        "select count(*) from secret_store where id = $1",
        &unrelated,
    )
    .await;
    assert_eq!(kept, 1, "a secret that is not a plugin's is untouched");

    // Installed under the rules; a later change to the schema replays it, and the pass is over.
    installed(&store, &account, "demo").await;
    replay(&store, false).await;
    assert_eq!(
        footprint(&store, &account).await,
        (1, 1, 1),
        "an install made since the pass survives every later replay"
    );
}
