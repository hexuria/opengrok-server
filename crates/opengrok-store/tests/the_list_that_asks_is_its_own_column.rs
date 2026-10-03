//! Auto-review's "Ask first" list was stored as `block_instructions`, and a match raised a card
//! (#354). Block now refuses, so the boot that adds `ask_instructions` moves what is written there
//! ONCE, and every old row reads the way it always behaved: the text asks, and block is empty.
//!
//! What it must get right, because a user cannot see it go wrong: text moves, an explicit `''`
//! ("this Bot has no ask-first rules") moves too so a global list does not reach a scope that
//! cleared it, every other field and the row's clock stay as written, text already in the ask
//! column is kept and appended to, and no later replay of the schema turns a block written since
//! into an ask.
//!
//! One test in its own binary, so its own database: it drops the column and makes the schema
//! replay, which takes the table locks a replay takes, and nothing else may be running beside it.
//!
//! Needs Postgres; skips loudly without OG_DATABASE_URL.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use opengrok_store::PgStore;
use opengrok_store::auto_review::AutoReviewRow;

const PASS: &str = "the-list-that-asks-is-its-own-column";

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

async fn recorded(store: &PgStore) -> bool {
    sqlx::query_scalar("select exists (select 1 from schema_migrations where name = $1)")
        .bind(PASS)
        .fetch_one(store.pool())
        .await
        .expect("schema_migrations")
}

/// A row as a build from before the column wrote it: it had nowhere to put an ask-first list.
async fn written_before(
    store: &PgStore,
    account: &str,
    (kind, id): (&str, &str),
    (enabled, allow, block): (Option<bool>, Option<&str>, Option<&str>),
    at_ms: i64,
) {
    sqlx::query(
        "insert into auto_review_policy
            (account_id, scope_kind, scope_id, enabled, allow_instructions, block_instructions,
             updated_at_ms)
         values ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(account)
    .bind(kind)
    .bind(id)
    .bind(enabled)
    .bind(allow)
    .bind(block)
    .bind(at_ms)
    .execute(store.pool())
    .await
    .expect("an old row");
}

async fn rows(store: &PgStore, account: &str) -> Vec<AutoReviewRow> {
    store.auto_review_rows(account).await.expect("rows")
}

fn find<'a>(rows: &'a [AutoReviewRow], id: &str) -> &'a AutoReviewRow {
    rows.iter()
        .find(|row| row.scope_id == id)
        .unwrap_or_else(|| panic!("no row for {id}: {rows:?}"))
}

/// `(allow, ask, block)` as stored.
fn lists(row: &AutoReviewRow) -> (Option<&str>, Option<&str>, Option<&str>) {
    (
        row.allow_instructions.as_deref(),
        row.ask_instructions.as_deref(),
        row.block_instructions.as_deref(),
    )
}

#[tokio::test]
async fn what_was_written_as_a_block_asks_once_and_a_block_written_since_stays_a_block() {
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
    let account = format!("acct_ask_{suffix}");
    let bystander = format!("acct_other_{suffix}");
    let cw = |name: &str| format!("cw_{name}_{suffix}");

    // The table as it was before the column, holding what people wrote in it.
    sqlx::query("alter table auto_review_policy drop column ask_instructions")
        .execute(store.pool())
        .await
        .expect("as before the column");
    let global = (
        Some(true),
        Some("git is fine"),
        Some("check with me before email"),
    );
    written_before(&store, &account, ("global", ""), global, 11).await;
    // Text alone, with every other field inheriting: they must go on inheriting.
    let text = (None, None, Some("check with me before a purchase"));
    written_before(&store, &account, ("coworker", &cw("text")), text, 20).await;
    // Cleared on purpose: no ask-first rules here, whatever global's say.
    let cleared = (Some(false), Some("ls is fine"), Some(""));
    written_before(&store, &account, ("coworker", &cw("cleared")), cleared, 21).await;
    // Nothing to move: this row must not be touched at all.
    let nothing = (Some(true), Some("cat is fine"), None);
    written_before(&store, &account, ("coworker", &cw("nothing")), nothing, 22).await;
    let elsewhere = (None, None, Some("ask me about deletes"));
    written_before(&store, &bystander, ("global", ""), elsewhere, 30).await;

    replay(&store, true).await;
    assert!(recorded(&store).await);

    let moved = rows(&store, &account).await;
    let row = find(&moved, "");
    assert_eq!(
        lists(row),
        (
            Some("git is fine"),
            Some("check with me before email"),
            None
        ),
        "the text moved, the allow list stayed, block is empty"
    );
    assert_eq!((row.enabled, row.updated_at_ms), (Some(true), 11));
    let row = find(&moved, &cw("text"));
    let wanted = (None, Some("check with me before a purchase"), None);
    assert_eq!(lists(row), wanted);
    assert_eq!((row.enabled, row.updated_at_ms), (None, 20));
    // An explicit '' moved as an explicit '': the Bot still has no ask-first rules.
    let row = find(&moved, &cw("cleared"));
    assert_eq!(lists(row), (Some("ls is fine"), Some(""), None));
    assert_eq!((row.enabled, row.updated_at_ms), (Some(false), 21));
    let row = find(&moved, &cw("nothing"));
    assert_eq!(lists(row), (Some("cat is fine"), None, None));
    assert_eq!((row.enabled, row.updated_at_ms), (Some(true), 22));
    // The pass is the table's, not one account's.
    let other = rows(&store, &bystander).await;
    let wanted = (None, Some("ask me about deletes"), None);
    assert_eq!(lists(find(&other, "")), wanted);

    // Once the pass is recorded a block is a block: written now, kept through every replay.
    let (ask, block) = (Some("keep asking about rm"), Some("never delete backups"));
    let nothing = cw("nothing");
    let write = store.set_auto_review_policy(
        &account,
        "coworker",
        &nothing,
        Some(true),
        None,
        ask,
        block,
        40,
    );
    write.await.expect("a block written since");
    let before = rows(&store, &account).await;
    replay(&store, false).await;
    replay(&store, false).await;
    assert_eq!(rows(&store, &account).await, before);
    assert_eq!(lists(find(&before, &cw("nothing"))), (None, ask, block));

    // Text already in the ask column is kept and the block text goes after it, a blank line
    // between: the column is the only list that asks, and neither list may lose a word. An empty
    // ask column has nothing to keep, so the block text is the list with no stray blank line;
    // and a cleared block moves nothing and does not clear the ask text beside it.
    let both = [
        ("both", Some("check first"), Some("and before any purchase")),
        ("blank_ask", Some(""), Some("and before any purchase")),
        ("cleared_block", Some("check first"), Some("")),
    ];
    for (name, ask, block) in both {
        let id = cw(name);
        let write =
            store.set_auto_review_policy(&bystander, "coworker", &id, None, None, ask, block, 50);
        write.await.expect("a row with both");
    }
    replay(&store, true).await;
    let moved = rows(&store, &bystander).await;
    let wanted = (None, Some("check first\n\nand before any purchase"), None);
    assert_eq!(lists(find(&moved, &cw("both"))), wanted);
    let wanted = (None, Some("and before any purchase"), None);
    assert_eq!(lists(find(&moved, &cw("blank_ask"))), wanted);
    let wanted = (None, Some("check first"), None);
    assert_eq!(lists(find(&moved, &cw("cleared_block"))), wanted);
    // This replay forgot the pass, so it ran again over every account, mine included: the block
    // written above, after the first pass, went the way of the rest. That is why the pass is
    // recorded, and why it must be: a second pass is not harmless.
    let again = rows(&store, &account).await;
    let wanted = (
        None,
        Some("keep asking about rm\n\nnever delete backups"),
        None,
    );
    assert_eq!(lists(find(&again, &cw("nothing"))), wanted);
}
