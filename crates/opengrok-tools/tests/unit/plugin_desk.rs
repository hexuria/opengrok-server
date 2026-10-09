use super::*;

/// A key, token or password in a call is refused unread, under every spelling a model reaches
/// for, on every tool: the transcript and the model's context keep whatever a call carries.
#[test]
fn a_call_carrying_a_secret_is_refused_unread() {
    for key in SECRET_KEYS {
        for tool in [ADD_PLUGIN_ACCOUNT, RENAME_PLUGIN_ACCOUNT, INSTALL_PLUGIN] {
            let arguments = json!({ "plugin": "cloudflare", "account": "conn_1", "label": "x", key: "sk-live" });
            assert_eq!(
                read(tool, &arguments),
                Err(NO_SECRETS.to_string()),
                "{tool} {key}"
            );
        }
    }
}

/// A pick names one account or clears the choice, never both and never neither; `bot` stays a
/// name to be resolved by the desk, among the person's own Bots.
#[test]
fn a_pick_is_one_account_or_ask_each_time() {
    let both = json!({"plugin":"cloudflare","account":"conn_1","ask_each_time":true});
    assert!(read(PICK_PLUGIN_ACCOUNT, &both).is_err());
    assert!(read(PICK_PLUGIN_ACCOUNT, &json!({"plugin":"cloudflare"})).is_err());
    let clear = json!({"plugin":"cloudflare","ask_each_time":true,"bot":"Jenga"});
    assert_eq!(
        read(PICK_PLUGIN_ACCOUNT, &clear),
        Ok(Ask::Pick {
            plugin: "cloudflare".into(),
            connector: None,
            account: None,
            bot: Some("Jenga".into()),
        })
    );
}

/// Every tool has a schema whose required arguments its reader asks for, so a call made from the
/// schema alone is never refused for its shape.
#[test]
fn a_call_made_from_each_schema_reads() {
    for tool in TOOLS {
        let schema = schema(tool).unwrap();
        let mut arguments = serde_json::Map::new();
        for name in schema["function"]["parameters"]["required"]
            .as_array()
            .unwrap()
        {
            let name = name.as_str().unwrap();
            let value = if name == "on" {
                json!(true)
            } else {
                json!("x")
            };
            arguments.insert(name.to_string(), value);
        }
        if tool == PICK_PLUGIN_ACCOUNT {
            arguments.insert("account".into(), json!("conn_1"));
        }
        assert!(read(tool, &Value::Object(arguments)).is_ok(), "{tool}");
    }
}
