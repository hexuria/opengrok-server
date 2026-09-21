//! A person's saved site logins, kept on the server so they follow them to every Mac.
//!
//! The rows are the person's own: every route reads the account from the bearer and never
//! from the body, and a row that is not theirs is "no such row". The password, and the seed
//! of an authenticator code when the row has one, are sealed in the vault under keys that
//! carry the account id, and they leave the server in exactly one place —
//! `POST /site-logins/{id}/reveal`, which NativeChat calls after the person passed Touch ID
//! on their Mac, to fill a login or a code card. A list never carries them.
//!
//! `GET /site-logins/icon/{origin}` fetches a site's icon for the list, from the server so
//! the person's Mac does not announce every site they have a login for. The fetch refuses
//! anything that is not a public host, follows no redirect off the site, and caps what it
//! reads.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use opengrok_core::id::AccountId;
use opengrok_store::SiteLoginWrite;
use serde_json::{Value, json};

use crate::host_state::HostState;

pub fn agui_router(state: HostState) -> Router {
    Router::new()
        .route("/site-logins", get(list).post(save))
        .route("/site-logins/{id}", delete(remove).patch(update))
        .route("/site-logins/{id}/reveal", post(reveal))
        .route("/site-logins/icon/{origin}", get(icon))
        .with_state(state)
}

/// Longest origin, username, title or password the vault takes. Nothing legitimate is
/// longer, and a bound keeps a runaway client from filling the vault.
const MAX_FIELD_CHARS: usize = 512;
/// Notes may be longer, but not a document.
const MAX_NOTES_CHARS: usize = 8_000;
/// The kinds a save may name. Passkeys are made by their own flow, never posted here.
const KINDS: [&str; 2] = ["password", "code"];

fn reply(code: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        axum::Json(body),
    )
        .into_response()
}

fn signed_in(state: &HostState, headers: &HeaderMap) -> Option<AccountId> {
    crate::agui::routes::account_from_bearer(&state.agui, headers)
}

fn sign_in_first() -> Response {
    (StatusCode::UNAUTHORIZED, "sign in first").into_response()
}

fn no_vault() -> Response {
    reply(
        503,
        json!({ "error": "the credential vault is not configured on this server (set OG_CREDENTIAL_KEK)" }),
    )
}

fn row_json(row: &opengrok_store::SiteLoginRow) -> Value {
    json!({
        "id": row.id,
        "origin": row.origin,
        "username": row.username,
        "label": row.label,
        "kind": row.kind,
        "notes": row.notes,
        "createdAtMs": row.created_at_ms,
        "updatedAtMs": row.updated_at_ms,
        "lastUsedAtMs": row.last_used_at_ms,
    })
}

async fn list(State(state): State<HostState>, headers: HeaderMap) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    match state.agui.auth.store.site_logins(&account_id).await {
        Ok(rows) => reply(200, Value::Array(rows.iter().map(row_json).collect())),
        Err(error) => {
            tracing::error!(%error, "could not list site logins");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

/// A string field of the body: trimmed unless it is a secret, absent when empty.
fn field(args: &Value, key: &str, trim: bool) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|text| if trim { text.trim() } else { text })
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// `{ origin, username, password?, otpauth?, label?, notes?, kind? }`. A `password` row
/// needs a password; a `code` row needs an `otpauth://` seed and may have no password. The
/// origin is stored as the bare host, lowercase.
async fn save(
    State(state): State<HostState>,
    headers: HeaderMap,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    let Some(vault) = state.agui.vault.as_deref() else {
        return no_vault();
    };
    let (Some(origin), Some(username)) =
        (field(&args, "origin", true), field(&args, "username", true))
    else {
        return reply(400, json!({ "error": "origin and username are required" }));
    };
    let password = field(&args, "password", false);
    let otpauth = field(&args, "otpauth", true);
    let label = field(&args, "label", true).unwrap_or_default();
    let notes = field(&args, "notes", false).unwrap_or_default();
    let kind = field(&args, "kind", true).unwrap_or_else(|| "password".to_string());
    if !KINDS.contains(&kind.as_str()) {
        return reply(400, json!({ "error": "kind is password or code" }));
    }
    if kind == "password" && password.is_none() {
        return reply(400, json!({ "error": "a password row needs a password" }));
    }
    if kind == "code" && otpauth.is_none() {
        return reply(400, json!({ "error": "a code row needs an otpauth seed" }));
    }
    if let Some(seed) = &otpauth
        && !seed.starts_with("otpauth://")
    {
        return reply(400, json!({ "error": "otpauth must be an otpauth:// URI" }));
    }
    let too_long = [&origin, &username, &label]
        .iter()
        .any(|text| text.chars().count() > MAX_FIELD_CHARS)
        || password
            .as_ref()
            .is_some_and(|p| p.chars().count() > MAX_FIELD_CHARS)
        || otpauth
            .as_ref()
            .is_some_and(|o| o.chars().count() > MAX_FIELD_CHARS * 4)
        || notes.chars().count() > MAX_NOTES_CHARS;
    if too_long {
        return reply(400, json!({ "error": "a field is too long" }));
    }
    // The bare host, lowercase: `https://X.com/login` and `x.com` are one site.
    let origin = opengrok_tools::credential::normalize_origin(&origin).to_ascii_lowercase();
    if origin.is_empty() {
        return reply(400, json!({ "error": "origin is not a site" }));
    }
    let write = SiteLoginWrite {
        origin: &origin,
        username: &username,
        label: &label,
        kind: &kind,
        notes: &notes,
        password: password.as_deref(),
        otpauth: otpauth.as_deref(),
    };
    match state
        .agui
        .auth
        .store
        .upsert_site_login(
            vault,
            &account_id,
            &write,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    {
        Ok(row) => {
            tracing::info!(account = %account_id, origin = %row.origin, kind = %row.kind, "saved a site login");
            reply(200, row_json(&row))
        }
        Err(error) => {
            tracing::error!(%error, "could not save a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

/// `{ label?, notes? }` on one of the person's rows.
async fn update(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    axum::Json(args): axum::Json<Value>,
) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    let label = args.get("label").and_then(Value::as_str).map(str::trim);
    let notes = args.get("notes").and_then(Value::as_str);
    if label.is_none() && notes.is_none() {
        return reply(400, json!({ "error": "label or notes is required" }));
    }
    if label.is_some_and(|l| l.is_empty() || l.chars().count() > MAX_FIELD_CHARS)
        || notes.is_some_and(|n| n.chars().count() > MAX_NOTES_CHARS)
    {
        return reply(400, json!({ "error": "a field is empty or too long" }));
    }
    match state
        .agui
        .auth
        .store
        .update_site_login(
            &account_id,
            &id,
            label,
            notes,
            chrono::Utc::now().timestamp_millis(),
        )
        .await
    {
        Ok(true) => reply(200, json!({ "ok": true, "id": id })),
        Ok(false) => reply(404, json!({ "error": "no such site login" })),
        Err(error) => {
            tracing::error!(%error, "could not update a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

async fn remove(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    match state
        .agui
        .auth
        .store
        .delete_site_login(&account_id, &id)
        .await
    {
        Ok(true) => reply(200, json!({ "ok": true, "id": id })),
        Ok(false) => reply(404, json!({ "error": "no such site login" })),
        Err(error) => {
            tracing::error!(%error, "could not delete a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

/// The one place a saved secret leaves the server: to the owner's own app, which asked
/// after Touch ID. The app sends its bearer in the header; the console's cookie does not
/// open this door, so a script running in the console cannot either. Logged without the
/// values.
async fn reveal(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if headers.get(header::AUTHORIZATION).is_none() {
        return sign_in_first();
    }
    let Some(account_id) = signed_in(&state, &headers) else {
        return sign_in_first();
    };
    let Some(vault) = state.agui.vault.as_deref() else {
        return no_vault();
    };
    match state
        .agui
        .auth
        .store
        .open_site_login(vault, &account_id, &id)
        .await
    {
        Ok(Some(secrets)) => {
            tracing::info!(account = %account_id, id = %id, "revealed a site login to its owner's app");
            reply(
                200,
                json!({ "id": id, "password": secrets.password, "otpauth": secrets.otpauth }),
            )
        }
        Ok(None) => reply(404, json!({ "error": "no such site login" })),
        Err(error) => {
            tracing::error!(%error, "could not open a site login");
            reply(500, json!({ "error": "store unavailable" }))
        }
    }
}

// ---- icons -------------------------------------------------------------------------------

/// How long an icon, or the fact that a site has none, is kept.
const ICON_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// An icon larger than this is not an icon.
const ICON_MAX_BYTES: usize = 64 * 1024;
/// How much of a site's front page is read to find its `<link rel="icon">`.
const PAGE_MAX_BYTES: usize = 256 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

struct CachedIcon {
    at: Instant,
    icon: Option<(String, Vec<u8>)>,
}

fn icon_cache() -> &'static Mutex<HashMap<String, CachedIcon>> {
    static CACHE: OnceLock<Mutex<HashMap<String, CachedIcon>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `GET /site-logins/icon/{origin}` → the image, or 204 when the site has none we could
/// find. The origin is a host as the vault keys it; anything else is 400.
async fn icon(
    State(state): State<HostState>,
    headers: HeaderMap,
    Path(origin): Path<String>,
) -> Response {
    if signed_in(&state, &headers).is_none() {
        return sign_in_first();
    }
    let Some(host) = icon_host(&origin) else {
        return reply(400, json!({ "error": "origin is not a site" }));
    };
    if let Some(hit) = icon_cache().lock().ok().and_then(|cache| {
        cache
            .get(&host)
            .filter(|c| c.at.elapsed() < ICON_TTL)
            .map(|c| c.icon.clone())
    }) {
        return icon_reply(hit);
    }
    let found = fetch_icon(&host).await;
    if let Ok(mut cache) = icon_cache().lock() {
        cache.retain(|_, c| c.at.elapsed() < ICON_TTL);
        cache.insert(
            host.clone(),
            CachedIcon {
                at: Instant::now(),
                icon: found.clone(),
            },
        );
    }
    icon_reply(found)
}

fn icon_reply(icon: Option<(String, Vec<u8>)>) -> Response {
    match icon {
        Some((mime, bytes)) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "private, max-age=86400".to_string()),
            ],
            bytes,
        )
            .into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

/// A host and nothing else: lowercase letters, digits, dots and hyphens, at least one dot,
/// no path, port or scheme. `localhost` and bare addresses are refused here; addresses a
/// name resolves to are refused in [`public_ip`].
fn icon_host(origin: &str) -> Option<String> {
    let host = origin.trim().to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && !host.starts_with(['.', '-'])
        && !host.ends_with(['.', '-'])
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && host.parse::<IpAddr>().is_err()
        && !host.ends_with(".localhost")
        && !host.ends_with(".local")
        && !host.ends_with(".internal");
    ok.then_some(host)
}

/// Only an address on the public internet is fetched from: not this machine, not the
/// private ranges, not link-local, not the carrier range, not a v4-mapped one of those.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || (o[0] == 198 && (18..=19).contains(&o[1])))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
                || seg[0] == 0x2001 && seg[1] == 0x0db8)
        }
    }
}

async fn resolves_to_public(host: &str) -> bool {
    match tokio::net::lookup_host((host, 443)).await {
        Ok(addrs) => {
            let addrs: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            !addrs.is_empty() && addrs.into_iter().all(public_ip)
        }
        Err(_) => false,
    }
}

async fn fetch_icon(host: &str) -> Option<(String, Vec<u8>)> {
    if !resolves_to_public(host).await {
        return None;
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
        .user_agent("opengrok-site-icon/1")
        .build()
        .ok()?;
    if let Some(icon) = fetch_image(&client, &format!("https://{host}/favicon.ico")).await {
        return Some(icon);
    }
    let page = fetch_capped(&client, &format!("https://{host}/"), PAGE_MAX_BYTES).await?;
    let page = String::from_utf8_lossy(&page.1);
    let href = icon_link(&page)?;
    let url = same_site_url(host, &href)?;
    fetch_image(&client, &url).await
}

/// A response body up to `cap` bytes, with its content type; more than the cap is not read.
async fn fetch_capped(
    client: &reqwest::Client,
    url: &str,
    cap: usize,
) -> Option<(String, Vec<u8>)> {
    let mut response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    if response
        .content_length()
        .is_some_and(|len| len as usize > cap)
    {
        return None;
    }
    let mime = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body.len() + chunk.len() > cap {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some((mime, body))
}

async fn fetch_image(client: &reqwest::Client, url: &str) -> Option<(String, Vec<u8>)> {
    let (mime, bytes) = fetch_capped(client, url, ICON_MAX_BYTES).await?;
    let mime = if mime.starts_with("image/") {
        mime
    } else if url.ends_with(".ico") && looks_like_ico(&bytes) {
        "image/x-icon".to_string()
    } else if looks_like_png(&bytes) {
        "image/png".to_string()
    } else {
        return None;
    };
    (!bytes.is_empty()).then_some((mime, bytes))
}

fn looks_like_ico(bytes: &[u8]) -> bool {
    bytes.len() > 6 && bytes[0..4] == [0, 0, 1, 0]
}

fn looks_like_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G'])
}

/// The `href` of the first `<link>` whose `rel` names an icon, read without an HTML parser:
/// tags are scanned, attributes split on whitespace, quotes stripped.
fn icon_link(page: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let mut from = 0;
    while let Some(start) = lower[from..].find("<link") {
        let start = from + start;
        let end = lower[start..].find('>').map(|e| start + e)?;
        let tag = &page[start..end];
        from = end;
        let attrs = attributes(tag);
        let rel = attrs
            .get("rel")
            .map(|r| r.to_ascii_lowercase())
            .unwrap_or_default();
        let is_icon = rel
            .split_whitespace()
            .any(|word| word == "icon" || word == "apple-touch-icon");
        if is_icon && let Some(href) = attrs.get("href") {
            return Some(href.clone());
        }
    }
    None
}

fn attributes(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = tag.trim_start_matches("<link").trim();
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq]
            .trim()
            .rsplit(char::is_whitespace)
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let after = rest[eq + 1..].trim_start();
        let (value, tail) = match after.chars().next() {
            Some(q @ ('"' | '\'')) => {
                let body = &after[1..];
                match body.find(q) {
                    Some(close) => (body[..close].to_string(), &body[close + 1..]),
                    None => (body.to_string(), ""),
                }
            }
            _ => {
                let close = after.find(char::is_whitespace).unwrap_or(after.len());
                (after[..close].to_string(), &after[close..])
            }
        };
        if !name.is_empty() {
            out.insert(name, value);
        }
        rest = tail;
    }
    out
}

/// A link on the same site: `https://host/...`, `//host/...`, or a path. Anything else is
/// another site, and another site's icon is not fetched on this site's account.
fn same_site_url(host: &str, href: &str) -> Option<String> {
    let href = href.trim();
    if let Some(rest) = href.strip_prefix("https://") {
        let (h, path) = rest.split_once('/').unwrap_or((rest, ""));
        return (h.eq_ignore_ascii_case(host)).then(|| format!("https://{host}/{path}"));
    }
    if let Some(rest) = href.strip_prefix("//") {
        let (h, path) = rest.split_once('/').unwrap_or((rest, ""));
        return (h.eq_ignore_ascii_case(host)).then(|| format!("https://{host}/{path}"));
    }
    if href.starts_with("http:") || href.starts_with("data:") || href.contains("://") {
        return None;
    }
    Some(format!("https://{host}/{}", href.trim_start_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_public_host_is_fetched_from() {
        for bad in [
            "localhost",
            "127.0.0.1",
            "[::1]",
            "10.0.0.1",
            "a",
            "x.local",
            "-x.com",
            "x.com/",
            "x.com:8080",
            "https://x.com",
        ] {
            assert!(icon_host(bad).is_none(), "{bad}");
        }
        assert_eq!(icon_host(" Facebook.COM ").as_deref(), Some("facebook.com"));
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(
                bad.parse::<IpAddr>().is_ok_and(|ip| !public_ip(ip)),
                "{bad}"
            );
        }
        for good in ["8.8.8.8", "2606:4700::1111"] {
            assert!(good.parse::<IpAddr>().is_ok_and(public_ip), "{good}");
        }
    }

    #[test]
    fn the_icon_link_is_read_off_the_page_and_kept_on_the_site() {
        let page = r#"<html><head><LINK rel="stylesheet" href="/a.css"><link href='/img/fav.png' rel="shortcut icon" type="image/png"></head>"#;
        assert_eq!(icon_link(page).as_deref(), Some("/img/fav.png"));
        assert_eq!(
            same_site_url("x.com", "/img/fav.png").as_deref(),
            Some("https://x.com/img/fav.png")
        );
        assert_eq!(
            same_site_url("x.com", "https://X.com/i.png").as_deref(),
            Some("https://x.com/i.png")
        );
        assert_eq!(
            same_site_url("x.com", "//x.com/i.png").as_deref(),
            Some("https://x.com/i.png")
        );
        assert!(same_site_url("x.com", "https://cdn.other.com/i.png").is_none());
        assert!(same_site_url("x.com", "http://x.com/i.png").is_none());
        assert!(icon_link("<html><link rel=stylesheet href=a.css>").is_none());
    }
}
