//! Run replay and thread history routes.

use super::*;

/// Replay a run from the log.
///
/// THIS IS THE PROMISE, MADE CHECKABLE. Close the tab mid-run, come back, ask here: every event
/// the run produced is returned, in order, without asking a model anything a second time.
pub async fn replay_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let run_id = RunId::from_stored(run_id);

    // LAYER 4 (`docs/PLAN.md` §4.5): a run holds a whole conversation, so without this check a run
    // id is a password — and run ids travel in client URLs and logs. `NOT_FOUND` rather than
    // `FORBIDDEN` for both "no such run" and "not yours", so probing ids reveals nothing about
    // which runs exist.
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    match state.auth.store.run_owned_by(&run_id, &account_id).await {
        Ok(true) => {}
        Ok(false) => return (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    }

    let (run, _) = match state.auth.store.load_run(&run_id).await {
        Ok(loaded) => loaded,
        Err(error) => {
            return (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response();
        }
    };

    if !run.started {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    }

    let window = run_time_window(&run.emitted);
    // The person's words ride the replay, and only the replay: an attached stream (`attach`)
    // sends what the live stream sent, which never carried them.
    let events = super::history::with_prompt_frames(
        &run,
        events_for_client(&state, &account_id, &run, window).await,
    );

    Json(serde_json::json!({
        "runId": run_id.as_str(),
        "threadId": run.thread_id,
        "status": run.status.as_str(),
        // When the turn began. A client that picks a run up after a restart has no bubble for
        // it and has to make one; without this it would stamp that bubble with the moment it
        // noticed, and the turn would sort to the wrong place in the thread for good.
        "startedAtMs": started_at_ms(window),
        "failure": run.failure,
        // Why a finished run finished, when it was not simply done: `"budget"` for a run that
        // reached a cap and ended on its wrap-up (#244). Null otherwise.
        "finishReason": run.finish_reason.map(|reason| reason.as_str()),
        "pending": run.pending,
        "events": events,
    }))
    .into_response()
}

/// The first and last timestamps a run has logged; `None` before it has logged any.
pub(super) fn run_time_window(emitted: &[serde_json::Value]) -> Option<(i64, i64)> {
    let mut times = emitted
        .iter()
        .filter_map(|event| event.get("timestamp").and_then(serde_json::Value::as_i64));
    let first = times.next()?;
    Some((first, times.next_back().unwrap_or(first)))
}

/// The window a run with no timestamps hydrates against: one no card falls inside. "Everything"
/// folded every card the coworker ever raised into a run that had not logged a timestamp yet.
pub(super) const EMPTY_WINDOW: (i64, i64) = (i64::MAX, i64::MIN);

/// `startedAtMs` for a run: 0 until it has logged a timestamp, as it always said. The empty
/// window's bound is not a time — past JavaScript's safe integers, and an Invalid Date.
pub(super) fn started_at_ms(window: Option<(i64, i64)>) -> i64 {
    window.map_or(0, |(started, _)| started)
}

pub(super) async fn events_for_client(
    state: &AgUiState,
    account_id: &AccountId,
    run: &opengrok_core::run::Run,
    window: Option<(i64, i64)>,
) -> Vec<serde_json::Value> {
    let (started_at_ms, updated_at_ms) = window.unwrap_or(EMPTY_WINDOW);
    let forms = match run.coworker_id.as_ref() {
        Some(coworker_id) => state
            .auth
            .store
            .gateway_transcript(coworker_id, account_id)
            .await
            .unwrap_or_default(),
        None => Vec::new(),
    };
    crate::agui::user_form::hydrate_agui_events(
        run.emitted.clone(),
        &forms,
        started_at_ms,
        updated_at_ms,
    )
}

/// How many runs a thread answers with when the caller does not ask for a number.
///
/// Twenty, which is what seam A's routine run list (removed in P0-E) asked `runs_for_thread` for,
/// and what the server-side history window uses. Two readers of the same history disagreeing about
/// how much of it is "recent" gets reported as "the app shows fewer turns than the pane does", and
/// the cheapest way not to have that conversation is to pick the number once. Twenty turns is also
/// more than a screenful, which is what a client reopening a conversation actually has to draw.
const THREAD_RUNS_DEFAULT: i64 = 20;

/// The most runs one request may ask for.
///
/// A run carries EVERY frame it emitted, and a streamed answer is hundreds of text deltas, so the
/// response grows with the size of the conversation and not with the number of runs in it — an
/// uncapped `?limit=` is a way to ask this server to send megabytes. A hundred turns bounds that
/// at something a client can still render, and a client that wants a deeper history than this is
/// not drawing a conversation: `?events=false` is the cheap way to ask which runs exist, and
/// `GET /ag-ui/runs/{id}` fetches the few whose frames are actually missing.
const THREAD_RUNS_MAX: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ThreadHistoryQuery {
    /// How many runs, counted from the newest end and clamped to `THREAD_RUNS_MAX`. Counting from
    /// the newest end is what makes a limit useful on a long thread: the turns a person is coming
    /// back to are the last ones, and they are still handed back oldest-first.
    pub limit: Option<i64>,
    /// `false` keeps every run and drops its frames, for a client that only wants to know which
    /// runs exist — the desktop working out which transcripts it is missing before it fetches
    /// them one at a time. Defaults to true, because the whole point of this route is the frames.
    pub events: Option<bool>,
}

/// One run as a thread's history lists it. `events` is the only optional part: everything else
/// costs a handful of bytes and a client that has to branch on which fields arrived is a client
/// that will get the branch wrong.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadRunReplay {
    run_id: String,
    status: &'static str,
    started_at_ms: i64,
    updated_at_ms: i64,
    failure: Option<String>,
    /// See `replay_run`'s `finishReason`.
    finish_reason: Option<&'static str>,
    /// Whose turn it was: in a pair's thread, which of its two Bots (#314). Null for none.
    coworker_id: Option<String>,
    /// ABSENT, not empty, when `?events=false` asked for the list without the bodies. An empty
    /// array would say this run emitted nothing, which is a different claim and a false one.
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<serde_json::Value>>,
}

/// Replay a whole thread from the log — `GET /ag-ui/threads/{thread_id}?limit=&events=`.
///
/// THE SAME PROMISE AS `replay_run`, ASKED THE WAY A PERSON REMEMBERS THINGS. A reconnecting
/// stream knows its run id and asks for that one run; somebody who closed the app, or switched to
/// another coworker and came back, knows only who they were talking to. Without a way to ask for
/// the conversation, a client has to keep its own copy of the transcript and trust it over ours —
/// and a local copy that is authoritative is exactly the arrangement that loses the messages a
/// turn produced after the app stopped watching.
///
/// BOTH SIDES OF THE CONVERSATION. A run's log holds the events it EMITTED — the coworker's text,
/// its tool calls, their results — and, on `RunEvent::Started::prompt`, the messages the person
/// sent that turn. Each run's replay carries those as `TEXT_MESSAGE_*` frames with `role: "user"`
/// under the client's own message id, right after its `RUN_STARTED` (`history::with_prompt_frames`),
/// so a new device draws the questions with the answers. A run journaled before prompts were
/// kept has only the coworker's half; nothing can recover the other one.
pub async fn replay_thread(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(thread_id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<ThreadHistoryQuery>,
) -> Response {
    // LAYER 4 (`docs/PLAN.md` §4.5), and the reasoning is `replay_run`'s: a thread holds a whole
    // conversation — more of one than a run does — so without this check a thread id is a
    // password, and thread ids travel in client URLs and logs. `NOT_FOUND` rather than `FORBIDDEN`
    // for "no such thread", "not yours" and "not signed in" alike, so probing ids reveals nothing
    // about which threads exist or who they belong to.
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such thread").into_response();
    };
    let limit = query
        .limit
        .unwrap_or(THREAD_RUNS_DEFAULT)
        .clamp(1, THREAD_RUNS_MAX);
    let with_events = query.events.unwrap_or(true);

    // Owner-filtered in the query, so "not yours" and "no such thread" arrive here as the same
    // empty answer and cannot be told apart even by accident.
    let store = &state.auth.store;
    let newest_first = match store
        .runs_for_thread_owned_by(&thread_id, &account_id, limit)
        .await
    {
        Ok(runs) => runs,
        Err(error) => return unavailable(error),
    };
    // The turns this account hid, named rather than silently missing: a client keeps its own
    // copy of a thread, and a name it is not told stays on its screen. Asked before the empty
    // check, because a thread whose every turn was hidden is exactly the one whose client most
    // needs to hear which names to put away — and it is also the answer that would otherwise
    // read as "no such thread" and leave that client painting from its cache for good.
    let hidden = match store.hidden_runs_in_thread(&thread_id, &account_id).await {
        Ok(hidden) => hidden,
        Err(error) => return unavailable(error),
    };
    let pending = crate::agui::pending::thread_pending_json(&state, &thread_id, &account_id);
    let pending = match pending.await {
        Ok(pending) => pending,
        Err(error) => return unavailable(error),
    };
    // A Bot's main chat carries its timeline rows (#314), even before the person has said a word.
    let timeline = match crate::pairs::timeline(&state, &account_id, &thread_id).await {
        Ok(timeline) => timeline,
        Err(error) => return unavailable(error),
    };
    if newest_first.is_empty() && hidden.is_empty() && timeline.is_empty() {
        return (StatusCode::NOT_FOUND, "no such thread").into_response();
    }

    let mut runs = Vec::with_capacity(newest_first.len());
    let mut forms_by_coworker: std::collections::HashMap<CoworkerId, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    // OLDEST FIRST, which is the other way round from the store. `runs_for_thread_owned_by` hands
    // back the newest runs because that is how a limit has to be counted on a long thread; a
    // transcript is read in the order it happened. Reversing once here, rather than leaving it to
    // each caller, is what stops two clients from disagreeing about which end a conversation
    // starts at — and a disagreement like that shows up as messages in the wrong order, which
    // reads as lost work.
    for summary in newest_first.into_iter().rev() {
        let (run, _) = match state.auth.store.load_run(&summary.id).await {
            Ok(loaded) => loaded,
            Err(error) => return unavailable(error),
        };
        // A run the log has no start for never took its turn, so it has nothing to contribute to a
        // transcript. `replay_run` answers `404` when one is asked about by id; a history simply
        // leaves it out, because the question here is what happened and nothing did.
        if !run.started {
            continue;
        }
        let events = if with_events {
            let forms = match run.coworker_id.as_ref() {
                Some(coworker_id) => {
                    if let Some(cached) = forms_by_coworker.get(coworker_id) {
                        cached.clone()
                    } else {
                        let loaded = state
                            .auth
                            .store
                            .gateway_transcript(coworker_id, &account_id)
                            .await
                            .unwrap_or_default();
                        forms_by_coworker.insert(coworker_id.clone(), loaded.clone());
                        loaded
                    }
                }
                None => Vec::new(),
            };
            Some(super::history::with_prompt_frames(
                &run,
                crate::agui::user_form::hydrate_agui_events(
                    run.emitted.clone(),
                    &forms,
                    summary.started_at_ms,
                    summary.updated_at_ms,
                ),
            ))
        } else {
            None
        };
        runs.push(ThreadRunReplay {
            run_id: summary.id.as_str().to_string(),
            // The aggregate's status, not the projection's, for the same reason `replay_run` uses
            // it: the log is the truth and the view is derived from it.
            status: run.status.as_str(),
            started_at_ms: summary.started_at_ms,
            updated_at_ms: summary.updated_at_ms,
            failure: run.failure,
            finish_reason: run.finish_reason.map(|reason| reason.as_str()),
            coworker_id: run.coworker_id.map(|id| id.as_str().to_string()),
            // The frames are loaded either way: whether a run started and why it failed are only
            // knowable from its log, and answering those two from the projection would mean
            // guessing. `events=false` saves the client the megabytes, not the server the read.
            events,
        });
    }

    // What kind of thread this is and who is in it, on every thread, so no client has to guess
    // from an id (#314): a pair's is its two Bots' and read-only, a chat's is the person's.
    let pair = opengrok_wire::pair::is_pair_thread(&thread_id);
    let ran = runs
        .iter()
        .filter_map(|run| run.coworker_id.clone())
        .collect();
    let coworkers = crate::pairs::coworkers(&state, &thread_id, ran).await;
    let mut body = serde_json::json!({
        "threadId": thread_id,
        "kind": if pair { "pair" } else { "chat" },
        "readOnly": pair,
        "coworkers": coworkers,
        "runs": runs,
        "hiddenRunIds": hidden,
        "timeline": timeline,
    });
    // Sibling of `runs`, never mixed into a run's frames: a queued send is not a turn yet, and
    // dropping it into `events` would make NativeChat paint a bubble as if the coworker had
    // already seen it. `pendingEvents` are CUSTOM `pending-user-message` snapshots for clients
    // that already walk CUSTOM; `pendingUserMessages` is the array to replace a local queue with.
    if let Some(object) = body.as_object_mut()
        && let Some(pending) = pending.as_object()
    {
        for (key, value) in pending {
            object.insert(key.clone(), value.clone());
        }
    }
    Json(body).into_response()
}

/// The most conversations one page of `GET /ag-ui/threads` holds, and how many when unasked.
/// A row is a handful of fields, so the cap is about a sidebar, not about bytes.
const THREAD_LIST_MAX: i64 = 100;
const THREAD_LIST_DEFAULT: i64 = 20;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListQuery {
    pub coworker_id: Option<String>,
    pub limit: Option<i64>,
    /// The keyset cursor: the last row's `updatedAtMs` and `threadId`. A page boundary can fall
    /// inside one millisecond, so the time alone would skip or repeat the threads that share it.
    pub before: Option<i64>,
    pub before_thread_id: Option<String>,
}

/// One conversation in the list. `coworkerId` and `title` are `null`, never absent, when there
/// is none — a client that has to branch on which fields arrived will get the branch wrong.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ThreadListRow {
    thread_id: String,
    coworker_id: Option<String>,
    origin: &'static str,
    /// A pair's thread's two Bots, lower id first (#314); absent on any other thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    peer_bot_ids: Option<[String; 2]>,
    title: Option<String>,
    last_run_id: String,
    last_status: String,
    updated_at_ms: i64,
}

/// The caller's conversations, newest first — `GET /ag-ui/threads?coworkerId=&limit=&before=&beforeThreadId=`.
///
/// WITHOUT THIS A CLIENT'S OWN COPY IS THE ONLY INDEX. `GET /ag-ui/threads/{id}` answers whoever
/// holds an id, and nothing handed the ids out, so a fresh device signed in to no history (#230).
///
/// A `401 {error}` rather than `replay_thread`'s 404: there is no id here to protect, and a sidebar
/// that reads "not found" for "signed out" never asks the person to sign in. A bot key is refused
/// like any other non-session bearer; turns taken with one are the minter's and list under them.
/// Origin comes from the caller's own routine rows, so a routine deleted since reads `chat`.
/// The order follows each thread's latest activity, which moves between pages: a client that pages
/// while turns land de-duplicates by `threadId`.
pub async fn list_threads(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    query: Result<Query<ThreadListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return unauthorized("sign in first");
    };
    // Signed out first, malformed second: a bad `?limit=` is no reason to skip the 401.
    let Ok(Query(query)) = query else {
        let json = serde_json::json!({ "error": "limit and before are numbers" });
        return (StatusCode::BAD_REQUEST, Json(json)).into_response();
    };
    // Answering page one would send a pager round the same page forever.
    if query.before.is_none() && query.before_thread_id.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "beforeThreadId needs before" })),
        )
            .into_response();
    }
    let limit = query
        .limit
        .unwrap_or(THREAD_LIST_DEFAULT)
        .clamp(1, THREAD_LIST_MAX);
    let coworker = query.coworker_id.map(CoworkerId::from_stored);
    let before = query
        .before
        .map(|at_ms| (at_ms, query.before_thread_id.as_deref()));
    let threads = match state
        .auth
        .store
        .threads_owned_by(
            &account_id,
            coworker.as_ref(),
            before,
            crate::mcp_door::MCP_AUDIT_THREAD_PREFIX,
            limit,
        )
        .await
    {
        Ok(threads) => threads,
        Err(error) => {
            tracing::warn!(%error, "thread list: the store could not answer");
            let json = serde_json::json!({ "error": "the thread list is unavailable" });
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json)).into_response();
        }
    };
    let rows: Vec<ThreadListRow> = threads
        .into_iter()
        .map(|thread| {
            let named = thread
                .routine
                .as_ref()
                .map(|(_, name)| name.trim())
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            let peers = opengrok_wire::pair::pair_peers(&thread.thread_id);
            ThreadListRow {
                origin: match peers {
                    Some(_) => "pair",
                    None => origin_word(thread.routine.as_ref().map(|(by, _)| *by)),
                },
                peer_bot_ids: peers.map(|(lo, hi)| [lo, hi]),
                title: named.or_else(|| {
                    super::history::title_of(thread.first_prompt.as_deref().unwrap_or_default())
                }),
                thread_id: thread.thread_id,
                coworker_id: thread.coworker_id.map(|id| id.as_str().to_string()),
                last_run_id: thread.last_run_id.as_str().to_string(),
                last_status: thread.last_status,
                updated_at_ms: thread.updated_at_ms,
            }
        })
        .collect();
    // An ARRAY, always: no history is an answer, and a client must be able to paint it.
    Json(rows).into_response()
}

/// Hide a turn from every client of the account that owns it.
///
/// The person deleting a turn in their app is not asking for it to be destroyed. They are asking
/// not to be shown it again — and on the next machine they sign in from, not to be shown it
/// there either. So nothing here is removed: the run keeps its frames, and the coworker keeps
/// its memory of the turn. What changes is that a thread stops offering the run, so no client
/// paints it and the one that hid it does not fetch it back.
///
/// LAYER 4 (`docs/PLAN.md` §4.5): the account comes from the bearer and another account's run is
/// "no such run", the same answer as one that never existed.
pub async fn hide_run(
    State(state): State<AgUiState>,
    headers: axum::http::HeaderMap,
    Path(run_id): Path<String>,
) -> Response {
    let Some(account_id) = account_from_bearer(&state, &headers) else {
        return (StatusCode::NOT_FOUND, "no such run").into_response();
    };
    let id = RunId::from_stored(run_id);
    match state.auth.store.hide_run(&id, &account_id, now_ms()).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "no such run").into_response(),
        Err(error) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}
