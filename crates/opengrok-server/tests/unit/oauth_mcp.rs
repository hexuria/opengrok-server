use super::*;

#[test]
fn redirects_are_loopback_http_or_https_only() {
    assert!(redirect_allowed("http://localhost:8123/callback"));
    assert!(redirect_allowed("http://127.0.0.1:8123/callback"));
    assert!(redirect_allowed("https://tool.example/cb"));
    assert!(!redirect_allowed("http://tool.example/cb"));
    assert!(!redirect_allowed("http://localhost.evil.example/cb"));
    assert!(!redirect_allowed("ftp://localhost/cb"));
}

#[test]
fn a_client_id_url_is_https_with_a_path_and_never_private() {
    assert!(cimd_url_allowed(
        "https://tool.example/oauth/client.json",
        false
    ));
    assert!(
        !cimd_url_allowed("https://tool.example", false),
        "a path is required"
    );
    assert!(!cimd_url_allowed("https://tool.example/c.json#x", false));
    assert!(!cimd_url_allowed("https://u:p@tool.example/c.json", false));
    assert!(!cimd_url_allowed("https://tool.example/a/../c.json", false));
    assert!(!cimd_url_allowed("http://tool.example/c.json", false));
    assert!(!cimd_url_allowed("https://10.0.0.5/c.json", false));
    assert!(!cimd_url_allowed("https://172.20.1.1/c.json", false));
    assert!(!cimd_url_allowed("http://127.0.0.1:9/c.json", false));
    assert!(
        cimd_url_allowed("http://127.0.0.1:9/c.json", true),
        "tests may allow loopback"
    );
}

#[test]
fn pkce_is_s256_of_the_verifier() {
    // RFC 7636 appendix B.
    assert!(pkce_matches(
        "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    ));
    assert!(!pkce_matches(
        "wrong",
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    ));
}

#[test]
fn query_values_are_percent_encoded() {
    assert_eq!(
        with_query("http://localhost:1/cb", &[("state", "a b&c".to_string())]),
        "http://localhost:1/cb?state=a%20b%26c"
    );
    assert_eq!(
        with_query("http://localhost:1/cb?x=1", &[("code", "ac_1".to_string())]),
        "http://localhost:1/cb?x=1&code=ac_1"
    );
}
