// SPDX-License-Identifier: GPL-3.0-or-later

//! Helpers of the integration tests: a relay on `127.0.0.1:0`, a blocking
//! WebSocket client and a fake host that signs the registration.
//!
//! The server runs on a runtime of its own, owned by [`TestServer`]; the tests
//! themselves are plain blocking code. That keeps the client simple (the
//! synchronous `tungstenite` API, with read timeouts) and the tests honest:
//! the relay never shares a thread with the peer it is talking to.

#![allow(dead_code)]

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ed25519_dalek::{Signer, SigningKey};
use porecatu_protocol::envelope::{
    self, ChannelId, ClientEnvelope, CloseReason, HostEnvelope, NONCE_LEN, Payload,
};
use porecatu_protocol::host_id::{HostId, host_id};
use porecatu_relay::config::Settings;
use porecatu_relay::logging::{self, CapturedLog, LogFormat, LogLevel};
use porecatu_relay::server::{self, ServerState, Timing};
use porecatu_relay::shutdown::{self, Outcome};
use porecatu_relay::token::TokenSet;
use tokio::net::TcpListener;
use tokio::runtime::{Builder, Runtime};
use tokio_tungstenite::tungstenite::client::{IntoClientRequest, client};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Error as WsError, HandshakeError, Message, WebSocket};
use tokio_util::sync::CancellationToken;
use tracing::subscriber::DefaultGuard;

/// The operator token every test server accepts (40 characters).
pub const TOKEN: &str = "test-operator-token-0123456789-abcdefghij";

/// How long a test waits for something that should happen.
pub const WAIT: Duration = Duration::from_secs(5);

thread_local! {
    /// The log subscriber of a worker thread of a logged [`TestServer`].
    static LOG_GUARD: RefCell<Option<DefaultGuard>> = const { RefCell::new(None) };
}

/// A relay running on a loopback port.
pub struct TestServer {
    pub addr: SocketAddr,
    pub state: Arc<ServerState>,
    shutdown: CancellationToken,
    runtime: Option<Runtime>,
    /// What [`shutdown::run`] returned, once it did.
    stopped: Mutex<Receiver<Result<Outcome, String>>>,
}

impl TestServer {
    /// A relay with the default settings and the default keepalive.
    pub fn start() -> Self {
        Self::start_with(Timing::default(), |_| {})
    }

    /// A relay whose settings `adjust` may change (limits, timeouts), with
    /// the given keepalive periods.
    pub fn start_with(timing: Timing, adjust: impl FnOnce(&mut Settings)) -> Self {
        Self::build(timing, adjust, Runtime::new().unwrap())
    }

    /// A relay with the default settings whose log, at `level` and in
    /// `format`, goes to `log`.
    ///
    /// The subscriber is per thread, so the runtime has a single worker thread
    /// with the subscriber installed on it (and on its blocking threads): every
    /// task of the relay logs into `log`.
    pub fn start_logged(log: CapturedLog, level: LogLevel, format: LogFormat) -> Self {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .on_thread_start(move || {
                let guard = logging::init_for_test(level, format, log.clone());
                LOG_GUARD.with(|slot| *slot.borrow_mut() = Some(guard));
            })
            .build()
            .unwrap();
        Self::build(Timing::default(), |_| {}, runtime)
    }

    fn build(timing: Timing, adjust: impl FnOnce(&mut Settings), runtime: Runtime) -> Self {
        let mut settings = Settings::default();
        settings.server.bind = "127.0.0.1:0".parse().unwrap();
        adjust(&mut settings);
        let grace = settings.server.shutdown_grace;
        let tokens = TokenSet::new([("test", TOKEN)]);
        let state = Arc::new(ServerState::new(settings, tokens).with_timing(timing));
        let listener = runtime.block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let (done, stopped) = mpsc::channel();
        let serving = server::serve(listener, Arc::clone(&state), shutdown.clone());
        let token = shutdown.clone();
        runtime.spawn(async move {
            let outcome = shutdown::run(serving, &token, grace).await;
            let _ = done.send(outcome.map_err(|error| error.to_string()));
        });
        Self {
            addr,
            state,
            shutdown,
            runtime: Some(runtime),
            stopped: Mutex::new(stopped),
        }
    }

    /// Waits up to `within` for the server to stop after [`TestServer::shut_down`]
    /// and returns how it ended; `None` if it did not stop in time.
    pub fn wait_stopped(&self, within: Duration) -> Option<Outcome> {
        let stopped = self.stopped.lock().unwrap();
        match stopped.recv_timeout(within) {
            Ok(outcome) => Some(outcome.expect("the server failed")),
            Err(_) => None,
        }
    }

    /// Begins the graceful shutdown.
    pub fn shut_down(&self) {
        self.shutdown.cancel();
    }

    /// Opens `/v0/host` with `token` in the `Authorization` header (none when
    /// `None`). `Err` carries the HTTP status of a refused upgrade.
    pub fn connect_host(&self, token: Option<&str>) -> Result<TestWs, u16> {
        self.connect("/v0/host", token.map(|t| format!("Bearer {t}")))
    }

    /// Opens `/v0/host` with a raw `Authorization` header value.
    pub fn connect_host_raw(&self, authorization: &str) -> Result<TestWs, u16> {
        self.connect("/v0/host", Some(authorization.to_owned()))
    }

    /// Opens `/v0/client/<host_id>`.
    pub fn connect_client(&self, host_id: &str) -> Result<TestWs, u16> {
        self.connect(&format!("/v0/client/{host_id}"), None)
    }

    fn connect(&self, path: &str, authorization: Option<String>) -> Result<TestWs, u16> {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut request = format!("ws://{}{path}", self.addr)
            .into_client_request()
            .unwrap();
        if let Some(value) = authorization {
            request
                .headers_mut()
                .insert("authorization", HeaderValue::from_str(&value).unwrap());
        }
        match client(request, stream) {
            Ok((ws, _response)) => Ok(TestWs { ws }),
            Err(HandshakeError::Failure(WsError::Http(response))) => {
                Err(response.status().as_u16())
            }
            Err(other) => panic!("unexpected handshake failure: {other}"),
        }
    }

    /// A plain `GET`: the status code and the body.
    pub fn http_get(&self, path: &str) -> (u16, String) {
        self.http_get_with(path, &[])
    }

    /// A plain `GET` with extra headers.
    pub fn http_get_with(&self, path: &str, headers: &[(&str, &str)]) -> (u16, String) {
        let mut stream = TcpStream::connect(self.addr).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut request = format!("GET {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .split(' ')
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {response:?}"));
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .unwrap_or_default();
        (status, body)
    }

    /// The presence status of `host_id` (`204` or `404`).
    pub fn presence(&self, host_id: &str) -> u16 {
        self.http_get(&format!("/v0/presence/{host_id}")).0
    }

    /// Waits until the presence of `host_id` is `expected`; panics after
    /// [`WAIT`].
    pub fn wait_presence(&self, host_id: &str, expected: u16) {
        let deadline = Instant::now() + WAIT;
        loop {
            let status = self.presence(host_id);
            if status == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "presence of {host_id} stayed {status}, expected {expected}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

/// What the client saw next.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// A binary frame.
    Binary(Vec<u8>),
    /// A `Close` frame with this code (`None` when it had no code).
    Closed(Option<u16>),
    /// The socket went away with no `Close` frame.
    Dropped,
    /// Nothing arrived within the timeout.
    Silence,
}

/// A blocking WebSocket client.
pub struct TestWs {
    ws: WebSocket<TcpStream>,
}

impl TestWs {
    /// Sets how long [`TestWs::next`] waits.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.ws.get_ref().set_read_timeout(Some(timeout)).unwrap();
    }

    pub fn send_binary(&mut self, bytes: Vec<u8>) {
        self.ws.send(Message::Binary(Bytes::from(bytes))).unwrap();
    }

    pub fn send_text(&mut self, text: &str) {
        self.ws.send(Message::Text(text.into())).unwrap();
    }

    /// Sends a `Close` frame with no code (a client leaving politely).
    pub fn close_normally(&mut self) {
        let _ = self.ws.close(None);
        let _ = self.ws.flush();
    }

    /// The next thing that matters; pings and pongs are handled by the
    /// library and skipped.
    pub fn next(&mut self) -> Event {
        loop {
            match self.ws.read() {
                Ok(Message::Binary(bytes)) => return Event::Binary(bytes.to_vec()),
                Ok(Message::Close(frame)) => {
                    return Event::Closed(frame.map(|frame| u16::from(frame.code)));
                }
                Ok(_) => {}
                Err(WsError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Event::Silence;
                }
                Err(_) => return Event::Dropped,
            }
        }
    }

    /// Like [`TestWs::next`], but `timeout` bounds the whole call, not each
    /// read: with a ping arriving more often than `timeout`, a per-read
    /// timeout would never expire and the call would never return.
    pub fn next_within(&mut self, timeout: Duration) -> Event {
        let deadline = Instant::now() + timeout;
        let event = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Event::Silence;
            }
            self.set_timeout(left);
            match self.ws.read() {
                Ok(Message::Binary(bytes)) => break Event::Binary(bytes.to_vec()),
                Ok(Message::Close(frame)) => {
                    break Event::Closed(frame.map(|frame| u16::from(frame.code)));
                }
                Ok(_) => {}
                Err(WsError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break Event::Silence;
                }
                Err(_) => break Event::Dropped,
            }
        };
        self.set_timeout(WAIT);
        event
    }

    /// Expects the next event to be a `Close` with `code`.
    #[track_caller]
    pub fn expect_close(&mut self, code: u16) {
        assert_eq!(self.next(), Event::Closed(Some(code)));
    }
}

/// A host with a routing key of its own, able to sign the registration.
pub struct FakeHost {
    key: SigningKey,
}

impl FakeHost {
    /// A host whose key is derived from `seed`.
    pub fn new(seed: u8) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
        }
    }

    pub fn routing_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn host_id(&self) -> HostId {
        host_id(&self.routing_key())
    }

    /// Reads the `challenge` and returns its nonce.
    #[track_caller]
    pub fn read_challenge(ws: &mut TestWs) -> [u8; NONCE_LEN] {
        let Event::Binary(frame) = ws.next() else {
            panic!("expected the challenge");
        };
        match envelope::decode_host(Bytes::from(frame)) {
            Ok(HostEnvelope::Challenge { nonce }) => nonce,
            _ => panic!("the first frame was not a challenge"),
        }
    }

    /// The `register` frame for `nonce`, signed with this host's key.
    pub fn register_frame(&self, nonce: &[u8; NONCE_LEN]) -> Vec<u8> {
        let signature = self.key.sign(&envelope::register_message(nonce));
        envelope::encode_host(&HostEnvelope::Register {
            routing_key: self.routing_key(),
            signature: signature.to_bytes(),
        })
    }

    /// The whole registration: challenge, `register`, and the `registered`
    /// answer, whose `host_id` is returned.
    #[track_caller]
    pub fn register(&self, ws: &mut TestWs) -> HostId {
        let nonce = Self::read_challenge(ws);
        ws.send_binary(self.register_frame(&nonce));
        let Event::Binary(frame) = ws.next() else {
            panic!("expected `registered`");
        };
        match envelope::decode_host(Bytes::from(frame)) {
            Ok(HostEnvelope::Registered { host_id }) => host_id,
            _ => panic!("the answer was not `registered`"),
        }
    }

    /// Connects with the right token and registers.
    #[track_caller]
    pub fn connect_and_register(&self, server: &TestServer) -> TestWs {
        let mut ws = server.connect_host(Some(TOKEN)).unwrap();
        let registered = self.register(&mut ws);
        assert_eq!(registered, self.host_id());
        ws
    }
}

// ---- frame helpers shared by the routing and limits tests ----------------------------

/// An opaque `payload` of the protocol crate holding `bytes`.
pub fn payload(bytes: &[u8]) -> Payload {
    Payload::from_bytes(Bytes::copy_from_slice(bytes))
}

/// A `data` frame from a client.
pub fn client_data(bytes: &[u8]) -> Vec<u8> {
    envelope::encode_client(&ClientEnvelope::Data {
        payload: payload(bytes),
    })
}

/// A `channel-data` frame from the host.
pub fn host_data(channel: u64, bytes: &[u8]) -> Vec<u8> {
    envelope::encode_host(&HostEnvelope::ChannelData {
        channel: ChannelId(channel),
        payload: payload(bytes),
    })
}

/// A `close-channel` frame from the host.
pub fn host_close(channel: u64, reason: CloseReason) -> Vec<u8> {
    envelope::encode_host(&HostEnvelope::CloseChannel {
        channel: ChannelId(channel),
        reason,
    })
}

#[track_caller]
pub fn host_next(ws: &mut TestWs) -> HostEnvelope {
    match ws.next() {
        Event::Binary(frame) => {
            envelope::decode_host(Bytes::from(frame)).expect("a valid host envelope")
        }
        other => panic!("expected a frame for the host, got {other:?}"),
    }
}

#[track_caller]
pub fn client_next(ws: &mut TestWs) -> ClientEnvelope {
    match ws.next() {
        Event::Binary(frame) => {
            envelope::decode_client(Bytes::from(frame)).expect("a valid client envelope")
        }
        other => panic!("expected a frame for the client, got {other:?}"),
    }
}

/// The host reads `channel-data` and returns channel and payload bytes.
#[track_caller]
pub fn host_reads_data(ws: &mut TestWs) -> (u64, Vec<u8>) {
    match host_next(ws) {
        HostEnvelope::ChannelData { channel, payload } => {
            (channel.0, payload.into_bytes().to_vec())
        }
        other => panic!("expected channel-data, got {other:?}"),
    }
}

/// The client reads `data` and returns the payload bytes.
#[track_caller]
pub fn client_reads_data(ws: &mut TestWs) -> Vec<u8> {
    match client_next(ws) {
        ClientEnvelope::Data { payload } => payload.into_bytes().to_vec(),
        other => panic!("expected data, got {other:?}"),
    }
}

/// The client connects to `host` and the host reads `channel-opened`.
#[track_caller]
pub fn attach(server: &TestServer, host: &FakeHost, host_ws: &mut TestWs) -> (TestWs, u64) {
    let client = server
        .connect_client(host.host_id().as_str())
        .expect("the client upgrade");
    match host_next(host_ws) {
        HostEnvelope::ChannelOpened { channel } => (client, channel.0),
        other => panic!("expected channel-opened, got {other:?}"),
    }
}

/// The client gets `closed { reason }` and then a `Close` with `code`.
#[track_caller]
pub fn expect_closed(ws: &mut TestWs, reason: CloseReason, code: u16) {
    assert_eq!(client_next(ws), ClientEnvelope::Closed { reason });
    ws.expect_close(code);
}

/// A server plus a registered host.
pub fn start() -> (TestServer, FakeHost, TestWs) {
    let server = TestServer::start();
    let host = FakeHost::new(1);
    let host_ws = host.connect_and_register(&server);
    (server, host, host_ws)
}
