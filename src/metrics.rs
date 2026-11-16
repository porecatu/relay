// SPDX-License-Identifier: GPL-3.0-or-later

//! Aggregate metrics in the Prometheus text format (PRD-001 RF-1.21).
//!
//! Off by default (`[metrics] enabled`). When on, a listener of its own, apart
//! from the relay's, answers `GET /metrics`. The text is written by hand: it is
//! a handful of counters and no crate is worth the dependency.
//!
//! # What may be a label
//!
//! Only values drawn from a closed set the relay itself defines: the direction
//! of a frame, the reason a channel ended, the WebSocket close code of a host
//! connection. **Never** a `host_id`, an IP address, a token name or a channel:
//! those identify somebody, and the number of distinct values is not bounded.
//! The registry that renders the text has no way to take such a label, because
//! every series is a field of [`Metrics`] with its name fixed here.
//!
//! The counters are relaxed atomics: a scrape reads a consistent-enough
//! snapshot of independent numbers, and nothing waits for it. The only lock is
//! the one of the close-code table, never held across an `.await`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Router;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::IntoResponse;
use axum::routing::get;
use porecatu_protocol::envelope::CloseReason;
use tokio_util::sync::CancellationToken;

use crate::host::reason_name;
use crate::server::ServerState;

/// The content type of the Prometheus text exposition format, version 0.0.4.
const CONTENT_TYPE_PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The reasons a channel may end with, as labels. The last one catches any
/// reason added to the protocol before this list learns it.
const CLOSE_REASONS: [&str; 7] = [
    "normal",
    "host-replaced",
    "host-gone",
    "host-rejected",
    "limit-exceeded",
    "protocol-error",
    "other",
];

/// Which way a `payload` travelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// From a client (the phone) to the host (the desktop).
    ClientToHost,
    /// From the host to a client.
    HostToClient,
}

impl Direction {
    const ALL: [Self; 2] = [Self::ClientToHost, Self::HostToClient];

    fn index(self) -> usize {
        match self {
            Self::ClientToHost => 0,
            Self::HostToClient => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClientToHost => "client_to_host",
            Self::HostToClient => "host_to_client",
        }
    }
}

/// Every series the relay exposes.
#[derive(Debug, Default)]
pub struct Metrics {
    hosts_registered_total: AtomicU64,
    channels_opened_total: AtomicU64,
    channels_open: AtomicU64,
    frames: [AtomicU64; 2],
    bytes: [AtomicU64; 2],
    channel_closes: [AtomicU64; CLOSE_REASONS.len()],
    /// Host connections that ended, by the WebSocket close code the relay sent
    /// (`0`: it sent none and dropped the socket). The codes are the few the
    /// relay itself defines.
    host_closes: Mutex<BTreeMap<u16, u64>>,
}

impl Metrics {
    /// A set of metrics, all at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A host took its `host_id` in the registry (a replacement counts: it is
    /// a registration too).
    pub fn host_registered(&self) {
        self.hosts_registered_total.fetch_add(1, Ordering::Relaxed);
    }

    /// A channel was opened.
    pub fn channel_opened(&self) {
        self.channels_opened_total.fetch_add(1, Ordering::Relaxed);
        self.channels_open.fetch_add(1, Ordering::Relaxed);
    }

    /// A channel ended with `reason`.
    pub fn channel_closed(&self, reason: CloseReason) {
        let name = reason_name(reason);
        let index = CLOSE_REASONS
            .iter()
            .position(|known| *known == name)
            .unwrap_or(CLOSE_REASONS.len() - 1);
        self.channel_closes[index].fetch_add(1, Ordering::Relaxed);
        // A gauge that never goes below zero, whatever the order of events.
        let _ = self
            .channels_open
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |open| {
                Some(open.saturating_sub(1))
            });
    }

    /// One `payload` of `bytes` bytes was forwarded in `direction`.
    pub fn payload_forwarded(&self, direction: Direction, bytes: usize) {
        let index = direction.index();
        self.frames[index].fetch_add(1, Ordering::Relaxed);
        self.bytes[index].fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// A host connection ended; `code` is the WebSocket close code the relay
    /// sent, or `None` when it dropped the socket without one.
    pub fn host_connection_closed(&self, code: Option<u16>) {
        *lock(&self.host_closes)
            .entry(code.unwrap_or(0))
            .or_insert(0) += 1;
    }

    /// The text of a scrape. `hosts_registered` is how many hosts the
    /// registry holds right now (read from the registry, not counted here, so
    /// a replacement or a crash cannot make it drift).
    #[must_use]
    pub fn render(&self, hosts_registered: usize) -> String {
        let mut out = String::with_capacity(2048);

        gauge(
            &mut out,
            "porecatu_relay_hosts_registered",
            "Hosts registered right now.",
            hosts_registered as u64,
        );
        counter(
            &mut out,
            "porecatu_relay_hosts_registered_total",
            "Host registrations since the relay started.",
            self.hosts_registered_total.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "porecatu_relay_channels_open",
            "Channels open right now.",
            self.channels_open.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "porecatu_relay_channels_opened_total",
            "Channels opened since the relay started.",
            self.channels_opened_total.load(Ordering::Relaxed),
        );

        header(
            &mut out,
            "porecatu_relay_frames_total",
            "counter",
            "Payload frames forwarded, by direction.",
        );
        for direction in Direction::ALL {
            let _ = writeln!(
                out,
                "porecatu_relay_frames_total{{direction=\"{}\"}} {}",
                direction.label(),
                self.frames[direction.index()].load(Ordering::Relaxed)
            );
        }
        header(
            &mut out,
            "porecatu_relay_bytes_total",
            "counter",
            "Payload bytes forwarded, by direction.",
        );
        for direction in Direction::ALL {
            let _ = writeln!(
                out,
                "porecatu_relay_bytes_total{{direction=\"{}\"}} {}",
                direction.label(),
                self.bytes[direction.index()].load(Ordering::Relaxed)
            );
        }

        header(
            &mut out,
            "porecatu_relay_channel_closes_total",
            "counter",
            "Channels that ended, by the reason the client was told.",
        );
        for (name, count) in CLOSE_REASONS.iter().zip(&self.channel_closes) {
            let _ = writeln!(
                out,
                "porecatu_relay_channel_closes_total{{reason=\"{name}\"}} {}",
                count.load(Ordering::Relaxed)
            );
        }

        header(
            &mut out,
            "porecatu_relay_host_connection_closes_total",
            "counter",
            "Host connections that ended, by WebSocket close code (\"none\": dropped without one).",
        );
        let closes = lock(&self.host_closes);
        if closes.is_empty() {
            // A series with no value yet still has to exist for rate().
            let _ = writeln!(
                out,
                "porecatu_relay_host_connection_closes_total{{code=\"none\"}} 0"
            );
        }
        for (code, count) in closes.iter() {
            if *code == 0 {
                let _ = writeln!(
                    out,
                    "porecatu_relay_host_connection_closes_total{{code=\"none\"}} {count}"
                );
            } else {
                let _ = writeln!(
                    out,
                    "porecatu_relay_host_connection_closes_total{{code=\"{code}\"}} {count}"
                );
            }
        }
        out
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain counters: a poisoned lock is taken as it is.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn header(out: &mut String, name: &str, kind: &str, help: &str) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    header(out, name, "counter", help);
    let _ = writeln!(out, "{name} {value}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: u64) {
    header(out, name, "gauge", help);
    let _ = writeln!(out, "{name} {value}");
}

/// The router of the metrics listener: `GET /metrics` and nothing else.
pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/metrics", get(scrape))
        .with_state(state)
}

async fn scrape(State(state): State<Arc<ServerState>>) -> impl IntoResponse {
    let body = state.metrics().render(state.registry().len());
    (
        [
            (CONTENT_TYPE, CONTENT_TYPE_PROMETHEUS),
            (CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

/// Serves `GET /metrics` on `listener` until `shutdown` is cancelled.
///
/// Plain HTTP: this listener is meant for loopback or an internal network
/// (`[metrics] bind`), and carries no secret.
///
/// # Errors
///
/// An I/O error of the listener.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<ServerState>,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_set_renders_every_series_at_zero() {
        let text = Metrics::new().render(0);
        for name in [
            "porecatu_relay_hosts_registered 0",
            "porecatu_relay_hosts_registered_total 0",
            "porecatu_relay_channels_open 0",
            "porecatu_relay_channels_opened_total 0",
            "porecatu_relay_frames_total{direction=\"client_to_host\"} 0",
            "porecatu_relay_bytes_total{direction=\"host_to_client\"} 0",
            "porecatu_relay_channel_closes_total{reason=\"limit-exceeded\"} 0",
            "porecatu_relay_host_connection_closes_total{code=\"none\"} 0",
        ] {
            assert!(text.contains(name), "missing {name:?} in:\n{text}");
        }
    }

    #[test]
    fn counters_follow_the_events() {
        let metrics = Metrics::new();
        metrics.host_registered();
        metrics.channel_opened();
        metrics.channel_opened();
        metrics.channel_closed(CloseReason::LimitExceeded);
        metrics.payload_forwarded(Direction::ClientToHost, 10);
        metrics.payload_forwarded(Direction::ClientToHost, 5);
        metrics.payload_forwarded(Direction::HostToClient, 7);
        metrics.host_connection_closed(Some(1001));
        metrics.host_connection_closed(None);
        let text = metrics.render(3);
        assert!(
            text.contains("porecatu_relay_hosts_registered 3\n"),
            "{text}"
        );
        assert!(text.contains("porecatu_relay_hosts_registered_total 1\n"));
        assert!(text.contains("porecatu_relay_channels_open 1\n"));
        assert!(text.contains("porecatu_relay_channels_opened_total 2\n"));
        assert!(text.contains("frames_total{direction=\"client_to_host\"} 2\n"));
        assert!(text.contains("bytes_total{direction=\"client_to_host\"} 15\n"));
        assert!(text.contains("bytes_total{direction=\"host_to_client\"} 7\n"));
        assert!(text.contains("channel_closes_total{reason=\"limit-exceeded\"} 1\n"));
        assert!(text.contains("host_connection_closes_total{code=\"1001\"} 1\n"));
        assert!(text.contains("host_connection_closes_total{code=\"none\"} 1\n"));
    }

    #[test]
    fn the_open_gauge_never_goes_below_zero() {
        let metrics = Metrics::new();
        metrics.channel_closed(CloseReason::Normal);
        assert!(
            metrics
                .render(0)
                .contains("porecatu_relay_channels_open 0\n")
        );
    }

    #[test]
    fn every_close_reason_has_a_label() {
        for reason in [
            CloseReason::Normal,
            CloseReason::HostReplaced,
            CloseReason::HostGone,
            CloseReason::HostRejected,
            CloseReason::LimitExceeded,
            CloseReason::ProtocolError,
        ] {
            assert!(
                CLOSE_REASONS.contains(&reason_name(reason)),
                "{reason:?} has no label"
            );
        }
    }
}
