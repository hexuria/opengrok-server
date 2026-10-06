//! Existing plugin credentials become token accounts without changing the sealed secret.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sqlx::postgres::PgPoolOptions;

#[tokio::test]
async fn an_existing_plugin_token_survives_upgrade_and_replay() {
    let url = match std::env::var("OG_DATABASE_URL") {
        Ok(url) => opengrok_store::gate_database_or_panic(url),
        Err(_) => {
            eprintln!("skipping: OG_DATABASE_URL is not set");
            return;
        }
    };
    let pool = PgPoolOptions::new().connect(&url).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(
        "create schema service_accounts_upgrade; set local search_path to service_accounts_upgrade",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    let current = include_str!("../src/schema.sql");
    let old = current
        .split("-- Several accounts for one service, and the one each Bot uses (#359).")
        .next()
        .unwrap();
    sqlx::raw_sql(old).execute(&mut *tx).await.unwrap();
    sqlx::query("insert into plugin_installation(account_id,name,registry,registry_revision,repository,revision,bundle,installed_at_ms) values ('person','demo','registry','old','repository','old','{}',1)").execute(&mut *tx).await.unwrap();
    sqlx::query("insert into plugin_credential(account_id,plugin_name,connector,secret_id) values ('person','demo','github','plugin/person/demo/github')").execute(&mut *tx).await.unwrap();
    let vault =
        opengrok_store::Vault::from_base64_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
            .unwrap();
    let secret_id = "plugin/person/demo/github";
    let sealed = vault.seal(secret_id, "kept-token").unwrap();
    sqlx::query(
        "insert into secret_store(id,nonce,ciphertext,key_id,updated_at_ms) values ($1,$2,$3,$4,1)",
    )
    .bind(secret_id)
    .bind(&sealed.nonce)
    .bind(&sealed.ciphertext)
    .bind(&sealed.key_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    for _ in 0..2 {
        sqlx::raw_sql(current).execute(&mut *tx).await.unwrap();
        let rows: Vec<(String,String,String)> = sqlx::query_as("select v.id,v.kind,c.secret_id from plugin_credential c join connection_view v on v.id=c.connection_id").fetch_all(&mut *tx).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "token");
        assert_eq!(rows[0].2, secret_id);
        let (nonce, ciphertext, key_id): (Vec<u8>, Vec<u8>, Option<String>) =
            sqlx::query_as("select nonce,ciphertext,key_id from secret_store where id=$1")
                .bind(secret_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(
            vault
                .open(
                    secret_id,
                    &opengrok_store::Sealed {
                        nonce,
                        ciphertext,
                        key_id
                    }
                )
                .unwrap(),
            "kept-token"
        );
        let count: i64 = sqlx::query_scalar("select count(*) from events where stream_id=$1")
            .bind(format!("connection/{}", rows[0].0))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(
            count, 1,
            "replaying boot must not duplicate the account event"
        );
    }
    tx.rollback().await.unwrap();
}
