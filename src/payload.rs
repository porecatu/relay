// SPDX-License-Identifier: GPL-3.0-or-later

//! The opaque `payload` carried by the relay (ADR-0002).
//!
//! The relay is blind: it routes `payload` bytes between host and clients and
//! never looks inside them. The type that carries them deliberately offers no
//! way to print or serialize its content, so logging a payload is a compile
//! error rather than a code-review item. The only operations are measuring it
//! and handing the bytes to the writer that forwards them.
//!
//! The type wraps the `Payload` of the `porecatu-protocol` crate, which the
//! envelope decoder returns as a zero-copy slice of the received frame.
//!
//! The type does not implement `Debug`, `Display`, `Serialize` or `Clone`.
//! `Clone` is left out on purpose: the relay forwards a frame to exactly one
//! recipient, so a copy is never needed, and without `Clone` an accidental
//! second copy (a retained "for debugging" one, say) does not compile either
//! (the protocol's own `Payload` is `Clone`, which is why it is wrapped and
//! not re-exported).
//!
//! Each of these fails to compile:
//!
//! ```compile_fail
//! use porecatu_relay::payload::OpaquePayload;
//!
//! let payload = OpaquePayload::from_bytes(bytes::Bytes::from_static(b"secret"));
//! let _ = format!("{:?}", payload);
//! ```
//!
//! ```compile_fail
//! use porecatu_relay::payload::OpaquePayload;
//!
//! let payload = OpaquePayload::from_bytes(bytes::Bytes::from_static(b"secret"));
//! let _ = format!("{}", payload);
//! ```
//!
//! ```compile_fail
//! use porecatu_relay::payload::OpaquePayload;
//!
//! let payload = OpaquePayload::from_bytes(bytes::Bytes::from_static(b"secret"));
//! tracing::info!(?payload, "forwarding");
//! ```
//!
//! ```compile_fail
//! use porecatu_relay::payload::OpaquePayload;
//!
//! fn assert_serialize<T: serde::Serialize>(_: &T) {}
//!
//! let payload = OpaquePayload::from_bytes(bytes::Bytes::from_static(b"secret"));
//! assert_serialize(&payload);
//! ```
//!
//! ```compile_fail
//! use porecatu_relay::payload::OpaquePayload;
//!
//! let payload = OpaquePayload::from_bytes(bytes::Bytes::from_static(b"secret"));
//! let _copy = payload.clone();
//! ```
//!
//! The setup those examples share does compile, so a failure above is about
//! the formatting and not about a typo:
//!
//! ```
//! use porecatu_relay::payload::OpaquePayload;
//!
//! let payload = OpaquePayload::from_bytes(bytes::Bytes::from_static(b"secret"));
//! assert_eq!(payload.len(), 6);
//! tracing::info!(bytes_in = payload.len(), "forwarding");
//! ```

use bytes::Bytes;
use porecatu_protocol::envelope::Payload;

/// Opaque bytes of a `payload`, to be forwarded and measured, never read.
pub struct OpaquePayload(Payload);

impl OpaquePayload {
    /// Wraps the bytes of a `payload`.
    #[must_use]
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self(Payload::from_bytes(bytes))
    }

    /// Wraps the `payload` of a decoded envelope.
    #[must_use]
    pub fn from_protocol(payload: Payload) -> Self {
        Self(payload)
    }

    /// Gives the protocol `payload` back, to build the outgoing envelope.
    #[must_use]
    pub fn into_protocol(self) -> Payload {
        self.0
    }

    /// Length of the `payload`, in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the `payload` has no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Gives the bytes back, to hand them to the writer that forwards them.
    #[must_use]
    pub fn into_bytes(self) -> Bytes {
        self.0.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_and_returns_the_bytes_untouched() {
        let payload = OpaquePayload::from_bytes(Bytes::from_static(b"abc"));
        assert_eq!(payload.len(), 3);
        assert!(!payload.is_empty());
        assert_eq!(payload.into_bytes(), Bytes::from_static(b"abc"));
    }

    #[test]
    fn empty_payload_is_empty() {
        let payload = OpaquePayload::from_bytes(Bytes::new());
        assert_eq!(payload.len(), 0);
        assert!(payload.is_empty());
    }
}
