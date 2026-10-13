//! The one-time codes a person signs in to the gateway with: the SSH username, minted by the app's
//! "Copy SSH command" for one Bot's screen, and good for `CODE_LIFETIME`. Kept in this process
//! only, so a server restart ends every code, which is the safe way round.
//!
//! A CODE IS A KEY, NOT A NAME. It is 128 random bits, compared whole. It reaches one Bot's screen
//! of one account and nothing else, and it still has to pass the server's check on each use
//! (`Screens::screen_for`): the right to that Bot, and the computer it is on now.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a code lets a person in: long enough to paste it and open a VNC app, short enough
/// that one copied into a chat or a log is soon worthless. A forward already open stays open.
pub const CODE_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Whose screen a code reaches: one account's right to one Bot, on the box it had when the code
/// was minted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub account: String,
    pub coworker: String,
    pub box_id: String,
}

#[derive(Debug, Default)]
pub struct Codes {
    live: Mutex<HashMap<String, (Grant, Instant)>>,
}

impl Codes {
    /// A new code for `grant`, good until `CODE_LIFETIME` from now: `og_` and 26 letters and
    /// digits, which an SSH username takes as it is. `None` only when the system has no randomness
    /// to give, when no code is better than a guessable one.
    pub fn mint(&self, grant: Grant) -> Option<String> {
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes).ok()?;
        let code = format!("og_{}", base32(&bytes));
        let until = Instant::now() + CODE_LIFETIME;
        let mut live = self.live.lock().ok()?;
        live.retain(|_, (_, until)| *until > Instant::now());
        live.insert(code.clone(), (grant, until));
        Some(code)
    }

    /// The grant behind `code` while it is good. Not used up: a VNC app reconnects, and each
    /// connection and forward asks again.
    pub fn redeem(&self, code: &str) -> Option<Grant> {
        let live = self.live.lock().ok()?;
        let (grant, until) = live.get(code)?;
        (*until > Instant::now()).then(|| grant.clone())
    }

    #[cfg(test)]
    fn expire(&self, code: &str) {
        if let Ok(mut live) = self.live.lock()
            && let Some((_, until)) = live.get_mut(code)
        {
            *until = Instant::now();
        }
    }
}

/// Lowercase RFC 4648 base32, no padding: letters and digits only, so the code needs no quoting
/// in a shell or an SSH username.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buffer >> bits) & 31) as usize]));
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buffer << (5 - bits)) & 31) as usize]));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/unit/codes.rs"]
mod tests;
