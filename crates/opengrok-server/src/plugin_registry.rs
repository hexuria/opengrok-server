//! #356's HTTP adapter authenticates with the same bearer/cookie verifier as existing routes.
use crate::agui::AgUiState;
use opengrok_integrations::registry::Registry;
pub fn router(state: AgUiState) -> axum::Router {
    router_with_registry(state, Registry::from_env())
}
pub fn router_with_registry(state: AgUiState, registry: Option<Registry>) -> axum::Router {
    let reserved_names = opengrok_tools::Executor::every_builtin()
        .map(str::to_string)
        .chain(state.plugins.keys().cloned())
        .chain([
            opengrok_tools::routine::ROW.to_string(),
            opengrok_tools::skill::USE_SKILL.to_string(),
        ])
        .collect();
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
