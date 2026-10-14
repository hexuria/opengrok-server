#![cfg(unix)]

use std::net::TcpListener;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use tokio::process::Command;

#[tokio::test]
async fn sigterm_ends_the_account_event_stream_and_process() -> anyhow::Result<()> {
    let database_url = match std::env::var("OG_DATABASE_URL") {
        Ok(url) => opengrok_store::gate_database_or_panic(url),
        Err(_) => {
            eprintln!("skipping: OG_DATABASE_URL is not set");
            return Ok(());
        }
    };

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let bind = listener.local_addr()?;
    drop(listener);

    let token_secret = uuid::Uuid::new_v4().to_string();
    let email = format!("shutdown-{}@example.test", uuid::Uuid::new_v4());
    let mut child = Command::new(env!("CARGO_BIN_EXE_opengrok"))
        .env("OG_DATABASE_URL", database_url)
        .env("OG_BIND", bind.to_string())
        .env("OG_TOKEN_SECRET", token_secret)
        .env("OG_DEV_SIGN_IN", "1")
        .env("OG_MODEL_DOOR", "mock")
        .env("OG_COMPUTER", "none")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let base = format!("http://{bind}");
    let client = reqwest::Client::new();
    let ready_by = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("server exited before becoming ready: {status}");
        }
        if let Ok(Ok(response)) = tokio::time::timeout(
            Duration::from_secs(1),
            client.get(format!("{base}/health")).send(),
        )
        .await
            && response.status().is_success()
        {
            break;
        }
        ensure!(
            Instant::now() < ready_by,
            "server did not become ready within 30 seconds"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let reply = client
        .get(format!("{base}/auth/cursor_dev_session_token"))
        .query(&[("email", email.as_str())])
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    let token = reply["accessToken"]
        .as_str()
        .context("dev sign-in reply has no accessToken")?;
    let mut events = client
        .get(format!("{base}/ag-ui/events"))
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;

    let pid = child.id().context("server process has no id")?.to_string();
    let signal = std::process::Command::new("kill")
        .args(["-TERM", pid.as_str()])
        .status()
        .context("could not send SIGTERM to the server")?;
    ensure!(signal.success(), "kill returned {signal}");

    let finished = tokio::time::timeout(Duration::from_secs(8), async {
        while events.chunk().await?.is_some() {}
        let status = child.wait().await?;
        ensure!(status.success(), "server exited unsuccessfully: {status}");
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("stream and process did not finish within eight seconds")?;
    finished?;
    Ok(())
}
