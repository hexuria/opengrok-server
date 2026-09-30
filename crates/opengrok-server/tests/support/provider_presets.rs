//! Google and GitHub as a provider is built in code, and a PKCE pair. Every caller is a test: a
//! deployment's providers come from the `OG_CONNECTORS` file, never from here, and no sign-in it
//! starts sends a PKCE pair yet. Mounted from `connections/oauth.rs`, so the tests reach them as
//! `ProviderConfig::google`, `ProviderConfig::github` and `Pkce::new`, as before; a server path
//! that builds one takes it back.

use std::collections::BTreeMap;

use super::{Pkce, ProviderConfig};

impl Pkce {
    /// S256, which is the only method worth using; `plain` exists in the spec and defeats the point.
    pub fn new(verifier: impl Into<String>) -> Self {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        let verifier = verifier.into();
        let digest = Sha256::digest(verifier.as_bytes());
        Self {
            challenge: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest),
            verifier,
        }
    }
}

impl ProviderConfig {
    /// Google, with the parameters that actually produce a refresh token.
    pub fn google(connector: &str, client_id: &str, client_secret: &str, scopes: &[&str]) -> Self {
        Self {
            connector: connector.to_string(),
            authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            token_url: "https://oauth2.googleapis.com/token".to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            offline: true,
            extra_authorize_params: BTreeMap::new(),
        }
    }

    pub fn github(client_id: &str, client_secret: &str, scopes: &[&str]) -> Self {
        Self {
            connector: "github".to_string(),
            authorize_url: "https://github.com/login/oauth/authorize".to_string(),
            token_url: "https://github.com/login/oauth/access_token".to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
            scopes: scopes.iter().map(|s| (*s).to_string()).collect(),
            // GitHub OAuth apps issue no refresh token whatever you ask for.
            offline: false,
            extra_authorize_params: BTreeMap::new(),
        }
    }
}
