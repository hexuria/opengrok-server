//! `opengrok admin …` — the operator's identity commands, run on the box.
//!
//! Bootstrap lives here rather than on an HTTP surface: a fresh server has no admin to authorize
//! an admin call, and the operator has shell. So the first org, the first admin, invite codes,
//! account enablement, and direct test-account creation are all CLI — no web attack surface, and
//! the operator's own shell is the authorization.
//!
//! Args are parsed by hand (no clap dependency in this binary): `--flag value` pairs after the
//! subcommand. Unknown flags are refused rather than ignored, so a typo does not silently drop a
//! domain or a password.

use std::collections::HashMap;

use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
use opengrok_core::id::{AccountId, OrgId};
use opengrok_core::org::{Org, OrgCommand};
use opengrok_store::PgStore;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// A short human-friendly random token — for invite codes and generated passwords.
fn random_token(prefix: &str) -> String {
    use rand::RngExt;
    let bytes: [u8; 12] = rand::rng().random();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{hex}")
}

/// Parse `--flag value` pairs; every flag must be in `allowed` or it is an error.
fn parse_flags(args: &[String], allowed: &[&str]) -> Result<HashMap<String, String>, String> {
    let mut flags = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let key = args[i]
            .strip_prefix("--")
            .ok_or_else(|| format!("expected a --flag, got {}", args[i]))?;
        if !allowed.contains(&key) {
            return Err(format!(
                "unknown flag --{key}; allowed: {}",
                allowed.join(", ")
            ));
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("--{key} needs a value"))?;
        flags.insert(key.to_string(), value.clone());
        i += 2;
    }
    Ok(flags)
}

async fn store() -> Result<PgStore, String> {
    let url =
        std::env::var("OG_DATABASE_URL").map_err(|_| "OG_DATABASE_URL is required".to_string())?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .map_err(|error| format!("connect: {error}"))?;
    opengrok_store::migrations::run(&pool)
        .await
        .map_err(|error| format!("migrations: {error}"))?;
    Ok(PgStore::new(pool))
}

/// Mint a ready account (verified + enabled) with a password — the admin, and every test account.
async fn mint_account(
    store: &PgStore,
    email: &str,
    password: &str,
    first: &str,
    last: &str,
    org_id: &str,
) -> Result<AccountId, String> {
    if store
        .account_by_email(email)
        .await
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err(format!("an account with email {email} already exists"));
    }
    let hash = opengrok_server::auth::password::hash_password(password)?;
    let id = AccountId::new();
    let at_ms = now_ms();
    let events = Account::default()
        .decide(AccountCommand::Register {
            email: email.to_string(),
            password_hash: hash.clone(),
            first_name: first.to_string(),
            last_name: last.to_string(),
            org_id: org_id.to_string(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .map_err(|e| e.to_string())?;
    let view = AccountView {
        id: id.clone(),
        email: email.to_string(),
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some(hash),
        first_name: first.to_string(),
        last_name: last.to_string(),
        org_id: Some(org_id.to_string()),
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&id, 0, &events, &view)
        .await
        .map_err(|e| e.to_string())?;
    Ok(id)
}

fn split_name(full: &str) -> (String, String) {
    let mut parts = full.trim().splitn(2, ' ');
    let first = parts.next().unwrap_or("").to_string();
    let last = parts.next().unwrap_or("").to_string();
    (first, last)
}

/// Returns `Some(exit_code)` when argv names an admin command (handled here), `None` to fall
/// through to the server.
pub async fn maybe_run() -> Option<i32> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) != Some("admin") {
        return None;
    }
    let result = run(&argv[2..]).await;
    match result {
        Ok(()) => Some(0),
        Err(message) => {
            eprintln!("error: {message}");
            Some(1)
        }
    }
}

async fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("org") if args.get(1).map(String::as_str) == Some("create") => {
            let flags = parse_flags(&args[2..], &["name", "admin-email", "domain", "password"])?;
            let name = flags.get("name").ok_or("--name is required")?;
            let admin_email = flags.get("admin-email").ok_or("--admin-email is required")?;
            let domains: Vec<String> = flags
                .get("domain")
                .ok_or("--domain is required (comma-separate several)")?
                .split(',')
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty())
                .collect();
            // The admin's email domain must be one of the org's, or the admin could not sign in.
            let admin_domain = opengrok_core::org::email_domain(admin_email)
                .ok_or("--admin-email is not an email")?;
            if !domains.iter().any(|d| opengrok_core::org::normalize_domain(d) == admin_domain) {
                return Err("the admin's email domain must be one of the org's --domain values".to_string());
            }
            let password = flags
                .get("password")
                .cloned()
                .unwrap_or_else(|| random_token("pw_"));

            let store = store().await?;
            let org_id = OrgId::new();
            let (first, last) = split_name(name);
            let admin_id = mint_account(&store, admin_email, &password, &first, &last, org_id.as_str()).await?;

            let at_ms = now_ms();
            let events = Org::default()
                .decide(OrgCommand::Create {
                    name: name.clone(),
                    admin: admin_id.clone(),
                    domains: domains.clone(),
                    at_ms,
                })
                .map_err(|e| e.to_string())?;
            let state = Org::replay(&events);
            store
                .append_org(&org_id, 0, &events, &state, at_ms)
                .await
                .map_err(|e| e.to_string())?;

            println!("org created");
            println!("  org id:   {org_id}");
            println!("  admin:    {admin_email} ({admin_id})");
            println!("  domains:  {}", domains.join(", "));
            if !flags.contains_key("password") {
                println!("  password: {password}   (generated — save it)");
            }
            Ok(())
        }

        // The operator's shell vouches for a domain: no DNS proof, because whoever runs this
        // already owns the deployment. A console admin has to prove theirs (`/admin/domains`).
        Some("org")
            if args.get(1).map(String::as_str) == Some("domain")
                && args.get(2).map(String::as_str) == Some("add") =>
        {
            let flags = parse_flags(&args[3..], &["org", "domain"])?;
            let org_id = OrgId::from_stored(flags.get("org").ok_or("--org is required")?.clone());
            let domain = flags.get("domain").ok_or("--domain is required")?.clone();
            let store = store().await?;
            let (org, seq) = store.load_org(&org_id).await.map_err(|e| e.to_string())?;
            let at_ms = now_ms();
            let events = org
                .decide(OrgCommand::AddDomain {
                    domain: domain.clone(),
                    at_ms,
                })
                .map_err(|e| e.to_string())?;
            let mut after = org;
            for event in &events {
                after.apply(event);
            }
            store
                .append_org(&org_id, seq, &events, &after, at_ms)
                .await
                .map_err(|e| e.to_string())?;
            println!("domain added (operator-vouched): {}", opengrok_core::org::normalize_domain(&domain));
            println!("  domains: {}", after.domains.join(", "));
            Ok(())
        }

        Some("invite") => {
            let flags = parse_flags(&args[1..], &["org"])?;
            let org_id = OrgId::from_stored(flags.get("org").ok_or("--org is required")?.clone());
            let store = store().await?;
            let (org, seq) = store.load_org(&org_id).await.map_err(|e| e.to_string())?;
            let code = random_token("inv_");
            let at_ms = now_ms();
            let events = org
                .decide(OrgCommand::IssueInvite {
                    code: code.clone(),
                    at_ms,
                })
                .map_err(|e| e.to_string())?;
            let mut after = org;
            for event in &events {
                after.apply(event);
            }
            store
                .append_org(&org_id, seq, &events, &after, at_ms)
                .await
                .map_err(|e| e.to_string())?;
            println!("invite code: {code}");
            Ok(())
        }

        Some("account") if args.get(1).map(String::as_str) == Some("create") => {
            let flags = parse_flags(&args[2..], &["email", "org", "name", "password"])?;
            let email = flags.get("email").ok_or("--email is required")?;
            let org_id = flags.get("org").ok_or("--org is required")?;
            let (first, last) = split_name(flags.get("name").map(String::as_str).unwrap_or(""));
            let password = flags
                .get("password")
                .cloned()
                .unwrap_or_else(|| random_token("pw_"));
            let store = store().await?;
            let id = mint_account(&store, email, &password, &first, &last, org_id).await?;
            println!("account created (verified + enabled)");
            println!("  id:    {id}");
            println!("  email: {email}");
            if !flags.contains_key("password") {
                println!("  password: {password}   (generated — save it)");
            }
            Ok(())
        }

        // The no-mailer reset path: a person who forgot their password asks the operator, who
        // sets a new one here. Sessions already issued stay valid — the same as a self-service
        // change, and the account's own sign-out is the remedy.
        Some("account") if args.get(1).map(String::as_str) == Some("password") => {
            let flags = parse_flags(&args[2..], &["email", "password"])?;
            let email = flags.get("email").ok_or("--email is required")?;
            let password = flags
                .get("password")
                .cloned()
                .unwrap_or_else(|| random_token("pw_"));
            if password.len() < 8 {
                return Err("the password must be at least 8 characters".to_string());
            }
            let store = store().await?;
            let view = store
                .account_by_email(email)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no account with email {email}"))?;
            let (account, seq) = store.load_account(&view.id).await.map_err(|e| e.to_string())?;
            let hash = opengrok_server::auth::password::hash_password(&password)?;
            let at_ms = now_ms();
            let events = account
                .decide(AccountCommand::ChangePassword {
                    password_hash: hash,
                    at_ms,
                })
                .map_err(|e| e.to_string())?;
            let mut after = account;
            for event in &events {
                after.apply(event);
            }
            let updated = AccountView {
                id: view.id.clone(),
                email: after.email.clone(),
                plan: after.plan.unwrap_or(Plan::Ultra),
                trial: after.trial,
                updated_at_ms: at_ms,
                password_hash: after.password_hash.clone(),
                first_name: after.first_name.clone(),
                last_name: after.last_name.clone(),
                org_id: after.org_id.clone(),
                verified: after.verified,
                enabled: after.enabled,
                avatar_url: after.avatar_url.clone(),
            };
            store
                .append_account(&view.id, seq, &events, &updated)
                .await
                .map_err(|e| e.to_string())?;
            println!("password set: {email}");
            if !flags.contains_key("password") {
                println!("  password: {password}   (generated — hand it over out of band)");
            }
            Ok(())
        }

        Some("account") if args.get(1).map(String::as_str) == Some("enable") => {
            let flags = parse_flags(&args[2..], &["email"])?;
            let email = flags.get("email").ok_or("--email is required")?;
            let store = store().await?;
            let view = store
                .account_by_email(email)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no account with email {email}"))?;
            let (account, seq) = store.load_account(&view.id).await.map_err(|e| e.to_string())?;
            let at_ms = now_ms();
            let events = account
                .decide(AccountCommand::Enable { at_ms })
                .map_err(|e| e.to_string())?;
            let mut after = account;
            for event in &events {
                after.apply(event);
            }
            let updated = AccountView {
                id: view.id.clone(),
                email: after.email.clone(),
                plan: after.plan.unwrap_or(Plan::Ultra),
                trial: after.trial,
                updated_at_ms: at_ms,
                password_hash: after.password_hash.clone(),
                first_name: after.first_name.clone(),
                last_name: after.last_name.clone(),
                org_id: after.org_id.clone(),
                verified: after.verified,
                enabled: after.enabled,
                avatar_url: after.avatar_url.clone(),
            };
            store
                .append_account(&view.id, seq, &events, &updated)
                .await
                .map_err(|e| e.to_string())?;
            println!("account enabled: {email}");
            Ok(())
        }

        _ => Err(concat!(
            "usage:\n",
            "  opengrok admin org create --name <name> --admin-email <email> --domain <d[,d]> [--password <p>]\n",
            "  opengrok admin org domain add --org <org_id> --domain <domain>   (operator-vouched, no DNS proof)\n",
            "  opengrok admin invite --org <org_id>\n",
            "  opengrok admin account create --email <email> --org <org_id> --name \"<First Last>\" [--password <p>]\n",
            "  opengrok admin account enable --email <email>\n",
            "  opengrok admin account password --email <email> [--password <p>]   (the no-mailer reset)"
        )
        .to_string()),
    }
}
