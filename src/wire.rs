// SPDX-License-Identifier: GPL-3.0-or-later

//! The wire adapter: decoding by direction, encoding, and the close code of
//! every error (specification 01 §3 to §5).
//!
//! The envelope itself (CBOR, canonical form, sizes) lives in
//! `porecatu_protocol::envelope`; this module only narrows it to what the
//! relay may *receive* on each connection, wraps the `payload` in the opaque
//! type of [`crate::payload`], and maps each failure to the WebSocket close
//! code the specification gives it. No function here touches a socket.
//!
//! Direction matters: the protocol decoder knows every host envelope, but a
//! host may only send `register`, `channel-data` and `close-channel`, and a
//! client may only send `data`. Anything else is a valid envelope in the wrong
//! direction, which is a protocol error all the same ([`WireError::WrongDirection`]).

use core::fmt;

use bytes::Bytes;
use ed25519_dalek::{Signature, VerifyingKey};
use porecatu_protocol::envelope::{
    self, ChannelId, ClientEnvelope, CloseReason, EnvelopeError, HostEnvelope, MAX_FRAME,
    NONCE_LEN, ROUTING_KEY_LEN, SIGNATURE_LEN, close_code,
};
use porecatu_protocol::host_id::{self, HostId};

use crate::payload::OpaquePayload;

/// What the relay accepts from a host connection.
pub enum FromHost {
    /// `register`: the routing key and the signature over the challenge.
    Register {
        /// Ed25519 public routing key.
        routing_key: [u8; ROUTING_KEY_LEN],
        /// Ed25519 signature over `register_message(nonce)`.
        signature: [u8; SIGNATURE_LEN],
    },
    /// `channel-data`: a record for a channel.
    ChannelData {
        /// The channel.
        channel: ChannelId,
        /// The opaque record.
        payload: OpaquePayload,
    },
    /// `close-channel`: the host closes a channel.
    CloseChannel {
        /// The channel.
        channel: ChannelId,
        /// Why.
        reason: CloseReason,
    },
}

/// What the relay accepts from a client connection.
pub enum FromClient {
    /// `data`: a record for the host.
    Data {
        /// The opaque record.
        payload: OpaquePayload,
    },
}

// `Debug` shows the payload length and never its bytes.
impl fmt::Debug for FromHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Register { .. } => f.write_str("Register"),
            Self::ChannelData { channel, payload } => f
                .debug_struct("ChannelData")
                .field("channel", channel)
                .field("payload_len", &payload.len())
                .finish(),
            Self::CloseChannel { channel, reason } => f
                .debug_struct("CloseChannel")
                .field("channel", channel)
                .field("reason", reason)
                .finish(),
        }
    }
}

impl fmt::Debug for FromClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data { payload } => f
                .debug_struct("Data")
                .field("payload_len", &payload.len())
                .finish(),
        }
    }
}

/// Why a frame was refused or could not be forwarded. Never carries payload
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The frame is larger than the maximum (`1009`).
    FrameTooLarge,
    /// The frame is not a valid canonical envelope (`4004`).
    InvalidEnvelope(EnvelopeError),
    /// A valid envelope that this side of the connection never sends (`4004`).
    WrongDirection,
    /// The routing key is not a valid Ed25519 public key (`4001`).
    InvalidRoutingKey,
    /// The registration signature does not verify (`4001`).
    InvalidSignature,
    /// Forwarding the `payload` would make a frame larger than the maximum, a
    /// limit exceeded by the channel (`4003`).
    ForwardTooLarge,
}

impl WireError {
    /// The WebSocket close code the specification gives this error (01 §3,
    /// §4, §5).
    ///
    /// For a client connection the relay sends `closed { reason }` first and
    /// then closes with the code of [`WireError::client_reason`]; the two agree
    /// except for [`WireError::FrameTooLarge`], which closes with `1009` and no
    /// `closed` before (the frame never reached the envelope level).
    #[must_use]
    pub fn close_code(&self) -> u16 {
        match self {
            Self::FrameTooLarge => close_code::MESSAGE_TOO_BIG,
            Self::InvalidEnvelope(_) | Self::WrongDirection => close_code::INVALID_ENVELOPE,
            Self::InvalidRoutingKey | Self::InvalidSignature => close_code::REGISTER_REJECTED,
            Self::ForwardTooLarge => close_code::LIMIT_EXCEEDED,
        }
    }

    /// The `close-reason` a client channel carries when this error ends it:
    /// `limit-exceeded` for a size limit, `protocol-error` for the rest.
    #[must_use]
    pub fn client_reason(&self) -> CloseReason {
        match self {
            Self::ForwardTooLarge | Self::FrameTooLarge => CloseReason::LimitExceeded,
            _ => CloseReason::ProtocolError,
        }
    }
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge => f.write_str("frame larger than the maximum"),
            Self::InvalidEnvelope(e) => write!(f, "{e}"),
            Self::WrongDirection => f.write_str("envelope type not allowed in this direction"),
            Self::InvalidRoutingKey => f.write_str("invalid routing key"),
            Self::InvalidSignature => f.write_str("invalid registration signature"),
            Self::ForwardTooLarge => f.write_str("payload too large to forward"),
        }
    }
}

impl core::error::Error for WireError {}

impl From<EnvelopeError> for WireError {
    fn from(e: EnvelopeError) -> Self {
        match e {
            EnvelopeError::FrameTooLarge => Self::FrameTooLarge,
            other => Self::InvalidEnvelope(other),
        }
    }
}

/// The WebSocket close code after `closed { reason }` on a client connection
/// (01 §4): `4003` for `limit-exceeded`, `4004` for `protocol-error`, `1000`
/// for the others.
#[must_use]
pub fn close_code_for_client(reason: CloseReason) -> u16 {
    reason.client_close_code()
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decodes a frame received on the host connection. Only `register`,
/// `channel-data` and `close-channel` are accepted; every other envelope of
/// the host family is [`WireError::WrongDirection`].
///
/// # Errors
///
/// Any [`WireError`] except the signature, key and forwarding ones.
pub fn decode_from_host(frame: Bytes) -> Result<FromHost, WireError> {
    Ok(match envelope::decode_host(frame)? {
        HostEnvelope::Register {
            routing_key,
            signature,
        } => FromHost::Register {
            routing_key,
            signature,
        },
        HostEnvelope::ChannelData { channel, payload } => FromHost::ChannelData {
            channel,
            payload: OpaquePayload::from_protocol(payload),
        },
        HostEnvelope::CloseChannel { channel, reason } => {
            FromHost::CloseChannel { channel, reason }
        }
        HostEnvelope::Challenge { .. }
        | HostEnvelope::Registered { .. }
        | HostEnvelope::ChannelOpened { .. }
        | HostEnvelope::ChannelClosed { .. } => return Err(WireError::WrongDirection),
    })
}

/// Decodes a frame received on a client connection. Only `data` is accepted;
/// `host-unavailable` and `closed` are [`WireError::WrongDirection`].
///
/// # Errors
///
/// Any [`WireError`] except the signature, key and forwarding ones.
pub fn decode_from_client(frame: Bytes) -> Result<FromClient, WireError> {
    match envelope::decode_client(frame)? {
        ClientEnvelope::Data { payload } => Ok(FromClient::Data {
            payload: OpaquePayload::from_protocol(payload),
        }),
        ClientEnvelope::HostUnavailable | ClientEnvelope::Closed { .. } => {
            Err(WireError::WrongDirection)
        }
    }
}

/// Verifies the registration of a host and derives its address.
///
/// The signature is checked with `verify_strict` over
/// `register_message(nonce)` (context followed by the challenge nonce), which
/// also rejects small-order keys and non-canonical signatures.
///
/// # Errors
///
/// [`WireError::InvalidRoutingKey`] if the bytes are not a public key,
/// [`WireError::InvalidSignature`] if the signature does not verify. Both
/// close with `4001`.
pub fn verify_register(
    nonce: &[u8; NONCE_LEN],
    routing_key: &[u8; ROUTING_KEY_LEN],
    signature: &[u8; SIGNATURE_LEN],
) -> Result<HostId, WireError> {
    let key = VerifyingKey::from_bytes(routing_key).map_err(|_| WireError::InvalidRoutingKey)?;
    let signature = Signature::from_bytes(signature);
    key.verify_strict(&envelope::register_message(nonce), &signature)
        .map_err(|_| WireError::InvalidSignature)?;
    Ok(host_id::host_id(routing_key))
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// `challenge`, relay to host.
#[must_use]
pub fn encode_challenge(nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
    envelope::encode_host(&HostEnvelope::Challenge { nonce: *nonce })
}

/// `registered`, relay to host.
#[must_use]
pub fn encode_registered(host_id: &HostId) -> Vec<u8> {
    envelope::encode_host(&HostEnvelope::Registered {
        host_id: host_id.clone(),
    })
}

/// `channel-opened`, relay to host.
#[must_use]
pub fn encode_channel_opened(channel: ChannelId) -> Vec<u8> {
    envelope::encode_host(&HostEnvelope::ChannelOpened { channel })
}

/// `channel-data`, relay to host. The frame is measured before it is built.
///
/// # Errors
///
/// [`WireError::ForwardTooLarge`] if the frame would exceed the maximum size:
/// the channel exceeded a limit, and the caller closes it with
/// `limit-exceeded` rather than dropping the `payload`.
pub fn encode_channel_data(
    channel: ChannelId,
    payload: OpaquePayload,
) -> Result<Vec<u8>, WireError> {
    if envelope::channel_data_len(channel, payload.len()) > MAX_FRAME {
        return Err(WireError::ForwardTooLarge);
    }
    Ok(envelope::encode_host(&HostEnvelope::ChannelData {
        channel,
        payload: payload.into_protocol(),
    }))
}

/// `channel-closed`, relay to host.
#[must_use]
pub fn encode_channel_closed(channel: ChannelId, reason: CloseReason) -> Vec<u8> {
    envelope::encode_host(&HostEnvelope::ChannelClosed { channel, reason })
}

/// `data`, relay to client. The frame is measured before it is built.
///
/// # Errors
///
/// [`WireError::ForwardTooLarge`] if the frame would exceed the maximum size.
pub fn encode_data(payload: OpaquePayload) -> Result<Vec<u8>, WireError> {
    if envelope::client_data_len(payload.len()) > MAX_FRAME {
        return Err(WireError::ForwardTooLarge);
    }
    Ok(envelope::encode_client(&ClientEnvelope::Data {
        payload: payload.into_protocol(),
    }))
}

/// `host-unavailable`, relay to client.
#[must_use]
pub fn encode_host_unavailable() -> Vec<u8> {
    envelope::encode_client(&ClientEnvelope::HostUnavailable)
}

/// `closed`, relay to client.
#[must_use]
pub fn encode_closed(reason: CloseReason) -> Vec<u8> {
    envelope::encode_client(&ClientEnvelope::Closed { reason })
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};
    use porecatu_protocol::cbor::CborError;

    use super::*;

    fn signing_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn payload(bytes: &'static [u8]) -> OpaquePayload {
        OpaquePayload::from_bytes(Bytes::from_static(bytes))
    }

    fn host_frame(envelope: &HostEnvelope) -> Bytes {
        Bytes::from(envelope::encode_host(envelope))
    }

    fn client_frame(envelope: &ClientEnvelope) -> Bytes {
        Bytes::from(envelope::encode_client(envelope))
    }

    fn proto_payload(bytes: &'static [u8]) -> envelope::Payload {
        envelope::Payload::from_bytes(Bytes::from_static(bytes))
    }

    // ---- direction -------------------------------------------------------

    #[test]
    fn host_may_send_register_channel_data_and_close_channel() {
        let frame = host_frame(&HostEnvelope::Register {
            routing_key: [1; 32],
            signature: [2; 64],
        });
        match decode_from_host(frame) {
            Ok(FromHost::Register {
                routing_key,
                signature,
            }) => {
                assert_eq!(routing_key, [1; 32]);
                assert_eq!(signature, [2; 64]);
            }
            other => panic!("unexpected: {other:?}"),
        }

        let frame = host_frame(&HostEnvelope::ChannelData {
            channel: ChannelId(300),
            payload: proto_payload(b"abc"),
        });
        match decode_from_host(frame) {
            Ok(FromHost::ChannelData { channel, payload }) => {
                assert_eq!(channel, ChannelId(300));
                assert_eq!(payload.into_bytes().as_ref(), b"abc");
            }
            other => panic!("unexpected: {other:?}"),
        }

        let frame = host_frame(&HostEnvelope::CloseChannel {
            channel: ChannelId(7),
            reason: CloseReason::HostRejected,
        });
        match decode_from_host(frame) {
            Ok(FromHost::CloseChannel { channel, reason }) => {
                assert_eq!(channel, ChannelId(7));
                assert_eq!(reason, CloseReason::HostRejected);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn host_envelopes_the_host_never_sends_are_wrong_direction() {
        let host_id = host_id::host_id(&[9; 32]);
        for envelope in [
            HostEnvelope::Challenge { nonce: [0; 32] },
            HostEnvelope::Registered { host_id },
            HostEnvelope::ChannelOpened {
                channel: ChannelId(1),
            },
            HostEnvelope::ChannelClosed {
                channel: ChannelId(1),
                reason: CloseReason::Normal,
            },
        ] {
            let error = decode_from_host(host_frame(&envelope)).unwrap_err();
            assert_eq!(error, WireError::WrongDirection, "{envelope:?}");
            assert_eq!(error.close_code(), 4004);
        }
    }

    #[test]
    fn client_envelopes_on_the_host_connection_are_refused() {
        // `data` (type 16) is not in the host family at all.
        let frame = client_frame(&ClientEnvelope::Data {
            payload: proto_payload(b"x"),
        });
        let error = decode_from_host(frame).unwrap_err();
        assert_eq!(
            error,
            WireError::InvalidEnvelope(EnvelopeError::UnknownType(16))
        );
        assert_eq!(error.close_code(), 4004);
    }

    #[test]
    fn client_may_only_send_data() {
        let frame = client_frame(&ClientEnvelope::Data {
            payload: proto_payload(b"hello"),
        });
        match decode_from_client(frame) {
            Ok(FromClient::Data { payload }) => {
                assert_eq!(payload.into_bytes().as_ref(), b"hello");
            }
            other => panic!("unexpected: {other:?}"),
        }

        for envelope in [
            ClientEnvelope::HostUnavailable,
            ClientEnvelope::Closed {
                reason: CloseReason::Normal,
            },
        ] {
            let error = decode_from_client(client_frame(&envelope)).unwrap_err();
            assert_eq!(error, WireError::WrongDirection, "{envelope:?}");
            assert_eq!(error.close_code(), 4004);
            assert_eq!(error.client_reason(), CloseReason::ProtocolError);
        }
    }

    #[test]
    fn host_envelopes_on_the_client_connection_are_refused() {
        let frame = host_frame(&HostEnvelope::ChannelData {
            channel: ChannelId(0),
            payload: proto_payload(b"x"),
        });
        let error = decode_from_client(frame).unwrap_err();
        assert_eq!(
            error,
            WireError::InvalidEnvelope(EnvelopeError::UnknownType(4))
        );
        assert_eq!(error.close_code(), 4004);
    }

    #[test]
    fn malformed_and_non_canonical_frames_are_invalid_envelopes() {
        // Trailing byte, and a non-minimal integer.
        for frame in [&[0x82, 0x10, 0x40, 0x00][..], &[0x82, 0x18, 0x10, 0x40][..]] {
            let error = decode_from_client(Bytes::copy_from_slice(frame)).unwrap_err();
            assert!(matches!(error, WireError::InvalidEnvelope(_)), "{error:?}");
            assert_eq!(error.close_code(), 4004);
        }
        assert_eq!(
            decode_from_host(Bytes::from_static(&[0x82, 0x03, 0x07, 0x00])).unwrap_err(),
            WireError::InvalidEnvelope(EnvelopeError::Cbor(CborError::TrailingBytes))
        );
        // Empty frame.
        assert!(matches!(
            decode_from_host(Bytes::new()).unwrap_err(),
            WireError::InvalidEnvelope(_)
        ));
    }

    #[test]
    fn oversized_frames_close_with_1009() {
        let over = Bytes::from(vec![0u8; MAX_FRAME + 1]);
        let error = decode_from_host(over.clone()).unwrap_err();
        assert_eq!(error, WireError::FrameTooLarge);
        assert_eq!(error.close_code(), 1009);
        let error = decode_from_client(over).unwrap_err();
        assert_eq!(error, WireError::FrameTooLarge);
        assert_eq!(error.close_code(), 1009);
    }

    #[test]
    fn payload_is_not_copied_by_the_decoder() {
        let frame = host_frame(&HostEnvelope::ChannelData {
            channel: ChannelId(1),
            payload: proto_payload(b"zero-copy"),
        });
        let start = frame.as_ptr() as usize;
        let Ok(FromHost::ChannelData { payload, .. }) = decode_from_host(frame.clone()) else {
            panic!("not channel-data");
        };
        let bytes = payload.into_bytes();
        let offset = bytes.as_ptr() as usize - start;
        assert!(offset < frame.len());
        assert_eq!(bytes.as_ref(), b"zero-copy");
    }

    #[test]
    fn debug_shows_the_length_and_never_the_bytes() {
        let frame = client_frame(&ClientEnvelope::Data {
            payload: proto_payload(b"\xde\xad\xbe\xef"),
        });
        let shown = format!("{:?}", decode_from_client(frame).unwrap());
        assert!(shown.contains("payload_len: 4"), "{shown}");
        assert!(!shown.contains("222"), "{shown}");
    }

    // ---- registration ----------------------------------------------------

    #[test]
    fn valid_signature_gives_the_host_id_of_the_key() {
        let key = signing_key(7);
        let public = key.verifying_key().to_bytes();
        let nonce = [0x42; 32];
        let signature = key.sign(&envelope::register_message(&nonce)).to_bytes();
        let host_id = verify_register(&nonce, &public, &signature).unwrap();
        assert_eq!(host_id, host_id::host_id(&public));
        assert_eq!(host_id.as_str().len(), 32);
    }

    #[test]
    fn signature_over_another_nonce_is_refused() {
        let key = signing_key(7);
        let public = key.verifying_key().to_bytes();
        let signature = key.sign(&envelope::register_message(&[1; 32])).to_bytes();
        let error = verify_register(&[2; 32], &public, &signature).unwrap_err();
        assert_eq!(error, WireError::InvalidSignature);
        assert_eq!(error.close_code(), 4001);
    }

    #[test]
    fn signature_without_the_context_is_refused() {
        // Signing the bare nonce, as a host that forgot the context would.
        let key = signing_key(7);
        let public = key.verifying_key().to_bytes();
        let nonce = [5; 32];
        let signature = key.sign(&nonce).to_bytes();
        assert_eq!(
            verify_register(&nonce, &public, &signature).unwrap_err(),
            WireError::InvalidSignature
        );
    }

    #[test]
    fn signature_of_another_key_is_refused() {
        let signer = signing_key(7);
        let other = signing_key(8).verifying_key().to_bytes();
        let nonce = [3; 32];
        let signature = signer.sign(&envelope::register_message(&nonce)).to_bytes();
        assert_eq!(
            verify_register(&nonce, &other, &signature).unwrap_err(),
            WireError::InvalidSignature
        );
    }

    #[test]
    fn small_order_key_is_refused_by_verify_strict() {
        // The identity point (compressed: 01 followed by zeros) has small
        // order. A zero signature never verifies under `verify_strict`.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let error = verify_register(&[0; 32], &identity, &[0; 64]).unwrap_err();
        assert_eq!(error.close_code(), 4001);
    }

    #[test]
    fn garbage_key_closes_with_4001() {
        // 0xff..ff is not a valid compressed Edwards point.
        let error = verify_register(&[0; 32], &[0xff; 32], &[0; 64]).unwrap_err();
        assert!(
            matches!(
                error,
                WireError::InvalidRoutingKey | WireError::InvalidSignature
            ),
            "{error:?}"
        );
        assert_eq!(error.close_code(), 4001);
    }

    // ---- close codes -----------------------------------------------------

    #[test]
    fn client_close_codes_follow_the_specification() {
        assert_eq!(close_code_for_client(CloseReason::LimitExceeded), 4003);
        assert_eq!(close_code_for_client(CloseReason::ProtocolError), 4004);
        for reason in [
            CloseReason::Normal,
            CloseReason::HostReplaced,
            CloseReason::HostGone,
            CloseReason::HostRejected,
        ] {
            assert_eq!(close_code_for_client(reason), 1000, "{reason:?}");
        }
    }

    #[test]
    fn every_error_maps_to_its_code() {
        let cases = [
            (WireError::FrameTooLarge, 1009),
            (
                WireError::InvalidEnvelope(EnvelopeError::UnknownType(99)),
                4004,
            ),
            (WireError::WrongDirection, 4004),
            (WireError::InvalidRoutingKey, 4001),
            (WireError::InvalidSignature, 4001),
            (WireError::ForwardTooLarge, 4003),
        ];
        for (error, code) in cases {
            assert_eq!(error.close_code(), code, "{error:?}");
        }
        assert_eq!(
            WireError::ForwardTooLarge.client_reason(),
            CloseReason::LimitExceeded
        );
        assert_eq!(
            WireError::WrongDirection.client_reason(),
            CloseReason::ProtocolError
        );
        // The code after `closed { reason }` agrees with the code of the error.
        for error in [WireError::ForwardTooLarge, WireError::WrongDirection] {
            assert_eq!(
                close_code_for_client(error.client_reason()),
                error.close_code()
            );
        }
    }

    // ---- encoding --------------------------------------------------------

    #[test]
    fn relay_to_host_encoders_produce_the_canonical_bytes() {
        assert_eq!(
            encode_challenge(&[0xaa; 32]),
            [&[0x82, 0x00, 0x58, 0x20][..], &[0xaa; 32][..]].concat()
        );
        assert_eq!(encode_channel_opened(ChannelId(7)), [0x82, 0x03, 0x07]);
        assert_eq!(
            encode_channel_closed(ChannelId(7), CloseReason::HostReplaced),
            [0x83, 0x05, 0x07, 0x01]
        );
        assert_eq!(
            encode_channel_data(ChannelId(7), payload(&[1, 2, 3])).unwrap(),
            [0x83, 0x04, 0x07, 0x43, 1, 2, 3]
        );
        let host_id = host_id::host_id(&[0; 32]);
        let registered = encode_registered(&host_id);
        assert_eq!(&registered[..4], &[0x82, 0x02, 0x78, 0x20]);
        assert_eq!(&registered[4..], host_id.as_str().as_bytes());
    }

    #[test]
    fn relay_to_client_encoders_produce_the_canonical_bytes() {
        assert_eq!(
            encode_data(payload(&[0xaa, 0xbb])).unwrap(),
            [0x82, 0x10, 0x42, 0xaa, 0xbb]
        );
        assert_eq!(encode_host_unavailable(), [0x81, 0x11]);
        assert_eq!(
            encode_closed(CloseReason::LimitExceeded),
            [0x82, 0x12, 0x04]
        );
    }

    #[test]
    fn encoded_frames_decode_back() {
        let bytes = encode_channel_data(ChannelId(300), payload(b"abc")).unwrap();
        assert_eq!(
            envelope::decode_host(Bytes::from(bytes)),
            Ok(HostEnvelope::ChannelData {
                channel: ChannelId(300),
                payload: proto_payload(b"abc"),
            })
        );
        let bytes = encode_data(payload(b"abc")).unwrap();
        assert_eq!(
            envelope::decode_client(Bytes::from(bytes)),
            Ok(ClientEnvelope::Data {
                payload: proto_payload(b"abc"),
            })
        );
    }

    #[test]
    fn a_payload_that_would_overflow_the_frame_is_a_limit_not_a_drop() {
        // 69_992 bytes with channel 0 make a frame of exactly MAX_FRAME.
        let at_limit = OpaquePayload::from_bytes(Bytes::from(vec![0u8; 69_992]));
        let frame = encode_channel_data(ChannelId(0), at_limit).unwrap();
        assert_eq!(frame.len(), MAX_FRAME);

        let over = OpaquePayload::from_bytes(Bytes::from(vec![0u8; 69_993]));
        let error = encode_channel_data(ChannelId(0), over).unwrap_err();
        assert_eq!(error, WireError::ForwardTooLarge);
        assert_eq!(error.close_code(), 4003);

        // The same payload fits as a `data` frame (smaller head), but one that
        // is itself over the maximum does not.
        let over = OpaquePayload::from_bytes(Bytes::from(vec![0u8; MAX_FRAME]));
        assert_eq!(encode_data(over).unwrap_err(), WireError::ForwardTooLarge);
    }
}
