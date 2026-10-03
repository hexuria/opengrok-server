//! The outbox's SQL: write notes under the account's head lock, read them back by id, and bound
//! what is kept.

use opengrok_wire::events::Note;
use serde_json::Value;
use sqlx::{PgConnection, PgPool};

use crate::hub::CHANNEL;

/// How long a note is kept, and how many of one account's. An id older than either is not
/// resumed from: the app is told to read everything again (`reset`).
pub const RETAIN_HOURS: i32 = 24;
pub const RETAIN_MAX: i64 = 10_000;

/// The account's next `$2` ids, in one statement: the head row it updates is locked until the
/// transaction ends, which is what numbers an account's notes in commit order. A head that is
/// behind its notes (its row deleted by hand) is mended here and not trusted: a duplicate key
/// would read as `StoreError::Conflict`, and a run's first batch would take it for a lost race.
const TAKE: &str = "insert into account_event_head (account_id, head)
    select $1::text, coalesce(max(id), 0) + $2 from account_event where account_id = $1
    on conflict (account_id) do update
       set head = greatest(account_event_head.head, excluded.head - $2) + $2
    returning head";

const WRITE: &str = "insert into account_event (account_id, id, kind, payload)
    select $1, $2 + t.n, t.kind, t.payload::json
      from unnest($3::text[], $4::text[]) with ordinality as t(kind, payload, n)";

/// Delete what is past retention, and lift `floor` to the highest id gone so a resume below it is
/// refused. `head` is never touched: ids do not go back, whatever is deleted.
const PRUNE: &str = "with gone as (
      delete from account_event
       where account_id = $1 and (id <= $2 or created_at < now() - make_interval(hours => $3))
      returning id)
    update account_event_head set floor = greatest(floor, (select max(id) from gone))
     where account_id = $1 and exists (select 1 from gone)";

/// The head, and the lowest id a resume is refused below: retention's `floor`, or the newest note
/// already past its age, which a prune that has not run yet would have removed.
const BOUNDS: &str = "select
      coalesce((select head from account_event_head where account_id = $1), 0),
      greatest(
        coalesce((select floor from account_event_head where account_id = $1), 0),
        coalesce((select max(id) from account_event
                   where account_id = $1 and created_at < now() - make_interval(hours => $2)), 0))";

const PAGE: &str = "select id, kind, payload from account_event
    where account_id = $1 and id > $2 order by id limit $3";

/// Write `notes` for `account` on the caller's connection, then wake whoever follows the account.
///
/// MUST BE THE LAST STATEMENT OF ITS TRANSACTION. It takes the account's head lock, and a
/// transaction that waited on anything after holding it would make every other writer of the
/// account wait too. Nothing is written for an empty slice. NOTIFY is queued by the transaction
/// and sent only if it commits.
pub async fn emit(
    conn: &mut PgConnection,
    account: &str,
    notes: &[Note<'_>],
) -> Result<(), sqlx::Error> {
    if notes.is_empty() {
        return Ok(());
    }
    let count = notes.len() as i64;
    let head: i64 = sqlx::query_scalar(TAKE)
        .bind(account)
        .bind(count)
        .fetch_one(&mut *conn)
        .await?;
    let kinds: Vec<&str> = notes.iter().map(Note::event).collect();
    let data: Vec<String> = notes.iter().map(Note::data).collect();
    let write = sqlx::query(WRITE).bind(account).bind(head - count);
    write.bind(&kinds).bind(&data).execute(&mut *conn).await?;
    let wake = sqlx::query("select pg_notify($1, $2)").bind(CHANNEL);
    wake.bind(format!("{account}:{head}"))
        .execute(&mut *conn)
        .await?;
    let prune = sqlx::query(PRUNE).bind(account);
    prune
        .bind(head - RETAIN_MAX)
        .bind(RETAIN_HOURS)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `(head, floor)`: ids from `floor` to `head` can be resumed from.
pub(crate) async fn bounds(pool: &PgPool, account: &str) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(BOUNDS)
        .bind(account)
        .bind(RETAIN_HOURS)
        .fetch_one(pool)
        .await
}

/// A note as stored: what a block is made of.
pub(crate) struct Stored {
    pub id: i64,
    pub kind: String,
    pub payload: Value,
}

/// The account's notes after `after`, oldest first, at most `limit`.
pub(crate) async fn page(
    pool: &PgPool,
    account: &str,
    after: i64,
    limit: i64,
) -> Result<Vec<Stored>, sqlx::Error> {
    let rows: Vec<(i64, String, Value)> = sqlx::query_as(PAGE)
        .bind(account)
        .bind(after)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, kind, payload)| Stored { id, kind, payload })
        .collect())
}
