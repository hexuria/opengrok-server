//! Coworker templates (`docs/plan-spend-policy.md` §4): a coworker type the org admin writes
//! once — model pin, tool ceiling, what needs a human yes, spend limits — that members hire
//! from. Applied at hire by COPY: the coworker gets the template's grant and its own
//! `spend_limit` row, and remembers the template id. Nothing links them after that.
//!
//! A template names only tools this server implements, and only asks approval for tools inside
//! its own ceiling — the same rule the policy layer enforces at run time, refused here at the
//! door it came in through so an admin sees the mistake, not a coworker that silently refuses.

use opengrok_core::id::{AccountId, CoworkerId};
use opengrok_policy::ToolSet;
use opengrok_store::{CoworkerTemplate, SpendLimit, SpendScope};
use serde::Deserialize;

use crate::agui::AgUiState;

/// What the admin sends. Tools are plain names; limits are money strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub needs_approval: Vec<String>,
    #[serde(default)]
    pub limits: SpendLimit,
}

/// The template's tool ceiling, as `Only(...)`: a template that lists no tools makes a coworker
/// that can only talk, which is a valid type (a writer, a planner) and is stored as such.
pub fn validate(input: &TemplateInput) -> Result<(ToolSet, ToolSet), String> {
    let name = input.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err("name: required, up to 80 characters".to_string());
    }
    let known = opengrok_tools::Executor::builtin_tool_names();
    for tool in &input.tools {
        if !known.contains(&tool.as_str()) {
            return Err(format!(
                "tools: '{tool}' is not a tool this server implements ({})",
                known.join(", ")
            ));
        }
    }
    for tool in &input.needs_approval {
        if !input.tools.contains(tool) {
            return Err(format!(
                "needsApproval: '{tool}' is not in the template's tools; approval is asked only \
                 inside the ceiling"
            ));
        }
    }
    crate::spend::validate_limit(&input.limits)?;
    Ok((
        ToolSet::only(input.tools.clone()),
        if input.needs_approval.is_empty() {
            ToolSet::None
        } else {
            ToolSet::only(input.needs_approval.clone())
        },
    ))
}

/// The template `id` names, only if it belongs to the account's org. `Ok(None)` when the
/// account is in no org or the org has no such template — the caller says "no such template".
pub async fn for_account(
    state: &AgUiState,
    account_id: &AccountId,
    id: &str,
) -> Result<Option<CoworkerTemplate>, String> {
    let org_id = match state.auth.store.load_account(account_id).await {
        Ok((account, _)) => account.org_id.filter(|org| !org.is_empty()),
        Err(error) => return Err(error.to_string()),
    };
    let Some(org_id) = org_id else {
        return Ok(None);
    };
    state
        .auth
        .store
        .template_in_org(&org_id, id)
        .await
        .map_err(|error| error.to_string())
}

/// Copy what the template says onto a freshly hired coworker: the grant (ceiling = profile,
/// needs-approval inside it), the spend limits as the coworker's own row, the profile
/// description, and the memory of which template it was. The grant is the part that must not
/// fail silently — a coworker nobody may use is worse than none.
pub async fn apply_at_hire(
    state: &AgUiState,
    account_id: &AccountId,
    coworker_id: &CoworkerId,
    template: &CoworkerTemplate,
    at_ms: i64,
) -> Result<(), String> {
    let store = &state.auth.store;
    store
        .grant_access(
            account_id,
            coworker_id,
            &template.tool_ceiling,
            &template.tool_ceiling,
            &template.needs_approval,
            at_ms,
        )
        .await
        .map_err(|error| format!("could not grant the template's tools: {error}"))?;
    if !template.limits.is_empty()
        && let Err(error) = store
            .put_spend_limit(
                SpendScope::Coworker,
                coworker_id.as_str(),
                &template.limits,
                at_ms,
            )
            .await
    {
        // Limits are the part a person can re-apply from the admin page; the grant is not.
        tracing::error!(%error, coworker = %coworker_id.as_str(), template = %template.id, "template: limits could not be copied");
    }
    if !template.description.trim().is_empty() {
        let profile = serde_json::json!({
            "description": template.description,
            "title": "",
            "avatarShape": "",
            "avatarColor": "",
        });
        if let Err(error) = store.put_seamb_profile(coworker_id, &profile, at_ms).await {
            tracing::warn!(%error, coworker = %coworker_id.as_str(), "template: description not written");
        }
    }
    if let Err(error) = store
        .record_template_use(coworker_id, &template.id, at_ms)
        .await
    {
        tracing::warn!(%error, coworker = %coworker_id.as_str(), "template: use not recorded");
    }
    Ok(())
}

/// The listing shape both the admin card and the member's hire picker read.
pub fn template_json(template: &CoworkerTemplate) -> serde_json::Value {
    let names = |set: &ToolSet| match set {
        ToolSet::Only(names) => names.iter().cloned().collect::<Vec<_>>(),
        ToolSet::All => opengrok_tools::Executor::builtin_tool_names()
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        ToolSet::None => Vec::new(),
    };
    serde_json::json!({
        "id": template.id,
        "name": template.name,
        "description": template.description,
        "model": template.model,
        "tools": names(&template.tool_ceiling),
        "needsApproval": names(&template.needs_approval),
        "limits": template.limits,
        "updatedAtMs": template.updated_at_ms,
    })
}
