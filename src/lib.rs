// SPDX-License-Identifier: GPL-3.0-or-later

//! Porecatu Relay: a self-hosted, blind relay between the Porecatu desktop
//! (the host) and the Porecatu Remote app (the client).
//!
//! The relay implements layers 1 and 2 of the Porecatu protocol (WebSocket
//! over TLS, and the CBOR registration and routing envelope) and never sees
//! anything above them. This crate is only a skeleton for now.

/// Version segment of the envelope paths (`/v0/host`, `/v0/client/{host_id}`).
///
/// Provisional: it is defined by section 1 of the protocol specification
/// (<https://github.com/porecatu/protocol/blob/main/docs/especificacao/01-transporte-e-relay.md>)
/// and will come from the `porecatu-protocol` crate once that dependency lands.
pub const ENVELOPE_PATH_VERSION: &str = "v0";

pub mod cli;
pub mod client;
pub mod config;
pub mod host;
pub mod limits;
pub mod logging;
pub mod metrics;
pub mod payload;
pub mod peer;
pub mod registry;
pub mod server;
pub mod shutdown;
pub mod tls;
pub mod token;
pub mod wire;
