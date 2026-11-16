// SPDX-License-Identifier: GPL-3.0-or-later

//! The blind log (PRD-001 RF-1.18, ADR-0002): a whole session with a known
//! marker in the `payload`, at the most verbose level, and the marker must not
//! show up in the output, whatever the way it is written. Every field of every
//! JSON log line must be one of the allowed ones.

mod common;

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use common::{
    FakeHost, TOKEN, TestServer, attach, client_data, client_reads_data, host_data, host_next,
    host_reads_data,
};
use porecatu_protocol::envelope::{ChannelId, CloseReason, HostEnvelope};
use porecatu_relay::logging::{CapturedLog, LogFormat, LogLevel};

/// What the client sends to the host.
const UP: &[u8] = b"ZQ7-up-marker-5ca1ab1e-do-not-log";
/// What the host sends to the client.
const DOWN: &[u8] = b"ZQ7-down-marker-0ddba11-do-not-log";
/// A token that is not configured: a refused attempt is logged too.
const BAD_TOKEN: &str = "bad-operator-token-ZQ7-wrong-0123456789";

/// The fields a log line may carry: the ones of PRD-001 RF-1.18 plus the ones
/// `tracing` itself puts in a line.
const ALLOWED_FIELDS: &[&str] = &[
    "host_id",
    "channel",
    "token_name",
    "reason",
    "bytes_in",
    "bytes_out",
    "duration_ms",
    "peer",
    "timestamp",
    "level",
    "target",
    "fields",
    "message",
    "event",
];

/// Runs a session through a relay logging at `trace` in `format` and returns
/// what it logged.
fn session_log(format: LogFormat) -> String {
    let log = CapturedLog::new();
    let server = TestServer::start_logged(log.clone(), LogLevel::Trace, format);

    // A refused host first: the token must not be logged.
    assert_eq!(server.connect_host(Some(BAD_TOKEN)).err(), Some(401));

    let host = FakeHost::new(1);
    let mut host_ws = host.connect_and_register(&server);
    let (mut client, channel) = attach(&server, &host, &mut host_ws);

    client.send_binary(client_data(UP));
    assert_eq!(host_reads_data(&mut host_ws), (channel, UP.to_vec()));
    host_ws.send_binary(host_data(channel, DOWN));
    assert_eq!(client_reads_data(&mut client), DOWN);

    client.close_normally();
    assert_eq!(
        host_next(&mut host_ws),
        HostEnvelope::ChannelClosed {
            channel: ChannelId(channel),
            reason: CloseReason::Normal,
        }
    );
    drop(host_ws);
    server.wait_presence(host.host_id().as_str(), 404);

    // The tasks log as they end: wait for the last two lines.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !(log.contents().contains("channel closed")
        && log.contents().contains("host disconnected"))
    {
        assert!(
            Instant::now() < deadline,
            "the session never finished logging"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(server);
    log.contents()
}

/// Every way of writing `marker` that a careless log could produce.
fn spellings(marker: &[u8]) -> Vec<String> {
    let lower: String = marker.iter().map(|byte| format!("{byte:02x}")).collect();
    let spaced: String = marker
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut all = vec![
        String::from_utf8_lossy(marker).into_owned(),
        lower.to_uppercase(),
        lower,
        spaced.to_uppercase(),
        spaced,
        format!("{marker:?}"),
        format!("{:?}", marker.to_vec()),
        STANDARD.encode(marker),
        STANDARD_NO_PAD.encode(marker),
        URL_SAFE.encode(marker),
        URL_SAFE_NO_PAD.encode(marker),
    ];
    // A truncated payload is a payload too.
    let prefix = &marker[..8];
    let lower_prefix: String = prefix.iter().map(|byte| format!("{byte:02x}")).collect();
    all.push(String::from_utf8_lossy(prefix).into_owned());
    all.push(lower_prefix.to_uppercase());
    all.push(lower_prefix);
    all
}

#[track_caller]
fn assert_blind(log: &str) {
    assert!(
        log.contains("host registered") && log.contains("channel opened"),
        "the session left no log at trace: {log}"
    );
    for marker in [UP, DOWN] {
        for spelling in spellings(marker) {
            assert!(
                !log.contains(&spelling),
                "the log shows the payload as {spelling:?}"
            );
        }
    }
    for secret in [TOKEN, BAD_TOKEN, "Bearer", "authorization"] {
        assert!(
            !log.to_lowercase().contains(&secret.to_lowercase()),
            "the log shows {secret:?}"
        );
    }
}

#[test]
fn the_text_log_never_shows_the_payload() {
    assert_blind(&session_log(LogFormat::Text));
}

#[test]
fn the_json_log_never_shows_the_payload_and_has_only_allowed_fields() {
    let log = session_log(LogFormat::Json);
    assert_blind(&log);

    let mut lines = 0;
    for line in log.lines().filter(|line| !line.trim().is_empty()) {
        lines += 1;
        assert!(
            line.starts_with('{') && line.ends_with('}'),
            "not a JSON object: {line}"
        );
        for key in object_keys(line) {
            assert!(
                ALLOWED_FIELDS.contains(&key.as_str()),
                "the field {key:?} is not allowed in a log line: {line}"
            );
        }
    }
    assert!(lines >= 5, "too few log lines: {lines}");
}

/// Every object key in a JSON text: a string followed by a colon. Only meant
/// for the flat, well-formed lines `tracing` writes.
fn object_keys(json: &str) -> Vec<String> {
    let chars: Vec<char> = json.chars().collect();
    let mut keys = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        let mut text = String::new();
        i += 1;
        while i < chars.len() && chars[i] != '"' {
            if chars[i] == '\\' {
                i += 1;
            }
            if let Some(c) = chars.get(i) {
                text.push(*c);
            }
            i += 1;
        }
        i += 1; // the closing quote
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if chars.get(j) == Some(&':') {
            keys.push(text);
        }
    }
    keys
}

#[test]
fn the_key_scanner_finds_keys_and_not_values() {
    let line = r#"{"timestamp":"a:b","fields":{"message":"x \" y: z","channel":3},"target":"t"}"#;
    assert_eq!(
        object_keys(line),
        ["timestamp", "fields", "message", "channel", "target"]
    );
}
