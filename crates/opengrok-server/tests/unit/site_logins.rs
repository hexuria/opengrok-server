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
        "::10.0.0.1",
        "64:ff9b::a9fe:a9fe",
        "2002:c0a8:101::",
        "fec0::1",
        "2001::1",
        "100::1",
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
fn an_icon_is_served_only_as_a_raster_image() {
    let png = [0x89, b'P', b'N', b'G', 1, 2, 3];
    assert_eq!(
        icon_mime("image/png", "https://x.com/i", &png),
        Some("image/png")
    );
    assert_eq!(
        icon_mime(
            "image/svg+xml",
            "https://x.com/i.svg",
            b"<svg><script/></svg>"
        ),
        None
    );
    assert_eq!(
        icon_mime("text/html", "https://x.com/i.png", b"<html>"),
        None
    );
    // A site's wrong word is overruled by the bytes, never the other way round.
    assert_eq!(
        icon_mime("text/plain", "https://x.com/i", &png),
        Some("image/png")
    );
    assert_eq!(
        icon_mime(
            "application/octet-stream",
            "https://x.com/favicon.ico",
            &[0, 0, 1, 0, 1, 0, 0, 0]
        ),
        Some("image/x-icon")
    );
    assert_eq!(icon_mime("image/png", "https://x.com/i", &[]), None);
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
