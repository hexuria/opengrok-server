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

use std::collections::HashMap;
use std::time::Duration;

use opengrok_core::id::{AccountId, RunId};
use opengrok_core::monitor::{MonitorCommand, is_watchable};
use opengrok_core::schedule::ScheduleCommand;

use crate::agui::routes::AgUiState;

/// How often to look for due schedules. Cron has seconds resolution, so the tick must too.
pub const SCHEDULE_INTERVAL: Duration = Duration::from_secs(1);

/// How often to read the log for monitors, and how much of it at once.
pub const MONITOR_INTERVAL: Duration = Duration::from_secs(1);
const MONITOR_BATCH: i64 = 200;

/// How long a monitor's firing counts as in flight before its run has journaled a frame. Longer
/// than a turn's wake of a sleeping computer (`TURN_WAKE_PATIENCE`, 90s), which is what stands
/// between the firing and the run's first row; short enough that a firing the policy refused, and
/// so never journaled, stops holding a slot within minutes.
const FIRING_PENDING_MS: i64 = 5 * 60 * 1000;

/// How many schedules one tick may claim — the same anti-stampede cap recovery uses.
const CLAIM_LIMIT: i64 = 20;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Fire due schedules forever. Started by the binary; stops when the process does. Takes the
/// host state rather than the bare AG-UI state because a fired run that stops on a form mints its
/// card through it (`autonomy::fire`).
pub async fn schedules_forever(gateway: crate::host_state::HostState) {
    loop {
        if let Err(error) = schedule_tick(&gateway).await {
            // Same stance as recovery: a failed tick is a warning, not an outage. The schedules
            // stay due and the next tick tries again.
            tracing::warn!(%error, "a schedule tick failed; will try again");
        }
        tokio::time::sleep(SCHEDULE_INTERVAL).await;
    }
}

pub async fn schedule_tick(
    gateway: &crate::host_state::HostState,
) -> Result<usize, opengrok_store::StoreError> {
    let state: &AgUiState = &gateway.agui;
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
            cause: opengrok_core::schedule::FireCause::Clock,
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
            gateway.clone(),
            crate::autonomy::Firing {
                origin: format!("schedule {}", schedule.id),
                account_id: schedule.account_id.clone(),
                coworker_id: schedule.coworker_id.clone(),
                prompt: schedule.prompt.clone(),
                // Every firing of one schedule shares a thread, so its history reads as one
                // continuing conversation rather than a pile of orphans.
                thread_id: schedule.id.as_str().to_string(),
                run_id,
            },
        ));
        fired += 1;
    }
    Ok(fired)
}

/// Match new log events against active monitors, forever. The host state for the same reason
/// as `schedules_forever`: a monitor's run can stop on a form too.
pub async fn monitors_forever(gateway: crate::host_state::HostState) {
    loop {
        if let Err(error) = monitor_tick(&gateway).await {
            tracing::warn!(%error, "a monitor tick failed; will try again");
        }
        tokio::time::sleep(MONITOR_INTERVAL).await;
    }
}

/// Match one span of the deployment's log against every active monitor.
///
/// THE LOG IS EVERY TENANT'S, THE MONITOR IS ONE ACCOUNT'S. An event fires a monitor only when its
/// stream resolves (`PgStore::stream_owner`, which carries the published prefix table) to the
/// account that owns the monitor. Before #179 the match was on event type alone: Alice's
/// `run-failed` monitor woke her coworker, on her points, for every other tenant's failed run,
/// and its prompt quoted their stream id.
pub async fn monitor_tick(
    gateway: &crate::host_state::HostState,
) -> Result<usize, opengrok_store::StoreError> {
    let state: &AgUiState = &gateway.agui;
    let span = state.auth.store.next_log_span(MONITOR_BATCH).await?;
    if span.is_empty() {
        return Ok(0);
    }
    let monitors = state.auth.store.active_monitors().await?;
    if monitors.is_empty() {
        return Ok(0);
    }

    // Resolved once per stream per tick, and only for events some monitor watches: a span is
    // mostly `run-emitted` frames that no monitor may watch, and they cost nothing here.
    let mut owners: HashMap<String, Option<AccountId>> = HashMap::new();
    let mut fired = 0;
    for event in &span {
        for (monitor_id, account_id, coworker_id, watches, prompt) in &monitors {
            if watches != &event.event_type {
                continue;
            }
            // A monitor stored before the published list existed may watch a type the list left
            // out (`run-emitted`, fired per token). It could not be created today, so it does not
            // fire today either.
            if !is_watchable(watches) {
                continue;
            }
            let owner = match owners.get(&event.stream_id) {
                Some(owner) => owner.clone(),
                None => {
                    let owner = match state.auth.store.stream_owner(&event.stream_id).await {
                        Ok(owner) => owner,
                        Err(error) => {
                            // The cursor is already past this span, so aborting the tick would
                            // drop every other monitor's matches too. An owner that cannot be
                            // read is an owner that does not match.
                            tracing::warn!(%error, stream = %event.stream_id, "could not resolve a stream's owner; no monitor fires on it");
                            None
                        }
                    };
                    owners.insert(event.stream_id.clone(), owner.clone());
                    owner
                }
            };
            if owner.as_ref() != Some(account_id) {
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

            // Checked before the firing is recorded, so a refused wake leaves nothing behind — no
            // `Fired` naming a run that was never started.
            match state
                .auth
                .store
                .monitor_runs_in_flight(monitor_id, now_ms() - FIRING_PENDING_MS)
                .await
            {
                Ok(in_flight) if in_flight >= crate::autonomy::MAX_RUNS_IN_FLIGHT => {
                    tracing::info!(monitor = %monitor_id, %in_flight, stream = %event.stream_id, "a matching monitor skipped: too much already running");
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, monitor = %monitor_id, "could not count a monitor's runs in flight; not firing it");
                    continue;
                }
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
                gateway.clone(),
                crate::autonomy::Firing {
                    origin: format!("monitor {monitor_id}"),
                    account_id: account_id.clone(),
                    coworker_id: coworker_id.clone(),
                    prompt,
                    thread_id: monitor_id.as_str().to_string(),
                    run_id,
                },
            ));
            fired += 1;
        }
    }
    Ok(fired)
}
