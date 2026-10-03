use super::*;
use serde_json::json;

fn read_ok(name: &str, arguments: Value) -> Ask {
    read(name, &arguments).unwrap()
}

fn refused(name: &str, arguments: Value) -> String {
    read(name, &arguments).unwrap_err()
}

/// THE TIME AND DAYS ARE THE PERSON'S. A create with no `when` is refused in the contract's words,
/// before its prompt is looked at, and the description says never to guess one.
#[test]
fn a_create_with_no_when_is_sent_to_ask_the_person() {
    for missing in [
        json!({ "prompt": "standup" }),
        json!({ "prompt": "x", "when": [] }),
        json!({}),
    ] {
        assert_eq!(refused(CREATE_ROUTINE, missing), NO_WHEN);
    }
    assert!(
        description(CREATE_ROUTINE)
            .unwrap()
            .contains("never guessed")
    );
    assert!(
        description(CREATE_ROUTINE)
            .unwrap()
            .contains("request_user_form")
    );
    let schema = schema(CREATE_ROUTINE).unwrap();
    assert_eq!(
        schema["function"]["parameters"]["required"],
        json!(["prompt", "when"])
    );
}

/// ONE CRON, AS A STRING OR A LIST OF ONE. Two are refused in the contract's words until #315.
#[test]
fn a_routine_takes_one_cron_for_now() {
    let one = read_ok(
        CREATE_ROUTINE,
        json!({ "prompt": "p", "when": ["0 9 * * MON-FRI"] }),
    );
    let when = match one {
        Ask::Create(fields) => fields.when,
        other => Some(format!("not a create: {other:?}")),
    };
    assert_eq!(when.as_deref(), Some("0 9 * * MON-FRI"));
    let two = json!({ "prompt": "p", "when": ["0 9 * * 1", "0 17 * * 1"] });
    assert_eq!(refused(CREATE_ROUTINE, two), ONE_SCHEDULE);
    let update = json!({ "routine": "sched_1", "when": ["0 9 * * 1", { "cron": "0 10 * * 1" }] });
    assert_eq!(refused(UPDATE_ROUTINE, update), ONE_SCHEDULE);
}

/// A BOT CANNOT MAKE A WEBHOOK, however it asks: its key would pass through the chat.
#[test]
fn a_webhook_is_refused_however_it_is_asked_for() {
    for asked in [
        json!({ "prompt": "p", "kind": "webhook" }),
        json!({ "prompt": "p", "when": "webhook" }),
        json!({ "prompt": "p", "when": [{ "kind": "webhook" }] }),
        json!({ "prompt": "p", "when": "0 9 * * 1", "webhook": true }),
    ] {
        assert_eq!(
            refused(CREATE_ROUTINE, asked.clone()),
            NO_WEBHOOK,
            "{asked}"
        );
    }
    let edit = json!({ "routine": "sched_1", "when": [{ "kind": "webhook" }] });
    assert_eq!(refused(UPDATE_ROUTINE, edit), NO_WEBHOOK);
}

/// An edit names its routine and something to change; a delete names its routine. The identity
/// keys `overwrite_identity` writes are never read as anything.
#[test]
fn an_edit_and_a_delete_name_their_routine() {
    assert!(refused(UPDATE_ROUTINE, json!({ "name": "x" })).contains("by its id"));
    assert!(refused(DELETE_ROUTINE, json!({})).contains("by its id"));
    let nothing = json!({ "routine": "sched_1", "coworker_id": "cw_x", "account_id": "a" });
    assert!(refused(UPDATE_ROUTINE, nothing).starts_with("nothing to change"));
    let paused = read_ok(
        UPDATE_ROUTINE,
        json!({ "routine": "sched_1", "active": false }),
    );
    let fields = Fields {
        active: Some(false),
        ..Fields::default()
    };
    let expected = Ask::Update {
        routine: "sched_1".to_string(),
        fields,
    };
    assert_eq!(paused, expected);
    let delete = read_ok(
        DELETE_ROUTINE,
        json!({ "routine": " sched_1 ", "name": "Lies" }),
    );
    assert_eq!(
        delete,
        Ask::Delete {
            routine: "sched_1".to_string()
        }
    );
}

/// A ROUTINE IS THE CALLING BOT'S, AND NO ARGUMENT SAYS OTHERWISE (3 Oct 2026): a `bot`, or a
/// coworker under any spelling, is not read, so a create carries no target and an update that
/// names only one has nothing to change, which is a refusal, not a hand-over. Nor does the offer
/// have a place for one.
#[test]
fn no_argument_aims_a_routine_at_another_bot() {
    let aimed = json!({ "prompt": "p", "when": "0 9 * * 1", "bot": "Sol", "coworker": "cw_sol",
        "coworker_id": "cw_sol", "coworkerId": "cw_sol", "account_id": "acct_x" });
    let plain = Fields {
        prompt: Some("p".to_string()),
        when: Some("0 9 * * 1".to_string()),
        ..Fields::default()
    };
    assert_eq!(read_ok(CREATE_ROUTINE, aimed), Ask::Create(plain));
    let hand_over = json!({ "routine": "sched_1", "bot": "Sol", "coworkerId": "cw_sol" });
    assert!(refused(UPDATE_ROUTINE, hand_over).starts_with("nothing to change"));
    for name in [CREATE_ROUTINE, UPDATE_ROUTINE] {
        let schema = schema(name).unwrap();
        let properties = schema["function"]["parameters"]["properties"].as_object();
        assert!(!properties.unwrap().contains_key("bot"), "{name}");
    }
}

/// THE ONLY BOT A ROUTINE IS MADE FOR is the session's own, if it is one of the person's; any
/// other is refused in words, naming no Bot to try instead.
#[test]
fn the_session_bot_must_be_one_of_the_persons_own() {
    let theirs = bots(vec![
        ("cw_luna".to_string(), "Luna".to_string(), false),
        ("cw_sol".to_string(), "Sol".to_string(), true),
    ]);
    assert_eq!(own(&theirs, "cw_sol").unwrap().label, "Sol");
    assert!(own(&theirs, "cw_sol").unwrap().on_plan);
    let shared = own(&theirs, "cw_someone_elses").unwrap_err();
    assert!(
        shared.starts_with("this Bot is not one of your person's own"),
        "{shared}"
    );
    assert!(
        !shared.contains("Luna") && !shared.contains("Sol"),
        "{shared}"
    );
}

/// THE WORDS THE MODEL READS SAY WHOSE ROUTINES THESE ARE: this Bot's, not the person's. The
/// listing says what an empty answer means, or a Bot with none tells its person they have none.
#[test]
fn the_descriptions_say_these_are_this_bots_routines() {
    for name in TOOLS {
        let said = description(name).unwrap();
        assert!(said.contains("this Bot"), "{name}: {said}");
        assert!(!said.contains("your person's routines"), "{name}: {said}");
        assert!(
            !said.contains("one of your person's Bots"),
            "{name}: {said}"
        );
    }
    let list = description(LIST_ROUTINES).unwrap();
    assert!(
        list.contains("an empty list means this Bot has none"),
        "{list}"
    );
}

/// Every tool has its schema and words; the row's words are the group's, and nothing else is one.
#[test]
fn each_tool_is_described_and_the_row_switches_all_five() {
    for name in TOOLS {
        let schema = schema(name).unwrap();
        assert_eq!(schema["function"]["name"], name);
        assert!(is_routine_tool(name));
    }
    assert_eq!(description(ROW), Some(ROW_DESCRIPTION));
    assert!(schema(ROW).is_none() && !is_routine_tool(ROW));
    let list = schema(LIST_ROUTINES).unwrap();
    assert!(
        list["function"]["description"]
            .as_str()
            .unwrap()
            .contains("never listed")
    );
}

/// `run_routine` NAMES ITS ROUTINE by id or by name (#337), and asks no card; a run a routine
/// started, or a Bot's message, is offered the listing alone, which comes first. A name two
/// routines share is refused naming both.
#[test]
fn run_routine_takes_an_id_or_a_name_and_is_listed_last() {
    for named in ["sched_1", "Standup"] {
        let ask = read_ok(RUN_ROUTINE, json!({ "routine": named }));
        assert_eq!(
            ask,
            Ask::Run {
                routine: named.to_string()
            }
        );
    }
    let missing = refused(RUN_ROUTINE, json!({}));
    assert!(missing.contains("by its id or its name"), "{missing}");
    assert_eq!(
        TOOLS[0], LIST_ROUTINES,
        "the one a routine's run is offered"
    );
    assert_eq!(TOOLS.last(), Some(&RUN_ROUTINE));
    let schema = schema(RUN_ROUTINE).unwrap();
    let required = &schema["function"]["parameters"]["required"];
    assert_eq!(required, &json!(["routine"]));
    let said = ambiguous("Standup", &["sched_1".to_string(), "sched_2".to_string()]);
    let words = "more than one of your routines is called \"Standup\" (sched_1, sched_2); run \
                 one by its id.";
    assert_eq!(said, words);
}
