#![allow(clippy::expect_used)]

use super::*;

/// A request's headers as it arrives with `Host: host`, carrying `more` besides.
fn arriving(host: &str, more: &[(&str, &str)]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, host.parse().expect("host"));
    for (name, value) in more {
        let name: header::HeaderName = name.parse().expect("name");
        headers.insert(name, value.parse().expect("value"));
    }
    headers
}

/// 3 Oct 2026: `OG_PUBLIC_GATEWAY_URL` named an https front nobody was running, the server spoke
/// plain http on 127.0.0.1:1447 only, and the app there got a `vncUrl` it could not open.
#[test]
fn an_app_on_loopback_is_sent_back_where_it_came_in() {
    let configured = "https://uriahs-MacBook-Pro.local:1447";
    let origin = public_origin(configured, &arriving("127.0.0.1:1447", &[])).expect("an origin");
    let minter = TokenMinter::new(b"screen-origin-test-secret");
    let account = AccountId::from_stored("acct_1".to_string());
    let coworker = CoworkerId::from_stored("cw_1".to_string());
    let page = "http://127.0.0.1:5/vnc.html?autoconnect=true";
    let url = proxied_page(&minter, &origin, &account, &coworker, "bx", page, 0).expect("signed");
    assert!(
        url.starts_with("http://127.0.0.1:1447/coworkers/cw_1/computer/vnc/"),
        "{url}"
    );
    for (host, origin) in [
        ("[::1]:1447", "http://[::1]:1447"),
        ("localhost:1447", "http://localhost:1447"),
        ("[::1]", "http://[::1]"),
        ("localhost", "http://localhost"),
    ] {
        assert_eq!(
            public_origin(configured, &arriving(host, &[])).as_deref(),
            Some(origin),
            "{host}"
        );
    }
}

/// Behind a proxy the configured origin is the truth, and a `Host` that only begins like loopback
/// would carry the ticket wherever it points.
#[test]
fn no_other_host_overrides_the_configured_origin() {
    let configured = "https://og.example.com";
    for host in [
        "192.168.1.5:1447",
        "opengrok:1447",
        "localhost.evil.example",
        "127.0.0.1.nip.io:1447",
        "127.0.0.1:1447@evil.example",
        "127.0.0.1:evil.example",
        "127.0.0.1:99999",
        "127.0.0.1:",
        "[::1]x",
    ] {
        assert_eq!(
            public_origin(configured, &arriving(host, &[])).as_deref(),
            Some(configured),
            "{host}"
        );
    }
    assert_eq!(
        public_origin(configured, &HeaderMap::new()).as_deref(),
        Some(configured),
        "no Host at all"
    );
}

/// A front proxy on this machine hands requests on to loopback: nginx's default `Host` is its
/// upstream's, which the app (perhaps elsewhere) cannot open. What the proxy stamps is never read,
/// only noticed: a forged `X-Forwarded-Host` names nothing either.
#[test]
fn a_loopback_host_a_proxy_handed_on_keeps_the_configured_origin() {
    let configured = "https://og.example.com";
    for stamp in [
        ("x-forwarded-for", "203.0.113.7"),
        ("x-forwarded-proto", "https"),
        ("x-forwarded-host", "evil.example"),
        ("forwarded", "for=203.0.113.7;proto=https"),
        ("x-real-ip", "203.0.113.7"),
    ] {
        assert_eq!(
            public_origin(configured, &arriving("127.0.0.1:1447", &[stamp])).as_deref(),
            Some(configured),
            "{stamp:?}"
        );
    }
}

/// A listen address names nothing a webview can open, so it still yields to the `Host`, as it did
/// before a loopback `Host` was looked at first — scheme from `X-Forwarded-Proto` included.
#[test]
fn a_listen_address_still_falls_back_to_the_host() {
    for configured in ["http://0.0.0.0:1447", "http://[::]:1447"] {
        for (host, more, origin) in [
            ("192.168.1.5:1447", None, "http://192.168.1.5:1447"),
            (
                "192.168.1.5:1447",
                Some("https"),
                "https://192.168.1.5:1447",
            ),
            ("127.0.0.1:1447", None, "http://127.0.0.1:1447"),
            ("127.0.0.1:1447", Some("https"), "https://127.0.0.1:1447"),
            ("localhost:1447", None, "http://localhost:1447"),
        ] {
            let more: Vec<_> = more
                .map(|proto| ("x-forwarded-proto", proto))
                .into_iter()
                .collect();
            assert_eq!(
                public_origin(configured, &arriving(host, &more)).as_deref(),
                Some(origin),
                "{configured} {host} {more:?}"
            );
        }
    }
}

#[test]
fn only_a_loopback_page_is_ever_dialled() {
    assert_eq!(
        loopback_port("http://127.0.0.1:49160/vnc.html?password=x"),
        Some(49160)
    );
    assert_eq!(loopback_port("https://desk.ascii.dev/vnc.html"), None);
    assert_eq!(loopback_port("http://127.0.0.1.evil:80/vnc.html"), None);
    assert_eq!(loopback_port("http://10.0.0.5:6080/vnc.html"), None);
}

#[test]
fn the_public_origin_is_never_a_listen_address() {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, "192.168.1.5:1447".parse().expect("host"));
    assert_eq!(
        public_origin("https://mac.local:1447/", &headers).as_deref(),
        Some("https://mac.local:1447")
    );
    assert_eq!(
        public_origin("http://0.0.0.0:1447", &headers).as_deref(),
        Some("http://192.168.1.5:1447")
    );
    assert_eq!(
        public_origin("http://[::]:1447", &headers).as_deref(),
        Some("http://192.168.1.5:1447"),
        "an IPv6 listen address is no more openable than 0.0.0.0"
    );
    headers.insert("x-forwarded-proto", "https".parse().expect("proto"));
    assert_eq!(
        public_origin("", &headers).as_deref(),
        Some("https://192.168.1.5:1447")
    );
    headers.insert(header::HOST, "evil/x?y".parse().expect("host"));
    assert_eq!(public_origin("", &headers), None);
}

#[test]
fn a_ticket_is_the_same_all_window_and_is_not_an_access_token() {
    let minter = TokenMinter::new(b"screen-ticket-test-secret");
    let account = AccountId::from_stored("acct_1".to_string());
    let coworker = CoworkerId::from_stored("cw_1".to_string());
    let page = "http://127.0.0.1:5/vnc.html?autoconnect=true&password=pw";
    let at = |now| {
        proxied_page(
            &minter,
            "http://og.lan",
            &account,
            &coworker,
            "bx",
            page,
            now,
        )
    };
    let first = at(WINDOW_SECONDS * 100 + 1);
    assert_eq!(
        first,
        at(WINDOW_SECONDS * 101 - 1),
        "a poll must not reload"
    );
    assert_ne!(first, at(WINDOW_SECONDS * 101 + 1));
    let url = first.unwrap_or_default();
    assert!(
        url.starts_with("http://og.lan/coworkers/cw_1/computer/vnc/"),
        "{url}"
    );
    assert!(url.contains("?autoconnect=true&password=pw&path=coworkers/cw_1/computer/vnc/"));
    assert!(url.ends_with("/websockify"), "{url}");
    let ticket = url.split('/').nth(6).unwrap_or_default().to_string();
    assert!(minter.verify_access(&ticket).is_err());
}

#[test]
fn the_storage_shim_goes_first_in_a_page_and_nowhere_else() {
    let shim = String::from_utf8_lossy(STORAGE_SHIM).into_owned();
    let placed =
        |page: &str| String::from_utf8_lossy(&with_storage_shim(page.as_bytes())).into_owned();
    assert_eq!(
        placed("<!DOCTYPE html>\n<html><head><title>noVNC</title>"),
        format!("<!DOCTYPE html>\n<html><head>{shim}<title>noVNC</title>")
    );
    assert_eq!(
        placed("<HTML><HEAD lang=\"en\">\n<script type=module src=app/ui.js></script>"),
        format!("<HTML><HEAD lang=\"en\">{shim}\n<script type=module src=app/ui.js></script>")
    );
    assert_eq!(
        placed("<header>x</header>"),
        format!("{shim}<header>x</header>"),
        "a <header> is not a <head>"
    );
    assert_eq!(placed(""), shim);
}
