//! The transcribed proto surface — seam B, in our own words.
//!
//! The desktop client talks to a second backend (`GrokBotService`, `DashboardService`,
//! `InferenceService`) over **ConnectRPC on HTTP/1.1**, not classic gRPC:
//! `opengrok/source/shared/node/cursor-backend/cursor-inference.ts:157` builds
//! `createConnectTransport({ httpVersion: "1.1" })`. A bare tonic server speaks gRPC
//! (HTTP/2 + trailers) and that client would never reach it — so the Connect routes are served
//! from our existing Axum listener (Connect unary is POST + JSON/binary over plain HTTP), with
//! tonic reserved for internal gRPC if we ever want it. This is why this crate holds prost
//! message types and NOT a tonic service.
//!
//! THE LEGAL LINE APPLIES HERE HARDER THAN ANYWHERE ELSE (docs/LEGAL.md). The client tree
//! carries 157 of Anysphere's recovered generated stubs; none are vendored, none will be.
//! Every message here is hand-transcribed from what the client observably sends and expects,
//! carries a provenance comment naming the file it was read from, and exists only because the
//! client calls it. "Match the whole proto so it's 100% compatible" is the trap, not the goal.

/// The transcribed seam-B surface, generated from `proto/opengrok_seamb.proto` at build time.
///
/// `aiserver.v1` because that is the package the wire speaks; the transcription's provenance is
/// the proto file's own header.
pub mod aiserver {
    pub mod v1 {
        #![allow(clippy::all)]
        tonic::include_proto!("aiserver.v1");
    }
}
