// SPDX-License-Identifier: GPL-3.0-or-later

//! Graceful shutdown (PRD-001 RF-1.22): cancelling the token closes hosts with
//! `1001` and clients with `closed { normal }` and `1001`, and the server stops
//! within the grace period even when a peer does not answer the close.

mod common;

use std::time::{Duration, Instant};

use common::{FakeHost, TestServer, attach, expect_closed, start};
use porecatu_protocol::envelope::{CloseReason, close_code};
use porecatu_relay::server::Timing;
use porecatu_relay::shutdown::Outcome;

#[test]
fn cancelling_the_token_closes_hosts_with_1001_and_clients_with_closed_normal() {
    let (server, host, mut host_ws) = start();
    let (mut client, _channel) = attach(&server, &host, &mut host_ws);

    server.shut_down();

    expect_closed(&mut client, CloseReason::Normal, close_code::GOING_AWAY);
    host_ws.expect_close(close_code::GOING_AWAY);
    // Both peers answered the close: nothing keeps the server up.
    assert_eq!(
        server.wait_stopped(Duration::from_secs(5)),
        Some(Outcome::Clean)
    );
}

#[test]
fn a_host_still_waiting_to_register_gets_1001_too() {
    let server = TestServer::start();
    let mut host_ws = server.connect_host(Some(common::TOKEN)).unwrap();
    FakeHost::read_challenge(&mut host_ws);

    server.shut_down();

    host_ws.expect_close(close_code::GOING_AWAY);
    assert_eq!(
        server.wait_stopped(Duration::from_secs(5)),
        Some(Outcome::Clean)
    );
}

#[test]
fn a_peer_that_does_not_answer_the_close_does_not_hold_the_relay_past_the_grace_period() {
    let grace = Duration::from_millis(300);
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.server.shutdown_grace = grace;
    });
    let host = FakeHost::new(1);
    // The host never reads again, so it never answers the `Close`.
    let _silent = host.connect_and_register(&server);

    let started = Instant::now();
    server.shut_down();
    let outcome = server.wait_stopped(Duration::from_secs(5));
    let elapsed = started.elapsed();

    assert_eq!(outcome, Some(Outcome::GraceElapsed));
    assert!(
        elapsed >= grace,
        "left before the grace period: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1800),
        "waited for the peer instead of the grace period: {elapsed:?}"
    );
}
