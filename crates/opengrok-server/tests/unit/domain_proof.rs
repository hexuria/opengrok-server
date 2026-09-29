#![allow(clippy::expect_used, clippy::panic)]

use super::*;

/// Against the real resolver and the public DNS. Ignored by default — the gate must not
/// depend on the network — and run by hand as the evidence that `SystemDns` reads real TXT
/// records and treats NXDOMAIN as "nothing there" rather than an error:
/// `cargo test -p opengrok-server domain_proof -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs network"]
async fn the_system_resolver_reads_txt_and_treats_nxdomain_as_empty() {
    let dns = SystemDns::from_system().expect("system resolver");
    let spf = dns.txt("example.com").await.expect("lookup example.com");
    eprintln!("example.com TXT: {spf:?}");
    assert!(!spf.is_empty(), "example.com publishes TXT records");
    let none = dns
        .txt("_opengrok-verify.example.com")
        .await
        .expect("NXDOMAIN is not an error");
    eprintln!("_opengrok-verify.example.com TXT: {none:?}");
    assert!(none.is_empty());
}
