#![allow(clippy::expect_used, clippy::panic)]

use super::*;

fn created() -> Schedule {
    Schedule::replay(&[ScheduleEvent::Created {
        coworker_id: CoworkerId::from_stored("cw_1"),
        cron: "0 */5 * * * *".to_string(),
        prompt: "check the queue".to_string(),
        name: "queue check".to_string(),
        kind: WakeKind::Cron,
        hook_id: String::new(),
        secret_hash: String::new(),
        webhook_key: String::new(),
        run_limits: RunLimits::default(),
        tz: UTC.to_string(),
        at_ms: 1_000,
    }])
}

fn webhook() -> Schedule {
    Schedule::replay(&[ScheduleEvent::Created {
        coworker_id: CoworkerId::from_stored("cw_1"),
        cron: String::new(),
        prompt: "handle the ping".to_string(),
        name: "todo ping".to_string(),
        kind: WakeKind::Webhook,
        hook_id: "hook_abc".to_string(),
        secret_hash: "hash".to_string(),
        webhook_key: "og_secret".to_string(),
        run_limits: RunLimits::default(),
        tz: UTC.to_string(),
        at_ms: 1_000,
    }])
}

fn cron_wake(cron: &str) -> Wake {
    Wake::Cron {
        cron: cron.to_string(),
    }
}

/// What `normalized_cron` stores, or the refusal it says instead.
fn stored(typed: &str) -> String {
    normalized_cron(typed).unwrap_or_else(|refusal| refusal.to_string())
}

#[test]
fn five_field_cron_is_promoted_and_six_field_is_kept() {
    assert_eq!(stored("*/5 * * * *"), "0 */5 * * * *");
    assert_eq!(stored("*/2 * * * * *"), "*/2 * * * * *");
}

/// STANDARD CRON'S DAYS, one firing at a time across the week from a known Sunday. The crate
/// counts Sunday as 1: `1-5` handed to it as written ran Sunday to Thursday.
#[test]
fn a_five_field_day_of_the_week_fires_on_standard_crons_days() {
    let sunday = 1_790_467_200_000; // 2026-09-27T00:00:00Z
    let week = |cron: &str| {
        let (mut after, mut days, end) = (sunday, Vec::new(), sunday + 604_800_000);
        while let Some(next) = next_fire_ms(cron, UTC, after).filter(|next| *next < end) {
            let when = chrono::DateTime::from_timestamp_millis(next).expect("a time");
            assert_eq!(when.format("%H:%M").to_string(), "09:00", "{cron}");
            days.push(when.format("%a").to_string());
            after = next;
        }
        days.join(" ")
    };
    for (cron, days) in [
        ("0 9 * * 1-5", "Mon Tue Wed Thu Fri"),
        ("0 9 * * 0", "Sun"),
        ("0 9 * * 7", "Sun"),
        ("0 9 * * 1,3,5", "Mon Wed Fri"),
        ("0 9 * * */2", "Sun Tue Thu Sat"),
        ("0 9 * * 1-5/2", "Mon Wed Fri"),
        ("0 9 * * 5-7", "Sun Fri Sat"),
        ("0 9 * * 1/2", "Sun Mon Wed Fri"),
        ("0 9 * * MON-FRI", "Mon Tue Wed Thu Fri"),
    ] {
        assert_eq!(week(cron), days, "{cron}");
    }
}

/// What is stored names its days and reads back as the 5 fields that store it again. Six
/// fields are the crate's own, kept as written and never shown as 5 that would read Monday.
#[test]
fn a_numbered_day_is_stored_by_name_and_reads_back() {
    for (typed, named) in [
        ("0 9 * * 1-5", "0 0 9 * * MON-FRI"),
        ("0 9 * * 0,7", "0 0 9 * * SUN,SUN"),
        ("0 9 * * 5-7", "0 0 9 * * FRI-SAT,SUN"),
        ("0 9 * * 1-7/2", "0 0 9 * * MON-SAT/2,SUN"),
        ("0 9 * * */2,?,mon-Fri", "0 0 9 * * */2,?,mon-Fri"),
    ] {
        assert_eq!(stored(typed), named, "{typed}");
        assert_eq!(display_cron(named), named[2..], "{typed}");
        assert_eq!(stored(&display_cron(named)), named, "{typed}");
    }
    assert_eq!(stored("0 0 9 * * 1"), "0 0 9 * * 1");
    assert_eq!(display_cron("0 0 9 * * 1"), "0 0 9 * * 1");
}

/// A day past 7 is refused in words, and so is a numbered item the crate would read its own
/// way: a backward range, a step of 0, a number beside a name.
#[test]
fn a_day_of_the_week_past_seven_is_refused_in_words() {
    let refusal = format!("not a cron expression: 0 9 * * 8 (8 {NOT_A_DAY})");
    assert_eq!(stored("0 9 * * 8"), refusal);
    for typed in [
        "0 9 * * 1-8",
        "0 9 * * 5-1",
        "0 9 * * 1-5/0",
        "0 9 * * MON-5",
    ] {
        assert!(
            stored(typed).starts_with("not a cron expression: "),
            "{typed}"
        );
    }
}

#[test]
fn an_update_keeps_the_id_and_revalidates_the_cron() {
    let mut schedule = created();
    assert!(matches!(
        schedule.decide(ScheduleCommand::Update {
            name: "x".to_string(),
            prompt: "y".to_string(),
            wake: cron_wake("not cron"),
            coworker_id: None,
            run_limits: None,
            tz: None,
            at_ms: 2,
        }),
        Err(ScheduleError::BadCron(_))
    ));
    let events = schedule
        .decide(ScheduleCommand::Update {
            name: "Monday report".to_string(),
            prompt: "write the weekly report".to_string(),
            wake: cron_wake("0 9 * * 1"),
            coworker_id: None,
            run_limits: None,
            tz: None,
            at_ms: 2,
        })
        .expect("update");
    for event in &events {
        schedule.apply(event);
    }
    assert_eq!(schedule.name, "Monday report");
    assert_eq!(schedule.cron, "0 0 9 * * MON");
    assert_eq!(display_cron(&schedule.cron), "0 9 * * MON");
    assert_eq!(display_cron("*/2 * * * * *"), "*/2 * * * * *");
    assert_eq!(schedule.prompt, "write the weekly report");
}

#[test]
fn next_fire_is_strictly_after_the_given_moment() {
    // Every 2 seconds from t=0: the next fire after t=0 is t=2s, not t=0 again — `after` must
    // be exclusive or a claimed schedule would be claimed forever.
    let next = next_fire_ms("*/2 * * * * *", UTC, 0).expect("a next occurrence");
    assert_eq!(next, 2_000);
    let after_that = next_fire_ms("*/2 * * * * *", UTC, next).expect("another");
    assert_eq!(after_that, 4_000);
}

#[test]
fn a_bad_expression_is_refused_at_create() {
    let error = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: CoworkerId::from_stored("cw_1"),
            prompt: "hi".to_string(),
            name: String::new(),
            wake: cron_wake("every tuesday probably"),
            run_limits: RunLimits::default(),
            tz: UTC.to_string(),
            at_ms: 0,
        })
        .expect_err("should refuse");
    assert!(matches!(error, ScheduleError::BadCron(_)));
}

#[test]
fn an_empty_prompt_is_refused() {
    let error = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: CoworkerId::from_stored("cw_1"),
            prompt: "   ".to_string(),
            name: String::new(),
            wake: cron_wake("*/2 * * * * *"),
            run_limits: RunLimits::default(),
            tz: UTC.to_string(),
            at_ms: 0,
        })
        .expect_err("should refuse");
    assert!(matches!(error, ScheduleError::EmptyPrompt));
}

#[test]
fn a_paused_schedule_cannot_fire() {
    let mut schedule = created();
    schedule.apply(&ScheduleEvent::Paused { at_ms: 2_000 });
    let error = schedule
        .decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_1"),
            cause: FireCause::Clock,
            by: None,
            at_ms: 3_000,
        })
        .expect_err("paused must not fire");
    // A person's "run now" is the exception: they asked.
    let events = schedule
        .decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_manual"),
            cause: FireCause::Manual,
            by: None,
            at_ms: 3_000,
        })
        .expect("a manual fire on a paused schedule");
    for event in &events {
        schedule.apply(event);
    }
    assert!(schedule.manual_runs.contains("run_manual"));
    assert!(matches!(error, ScheduleError::Paused));
}

#[test]
fn pause_resume_fire_round_trip() {
    let mut schedule = created();
    schedule.apply(&ScheduleEvent::Paused { at_ms: 2 });
    let events = schedule
        .decide(ScheduleCommand::Resume { at_ms: 3 })
        .expect("resume");
    for event in &events {
        schedule.apply(event);
    }
    schedule
        .decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_1"),
            cause: FireCause::Clock,
            by: None,
            at_ms: 4,
        })
        .expect("a resumed schedule fires");
}

#[test]
fn a_deleted_schedule_refuses_everything() {
    let mut schedule = created();
    schedule.apply(&ScheduleEvent::Deleted { at_ms: 2 });
    assert!(matches!(
        schedule.decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_1"),
            cause: FireCause::Clock,
            by: None,
            at_ms: 3,
        }),
        Err(ScheduleError::Deleted)
    ));
    assert!(matches!(
        schedule.decide(ScheduleCommand::Pause { at_ms: 3 }),
        Err(ScheduleError::Deleted)
    ));
}

#[test]
fn a_webhook_create_skips_the_clock_and_keeps_the_secret() {
    let events = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: CoworkerId::from_stored("cw_1"),
            prompt: "handle the ping".to_string(),
            name: "todo ping".to_string(),
            wake: Wake::Webhook {
                hook_id: "hook_abc".to_string(),
                secret_hash: "hash".to_string(),
                webhook_key: "og_secret".to_string(),
            },
            run_limits: RunLimits::default(),
            tz: UTC.to_string(),
            at_ms: 1,
        })
        .expect("webhook create");
    let schedule = Schedule::replay(&events);
    assert_eq!(schedule.kind, WakeKind::Webhook);
    assert!(schedule.cron.is_empty());
    assert_eq!(schedule.hook_id, "hook_abc");
    assert_eq!(schedule.webhook_key, "og_secret");
    assert_eq!(schedule.secret_hash, "hash");
}

#[test]
fn a_webhook_without_a_secret_is_refused() {
    let error = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: CoworkerId::from_stored("cw_1"),
            prompt: "handle the ping".to_string(),
            name: "todo ping".to_string(),
            wake: Wake::Webhook {
                hook_id: "hook_abc".to_string(),
                secret_hash: String::new(),
                webhook_key: "og_secret".to_string(),
            },
            run_limits: RunLimits::default(),
            tz: UTC.to_string(),
            at_ms: 1,
        })
        .expect_err("empty hash");
    assert!(matches!(error, ScheduleError::BadWebhook));
}

#[test]
fn a_paused_webhook_refuses_an_inbound_fire_but_not_run_now() {
    let mut schedule = webhook();
    schedule.apply(&ScheduleEvent::Paused { at_ms: 2 });
    assert!(matches!(
        schedule.decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_hook"),
            cause: FireCause::Webhook,
            by: None,
            at_ms: 3,
        }),
        Err(ScheduleError::Paused)
    ));
    let events = schedule
        .decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_manual"),
            cause: FireCause::Manual,
            by: None,
            at_ms: 3,
        })
        .expect("run now on a paused webhook");
    for event in &events {
        schedule.apply(event);
    }
    assert!(schedule.manual_runs.contains("run_manual"));
    assert!(schedule.webhook_runs.is_empty());
}

#[test]
fn rotating_the_secret_replaces_the_key_and_keeps_the_hook_id() {
    let mut schedule = webhook();
    let events = schedule
        .decide(ScheduleCommand::RotateWebhookSecret {
            secret_hash: "hash2".to_string(),
            webhook_key: "og_new".to_string(),
            at_ms: 2,
        })
        .expect("rotate");
    for event in &events {
        schedule.apply(event);
    }
    assert_eq!(schedule.hook_id, "hook_abc");
    assert_eq!(schedule.secret_hash, "hash2");
    assert_eq!(schedule.webhook_key, "og_new");
    assert!(matches!(
        created().decide(ScheduleCommand::RotateWebhookSecret {
            secret_hash: "x".to_string(),
            webhook_key: "og_y".to_string(),
            at_ms: 2,
        }),
        Err(ScheduleError::NotWebhook)
    ));
}

#[test]
fn a_webhook_fire_is_labelled_webhook_not_manual() {
    let mut schedule = webhook();
    let events = schedule
        .decide(ScheduleCommand::Fire {
            run_id: RunId::from_stored("run_hook"),
            cause: FireCause::Webhook,
            by: None,
            at_ms: 2,
        })
        .expect("webhook fire");
    for event in &events {
        schedule.apply(event);
    }
    assert!(schedule.webhook_runs.contains("run_hook"));
    assert!(!schedule.manual_runs.contains("run_hook"));
}

/// The routine's thread is its id, and a person can reply into it; a run the routine never
/// fired must not be listed as one the clock did. So every firing is remembered by cause.
#[test]
fn every_firing_is_remembered_by_what_caused_it() {
    let mut schedule = created();
    for (run, cause) in [
        ("run_clock", FireCause::Clock),
        ("run_manual", FireCause::Manual),
    ] {
        let events = schedule
            .decide(ScheduleCommand::Fire {
                run_id: RunId::from_stored(run),
                cause,
                by: None,
                at_ms: 2,
            })
            .expect("fire");
        for event in &events {
            schedule.apply(event);
        }
    }
    assert!(schedule.clock_runs.contains("run_clock"));
    assert!(!schedule.clock_runs.contains("run_manual"));
    assert!(schedule.manual_runs.contains("run_manual"));
}

/// A ROUTINE CAN CHANGE HANDS WITHOUT LOSING ITS HISTORY. Delete-and-recreate would mint a
/// new id, and the id is the routine's thread — every run it ever made would fall off it.
#[test]
fn an_update_can_hand_the_routine_to_another_coworker() {
    let mut schedule = created();
    let events = schedule
        .decide(ScheduleCommand::Update {
            name: "queue check".to_string(),
            prompt: "check the queue".to_string(),
            wake: cron_wake("0 */5 * * * *"),
            coworker_id: Some(CoworkerId::from_stored("cw_2")),
            run_limits: None,
            tz: None,
            at_ms: 2,
        })
        .expect("update");
    for event in &events {
        schedule.apply(event);
    }
    assert_eq!(schedule.coworker_id, Some(CoworkerId::from_stored("cw_2")));
}

/// An edit that does not name a coworker keeps the one it had — and so does every
/// `schedule-updated` written before the field existed.
#[test]
fn an_update_without_a_coworker_keeps_the_one_it_had() {
    let mut schedule = created();
    let events = schedule
        .decide(ScheduleCommand::Update {
            name: "renamed".to_string(),
            prompt: "check the queue".to_string(),
            wake: cron_wake("0 */5 * * * *"),
            coworker_id: None,
            run_limits: None,
            tz: None,
            at_ms: 2,
        })
        .expect("update");
    for event in &events {
        schedule.apply(event);
    }
    assert_eq!(schedule.coworker_id, Some(CoworkerId::from_stored("cw_1")));

    let old: ScheduleEvent = serde_json::from_str(
        r#"{"type":"updated","name":"n","cron":"0 */5 * * * *","prompt":"p","at_ms":3}"#,
    )
    .expect("an updated event from before coworker_id");
    schedule.apply(&old);
    assert_eq!(schedule.coworker_id, Some(CoworkerId::from_stored("cw_1")));
}

/// A routine's limits are set at create, kept by an edit that says nothing about them, and
/// replaced whole by one that does — and every event written before limits existed replays
/// as one that set or changed none.
#[test]
fn a_routine_keeps_its_limits_until_an_edit_replaces_them() {
    let two = RunLimits {
        max_rounds: std::num::NonZeroU32::new(2),
        ..RunLimits::default()
    };
    let events = Schedule::default()
        .decide(ScheduleCommand::Create {
            coworker_id: CoworkerId::from_stored("cw_1"),
            prompt: "check the queue".to_string(),
            name: String::new(),
            wake: cron_wake("0 */5 * * * *"),
            run_limits: two,
            tz: UTC.to_string(),
            at_ms: 1,
        })
        .expect("create");
    let mut schedule = Schedule::replay(&events);
    assert_eq!(schedule.run_limits, two);

    let edit = |run_limits| ScheduleCommand::Update {
        name: "renamed".to_string(),
        prompt: "check the queue".to_string(),
        wake: cron_wake("0 */5 * * * *"),
        coworker_id: None,
        run_limits,
        tz: None,
        at_ms: 2,
    };
    for event in &schedule.decide(edit(None)).expect("rename") {
        schedule.apply(event);
    }
    assert_eq!(
        schedule.run_limits, two,
        "an edit that names no limits keeps them"
    );
    let old: ScheduleEvent = serde_json::from_str(
        r#"{"type":"updated","name":"n","cron":"0 */5 * * * *","prompt":"p","at_ms":3}"#,
    )
    .expect("an updated event from before run limits");
    schedule.apply(&old);
    assert_eq!(schedule.run_limits, two);
    for event in &schedule
        .decide(edit(Some(RunLimits::default())))
        .expect("clear")
    {
        schedule.apply(event);
    }
    assert!(schedule.run_limits.is_empty(), "replaced whole, so cleared");

    let old: ScheduleEvent = serde_json::from_str(
        r#"{"type":"created","coworker_id":"cw_1","cron":"0 */5 * * * *","prompt":"x","at_ms":1}"#,
    )
    .expect("a created event from before run limits");
    assert!(Schedule::replay(&[old]).run_limits.is_empty());
}

#[test]
fn old_created_events_replay_as_cron() {
    let event: ScheduleEvent = serde_json::from_str(
        r#"{"type":"created","coworker_id":"cw_1","cron":"0 */5 * * * *","prompt":"x","name":"n","at_ms":1}"#,
    )
    .expect("old created");
    let schedule = Schedule::replay(&[event]);
    assert_eq!(schedule.kind, WakeKind::Cron);
    assert!(schedule.hook_id.is_empty());
    assert!(schedule.webhook_key.is_empty());
}

/// A ROUTINE'S CRON IS READ IN ITS OWN ZONE (#316). Weekdays at nine in Manila (UTC+8, no summer
/// time) is one in the morning UTC, the same day; the same cron in UTC is nine.
#[test]
fn a_cron_is_read_in_the_routines_own_zone() {
    // Friday 2 October 2026, 00:00 UTC: Friday 08:00 in Manila.
    let friday = chrono::DateTime::parse_from_rfc3339("2026-10-02T00:00:00Z")
        .expect("a time")
        .timestamp_millis();
    let at = |tz: &str| {
        let next = next_fire_ms("0 9 * * MON-FRI", tz, friday).expect("a next wake");
        chrono::DateTime::from_timestamp_millis(next)
            .expect("a time")
            .to_rfc3339()
    };
    assert_eq!(at("Asia/Manila"), "2026-10-02T01:00:00+00:00");
    assert_eq!(at(UTC), "2026-10-02T09:00:00+00:00");
    assert_eq!(next_fire_ms("0 9 * * *", "Mars/Olympus_Mons", friday), None);
}

/// A zone is held to the IANA database in `decide`, in the account's own words, before the cron
/// is asked anything: an unknown zone is not reported as a bad cron. An edit keeps its zone, or
/// moves it; and a routine written before zones replays as the UTC its cron was read in.
#[test]
fn a_routine_keeps_an_iana_zone_and_refuses_any_other() {
    let create = |tz: &str| ScheduleCommand::Create {
        coworker_id: CoworkerId::from_stored("cw_1"),
        prompt: "standup".to_string(),
        name: String::new(),
        wake: cron_wake("0 9 * * MON-FRI"),
        run_limits: RunLimits::default(),
        tz: tz.to_string(),
        at_ms: 1,
    };
    let refused = Schedule::default()
        .decide(create("asia/manila"))
        .expect_err("a zone in the wrong case");
    assert_eq!(
        refused.to_string(),
        "\"asia/manila\" is not an IANA time zone this server knows; send one like \"Europe/London\""
    );
    let created = Schedule::default().decide(create("Asia/Manila"));
    let mut schedule = Schedule::replay(&created.expect("create"));
    assert_eq!(schedule.tz, "Asia/Manila");
    let edit = |tz: Option<&str>| ScheduleCommand::Update {
        name: "standup".to_string(),
        prompt: "standup".to_string(),
        wake: cron_wake("0 10 * * MON-FRI"),
        coworker_id: None,
        run_limits: None,
        tz: tz.map(str::to_string),
        at_ms: 2,
    };
    for event in &schedule.decide(edit(None)).expect("an edit that keeps it") {
        schedule.apply(event);
    }
    assert_eq!(schedule.tz, "Asia/Manila");
    assert!(matches!(
        schedule.decide(edit(Some("Nowhere"))),
        Err(ScheduleError::UnknownTimeZone(_))
    ));
    for event in &schedule
        .decide(edit(Some("Europe/London")))
        .expect("move it")
    {
        schedule.apply(event);
    }
    assert_eq!(schedule.tz, "Europe/London");

    let old: ScheduleEvent = serde_json::from_str(
        r#"{"type":"created","coworker_id":"cw_1","cron":"0 0 9 * * 1","prompt":"x","at_ms":1}"#,
    )
    .expect("a created event from before zones");
    assert_eq!(Schedule::replay(&[old]).tz, UTC);
}

/// THE ONE-MINUTE FLOOR (#315): only a seconds field other than `0` wakes more often than once a
/// minute, and only a 6- or 7-field expression has one.
#[test]
fn only_a_seconds_field_other_than_zero_is_under_a_minute() {
    for slow in [
        "* * * * *",
        "*/5 * * * *",
        "0 * * * * *",
        "0 0 9 1 1 * 2031",
    ] {
        assert!(!under_a_minute(slow), "{slow}");
    }
    for faster in [
        "* * * * * *",
        "*/2 * * * * *",
        "30 * * * * *",
        "0,30 * * * * *",
    ] {
        assert!(under_a_minute(faster), "{faster}");
    }
}

/// A SKIPPED FIRING IS HELD TO A FIRING'S RULES and kept for the history: a paused routine skips
/// nothing the clock or a hook asked for, and a person's "run now" is recorded either way.
#[test]
fn a_skip_is_recorded_under_the_rules_a_firing_is() {
    let skip = |cause| {
        ScheduleCommand::Skip(Skip {
            cause,
            code: "relay_offline".to_string(),
            at_ms: 3,
            by: None,
        })
    };
    let mut schedule = created();
    for event in &schedule
        .decide(skip(FireCause::Clock))
        .expect("a clock skip")
    {
        schedule.apply(event);
    }
    assert_eq!(schedule.skipped.len(), 1);
    assert_eq!(schedule.skipped[0].cause, FireCause::Clock);
    assert!(schedule.clock_runs.is_empty(), "no run");
    schedule.apply(&ScheduleEvent::Paused { at_ms: 4 });
    assert!(matches!(
        schedule.decide(skip(FireCause::Webhook)),
        Err(ScheduleError::Paused)
    ));
    let events = schedule.decide(skip(FireCause::Manual)).expect("run now");
    assert_eq!(events[0].event_type(), "schedule-skipped");
    let written = serde_json::to_value(&events[0]).expect("serialise");
    let expected = serde_json::json!({"type": "skipped", "cause": "manual",
        "code": "relay_offline", "at_ms": 3});
    assert_eq!(written, expected);
}

/// A BOT'S PRESS IS ITS OWN CAUSE (#337): its `Fired` names the Bot, as it was called then, its
/// run is labelled `bot` with that `by`, and its skip says so too; every other firing has no `by`,
/// and a `Fired` from before replays as it always did. A pause holds a Bot's press, where it does
/// not hold the person's own.
#[test]
fn a_bots_press_is_its_own_cause_and_names_the_bot() {
    let luna = FiringBot {
        coworker_id: CoworkerId::from_stored("cw_luna"),
        name: "Luna".to_string(),
    };
    let fire = |run: &str, cause, by: Option<FiringBot>| ScheduleCommand::Fire {
        run_id: RunId::from_stored(run),
        cause,
        by,
        at_ms: 2,
    };
    let mut schedule = created();
    let pressed = fire("run_bot", FireCause::Bot, Some(luna.clone()));
    let events = schedule.decide(pressed).expect("a Bot's press fires");
    let stored = serde_json::to_value(&events[0]).expect("json");
    assert_eq!(
        stored["by"],
        serde_json::json!({ "coworkerId": "cw_luna", "name": "Luna" })
    );
    let manual = fire("run_person", FireCause::Manual, None);
    let events = [events, schedule.decide(manual).expect("fires")].concat();
    let none = serde_json::to_value(&events[1]).expect("json");
    assert!(none.get("by").is_none(), "no `by` but a Bot's: {none}");
    for event in &events {
        schedule.apply(event);
    }
    assert_eq!(schedule.fired("run_bot"), Some(("bot", Some(&luna))));
    assert_eq!(schedule.fired("run_person"), Some(("manual", None)));
    assert_eq!(schedule.fired("run_a_reply"), None, "no firing of its own");

    let old: ScheduleEvent = serde_json::from_value(serde_json::json!({
        "type": "fired", "run_id": "run_old", "manual": true, "at_ms": 1 }))
    .expect("a Fired from before");
    schedule.apply(&old);
    assert_eq!(
        schedule.fired("run_old"),
        Some(("manual", None)),
        "as it always did"
    );

    schedule.apply(&ScheduleEvent::Paused { at_ms: 3 });
    let held = schedule.decide(fire("run_held", FireCause::Bot, Some(luna.clone())));
    let held = held.expect_err("a pause holds a Bot's press");
    assert_eq!(held, ScheduleError::Paused);
    let skip = ScheduleCommand::firing(
        Some(("relay_offline", "")),
        (FireCause::Bot, Some(luna.clone())),
        &RunId::from_stored("run_skipped"),
        4,
    );
    let mut alive = created();
    let skipped = alive
        .decide(skip)
        .expect("a Bot's press is skipped like the person's");
    alive.apply(&skipped[0]);
    let last = alive.skipped.last().expect("the skip");
    assert_eq!(
        (last.cause.as_str(), last.by.as_ref()),
        ("bot", Some(&luna))
    );
}
