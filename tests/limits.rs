// SPDX-License-Identifier: GPL-3.0-or-later

//! Every limit of `[limits]` that a channel can exceed, over real sockets
//! (specification 01 §4 and §6; PRD-001 RF-1.14 and RF-1.15).
//!
//! The rule under test is the same for all of them: the channel (never the
//! relay, never another channel) ends with `closed { limit-exceeded }` and
//! `4003` for the client and `channel-closed { limit-exceeded }` for the host,
//! and a `payload` is never dropped in silence. The two exceptions are the
//! frame over the WebSocket maximum, which closes with `1009`, and the host
//! queue, which is backpressure and closes nothing (architecture §5).
//!
//! Limits are shortened through [`porecatu_relay::config::Settings`], so no
//! test needs megabytes of traffic or minutes of waiting. A `rate` of `1`
//! byte per second makes a bucket a plain budget: nothing refills in the time
//! a test takes.

mod common;

use std::time::Duration;

use bytes::Bytes;
use common::{
    Event, FakeHost, TestServer, TestWs, attach, client_data, client_next, client_reads_data,
    expect_closed, host_data, host_next, host_reads_data, start,
};
use porecatu_protocol::envelope::{
    self, ChannelId, ClientEnvelope, CloseReason, HostEnvelope, MAX_FRAME, close_code,
};
use porecatu_relay::config::LimitSettings;
use porecatu_relay::server::Timing;

const SHORT: Duration = Duration::from_millis(300);

/// A rate so high that the bucket is full again by the next frame: the
/// bandwidth cannot be what ends a test about something else.
const UNLIMITED_RATE: u64 = 1 << 40;

// ---- helpers ------------------------------------------------------------------------------

/// A server with the limits adjusted, and a registered host.
fn start_limited(adjust: impl FnOnce(&mut LimitSettings)) -> (TestServer, FakeHost, TestWs) {
    let server = TestServer::start_with(Timing::default(), |settings| adjust(&mut settings.limits));
    let host = FakeHost::new(1);
    let host_ws = host.connect_and_register(&server);
    (server, host, host_ws)
}

/// A server whose channels have a bucket of `burst` bytes refilled at `rate`
/// bytes per second, and a registered host.
fn start_with_bucket(rate: u64, burst: u64) -> (TestServer, FakeHost, TestWs) {
    start_limited(|limits| {
        limits.channel_bytes_per_second = rate;
        limits.channel_burst_bytes = burst;
    })
}

/// The host hears `channel-closed { limit-exceeded }` for `channel`.
#[track_caller]
fn expect_host_told_limit(host_ws: &mut TestWs, channel: u64) {
    assert_eq!(
        host_next(host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(channel),
            reason: CloseReason::LimitExceeded,
        }
    );
}

// ---- size ---------------------------------------------------------------------------------------

#[test]
fn a_websocket_frame_over_the_maximum_closes_the_client_with_1009() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    client.send_binary(vec![0; MAX_FRAME + 1]);
    client.expect_close(close_code::MESSAGE_TOO_BIG);
    expect_host_told_limit(&mut host_ws, 0);
}

#[test]
fn a_data_that_would_not_fit_as_channel_data_is_a_limit_exceeded() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    // The two lengths of the protocol crate are the limit: 69 993 bytes fit in
    // a `data` of exactly MAX_FRAME, and the `channel-data` that must carry
    // them is one byte over.
    assert_eq!(envelope::client_data_len(69_993), MAX_FRAME);
    assert_eq!(
        envelope::channel_data_len(ChannelId(0), 69_993),
        MAX_FRAME + 1
    );
    client.send_binary(client_data(&vec![1u8; 69_993]));
    expect_closed(
        &mut client,
        CloseReason::LimitExceeded,
        close_code::LIMIT_EXCEEDED,
    );
    expect_host_told_limit(&mut host_ws, 0);
    assert_eq!(server.presence(host.host_id().as_str()), 204);
}

#[test]
fn a_websocket_frame_over_the_maximum_closes_the_host_with_1009() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    host_ws.send_binary(vec![0; MAX_FRAME + 1]);
    host_ws.expect_close(close_code::MESSAGE_TOO_BIG);
    // The host connection is gone, and its client is told so.
    assert_eq!(
        client_next(&mut client),
        ClientEnvelope::Closed {
            reason: CloseReason::HostGone
        }
    );
    server.wait_presence(host.host_id().as_str(), 404);
}

// ---- channels per host ---------------------------------------------------------------------------

#[test]
fn the_channel_limit_closes_the_extra_client_with_limit_exceeded() {
    let (server, host, mut host_ws) = start_limited(|limits| limits.max_channels_per_host = 2);
    let (first, _) = attach(&server, &host, &mut host_ws);
    let (_second, _) = attach(&server, &host, &mut host_ws);

    let mut third = server.connect_client(host.host_id().as_str()).unwrap();
    expect_closed(
        &mut third,
        CloseReason::LimitExceeded,
        close_code::LIMIT_EXCEEDED,
    );
    // The host heard nothing about the refused one.
    assert_eq!(host_ws.next_within(SHORT), Event::Silence);

    // A slot frees up and the number moves on.
    drop(first);
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::Normal,
        }
    );
    let (_fourth, channel) = attach(&server, &host, &mut host_ws);
    assert_eq!(channel, 2);
}

// ---- bandwidth ------------------------------------------------------------------------------------

#[test]
fn client_to_host_bandwidth_over_the_burst_closes_the_channel() {
    // A bucket of 70 000 bytes: two frames of 30 000 fit, the third does not.
    let (server, host, mut host_ws) = start_with_bucket(1, 70_000);
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    let frame = vec![5u8; 30_000];
    for _ in 0..3 {
        client.send_binary(client_data(&frame));
    }
    // What fit arrives whole and in order, and then the host is told.
    assert_eq!(host_reads_data(&mut host_ws), (0, frame.clone()));
    assert_eq!(host_reads_data(&mut host_ws), (0, frame));
    expect_host_told_limit(&mut host_ws, 0);
    expect_closed(
        &mut client,
        CloseReason::LimitExceeded,
        close_code::LIMIT_EXCEEDED,
    );
    // The relay and the host connection are untouched.
    assert_eq!(server.presence(host.host_id().as_str()), 204);
}

#[test]
fn host_to_client_bandwidth_over_the_burst_closes_the_channel() {
    let (server, host, mut host_ws) = start_with_bucket(1, 70_000);
    let (mut client, channel) = attach(&server, &host, &mut host_ws);
    let frame = vec![6u8; 30_000];
    // One at a time: the client reads each before the next is sent, so what
    // fit is not in a queue when the channel ends.
    for _ in 0..2 {
        host_ws.send_binary(host_data(channel, &frame));
        assert_eq!(client_reads_data(&mut client), frame);
    }
    host_ws.send_binary(host_data(channel, &frame));
    expect_closed(
        &mut client,
        CloseReason::LimitExceeded,
        close_code::LIMIT_EXCEEDED,
    );
    expect_host_told_limit(&mut host_ws, channel);
    assert_eq!(server.presence(host.host_id().as_str()), 204);
}

#[test]
fn each_direction_of_a_channel_has_its_own_bucket() {
    let (server, host, mut host_ws) = start_with_bucket(1, 70_000);
    let (mut client, channel) = attach(&server, &host, &mut host_ws);
    let frame = vec![7u8; 60_000];
    // 60 000 up leave 10 000 in that bucket; if the other direction shared it,
    // 60 000 down would not fit.
    client.send_binary(client_data(&frame));
    assert_eq!(host_reads_data(&mut host_ws), (channel, frame.clone()));
    host_ws.send_binary(host_data(channel, &frame));
    assert_eq!(client_reads_data(&mut client), frame);
    assert_eq!(host_ws.next_within(SHORT), Event::Silence);
}

#[test]
fn each_channel_has_its_own_bucket() {
    let (server, host, mut host_ws) = start_with_bucket(1, 70_000);
    let (mut greedy, a) = attach(&server, &host, &mut host_ws);
    let (mut quiet, b) = attach(&server, &host, &mut host_ws);
    let frame = vec![8u8; 60_000];
    greedy.send_binary(client_data(&frame));
    assert_eq!(host_reads_data(&mut host_ws), (a, frame.clone()));
    greedy.send_binary(client_data(&frame));
    expect_host_told_limit(&mut host_ws, a);
    expect_closed(
        &mut greedy,
        CloseReason::LimitExceeded,
        close_code::LIMIT_EXCEEDED,
    );

    // The other channel still has its whole bucket.
    quiet.send_binary(client_data(&frame));
    assert_eq!(host_reads_data(&mut host_ws), (b, frame));
}

#[test]
fn the_sustained_rate_refills_the_bucket_client_to_host() {
    // 100 000 bytes per second, burst 70 000: after 60 000 there are 10 000
    // left, and 800 ms later the bucket is full again.
    let (server, host, mut host_ws) = start_with_bucket(100_000, 70_000);
    let (mut client, channel) = attach(&server, &host, &mut host_ws);
    let frame = vec![9u8; 60_000];
    client.send_binary(client_data(&frame));
    assert_eq!(host_reads_data(&mut host_ws), (channel, frame.clone()));
    std::thread::sleep(Duration::from_millis(800));
    client.send_binary(client_data(&frame));
    assert_eq!(host_reads_data(&mut host_ws), (channel, frame));
    assert_eq!(host_ws.next_within(SHORT), Event::Silence);
}

#[test]
fn the_sustained_rate_refills_the_bucket_host_to_client() {
    let (server, host, mut host_ws) = start_with_bucket(100_000, 70_000);
    let (mut client, channel) = attach(&server, &host, &mut host_ws);
    let frame = vec![9u8; 60_000];
    host_ws.send_binary(host_data(channel, &frame));
    assert_eq!(client_reads_data(&mut client), frame);
    std::thread::sleep(Duration::from_millis(800));
    host_ws.send_binary(host_data(channel, &frame));
    assert_eq!(client_reads_data(&mut client), frame);
    assert_eq!(host_ws.next_within(SHORT), Event::Silence);
}

// ---- queues ---------------------------------------------------------------------------------------

#[test]
fn a_client_that_does_not_read_loses_its_own_channel_and_nobody_elses() {
    let (server, host, mut host_ws) = start_limited(|limits| {
        limits.channel_queue_frames = 4;
        // The bandwidth must not be what ends this channel.
        limits.channel_bytes_per_second = UNLIMITED_RATE;
    });
    let (mut slow, a) = attach(&server, &host, &mut host_ws);
    let (mut other, b) = attach(&server, &host, &mut host_ws);

    // The slow client reads nothing while the host fills its socket buffers
    // and then its queue (4 frames). The host never waits for it.
    let chunk = vec![9u8; 60_000];
    let mut closed_by_relay = None;
    for _ in 0..400 {
        host_ws.send_binary(host_data(a, &chunk));
        if let Event::Binary(frame) = host_ws.next_within(Duration::from_millis(1))
            && let Ok(HostEnvelope::ChannelClosed { channel, reason }) =
                envelope::decode_host(Bytes::from(frame))
        {
            closed_by_relay = Some((channel.0, reason));
            break;
        }
    }
    if closed_by_relay.is_none() {
        // The last frames may still be in flight.
        closed_by_relay = match host_next(&mut host_ws) {
            HostEnvelope::ChannelClosed { channel, reason } => Some((channel.0, reason)),
            other => panic!("expected channel-closed, got {other:?}"),
        };
    }
    assert_eq!(closed_by_relay, Some((a, CloseReason::LimitExceeded)));

    // The other channel on the same host is untouched.
    host_ws.send_binary(host_data(b, b"still flowing"));
    assert_eq!(client_reads_data(&mut other), b"still flowing");
    assert_eq!(server.presence(host.host_id().as_str()), 204);

    // The slow client reads what reached it, in order, and then is told why.
    loop {
        match client_next(&mut slow) {
            ClientEnvelope::Data { payload } => assert_eq!(payload.len(), chunk.len()),
            ClientEnvelope::Closed { reason } => {
                assert_eq!(reason, CloseReason::LimitExceeded);
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    slow.expect_close(close_code::LIMIT_EXCEEDED);
}

/// The first eight bytes of a frame of the host-queue test: its number.
fn numbered(n: u64) -> Vec<u8> {
    let mut bytes = vec![0xABu8; 60_000];
    bytes[..8].copy_from_slice(&n.to_be_bytes());
    bytes
}

#[test]
fn a_full_host_queue_makes_the_client_wait_and_closes_nothing() {
    // The host queue is the one limit that is backpressure, not a close
    // (architecture §5): a slow host delays its clients and loses nothing.
    const FRAMES: u64 = 500;
    let (server, host, mut host_ws) = start_limited(|limits| {
        limits.host_queue_frames = 1;
        limits.channel_bytes_per_second = UNLIMITED_RATE;
    });
    let (mut client, channel) = attach(&server, &host, &mut host_ws);

    // 30 MB: more than the socket buffers on the way (client, relay, host)
    // and the one-frame queue can hold, while the host reads nothing.
    let sender = std::thread::spawn(move || {
        for n in 0..FRAMES {
            client.send_binary(client_data(&numbered(n)));
        }
        client
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !sender.is_finished(),
        "the client was not made to wait: the relay is buffering without a limit or dropping"
    );

    // The host wakes up: every frame arrives, in order, and none is a close.
    for n in 0..FRAMES {
        let (from, bytes) = host_reads_data(&mut host_ws);
        assert_eq!(from, channel);
        assert_eq!(bytes[..8], n.to_be_bytes(), "frame {n} out of order");
        assert_eq!(bytes.len(), 60_000);
    }
    let mut client = sender.join().unwrap();
    // The channel is still open both ways.
    host_ws.send_binary(host_data(channel, b"still open"));
    assert_eq!(client_reads_data(&mut client), b"still open");
    assert_eq!(server.presence(host.host_id().as_str()), 204);
}
