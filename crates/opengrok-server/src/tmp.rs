//! Pre-LLM token resolve. Candidates come from plugin-declared resolvers; bind lives in tmp2-core.
//!
//! Completions and an unbound mention must not open `ModelDoor`. MCP tools are not a lookup path.

use opengrok_core::account::AccountView;
use opengrok_core::id::AccountId;
use opengrok_harness::ChatMessage;
use opengrok_plugins::{TmpToken, tmp_tokens_from_extensions};
use opengrok_store::PgStore;
use serde_json::{Value, json};
use tmp2_core::{
    Binding, Candidate, CandidateSet, Catalog, EntitySchema, Grounding, Mention, MentionMode,
    Outcome, bind, complete, extract, extract_with_implicit,
};

const ORG_USERS_PLUGIN_JSON: &str = include_str!("../../../plugins/org-users/plugin.json");
const ORG_USERS_ROLES_JSON: &str = include_str!("../../../plugins/org-users/roles.json");

/// First-party TMP tokens. Empty if the org-users plugin is missing or has no extension.
#[must_use]
pub fn active_tmp_tokens() -> Vec<TmpToken> {
    let Ok(manifest) = serde_json::from_str::<Value>(ORG_USERS_PLUGIN_JSON) else {
        return Vec::new();
    };
    tmp_tokens_from_extensions(manifest.get("extensions"))
}

/// Tokens declared by enabled TMP plugins. Empty catalog means TMP is off: `@user` is not grounded.
#[must_use]
pub fn catalog() -> Catalog {
    catalog_from_tokens(&active_tmp_tokens())
}

/// Build a tmp2 catalog from plugin token declarations.
#[must_use]
pub fn catalog_from_tokens(tokens: &[TmpToken]) -> Catalog {
    let mut catalog = Catalog::new();
    for token in tokens {
        let schema = EntitySchema {
            token: token.name.clone(),
            display_name: token.display_name.clone(),
            implicit: token.implicit,
            bare_at: token.bare_at,
        };
        let _ = catalog.add(schema);
    }
    catalog
}

fn tmp_plugin_identity() -> (String, String) {
    let Ok(manifest) = serde_json::from_str::<Value>(ORG_USERS_PLUGIN_JSON) else {
        return ("org-users".to_string(), "person".to_string());
    };
    let plugin = manifest
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("org-users")
        .to_string();
    let mention = manifest
        .pointer("/extensions/app.opengrok.tmp/mention")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("person")
        .to_string();
    (plugin, mention)
}

/// JSON catalog the composer uses to decide whether `@` is a token.
#[must_use]
pub fn catalog_json(tokens: &[TmpToken]) -> Value {
    let (plugin, mention) = tmp_plugin_identity();
    json!(
        tokens
            .iter()
            .map(|token| json!({
                "name": token.name,
                "displayName": token.display_name,
                "implicit": token.implicit,
                "bareAt": token.bare_at,
                "resolver": token.resolver,
                "ui": token.ui,
                "required": token.required,
                "source": token.source,
                "validate": token.validate,
                "trigger": "#",
                "plugin": plugin,
                "mention": mention,
            }))
            .collect::<Vec<_>>()
    )
}

/// Org-scoped user rows as tmp2 candidates.
pub fn user_candidates(accounts: &[AccountView]) -> Vec<Candidate> {
    accounts
        .iter()
        .map(|account| {
            let label = display_name(account);
            let mut row = Candidate::new("user", account.id.as_str(), label)
                .with_qualifier("email", account.email.clone())
                .with_source("accounts_by_org");
            if let Some(org) = &account.org_id {
                row = row.with_qualifier("org", org.clone());
            }
            row
        })
        .collect()
}

fn display_name(account: &AccountView) -> String {
    let name = format!("{} {}", account.first_name.trim(), account.last_name.trim());
    let name = name.trim();
    if name.is_empty() {
        account.email.clone()
    } else {
        name.to_string()
    }
}

/// Load the candidate snapshot for plugin-declared resolvers only.
pub async fn candidates_for(store: &PgStore, account: &AccountView) -> CandidateSet {
    candidates_for_tokens(store, account, &active_tmp_tokens()).await
}

/// Snapshot each declared resolver. Unknown resolver ids contribute no rows (fail closed).
pub async fn candidates_for_tokens(
    store: &PgStore,
    account: &AccountView,
    tokens: &[TmpToken],
) -> CandidateSet {
    let mut set = CandidateSet::new();
    let want_users = tokens
        .iter()
        .any(|token| token.resolver == "org-accounts" && token.name == "user");
    if want_users
        && let Some(org) = &account.org_id
        && let Ok(accounts) = store.accounts_by_org(org).await
    {
        for row in user_candidates(&accounts) {
            set.push(row);
        }
    }
    for token in tokens {
        if token.resolver != "plugin-file" {
            continue;
        }
        let raw = match token.source.as_str() {
            "roles.json" => ORG_USERS_ROLES_JSON,
            _ => continue,
        };
        let Ok(labels) = serde_json::from_str::<Vec<String>>(raw) else {
            continue;
        };
        for label in labels {
            set.push(Candidate::new(&token.name, label.clone(), label).with_source("plugin-file"));
        }
    }
    set
}

/// Extract and bind.
///
/// `implicit` means this chat enabled a people connector: bare `@Uriah` and catalog scan run.
/// Without it, only explicit `@user:…` tokens bind — `@` on an agent name is not a person.
#[must_use]
pub fn preflight(text: &str, implicit: bool, candidates: &CandidateSet) -> Outcome {
    let catalog = if implicit {
        catalog()
    } else {
        explicit_catalog()
    };
    enforce_required(
        preflight_with_catalog(text, implicit, &catalog, candidates),
        implicit,
    )
}

/// When TMP is activated, missing required tokens fail closed (no model call).
fn enforce_required(outcome: Outcome, tmp_mode: bool) -> Outcome {
    if !tmp_mode {
        return outcome;
    }
    let Outcome::Ready(grounding) = &outcome else {
        return outcome;
    };
    let bound: std::collections::HashSet<&str> = grounding
        .bindings
        .iter()
        .map(|binding| binding.token.as_str())
        .collect();
    for token in active_tmp_tokens() {
        if !token.required || bound.contains(token.name.as_str()) {
            continue;
        }
        return Outcome::Unresolved {
            mention: Mention {
                span: 0..0,
                raw: format!("#{}", token.name),
                token: token.name.clone(),
                query: None,
                mode: MentionMode::Explicit,
            },
            reason: format!("{} is required", token.display_name),
            bindings: grounding.bindings.clone(),
        };
    }
    outcome
}

fn explicit_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    for token in active_tmp_tokens() {
        let schema = EntitySchema {
            token: token.name.clone(),
            display_name: token.display_name.clone(),
            implicit: false,
            bare_at: false,
        };
        let _ = catalog.add(schema);
    }
    catalog
}

/// Same as [`preflight`] with an explicit catalog (tests, disabled plugins).
#[must_use]
pub fn preflight_with_catalog(
    text: &str,
    implicit: bool,
    catalog: &Catalog,
    candidates: &CandidateSet,
) -> Outcome {
    if catalog.schemas().is_empty() {
        return bind(text, &[], catalog, candidates);
    }
    let mentions = if implicit {
        extract_with_implicit(text, catalog, candidates)
    } else {
        extract(text, catalog)
    };
    bind(text, &mentions, catalog, candidates)
}

/// Prefix filter for the composer. Never a model call.
#[must_use]
pub fn complete_token(token: &str, query: &str, candidates: &CandidateSet) -> Vec<Candidate> {
    complete(token, query, candidates)
}

/// Replace the last user chat message with grounded text + facts JSON.
pub fn rewrite_last_user(messages: &mut [ChatMessage], grounding: &Grounding) {
    if grounding.bindings.is_empty() {
        return;
    }
    let Some(last) = messages
        .iter_mut()
        .rev()
        .find(|message| message.role == "user")
    else {
        return;
    };
    let facts = grounding.facts_json().unwrap_or_else(|_| "[]".to_string());
    last.content = format!("{}\n\n{}", grounding.rewritten, facts);
}

/// Host-executed lookup. Not an LLM tool and not advertised on `ModelRequest.tools`.
pub fn resolve_tool_name(token: &str) -> String {
    if token == "user" {
        "find_user".to_string()
    } else {
        format!("tmp_{token}")
    }
}

/// `email Uriah the invoice` is actuation, not a chat turn.
#[must_use]
pub fn leftover_wants_mail(prompt: &str) -> bool {
    prompt
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .any(|word| word == "email" || word == "mail")
}

fn binding_email(binding: &Binding) -> Option<&str> {
    binding
        .qualifiers
        .get("email")
        .map(String::as_str)
        .filter(|email| !email.is_empty())
}

/// One completed `find_user` (or `tmp_*`) card per binding.
#[must_use]
pub fn resolve_tool_call(binding: &Binding, id: String, timestamp_ms: i64) -> Value {
    let email = binding_email(binding).unwrap_or("");
    json!({
        "kind": "tool-call",
        "id": id,
        "name": resolve_tool_name(&binding.token),
        "status": "done",
        "summary": format!("{} → {} {email}", binding.label, binding.value),
        "args": {
            "token": binding.token,
            "query": binding.mention.query_str(),
        },
        "result": {
            "id": binding.value,
            "label": binding.label,
            "email": email,
            "qualifiers": binding.qualifiers,
        },
        "timestampMs": timestamp_ms,
    })
}

/// There is no mail plugin on this coworker. Fail closed instead of asking the box-model to pretend.
#[must_use]
pub fn send_email_unavailable(grounding: &Grounding, id: String, timestamp_ms: i64) -> Value {
    let to = grounding
        .bindings
        .iter()
        .find(|binding| binding.token == "user")
        .and_then(binding_email)
        .unwrap_or("");
    json!({
        "kind": "tool-call",
        "id": id,
        "name": "send_email",
        "status": "failed",
        "summary": format!("no mail tool on this coworker (to {to})"),
        "args": { "to": to },
        "timestampMs": timestamp_ms,
    })
}

/// Transcript entry the desktop can treat as a pick-list. Unknown kinds round-trip.
pub fn pick_entry(outcome: &Outcome) -> Value {
    match outcome {
        Outcome::NeedsPick {
            mention,
            candidates,
            ..
        } => json!({
            "kind": "tmp-pick",
            "token": mention.token,
            "query": mention.query_str(),
            "mode": mention.mode,
            "candidates": candidates,
        }),
        Outcome::Unresolved {
            mention, reason, ..
        } => json!({
            "kind": "tmp-pick",
            "token": mention.token,
            "query": mention.query_str(),
            "mode": mention.mode,
            "reason": reason,
            "candidates": [],
        }),
        Outcome::Ready(_) => json!({ "kind": "tmp-pick", "candidates": [] }),
    }
}

/// Composer autocomplete payload. `catalog` is empty when no TMP plugin is enabled.
pub async fn complete_for_account(
    store: &PgStore,
    account: &AccountView,
    token: &str,
    query: &str,
) -> Value {
    let tokens = active_tmp_tokens();
    let candidates = candidates_for_tokens(store, account, &tokens).await;
    json!({
        "token": token,
        "candidates": complete_token(token, query, &candidates),
        "catalog": catalog_json(&tokens),
    })
}

/// Whether this account's org contains the named candidate id.
pub fn org_contains(candidates: &CandidateSet, value: &str) -> bool {
    candidates.iter().any(|row| row.value == value)
}

/// Helper so tests can name the caller without knowing store internals.
pub async fn account(store: &PgStore, id: &AccountId) -> Option<AccountView> {
    store.account_by_id(id).await.ok().flatten()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn first_party_org_users_plugin_declares_the_user_token() {
        let tokens = active_tmp_tokens();
        assert!(
            tokens
                .iter()
                .any(|token| token.name == "user" && token.resolver == "org-accounts"),
            "{tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.name == "years" && token.ui == "number"),
            "{tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.name == "role" && token.resolver == "plugin-file"),
            "{tokens:?}"
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.name == "pin" && token.ui == "text"),
            "{tokens:?}"
        );
        assert_eq!(
            catalog().get("user").map(|s| s.token.as_str()),
            Some("user")
        );
    }

    #[test]
    fn without_plugin_tokens_at_user_is_plain_text() {
        let catalog = Catalog::new();
        let outcome =
            preflight_with_catalog("@user:Uriah hi", true, &catalog, &CandidateSet::new());
        match outcome {
            Outcome::Ready(grounding) => {
                assert_eq!(grounding.original, "@user:Uriah hi");
                assert!(grounding.bindings.is_empty());
            }
            other => panic!("expected Ready with no bindings, got {other:?}"),
        }
    }

    #[test]
    fn tmp_mode_without_required_user_does_not_ready() {
        let outcome = preflight("say hi", true, &CandidateSet::new());
        match outcome {
            Outcome::Unresolved {
                mention, reason, ..
            } => {
                assert_eq!(mention.token, "user");
                assert!(reason.to_lowercase().contains("required"), "{reason}");
            }
            other => panic!("expected Unresolved required user, got {other:?}"),
        }
    }

    #[test]
    fn without_tmp_mode_prose_is_ready() {
        match preflight("email Uriah the invoice", false, &CandidateSet::new()) {
            Outcome::Ready(grounding) => assert!(grounding.bindings.is_empty()),
            other => panic!("expected Ready with no people bind, got {other:?}"),
        }
    }

    #[test]
    fn leftover_email_is_mail_intent() {
        assert!(leftover_wants_mail("email Uriah the invoice"));
        assert!(!leftover_wants_mail("say hi to Uriah"));
    }
}
