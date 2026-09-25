use super::*;

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
