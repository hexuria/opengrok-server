//! The door end to end, over real sockets: a person's SSH client signs in with a code, opens the
//! one forward a code allows, and talks to a stand-in for the box's viewer (websockify) through
//! it; and is refused with a wrong code, an expired code, another target, or a screen the server
//! no longer lets them see.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use opengrok_screen_ssh::{Codes, Grant, Screens, gateway, ws};
use russh::client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A box viewer that greets with an RFB version, as x11vnc does, then echoes every frame back.
async fn stand_in_viewer() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if socket.read_exact(&mut byte).await.is_err() {
                        return;
                    }
                    head.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&head);
                assert!(head.starts_with("GET /websockify?token=s2 "), "{head}");
                assert!(head.contains("Sec-WebSocket-Protocol: binary"), "{head}");
                let hello = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
                             Connection: Upgrade\r\nSec-WebSocket-Accept: x\r\n\
                             Sec-WebSocket-Protocol: binary\r\n\r\n";
                socket.write_all(hello.as_bytes()).await.unwrap();
                socket.write_all(&[0x82, 12]).await.unwrap();
                socket.write_all(b"RFB 003.008\n").await.unwrap();
                let mut reader = ws::Reader::default();
                let mut chunk = [0u8; 4096];
                loop {
                    let Ok(read) = socket.read(&mut chunk).await else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    reader.push(&chunk[..read]);
                    while let Ok(Some(heard)) = reader.next_frame() {
                        match heard {
                            ws::Heard::Data(bytes) => {
                                let mut echo = vec![0x82, bytes.len() as u8];
                                echo.extend(bytes);
                                socket.write_all(&echo).await.unwrap();
                            }
                            ws::Heard::Close => return,
                            _ => {}
                        }
                    }
                }
            });
        }
    });
    port
}

/// The server's check, which the test can turn against the code's holder.
struct StandInScreens {
    viewer: u16,
    allowed: AtomicBool,
}

#[async_trait::async_trait]
impl Screens for StandInScreens {
    async fn screen_for(&self, grant: &Grant) -> Option<(u16, String)> {
        (self.allowed.load(Ordering::SeqCst) && grant.coworker == "cw_ada")
            .then(|| (self.viewer, "websockify?token=s2".to_string()))
    }
}

struct Person;

impl client::Handler for Person {
    type Error = russh::Error;
    async fn check_server_key(
        &mut self,
        _key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

struct Door {
    port: u16,
    codes: Arc<Codes>,
    screens: Arc<StandInScreens>,
}

async fn door() -> Door {
    let viewer = stand_in_viewer().await;
    let codes = Arc::new(Codes::default());
    let screens = Arc::new(StandInScreens {
        viewer,
        allowed: AtomicBool::new(true),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("og-ssh-{}-{port}", std::process::id()));
    let key = gateway::host_key(&dir.join("host_key")).unwrap();
    let again = gateway::host_key(&dir.join("host_key")).unwrap();
    assert_eq!(key.public_key(), again.public_key(), "the host key is kept");
    let (served_codes, served_screens) = (codes.clone(), screens.clone());
    tokio::spawn(async move {
        let _ = gateway::serve(listener, key, served_codes, served_screens).await;
    });
    Door {
        port,
        codes,
        screens,
    }
}

impl Door {
    async fn sign_in(&self, user: &str) -> (client::Handle<Person>, bool) {
        let config = Arc::new(client::Config::default());
        let mut session = client::connect(config, ("127.0.0.1", self.port), Person)
            .await
            .expect("the gateway answers");
        let signed_in = session.authenticate_none(user).await.unwrap().success();
        (session, signed_in)
    }

    fn code(&self, coworker: &str) -> String {
        self.codes
            .mint(Grant {
                account: "acc_1".into(),
                coworker: coworker.into(),
                box_id: "og-box-1".into(),
            })
            .unwrap()
    }
}

async fn read_some(stream: &mut (impl AsyncReadExt + Unpin), want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    let mut chunk = [0u8; 256];
    while got.len() < want {
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .expect("the screen answered in time")
            .unwrap();
        assert!(read > 0, "the forward closed early");
        got.extend_from_slice(&chunk[..read]);
    }
    got
}

/// THE WAY IN: the code as the username, nothing else asked, and `-L …:screen:5900` carries the
/// VNC session both ways through the box's viewer, its greeting first.
#[tokio::test]
async fn a_code_opens_one_bots_screen_and_carries_its_vnc_session() {
    let door = door().await;
    let code = door.code("cw_ada");
    let (session, signed_in) = door.sign_in(&code).await;
    assert!(signed_in, "a live code signs in with no password or key");
    let channel = session
        .channel_open_direct_tcpip("screen", 5900, "127.0.0.1", 50000)
        .await
        .expect("the screen forward opens");
    let mut stream = channel.into_stream();
    assert_eq!(read_some(&mut stream, 12).await, b"RFB 003.008\n");
    stream.write_all(b"RFB 003.008\n").await.unwrap();
    assert_eq!(
        read_some(&mut stream, 12).await,
        b"RFB 003.008\n",
        "both ways"
    );
}

/// A wrong code, or one past its fifteen minutes, does not sign in.
#[tokio::test]
async fn a_wrong_code_does_not_sign_in() {
    let door = door().await;
    let (_, signed_in) = door.sign_in("og_aaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    assert!(!signed_in);
    let (_, signed_in) = door.sign_in("root").await;
    assert!(!signed_in);
}

/// A code is a key to one screen, not to the server: no other forward, no shell, and no screen the
/// server says its holder may no longer see (another account's Bot, a Bot retired since, a computer
/// that moved).
#[tokio::test]
async fn a_code_opens_nothing_but_the_screen_it_was_given_for() {
    let door = door().await;
    let code = door.code("cw_ada");
    let (session, signed_in) = door.sign_in(&code).await;
    assert!(signed_in);
    for (host, port) in [("127.0.0.1", 5900), ("screen", 22), ("localhost", 1447)] {
        assert!(
            session
                .channel_open_direct_tcpip(host, port, "127.0.0.1", 50000)
                .await
                .is_err(),
            "{host}:{port} is not the screen"
        );
    }
    assert!(session.channel_open_session().await.is_err(), "no shell");

    let other = door.code("cw_someone_elses");
    let (session, _) = door.sign_in(&other).await;
    assert!(
        session
            .channel_open_direct_tcpip("screen", 5900, "127.0.0.1", 50000)
            .await
            .is_err(),
        "the server's check refuses a screen the code's holder may not see"
    );

    door.screens.allowed.store(false, Ordering::SeqCst);
    let (session, signed_in) = door.sign_in(&code).await;
    assert!(signed_in);
    assert!(
        session
            .channel_open_direct_tcpip("screen", 5900, "127.0.0.1", 50000)
            .await
            .is_err(),
        "checked again at every forward, not only at mint"
    );
}
