// SPDX-License-Identifier: GPL-3.0-or-later

//! Host registration, presence and health over a real socket
//! (specification 01 §1 to §5; PRD-001).

mod common;

use std::time::{Duration, Instant};

use common::{Event, FakeHost, TOKEN, TestServer, WAIT};
use porecatu_protocol::envelope::{self, HostEnvelope, MAX_FRAME, close_code};
use porecatu_relay::server::Timing;

const REGISTERED_OTHER_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

// ---- token ----------------------------------------------------------------

#[test]
fn missing_or_wrong_token_is_401_before_the_upgrade() {
    let server = TestServer::start();
    assert_eq!(server.connect_host(None).err(), Some(401));
    assert_eq!(server.connect_host(Some("not-the-token")).err(), Some(401));
    // Not the Bearer scheme, and Bearer with no token.
    assert_eq!(
        server.connect_host_raw(&format!("Basic {TOKEN}")).err(),
        Some(401)
    );
    assert_eq!(server.connect_host_raw("Bearer ").err(), Some(401));
    // Nothing registered, nothing leaked.
    assert!(server.state.registry().is_empty());
}

#[test]
fn the_bearer_scheme_is_case_insensitive() {
    let server = TestServer::start();
    let mut ws = server
        .connect_host_raw(&format!("bearer {TOKEN}"))
        .expect("a lowercase scheme is accepted");
    FakeHost::read_challenge(&mut ws);
}

// ---- registration -----------------------------------------------------------

#[test]
fn valid_registration_answers_registered_with_the_right_host_id() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let id = host.host_id();
    assert_eq!(server.presence(id.as_str()), 404);

    let ws = host.connect_and_register(&server);
    assert_eq!(server.presence(id.as_str()), 204);
    assert_eq!(server.state.registry().len(), 1);

    // Leaving takes the host out of the registry.
    drop(ws);
    server.wait_presence(id.as_str(), 404);
    assert!(server.state.registry().is_empty());
}

#[test]
fn a_signature_by_another_key_closes_4001() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let impostor = FakeHost::new(2);
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    let nonce = FakeHost::read_challenge(&mut ws);

    // The impostor's signature under the victim's routing key.
    let forged = impostor.register_frame(&nonce);
    let Ok(HostEnvelope::Register { signature, .. }) = envelope::decode_host(forged.into()) else {
        panic!("not a register");
    };
    ws.send_binary(envelope::encode_host(&HostEnvelope::Register {
        routing_key: host.routing_key(),
        signature,
    }));
    ws.expect_close(close_code::REGISTER_REJECTED);
    assert_eq!(server.presence(host.host_id().as_str()), 404);
}

#[test]
fn a_signature_over_another_nonce_closes_4001() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    // A valid registration, but for a challenge this connection never got.
    ws.send_binary(host.register_frame(&[7; 32]));
    ws.expect_close(close_code::REGISTER_REJECTED);
}

#[test]
fn a_routing_key_that_is_not_a_point_closes_4001() {
    let server = TestServer::start();
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    // y = 2 is not on the curve.
    let mut key = [0u8; 32];
    key[0] = 2;
    ws.send_binary(envelope::encode_host(&HostEnvelope::Register {
        routing_key: key,
        signature: [0; 64],
    }));
    ws.expect_close(close_code::REGISTER_REJECTED);
}

#[test]
fn no_register_in_time_closes_4005() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.register_timeout = Duration::from_millis(300);
    });
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    let started = Instant::now();
    ws.expect_close(close_code::REGISTER_TIMEOUT);
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_secs(3),
        "closed after {waited:?}"
    );
}

#[test]
fn garbage_closes_4004() {
    let server = TestServer::start();
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    ws.send_binary(vec![0xff, 0x00, 0x13]);
    ws.expect_close(close_code::INVALID_ENVELOPE);
}

#[test]
fn an_envelope_of_the_wrong_kind_closes_4004() {
    let server = TestServer::start();
    let host = FakeHost::new(1);

    // A relay-to-host envelope, sent by the host.
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    ws.send_binary(envelope::encode_host(&HostEnvelope::Registered {
        host_id: host.host_id(),
    }));
    ws.expect_close(close_code::INVALID_ENVELOPE);

    // A host envelope that is valid, but not the registration.
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    ws.send_binary(envelope::encode_host(&HostEnvelope::CloseChannel {
        channel: envelope::ChannelId(0),
        reason: envelope::CloseReason::Normal,
    }));
    ws.expect_close(close_code::INVALID_ENVELOPE);
}

#[test]
fn a_second_register_after_registered_closes_4004() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    ws.send_binary(host.register_frame(&[0; 32]));
    ws.expect_close(close_code::INVALID_ENVELOPE);
    server.wait_presence(host.host_id().as_str(), 404);
}

#[test]
fn a_channel_that_was_never_opened_closes_4004() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    // The relay opens no channel yet, so any number is one it never gave.
    ws.send_binary(envelope::encode_host(&HostEnvelope::CloseChannel {
        channel: envelope::ChannelId(5),
        reason: envelope::CloseReason::Normal,
    }));
    ws.expect_close(close_code::INVALID_ENVELOPE);
}

#[test]
fn a_text_frame_closes_1003() {
    let server = TestServer::start();
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    ws.send_text("hello");
    ws.expect_close(close_code::UNSUPPORTED_DATA);

    // The same after the registration.
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    ws.send_text("hello");
    ws.expect_close(close_code::UNSUPPORTED_DATA);
}

#[test]
fn a_frame_over_the_maximum_closes_1009() {
    let server = TestServer::start();
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    ws.send_binary(vec![0; MAX_FRAME + 1]);
    ws.expect_close(close_code::MESSAGE_TOO_BIG);

    // The same after the registration, and registered hosts leave the registry.
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    ws.send_binary(vec![0; MAX_FRAME * 2]);
    ws.expect_close(close_code::MESSAGE_TOO_BIG);
    server.wait_presence(host.host_id().as_str(), 404);
}

#[test]
fn a_frame_of_exactly_the_maximum_is_read_and_judged_by_its_content() {
    let server = TestServer::start();
    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut ws);
    // Not too big for the transport; it is just not an envelope.
    ws.send_binary(vec![0; MAX_FRAME]);
    ws.expect_close(close_code::INVALID_ENVELOPE);
}

// ---- replacement and capacity ---------------------------------------------------

#[test]
fn a_newer_registration_replaces_the_older_with_4002() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        // A replacement does not count against the limit.
        settings.limits.max_hosts = 1;
    });
    let host = FakeHost::new(1);
    let id = host.host_id();

    let mut first = host.connect_and_register(&server);
    let mut second = host.connect_and_register(&server);

    first.expect_close(close_code::REPLACED);
    // The old task cleaning up must not take the new registration with it.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(server.presence(id.as_str()), 204);
    assert_eq!(server.state.registry().len(), 1);
    assert_eq!(
        second.next_within(Duration::from_millis(100)),
        Event::Silence
    );

    // When the current one leaves, the host is gone.
    drop(second);
    server.wait_presence(id.as_str(), 404);
}

#[test]
fn a_full_registry_closes_4003_for_a_new_host_only() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.max_hosts = 1;
    });
    let resident = FakeHost::new(1);
    let newcomer = FakeHost::new(2);

    let _resident_ws = resident.connect_and_register(&server);

    let mut ws = server.connect_host(Some(TOKEN)).unwrap();
    let nonce = FakeHost::read_challenge(&mut ws);
    ws.send_binary(newcomer.register_frame(&nonce));
    ws.expect_close(close_code::LIMIT_EXCEEDED);
    assert_eq!(server.presence(newcomer.host_id().as_str()), 404);
    assert_eq!(server.presence(resident.host_id().as_str()), 204);

    // Once the resident leaves, there is room.
    drop(_resident_ws);
    server.wait_presence(resident.host_id().as_str(), 404);
    let _newcomer_ws = newcomer.connect_and_register(&server);
    assert_eq!(server.presence(newcomer.host_id().as_str()), 204);
}

// ---- keepalive ----------------------------------------------------------------------

#[test]
fn a_host_that_never_answers_pings_is_dropped_without_a_code() {
    let timing = Timing {
        ping_interval: Duration::from_millis(100),
        pong_timeout: Duration::from_millis(500),
    };
    let server = TestServer::start_with(timing, |_| {});
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    let id = host.host_id();
    assert_eq!(server.presence(id.as_str()), 204);

    // The client does not read, so it does not answer.
    std::thread::sleep(Duration::from_millis(1200));
    server.wait_presence(id.as_str(), 404);
    // What it finds on the wire is a dropped connection, not a Close frame.
    let mut saw_close_frame = false;
    let deadline = Instant::now() + WAIT;
    loop {
        match ws.next() {
            Event::Closed(_) => {
                saw_close_frame = true;
                break;
            }
            Event::Dropped => break,
            Event::Silence | Event::Binary(_) => {}
        }
        assert!(Instant::now() < deadline, "the connection never ended");
    }
    assert!(!saw_close_frame, "the relay sent a Close frame");
}

#[test]
fn a_host_that_answers_pings_stays_registered() {
    let timing = Timing {
        ping_interval: Duration::from_millis(100),
        pong_timeout: Duration::from_millis(500),
    };
    let server = TestServer::start_with(timing, |_| {});
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    let id = host.host_id();

    // Reading is what makes the library answer each ping with a pong.
    let until = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < until {
        assert_eq!(ws.next_within(Duration::from_millis(100)), Event::Silence);
    }
    assert_eq!(server.presence(id.as_str()), 204);
}

// ---- shutdown --------------------------------------------------------------------------

#[test]
fn shutdown_closes_host_connections_with_1001() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let mut ws = host.connect_and_register(&server);
    let mut waiting = server.connect_host(Some(TOKEN)).unwrap();
    FakeHost::read_challenge(&mut waiting);

    server.shut_down();
    ws.expect_close(close_code::GOING_AWAY);
    waiting.expect_close(close_code::GOING_AWAY);
}

// ---- presence, health, client route --------------------------------------------------

#[test]
fn presence_is_204_404_or_400_and_has_no_body() {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let id = host.host_id();

    assert_eq!(
        server.http_get(&format!("/v0/presence/{id}")),
        (404, String::new())
    );
    let _ws = host.connect_and_register(&server);
    assert_eq!(
        server.http_get(&format!("/v0/presence/{id}")),
        (204, String::new())
    );
    assert_eq!(server.presence(REGISTERED_OTHER_ID), 404);

    // Wrong length, wrong alphabet, upper case.
    for bad in [
        "short",
        &"a".repeat(33),
        &"1".repeat(32),
        &id.as_str().to_uppercase(),
    ] {
        assert_eq!(server.presence(bad), 400, "{bad}");
    }
}

#[test]
fn healthz_answers_200_ok() {
    let server = TestServer::start();
    assert_eq!(server.http_get("/healthz"), (200, "ok".to_owned()));
}

#[test]
fn the_client_route_answers_host_unavailable_when_nobody_is_there() {
    let server = TestServer::start();
    assert_eq!(server.connect_client("short").err(), Some(400));

    let mut ws = server.connect_client(REGISTERED_OTHER_ID).unwrap();
    let Event::Binary(frame) = ws.next() else {
        panic!("expected host-unavailable");
    };
    assert_eq!(
        frame,
        envelope::encode_client(&envelope::ClientEnvelope::HostUnavailable)
    );
    ws.expect_close(close_code::NORMAL);
}

// ---- rate limits ---------------------------------------------------------------------------

#[test]
fn host_attempts_are_limited_per_ip_before_the_token_is_checked() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.host_attempts_per_ip_per_minute = 2;
    });
    assert_eq!(server.connect_host(Some("wrong")).err(), Some(401));
    assert_eq!(server.connect_host(Some("wrong")).err(), Some(401));
    // The limit comes first, whatever the token.
    assert_eq!(server.connect_host(Some("wrong")).err(), Some(429));
    assert_eq!(server.connect_host(Some(TOKEN)).err(), Some(429));
}

#[test]
fn client_connections_are_limited_per_ip() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.client_connects_per_ip_per_minute = 2;
    });
    for _ in 0..2 {
        let mut ws = server.connect_client(REGISTERED_OTHER_ID).unwrap();
        assert!(matches!(ws.next(), Event::Binary(_)));
    }
    assert_eq!(server.connect_client(REGISTERED_OTHER_ID).err(), Some(429));
}

#[test]
fn presence_queries_are_limited_per_ip() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.presence_per_minute_per_ip = 2;
    });
    assert_eq!(server.presence(REGISTERED_OTHER_ID), 404);
    assert_eq!(server.presence(REGISTERED_OTHER_ID), 404);
    assert_eq!(server.presence(REGISTERED_OTHER_ID), 429);
    // A malformed id is refused before it costs a slot of anybody's.
    assert_eq!(server.presence("short"), 400);
    // Health is not limited.
    assert_eq!(server.http_get("/healthz").0, 200);
}

#[test]
fn the_limit_follows_the_forwarded_address_behind_a_trusted_proxy() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.presence_per_minute_per_ip = 1;
        settings.server.trusted_proxies = vec!["127.0.0.1/32".parse().unwrap()];
    });
    let path = format!("/v0/presence/{REGISTERED_OTHER_ID}");
    let from = |ip: &str| server.http_get_with(&path, &[("X-Forwarded-For", ip)]).0;
    assert_eq!(from("203.0.113.1"), 404);
    assert_eq!(from("203.0.113.1"), 429);
    // Another client behind the same proxy has its own allowance.
    assert_eq!(from("203.0.113.2"), 404);
    // A client cannot dodge the limit by prepending addresses of its own: the
    // rightmost one that is not a trusted proxy counts.
    assert_eq!(from("198.51.100.9, 203.0.113.1"), 429);
}

#[test]
fn forwarded_for_from_an_untrusted_socket_is_ignored() {
    let server = TestServer::start_with(Timing::default(), |settings| {
        settings.limits.presence_per_minute_per_ip = 1;
    });
    let path = format!("/v0/presence/{REGISTERED_OTHER_ID}");
    let from = |ip: &str| server.http_get_with(&path, &[("X-Forwarded-For", ip)]).0;
    assert_eq!(from("203.0.113.1"), 404);
    assert_eq!(from("203.0.113.2"), 429);
}
