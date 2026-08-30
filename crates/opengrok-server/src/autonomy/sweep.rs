//! The two ticks: due schedules, and new log events.
//!
//! THE FIRING IS RECORDED BEFORE THE RUN HAPPENS, deliberately, in both sweeps. The `Fired` event
//! (and, for monitors, the `monitor_firing` guard row in the same transaction) reaches the log
//! first; only then does the run start. A crash in between leaves a firing that names a run which
//! never ran — a dangling provenance row, harmless — while the opposite order would leave a run
//! the loop guard has never heard of, and a monitor watching run events would match its own work
//! and fire forever. When only one side of a crash can be safe, it must be this side.
//!
//! Claiming already advanced the clock (schedules) or the cursor (monitors) in the claiming
//! transaction, so nothing here double-fires: every failure mode skips, none repeats.

use std::time::Duration;

use opengrok_core::id::RunId;
use opengrok_core::monitor::MonitorCommand;
use opengrok_core::schedule::ScheduleCommand;

use crate::agui::routes::AgUiState;

/// How often to look for due schedules. Cron has seconds resolution, so the tick must too.
pub const SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);

/// How often to read the log for monitors, and how much of it at once.
pub const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
const MONITOR_BATCH: i64 = 200;

/// How many schedules one tick may claim — the same anti-stampede cap recovery uses.
const CLAIM_LIMIT: i64 = 20;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Fire due schedules forever. Started by the binary; stops when the process does.
pub async fn schedules_forever(state: AgUiState) {
    loop {
        if let Err(error) = schedule_tick(&state).await {
            // Same stance as recovery: a failed tick is a warning, not an outage. The schedules
            // stay due and the next tick tries again.
            tracing::warn!(%error, "a schedule tick failed; will try again");
        }
        tokio::time::sleep(SCHEDULE_INTERVAL).await;
    }
}

pub async fn schedule_tick(state: &AgUiState) -> Result<usize, opengrok_store::StoreError> {
    let due = state
        .auth
        .store
        .claim_due_schedules(now_ms(), CLAIM_LIMIT)
        .await?;

    let mut fired = 0;
    for schedule in due {
        let run_id = RunId::new();

        // The aggregate gets the last word: a schedule paused or deleted between the claim and
        // now refuses here, and the projection having been momentarily stale fires nothing.
        let (loaded, seq) = state.auth.store.load_schedule(&schedule.id).await?;
        let events = match loaded.decide(ScheduleCommand::Fire {
            run_id: run_id.clone(),
            at_ms: now_ms(),
        }) {
            Ok(events) => events,
            Err(reason) => {
                tracing::info!(schedule = %schedule.id, %reason, "a claimed schedule declined to fire");
                continue;
            }
        };
        state
            .auth
            .store
            .append_schedule(
                &schedule.id,
                &schedule.account_id,
                seq,
                &events,
                &loaded,
                now_ms(),
            )
            .await?;

        // The run itself takes as long as a model takes; it must not hold up the other firings.
        tokio::spawn(crate::autonomy::fire(
            state.clone(),
            format!("schedule {}", schedule.id),
            schedule.account_id.clone(),
            schedule.coworker_id.clone(),
            schedule.prompt.clone(),
            // Every firing of one schedule shares a thread, so its history reads as one
            // continuing conversation rather than a pile of orphans.
            schedule.id.as_str().to_string(),
            run_id,
        ));
        fired += 1;
    }
    Ok(fired)
}

/// Match new log events against active monitors, forever.
pub async fn monitors_forever(state: AgUiState) {
    loop {
        if let Err(error) = monitor_tick(&state).await {
            tracing::warn!(%error, "a monitor tick failed; will try again");
        }
        tokio::time::sleep(MONITOR_INTERVAL).await;
    }
}

pub async fn monitor_tick(state: &AgUiState) -> Result<usize, opengrok_store::StoreError> {
    let span = state.auth.store.next_log_span(MONITOR_BATCH).await?;
    if span.is_empty() {
        return Ok(0);
    }
    let monitors = state.auth.store.active_monitors().await?;
    if monitors.is_empty() {
        return Ok(0);
    }

    let mut fired = 0;
    for event in &span {
        for (monitor_id, account_id, coworker_id, watches, prompt) in &monitors {
            if watches != &event.event_type {
                continue;
            }
            // THE LOOP GUARD. A monitor's own stream, and any run this monitor started, are
            // invisible to it — or its firings would be its triggers.
            if event.stream_id == opengrok_store::monitor_stream(monitor_id) {
                continue;
            }
            if let Some(run) = event.stream_id.strip_prefix("run/")
                && state
                    .auth
                    .store
                    .was_fired_by(monitor_id, &RunId::from_stored(run))
                    .await?
            {
                continue;
            }

            let run_id = RunId::new();
            let (loaded, seq) = state.auth.store.load_monitor(monitor_id).await?;
            let events = match loaded.decide(MonitorCommand::Fire {
                run_id: run_id.clone(),
                matched_stream: event.stream_id.clone(),
                at_ms: now_ms(),
            }) {
                Ok(events) => events,
                Err(reason) => {
                    tracing::info!(monitor = %monitor_id, %reason, "a matching monitor declined to fire");
                    continue;
                }
            };
            state
                .auth
                .store
                .append_monitor(monitor_id, account_id, seq, &events, &loaded, now_ms())
                .await?;

            // The coworker is told what woke it — the prompt alone would read as a question from
            // nowhere.
            let prompt = format!(
                "{prompt}\n\n[woken by event] {} on {}",
                event.event_type, event.stream_id
            );
            tokio::spawn(crate::autonomy::fire(
                state.clone(),
                format!("monitor {monitor_id}"),
                account_id.clone(),
                coworker_id.clone(),
                prompt,
                monitor_id.as_str().to_string(),
                run_id,
            ));
            fired += 1;
        }
    }
    Ok(fired)
}
