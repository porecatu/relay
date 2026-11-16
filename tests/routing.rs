// SPDX-License-Identifier: GPL-3.0-or-later

//! Client channels and the routing of `payload`s over real sockets
//! (specification 01 §4; PRD-001).

mod common;

use std::time::Duration;

use common::{
    Event, FakeHost, TestServer, attach, client_data, client_next, client_reads_data,
    expect_closed, host_close, host_data, host_next, host_reads_data, start,
};
use porecatu_protocol::envelope::{
    self, ChannelId, ClientEnvelope, CloseReason, HostEnvelope, close_code,
};
use porecatu_relay::server::Timing;

const SHORT: Duration = Duration::from_millis(300);

// ---- the cycle -------------------------------------------------------------------

#[test]
fn a_channel_goes_through_its_whole_cycle() {
    let (server, host, mut host_ws) = start();
    let (mut client, channel) = attach(&server, &host, &mut host_ws);
    assert_eq!(channel, 0);

    client.send_binary(client_data(b"hello host"));
    assert_eq!(host_reads_data(&mut host_ws), (0, b"hello host".to_vec()));

    host_ws.send_binary(host_data(0, b"hello client"));
    assert_eq!(client_reads_data(&mut client), b"hello client");

    // The client leaves; the host hears it, and the host stays registered.
    client.close_normally();
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::Normal,
        }
    );
    assert_eq!(server.presence(host.host_id().as_str()), 204);
}

#[test]
fn a_client_that_just_drops_the_socket_is_a_normal_close_for_the_host() {
    let (server, host, mut host_ws) = start();
    let (client, _) = attach(&server, &host, &mut host_ws);
    drop(client);
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::Normal,
        }
    );
}

#[test]
fn one_thousand_frames_arrive_in_order_client_to_host() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    // Sent all at once: the relay waits for room instead of dropping.
    for n in 0u32..1000 {
        client.send_binary(client_data(&n.to_be_bytes()));
    }
    for n in 0u32..1000 {
        let (channel, bytes) = host_reads_data(&mut host_ws);
        assert_eq!(channel, 0);
        assert_eq!(bytes, n.to_be_bytes(), "frame {n} out of order");
    }
}

#[test]
fn one_thousand_frames_arrive_in_order_host_to_client() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    // In batches under the queue of the channel (64 frames): a client slower
    // than that would have its channel closed, which is another test.
    for batch in 0u32..40 {
        for n in 0..25 {
            host_ws.send_binary(host_data(0, &(batch * 25 + n).to_be_bytes()));
        }
        for n in 0..25 {
            assert_eq!(
                client_reads_data(&mut client),
                (batch * 25 + n).to_be_bytes(),
                "frame {} out of order",
                batch * 25 + n
            );
        }
    }
}

#[test]
fn both_directions_at_once_keep_their_order() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);

    // The host side in a thread of its own, so both directions are busy while
    // the main thread reads and writes the client.
    let host_thread = std::thread::spawn(move || {
        for batch in 0u32..20 {
            for n in 0..25 {
                host_ws.send_binary(host_data(0, &(batch * 25 + n).to_be_bytes()));
            }
            for n in 0..25 {
                let (_, bytes) = host_reads_data(&mut host_ws);
                assert_eq!(bytes, (batch * 25 + n).to_be_bytes());
            }
        }
        host_ws
    });
    for batch in 0u32..20 {
        for n in 0..25 {
            client.send_binary(client_data(&(batch * 25 + n).to_be_bytes()));
        }
        for n in 0..25 {
            assert_eq!(
                client_reads_data(&mut client),
                (batch * 25 + n).to_be_bytes()
            );
        }
    }
    let _host_ws = host_thread.join().unwrap();
}

#[test]
fn two_clients_on_one_host_do_not_mix() {
    let (server, host, mut host_ws) = start();
    let (mut first, a) = attach(&server, &host, &mut host_ws);
    let (mut second, b) = attach(&server, &host, &mut host_ws);
    assert_eq!((a, b), (0, 1));

    first.send_binary(client_data(b"from first"));
    second.send_binary(client_data(b"from second"));
    let mut seen = [host_reads_data(&mut host_ws), host_reads_data(&mut host_ws)];
    seen.sort();
    assert_eq!(
        seen,
        [(0, b"from first".to_vec()), (1, b"from second".to_vec())]
    );

    host_ws.send_binary(host_data(1, b"to second"));
    host_ws.send_binary(host_data(0, b"to first"));
    assert_eq!(client_reads_data(&mut second), b"to second");
    assert_eq!(client_reads_data(&mut first), b"to first");
    // Nothing else leaked to either.
    assert_eq!(first.next_within(SHORT), Event::Silence);
    assert_eq!(second.next_within(SHORT), Event::Silence);

    // One leaves; the other is not touched.
    drop(first);
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::Normal,
        }
    );
    second.send_binary(client_data(b"still here"));
    assert_eq!(host_reads_data(&mut host_ws), (1, b"still here".to_vec()));
}

#[test]
fn a_frame_of_the_largest_forwardable_size_goes_through() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    // 69 992 bytes with channel 0 make a `channel-data` of exactly MAX_FRAME.
    let big = vec![7u8; 69_992];
    client.send_binary(client_data(&big));
    let (_, bytes) = host_reads_data(&mut host_ws);
    assert_eq!(bytes, big);
    host_ws.send_binary(host_data(0, &big));
    assert_eq!(client_reads_data(&mut client), big);
}

// ---- the host closes -----------------------------------------------------------------

#[test]
fn close_channel_of_the_host_closes_the_client_after_what_was_sent() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);

    host_ws.send_binary(host_data(0, b"last words"));
    host_ws.send_binary(host_close(0, CloseReason::HostRejected));
    // The data sent before the close still arrives, then the close.
    assert_eq!(client_reads_data(&mut client), b"last words");
    expect_closed(&mut client, CloseReason::HostRejected, close_code::NORMAL);

    // The relay does not answer a `close-channel` with `channel-closed`, and
    // the host stays registered.
    assert_eq!(host_ws.next_within(SHORT), Event::Silence);
    assert_eq!(server.presence(host.host_id().as_str()), 204);

    // A closed channel is never reopened or reused: the next client gets 1.
    let (_second, channel) = attach(&server, &host, &mut host_ws);
    assert_eq!(channel, 1);
}

#[test]
fn every_close_reason_of_the_host_reaches_the_client_with_its_code() {
    let (server, host, mut host_ws) = start();
    for (n, (reason, code)) in [
        (CloseReason::Normal, close_code::NORMAL),
        (CloseReason::HostRejected, close_code::NORMAL),
        (CloseReason::LimitExceeded, close_code::LIMIT_EXCEEDED),
        (CloseReason::ProtocolError, close_code::INVALID_ENVELOPE),
    ]
    .into_iter()
    .enumerate()
    {
        let (mut client, channel) = attach(&server, &host, &mut host_ws);
        assert_eq!(channel, n as u64);
        host_ws.send_binary(host_close(channel, reason));
        expect_closed(&mut client, reason, code);
    }
}

#[test]
fn a_host_that_drops_gives_host_gone_to_every_client() {
    let (server, host, mut host_ws) = start();
    let (mut first, _) = attach(&server, &host, &mut host_ws);
    let (mut second, _) = attach(&server, &host, &mut host_ws);

    drop(host_ws);
    expect_closed(&mut first, CloseReason::HostGone, close_code::NORMAL);
    expect_closed(&mut second, CloseReason::HostGone, close_code::NORMAL);
    server.wait_presence(host.host_id().as_str(), 404);
}

#[test]
fn a_host_that_breaks_the_protocol_gives_host_gone_to_its_clients() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    host_ws.send_binary(vec![0xff, 0x00, 0x13]);
    host_ws.expect_close(close_code::INVALID_ENVELOPE);
    expect_closed(&mut client, CloseReason::HostGone, close_code::NORMAL);
}

#[test]
fn a_replaced_host_gives_host_replaced_and_the_next_client_goes_to_the_new_one() {
    let (server, host, mut old_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut old_ws);

    let mut new_ws = host.connect_and_register(&server);
    old_ws.expect_close(close_code::REPLACED);
    expect_closed(&mut client, CloseReason::HostReplaced, close_code::NORMAL);

    // The new connection has its own numbering and its own channels.
    let (mut fresh, channel) = attach(&server, &host, &mut new_ws);
    assert_eq!(channel, 0);
    fresh.send_binary(client_data(b"to the new one"));
    assert_eq!(
        host_reads_data(&mut new_ws),
        (0, b"to the new one".to_vec())
    );
}

#[test]
fn shutdown_gives_closed_normal_and_1001_to_clients() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);

    server.shut_down();
    expect_closed(&mut client, CloseReason::Normal, close_code::GOING_AWAY);
    host_ws.expect_close(close_code::GOING_AWAY);
}

// ---- the client closes ---------------------------------------------------------------

#[test]
fn a_client_with_no_host_gets_host_unavailable() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let mut ws = server.connect_client(host.host_id().as_str()).unwrap();
    assert_eq!(client_next(&mut ws), ClientEnvelope::HostUnavailable);
    ws.expect_close(close_code::NORMAL);
}

// ---- closed and never-opened channels ----------------------------------------------------

#[test]
fn data_for_a_channel_the_client_already_closed_is_ignored() {
    let (server, host, mut host_ws) = start();
    let (client, _) = attach(&server, &host, &mut host_ws);
    drop(client);
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::Normal,
        }
    );

    // The host raced with the close: both of these are ignored, no error.
    host_ws.send_binary(host_data(0, b"too late"));
    host_ws.send_binary(host_close(0, CloseReason::Normal));
    assert_eq!(host_ws.next_within(SHORT), Event::Silence);
    assert_eq!(server.presence(host.host_id().as_str()), 204);

    // And the connection still works.
    let (mut next, channel) = attach(&server, &host, &mut host_ws);
    assert_eq!(channel, 1);
    next.send_binary(client_data(b"ok"));
    assert_eq!(host_reads_data(&mut host_ws), (1, b"ok".to_vec()));
}

#[test]
fn data_for_a_channel_never_opened_closes_the_host_with_4004() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    // Channel 0 is live; 1 is the next number the relay would give.
    host_ws.send_binary(host_data(1, b"who?"));
    host_ws.expect_close(close_code::INVALID_ENVELOPE);
    expect_closed(&mut client, CloseReason::HostGone, close_code::NORMAL);
}

// ---- wrong things from the client -------------------------------------------------------------

#[test]
fn an_envelope_that_is_not_data_closes_the_channel_with_protocol_error() {
    for frame in [
        // Valid envelopes of the relay's own direction.
        envelope::encode_client(&ClientEnvelope::HostUnavailable),
        envelope::encode_client(&ClientEnvelope::Closed {
            reason: CloseReason::Normal,
        }),
        // A host envelope.
        host_data(0, b"x"),
        // Not an envelope at all.
        vec![0xff, 0x00, 0x13],
        // Not canonical: a non-minimal integer.
        vec![0x82, 0x18, 0x10, 0x40],
    ] {
        let (server, host, mut host_ws) = start();
        let (mut client, _) = attach(&server, &host, &mut host_ws);
        client.send_binary(frame);
        expect_closed(
            &mut client,
            CloseReason::ProtocolError,
            close_code::INVALID_ENVELOPE,
        );
        assert_eq!(
            host_next(&mut host_ws),
            HostEnvelope::ChannelClosed {
                channel: ChannelId(0),
                reason: CloseReason::ProtocolError,
            }
        );
        // The host is not the one at fault.
        assert_eq!(server.presence(host.host_id().as_str()), 204);
    }
}

#[test]
fn a_text_frame_from_the_client_closes_1003_without_closed() {
    let (server, host, mut host_ws) = start();
    let (mut client, _) = attach(&server, &host, &mut host_ws);
    client.send_text("hello");
    client.expect_close(close_code::UNSUPPORTED_DATA);
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::ProtocolError,
        }
    );
}

// ---- keepalive ------------------------------------------------------------------------------------------

#[test]
fn a_client_that_never_answers_pings_is_dropped_and_the_host_hears_normal() {
    let timing = Timing {
        ping_interval: Duration::from_millis(100),
        pong_timeout: Duration::from_millis(500),
    };
    let server = TestServer::start_with(timing, |_| {});
    let host = FakeHost::new(1);
    let mut host_ws = host.connect_and_register(&server);
    let (mut client, _) = attach(&server, &host, &mut host_ws);

    // The client does not read, so it does not answer. The host keeps reading,
    // which answers its own pings.
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(0),
            reason: CloseReason::Normal,
        }
    );
    // What the client finds is a dropped connection, not a Close frame.
    loop {
        match client.next() {
            Event::Closed(code) => panic!("the relay sent a Close frame: {code:?}"),
            Event::Dropped => break,
            Event::Silence | Event::Binary(_) => {}
        }
    }
    assert_eq!(server.presence(host.host_id().as_str()), 204);
}

#[test]
fn a_client_that_answers_pings_stays() {
    let timing = Timing {
        ping_interval: Duration::from_millis(100),
        pong_timeout: Duration::from_millis(500),
    };
    let server = TestServer::start_with(timing, |_| {});
    let host = FakeHost::new(1);
    let mut host_ws = host.connect_and_register(&server);
    let (mut client, _) = attach(&server, &host, &mut host_ws);

    // Both peers read in turn, which makes the library answer the pings.
    let until = std::time::Instant::now() + Duration::from_millis(1500);
    while std::time::Instant::now() < until {
        assert_eq!(
            client.next_within(Duration::from_millis(50)),
            Event::Silence
        );
        assert_eq!(
            host_ws.next_within(Duration::from_millis(50)),
            Event::Silence
        );
    }
    client.send_binary(client_data(b"alive"));
    assert_eq!(host_reads_data(&mut host_ws), (0, b"alive".to_vec()));
}
