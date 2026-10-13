//! One Bot's screen over SSH (hexuria/opengrok-server#386, part 2): a person on another machine,
//! or with a VNC app of their own, reaches exactly one Bot's screen with plain `ssh -L`, like a
//! sandbox provider's tunnel, and nothing else on this server.
//!
//! The app asks for a code (`POST /coworkers/{id}/computer/ssh`, in opengrok-server); the code is
//! the SSH username, good for fifteen minutes (`codes`). The gateway (`gateway`) takes no password
//! and no key: the code is the credential, checked again at every forward against the person's
//! right to that Bot and the computer it is on now, the way the screen proxy checks its tickets.
//! The one forward it opens is carried to the box's viewer over loopback (`ws`), so no box port is
//! ever published wider than `127.0.0.1`. The VNC password stays required, in the VNC app.

pub mod codes;
pub mod gateway;
pub mod ws;

use std::sync::{Arc, LazyLock, OnceLock};

pub use codes::{CODE_LIFETIME, Codes, Grant};
pub use gateway::{SCREEN_HOST, SCREEN_PORT, Screens};

/// The process's codes: minted by the route, redeemed by the gateway.
pub fn codes() -> Arc<Codes> {
    static CODES: LazyLock<Arc<Codes>> = LazyLock::new(Arc::default);
    CODES.clone()
}

/// What the app is handed for a new code (`POST /coworkers/{id}/computer/ssh`): the command, with
/// the gateway's port and the host the app reached the server on (`origin`), its parts, the
/// screen's VNC password read from its page (`page`, the box's noVNC address), and when the code
/// stops working. `None` while the gateway is off, or when no code can be made.
pub fn offer(grant: Grant, origin: &str, page: &str, now_ms: i64) -> Option<serde_json::Value> {
    let port = listening()?;
    let code = codes().mint(grant)?;
    let host = origin.split_once("://").map_or(origin, |(_, rest)| rest);
    let host = match host.rsplit_once(':') {
        Some((name, port)) if port.parse::<u16>().is_ok() => name,
        _ => host,
    };
    let password = page
        .split(['?', '&'])
        .find_map(|pair| pair.strip_prefix("password="));
    let until = now_ms + i64::try_from(CODE_LIFETIME.as_millis()).unwrap_or(i64::MAX);
    Some(serde_json::json!({
        "command": format!("ssh -N -p {port} -L {LOCAL_PORT}:{SCREEN_HOST}:{SCREEN_PORT} {code}@{host}"),
        "user": code,
        "host": host,
        "port": port,
        "localPort": LOCAL_PORT,
        "target": format!("{SCREEN_HOST}:{SCREEN_PORT}"),
        "vncPassword": password,
        "expiresAtMs": until,
    }))
}

/// The route's answers when there is no code to give: the gateway is off (`OG_SSH_PORT=off`, or
/// its port was taken), or the Bot has no screen this server can reach (a hosted computer, or one
/// not started).
pub const OFF: &str = "SSH to a screen is off on this server";
pub const NO_SCREEN: &str =
    "this Bot's screen is not up on this server's own Docker; open it, then ask again";

/// The port the command forwards on the person's machine, where their VNC app connects: 5902, out
/// of the way of a VNC server of their own on 5900 or 5901.
pub const LOCAL_PORT: u16 = 5902;

/// Where the gateway listens, once it does: its port, for the command the app shows. Unset while
/// it is off or could not bind, and then the route says so instead of handing out a dead command.
pub fn listening() -> Option<u16> {
    LISTENING.get().copied()
}

pub fn set_listening(port: u16) {
    let _ = LISTENING.set(port);
}

static LISTENING: OnceLock<u16> = OnceLock::new();
