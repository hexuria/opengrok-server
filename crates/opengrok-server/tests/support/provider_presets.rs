//! Google and GitHub as a provider is built in code. Every caller is a test: a deployment's
//! providers come from the `OG_CONNECTORS` file, never from here. Mounted from
//! `connections/oauth.rs`, so the tests reach them as `ProviderConfig::google` and
//! `ProviderConfig::github`, as before; a server path that builds a provider takes them back.

use std::collections::BTreeMap;

use super::ProviderConfig;

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
