//! The DNS half of domain-ownership proof — resolving the TXT challenge an org admin published.
//!
//! WHY A SEAM AND NOT A CALL. The aggregate decides what a claim needs (`org::challenge_*`); this
//! module is the only place that asks the network whether it is there. It is a trait so a test can
//! answer the lookup from a map and prove the whole claim → verify → signup path without owning a
//! domain, and so the server's boot can refuse loudly if the system has no usable resolver rather
//! than discovering it in a handler.
//!
//! WHAT A LOOKUP MAY SAY. `Ok(values)` — the TXT strings found, possibly none (NXDOMAIN and an
//! empty answer are the same "not there yet" to the admin). `Err` — the resolver itself failed
//! (timeout, no nameserver), which is a 503 to the caller, never a "not verified": a transient
//! outage must not read as "your record is wrong".
//!
//! A TXT record is a sequence of character-strings; a long value arrives split. They are joined
//! before comparison, which is how every other verifier reads them and what a DNS host shows.

use std::collections::BTreeMap;

use async_trait::async_trait;

#[async_trait]
pub trait TxtLookup: Send + Sync {
    /// The TXT values published at `name` (a bare host name, no trailing dot required).
    async fn txt(&self, name: &str) -> Result<Vec<String>, String>;
}

/// The real resolver: hickory, configured from the system (`/etc/resolv.conf`) at boot.
pub struct SystemDns {
    resolver: hickory_resolver::TokioResolver,
}

impl SystemDns {
    /// Build from the system's resolver configuration. `Err` when the host has none we can read —
    /// the boot log says so and the domain surface answers 503 until it is fixed.
    pub fn from_system() -> Result<Self, String> {
        let resolver = hickory_resolver::Resolver::builder_tokio()
            .map_err(|error| format!("system resolver config: {error}"))?
            .build()
            .map_err(|error| format!("resolver: {error}"))?;
        Ok(Self { resolver })
    }
}

#[async_trait]
impl TxtLookup for SystemDns {
    async fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        // The trailing dot makes it fully qualified, so the resolver does not append a search
        // domain and go looking for `_opengrok-verify.acme.dev.corp.local` first.
        let fqdn = format!("{}.", name.trim_end_matches('.'));
        match self.resolver.txt_lookup(fqdn).await {
            Ok(lookup) => Ok(lookup
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    hickory_resolver::proto::rr::RData::TXT(txt) => Some(
                        txt.txt_data
                            .iter()
                            .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
                            .collect::<String>(),
                    ),
                    _ => None,
                })
                .collect()),
            Err(error) if error.is_no_records_found() => Ok(Vec::new()),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// A lookup answered from a map — the test double, and what a deployment with no resolver uses so
/// the surface still answers (with "no record") instead of panicking. Pub so integration tests
/// can drive it without reaching into private modules.
#[derive(Default)]
pub struct StaticDns {
    records: tokio::sync::RwLock<BTreeMap<String, Vec<String>>>,
}

impl StaticDns {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish (or replace) the TXT values at `name`.
    pub async fn publish(&self, name: &str, values: Vec<String>) {
        self.records
            .write()
            .await
            .insert(name.trim_end_matches('.').to_string(), values);
    }
}

#[async_trait]
impl TxtLookup for StaticDns {
    async fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        Ok(self
            .records
            .read()
            .await
            .get(name.trim_end_matches('.'))
            .cloned()
            .unwrap_or_default())
    }
}

/// The lookup a state has when nobody bound one. Every query fails with a reason, so the surface
/// answers 503 "resolver not configured" — never a false "your record is not there".
pub struct NoResolver;

#[async_trait]
impl TxtLookup for NoResolver {
    async fn txt(&self, _name: &str) -> Result<Vec<String>, String> {
        Err("this server has no DNS resolver bound for domain verification".to_string())
    }
}

/// What a handler asks: is the challenge for `domain` satisfied right now?
pub enum ProofOutcome {
    /// The record was found and carries the token.
    Proven,
    /// The lookup worked and the record is not (yet) there, or does not say the right thing.
    /// Carries a sentence the admin can act on.
    NotFound(String),
    /// The resolver could not answer. Not the admin's fault; try again later.
    Unavailable(String),
}

pub async fn check(dns: &dyn TxtLookup, domain: &str, token: &str) -> ProofOutcome {
    let name = opengrok_core::org::challenge_record_name(domain);
    match dns.txt(&name).await {
        Ok(values) if opengrok_core::org::challenge_satisfied(token, &values) => {
            ProofOutcome::Proven
        }
        Ok(values) if values.is_empty() => ProofOutcome::NotFound(format!(
            "no TXT record found at {name} yet — DNS changes can take a few minutes to appear"
        )),
        Ok(_) => ProofOutcome::NotFound(format!(
            "a TXT record exists at {name} but none of its values is {}",
            opengrok_core::org::challenge_record_value(token)
        )),
        Err(error) => ProofOutcome::Unavailable(format!("DNS lookup failed: {error}")),
    }
}

#[cfg(test)]
#[path = "../tests/unit/domain_proof.rs"]
mod tests;
