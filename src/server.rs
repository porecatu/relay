// SPDX-License-Identifier: GPL-3.0-or-later

//! The HTTP and WebSocket server (specification 01 §2 and §4; architecture §3).
//!
//! Four paths, in plain HTTP ([`serve`]) or over TLS ([`serve_tls`]); the
//! routes and the shutdown are the same:
//!
//! | Path | Checks before the answer |
//! |---|---|
//! | `/v0/host` | `429` (attempts per IP), `401` (operator token), upgrade |
//! | `/v0/client/{host_id}` | `400` (malformed `host_id`), `429`, upgrade |
//! | `/v0/presence/{host_id}` | `400`, `429`, then `204` or `404` with no body |
//! | `/healthz` | none: `200 ok` |
//!
//! The order matters: the per-IP limit comes **before** the token check, so a
//! guesser of tokens is counted on every attempt, right or wrong, and before
//! the upgrade, so a refused peer costs no WebSocket.
//!
//! The peer address is resolved once per request ([`crate::peer`]) and feeds
//! both the limits and the logs.
//!
//! The host connection lives in [`crate::host`] and the client connection in
//! [`crate::client`]; this module only accepts them.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{AUTHORIZATION, HeaderMap};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use porecatu_protocol::envelope::{PING_INTERVAL_MS, PONG_TIMEOUT_MS};
use porecatu_protocol::host_id::HostId;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::client;
use crate::config::{IpLogging, Settings, TlsSettings};
use crate::host::{self, HostHandle};
use crate::limits::RateLimiter;
use crate::metrics::Metrics;
use crate::peer::{self, IpLogPolicy};
use crate::registry::Registry;
use crate::tls;
use crate::token::{self, TokenSet};

/// Most distinct peers a per-IP limiter remembers; past it the oldest is
/// forgotten, so a flood of addresses costs bounded memory.
const RATE_LIMIT_MAX_KEYS: usize = 65_536;

/// How long one send, or the wait for the peer's `Close` answer, may take
/// before the socket is dropped.
pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// WebSocket close code `1011` (internal error); the protocol crate does not
/// name it because the specification does not use it on its own.
pub(crate) const CLOSE_INTERNAL_ERROR: u16 = 1011;

/// The keepalive periods (specification 01 §1). The defaults are the ones of
/// the specification; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How often a `Ping` is sent on a host connection.
    pub ping_interval: Duration,
    /// How long a connection may go without answering with a `Pong` before it
    /// is dropped (with no close code).
    pub pong_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_millis(PING_INTERVAL_MS),
            pong_timeout: Duration::from_millis(PONG_TIMEOUT_MS),
        }
    }
}

/// One per-IP limiter per use, each behind its own lock. The locks are never
/// held across an `.await`.
struct Limiters {
    host_attempts: Mutex<RateLimiter>,
    client_connects: Mutex<RateLimiter>,
    presence: Mutex<RateLimiter>,
}

/// Which limiter a request counts against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Use {
    HostAttempt,
    ClientConnect,
    Presence,
}

fn per_minute(value: u64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // The limiters hold plain counters: a poisoned lock is taken as it is.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Counts the WebSocket connection tasks that are alive.
///
/// `axum::serve` does not wait for an upgraded connection: its graceful
/// shutdown ends once the HTTP side is done, while the host and client tasks
/// still have a `Close` to send. Each task holds a [`ConnectionGuard`] from the
/// moment the upgrade is accepted, and [`serve`] waits for the count to reach
/// zero.
struct Connections {
    live: watch::Sender<usize>,
}

impl Connections {
    fn new() -> Self {
        Self {
            live: watch::Sender::new(0),
        }
    }

    fn track(self: &Arc<Self>) -> ConnectionGuard {
        self.live.send_modify(|live| *live += 1);
        ConnectionGuard {
            connections: Arc::clone(self),
        }
    }

    async fn wait_idle(&self) {
        let mut live = self.live.subscribe();
        // The sender lives in `self`, so the wait cannot fail.
        let _ = live.wait_for(|live| *live == 0).await;
    }
}

/// Keeps one connection task counted until it is dropped.
struct ConnectionGuard {
    connections: Arc<Connections>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.connections
            .live
            .send_modify(|live| *live = live.saturating_sub(1));
    }
}

/// Everything the connection tasks share.
pub struct ServerState {
    settings: Settings,
    tokens: TokenSet,
    registry: Registry<HostHandle>,
    timing: Timing,
    limiters: Limiters,
    shutdown: CancellationToken,
    connections: Arc<Connections>,
    metrics: Arc<Metrics>,
}

impl ServerState {
    /// The state for `settings`, with the operator tokens `tokens` and the
    /// keepalive periods of the specification.
    #[must_use]
    pub fn new(settings: Settings, tokens: TokenSet) -> Self {
        let limits = &settings.limits;
        let limiters = Limiters {
            host_attempts: Mutex::new(RateLimiter::new(
                per_minute(limits.host_attempts_per_ip_per_minute),
                RATE_LIMIT_MAX_KEYS,
            )),
            client_connects: Mutex::new(RateLimiter::new(
                per_minute(limits.client_connects_per_ip_per_minute),
                RATE_LIMIT_MAX_KEYS,
            )),
            presence: Mutex::new(RateLimiter::new(
                per_minute(limits.presence_per_minute_per_ip),
                RATE_LIMIT_MAX_KEYS,
            )),
        };
        let registry = Registry::new(limits.max_hosts);
        Self {
            settings,
            tokens,
            registry,
            timing: Timing::default(),
            limiters,
            shutdown: CancellationToken::new(),
            connections: Arc::new(Connections::new()),
            metrics: Arc::new(Metrics::new()),
        }
    }

    /// The aggregate metrics (exposed by [`crate::metrics`] when enabled).
    #[must_use]
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// Replaces the keepalive periods (tests shorten them).
    #[must_use]
    pub fn with_timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// The settings the server runs with.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The registered hosts.
    #[must_use]
    pub fn registry(&self) -> &Registry<HostHandle> {
        &self.registry
    }

    /// The keepalive periods.
    #[must_use]
    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// Cancelled when the relay is shutting down: every connection closes
    /// with `1001`.
    #[must_use]
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Counts a connection task until the guard is dropped.
    fn track_connection(&self) -> ConnectionGuard {
        self.connections.track()
    }

    /// Counts one request of `ip` against the limiter of `usage` and says
    /// whether it is within the limit.
    fn allow(&self, usage: Use, ip: IpAddr) -> bool {
        let limiter = match usage {
            Use::HostAttempt => &self.limiters.host_attempts,
            Use::ClientConnect => &self.limiters.client_connects,
            Use::Presence => &self.limiters.presence,
        };
        lock(limiter).check(ip, Instant::now())
    }

    /// The address of the peer for limits and logs (PRD-001 RF-1.13).
    fn resolve_peer(&self, socket: SocketAddr, headers: &HeaderMap) -> IpAddr {
        // Several `X-Forwarded-For` lines mean the same as one line with the
        // values joined by commas; a value that is not text resolves to the
        // socket.
        let mut joined = String::new();
        for value in headers.get_all("x-forwarded-for") {
            let Ok(value) = value.to_str() else {
                return socket.ip().to_canonical();
            };
            if !joined.is_empty() {
                joined.push(',');
            }
            joined.push_str(value);
        }
        let xff = (!joined.is_empty()).then_some(joined.as_str());
        peer::resolve_peer(socket, xff, &self.settings.server.trusted_proxies)
    }

    /// The peer as it may appear in a log line.
    pub(crate) fn peer_for_log(&self, ip: IpAddr) -> Option<String> {
        let policy = match self.settings.log.ip_addresses {
            IpLogging::Full => IpLogPolicy::Full,
            IpLogging::Truncated => IpLogPolicy::Truncated,
            IpLogging::None => IpLogPolicy::None,
        };
        peer::ip_for_log(ip, policy)
    }

    /// The `Settings` frame limit, as the WebSocket library wants it.
    pub(crate) fn max_frame(&self) -> usize {
        self.settings.limits.max_frame_bytes
    }
}

/// The router of the four paths. `state` is shared with every connection.
///
/// It reads the peer from [`ConnectInfo`], so it must be served with
/// `into_make_service_with_connect_info::<SocketAddr>()` ([`serve`] does).
pub fn router(state: Arc<ServerState>) -> Router {
    Router::new()
        .route("/v0/host", get(host_route))
        .route("/v0/client/{host_id}", get(client_route))
        .route("/v0/presence/{host_id}", get(presence_route))
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// Serves `state` on `listener`, in plain HTTP, until `shutdown` is cancelled.
///
/// On shutdown the connections close with `1001` and the future resolves once
/// they are gone (the WebSocket tasks included, which `axum` itself does not
/// wait for). Waiting only up to `shutdown_grace_seconds` is the caller's
/// business (it can drop this future, see [`crate::shutdown::run`]).
///
/// # Errors
///
/// An I/O error of the listener.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Arc<ServerState>,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let app = router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>();
    let tell_connections = {
        let state = Arc::clone(&state);
        async move {
            shutdown.cancelled().await;
            state.shutdown.cancel();
        }
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(tell_connections)
        .await?;
    state.connections.wait_idle().await;
    Ok(())
}

/// Serves `state` on `listener` over TLS, until `shutdown` is cancelled.
///
/// `config` is the pair loaded at startup (see [`crate::tls::load_from_files`]);
/// a task polls the files named by `tls` and, when a **new** valid pair
/// appears, swaps it in with `RustlsConfig::reload_from_config`. Only the
/// connections made afterwards use it: an open connection keeps the certificate
/// it started with (ADR-0004). A pair that fails to load is logged and the
/// previous one stays.
///
/// The shutdown is the one of [`serve`]: the connections close with `1001`,
/// and the future resolves once the WebSocket tasks (which `axum-server` does
/// not wait for either) are gone, through the same counter of connections.
/// Bounding that wait is [`crate::shutdown::run`]'s business.
///
/// # Errors
///
/// An I/O error of the listener.
pub async fn serve_tls(
    listener: tokio::net::TcpListener,
    state: Arc<ServerState>,
    shutdown: CancellationToken,
    config: rustls::ServerConfig,
    tls: TlsSettings,
) -> std::io::Result<()> {
    let rustls_config = RustlsConfig::from_config(Arc::new(config));
    let reloader = {
        let rustls_config = rustls_config.clone();
        tokio::spawn(async move {
            tls::watch(&tls, move |fresh| {
                rustls_config.reload_from_config(Arc::new(fresh));
            })
            .await;
        })
    };

    let handle = axum_server::Handle::new();
    let tell_connections = {
        let state = Arc::clone(&state);
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            state.shutdown.cancel();
            // No limit here: the grace period is applied by the caller, and
            // what keeps the process alive is the counter of connections.
            handle.graceful_shutdown(None);
        })
    };

    let app = router(Arc::clone(&state)).into_make_service_with_connect_info::<SocketAddr>();
    let served = match axum_server::from_tcp_rustls(listener.into_std()?, rustls_config) {
        Ok(server) => server.handle(handle).serve(app).await,
        Err(error) => Err(error),
    };
    reloader.abort();
    tell_connections.abort();
    served?;
    state.connections.wait_idle().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

/// Parses the `host_id` segment of a path, or answers `400`.
fn parse_host_id(text: &str) -> Result<HostId, StatusCode> {
    HostId::parse(text).map_err(|_| StatusCode::BAD_REQUEST)
}

async fn presence_route(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
) -> Response {
    let host_id = match parse_host_id(&host_id) {
        Ok(host_id) => host_id,
        Err(status) => return status.into_response(),
    };
    let peer = state.resolve_peer(socket, &headers);
    if !state.allow(Use::Presence, peer) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    if state.registry.is_present(&host_id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

async fn host_route(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let peer = state.resolve_peer(socket, &headers);
    if !state.allow(Use::HostAttempt, peer) {
        tracing::warn!(
            reason = "rate-limited",
            peer = state.peer_for_log(peer),
            "host refused"
        );
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let token_name = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(token::parse_bearer)
        .and_then(|presented| state.tokens.check(presented));
    let Some(token_name) = token_name else {
        tracing::warn!(
            reason = "bad-token",
            peer = state.peer_for_log(peer),
            "host refused"
        );
        return unauthorized();
    };
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let token_name = token_name.to_owned();
    let max_frame = state.max_frame();
    let guard = state.track_connection();
    upgrade
        .max_frame_size(max_frame)
        .max_message_size(max_frame)
        .on_upgrade(move |socket| async move {
            let _guard = guard;
            host::run(socket, state, token_name, peer).await;
        })
}

fn unauthorized() -> Response {
    let mut response = StatusCode::UNAUTHORIZED.into_response();
    response
        .headers_mut()
        .insert("www-authenticate", HeaderValue::from_static("Bearer"));
    response
}

async fn client_route(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(socket): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(host_id): Path<String>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    let host_id = match parse_host_id(&host_id) {
        Ok(host_id) => host_id,
        Err(status) => return status.into_response(),
    };
    let peer = state.resolve_peer(socket, &headers);
    if !state.allow(Use::ClientConnect, peer) {
        tracing::warn!(
            reason = "rate-limited",
            peer = state.peer_for_log(peer),
            "client refused"
        );
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    let max_frame = state.max_frame();
    let guard = state.track_connection();
    upgrade
        .max_frame_size(max_frame)
        .max_message_size(max_frame)
        .on_upgrade(move |socket| async move {
            let _guard = guard;
            client::run(socket, state, host_id, peer).await;
        })
}

/// Sends `message`, giving up if the peer does not read for [`IO_TIMEOUT`]: a
/// peer that never drains its socket must not hold a task forever.
pub(crate) async fn send_bounded(socket: &mut WebSocket, message: Message) -> Result<(), ()> {
    match tokio::time::timeout(IO_TIMEOUT, socket.send(message)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) | Err(_) => Err(()),
    }
}

/// Closes the WebSocket with `code`: sends the `Close` frame, then reads until
/// the peer answers (or [`IO_TIMEOUT`] passes), so that the socket is not
/// reset with unread data and the frame still reaches the peer.
///
/// A frame over the maximum is refused by the library as soon as its header is
/// read, with its payload still on the socket. Reading on is what takes that
/// payload off (the library reports the same capacity error for it), so those
/// errors do not end the drain: only a `Close`, the end of the stream, a
/// different error or the timeout do. Without this, dropping the socket with
/// the payload unread resets the connection, and the `Close` is lost with it.
pub(crate) async fn close_with(socket: &mut WebSocket, code: u16) {
    let frame = CloseFrame {
        code,
        reason: Utf8Bytes::from_static(""),
    };
    if send_bounded(socket, Message::Close(Some(frame)))
        .await
        .is_err()
    {
        return;
    }
    let drain = async {
        while let Some(next) = socket.recv().await {
            match next {
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(error) if host::is_too_big(&error) => {}
                Err(_) => break,
            }
        }
    };
    let _ = tokio::time::timeout(IO_TIMEOUT, drain).await;
}
