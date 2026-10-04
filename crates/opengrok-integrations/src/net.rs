//! How an account-installed bundle's MCP server is reached. The bundle names its own URL, so the
//! server must not become that account's way into the deployment's own network: no redirects
//! (one answered `307 http://127.0.0.1:<admin port>/` and the POST, headers and all, followed it),
//! and only public addresses, checked on every resolution rather than once at install, because a
//! name can answer anything and answer differently tomorrow.
//!
//! An operator's own plugins are configuration and keep the system resolver: those may sit on a
//! private network on purpose.
use opengrok_plugins::bundle::{is_public_ip, public_https};
use std::net::SocketAddr;

/// For `opengrok_tools::mcp::Endpoint::harden`.
pub fn harden(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    builder
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(PublicOnly)
}

/// The dial-time twin of the install-time check, read through the same URL parser the client
/// uses: it writes `0x7f.1` as `127.0.0.1`, and an address literal never reaches a resolver.
pub fn public_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or_default();
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    parsed.scheme() == "https" && public_https(url) && literal.parse().map_or(true, is_public_ip)
}

struct PublicOnly;

impl reqwest::dns::Resolve for PublicOnly {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = found.filter(|a| is_public_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/net.rs"]
mod tests;
