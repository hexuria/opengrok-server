//! The Mac relay's stream (`GET /inference-relay/requests`, #292): what this server sends a
//! person's own Mac, one JSON object per SSE `data:` line, so the Mac can carry a turn to the
//! opencodex it runs. The Mac answers each on `POST /inference-relay/responses/{requestId}`.
//!
//! Provenance: the contract agreed with NativeChat on opengrok-server#292 (FINAL there), which
//! names every frame and field below. `ready` comes first on every connect; a second stream from
//! the same machine replaces the first, which is sent `replaced` and closed. `disabled` and the
//! 409 a switched-off machine's stream is refused with are the per-computer relay contract
//! NativeChat confirmed on 3 Oct 2026, after #332.
//!
//! NO URL AND NO KEY EVER GO DOWN THIS STREAM, and no variant has anywhere to put one. The Mac
//! calls its own opencodex at an address and with a key entered ON THE MAC. The account's stored
//! `baseUrl` and `apiKey` belong to the loopback door alone: sent here they would hand a
//! credential to a machine that never needed it, and name an address that machine cannot reach.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RelayFrame {
    /// First on every connect: the machine this stream is held for.
    Ready { machine_id: String },
    /// A turn's model call: `request` is the chat/completions body the loopback door would POST,
    /// `stream: true` included. The answer is opencodex's SSE, verbatim.
    Infer {
        request_id: String,
        run_id: String,
        model: String,
        request: Value,
    },
    /// The model list: the answer is opencodex's `/v1/models` JSON.
    Models { request_id: String },
    /// Stop working on this request: the run stopped, gave up waiting, or cannot take more.
    Cancel { request_id: String },
    /// Every 15 s, so neither end nor a proxy between them takes a quiet stream for a dead one.
    Ping,
    /// Another stream from this machine took over; this one ends now.
    Replaced,
    /// This machine's relay was switched off (`PATCH /local-exec/daemon/{machineId}`, or the
    /// account's `relayEnabled: false`, which switches every machine): this stream ends now, and
    /// the next is refused with `RELAY_IS_OFF` until the switch is on again.
    Disabled,
}

/// What `GET /inference-relay/requests` answers a machine whose relay is switched off, 409 with
/// `code: "relay_disabled"`, before any frame, in the contract's words.
pub const RELAY_IS_OFF: &str = "Relay is off for this computer. Turn it on in Settings → Computer.";

impl RelayFrame {
    /// The JSON one SSE `data:` line carries: on one line, which an SSE field must be.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}
