// SPDX-License-Identifier: GPL-3.0-or-later

//! The host connection, from the upgrade to the registry, and then the
//! channels of its clients (specification 01 §1, §3, §4 and §5).
//!
//! One task per host connection. It sends the `challenge`, waits for the
//! `register` within `register_timeout` (`4005` if late), verifies the
//! signature (`4001`), takes the `host_id` in the registry (`4003` when full,
//! and `4002` for the connection it replaces), answers `registered`, and then
//! serves the channels while keeping the connection alive with a `Ping` every
//! 25 s, dropping it when no `Pong` arrives in 60 s. Whatever way the task
//! ends, it removes its own registration, and only that one: a connection that
//! was already replaced removes nothing (see [`crate::registry`]).
//!
//! # Channels
//!
//! The host task owns the channel table: the [`ChannelAllocator`] and, for each
//! live channel, the way to reach the client task that serves it. Client tasks
//! reach the host task through the bounded command queue of its
//! [`HostHandle`] ([`HostCommand`]): open a channel, data of a channel, the
//! client of a channel is gone. One queue carries all three, so what a client
//! sent arrives in the order it was sent, and a `ClientGone` never overtakes
//! the data before it. The select loop of the task **always** drains that queue
//! (and the socket), so a client that waits for room in it is waiting for a
//! task that is making progress.
//!
//! The way back is one bounded queue of payloads per channel plus a `oneshot`
//! with the reason the channel ended. The host task never waits for a client:
//! a full queue ends **that channel** with `limit-exceeded` (a slow phone must
//! not stop the others), and the reason travels out of band because the queue
//! it would have to travel in is the one that is full. The same happens when
//! the host sends faster than the channel's bandwidth bucket allows (the
//! bucket of the other direction is in [`crate::client`]).
//!
//! Every wait is a `sleep_until` inside a `select!`, together with the socket,
//! the "replaced" signal and the shutdown signal. No lock is held across an
//! `.await`.
//!
//! Only the fields allowed by the PRD (`host_id`, `channel`, `token_name`,
//! `reason`, `peer`, `duration_ms`) are logged; nothing of what a frame
//! carries is.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use porecatu_protocol::envelope::{ChannelId, CloseReason, NONCE_LEN, close_code};
use porecatu_protocol::host_id::HostId;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::LimitSettings;
use crate::limits::TokenBucket;
use crate::metrics::{Direction, Metrics};
use crate::payload::OpaquePayload;
use crate::registry::{AllocError, ChannelAllocator, Generation, RegisterOutcome};
use crate::server::{ServerState, close_with, send_bounded};
use crate::wire::{self, FromHost, WireError};

/// What a client task asks of the host task of its channel.
pub(crate) enum HostCommand {
    /// Open a channel for a new client.
    OpenChannel {
        /// Where the host task puts the `payload`s of the host for this client.
        to_client: mpsc::Sender<OpaquePayload>,
        /// Where the host task says why the channel ended (before it drops
        /// `to_client`).
        close: oneshot::Sender<CloseReason>,
        /// The answer: the channel, or why there is none.
        reply: oneshot::Sender<Result<ChannelId, AllocError>>,
    },
    /// A `data` of the client, to be forwarded as `channel-data`.
    ClientData {
        /// The channel.
        channel: ChannelId,
        /// The opaque record.
        payload: OpaquePayload,
    },
    /// The client of a channel is gone: the host gets
    /// `channel-closed { reason }`.
    ClientGone {
        /// The channel.
        channel: ChannelId,
        /// Why the client left.
        reason: CloseReason,
    },
}

/// What the registry keeps for a registered host.
///
/// It is cloned out of the registry by whoever needs to reach the host task:
/// the registration of a newer connection of the same `host_id` (the "replaced"
/// signal) and the client tasks (the command queue).
#[derive(Debug, Clone)]
pub struct HostHandle {
    replaced: CancellationToken,
    commands: mpsc::Sender<HostCommand>,
}

impl HostHandle {
    /// A handle and the receiving end of its command queue, which holds at
    /// most `queue_frames` commands.
    fn new(queue_frames: usize) -> (Self, mpsc::Receiver<HostCommand>) {
        let (commands, receiver) = mpsc::channel(queue_frames.max(1));
        let handle = Self {
            replaced: CancellationToken::new(),
            commands,
        };
        (handle, receiver)
    }

    /// Tells the host task that a newer registration took its `host_id`: it
    /// closes the connection with `4002`.
    fn notify_replaced(&self) {
        self.replaced.cancel();
    }

    /// The bounded queue of commands of the host task.
    pub(crate) fn commands(&self) -> &mpsc::Sender<HostCommand> {
        &self.commands
    }
}

/// The reason's name in logs.
pub(crate) fn reason_name(reason: CloseReason) -> &'static str {
    match reason {
        CloseReason::Normal => "normal",
        CloseReason::HostReplaced => "host-replaced",
        CloseReason::HostGone => "host-gone",
        CloseReason::HostRejected => "host-rejected",
        CloseReason::LimitExceeded => "limit-exceeded",
        CloseReason::ProtocolError => "protocol-error",
    }
}

/// How a live channel reaches its client task.
struct ChannelEntry {
    to_client: mpsc::Sender<OpaquePayload>,
    close: oneshot::Sender<CloseReason>,
    /// Bandwidth of the host-to-client direction of this channel, counted in
    /// `payload` bytes as the host sends them. The other direction has its own
    /// bucket in the client task.
    from_host: TokenBucket,
}

/// The channel table of one host connection.
struct Channels {
    allocator: ChannelAllocator,
    entries: HashMap<u64, ChannelEntry>,
    /// Burst of each new channel's bucket, in bytes.
    burst_bytes: u64,
    /// Sustained rate of each new channel's bucket, in bytes per second.
    bytes_per_second: u64,
    /// Where opened and ended channels are counted (aggregate only).
    metrics: Arc<Metrics>,
}

impl Channels {
    fn new(limits: &LimitSettings, metrics: Arc<Metrics>) -> Self {
        Self {
            allocator: ChannelAllocator::new(limits.max_channels_per_host),
            entries: HashMap::new(),
            burst_bytes: limits.channel_burst_bytes,
            bytes_per_second: limits.channel_bytes_per_second,
            metrics,
        }
    }

    fn open(
        &mut self,
        to_client: mpsc::Sender<OpaquePayload>,
        close: oneshot::Sender<CloseReason>,
    ) -> Result<ChannelId, AllocError> {
        let channel = self.allocator.allocate()?;
        let from_host = TokenBucket::new(
            self.burst_bytes,
            self.bytes_per_second,
            Instant::now().into_std(),
        );
        self.entries.insert(
            channel.0,
            ChannelEntry {
                to_client,
                close,
                from_host,
            },
        );
        self.metrics.channel_opened();
        Ok(channel)
    }

    /// Ends a live channel and tells its client task why. Returns whether the
    /// channel was live.
    fn close(&mut self, channel: ChannelId, reason: CloseReason) -> bool {
        let Some(entry) = self.entries.remove(&channel.0) else {
            return false;
        };
        self.allocator.release(channel);
        // The reason first, then the queue sender drops with `entry`: the
        // client task sees the end of the queue with the reason already there.
        let _ = entry.close.send(reason);
        self.metrics.channel_closed(reason);
        true
    }

    /// Ends every channel with `reason` (the host connection is over).
    fn close_all(&mut self, reason: CloseReason) {
        for (_, entry) in self.entries.drain() {
            let _ = entry.close.send(reason);
            self.metrics.channel_closed(reason);
        }
    }
}

/// How the connection ends.
enum Exit {
    /// Send a `Close` frame with this code, then drop the socket.
    Close(u16, &'static str),
    /// Drop the socket with no `Close` frame (the peer is gone, or stopped
    /// answering the keepalive).
    Drop(&'static str),
}

impl Exit {
    /// What the clients of the host's channels are told (01 §4).
    fn client_reason(&self) -> CloseReason {
        match self {
            Self::Close(close_code::REPLACED, _) => CloseReason::HostReplaced,
            // The relay is shutting down: clients get `closed { normal }`.
            Self::Close(close_code::GOING_AWAY, _) => CloseReason::Normal,
            _ => CloseReason::HostGone,
        }
    }
}

/// Runs a host connection until it ends.
pub(crate) async fn run(
    mut socket: WebSocket,
    state: Arc<ServerState>,
    token_name: String,
    peer: IpAddr,
) {
    let started = Instant::now();
    let peer_log = state.peer_for_log(peer);
    let limits = &state.settings().limits;
    let (handle, mut commands) = HostHandle::new(limits.host_queue_frames);
    let mut channels = Channels::new(limits, Arc::clone(state.metrics()));
    let mut registration: Option<(HostId, Generation)> = None;

    let exit = session(
        &mut socket,
        &state,
        &handle,
        &mut registration,
        &mut commands,
        &mut channels,
    )
    .await;

    // Out of the registry before anything slow (the close handshake), so a
    // presence query does not see a host that is already leaving, and a client
    // that arrives now does not find a task that is going away.
    if let Some((host_id, generation)) = &registration {
        state.registry().unregister(host_id, *generation);
    }
    // Nobody is draining the queue any more: whoever still puts a command in
    // it learns that from the closed queue, and the commands already in it are
    // dropped with their `reply` (the opener then looks the host up again).
    commands.close();
    while commands.try_recv().is_ok() {}
    // Every client of this connection hears why, before the slow close below.
    channels.close_all(exit.client_reason());

    let reason = match &exit {
        Exit::Close(code, reason) => {
            state.metrics().host_connection_closed(Some(*code));
            close_with(&mut socket, *code).await;
            *reason
        }
        Exit::Drop(reason) => {
            state.metrics().host_connection_closed(None);
            reason
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match &registration {
        Some((host_id, _)) => tracing::info!(
            host_id = %host_id,
            token_name = token_name.as_str(),
            peer = peer_log.as_deref(),
            reason,
            duration_ms,
            "host disconnected"
        ),
        None => tracing::info!(
            token_name = token_name.as_str(),
            peer = peer_log.as_deref(),
            reason,
            duration_ms,
            "host connection ended before registration"
        ),
    }
}

/// The life of the connection: registration, then keepalive and channels.
/// `registration` is filled as soon as the registry holds this connection.
async fn session(
    socket: &mut WebSocket,
    state: &ServerState,
    handle: &HostHandle,
    registration: &mut Option<(HostId, Generation)>,
    commands: &mut mpsc::Receiver<HostCommand>,
    channels: &mut Channels,
) -> Exit {
    let shutdown = state.shutdown_token();
    let timing = state.timing();

    // ---- 1. challenge ----------------------------------------------------
    let mut nonce = [0u8; NONCE_LEN];
    if getrandom::fill(&mut nonce).is_err() {
        return Exit::Close(crate::server::CLOSE_INTERNAL_ERROR, "no-randomness");
    }
    if send_bounded(
        socket,
        Message::Binary(wire::encode_challenge(&nonce).into()),
    )
    .await
    .is_err()
    {
        return Exit::Drop("send-failed");
    }

    // ---- 2. register, within the deadline --------------------------------
    let deadline = Instant::now() + state.settings().limits.register_timeout;
    let frame = loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {
                return Exit::Close(close_code::REGISTER_TIMEOUT, "register-timeout");
            }
            () = shutdown.cancelled() => return Exit::Close(close_code::GOING_AWAY, "shutdown"),
            incoming = socket.recv() => match classify(incoming) {
                Incoming::Frame(frame) => break frame,
                Incoming::Ignore => {}
                Incoming::End(exit) => return exit,
            },
        }
    };
    let (routing_key, signature) = match wire::decode_from_host(frame) {
        Ok(FromHost::Register {
            routing_key,
            signature,
        }) => (routing_key, signature),
        Ok(_) => return protocol_error(&WireError::WrongDirection),
        Err(error) => return protocol_error(&error),
    };
    let host_id = match wire::verify_register(&nonce, &routing_key, &signature) {
        Ok(host_id) => host_id,
        Err(error) => return Exit::Close(error.close_code(), "register-rejected"),
    };

    // ---- 3. the registry -------------------------------------------------
    match state.registry().register(host_id.clone(), handle.clone()) {
        RegisterOutcome::Full => return Exit::Close(close_code::LIMIT_EXCEEDED, "hosts-full"),
        RegisterOutcome::New { generation } => *registration = Some((host_id.clone(), generation)),
        RegisterOutcome::Replaced { generation, old } => {
            *registration = Some((host_id.clone(), generation));
            // Outside the registry lock: the old task wakes up and closes
            // with 4002 on its own.
            old.notify_replaced();
            tracing::info!(host_id = %host_id, reason = "replaced", "host replaced an older connection");
        }
    }
    state.metrics().host_registered();
    let registered = wire::encode_registered(&host_id);
    if send_bounded(socket, Message::Binary(registered.into()))
        .await
        .is_err()
    {
        return Exit::Drop("send-failed");
    }
    tracing::info!(host_id = %host_id, "host registered");

    // ---- 4. keepalive and channels ---------------------------------------
    let mut next_ping = Instant::now() + timing.ping_interval;
    let mut pong_deadline = Instant::now() + timing.pong_timeout;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Exit::Close(close_code::GOING_AWAY, "shutdown"),
            () = handle.replaced.cancelled() => {
                return Exit::Close(close_code::REPLACED, "replaced");
            }
            () = tokio::time::sleep_until(pong_deadline) => return Exit::Drop("pong-timeout"),
            () = tokio::time::sleep_until(next_ping) => {
                if send_bounded(socket, Message::Ping(Bytes::new())).await.is_err() {
                    return Exit::Drop("send-failed");
                }
                next_ping += timing.ping_interval;
            }
            command = commands.recv() => match command {
                Some(command) => {
                    if let Some(exit) = on_command(socket, channels, &host_id, command).await {
                        return exit;
                    }
                }
                // The handle of this very task keeps the queue open.
                None => return Exit::Drop("queue-closed"),
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Pong(_))) => pong_deadline = Instant::now() + timing.pong_timeout,
                other => match classify(other) {
                    Incoming::Frame(frame) => {
                        if let Some(exit) = on_host_frame(socket, channels, &host_id, frame).await {
                            return exit;
                        }
                    }
                    Incoming::Ignore => {}
                    Incoming::End(exit) => return exit,
                },
            },
        }
    }
}

/// Writes one binary frame to the host. `Some` is the exit when it fails.
async fn write_to_host(socket: &mut WebSocket, frame: Vec<u8>) -> Option<Exit> {
    send_bounded(socket, Message::Binary(frame.into()))
        .await
        .err()
        .map(|()| Exit::Drop("send-failed"))
}

/// A command of a client task. `Some` ends the connection.
async fn on_command(
    socket: &mut WebSocket,
    channels: &mut Channels,
    host_id: &HostId,
    command: HostCommand,
) -> Option<Exit> {
    match command {
        HostCommand::OpenChannel {
            to_client,
            close,
            reply,
        } => {
            let channel = match channels.open(to_client, close) {
                Ok(channel) => channel,
                Err(error) => {
                    // The client is told `limit-exceeded`; nothing was opened,
                    // so the host hears nothing.
                    let _ = reply.send(Err(error));
                    tracing::warn!(host_id = %host_id, reason = "channels-full", "channel refused");
                    return None;
                }
            };
            // `channel-opened` goes out before the client can send anything.
            if let Some(exit) = write_to_host(socket, wire::encode_channel_opened(channel)).await {
                return Some(exit);
            }
            tracing::info!(host_id = %host_id, channel = channel.0, "channel opened");
            if reply.send(Ok(channel)).is_err() {
                // The client went away while it waited: close what was opened.
                return close_towards_host(
                    socket,
                    channels,
                    channel,
                    CloseReason::Normal,
                    CloseReason::Normal,
                )
                .await;
            }
            None
        }
        HostCommand::ClientData { channel, payload } => {
            if !channels.allocator.is_live(channel) {
                // The host closed it meanwhile: ignored (01 §4).
                return None;
            }
            match wire::encode_channel_data(channel, payload) {
                Ok(frame) => write_to_host(socket, frame).await,
                Err(error) => {
                    // The frame would be over the maximum: the channel
                    // exceeded a limit, the payload is not dropped quietly.
                    tracing::warn!(
                        host_id = %host_id,
                        channel = channel.0,
                        reason = "frame-too-large",
                        "channel limit exceeded"
                    );
                    let reason = error.client_reason();
                    close_towards_host(socket, channels, channel, reason, reason).await
                }
            }
        }
        HostCommand::ClientGone { channel, reason } => {
            if !channels.allocator.is_live(channel) {
                return None;
            }
            close_towards_host(socket, channels, channel, reason, reason).await
        }
    }
}

/// Ends a live channel: its client task is told `to_client`, the host gets
/// `channel-closed { to_host }`.
async fn close_towards_host(
    socket: &mut WebSocket,
    channels: &mut Channels,
    channel: ChannelId,
    to_client: CloseReason,
    to_host: CloseReason,
) -> Option<Exit> {
    if !channels.close(channel, to_client) {
        return None;
    }
    write_to_host(socket, wire::encode_channel_closed(channel, to_host)).await
}

/// A frame from the registered host. `Some` ends the connection.
///
/// `channel-data` goes to the client task of the channel and `close-channel`
/// ends it. For a channel that was closed, both are ignored; for one that was
/// never opened (and for a second `register`) they are an invalid envelope
/// (specification 01 §4).
async fn on_host_frame(
    socket: &mut WebSocket,
    channels: &mut Channels,
    host_id: &HostId,
    frame: Bytes,
) -> Option<Exit> {
    let message = match wire::decode_from_host(frame) {
        Ok(message) => message,
        Err(error) => return Some(protocol_error(&error)),
    };
    match message {
        FromHost::Register { .. } => {
            Some(Exit::Close(close_code::INVALID_ENVELOPE, "protocol-error"))
        }
        FromHost::ChannelData { channel, payload } => {
            let Some(entry) = channels.entries.get_mut(&channel.0) else {
                return never_opened(channels, channel);
            };
            // The bandwidth first, then the queue. Either one that says no
            // ends **this channel** with `limit-exceeded`: the payload is not
            // dropped quietly, and a slow or greedy client costs the others
            // nothing.
            let size = payload.len();
            let exceeded = if !entry.from_host.try_take(size, Instant::now().into_std()) {
                Some("bandwidth")
            } else {
                match entry.to_client.try_send(payload) {
                    Ok(()) => {
                        channels
                            .metrics
                            .payload_forwarded(Direction::HostToClient, size);
                        None
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => Some("queue-full"),
                    // The client task is gone; its `ClientGone` is on the way.
                    Err(mpsc::error::TrySendError::Closed(_)) => return None,
                }
            };
            let limit = exceeded?;
            tracing::warn!(
                host_id = %host_id,
                channel = channel.0,
                reason = limit,
                "channel limit exceeded"
            );
            close_towards_host(
                socket,
                channels,
                channel,
                CloseReason::LimitExceeded,
                CloseReason::LimitExceeded,
            )
            .await
        }
        FromHost::CloseChannel { channel, reason } => {
            if channels.close(channel, reason) {
                // No `channel-closed` comes back for the host's own close.
                None
            } else {
                never_opened(channels, channel)
            }
        }
    }
}

/// What a frame for a channel that is not live means: ignored if the channel
/// was closed, an invalid envelope if it was never opened.
fn never_opened(channels: &Channels, channel: ChannelId) -> Option<Exit> {
    if channels.allocator.was_issued(channel) {
        None
    } else {
        Some(Exit::Close(close_code::INVALID_ENVELOPE, "protocol-error"))
    }
}

fn protocol_error(error: &WireError) -> Exit {
    let reason = match error {
        WireError::FrameTooLarge => "frame-too-large",
        _ => "protocol-error",
    };
    Exit::Close(error.close_code(), reason)
}

/// What one read of the socket means.
enum Incoming {
    /// A binary frame: one envelope.
    Frame(Bytes),
    /// A control frame that needs no action here (`Ping`, `Pong`; the library
    /// answers pings itself).
    Ignore,
    /// The connection is over, one way or the other.
    End(Exit),
}

fn classify(incoming: Option<Result<Message, axum::Error>>) -> Incoming {
    match incoming {
        None => Incoming::End(Exit::Drop("peer-closed")),
        Some(Err(error)) => Incoming::End(if is_too_big(&error) {
            Exit::Close(close_code::MESSAGE_TOO_BIG, "frame-too-large")
        } else {
            Exit::Drop("read-error")
        }),
        Some(Ok(Message::Binary(frame))) => Incoming::Frame(frame),
        Some(Ok(Message::Text(_))) => {
            Incoming::End(Exit::Close(close_code::UNSUPPORTED_DATA, "text-frame"))
        }
        Some(Ok(Message::Ping(_) | Message::Pong(_))) => Incoming::Ignore,
        Some(Ok(Message::Close(_))) => Incoming::End(Exit::Drop("peer-closed")),
    }
}

/// Whether the library refused a frame for its size.
///
/// `axum::Error` wraps the `tungstenite::Error` of the WebSocket library and
/// hands it out as its `source`, so the kind is found by a downcast, not by the
/// text of the message. The library is a direct dependency pinned to the
/// version `axum` resolves, so the downcast always finds the type it looks for.
pub(crate) fn is_too_big(error: &axum::Error) -> bool {
    use std::error::Error as _;

    error
        .source()
        .and_then(|source| source.downcast_ref::<tungstenite::Error>())
        .is_some_and(|error| matches!(error, tungstenite::Error::Capacity(_)))
}

#[cfg(test)]
mod tests {
    use tungstenite::error::CapacityError;

    use super::is_too_big;

    #[test]
    fn a_capacity_error_is_too_big() {
        let error = axum::Error::new(tungstenite::Error::Capacity(
            CapacityError::MessageTooLong {
                size: 70_001,
                max_size: 70_000,
            },
        ));
        assert!(is_too_big(&error));
    }

    #[test]
    fn other_library_errors_are_not_too_big() {
        let closed = axum::Error::new(tungstenite::Error::ConnectionClosed);
        let io = axum::Error::new(tungstenite::Error::Io(std::io::Error::other("reset")));
        assert!(!is_too_big(&closed));
        assert!(!is_too_big(&io));
    }

    #[test]
    fn the_text_of_the_message_is_not_what_decides() {
        let lookalike = axum::Error::new(std::io::Error::other("Message too long"));
        assert!(!is_too_big(&lookalike));
    }
}
