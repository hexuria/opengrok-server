//! #356's HTTP adapter authenticates with the same bearer/cookie verifier as existing routes.
use crate::agui::AgUiState;
use opengrok_integrations::registry::Registry;

/// The deployment's registry, read from the environment once: the one the Plugins routes serve
/// and a Bot's plugin tools install from (#359), so both see the same catalog and its cache.
pub(crate) fn registry() -> Option<Registry> {
    static REGISTRY: std::sync::OnceLock<Option<Registry>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Registry::from_env).clone()
}

/// The names no account's plugin may install under: every built-in, the deployment's own plugins,
/// and the ceiling's rows that are not tools.
pub(crate) fn reserved_names(state: &AgUiState) -> std::collections::BTreeSet<String> {
    opengrok_tools::Executor::every_builtin()
        .map(str::to_string)
        .chain(state.plugins.keys().cloned())
        .chain([
            opengrok_tools::routine::ROW.to_string(),
            opengrok_tools::plugin_desk::ROW.to_string(),
            opengrok_tools::skill::USE_SKILL.to_string(),
        ])
        .collect()
}

pub fn router(state: AgUiState) -> axum::Router {
    router_with_registry(state, registry())
}
pub fn router_with_registry(state: AgUiState, registry: Option<Registry>) -> axum::Router {
    let reserved_names = reserved_names(&state);
    let store = state.auth.store.clone();
    let vault = state.vault.clone();
    let authenticate = std::sync::Arc::new(move |headers: &axum::http::HeaderMap| {
        crate::agui::routes::account_from_bearer(&state, headers)
    });
    opengrok_plugin_api::router(opengrok_plugin_api::RegistryState {
        store,
        vault,
        authenticate,
        registry,
        reserved_names,
    })
}
