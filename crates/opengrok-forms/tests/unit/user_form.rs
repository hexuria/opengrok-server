use super::*;
use async_trait::async_trait;
use opengrok_box::{BoxResult, CommandOutput, StartedCommand};
use std::sync::Mutex;

#[test]
fn password_and_otp_are_secret_without_the_flag() {
    let password = FormField {
        id: "p".into(),
        label: "Password".into(),
        r#type: "password".into(),
        required: true,
        secret: false,
        at: None,
    };
    let otp = FormField {
        id: "o".into(),
        label: "Code".into(),
        r#type: "otp".into(),
        required: true,
        secret: false,
        at: None,
    };
    let email = FormField {
        id: "e".into(),
        label: "Email".into(),
        r#type: "email".into(),
        required: true,
        secret: false,
        at: None,
    };
    assert!(password.is_secret());
    assert!(otp.is_secret());
    assert!(!email.is_secret());
}

#[test]
fn sanitize_drops_values_and_identity() {
    let raw = json!({
        "title": "Google account email",
        "instruction": "Enter the address.",
        "fields": [{
            "id": "email",
            "label": "Email",
            "type": "email",
            "required": true,
            "value": "should-not-keep@example.com"
        }],
        "values": { "password": "s3cret" },
        "coworker_id": "cw_x",
        "box_id": "box_x",
        "domain": "accounts.google.com",
        "liveHost": "accounts.google.com"
    });
    let cleaned = sanitize_arguments(&raw);
    assert!(cleaned.get("values").is_none(), "{cleaned}");
    assert!(cleaned.get("coworker_id").is_none(), "{cleaned}");
    let dumped = cleaned.to_string();
    assert!(!dumped.contains("s3cret"), "{dumped}");
    assert!(!dumped.contains("should-not-keep"), "{dumped}");
    assert_eq!(cleaned["title"], "Google account email");
    assert_eq!(cleaned["liveHost"], "accounts.google.com");
    assert_eq!(cleaned["fields"][0]["id"], "email");
    assert!(cleaned["fields"][0].get("value").is_none());
}

#[test]
fn collection_requires_boolean_true_in_the_selected_form_source() {
    for flag in [
        Value::Null,
        json!(false),
        json!("true"),
        json!(1),
        json!({}),
        json!([]),
    ] {
        let form = json!({"collect": flag, "fields": [{"id": "name", "value": "Juana Jane"}]});
        for input in [
            form.clone(),
            json!({"formRequest": form.clone()}),
            json!({"message": {"formRequest": form}}),
        ] {
            assert!(!is_chat_collection(&input), "{input}");
            assert!(
                sanitize_arguments(&input)["fields"][0]
                    .get("value")
                    .is_none()
            );
        }
    }
    let login = json!({"fields": [{"id": "name", "value": "Juana Jane"}]});
    assert!(!is_chat_collection(&login));
    assert!(
        sanitize_arguments(&login)["fields"][0]
            .get("value")
            .is_none()
    );
    let selected = json!({"collect": true, "title": "Selected", "fields": [{"id": "name", "value": "Juana Jane"}]});
    let shadowed = json!({"collect": false, "title": "Shadowed", "fields": [{"id": "name", "value": "Wrong value"}]});
    let input = json!({"collect": false, "fields": shadowed["fields"], "formRequest": selected.clone(), "message": {"formRequest": shadowed}});
    assert!(is_chat_collection(&input));
    assert_eq!(sanitize_arguments(&input), sanitize_arguments(&selected));
    let null_wrapper =
        json!({"collect": true, "formRequest": null, "message": {"formRequest": selected}});
    assert!(!is_chat_collection(&null_wrapper));
    assert!(form_request_from(&null_wrapper).fields.is_empty());
}

#[test]
fn a_collect_retry_and_history_filter_legacy_shared_answers() {
    let form = json!({
        "collect": true,
        "title": "New tax profile",
        "fields": [
            {"id": "x", "type": "password"},
            {"id": "x", "type": "text"},
            {"id": "password", "type": "password"},
            {"id": "name", "type": "text"}
        ]
    });
    let entry = json!({
        "message": {"type": "user-form", "formRequest": form},
        "formResolution": "submitted",
        "sharedValues": {"x": "ambiguous-value", "password": "private-value", "unknown": "extra-value", "name": "Juana Jane"}
    });
    let parsed = form_request_from(&entry);
    let raw_shared = BTreeMap::from([
        ("x".into(), "ambiguous-value".into()),
        ("password".into(), "private-value".into()),
        ("unknown".into(), "extra-value".into()),
        ("name".into(), "Juana Jane".into()),
    ]);
    let retried = model_facing_result(
        &entry,
        &parsed,
        FormResolution::Submitted,
        &raw_shared,
        false,
    );
    let history = history_line(&entry).expect("settled collect history");
    for result in [retried, history] {
        assert!(
            result.contains("Shared fields: name=Juana Jane."),
            "{result}"
        );
        assert!(result.contains("Nothing was typed into a page"), "{result}");
        for forbidden in [
            "ambiguous-value",
            "private-value",
            "extra-value",
            "filled into the page",
        ] {
            assert!(!result.contains(forbidden), "{result}");
        }
    }
}

#[test]
fn shared_answers_exclude_every_ambiguous_or_secret_id() {
    for secret in [
        json!({"id": "x", "type": "password"}),
        json!({"id": "x", "type": "OTP"}),
        json!({"id": "x", "secret": true}),
        json!({"id": "x", "type": "text"}),
    ] {
        for reversed in [false, true] {
            let mut fields = vec![secret.clone(), json!({"id": "x", "type": "text"})];
            if reversed {
                fields.reverse();
            }
            fields.extend([
                json!({"id": "name"}),
                json!({"id": "password", "type": "password"}),
            ]);
            let form = form_request_from(&json!({"fields": fields}));
            let values = BTreeMap::from([
                ("x".into(), "ambiguous-value".into()),
                ("password".into(), "private-value".into()),
                ("name".into(), "Juana Jane".into()),
                ("unknown".into(), "extra-value".into()),
            ]);
            assert_eq!(
                shared_values(&form, &values),
                BTreeMap::from([("name".into(), "Juana Jane".into())])
            );
        }
    }
}

#[test]
fn a_collect_form_preserves_prefills_from_each_supported_wrapper() {
    let form = json!({
        "collect": true,
        "fields": [
            {"id": "name", "value": " Juana Jane "},
            {"id": "tin", "value": "00000000000001"},
            {"id": "zip", "value": 1226},
            {"id": "confirmed", "value": true},
            {"id": "password", "type": "PASSWORD", "value": "private-value"},
            {"id": "otp", "type": "otp", "value": "private-value"},
            {"id": "masked", "secret": true, "value": "private-value"},
            {"id": "empty", "value": "  "},
            {"id": "null", "value": null},
            {"id": "object", "value": {}},
            {"id": "array", "value": []}
        ]
    });
    for input in [
        form.clone(),
        json!({"formRequest": form.clone()}),
        json!({"message": {"formRequest": form}}),
    ] {
        let cleaned = sanitize_arguments(&input);
        assert_eq!(cleaned["fields"][0]["value"], "Juana Jane");
        assert_eq!(cleaned["fields"][1]["value"], "00000000000001");
        assert_eq!(cleaned["fields"][2]["value"], "1226");
        assert_eq!(cleaned["fields"][3]["value"], "true");
        for index in 4..11 {
            assert!(cleaned["fields"][index].get("value").is_none(), "{cleaned}");
        }
        assert_eq!(sanitize_arguments(&cleaned), cleaned);
    }
}

#[test]
fn a_collect_form_never_prefills_an_ambiguous_field_id() {
    for secret in [
        json!({"id": "x", "type": "password", "value": "private-value"}),
        json!({"id": "x", "type": "OTP", "value": "private-value"}),
        json!({"id": "x", "type": "text", "secret": true, "value": "private-value"}),
        json!({"id": "x", "type": "text", "value": "private-value"}),
    ] {
        for reversed in [false, true] {
            let mut fields = vec![secret.clone(), json!({"id": "x", "type": "text"})];
            if reversed {
                fields.reverse();
            }
            fields.push(json!({"id": "name", "value": "Juana Jane"}));
            let cleaned = sanitize_arguments(&json!({"collect": true, "fields": fields}));
            assert!(cleaned["fields"][0].get("value").is_none(), "{cleaned}");
            assert!(cleaned["fields"][1].get("value").is_none(), "{cleaned}");
            assert_eq!(cleaned["fields"][2]["value"], "Juana Jane");
        }
    }
}

#[test]
fn a_collect_form_keeps_a_prefill_and_still_drops_a_secret() {
    let raw = json!({
        "collect": true,
        "title": "New tax profile",
        "fields": [
            { "id": "name", "label": "Name", "type": "text", "value": "Juana Jane" },
            { "id": "tin", "label": "TIN", "type": "text", "value": "000-000-000-00001" },
            { "id": "zip", "label": "ZIP", "type": "text", "value": 1226 },
            { "id": "password", "label": "Password", "type": "password", "value": "s3cret" }
        ]
    });
    let cleaned = sanitize_arguments(&raw);
    assert_eq!(cleaned["collect"], true);
    assert_eq!(cleaned["fields"][0]["value"], "Juana Jane");
    assert_eq!(cleaned["fields"][1]["value"], "000-000-000-00001");
    assert_eq!(cleaned["fields"][2]["value"], "1226");
    assert!(cleaned["fields"][3].get("value").is_none(), "{cleaned}");
    assert!(!cleaned.to_string().contains("s3cret"));
    let shared = shared_values(
        &form_request_from(&cleaned),
        &std::collections::BTreeMap::from([
            ("name".to_string(), "Juana Jane".to_string()),
            ("tin".to_string(), "00000000000001".to_string()),
        ]),
    );
    let told = collection_tool_result(&form_request_from(&cleaned), &shared);
    assert!(told.contains("Nothing was typed into a page"), "{told}");
    assert!(told.contains("name=Juana Jane"), "{told}");
    assert!(!told.contains("filled into the page"), "{told}");
    assert!(is_chat_collection(&json!({"formRequest": cleaned.clone()})));
    assert!(!is_chat_collection(
        &json!({"title": "Sign in", "fields": []})
    ));
    let settled = json!({
        "kind": "send-message",
        "id": "e_tax",
        "formResolution": "submitted",
        "sharedValues": { "name": "Juana Jane", "tin": "00000000000001" },
        "message": { "type": "user-form", "formRequest": cleaned }
    });
    let again = history_line(&settled).expect("collect history");
    assert!(again.contains("Nothing was typed into a page"), "{again}");
    assert!(again.contains("name=Juana Jane"), "{again}");
    assert!(!again.contains("filled into the page"), "{again}");
    let retried = model_facing_result(
        &settled,
        &form_request_from(&settled),
        FormResolution::Submitted,
        &shared,
        false,
    );
    assert_eq!(retried, told);
}

#[test]
fn challenge_kind_round_trips_on_sanitize() {
    let raw = json!({
        "title": "Enter code",
        "fields": [{ "id": "otp", "label": "Code", "type": "otp", "required": true }],
        "challengeKind": "otp",
        "samePage": true,
        "submit": true,
        "values": { "otp": "123456" }
    });
    let cleaned = sanitize_arguments(&raw);
    assert_eq!(cleaned["challengeKind"], "otp");
    assert_eq!(cleaned["samePage"], true);
    assert_eq!(cleaned["submit"], true);
    assert!(cleaned.get("values").is_none());
    assert!(!cleaned.to_string().contains("123456"));
}

#[test]
fn shared_values_omit_secrets() {
    let form = FormRequest {
        title: "Sign in".into(),
        instruction: String::new(),
        fields: vec![
            FormField {
                id: "email".into(),
                label: "Email".into(),
                r#type: "email".into(),
                required: true,
                secret: false,
                at: None,
            },
            FormField {
                id: "password".into(),
                label: "Password".into(),
                r#type: "password".into(),
                required: true,
                secret: false,
                at: None,
            },
        ],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: false,
        submit: false,
    };
    let values = BTreeMap::from([
        ("email".into(), "ada@example.com".into()),
        ("password".into(), "s3cret-pass".into()),
    ]);
    let shared = shared_values(&form, &values);
    assert_eq!(
        shared.get("email").map(String::as_str),
        Some("ada@example.com")
    );
    assert!(!shared.contains_key("password"));
    let content = tool_result_content(&form, FormResolution::Submitted, &shared, false);
    assert!(content.contains("ada@example.com"));
    assert!(content.contains("filled into the page"));
    assert!(content.contains("Screenshot"));
    assert!(content.contains("password-only"));
    assert!(content.contains("new entryId"));
    assert!(!content.contains("logged in"));
    assert!(!content.contains("s3cret-pass"));
    assert!(!contains_secret_value(&content, &form, &values));
}

#[test]
fn unresolved_reads_the_official_rule() {
    let pending = json!({
        "kind": "send-message",
        "id": "e_1",
        "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
    });
    assert!(is_unresolved(&pending));
    let settled = json!({
        "kind": "send-message",
        "id": "e_1",
        "formResolution": "submitted",
        "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
    });
    assert!(!is_unresolved(&settled));
    let dismissed_widget = json!({
        "kind": "send-message",
        "id": "e_1",
        "widgetDismissed": true,
        "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
    });
    assert!(!is_unresolved(&dismissed_widget));
    assert!(history_line(&pending).is_none());
    let line = history_line(&settled).expect("settled history");
    assert!(
        line.contains("submitted")
            || line.contains("filled")
            || line.contains("Filled")
            || line.contains("submitted")
            || line.contains("form")
    );
    assert!(!line.contains("s3cret"));
}

#[derive(Default)]
struct FillSpy {
    acts: Mutex<Vec<CuaAction>>,
    shots: Mutex<u32>,
    fail_type: bool,
}

#[async_trait]
impl Computer for FillSpy {
    async fn create(&self, _ttl: Option<u64>) -> BoxResult<String> {
        Ok("box".into())
    }
    async fn run(&self, _b: &str, _c: &str, _t: u32) -> BoxResult<CommandOutput> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn start(&self, _b: &str, _c: &str) -> BoxResult<StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn watch(&self, _b: &str, _p: &str) -> BoxResult<StartedCommand> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn read_file(&self, _b: &str, _p: &str) -> BoxResult<String> {
        Err(opengrok_box::BoxError::NoSuchBox)
    }
    async fn write_file(&self, _b: &str, _p: &str, _c: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn expose_port(&self, _b: &str, _p: u16, _t: &str) -> BoxResult<String> {
        Ok(String::new())
    }
    async fn stop(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn resume(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn destroy(&self, _b: &str) -> BoxResult<()> {
        Ok(())
    }
    async fn state(&self, _b: &str) -> BoxResult<String> {
        Ok("running".into())
    }
    async fn screenshot(&self, _b: &str) -> BoxResult<opengrok_box::Screenshot> {
        *self.shots.lock().unwrap() += 1;
        Err(opengrok_box::no_screen())
    }
    async fn act(&self, _b: &str, action: &CuaAction) -> BoxResult<()> {
        self.acts.lock().unwrap().push(action.clone());
        if self.fail_type && matches!(action, CuaAction::Type { .. }) {
            return Err(opengrok_box::BoxError::NoSuchBox);
        }
        Ok(())
    }
}

fn field(id: &str, kind: &str, at: Option<(i32, i32)>) -> FormField {
    FormField {
        id: id.into(),
        label: id.into(),
        r#type: kind.into(),
        required: true,
        secret: false,
        at: at.map(|(x, y)| FieldAt { x, y }),
    }
}

/// With positions, every field is clicked, emptied and typed: the password lands in the
/// password box whatever the browser had focused (the Facebook mistype of 21 Sep 2026).
#[tokio::test]
async fn positioned_fields_are_clicked_before_they_are_typed() {
    let spy = FillSpy::default();
    let form = FormRequest {
        title: "Log in".into(),
        instruction: String::new(),
        fields: vec![
            field("email", "email", Some((640, 512))),
            field("password", "password", Some((640, 560))),
        ],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: true,
        submit: true,
    };
    let values = BTreeMap::from([
        ("email".into(), "ada@example.com".into()),
        ("password".into(), "s3cret-pass".into()),
    ]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert!(
        outcomes.iter().all(|o| o.filled && !o.fill_failed),
        "{outcomes:?}"
    );
    let acts = spy.acts.lock().unwrap().clone();
    assert_eq!(
        acts,
        vec![
            CuaAction::Click {
                x: 640,
                y: 512,
                button: None
            },
            CuaAction::Key {
                key: "ctrl+a".into()
            },
            CuaAction::Type {
                text: "ada@example.com".into()
            },
            CuaAction::Click {
                x: 640,
                y: 560,
                button: None
            },
            CuaAction::Key {
                key: "ctrl+a".into()
            },
            CuaAction::Type {
                text: "s3cret-pass".into()
            },
            CuaAction::Key {
                key: "Return".into()
            },
        ],
        "click, select, type per field, no Tab, then Return: {acts:?}"
    );
}

/// The likeliest model output: one card, both fields positioned, `samePage` forgotten,
/// `submit` forgotten. Positions make it a whole fill, not a silent half-fill reported as
/// submitted — but not a Return: only a card the model marked as one page submits itself,
/// so a position gone stale while the person typed cannot post a password as a username.
#[tokio::test]
async fn a_positioned_form_types_every_field_without_same_page_but_does_not_return() {
    let spy = FillSpy::default();
    let form = FormRequest {
        title: "Log in".into(),
        instruction: String::new(),
        fields: vec![
            field("email", "email", Some((640, 512))),
            field("password", "password", Some((640, 560))),
        ],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: false,
        submit: false,
    };
    assert!(!types_only_first_field(&form));
    let values = BTreeMap::from([
        ("email".into(), "ada@example.com".into()),
        ("password".into(), "s3cret-pass".into()),
    ]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert!(
        outcomes.iter().all(|o| o.filled && !o.fill_failed),
        "{outcomes:?}"
    );
    let acts = spy.acts.lock().unwrap().clone();
    assert_eq!(
        acts.len(),
        6,
        "click, select, type ×2, and no Return: {acts:?}"
    );
    assert!(
        !acts.contains(&CuaAction::Key {
            key: "Return".into()
        }),
        "{acts:?}"
    );
    // Marked as one page, the same card logs in.
    let mut marked = form.clone();
    marked.same_page = true;
    assert!(should_press_return(&marked, 2));
    // The model is told what happened, and not the stepped advice.
    let said = tool_result_content(&form, FormResolution::Submitted, &BTreeMap::new(), false);
    assert!(said.contains("clicked at the position"), "{said}");
    assert!(!said.contains("Only the FIRST field"), "{said}");
}

/// Positions arrive as floats more often than not; a point off any screen is no point.
#[test]
fn float_positions_are_points_and_absurd_ones_are_not() {
    let form = form_request_from(&serde_json::json!({
        "title": "Log in",
        "fields": [
            { "id": "a", "label": "A", "at": { "x": 640.4, "y": 511.6 } },
            { "id": "b", "label": "B", "at": { "x": 4294968576i64, "y": 10 } },
            { "id": "c", "label": "C", "at": { "x": -3, "y": 10 } }
        ]
    }));
    assert_eq!(form.fields[0].at, Some(FieldAt { x: 640, y: 512 }));
    assert_eq!(form.fields[1].at, None);
    assert_eq!(form.fields[2].at, None);
}

/// A same-page form that skips an optional blank field still Tabs past it, so the next
/// value does not land in the blank field's box (a pre-existing miss).
#[tokio::test]
async fn a_skipped_field_on_a_same_page_form_is_still_tabbed_past() {
    let spy = FillSpy::default();
    let mut optional = field("nickname", "text", None);
    optional.required = false;
    let form = FormRequest {
        title: "Sign up".into(),
        instruction: String::new(),
        fields: vec![
            field("email", "email", None),
            optional,
            field("password", "password", None),
        ],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: true,
        submit: false,
    };
    let values = BTreeMap::from([
        ("email".into(), "ada@example.com".into()),
        ("password".into(), "s3cret-pass".into()),
    ]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert!(
        outcomes.iter().all(|o| o.filled && !o.fill_failed),
        "{outcomes:?}"
    );
    let acts = spy.acts.lock().unwrap().clone();
    assert_eq!(
        acts,
        vec![
            CuaAction::Type {
                text: "ada@example.com".into()
            },
            CuaAction::Key { key: "Tab".into() },
            CuaAction::Key { key: "Tab".into() },
            CuaAction::Type {
                text: "s3cret-pass".into()
            },
        ],
        "two Tabs: one past the blank optional field: {acts:?}"
    );
}

/// `at` is read from the tool call, and a half-given point is no point.
#[test]
fn a_field_position_is_read_from_the_request() {
    let form = form_request_from(&serde_json::json!({
        "title": "Log in",
        "samePage": true,
        "fields": [
            { "id": "email", "label": "Email", "type": "email", "at": { "x": 640, "y": 512 } },
            { "id": "password", "label": "Password", "type": "password", "at": { "x": 640 } },
            { "id": "otp", "label": "Code" }
        ]
    }));
    assert_eq!(form.fields[0].at, Some(FieldAt { x: 640, y: 512 }));
    assert_eq!(form.fields[1].at, None, "x without y is no position");
    assert_eq!(form.fields[2].at, None);
}

#[tokio::test]
async fn fill_types_only_the_first_field_by_default() {
    let spy = FillSpy::default();
    let form = FormRequest {
        title: "Sign in".into(),
        instruction: String::new(),
        fields: vec![
            FormField {
                id: "email".into(),
                label: "Email".into(),
                r#type: "email".into(),
                required: true,
                secret: false,
                at: None,
            },
            FormField {
                id: "password".into(),
                label: "Password".into(),
                r#type: "password".into(),
                required: true,
                secret: false,
                at: None,
            },
        ],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: false,
        submit: false,
    };
    let values = BTreeMap::from([
        ("email".into(), "ada@example.com".into()),
        ("password".into(), "s3cret-pass".into()),
    ]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes[0].filled && !outcomes[0].fill_failed);
    assert!(
        !outcomes[1].filled && !outcomes[1].fill_failed,
        "password is the next challenge, not a CUA miss: {:?}",
        outcomes[1]
    );
    assert_eq!(overall_resolution(&outcomes), FormResolution::Submitted);
    let acts = spy.acts.lock().unwrap().clone();
    assert_eq!(
        acts,
        vec![CuaAction::Type {
            text: "ada@example.com".into()
        }],
        "default multi-field must not Tab or Return: {acts:?}"
    );
    assert_eq!(*spy.shots.lock().unwrap(), 0, "fill must not screenshot");
}

#[tokio::test]
async fn a_single_password_field_may_press_return() {
    let spy = FillSpy::default();
    let form = FormRequest {
        title: "Password".into(),
        instruction: String::new(),
        fields: vec![FormField {
            id: "password".into(),
            label: "Password".into(),
            r#type: "password".into(),
            required: true,
            secret: false,
            at: None,
        }],
        domain: None,
        live_host: None,
        challenge_kind: Some("password".into()),
        passkey_mode: None,
        same_page: false,
        submit: false,
    };
    let values = BTreeMap::from([("password".into(), "s3cret-pass".into())]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert_eq!(overall_resolution(&outcomes), FormResolution::Submitted);
    let acts = spy.acts.lock().unwrap().clone();
    assert_eq!(
        acts,
        vec![
            CuaAction::Type {
                text: "s3cret-pass".into()
            },
            CuaAction::Key {
                key: "Return".into()
            },
        ]
    );
    assert_eq!(*spy.shots.lock().unwrap(), 0);
}

#[tokio::test]
async fn same_page_with_submit_tabs_and_returns() {
    let spy = FillSpy::default();
    let form = FormRequest {
        title: "Sign in".into(),
        instruction: String::new(),
        fields: vec![
            FormField {
                id: "email".into(),
                label: "Email".into(),
                r#type: "email".into(),
                required: true,
                secret: false,
                at: None,
            },
            FormField {
                id: "password".into(),
                label: "Password".into(),
                r#type: "password".into(),
                required: true,
                secret: false,
                at: None,
            },
        ],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: true,
        submit: true,
    };
    let values = BTreeMap::from([
        ("email".into(), "ada@example.com".into()),
        ("password".into(), "s3cret-pass".into()),
    ]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert!(outcomes.iter().all(|o| o.filled && !o.fill_failed));
    let acts = spy.acts.lock().unwrap().clone();
    assert_eq!(
        acts,
        vec![
            CuaAction::Type {
                text: "ada@example.com".into()
            },
            CuaAction::Key { key: "Tab".into() },
            CuaAction::Type {
                text: "s3cret-pass".into()
            },
            CuaAction::Key {
                key: "Return".into()
            },
        ]
    );
    assert_eq!(*spy.shots.lock().unwrap(), 0);
}

#[test]
fn return_is_opt_in_on_multi_field_forms() {
    let email = FormField {
        id: "email".into(),
        label: "Email".into(),
        r#type: "email".into(),
        required: true,
        secret: false,
        at: None,
    };
    let password = FormField {
        id: "password".into(),
        label: "Password".into(),
        r#type: "password".into(),
        required: true,
        secret: false,
        at: None,
    };
    let multi = FormRequest {
        fields: vec![email.clone(), password.clone()],
        ..FormRequest::default()
    };
    assert!(
        !should_press_return(&multi, 1),
        "default multi-field must not Return"
    );
    assert!(types_only_first_field(&multi));
    let single = FormRequest {
        fields: vec![password.clone()],
        challenge_kind: Some("password".into()),
        passkey_mode: None,
        ..FormRequest::default()
    };
    assert!(should_press_return(&single, 1));
    let same_page_submit = FormRequest {
        fields: vec![email, password],
        same_page: true,
        submit: true,
        ..FormRequest::default()
    };
    assert!(should_press_return(&same_page_submit, 2));
    assert!(!types_only_first_field(&same_page_submit));
}

#[test]
fn submitted_values_keep_only_this_forms_fields() {
    let form = FormRequest {
        title: "Sign in".into(),
        instruction: String::new(),
        fields: vec![FormField {
            id: "email".into(),
            label: "Email".into(),
            r#type: "email".into(),
            required: true,
            secret: false,
            at: None,
        }],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: false,
        submit: false,
    };
    let raw = json!({
        "email": "ada@example.com",
        "password": "s3cret",
        "coworker_id": "cw_x"
    });
    let values = submitted_values(&form, &raw);
    assert_eq!(
        values.get("email").map(String::as_str),
        Some("ada@example.com")
    );
    assert!(!values.contains_key("password"));
    assert!(!values.contains_key("coworker_id"));
}

#[test]
fn field_outcomes_are_the_recovery_card_shape() {
    let outcomes = vec![
        FieldOutcome {
            id: "email".into(),
            filled: true,
            fill_failed: false,
        },
        FieldOutcome {
            id: "password".into(),
            filled: false,
            fill_failed: true,
        },
    ];
    assert_eq!(overall_resolution(&outcomes), FormResolution::FillFailed);
    let dumped = serde_json::to_value(&outcomes).expect("outcomes json");
    assert_eq!(dumped[0]["id"], "email");
    assert_eq!(dumped[0]["filled"], true);
    assert_eq!(dumped[0]["fillFailed"], false);
    assert_eq!(dumped[1]["fillFailed"], true);
}

#[test]
fn live_handoff_holds_the_screen_after_the_form_settles() {
    let form = json!({
        "kind": "send-message",
        "id": "e_form",
        "formResolution": "escalated",
        "widgetDismissed": true,
        "message": { "type": "user-form", "formRequest": { "title": "x", "fields": [] } }
    });
    assert!(!is_unresolved(&form));
    assert!(
        !holds_the_screen(&form),
        "escalated form alone must not hold"
    );
    let live = json!({
        "kind": "send-message",
        "id": "e_hand",
        "message": { "type": "attachment", "url": "sand://box" },
        "boxRequestId": "req_1",
        "boxInstruction": "Finish this form on the computer."
    });
    assert!(is_live_handoff(&live));
    assert!(holds_the_screen(&live));
    let handed_back = json!({
        "kind": "send-message",
        "id": "e_hand",
        "message": { "type": "attachment", "url": "sand://box" },
        "boxRequestId": "req_1",
        "boxResolution": "handed_back"
    });
    assert!(!is_live_handoff(&handed_back));
    assert!(!holds_the_screen(&handed_back));
    let declined = json!({
        "kind": "send-message",
        "id": "e_hand",
        "message": { "type": "attachment", "url": "sand://box" },
        "boxRequestId": "req_1",
        "boxResolution": "declined"
    });
    assert!(!is_live_handoff(&declined));
    assert!(HAND_BACK_TOOL_RESULT.contains("Screenshot"));
    assert!(!HAND_BACK_TOOL_RESULT.to_lowercase().contains("logged in"));
}

#[tokio::test]
async fn a_failed_type_is_fill_failed_and_does_not_press_return() {
    let spy = FillSpy {
        fail_type: true,
        ..FillSpy::default()
    };
    let form = FormRequest {
        title: "Sign in".into(),
        instruction: String::new(),
        fields: vec![FormField {
            id: "password".into(),
            label: "Password".into(),
            r#type: "password".into(),
            required: true,
            secret: false,
            at: None,
        }],
        domain: None,
        live_host: None,
        challenge_kind: None,
        passkey_mode: None,
        same_page: false,
        submit: false,
    };
    let values = BTreeMap::from([("password".into(), "s3cret-pass".into())]);
    let outcomes = fill_into_focus(&spy, "box_1", &form, &values).await;
    assert_eq!(overall_resolution(&outcomes), FormResolution::FillFailed);
    assert_eq!(outcomes[0].id, "password");
    assert!(outcomes[0].fill_failed);
    let acts = spy.acts.lock().unwrap().clone();
    assert!(
        !acts
            .iter()
            .any(|act| matches!(act, CuaAction::Key { key } if key == "Return")),
        "partial fill must not submit the page: {acts:?}"
    );
    assert_eq!(*spy.shots.lock().unwrap(), 0);
}

#[test]
fn timed_out_dismiss_short_circuits() {
    let form = FormRequest {
        title: "Sign in".into(),
        ..FormRequest::default()
    };
    let line = tool_result_content(&form, FormResolution::Dismissed, &BTreeMap::new(), true);
    assert_eq!(line, HOLD_TIMED_OUT_TOOL_RESULT);
    let settled = json!({
        "kind": "send-message",
        "id": "e_1",
        "formResolution": "dismissed",
        "timedOut": true,
        "message": { "type": "user-form", "formRequest": { "title": "Sign in", "fields": [] } }
    });
    let history = history_line(&settled).expect("timed out history");
    assert_eq!(history, HOLD_TIMED_OUT_TOOL_RESULT);
}
