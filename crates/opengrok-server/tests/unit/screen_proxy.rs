#![allow(clippy::expect_used)]

use super::*;
use opengrok_box::viewer::STORAGE_SHIM;

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

/// A Local VM whose screen is always on one loopback port.
struct OnePort;

const ONE_PORT_PAGE: &str = "http://127.0.0.1:5999/vnc.html?autoconnect=true&password=pw4boxA1";

#[async_trait::async_trait]
impl opengrok_box::Computer for OnePort {
    async fn create(&self, _: Option<u64>) -> opengrok_box::BoxResult<String> {
        Ok("bx".to_string())
    }
    async fn run(
        &self,
        _: &str,
        _: &str,
        _: u32,
    ) -> opengrok_box::BoxResult<opengrok_box::CommandOutput> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn start(
        &self,
        _: &str,
        _: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(
        &self,
        _: &str,
        _: &str,
    ) -> opengrok_box::BoxResult<opengrok_box::StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, _: &str, _: &str) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn write_file(&self, _: &str, _: &str, _: &str) -> opengrok_box::BoxResult<()> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn expose_port(&self, _: &str, _: u16, _: &str) -> opengrok_box::BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn stop(&self, _: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _: &str) -> opengrok_box::BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _: &str) -> opengrok_box::BoxResult<String> {
        Ok("running".to_string())
    }
    async fn screen_url(
        &self,
        _: &str,
        _screen: &opengrok_box::Screen,
    ) -> opengrok_box::BoxResult<Option<String>> {
        Ok(Some(ONE_PORT_PAGE.to_string()))
    }
}

/// A place check that does not finish leaves its ticket due, so the next request makes its own.
/// Stamped up front, a check whose request was dropped mid-way (a webview closing, a client
/// timing out) passed the next five seconds of requests with no check at all, and a box that
/// moved meanwhile was still served (#341 review). The check is held here on the store with a
/// table lock, and the request dropped while it waits.
#[tokio::test]
async fn a_place_check_cut_short_leaves_the_next_request_to_check() {
    use opengrok_core::account::{Account, AccountCommand, AccountView, Plan};
    use opengrok_core::coworker::{Coworker, CoworkerCommand, CoworkerView};
    let Ok(url) = std::env::var("OG_DATABASE_URL") else {
        eprintln!("skipping: OG_DATABASE_URL is not set");
        return;
    };
    let url = opengrok_store::gate_database_or_panic(url);
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to Postgres");
    opengrok_store::migrations::run(&pool)
        .await
        .expect("migrations");
    let store = opengrok_store::PgStore::new(pool.clone());
    let minter = std::sync::Arc::new(TokenMinter::new(b"place-check-cut-short-test-secret"));
    let state = AgUiState {
        auth: crate::auth::AuthState::new(store.clone(), minter.clone(), "host@og.local".into()),
        door: std::sync::Arc::new(opengrok_harness::MockDoor::echoing()),
        model: "oag/cheap".to_string(),
        auto_review_model: "oag/cheap".to_string(),
        computer: Some(std::sync::Arc::new(OnePort)),
        vault: None,
        connectors: crate::connections::routes::Connectors {
            providers: std::sync::Arc::new(std::collections::BTreeMap::new()),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
        },
        plugins: std::sync::Arc::new(std::collections::BTreeMap::new()),
        host_settings: None,
    };

    let (account, at_ms) = (AccountId::new(), chrono::Utc::now().timestamp_millis());
    let email = format!("place-{}@og.local", uuid::Uuid::now_v7().simple());
    let registered = Account::default()
        .decide(AccountCommand::Register {
            email: email.clone(),
            password_hash: "x".to_string(),
            first_name: "Place".to_string(),
            last_name: String::new(),
            org_id: String::new(),
            plan: Plan::Ultra,
            verified: true,
            enabled: true,
            at_ms,
        })
        .expect("register");
    let view = AccountView {
        id: account.clone(),
        email,
        plan: Plan::Ultra,
        trial: false,
        updated_at_ms: at_ms,
        password_hash: Some("x".to_string()),
        first_name: "Place".to_string(),
        last_name: String::new(),
        org_id: None,
        verified: true,
        enabled: true,
        avatar_url: None,
    };
    store
        .append_account(&account, 0, &registered, &view)
        .await
        .expect("append the account");
    let coworker = CoworkerId::new();
    let hired = Coworker::default()
        .decide(CoworkerCommand::Hire {
            name: "Place".to_string(),
            model: "oag/cheap".to_string(),
            at_ms,
        })
        .expect("hire");
    let view = CoworkerView::of(coworker.clone(), &Coworker::replay(&hired), at_ms);
    store
        .append_coworker(&coworker, &account, 0, &hired, &view)
        .await
        .expect("append the coworker");
    let box_id = format!("bx_place_{}", uuid::Uuid::now_v7().simple());
    let scope = (account.as_str(), "local-docker");
    store
        .claim_scoped_computer("account", scope.0, None, &box_id, scope.1, None, at_ms)
        .await
        .expect("the account's box");

    let now = chrono::Utc::now().timestamp();
    let page = proxied_page(
        &minter,
        "http://og.test",
        &account,
        &coworker,
        &box_id,
        ONE_PORT_PAGE,
        now,
    );
    let page = page.expect("a page");
    let ticket = page.split('/').nth(7).expect("the ticket").to_string();
    let cw = coworker.as_str();
    let first = upstream_for(&state, cw, &ticket).await;
    assert_eq!(first.map(|(port, _, _)| port), Some(5999), "learned");
    let long_ago = Duration::from_secs(60 * 60);
    if let Ok(mut known) = UPSTREAMS.lock()
        && let Some(upstream) = known.get_mut(&ticket)
    {
        upstream.3 = upstream.3.checked_sub(long_ago).expect("an hour ago");
    }

    let mut held = pool.begin().await.expect("begin");
    sqlx::query("lock table scoped_computer in access exclusive mode")
        .execute(&mut *held)
        .await
        .expect("hold the place check");
    let cut = tokio::time::timeout(
        Duration::from_millis(500),
        upstream_for(&state, cw, &ticket),
    );
    assert!(cut.await.is_err(), "the check waited on the store");
    let stamp = UPSTREAMS
        .lock()
        .expect("known")
        .get(&ticket)
        .map(|upstream| upstream.3);
    let stamp = stamp.expect("still remembered");
    assert!(
        stamp.elapsed() >= PLACE_FOR,
        "a check that never finished stamped its ticket"
    );
    held.rollback().await.expect("release");

    store
        .claim_scoped_computer(
            "account",
            scope.0,
            Some(&box_id),
            "bx_moved",
            scope.1,
            None,
            at_ms,
        )
        .await
        .expect("the account's box moves");
    let next = upstream_for(&state, cw, &ticket).await;
    assert_eq!(
        next, None,
        "the next request checked, and the box had moved"
    );
}

/// The websocket path a box's page names: a Bot's own screen's token (#376), else the shared
/// screen's; nothing but those two shapes is passed on to the box.
#[test]
fn the_socket_path_is_the_shared_screens_or_one_bots_token() {
    let page = "http://127.0.0.1:5999/vnc.html?autoconnect=true&password=pw";
    assert_eq!(socket_path(page), "websockify");
    let own = format!("{page}&path=websockify%3Ftoken%3Ds3");
    assert_eq!(socket_path(&own), "websockify?token=s3");
    for odd in [
        "&path=../../etc",
        "&path=websockify%3Ftoken%3Ds3%20x",
        "&path=websockify%3Ftoken%3D",
    ] {
        assert_eq!(socket_path(&format!("{page}{odd}")), "websockify", "{odd}");
    }
}
