//! The browser console's session cookies — the transport that lets a page authenticate without
//! putting a token where a script can read it.
//!
//! WHY COOKIES AND NOT localStorage. The web console is a pure client SPA served same-origin by
//! this server. A refresh token kept in `localStorage` is readable by any script the page runs, so
//! one XSS is one stolen session that does not expire on its own. httpOnly cookies keep both tokens
//! out of JavaScript entirely: the browser holds them and sends them on same-origin requests, and
//! the SPA never touches a token. The desktop client is unaffected — it still presents a `Bearer`
//! header, which `account_api::caller` accepts alongside the cookie.
//!
//! SameSite=Lax + JSON-only mutations are the CSRF guard: a cross-site page cannot forge a
//! state-changing `POST application/json` without a preflight this server does not satisfy, and Lax
//! cookies are not sent on cross-site POSTs anyway. `Secure` is opt-in (`OG_COOKIE_SECURE=1`)
//! because this server is reached over plain HTTP on a LAN today; a hardcoded `Secure` would
//! silently drop every cookie there and look like a broken login.

use axum::http::HeaderMap;
use axum::http::header::COOKIE;

/// The access-token cookie. Short-lived, mirrors the JWT's own TTL.
pub const ACCESS_COOKIE: &str = "og_access";
/// The refresh-token cookie. Long-lived; the token is opaque and only ever handed back to us.
pub const REFRESH_COOKIE: &str = "og_refresh";

/// How long the refresh cookie is offered for. The token does not expire server-side (it is a hash
/// lookup), so this only bounds how long a browser keeps offering it before a fresh sign-in.
pub const REFRESH_COOKIE_MAX_AGE_SECONDS: i64 = 30 * 24 * 60 * 60;

/// Read one cookie's value from the request's `Cookie` header. Returns the first match; a malformed
/// header yields `None` rather than an error, because a browser sending garbage is not a 500.
pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some((key, value)) = pair.split_once('=')
            && key.trim() == name
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

/// Whether to stamp `Secure` on the cookies. Off by default — see the module note.
fn secure() -> bool {
    std::env::var("OG_COOKIE_SECURE").as_deref() == Ok("1")
}

/// Build a `Set-Cookie` value that stores `value` under `name` for `max_age` seconds.
///
/// The values we store are our own tokens (`A-Za-z0-9._-` for the JWT, `ogr_<hex>` for the
/// refresh), all cookie-safe, so no percent-encoding is needed.
pub fn set_cookie(name: &str, value: &str, max_age: i64) -> String {
    let mut cookie = format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    if secure() {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Build a `Set-Cookie` value that clears `name` immediately.
pub fn clear_cookie(name: &str) -> String {
    let mut cookie = format!("{name}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    if secure() {
        cookie.push_str("; Secure");
    }
    cookie
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(cookie: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_str(cookie).unwrap());
        headers
    }

    #[test]
    fn reads_one_cookie_among_several() {
        let headers = headers_with("a=1; og_access=the.jwt.value; b=2");
        assert_eq!(
            read_cookie(&headers, "og_access").as_deref(),
            Some("the.jwt.value")
        );
        assert_eq!(read_cookie(&headers, "b").as_deref(), Some("2"));
        assert_eq!(read_cookie(&headers, "missing"), None);
    }

    #[test]
    fn a_missing_or_garbled_header_is_none_not_an_error() {
        assert_eq!(read_cookie(&HeaderMap::new(), "og_access"), None);
        // No `=` at all: skipped, not panicked on.
        assert_eq!(read_cookie(&headers_with("justaflag"), "og_access"), None);
    }

    #[test]
    fn set_cookie_is_httponly_and_lax_and_carries_the_value() {
        let cookie = set_cookie(ACCESS_COOKIE, "abc.def.ghi", 3600);
        assert!(cookie.starts_with("og_access=abc.def.ghi;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Max-Age=3600"));
        assert!(cookie.contains("Path=/"));
    }

    #[test]
    fn clear_cookie_expires_immediately() {
        let cookie = clear_cookie(REFRESH_COOKIE);
        assert!(cookie.starts_with("og_refresh=;"));
        assert!(cookie.contains("Max-Age=0"));
        assert!(cookie.contains("HttpOnly"));
    }
}
