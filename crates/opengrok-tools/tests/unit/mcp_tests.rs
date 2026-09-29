use super::*;

fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn a_placeholder_is_filled_from_the_resolved_value() {
    let (filled, unresolved) = fill_placeholders(
        &declared(&[("authorization", "Bearer ${GITHUB_TOKEN}")]),
        &declared(&[("GITHUB_TOKEN", "gho_realtoken")]),
    );
    assert_eq!(filled.get("authorization").unwrap(), "Bearer gho_realtoken");
    assert!(unresolved.is_empty());
}

#[test]
fn several_placeholders_in_one_value_all_resolve() {
    let (filled, _) = fill_placeholders(
        &declared(&[("x", "${A}-${B}")]),
        &declared(&[("A", "one"), ("B", "two")]),
    );
    assert_eq!(filled.get("x").unwrap(), "one-two");
}

/// A header reading `Bearer ${TOKEN}` is not a credential. Sending it produces a confusing 401
/// instead of the honest "not connected" a person can act on.
#[test]
fn an_unresolved_placeholder_is_dropped_not_sent_literally() {
    let (filled, unresolved) = fill_placeholders(
        &declared(&[("authorization", "Bearer ${MISSING}")]),
        &declared(&[]),
    );
    assert!(filled.is_empty(), "{filled:?}");
    assert_eq!(unresolved, vec!["MISSING".to_string()]);
}

#[test]
fn a_header_with_no_placeholder_passes_through() {
    let (filled, unresolved) =
        fill_placeholders(&declared(&[("x-client", "opengrok")]), &declared(&[]));
    assert_eq!(filled.get("x-client").unwrap(), "opengrok");
    assert!(unresolved.is_empty());
}

/// An unterminated `${` must not loop forever or panic.
#[test]
fn a_malformed_placeholder_does_not_hang() {
    let (filled, _) = fill_placeholders(&declared(&[("x", "Bearer ${OPEN")]), &declared(&[]));
    assert_eq!(filled.get("x").unwrap(), "Bearer ${OPEN");
}

/// Header names survive redaction and values do not: a 401 is much easier to debug when you
/// can see that an `authorization` header was sent at all.
#[test]
fn an_endpoint_does_not_print_its_token() {
    let endpoint = Endpoint {
        plugin: "github".to_string(),
        server: "api".to_string(),
        url: "https://mcp.example/".to_string(),
        headers: declared(&[("authorization", "Bearer gho_verysecret")]),
    };
    let printed = format!("{endpoint:?}");
    assert!(!printed.contains("gho_verysecret"), "{printed}");
    assert!(printed.contains("authorization"), "{printed}");
}

#[test]
fn tools_are_namespaced_by_plugin_and_server() {
    let endpoint = Endpoint {
        plugin: "github".to_string(),
        server: "api".to_string(),
        url: "https://x/".to_string(),
        headers: BTreeMap::new(),
    };
    assert_eq!(endpoint.qualify("search"), "github.api.search");
}

/// Two plugins bringing a `search` must stay distinguishable, or the model calls whichever won.
#[test]
fn two_plugins_with_the_same_tool_do_not_collide() {
    let first = Endpoint {
        plugin: "github".to_string(),
        server: "api".to_string(),
        url: "https://x/".to_string(),
        headers: BTreeMap::new(),
    };
    let second = Endpoint {
        plugin: "gdrive".to_string(),
        server: "api".to_string(),
        url: "https://y/".to_string(),
        headers: BTreeMap::new(),
    };
    assert_ne!(first.qualify("search"), second.qualify("search"));
}

#[test]
fn a_qualified_name_splits_back_into_its_parts() {
    let (plugin, server, tool) = split_qualified("github.api.search").unwrap();
    assert_eq!(
        (plugin.as_str(), server.as_str(), tool.as_str()),
        ("github", "api", "search")
    );
}

/// A built-in tool must not be mistaken for a plugin's.
#[test]
fn a_builtin_tool_name_is_not_a_qualified_name() {
    assert!(split_qualified("shell").is_none());
    assert!(split_qualified("read_file").is_none());
    assert!(split_qualified("github.api").is_none());
}

/// A remote tool whose own name contains a dot must still round-trip.
#[test]
fn a_remote_tool_name_may_contain_dots() {
    let (plugin, server, tool) = split_qualified("gh.api.repos.list").unwrap();
    assert_eq!(plugin, "gh");
    assert_eq!(server, "api");
    assert_eq!(tool, "repos.list", "only the first two dots are separators");
}

#[test]
fn openai_safe_tool_name_maps_dots_and_never_goes_empty() {
    assert_eq!(openai_safe_tool_name("gmail.api.send"), "gmail_api_send");
    assert_eq!(openai_safe_tool_name("shell"), "shell");
    assert_eq!(
        openai_safe_tool_name("user_machine_shell"),
        "user_machine_shell"
    );
    assert_eq!(openai_safe_tool_name(""), "_");
    assert_eq!(openai_safe_tool_name("..."), "___");
    let long = format!("{}.api.{}", "p".repeat(40), "t".repeat(40));
    let safe = openai_safe_tool_name(&long);
    assert_eq!(safe.len(), 64);
    assert!(
        safe.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    );
    assert!(!safe.contains('.'));
}

#[test]
fn colliding_sanitised_names_get_a_numeric_suffix() {
    let names = openai_unique_tool_names(
        ["shell"],
        ["gmail.api.send", "gmail_api.send", "foo.bar.baz"],
    );
    assert_eq!(
        names,
        vec![
            ("gmail.api.send".to_string(), "gmail_api_send".to_string()),
            ("gmail_api.send".to_string(), "gmail_api_send_2".to_string()),
            ("foo.bar.baz".to_string(), "foo_bar_baz".to_string()),
        ]
    );
    // A plugin that sanitises to a builtin does not steal the builtin wire name.
    let stolen = openai_unique_tool_names(["shell"], ["shell"]);
    assert_eq!(stolen[0].1, "shell_2");
    let long_a = "x".repeat(64);
    let long_b = "x".repeat(70);
    let longs = openai_unique_tool_names(Vec::<&str>::new(), [long_a.as_str(), long_b.as_str()]);
    assert_eq!(longs[0].1.len(), 64);
    assert!(longs[1].1.ends_with("_2"), "{:?}", longs[1]);
    assert!(longs[1].1.len() <= 64);
    assert_ne!(longs[0].1, longs[1].1);
}
