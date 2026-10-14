//! The serve loop and every long stream must observe the same process-wide shutdown moment.

use std::sync::LazyLock;

use futures::{Stream, StreamExt};
use tokio::sync::watch;

static SHUTDOWN: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::channel(false).0);

pub fn begin() {
    SHUTDOWN.send_replace(true);
}

pub async fn begun() {
    let mut receiver = SHUTDOWN.subscribe();
    let _ = receiver.wait_for(|begun| *begun).await;
}

pub fn until_shutdown<S: Stream>(stream: S) -> impl Stream<Item = S::Item> {
    stream.take_until(begun())
}
