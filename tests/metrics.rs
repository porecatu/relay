// SPDX-License-Identifier: GPL-3.0-or-later

//! `GET /metrics` (PRD-001 RF-1.21): the series exist, the numbers follow what
//! went through the relay, and no label can identify a host, a peer, a token or
//! a channel.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{
    TOKEN, TestServer, attach, client_data, client_reads_data, host_close, host_data,
    host_reads_data, start,
};
use porecatu_protocol::envelope::CloseReason;
use porecatu_relay::metrics;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

/// The metrics listener of a [`TestServer`], on a runtime of its own.
struct MetricsListener {
    addr: SocketAddr,
    shutdown: CancellationToken,
    runtime: Option<Runtime>,
}

impl MetricsListener {
    fn start(server: &TestServer) -> Self {
        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        runtime.spawn(metrics::serve(
            listener,
            Arc::clone(&server.state),
            shutdown.clone(),
        ));
        Self {
            addr,
            shutdown,
            runtime: Some(runtime),
        }
    }

    /// A plain `GET`: the status line, the headers and the body.
    fn get(&self, path: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: metrics\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {response:?}"));
        let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
        (status, head.to_owned(), body.to_owned())
    }

    fn scrape(&self) -> String {
        let (status, _, body) = self.get("/metrics");
        assert_eq!(status, 200, "{body}");
        body
    }

    /// Scrapes until `ready` accepts the text; panics after five seconds.
    #[track_caller]
    fn scrape_until(&self, ready: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let text = self.scrape();
            if ready(&text) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "the metrics never settled:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for MetricsListener {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

#[test]
fn the_endpoint_answers_in_the_prometheus_text_format() {
    let server = TestServer::start();
    let metrics = MetricsListener::start(&server);

    let (status, head, body) = metrics.get("/metrics");
    assert_eq!(status, 200);
    assert!(
        head.to_ascii_lowercase()
            .contains("content-type: text/plain; version=0.0.4"),
        "{head}"
    );
    assert!(body.contains("# TYPE porecatu_relay_hosts_registered gauge"));
    assert!(body.contains("# TYPE porecatu_relay_channels_opened_total counter"));
    // Nothing but `/metrics` is served on this listener.
    assert_eq!(metrics.get("/healthz").0, 404);
    assert_eq!(metrics.get("/v0/host").0, 404);
}

#[test]
fn the_numbers_follow_what_went_through_the_relay() {
    let (server, host, mut host_ws) = start();
    let metrics = MetricsListener::start(&server);

    let (mut client, channel) = attach(&server, &host, &mut host_ws);
    client.send_binary(client_data(b"12345"));
    assert_eq!(host_reads_data(&mut host_ws), (channel, b"12345".to_vec()));
    host_ws.send_binary(host_data(channel, b"abc"));
    assert_eq!(client_reads_data(&mut client), b"abc");

    let text = metrics.scrape_until(|t| {
        t.contains("hosts_registered 1\n")
            && t.contains("channels_open 1\n")
            && t.contains("frames_total{direction=\"client_to_host\"} 1\n")
            && t.contains("frames_total{direction=\"host_to_client\"} 1\n")
    });
    assert!(text.contains("hosts_registered_total 1\n"), "{text}");
    assert!(text.contains("channels_opened_total 1\n"), "{text}");
    assert!(
        text.contains("bytes_total{direction=\"client_to_host\"} 5\n"),
        "{text}"
    );
    assert!(
        text.contains("bytes_total{direction=\"host_to_client\"} 3\n"),
        "{text}"
    );

    // The host ends the channel: a close by reason, the gauge back to zero.
    host_ws.send_binary(host_close(channel, CloseReason::HostRejected));
    let text = metrics.scrape_until(|t| t.contains("channels_open 0\n"));
    assert!(
        text.contains("channel_closes_total{reason=\"host-rejected\"} 1\n"),
        "{text}"
    );
    drop(client);

    // The host drops its socket: a connection close with no code.
    drop(host_ws);
    let text = metrics.scrape_until(|t| t.contains("hosts_registered 0\n"));
    assert!(
        text.contains("host_connection_closes_total{code=\"none\"} 1\n"),
        "{text}"
    );
}

#[test]
fn a_channel_closed_for_a_limit_counts_under_limit_exceeded() {
    let (server, host, mut host_ws) = start();
    let metrics = MetricsListener::start(&server);
    let (_client, channel) = attach(&server, &host, &mut host_ws);

    host_ws.send_binary(host_close(channel, CloseReason::LimitExceeded));
    let text =
        metrics.scrape_until(|t| t.contains("channel_closes_total{reason=\"limit-exceeded\"} 1\n"));
    assert!(text.contains("channels_open 0\n"), "{text}");
}

/// What a label may never carry (RF-1.21): who the host is, where the peer is,
/// what the operator's token is or which channel it was.
#[test]
fn no_label_identifies_a_host_a_peer_a_token_or_a_channel() {
    let (server, host, mut host_ws) = start();
    let metrics = MetricsListener::start(&server);
    let (mut client, _channel) = attach(&server, &host, &mut host_ws);
    client.send_binary(client_data(b"x"));
    let _ = host_reads_data(&mut host_ws);

    let text = metrics.scrape_until(|t| t.contains("channels_open 1\n"));

    let host_id = host.host_id();
    assert!(!text.contains(host_id.as_str()), "host_id in:\n{text}");
    assert!(!text.contains(TOKEN), "token in:\n{text}");
    assert!(!text.contains("127.0.0.1"), "peer address in:\n{text}");
    assert!(!text.contains("::1"), "peer address in:\n{text}");

    // Only these label names exist, each with a value of a closed set.
    let mut label_names = std::collections::BTreeSet::new();
    for line in text.lines().filter(|line| !line.starts_with('#')) {
        if let Some((_, labels)) = line.split_once('{') {
            let labels = labels.split_once('}').expect("a closed label set").0;
            for pair in labels.split(',') {
                label_names.insert(pair.split_once('=').expect("name=value").0.to_owned());
            }
        }
    }
    let allowed: std::collections::BTreeSet<String> = ["direction", "reason", "code"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(label_names, allowed, "unexpected label names in:\n{text}");
    for forbidden in ["host_id", "channel=", "peer", "ip", "token"] {
        for name in &label_names {
            assert_ne!(name, forbidden);
        }
    }
}
