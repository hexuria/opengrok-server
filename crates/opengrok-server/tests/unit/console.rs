use super::serve_console_path;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};

/// A throwaway `dist/` laid out the way Vite emits one: an entry document naming a
/// content-hashed bundle, and that bundle beside it under `assets/`.
struct Dist {
    root: std::path::PathBuf,
}

impl Dist {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "og-console-{}-{}",
            tag,
            uuid::Uuid::now_v7().simple()
        ));
        std::fs::create_dir_all(root.join("assets")).expect("make dist");
        let dist = Self {
            root: root.canonicalize().expect("canonicalize dist"),
        };
        dist.build("AAA");
        dist
    }

    /// Write an entry document naming `hash`, and the bundle it names. Any previous bundle is
    /// removed, exactly as a real rebuild removes the file it replaces.
    fn build(&self, hash: &str) {
        let assets = self.root.join("assets");
        for entry in std::fs::read_dir(&assets).expect("read assets").flatten() {
            std::fs::remove_file(entry.path()).expect("remove old bundle");
        }
        std::fs::write(
            assets.join(format!("index-{hash}.js")),
            b"export const x = 1;",
        )
        .expect("write bundle");
        std::fs::write(
            self.root.join("index.html"),
            format!(r#"<!doctype html><script src="/console/assets/index-{hash}.js"></script>"#),
        )
        .expect("write index");
    }

    fn get(&self, rel: &str) -> axum::response::Response {
        serve_console_path(&self.root, rel)
    }
}

impl Drop for Dist {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn header(res: &axum::response::Response, name: axum::http::HeaderName) -> String {
    res.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// The entry document must never be cached and the hashed bundles must never be revalidated.
///
/// Only `index.html` says which hashes are current, so a browser holding a stale copy asks for
/// a bundle that the next build deleted. The bundles are the opposite case: their name is their
/// version, so they can be pinned forever.
#[test]
fn the_entry_document_is_never_cached_and_the_bundles_always_are() {
    let dist = Dist::new("cache");

    let index = dist.get("");
    assert_eq!(index.status(), 200);
    assert_eq!(header(&index, CONTENT_TYPE), "text/html; charset=utf-8");
    assert!(
        header(&index, CACHE_CONTROL).contains("no-store"),
        "the entry document must carry no-store, got {:?}",
        header(&index, CACHE_CONTROL)
    );

    let bundle = dist.get("assets/index-AAA.js");
    assert_eq!(bundle.status(), 200);
    assert_eq!(
        header(&bundle, CONTENT_TYPE),
        "text/javascript; charset=utf-8"
    );
    assert!(
        header(&bundle, CACHE_CONTROL).contains("immutable"),
        "a content-hashed bundle must be immutable, got {:?}",
        header(&bundle, CACHE_CONTROL)
    );
}

/// A missing asset is a 404, and a client route is still the SPA.
///
/// The fallback used to answer EVERY unresolved path with `index.html` at 200 — including a
/// bundle a previous build had deleted. The browser then parsed an HTML document as a module
/// and reported a syntax error about an unexpected `<`, naming nothing that led back here.
#[test]
fn a_missing_asset_is_not_answered_with_the_page() {
    let dist = Dist::new("missing");

    let gone = dist.get("assets/index-DELETED.js");
    assert_eq!(
        gone.status(),
        404,
        "a missing bundle must 404, not serve HTML as JavaScript"
    );

    // Anything carrying an extension names a file, not a route.
    assert_eq!(dist.get("favicon.ico").status(), 404);

    // A client route has no extension and is the SPA, so deep links and hard refreshes work.
    for route in ["account", "coworkers", "admin/domains"] {
        let res = dist.get(route);
        assert_eq!(res.status(), 200, "{route} should deep-link");
        assert_eq!(header(&res, CONTENT_TYPE), "text/html; charset=utf-8");
    }
}

/// A rebuild lands without restarting the server.
///
/// The entry document used to be read into an `Arc<String>` at boot while the bundles beside it
/// were read per request. A console rebuilt against a running server therefore served new
/// bundles behind an index still naming the deleted old ones — a white page on every reload,
/// with nothing in the server log or the browser console saying why.
#[tokio::test]
async fn a_rebuilt_console_is_served_without_a_restart() {
    let dist = Dist::new("rebuild");

    let before = dist.get("");
    let before = axum::body::to_bytes(before.into_body(), 64 * 1024)
        .await
        .expect("read index");
    let before = String::from_utf8_lossy(&before).to_string();
    assert!(before.contains("index-AAA.js"), "sanity: {before}");

    dist.build("BBB");

    let after = dist.get("");
    let after = axum::body::to_bytes(after.into_body(), 64 * 1024)
        .await
        .expect("read index");
    let after = String::from_utf8_lossy(&after).to_string();
    assert!(
        after.contains("index-BBB.js"),
        "the rebuilt entry document must be served, got {after}"
    );
    assert_eq!(dist.get("assets/index-BBB.js").status(), 200);
    assert_eq!(
        dist.get("assets/index-AAA.js").status(),
        404,
        "the replaced bundle is gone, and says so"
    );
}

/// The entry document asked for by name is the entry document, cache rule included.
///
/// `index.html` carries an extension, so it used to take the file branch and leave with
/// `max-age=300` — a five-minute window in which the one URL a bookmark would use served a
/// page naming hashes the next build deletes. The exact hole `no-store` exists to close.
#[test]
fn the_entry_document_by_name_is_still_never_cached() {
    let dist = Dist::new("byname");
    let res = dist.get("index.html");
    assert_eq!(res.status(), 200);
    assert_eq!(header(&res, CONTENT_TYPE), "text/html; charset=utf-8");
    assert_eq!(
        header(&res, CACHE_CONTROL),
        "no-store",
        "index.html by name must carry the same no-store as the bare route"
    );
}

/// A `dist` that disappears after boot says so, rather than serving a blank page — and an
/// asset that is still on disk is still served, because its request never reads the index.
#[test]
fn a_console_that_vanished_is_not_a_blank_page() {
    let dist = Dist::new("vanished");
    std::fs::remove_file(dist.root.join("index.html")).expect("remove index");
    assert_eq!(dist.get("").status(), 503);
    assert_eq!(dist.get("index.html").status(), 503);
    assert_eq!(dist.get("account").status(), 503);
    assert_eq!(
        dist.get("assets/index-AAA.js").status(),
        200,
        "an asset request does not depend on the entry document"
    );
}

/// A path cannot climb out of the console directory.
#[test]
fn a_request_cannot_escape_the_console_directory() {
    let dist = Dist::new("escape");
    for climb in ["../../etc/passwd", "assets/../../../etc/passwd"] {
        assert_eq!(dist.get(climb).status(), 404, "{climb} must not be served");
    }
}
