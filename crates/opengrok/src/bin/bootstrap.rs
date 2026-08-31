//! First-run bootstrap: seed an organization, its admin account, and an invite code.
//!
//! A fresh OpenGrok deployment has no way to create its FIRST org/admin — `OrgCommand::Create` has
//! no HTTP surface (signup only REDEEMS an existing invite, and a new account lands `enabled=false`
//! awaiting an admin who does not exist yet). This binary closes that chicken-and-egg: it writes the
//! org, an enabled+verified admin account whose id is the org's `admin`, and an invite code for the
//! next member — directly through the same aggregates and store the server uses, so the result is
//! indistinguishable from an org grown the normal way.
//!
//! Run it against the server's own database:
//!
//! ```sh
//! OG_DATABASE_URL=… BOOTSTRAP_EMAIL=you@acme.test BOOTSTRAP_PASSWORD='at-least-8' \
//!   cargo run -p opengrok --bin bootstrap
//! ```
//!
//! Optional env: BOOTSTRAP_ORG (display name, default "Acme"), BOOTSTRAP_DOMAIN (default the email's
//! domain), BOOTSTRAP_FIRST / BOOTSTRAP_LAST. It refuses to run if the email already has an account.

use anyhow::{Context, Result, bail};
use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::{Org, OrgCommand};
use opengrok_server::auth::password::hash_password;
use opengrok_store::PgStore;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[tokio::main]
async fn main() -> Result<()> {
    let database_url = std::env::var("OG_DATABASE_URL").context("OG_DATABASE_URL is required")?;
    let email = std::env::var("BOOTSTRAP_EMAIL").context("BOOTSTRAP_EMAIL is required")?;
    let password = std::env::var("BOOTSTRAP_PASSWORD").context("BOOTSTRAP_PASSWORD is required")?;
    if password.len() < 8 {
        bail!("BOOTSTRAP_PASSWORD must be at least 8 characters");
    }
    let domain = std::env::var("BOOTSTRAP_DOMAIN").ok().unwrap_or_else(|| {
        email
            .split('@')
            .nth(1)
            .unwrap_or("acme.test")
            .to_lowercase()
    });
    let org_name = std::env::var("BOOTSTRAP_ORG").unwrap_or_else(|_| "Acme".to_string());
    let first_name = std::env::var("BOOTSTRAP_FIRST").unwrap_or_default();
    let last_name = std::env::var("BOOTSTRAP_LAST").unwrap_or_default();

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .context("connect to Postgres")?;
    opengrok_store::migrations::run(&pool)
        .await
        .context("run migrations")?;
    let store = PgStore::new(pool);

    if store
        .account_by_email(&email)
        .await
        .context("look up existing account")?
        .is_some()
    {
        bail!("an account for {email} already exists — nothing to bootstrap");
    }

    let account_id = AccountId::new();
    let org_id = OrgId::new();
    let at_ms = now_ms();

    // 1. The org, with this account pre-named as its admin.
    let mut org = Org::default();
    let created = org
        .decide(OrgCommand::Create {
            name: org_name.clone(),
            admin: account_id.clone(),
            domains: vec![domain.clone()],
            at_ms,
        })
        .map_err(|error| anyhow::anyhow!("create org: {error}"))?;
    for event in &created {
        org.apply(event);
    }
    let org_seq = store
        .append_org(&org_id, 0, &created, &org, at_ms)
        .await
        .context("append org")?;

    // 2. The admin account: enabled and verified (no admin exists to enable it, so it enables
    //    itself by construction), password hashed the same way signup hashes it.
    let password_hash = hash_password(&password).map_err(|error| anyhow::anyhow!(error))?;
    let register = Account::default()
        .decide(AccountCommand::Register {
            email: email.clone(),
            password_hash: password_hash.clone(),
            first_name: first_name.clone(),
            last_name: last_name.clone(),
            org_id: org_id.as_str().to_string(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .map_err(|error| anyhow::anyhow!("register account: {error}"))?;
    let account = Account::replay(&register);
    let view = AccountView {
        id: account_id.clone(),
        email: email.clone(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(password_hash),
        first_name,
        last_name,
        org_id: Some(org_id.as_str().to_string()),
        verified: account.verified,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&account_id, 0, &register, &view)
        .await
        .context("append account")?;

    // 3. An invite code for the next member of this org.
    let code = format!("inv_{}", uuid::Uuid::now_v7().simple());
    let invite = org
        .decide(OrgCommand::IssueInvite {
            code: code.clone(),
            at_ms,
        })
        .map_err(|error| anyhow::anyhow!("issue invite: {error}"))?;
    for event in &invite {
        org.apply(event);
    }
    store
        .append_org(&org_id, org_seq, &invite, &org, at_ms)
        .await
        .context("append invite")?;

    println!("bootstrapped:");
    println!(
        "  org      {org_id}  ({org_name}, domain {domain})",
        org_id = org_id.as_str()
    );
    println!(
        "  admin    {email}  (account {account_id}, enabled+verified)",
        account_id = account_id.as_str()
    );
    println!("  invite   {code}  (for the next member of this org)");
    Ok(())
}
