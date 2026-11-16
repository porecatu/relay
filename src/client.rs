// SPDX-License-Identifier: GPL-3.0-or-later

//! The client connection: one task per phone, one channel on one host
//! (specification 01 §4; architecture §4 and §5).
//!
//! The task looks the host up, asks the host task for a channel (which writes
//! `channel-opened` to the host), and then moves `payload`s both ways, in the
//! order they came. It owns three things: the socket of the client, the
//! receiving end of the channel's queue (host to client), and the receiving end
//! of a `oneshot` with the reason the host side ended the channel.
//!
//! # Backpressure
//!
//! * **Client to host.** A `data` frame waits in `pending` until the host
//!   task's command queue has room (`Sender::reserve`, inside the `select!`).
//!   While one waits the task reads nothing more from the client, so the
//!   pressure climbs to the TCP connection of the phone; it still forwards what
//!   the host sends, answers its keepalive, and obeys shutdown and the end of
//!   the channel. A slow host slows its clients down and nothing is dropped.
//! * **Host to client.** The host task never waits for this one: if the
//!   channel's queue is full the host task ends the channel with
//!   `limit-exceeded`, and the reason reaches this task by the `oneshot`, which
//!   needs no room in the full queue.
//!
//! The sockets are written with [`send_bounded`], so a peer that never reads
//! cannot hold the task longer than [`crate::server::IO_TIMEOUT`] per frame.
//!
//! # Bandwidth
//!
//! Each direction of a channel has a token bucket in `payload` bytes
//! (`channel_bandwidth_kbps`, `channel_burst_kib`). This task counts what the
//! client sends; the host task counts what the host sends. A frame the bucket
//! refuses ends the channel with `limit-exceeded`, like a full queue does.
//!
//! Only the fields allowed by the PRD (`host_id`, `channel`, `reason`,
//! `bytes_in`, `bytes_out`, `duration_ms`, `peer`) are logged.

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use bytes::Bytes;
use porecatu_protocol::envelope::{ChannelId, CloseReason, close_code};
use porecatu_protocol::host_id::HostId;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::host::{HostCommand, HostHandle, is_too_big, reason_name};
use crate::limits::TokenBucket;
use crate::metrics::Direction;
use crate::payload::OpaquePayload;
use crate::registry::AllocError;
use crate::server::{ServerState, close_with, send_bounded};
use crate::wire::{self, FromClient, WireError};

/// How many times a client asks for a channel when the host task it found
/// turns out to be leaving (replaced or dropped between the lookup and the
/// answer). The last look-up that still finds a leaving host answers
/// `host-unavailable`.
const OPEN_ATTEMPTS: usize = 3;

/// How the client's WebSocket ends.
enum ClientEnd {
    /// `closed { reason }`, then a `Close` with the code of the reason.
    Closed(CloseReason),
    /// A `Close` with this code and no `closed` before it (the frame never
    /// reached the envelope level).
    Code(u16),
    /// Drop the socket: the peer is gone or stopped answering the keepalive.
    Drop,
}

/// How a channel ends: what the client is told, and what the host is told (if
/// it has to be: when the host side ended the channel it already knows).
struct Finish {
    client: ClientEnd,
    to_host: Option<CloseReason>,
    /// The host side ended the channel in an orderly way: what is still queued
    /// for the client goes out before `closed`.
    drain: bool,
}

impl Finish {
    /// The client ended it (or the connection to the client failed).
    fn client_side(client: ClientEnd, to_host: CloseReason) -> Self {
        Self {
            client,
            to_host: Some(to_host),
            drain: false,
        }
    }

    /// The host side ended it, with `reason`.
    fn host_side(reason: CloseReason) -> Self {
        Self {
            client: ClientEnd::Closed(reason),
            to_host: None,
            drain: matches!(reason, CloseReason::Normal | CloseReason::HostRejected),
        }
    }
}

/// What came of asking for a channel.
enum Opened {
    /// The channel, and the two ways the host side talks back.
    Channel {
        channel: ChannelId,
        data: mpsc::Receiver<OpaquePayload>,
        close: oneshot::Receiver<CloseReason>,
    },
    /// The host has no room for another channel.
    Refused,
    /// The host task was leaving: look the host up again.
    Retry,
    /// The relay is shutting down.
    Shutdown,
}

/// Runs a client connection until it ends.
pub(crate) async fn run(
    mut socket: WebSocket,
    state: Arc<ServerState>,
    host_id: HostId,
    peer: IpAddr,
) {
    let started = Instant::now();
    let peer_log = state.peer_for_log(peer);
    let shutdown = state.shutdown_token();

    // ---- 1. a host, and a channel on it ----------------------------------
    let mut opened = None;
    let mut handle = None;
    for _ in 0..OPEN_ATTEMPTS {
        let Some(found) = state.registry().lookup(&host_id) else {
            break;
        };
        match open_channel(
            &found,
            state.settings().limits.channel_queue_frames,
            shutdown,
        )
        .await
        {
            Opened::Retry => {}
            other => {
                opened = Some(other);
                handle = Some(found);
                break;
            }
        }
    }
    let (Some(opened), Some(handle)) = (opened, handle) else {
        // No host, or one that kept leaving: 01 §4, step 2.
        let frame = wire::encode_host_unavailable();
        if send_bounded(&mut socket, Message::Binary(frame.into()))
            .await
            .is_ok()
        {
            close_with(&mut socket, close_code::NORMAL).await;
        }
        return;
    };
    let (channel, mut data, mut close) = match opened {
        Opened::Channel {
            channel,
            data,
            close,
        } => (channel, data, close),
        Opened::Refused => {
            finish_client(
                &mut socket,
                &state,
                ClientEnd::Closed(CloseReason::LimitExceeded),
            )
            .await;
            return;
        }
        Opened::Shutdown => {
            finish_client(&mut socket, &state, ClientEnd::Closed(CloseReason::Normal)).await;
            return;
        }
        Opened::Retry => return,
    };

    // ---- 2. the channel ---------------------------------------------------
    let timing = state.timing();
    let mut next_ping = Instant::now() + timing.ping_interval;
    let mut pong_deadline = Instant::now() + timing.pong_timeout;
    let mut pending: Option<OpaquePayload> = None;
    let mut bytes_in: u64 = 0;
    let mut bytes_out: u64 = 0;
    // Bandwidth of the client-to-host direction of this channel; the other
    // direction is counted by the host task.
    let mut from_client = TokenBucket::new(
        state.settings().limits.channel_burst_bytes,
        state.settings().limits.channel_bytes_per_second,
        Instant::now().into_std(),
    );

    let finish = loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                // The host task closes its own connection; nothing to tell it.
                break Finish {
                    client: ClientEnd::Closed(CloseReason::Normal),
                    to_host: None,
                    drain: false,
                };
            }
            reason = &mut close => {
                break Finish::host_side(reason.unwrap_or(CloseReason::HostGone));
            }
            // While a `data` waits for room, the client is not being read, so
            // its `Pong` cannot arrive: its deadline is not running.
            () = tokio::time::sleep_until(pong_deadline), if pending.is_none() => {
                break Finish::client_side(ClientEnd::Drop, CloseReason::Normal);
            }
            () = tokio::time::sleep_until(next_ping) => {
                if send_bounded(&mut socket, Message::Ping(Bytes::new())).await.is_err() {
                    break Finish::client_side(ClientEnd::Drop, CloseReason::Normal);
                }
                next_ping += timing.ping_interval;
            }
            payload = data.recv() => match payload {
                Some(payload) => {
                    bytes_out = bytes_out.saturating_add(payload.len() as u64);
                    match wire::encode_data(payload) {
                        Ok(frame) => {
                            if send_bounded(&mut socket, Message::Binary(frame.into())).await.is_err() {
                                break Finish::client_side(ClientEnd::Drop, CloseReason::Normal);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                host_id = %host_id,
                                channel = channel.0,
                                reason = "frame-too-large",
                                "channel limit exceeded"
                            );
                            let reason = error.client_reason();
                            break Finish::client_side(ClientEnd::Closed(reason), reason);
                        }
                    }
                }
                // The host task dropped its end: the reason is already in
                // `close`.
                None => {
                    let reason = close.await.unwrap_or(CloseReason::HostGone);
                    break Finish::host_side(reason);
                }
            },
            permit = handle.commands().reserve(), if pending.is_some() => {
                let Ok(permit) = permit else {
                    // The host task is gone and the queue with it.
                    break Finish::host_side(CloseReason::HostGone);
                };
                if let Some(payload) = pending.take() {
                    permit.send(HostCommand::ClientData { channel, payload });
                }
                // Reading resumes: give the client's `Pong` a fresh period.
                pong_deadline = Instant::now() + timing.pong_timeout;
            }
            incoming = socket.recv(), if pending.is_none() => match incoming {
                Some(Ok(Message::Pong(_))) => pong_deadline = Instant::now() + timing.pong_timeout,
                Some(Ok(Message::Ping(_))) => {}
                Some(Ok(Message::Binary(frame))) => match wire::decode_from_client(frame) {
                    Ok(FromClient::Data { payload }) => {
                        // A frame the bucket refuses is not queued and not
                        // dropped quietly: the channel ends.
                        if !from_client.try_take(payload.len(), Instant::now().into_std()) {
                            tracing::warn!(
                                host_id = %host_id,
                                channel = channel.0,
                                reason = "bandwidth",
                                "channel limit exceeded"
                            );
                            break Finish::client_side(
                                ClientEnd::Closed(CloseReason::LimitExceeded),
                                CloseReason::LimitExceeded,
                            );
                        }
                        bytes_in = bytes_in.saturating_add(payload.len() as u64);
                        state
                            .metrics()
                            .payload_forwarded(Direction::ClientToHost, payload.len());
                        pending = Some(payload);
                    }
                    Err(error) => break on_wire_error(&error),
                },
                Some(Ok(Message::Text(_))) => {
                    break Finish::client_side(
                        ClientEnd::Code(close_code::UNSUPPORTED_DATA),
                        CloseReason::ProtocolError,
                    );
                }
                Some(Ok(Message::Close(_))) | None => {
                    break Finish::client_side(ClientEnd::Drop, CloseReason::Normal);
                }
                Some(Err(error)) => {
                    break if is_too_big(&error) {
                        tracing::warn!(
                            host_id = %host_id,
                            channel = channel.0,
                            reason = "frame-too-large",
                            "channel limit exceeded"
                        );
                        Finish::client_side(
                            ClientEnd::Code(close_code::MESSAGE_TOO_BIG),
                            CloseReason::LimitExceeded,
                        )
                    } else {
                        Finish::client_side(ClientEnd::Drop, CloseReason::Normal)
                    };
                }
            },
        }
    };

    // ---- 3. the end -------------------------------------------------------
    drop(pending);
    let reason_for_log = match (&finish.client, finish.to_host) {
        (ClientEnd::Closed(reason), _) => reason_name(*reason),
        (_, Some(reason)) => reason_name(reason),
        (_, None) => "normal",
    };
    // The host first: the close handshake with the client may take a while.
    if let Some(reason) = finish.to_host {
        tokio::select! {
            _ = handle.commands().send(HostCommand::ClientGone { channel, reason }) => {}
            () = shutdown.cancelled() => {}
        }
    }
    if finish.drain {
        // What the host sent before it closed the channel is still the
        // client's, and in order. The host task put the reason in `close`
        // before it dropped its end, so nothing more can arrive.
        while let Ok(payload) = data.try_recv() {
            bytes_out = bytes_out.saturating_add(payload.len() as u64);
            let Ok(frame) = wire::encode_data(payload) else {
                break;
            };
            if send_bounded(&mut socket, Message::Binary(frame.into()))
                .await
                .is_err()
            {
                break;
            }
        }
    }
    finish_client(&mut socket, &state, finish.client).await;

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        host_id = %host_id,
        channel = channel.0,
        peer = peer_log.as_deref(),
        reason = reason_for_log,
        bytes_in,
        bytes_out,
        duration_ms,
        "channel closed"
    );
}

/// The wire error of a frame of the client: what the client and the host are
/// told (01 §4).
fn on_wire_error(error: &WireError) -> Finish {
    let reason = error.client_reason();
    match error {
        // The frame never became an envelope: `1009`, no `closed` before it.
        WireError::FrameTooLarge => {
            Finish::client_side(ClientEnd::Code(close_code::MESSAGE_TOO_BIG), reason)
        }
        _ => Finish::client_side(ClientEnd::Closed(reason), reason),
    }
}

/// Asks the host task for a channel and waits for the answer.
async fn open_channel(
    handle: &HostHandle,
    queue_frames: usize,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Opened {
    let (to_client, data) = mpsc::channel(queue_frames.max(1));
    let (close_tx, close) = oneshot::channel();
    let (reply_tx, reply) = oneshot::channel();
    let command = HostCommand::OpenChannel {
        to_client,
        close: close_tx,
        reply: reply_tx,
    };
    let permit = tokio::select! {
        permit = handle.commands().reserve() => permit,
        () = shutdown.cancelled() => return Opened::Shutdown,
    };
    match permit {
        Ok(permit) => permit.send(command),
        Err(_) => return Opened::Retry,
    }
    let answer = tokio::select! {
        answer = reply => answer,
        () = shutdown.cancelled() => return Opened::Shutdown,
    };
    match answer {
        Ok(Ok(channel)) => Opened::Channel {
            channel,
            data,
            close,
        },
        Ok(Err(AllocError::TooManyLive | AllocError::Exhausted)) => Opened::Refused,
        Err(_) => Opened::Retry,
    }
}

/// Ends the client's WebSocket as `end` says. A relay that is shutting down
/// closes with `1001` whatever the reason.
async fn finish_client(socket: &mut WebSocket, state: &ServerState, end: ClientEnd) {
    match end {
        ClientEnd::Closed(reason) => {
            let frame = wire::encode_closed(reason);
            if send_bounded(socket, Message::Binary(frame.into()))
                .await
                .is_err()
            {
                return;
            }
            let code = if state.shutdown_token().is_cancelled() {
                close_code::GOING_AWAY
            } else {
                wire::close_code_for_client(reason)
            };
            close_with(socket, code).await;
        }
        ClientEnd::Code(code) => close_with(socket, code).await,
        ClientEnd::Drop => {}
    }
}
