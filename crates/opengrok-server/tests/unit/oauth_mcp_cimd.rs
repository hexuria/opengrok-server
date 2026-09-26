use super::*;
use std::net::IpAddr;

fn ip(text: &str) -> IpAddr {
    text.parse().unwrap()
}

#[test]
fn only_global_unicast_addresses_may_serve_a_document() {
    for bad in [
        "127.0.0.1",
        "127.0.0.2",
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.5",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "198.18.0.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:10.0.0.5",
        "::ffff:127.0.0.1",
        "::10.0.0.5",
        "fc00::1",
        "fd12::1",
        "fe80::1",
        "fec0::1",
        "ff02::1",
        "2001:db8::1",
        "64:ff9b::a00:5",
    ] {
        assert!(!address_permitted(ip(bad), false), "{bad} must be refused");
    }
    for good in [
        "93.184.216.34",
        "8.8.8.8",
        "2606:2800:220:1:248:1893:25c8:1946",
    ] {
        assert!(address_permitted(ip(good), false), "{good} must be allowed");
    }
    // The test seam admits loopback and nothing else.
    assert!(address_permitted(ip("127.0.0.1"), true));
    assert!(address_permitted(ip("::1"), true));
    assert!(!address_permitted(ip("10.0.0.5"), true));
    assert!(!address_permitted(ip("169.254.169.254"), true));
    assert!(!address_permitted(ip("::ffff:10.0.0.5"), true));
}

#[tokio::test]
async fn a_name_is_judged_by_what_it_resolves_to() {
    // `localhost.` — the trailing dot form the string list never matched — resolves to
    // loopback and is refused without the seam, admitted with it.
    assert!(
        cimd_target("https://localhost./client.json", false)
            .await
            .is_err()
    );
    assert!(
        cimd_target("http://localhost./client.json", true)
            .await
            .is_ok()
    );
    assert!(
        cimd_target("https://127.0.0.2/client.json", false)
            .await
            .is_err()
    );
    assert!(
        cimd_target("https://[::ffff:10.0.0.5]/client.json", false)
            .await
            .is_err()
    );
    assert!(
        cimd_target("https://169.254.169.254/latest/meta-data", false)
            .await
            .is_err()
    );
}

#[test]
fn the_url_shape_rule_is_shape_only() {
    assert!(cimd_url_allowed("https://tool.example/client.json", false));
    assert!(!cimd_url_allowed("http://tool.example/client.json", false));
    assert!(!cimd_url_allowed("https://tool.example/", false));
    assert!(!cimd_url_allowed(
        "https://tool.example/a/../client.json",
        false
    ));
    assert!(!cimd_url_allowed(
        "https://user:pw@tool.example/client.json",
        false
    ));
    assert!(!cimd_url_allowed(
        "https://tool.example/client.json#frag",
        false
    ));
    assert!(cimd_url_allowed("http://127.0.0.1:9/client.json", true));
}

#[test]
fn a_display_name_is_cut_first_and_cannot_carry_overrides() {
    let long = format!("{}\u{202e} (evil.example)", "A".repeat(300));
    let name = safe_display_name(&long, 80);
    assert_eq!(name.chars().count(), 80);
    assert!(!name.contains("evil"));
    assert!(!name.contains('\u{202e}'));
    assert_eq!(safe_display_name("  Doc\tTool\n  ", 80), "Doc Tool");
    assert_eq!(safe_display_name("\u{200f}\u{2066}", 80), "");
    assert_eq!(
        origin_of("https://tool.example/c.json").as_deref(),
        Some("tool.example")
    );
    assert_eq!(origin_of("mc_0123"), None);
}
