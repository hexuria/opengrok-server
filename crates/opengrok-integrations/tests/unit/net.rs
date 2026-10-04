#![allow(clippy::unwrap_used)]
use super::*;
use reqwest::dns::Resolve;

#[test]
fn private_and_disguised_addresses_are_never_public() {
    for url in [
        "https://127.0.0.1/mcp",
        "https://127.1/mcp",
        "https://0x7f.1/mcp",
        "https://2130706433/mcp",
        "https://10.0.0.5:8443/mcp",
        "https://169.254.169.254/latest",
        "https://[::1]/mcp",
        "https://[::ffff:127.0.0.1]/mcp",
        "https://[fd00::1]/mcp",
        "https://localhost/mcp",
        "https://api.localhost/mcp",
        "https://user@example.com/mcp",
        "http://example.com/mcp",
        "not a url",
    ] {
        assert!(!public_url(url), "{url}");
    }
    for url in [
        "https://mcp.example.com/mcp",
        "https://8.8.8.8/mcp",
        "https://[2606:4700::1111]/mcp",
    ] {
        assert!(public_url(url), "{url}");
    }
}

/// The resolver is what a name meets on every dial: a name that answers loopback is refused.
#[tokio::test]
async fn a_name_resolving_to_loopback_is_refused() {
    let name = "localhost".parse().unwrap();
    assert!(PublicOnly.resolve(name).await.is_err());
}

/// A redirect is never followed: the plugin's headers would go wherever it pointed. The name is
/// pinned to the loopback test server, which `PublicOnly` would otherwise rightly refuse, so what
/// this exercises is `harden`'s redirect policy.
#[tokio::test]
async fn a_hardened_client_does_not_follow_redirects() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let reply = "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:9/admin\r\ncontent-length: 0\r\n\r\n";
            let _ = socket.write_all(reply.as_bytes()).await;
        }
    });
    let pinned = harden(reqwest::Client::builder().no_proxy())
        .resolve("redirector.test", addr)
        .build()
        .unwrap();
    let url = format!("http://redirector.test:{}/mcp", addr.port());
    assert_eq!(pinned.post(&url).send().await.unwrap().status(), 307);
    let hardened = harden(reqwest::Client::builder().no_proxy())
        .build()
        .unwrap();
    let local = format!("http://localhost:{}/mcp", addr.port());
    assert!(hardened.post(local).send().await.is_err());
}
