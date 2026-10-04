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
///
/// And no proxy is used: the builder arrives with one already set, as `HTTPS_PROXY` sets one in a
/// proxied deployment, and `harden` must drop it. Through a proxy the resolver is asked for the
/// proxy's address while the proxy resolves the bundle's host, so the address check never ran.
#[tokio::test]
async fn a_hardened_client_goes_direct_and_does_not_follow_redirects() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn serve(reply: &'static str, hit: Arc<AtomicBool>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                hit.store(true, Ordering::SeqCst);
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let _ = socket.write_all(reply.as_bytes()).await;
            }
        });
        addr
    }
    let redirect = "HTTP/1.1 307 Temporary Redirect\r\nlocation: http://127.0.0.1:9/admin\r\ncontent-length: 0\r\n\r\n";
    let proxied = Arc::new(AtomicBool::new(false));
    let proxy = serve(
        "HTTP/1.1 502 Bad Gateway\r\ncontent-length: 0\r\n\r\n",
        proxied.clone(),
    )
    .await;
    let addr = serve(redirect, Arc::new(AtomicBool::new(false))).await;
    let with_proxy = || {
        let proxy = reqwest::Proxy::all(format!("http://{proxy}")).unwrap();
        reqwest::Client::builder().proxy(proxy)
    };
    let pinned = harden(with_proxy())
        .resolve("redirector.test", addr)
        .build()
        .unwrap();
    let url = format!("http://redirector.test:{}/mcp", addr.port());
    assert_eq!(pinned.post(&url).send().await.unwrap().status(), 307);
    let hardened = harden(with_proxy()).build().unwrap();
    let local = format!("http://localhost:{}/mcp", addr.port());
    assert!(hardened.post(local).send().await.is_err());
    assert!(!proxied.load(Ordering::SeqCst), "the proxy was used");
}
