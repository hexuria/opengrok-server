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
        .dispatch(
            "acct_a",
            "mac_a",
            "req-1",
            "req-1",
            json!({ "shellArgs": {} }),
        )
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
            "acct_a",
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

    broker
        .resolve("acct_a", "mac_a", "req-1", success("hi\n"))
        .await;
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
        .dispatch(
            "acct_a",
            "mac_a",
            "req-1",
            "req-1",
            json!({ "shellArgs": {} }),
        )
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
    let held = broker.dispatch("acct_a", "mac_a", "req-1", "req-1", shell.clone());
    held.await.expect("another account's revoke leaves it");
    assert_eq!(stream.recv().await.expect("exec")["kind"], "exec");

    broker.disconnect("acct_a", "mac_a").await;
    assert_eq!(stream.recv().await, Some(Value::Null));
    assert_eq!(stream.recv().await, None, "nothing is sent after the end");
    let gone = broker
        .dispatch("acct_a", "mac_a", "req-2", "req-2", shell)
        .await;
    assert_eq!(gone.unwrap_err(), DispatchError::NoDaemon);
}

#[tokio::test]
async fn a_shared_machine_id_keeps_accounts_commands_and_streams_separate() {
    let broker = LocalExecBroker::new();
    let mut a = broker.connect("acct_a", "shared_mac").await;
    let mut b = broker.connect("acct_b", "shared_mac").await;
    let _ = a.recv().await;
    let _ = b.recv().await;
    assert!(broker.has_provider("acct_a", "shared_mac").await);
    assert!(broker.has_provider("acct_b", "shared_mac").await);

    let a_result = broker
        .dispatch(
            "acct_a",
            "shared_mac",
            "req_a",
            "req_a",
            json!({ "account": "a" }),
        )
        .await
        .expect("account A's daemon is connected");
    let b_result = broker
        .dispatch(
            "acct_b",
            "shared_mac",
            "req_b",
            "req_b",
            json!({ "account": "b" }),
        )
        .await
        .expect("account B's daemon is connected");
    assert_eq!(
        a.recv().await.expect("A's command")["serverMessage"]["account"],
        "a"
    );
    assert_eq!(
        b.recv().await.expect("B's command")["serverMessage"]["account"],
        "b"
    );

    broker
        .resolve("acct_b", "shared_mac", "req_a", success("cross-account"))
        .await;
    broker
        .resolve("acct_a", "shared_mac", "req_a", success("account-a"))
        .await;
    assert_eq!(
        a_result.await.expect("A's result").stdout,
        "account-a",
        "B cannot resolve A's command"
    );
    broker
        .resolve("acct_a", "shared_mac", "req_b", success("cross-account"))
        .await;
    broker
        .resolve("acct_b", "shared_mac", "req_b", success("account-b"))
        .await;
    assert_eq!(
        b_result.await.expect("B's result").stdout,
        "account-b",
        "A cannot resolve B's command"
    );

    let streamed = broker
        .dispatch(
            "acct_b",
            "shared_mac",
            "req_stream",
            "req_stream",
            json!({ "shellStreamArgs": {} }),
        )
        .await
        .expect("B's stream command");
    let _ = b.recv().await.expect("B's stream command frame");
    broker
        .accumulate("acct_a", "shared_mac", "req_stream", false, "wrong-account")
        .await;
    broker
        .finish_stream("acct_a", "shared_mac", "req_stream", "success", Some(0), "")
        .await;
    broker
        .accumulate("acct_b", "shared_mac", "req_stream", false, "account-b")
        .await;
    broker
        .finish_stream("acct_b", "shared_mac", "req_stream", "success", Some(0), "")
        .await;
    assert_eq!(
        streamed.await.expect("B's streamed result").stdout,
        "account-b",
        "A cannot write or finish B's stream"
    );

    broker.disconnect("acct_a", "shared_mac").await;
    assert_eq!(a.recv().await, Some(Value::Null));
    assert!(broker.has_provider("acct_b", "shared_mac").await);
    assert!(!broker.has_provider("acct_a", "shared_mac").await);
    let still_connected = broker
        .dispatch(
            "acct_b",
            "shared_mac",
            "req_after_disconnect",
            "req_after_disconnect",
            json!({}),
        )
        .await
        .expect("A's disconnect leaves B connected");
    assert_eq!(b.recv().await.expect("B remains open")["kind"], "exec");
    broker
        .resolve(
            "acct_b",
            "shared_mac",
            "req_after_disconnect",
            success("still connected"),
        )
        .await;
    assert_eq!(
        still_connected.await.expect("B's last result").stdout,
        "still connected"
    );
}

#[tokio::test]
async fn an_unknown_result_id_is_ignored() {
    let broker = LocalExecBroker::new();
    // No waiter registered — resolving must not panic.
    broker.resolve("acct_a", "mac_a", "nope", success("")).await;
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
            "acct_a",
            "mac_a",
            "req-1",
            "req-1",
            json!({ "shellStreamArgs": { "command": "uname" } }),
        )
        .await
        .expect("dispatched");
    let _ = stream.recv().await; // exec frame

    broker
        .accumulate("acct_a", "mac_a", "req-1", false, "Dar")
        .await;
    broker
        .accumulate("acct_a", "mac_a", "req-1", false, "win\n")
        .await;
    broker
        .accumulate("acct_a", "mac_a", "req-1", true, "")
        .await;
    broker
        .finish_stream("acct_a", "mac_a", "req-1", "success", Some(0), "")
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
        .dispatch(
            "acct_a",
            "mac_a",
            "req-1",
            "req-1",
            json!({ "shellStreamArgs": {} }),
        )
        .await
        .expect("dispatched");
    let _ = a.recv().await;
    // A different machine cannot feed or finish this request.
    broker
        .accumulate("acct_a", "mac_evil", "req-1", false, "pwned")
        .await;
    broker
        .finish_stream("acct_a", "mac_evil", "req-1", "success", Some(0), "")
        .await;
    // The real machine finishes it; the evil chunk never landed.
    broker
        .finish_stream("acct_a", "mac_a", "req-1", "success", Some(0), "")
        .await;
    let outcome = rx.await.expect("result");
    assert_eq!(outcome.stdout, "");
}
