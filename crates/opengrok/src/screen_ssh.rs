//! The SSH door to one Bot's screen (#386, `opengrok-screen-ssh`), started beside the server on
//! the same address as `OG_BIND`. `OG_SSH_PORT` picks its port (2222 when unset; `off` leaves it
//! shut), and `OG_SSH_HOST_KEY` where its host key is kept (made the first time, 0600). A port
//! already taken leaves it off with a warning, and the Computer pane's route says it is off rather
//! than handing out a command for a door that is not there.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use opengrok_screen_ssh::gateway;
use opengrok_server::agui::AgUiState;
use opengrok_server::agui::screen_proxy::SshScreens;

const DEFAULT_PORT: u16 = 2222;

pub async fn start(bind: SocketAddr, state: AgUiState) {
    let port = match std::env::var("OG_SSH_PORT").as_deref().map(str::trim) {
        Ok("off" | "0") => return,
        Ok(word) => match word.parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                tracing::warn!(word, "OG_SSH_PORT is not a port; SSH to a screen is off");
                return;
            }
        },
        Err(_) => DEFAULT_PORT,
    };
    let key_path = std::env::var_os("OG_SSH_HOST_KEY")
        .map(PathBuf::from)
        .or_else(|| {
            let home = std::env::var_os("HOME")?;
            Some(PathBuf::from(home).join(".config/opengrok/ssh_host_ed25519_key"))
        });
    let Some(key_path) = key_path else {
        tracing::warn!("no OG_SSH_HOST_KEY and no HOME; SSH to a screen is off");
        return;
    };
    let key = match gateway::host_key(&key_path) {
        Ok(key) => key,
        Err(error) => {
            tracing::warn!(%error, path = %key_path.display(), "SSH to a screen is off: no host key");
            return;
        }
    };
    let listener = match tokio::net::TcpListener::bind((bind.ip(), port)).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%error, port, "SSH to a screen is off: its port could not be bound");
            return;
        }
    };
    opengrok_screen_ssh::set_listening(port);
    tracing::info!(port, "SSH to a screen: listening");
    let (codes, screens) = (opengrok_screen_ssh::codes(), Arc::new(SshScreens(state)));
    tokio::spawn(async move {
        if let Err(error) = gateway::serve(listener, key, codes, screens).await {
            tracing::warn!(%error, "SSH to a screen stopped");
        }
    });
}
