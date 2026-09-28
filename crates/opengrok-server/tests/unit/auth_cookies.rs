#![allow(clippy::unwrap_used)]
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
