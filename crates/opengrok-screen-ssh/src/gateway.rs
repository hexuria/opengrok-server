//! The SSH door itself: a code as the username, `none` as the method, and one forward,
//! `-L <local port>:screen:5900`, carried to the Bot's screen through the box's viewer. No shell,
//! no command, no other forward: a code is a key to one screen, not to the server.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use opengrok_box::viewer::{Dialled, open_socket};
use russh::keys::PrivateKey;
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::{Ed25519Keypair, KeypairData};
use russh::server::{Auth, ChannelOpenHandle, Config, Handler, Msg, Server, Session};
use russh::{Channel, ChannelOpenFailure, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::codes::{Codes, Grant};
use crate::ws;

/// The only forward a code opens, as the person's command names it: `-L 5902:screen:5900`.
pub const SCREEN_HOST: &str = "screen";
pub const SCREEN_PORT: u32 = 5900;

/// The server's side of a code: where its screen is now.
#[async_trait::async_trait]
pub trait Screens: Send + Sync + 'static {
    /// The loopback port and websocket path of the screen `grant` reaches, checked again now:
    /// `None` when its account may no longer see that Bot, or the Bot's computer is not the box
    /// the code was minted for, or has no screen to show.
    async fn screen_for(&self, grant: &Grant) -> Option<(u16, String)>;
}

/// Serve the door on `listener` until the process ends, with `key` as the host key.
pub async fn serve(
    listener: TcpListener,
    key: PrivateKey,
    codes: Arc<Codes>,
    screens: Arc<dyn Screens>,
) -> std::io::Result<()> {
    let config = Config {
        keys: vec![key],
        methods: MethodSet::from(&[MethodKind::None][..]),
        auth_rejection_time: Duration::from_millis(500),
        auth_rejection_time_initial: Some(Duration::ZERO),
        // A VNC session that idles is still a session; the keepalive notices one that died.
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        server_id: russh::SshId::Standard("SSH-2.0-opengrok-screen".into()),
        ..Config::default()
    };
    let mut doors = Doors { codes, screens };
    doors.run_on_socket(Arc::new(config), &listener).await
}

/// The gateway's host key, read from `path`, or made and written there (0600) the first time, so
/// a person's `known_hosts` keeps trusting it across restarts.
pub fn host_key(path: &Path) -> std::io::Result<PrivateKey> {
    let invalid = |error: russh::keys::ssh_key::Error| std::io::Error::other(error.to_string());
    if path.exists() {
        return PrivateKey::read_openssh_file(path).map_err(invalid);
    }
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|error| std::io::Error::other(error.to_string()))?;
    let pair = KeypairData::Ed25519(Ed25519Keypair::from_seed(&seed));
    let key = PrivateKey::new(pair, "opengrok screen gateway").map_err(invalid)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    key.write_openssh_file(path, LineEnding::LF)
        .map_err(invalid)?;
    Ok(key)
}

struct Doors {
    codes: Arc<Codes>,
    screens: Arc<dyn Screens>,
}

impl Server for Doors {
    type Handler = Door;
    fn new_client(&mut self, _peer: Option<std::net::SocketAddr>) -> Door {
        Door {
            codes: self.codes.clone(),
            screens: self.screens.clone(),
            code: None,
        }
    }
}

/// One person's connection: the code it signed in with, asked again at every forward.
struct Door {
    codes: Arc<Codes>,
    screens: Arc<dyn Screens>,
    code: Option<String>,
}

impl Handler for Door {
    type Error = russh::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        if self.codes.redeem(user).is_none() {
            return Ok(Auth::reject());
        }
        self.code = Some(user.to_string());
        Ok(Auth::Accept)
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let grant = self
            .code
            .as_deref()
            .and_then(|code| self.codes.redeem(code));
        let screen = match grant {
            Some(grant) if (host_to_connect, port_to_connect) == (SCREEN_HOST, SCREEN_PORT) => {
                self.screens.screen_for(&grant).await
            }
            _ => None,
        };
        match screen {
            Some((port, path)) => {
                reply.accept().await;
                tokio::spawn(bridge(channel, port, path));
            }
            None => {
                let why = ChannelOpenFailure::AdministrativelyProhibited;
                reply.reject(why).await;
            }
        }
        Ok(())
    }
}

/// Carry a VNC session between the person's forward and the box's viewer until either ends: raw
/// bytes one side, websocket frames the other.
async fn bridge(channel: Channel<Msg>, port: u16, path: String) {
    let Some(key) = websocket_key() else {
        return;
    };
    let Dialled::Open(open) = open_socket(port, &path, &key, Some("binary")).await else {
        tracing::warn!("a screen over SSH: the box's viewer did not take the connection");
        return;
    };
    let (mut from_box, to_box) = open.upstream.into_split();
    let to_box = Arc::new(Mutex::new(to_box));
    let (mut from_person, mut to_person) = tokio::io::split(channel.into_stream());
    let downstream = {
        let to_box = to_box.clone();
        async move {
            let mut reader = ws::Reader::default();
            reader.push(&open.early);
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                loop {
                    match reader.next_frame() {
                        Ok(Some(ws::Heard::Data(bytes))) => {
                            if to_person.write_all(&bytes).await.is_err() {
                                return;
                            }
                        }
                        Ok(Some(ws::Heard::Ping(said))) => {
                            let Some(mask) = mask() else { return };
                            let _ = to_box.lock().await.write_all(&ws::pong(&said, mask)).await;
                        }
                        Ok(Some(ws::Heard::Nothing)) => {}
                        Ok(None) => break,
                        Ok(Some(ws::Heard::Close)) | Err(_) => return,
                    }
                }
                match from_box.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => reader.push(&chunk[..read]),
                }
            }
        }
    };
    let upstream = {
        let to_box = to_box.clone();
        async move {
            let mut chunk = vec![0u8; 32 * 1024];
            loop {
                let read = match from_person.read(&mut chunk).await {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                let Some(mask) = mask() else { return };
                let frame = ws::binary(&chunk[..read], mask);
                if to_box.lock().await.write_all(&frame).await.is_err() {
                    return;
                }
            }
        }
    };
    tokio::select! {
        () = downstream => {}
        () = upstream => {}
    }
    if let Some(mask) = mask() {
        let _ = to_box.lock().await.write_all(&ws::close(mask)).await;
    }
}

/// A frame's mask: random each time, as RFC 6455 asks of a client.
fn mask() -> Option<[u8; 4]> {
    let mut mask = [0u8; 4];
    getrandom::getrandom(&mut mask).ok()?;
    Some(mask)
}

/// The handshake's `Sec-WebSocket-Key`: 16 random bytes, base64.
fn websocket_key() -> Option<String> {
    use base64::Engine as _;
    let mut key = [0u8; 16];
    getrandom::getrandom(&mut key).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(key))
}
