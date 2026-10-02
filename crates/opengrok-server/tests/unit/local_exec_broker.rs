// Test-only: a failed `expect` here IS the assertion; the workspace-wide deny targets shipped code.
#![allow(clippy::expect_used, clippy::unwrap_used)]
use super::*;

fn success(stdout: &str) -> ExecOutcome {
    ExecOutcome {
        case: "success".to_string(),
        exit_code: Some(0),
        stdout: stdout.to_string(),
        stderr: String::new(),
        detail: String::new(),
    }
}

#[tokio::test]
async fn dispatch_refuses_when_no_daemon_is_connected() {
    let broker = LocalExecBroker::new();
    let result = broker
        .dispatch("mac_a", "req-1", "req-1", json!({ "shellArgs": {} }))
        .await;
    assert_eq!(result.unwrap_err(), DispatchError::NoDaemon);
}

#[tokio::test]
async fn a_connected_daemon_receives_the_exec_frame_and_the_caller_gets_the_result() {
    let broker = LocalExecBroker::new();
    let mut stream = broker.connect("acct_a", "mac_a").await;

    // First frame down the stream is the welcome.
    let welcome = stream.recv().await.expect("welcome");
    assert_eq!(welcome["kind"], "welcome");

    let rx = broker
        .dispatch(
            "mac_a",
            "req-1",
            "req-1",
            json!({ "shellArgs": { "command": "echo hi" } }),
        )
        .await
        .expect("dispatched");

    let frame = stream.recv().await.expect("exec frame");
    assert_eq!(frame["kind"], "exec");
    assert_eq!(frame["requestId"], "req-1");
    assert_eq!(frame["serverMessage"]["shellArgs"]["command"], "echo hi");

    broker.resolve("mac_a", "req-1", success("hi\n")).await;
    let outcome = rx.await.expect("result");
    assert!(outcome.succeeded());
    assert_eq!(outcome.stdout, "hi\n");
}

#[tokio::test]
async fn a_reconnect_replaces_the_old_stream() {
    let broker = LocalExecBroker::new();
    let mut first = broker.connect("acct_a", "mac_a").await;
    let _ = first.recv().await; // welcome
    let mut second = broker.connect("acct_a", "mac_a").await;
    let _ = second.recv().await; // welcome

    broker
        .dispatch("mac_a", "req-1", "req-1", json!({ "shellArgs": {} }))
        .await
        .expect("dispatched to the live stream");
    // The exec frame goes to the NEW stream, not the retired one.
    let frame = second.recv().await.expect("exec on second");
    assert_eq!(frame["requestId"], "req-1");
}

/// A retired token's stream is ended with a `null` its route stops at (#299), and only by its own
/// account: a revoke names a machine id the client chose, which another account may share.
#[tokio::test]
async fn disconnect_ends_the_stream_only_for_the_account_that_opened_it() {
    let broker = LocalExecBroker::new();
    let mut stream = broker.connect("acct_a", "mac_a").await;
    let _ = stream.recv().await; // welcome

    broker.disconnect("acct_b", "mac_a").await;
    let shell = json!({ "shellArgs": {} });
    let held = broker.dispatch("mac_a", "req-1", "req-1", shell.clone());
    held.await.expect("another account's revoke leaves it");
    assert_eq!(stream.recv().await.expect("exec")["kind"], "exec");

    broker.disconnect("acct_a", "mac_a").await;
    assert_eq!(stream.recv().await, Some(Value::Null));
    assert_eq!(stream.recv().await, None, "nothing is sent after the end");
    let gone = broker.dispatch("mac_a", "req-2", "req-2", shell).await;
    assert_eq!(gone.unwrap_err(), DispatchError::NoDaemon);
}

#[tokio::test]
async fn an_unknown_result_id_is_ignored() {
    let broker = LocalExecBroker::new();
    // No waiter registered — resolving must not panic.
    broker.resolve("mac_a", "nope", success("")).await;
}

#[test]
fn render_shows_exit_and_streams_for_a_run_and_the_case_for_a_refusal() {
    assert!(success("hi").render().contains("exit 0"));
    let denied = ExecOutcome {
        case: "permissionDenied".to_string(),
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        detail: "not permitted".to_string(),
    };
    assert_eq!(denied.render(), "permissionDenied: not permitted");
}

#[tokio::test]
async fn a_streaming_shell_accumulates_chunks_and_resolves_on_exit() {
    let broker = LocalExecBroker::new();
    let mut stream = broker.connect("acct_a", "mac_a").await;
    let _ = stream.recv().await; // welcome
    let rx = broker
        .dispatch(
            "mac_a",
            "req-1",
            "req-1",
            json!({ "shellStreamArgs": { "command": "uname" } }),
        )
        .await
        .expect("dispatched");
    let _ = stream.recv().await; // exec frame

    broker.accumulate("mac_a", "req-1", false, "Dar").await;
    broker.accumulate("mac_a", "req-1", false, "win\n").await;
    broker.accumulate("mac_a", "req-1", true, "").await;
    broker
        .finish_stream("mac_a", "req-1", "success", Some(0), "")
        .await;

    let outcome = rx.await.expect("result");
    assert!(outcome.succeeded());
    assert_eq!(outcome.stdout, "Darwin\n");
}

#[tokio::test]
async fn a_stream_chunk_from_the_wrong_machine_is_dropped() {
    let broker = LocalExecBroker::new();
    let mut a = broker.connect("acct_a", "mac_a").await;
    let _ = a.recv().await;
    let rx = broker
        .dispatch("mac_a", "req-1", "req-1", json!({ "shellStreamArgs": {} }))
        .await
        .expect("dispatched");
    let _ = a.recv().await;
    // A different machine cannot feed or finish this request.
    broker.accumulate("mac_evil", "req-1", false, "pwned").await;
    broker
        .finish_stream("mac_evil", "req-1", "success", Some(0), "")
        .await;
    // The real machine finishes it; the evil chunk never landed.
    broker
        .finish_stream("mac_a", "req-1", "success", Some(0), "")
        .await;
    let outcome = rx.await.expect("result");
    assert_eq!(outcome.stdout, "");
}
