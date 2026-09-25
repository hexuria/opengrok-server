//! The credential key: loading it, checking it at boot, and `opengrok vault …`.
//!
//! The key has been lost once already — a reboot regenerated `OG_CREDENTIAL_KEK`, the org's sealed
//! box key stopped opening, and computers looked absent with nothing in the log saying why. So
//! boot now checks what is sealed against what is held and says so loudly, and rotation is a
//! command rather than an operator re-typing every secret.

use opengrok_store::{PgStore, Vault, VaultCheck};

/// `OG_CREDENTIAL_KEK` is the current key; `OG_CREDENTIAL_KEK_OLD` is a comma-separated list of
/// retired ones that still open what they sealed. `None` is a deployment with no key at all.
pub fn from_env() -> Result<Option<Vault>, String> {
    let current = std::env::var("OG_CREDENTIAL_KEK").unwrap_or_default();
    let retired = std::env::var("OG_CREDENTIAL_KEK_OLD").unwrap_or_default();
    let retired: Vec<&str> = retired
        .split(',')
        .map(str::trim)
        .filter(|kek| !kek.is_empty())
        .collect();
    if current.trim().is_empty() {
        // Refused rather than ignored: the likely cause is a rotation half done — the old key moved
        // to _OLD and the new one never set — and booting would seal nothing and open nothing.
        if retired.is_empty() {
            return Ok(None);
        }
        return Err(
            "OG_CREDENTIAL_KEK_OLD is set but OG_CREDENTIAL_KEK is not: a retired key \
             needs a current one to seal under; generate one with `openssl rand -base64 32`"
                .to_string(),
        );
    }
    Vault::from_base64_keys(&current, &retired)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The boot canary. Never stops the boot — a server whose saved logins will not open still runs
/// coworkers — but a lost key is an `error!` with the fix in it, not a silence.
pub async fn check_at_boot(store: &PgStore, vault: Option<&Vault>) {
    let check = match store.vault_check(vault).await {
        Ok(check) => check,
        Err(error) => {
            tracing::error!(%error, "could not check the sealed credentials against OG_CREDENTIAL_KEK");
            return;
        }
    };
    if let Some(problem) = check.problem() {
        tracing::error!(
            lost_key_ids = ?check.lost,
            failed_key_ids = ?check.failed,
            legacy_rows = check.legacy,
            "{problem}"
        );
    } else if check.to_reseal() > 0 {
        tracing::warn!(
            rows = check.to_reseal(),
            "credentials sealed under a retired key or before key ids were recorded; run `opengrok vault reseal` so a lost key is caught by id"
        );
    }
    match vault {
        Some(vault) => {
            tracing::info!(key_id = vault.key_id(), retired = ?vault.retired_key_ids(), rows = check.current, "credential vault ready")
        }
        None => tracing::info!(
            "no OG_CREDENTIAL_KEK — connectors, site logins and org computer keys cannot be stored"
        ),
    }
}

fn print_check(vault: &Vault, check: &VaultCheck) {
    println!("current key:  {} ({} rows)", vault.key_id(), check.current);
    for id in vault.retired_key_ids() {
        println!("retired key:  {id}");
    }
    println!("retired rows: {}", check.retired);
    println!(
        "no key id:    {} (sealed before key ids were recorded)",
        check.legacy
    );
    for (id, rows) in &check.lost {
        println!("LOST key:     {id} ({rows} rows) — this server does not hold it");
    }
    match check.problem() {
        Some(problem) => println!("problem:      {problem}"),
        None if check.to_reseal() > 0 => println!("to do:        run `opengrok vault reseal`"),
        None => println!("every row opens under the current key"),
    }
}

/// `opengrok vault status` and `opengrok vault reseal`. Counts and key ids only, never a value.
pub async fn run(args: &[String]) -> Result<(), String> {
    let usage = "usage:\n  opengrok vault status   (which keys sealed what; exits 1 on a problem)\n  opengrok vault reseal   (re-seal every row under OG_CREDENTIAL_KEK; resumable)";
    let command = args.first().map(String::as_str);
    if !matches!(command, Some("status" | "reseal")) || args.len() > 1 {
        return Err(usage.to_string());
    }
    let vault = from_env()?.ok_or("OG_CREDENTIAL_KEK is not set")?;
    let store = crate::admin::store().await?;
    let mut unopenable = 0;
    if command == Some("reseal") {
        let report = store
            .reseal_secrets(&vault, "")
            .await
            .map_err(|error| error.to_string())?;
        println!("resealed:        {}", report.resealed);
        println!("already current: {}", report.already_current);
        println!(
            "changed meanwhile by the server (already current): {}",
            report.raced
        );
        println!(
            "unopenable:      {} (kept; they open again if their key is put back)",
            report.unopenable.len()
        );
        for id in report.unopenable.iter().take(50) {
            println!("  {id}");
        }
        if report.unopenable.len() > 50 {
            println!("  … and {} more", report.unopenable.len() - 50);
        }
        unopenable = report.unopenable.len();
    }
    let check = store
        .vault_check(Some(&vault))
        .await
        .map_err(|error| error.to_string())?;
    print_check(&vault, &check);
    if check.problem().is_some() || unopenable > 0 {
        return Err("some sealed credentials do not open with the keys given".to_string());
    }
    Ok(())
}
