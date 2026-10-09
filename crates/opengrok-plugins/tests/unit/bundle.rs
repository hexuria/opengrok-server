#![allow(clippy::unwrap_used)]
use super::*;
fn files(manifest: &str, mcp: &str) -> BTreeMap<String, String> {
    [
        (".grok-plugin/plugin.json".into(), manifest.into()),
        (".mcp.json".into(), mcp.into()),
    ]
    .into()
}
#[test]
fn marketplace_transports_are_adapted_without_launching_processes() {
    let mut files = files(
        r#"{"name":"demo","author":"Upstream","hooks":{}}"#,
        r#"{"mcpServers":{
      "hosted":{"type":"http","url":"https://mcp.example.com","headers":{"Authorization":"Bearer ${DEMO_TOKEN}"}},
      "local":{"command":"uvx","args":["server"]},
      "legacy":{"type":"sse","url":"https://mcp.example.com/sse"},
      "bad":{"type":"custom"}}}"#,
    );
    files.insert("commands/deploy.md".into(), "run it".into());
    files.insert(
        "skills/triage/SKILL.md".into(),
        "---\nname: triage\ndescription: Triage\n---\nRead first.".into(),
    );
    files.insert("skills/triage/reference.txt".into(), "reference".into());
    let bundle = Bundle::from_files(&files).unwrap();
    assert_eq!(bundle.mcp.servers.len(), 1);
    assert_eq!(bundle.connectors(), vec!["demo"]);
    assert_eq!(bundle.skills.len(), 1);
    assert_eq!(bundle.files.len(), 1);
    for kind in ["commands", "hooks"] {
        assert!(
            bundle
                .parts
                .iter()
                .any(|p| p.kind == kind && !p.supported && p.reason.is_some())
        );
    }
    for name in ["local", "legacy", "bad"] {
        assert!(
            bundle
                .parts
                .iter()
                .any(|p| p.name == name && !p.supported && p.reason.is_some())
        );
    }
    assert_eq!(bundle.plugin().trust, Trust::Unverified);
}
#[test]
fn root_agent_plugins_and_bad_skills_are_reported() {
    let mut files: BTreeMap<String, String> =
        [("plugin.json".into(), r#"{"name":"demo"}"#.into())].into();
    files.insert(
        "skills/broken/SKILL.md".into(),
        "---\nname: broken\nno closing fence".into(),
    );
    let bundle = Bundle::from_files(&files).unwrap();
    assert!(bundle.skills.is_empty());
    assert!(!bundle.parts[0].supported);
    assert!(bundle.parts[0].reason.is_some());
}
#[test]
fn malformed_manifests_and_ambiguous_names_are_refused() {
    for manifest in ["not JSON", r#"{"name":"../escape"}"#, r#"{"name":"a.b"}"#] {
        assert!(Bundle::from_files(&files(manifest, r#"{"mcpServers":{}}"#)).is_err());
    }
}
#[test]
fn a_plugin_author_is_never_presented_as_the_account_or_a_colleague() {
    let framed = crate::skill::fenced_skill(
        crate::skill::SkillDoor::Read,
        "demo.triage",
        "body",
        "marker",
        crate::skill::SkillAuthor::Plugin,
        "",
    );
    assert!(framed.contains("THIRD-PARTY PLUGIN"));
    assert!(framed.ends_with(crate::skill::SKILL_CLOSING_LINE));
}

#[test]
fn explicitly_supplied_bearers_stay_with_their_own_server() {
    let bundle = Bundle::from_files(&files(r#"{"name":"demo"}"#,r#"{"mcpServers":{"public":{"type":"http","url":"https://example.com/public"},"private":{"type":"http","url":"https://example.com/private"}}}"#)).unwrap();
    assert_eq!(bundle.connectors(), vec!["private", "public"]);
    let plugin = bundle.plugin_with_values(&[("PRIVATE_TOKEN".into(), "secret".into())].into());
    match &plugin.mcp.servers["private"] {
        McpServer::StreamableHttp { headers, .. } => {
            assert_eq!(headers["Authorization"], "Bearer ${PRIVATE_TOKEN}")
        }
        _ => unreachable!(),
    }
    match &plugin.mcp.servers["public"] {
        McpServer::StreamableHttp { headers, .. } => assert!(headers.is_empty()),
        _ => unreachable!(),
    }
    match &bundle.mcp.servers["private"] {
        McpServer::StreamableHttp { headers, .. } => assert!(headers.is_empty()),
        _ => unreachable!(),
    }
}

#[test]
fn routing_headers_do_not_hide_the_bearer_connector() {
    let bundle = Bundle::from_files(&files(r#"{"name":"demo"}"#,r#"{"mcpServers":{"demo":{"type":"http","url":"https://example.com/mcp/oauth","headers":{"x-client-source":"opengrok"}}}}"#)).unwrap();
    assert_eq!(bundle.connectors(), vec!["demo"]);
    let plugin = bundle.plugin_with_values(&[("DEMO_TOKEN".into(), "secret".into())].into());
    if let McpServer::StreamableHttp { headers, .. } = &plugin.mcp.servers["demo"] {
        assert_eq!(headers["x-client-source"], "opengrok");
        assert_eq!(headers["Authorization"], "Bearer ${DEMO_TOKEN}");
    }
}

#[test]
fn a_hosted_server_must_name_a_public_host() {
    for url in [
        "https://mcp.example.com/mcp",
        "https://mcp.example.com:8443/mcp",
        "https://8.8.8.8/mcp",
        "https://[2606:4700::1111]/mcp",
    ] {
        assert!(public_https(url), "{url}");
    }
    for url in [
        "http://mcp.example.com/mcp",
        "https://localhost/mcp",
        "https://LOCALHOST./mcp",
        "https://x.localhost/mcp",
        "https://127.0.0.1/mcp",
        "https://127.1/mcp",
        "https://0x7f.1/mcp",
        "https://2130706433/mcp",
        "https://10.1.2.3/mcp",
        "https://172.16.0.1/mcp",
        "https://192.168.1.1/mcp",
        "https://169.254.169.254/latest",
        "https://100.64.0.1/mcp",
        "https://0.0.0.0/mcp",
        "https://[::1]/mcp",
        "https://[fe80::1]/mcp",
        "https://[fd12::1]/mcp",
        "https://[::ffff:10.0.0.1]/mcp",
        "https://[2001:db8::1]/mcp",
        "https://user@mcp.example.com/mcp",
        "https:///mcp",
    ] {
        assert!(!public_https(url), "{url}");
    }
}

#[test]
fn each_unsupported_skill_says_which_rule_it_broke() {
    let mut files: BTreeMap<String, String> =
        [("plugin.json".into(), r#"{"name":"demo"}"#.into())].into();
    let long_body = format!(
        "---\nname: long\n---\n{}",
        "x".repeat(MAX_SKILL_BODY_CHARS + 1)
    );
    let long_description = format!(
        "---\nname: wordy\ndescription: {}\n---\nbody",
        "d".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1)
    );
    for (name, text) in [
        ("Bad_Name", "---\nname: x\n---\nbody".to_string()),
        ("open", "---\nname: open\nno closing fence".to_string()),
        ("long", long_body),
        ("wordy", long_description),
    ] {
        files.insert(format!("skills/{name}/SKILL.md"), text);
    }
    let bundle = Bundle::from_files(&files).unwrap();
    assert!(bundle.skills.is_empty());
    let reason = |name: &str| {
        let part = bundle.parts.iter().find(|p| p.name == name).unwrap();
        part.reason.clone().unwrap()
    };
    assert_eq!(reason("Bad_Name"), "skill name is not a valid name");
    assert_eq!(reason("open"), "SKILL.md frontmatter has no closing fence");
    let cap = crate::skill::MAX_SKILL_BODY_CHARS;
    assert_eq!(
        reason("long"),
        format!("skill body exceeds {cap} characters")
    );
    let cap = crate::skill::MAX_SKILL_DESCRIPTION_CHARS;
    assert_eq!(
        reason("wordy"),
        format!("skill description exceeds {cap} characters")
    );
}
