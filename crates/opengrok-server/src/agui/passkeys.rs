//! A saved passkey, used or made in the bot's browser.
//!
//! The person's passkey lives in their vault: the public half on the row, the private key
//! sealed apart, never revealed to the Mac. A sign-in with it happens in the box's Chromium
//! through the DevTools pipe the server holds (`opengrok_box::devtools`): a platform-shaped
//! virtual authenticator is added to the page, the key is loaded into it, the bot clicks the
//! site's passkey button, the site's challenge is signed, and the key is taken out again.
//! The key is in Chromium's memory for that window and nowhere else in the box.
//!
//! The order matters: Chromium answers "no credentials" at once when the site asks before the
//! key is loaded, so the person confirms first (Touch ID on the Mac, the card), the key is
//! loaded, and only then is the bot told to click.
//!
//! Registration is the same dance without a key: an empty authenticator, the bot clicks the
//! site's "add a passkey" button, the site makes one, and the new key is sealed into the vault.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use opengrok_box::devtools::{DevTools, Passkey};
use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_store::{PasskeyWrite, SiteLoginWrite};
use opengrok_tools::user_form::FormRequest;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::host_state::HostState;

/// How long a loaded key waits for the site to ask before it is taken out again.
const ASSERTION_PATIENCE: Duration = Duration::from_secs(120);

/// One pipe per box, held by the server for as long as it runs.
fn pipes() -> &'static Mutex<HashMap<String, Arc<DevTools>>> {
    static PIPES: OnceLock<Mutex<HashMap<String, Arc<DevTools>>>> = OnceLock::new();
    PIPES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The box's Chromium on the pipe: the one already held, or a fresh one. A browser started
/// any other way (the dock, an earlier server) has no pipe and is replaced, tabs and all;
/// the page is reopened by the caller. Nothing in the box is touched unless the computer
/// offers a pipe at all. Returns the pipe and whether the browser was replaced.
async fn ensure_devtools(
    computer: &Arc<dyn opengrok_box::Computer>,
    box_id: &str,
) -> Result<(Arc<DevTools>, bool), String> {
    if !computer.offers_a_pipe() {
        return Err("this computer has no DevTools pipe".to_string());
    }
    // The registry lock is held only to look, never across a call on the pipe: a hung pipe
    // on one box must not stall every other box's passkey.
    let held = pipes().lock().await.get(box_id).cloned();
    if let Some(held) = held
        && held
            .call("Browser.getVersion", serde_json::json!({}), None)
            .await
            .is_ok()
    {
        return Ok((held, false));
    }
    let _ = computer
        .run(
            box_id,
            "pkill -x chromium >/dev/null 2>&1 || true; sleep 1",
            15,
        )
        .await;
    let fresh = computer
        .devtools(box_id, "about:blank")
        .await
        .map_err(|error| format!("the browser did not come up on the pipe: {error}"))?;
    let fresh = Arc::new(fresh);
    pipes()
        .lock()
        .await
        .insert(box_id.to_string(), fresh.clone());
    Ok((fresh, true))
}

/// A page attached for one passkey, and what the bot has to be told about it.
struct ReadyPage {
    devtools: Arc<DevTools>,
    page: opengrok_box::devtools::AttachedPage,
    /// The tab was opened just now at the site's front page (the browser was replaced, or
    /// no tab was on the site): the bot has to find its way back to the sign-in page.
    reopened: bool,
}

impl ReadyPage {
    fn where_to_click(&self) -> &'static str {
        if self.reopened {
            "The browser was reopened at the site's front page: go back to the sign-in page \
             first, then click"
        } else {
            "Click"
        }
    }
}

/// The box and the page the card is about, attached and ready for WebAuthn.
async fn page_session(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form: &FormRequest,
) -> Result<ReadyPage, String> {
    let runner = crate::agui::routes::tools_for_coworker(
        &state.agui,
        account_id,
        coworker_id,
        &[],
        &[],
        crate::agui::routes::TURN_WAKE_PATIENCE,
    )
    .await
    .ok_or_else(|| "this bot has no computer".to_string())?;
    let (computer, box_id) = runner
        .fill_target()
        .ok_or_else(|| "this bot has no computer".to_string())?;
    runner.wake_fill_target().await?;
    let (devtools, replaced) = ensure_devtools(&computer, &box_id).await?;
    let host = form
        .live_host
        .as_deref()
        .or(form.domain.as_deref())
        .map(opengrok_tools::credential::normalize_origin)
        .filter(|h| !h.is_empty());
    let page = devtools
        .attach_to_page(host.as_deref())
        .await
        .map_err(|error| format!("could not attach to the page: {error}"))?;
    if page.opened {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Ok(ReadyPage {
        devtools,
        reopened: replaced || page.opened,
        page,
    })
}

/// Load the person's passkey into the page for one sign-in. Returns the sentence the bot is
/// told; the key is taken out after the site's challenge is signed, or after the patience.
pub async fn use_passkey(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form: &FormRequest,
    login_id: &str,
) -> Result<String, String> {
    let vault = state
        .agui
        .vault
        .as_deref()
        .ok_or_else(|| "the credential vault is not configured on this server".to_string())?;
    let row = state
        .agui
        .auth
        .store
        .site_logins(account_id)
        .await
        .map_err(|error| error.to_string())?
        .into_iter()
        .find(|row| row.id == login_id && row.kind == "passkey")
        .ok_or_else(|| "no such passkey".to_string())?;
    let meta = row
        .passkey
        .clone()
        .ok_or_else(|| "that row has no passkey".to_string())?;
    // The page first: the key is opened only once there is somewhere to put it. From here
    // on every way out gives the page back, so a tab this flow opened is not left behind.
    let ready = page_session(state, account_id, coworker_id, form).await?;
    let where_to_click = ready.where_to_click();
    let ReadyPage { devtools, page, .. } = ready;
    let key = match state
        .agui
        .auth
        .store
        .open_site_login(vault, account_id, login_id)
        .await
    {
        Ok(Some(secrets)) => secrets.passkey_key,
        Ok(None) => None,
        Err(error) => {
            let _ = devtools.close_page(&page).await;
            return Err(error.to_string());
        }
    };
    let Some(key) = key else {
        let _ = devtools.close_page(&page).await;
        return Err("the passkey's key is missing".to_string());
    };
    let passkey = Passkey {
        rp_id: meta.rp_id.clone(),
        credential_id_b64: meta.credential_id_b64.clone(),
        user_handle_b64: meta.user_handle_b64.clone(),
        private_key_b64: key,
        user_name: row.username.clone(),
    };
    let session = page.session_id.clone();
    let events = devtools.events();
    let authenticator = match devtools.add_platform_authenticator(&session).await {
        Ok(id) => id,
        Err(error) => {
            let _ = devtools.close_page(&page).await;
            return Err(format!("could not add the authenticator: {error}"));
        }
    };
    if let Err(error) = devtools
        .add_credential(&session, &authenticator, &passkey)
        .await
    {
        let _ = devtools
            .remove_authenticator(&session, &authenticator)
            .await;
        let _ = devtools.close_page(&page).await;
        return Err(format!("could not load the passkey: {error}"));
    }
    let store = state.agui.auth.store.clone();
    let account = account_id.clone();
    let id = login_id.to_string();
    let credential_id = passkey.credential_id_b64.clone();
    tokio::spawn(async move {
        let asserted = devtools
            .wait_event(
                events,
                "WebAuthn.credentialAsserted",
                Some(&session),
                ASSERTION_PATIENCE,
            )
            .await;
        match asserted {
            Ok(_) => {
                let _ = store
                    .touch_site_login_used(&account, &id, chrono::Utc::now().timestamp_millis())
                    .await;
                tracing::info!(login = %id, "a passkey signed a site's challenge");
            }
            Err(error) => tracing::info!(%error, login = %id, "a loaded passkey was not asked for"),
        }
        let _ = devtools
            .remove_credential(&session, &authenticator, &credential_id)
            .await;
        let _ = devtools
            .remove_authenticator(&session, &authenticator)
            .await;
        let _ = devtools.close_page(&page).await;
    });
    Ok(format!(
        "The passkey for {} as {} is loaded in the browser for the next two minutes. \
         {where_to_click} the site's passkey button now (\"Sign in with a passkey\", \
         \"Continue\", or the key icon); the site's challenge is signed out of your view and no \
         password is typed. Then screenshot and confirm what the page shows.",
        meta.rp_id, row.username
    ))
}

/// Hold an empty authenticator up to the page so the site can make a passkey; the new key is
/// sealed into the person's vault the moment the site hands it over.
pub async fn register_passkey(
    state: &HostState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    form: &FormRequest,
    username_hint: &str,
) -> Result<String, String> {
    if state.agui.vault.is_none() {
        return Err("the credential vault is not configured on this server".to_string());
    }
    let ready = page_session(state, account_id, coworker_id, form).await?;
    let where_to_click = ready.where_to_click();
    let ReadyPage { devtools, page, .. } = ready;
    let session = page.session_id.clone();
    let events = devtools.events();
    let authenticator = match devtools.add_platform_authenticator(&session).await {
        Ok(id) => id,
        Err(error) => {
            let _ = devtools.close_page(&page).await;
            return Err(format!("could not add the authenticator: {error}"));
        }
    };
    let state = state.clone();
    let account = account_id.clone();
    let hint = username_hint.to_string();
    let site: String = form
        .live_host
        .as_deref()
        .or(form.domain.as_deref())
        .map(opengrok_tools::credential::normalize_origin)
        .unwrap_or_default();
    let site_for_task = site.clone();
    tokio::spawn(async move {
        let site = site_for_task;
        let added = devtools
            .wait_event(
                events,
                "WebAuthn.credentialAdded",
                Some(&session),
                ASSERTION_PATIENCE,
            )
            .await;
        match added {
            Ok(params) => match seal_new_passkey(&state, &account, &params, &hint, &site).await {
                Ok(row_id) => tracing::info!(login = %row_id, "a site made a passkey; sealed"),
                Err(error) => {
                    tracing::error!(%error, "a site made a passkey that could not be sealed")
                }
            },
            Err(error) => tracing::info!(%error, "the site made no passkey"),
        }
        let _ = devtools
            .remove_authenticator(&session, &authenticator)
            .await;
        let _ = devtools.close_page(&page).await;
    });
    Ok(format!(
        "A passkey holder is ready in the browser for the next two minutes. {where_to_click} \
         the site's \"Create a passkey\" / \"Add a passkey\" button now; the site makes one, and \
         it is saved to the person's logins for {site}. Then screenshot and confirm what the page \
         shows."
    ))
}

/// The credential from a `WebAuthn.credentialAdded` event into a vault row.
async fn seal_new_passkey(
    state: &HostState,
    account_id: &AccountId,
    params: &Value,
    username_hint: &str,
    site: &str,
) -> Result<String, String> {
    let credential = params
        .get("credential")
        .ok_or_else(|| "no credential on the event".to_string())?;
    let text = |key: &str| {
        credential
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let rp_id = text("rpId");
    let private_key = text("privateKey");
    let credential_id = text("credentialId");
    if rp_id.is_empty() || private_key.is_empty() || credential_id.is_empty() {
        return Err("the credential is missing its key, id or relying party".to_string());
    }
    let username = {
        let named = text("userName");
        if named.is_empty() {
            username_hint.to_string()
        } else {
            named
        }
    };
    let username = if username.is_empty() {
        "passkey".to_string()
    } else {
        username
    };
    let origin = if site.is_empty() {
        rp_id.clone()
    } else {
        site.to_string()
    };
    let vault = state
        .agui
        .vault
        .as_deref()
        .ok_or_else(|| "no vault".to_string())?;
    let write = SiteLoginWrite {
        origin: &origin,
        username: &username,
        label: &format!("{origin} passkey"),
        kind: "passkey",
        notes: "",
        password: None,
        otpauth: None,
        passkey: Some(PasskeyWrite {
            credential_id_b64: credential_id,
            rp_id,
            user_handle_b64: text("userHandle"),
            private_key_b64: private_key,
        }),
    };
    state
        .agui
        .auth
        .store
        .upsert_site_login(
            vault,
            account_id,
            &write,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
        .map(|row| row.id)
        .map_err(|error| error.to_string())
}
