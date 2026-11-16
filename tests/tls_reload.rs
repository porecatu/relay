// SPDX-License-Identifier: GPL-3.0-or-later

//! The relay serving over TLS (ADR-0004): a `wss` connection works, a new
//! certificate pair on disk is seen by the **next** connection while the open
//! one stays alive with the old certificate, a pair that does not load is
//! ignored, and the graceful shutdown is the same as in plain HTTP.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use bytes::Bytes;
use ed25519_dalek::{Signer, SigningKey};
use porecatu_protocol::envelope::{self, HostEnvelope};
use porecatu_relay::config::{Settings, TlsSettings, TlsVersion};
use porecatu_relay::server::{self, ServerState};
use porecatu_relay::shutdown::{self, Outcome};
use porecatu_relay::tls;
use porecatu_relay::token::TokenSet;
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use tokio::runtime::Runtime;
use tokio_tungstenite::tungstenite::client::{IntoClientRequest, client};
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Message, WebSocket};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "test-operator-token-0123456789-abcdefghij";
const WAIT: Duration = Duration::from_secs(10);

type Tls = StreamOwned<ClientConnection, TcpStream>;

struct Pair {
    cert_pem: String,
    key_pem: String,
}

fn pair() -> Pair {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    Pair {
        cert_pem: generated.cert.pem(),
        key_pem: generated.signing_key.serialize_pem(),
    }
}

fn der(pem: &str) -> Vec<u8> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .next()
        .unwrap()
        .unwrap()
        .as_ref()
        .to_vec()
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A scratch directory with the certificate pair, removed on drop.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "porecatu-relay-tls-reload-{}-{n}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn cert(&self) -> PathBuf {
        self.0.join("cert.pem")
    }

    fn key(&self) -> PathBuf {
        self.0.join("key.pem")
    }

    fn write(&self, pair: &Pair) {
        fs::write(self.cert(), &pair.cert_pem).unwrap();
        fs::write(self.key(), &pair.key_pem).unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A relay over TLS on a loopback port, on a runtime of its own.
struct TlsServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    runtime: Option<Runtime>,
    stopped: Receiver<Result<Outcome, String>>,
    dir: Dir,
}

impl TlsServer {
    /// Replaces the pair on disk (the watcher picks it up within 50 ms).
    fn write_pair(&self, pair: &Pair) {
        self.dir.write(pair);
    }

    fn start(first: &Pair) -> Self {
        let dir = Dir::new();
        dir.write(first);
        let tls_settings = TlsSettings {
            cert_file: dir.cert(),
            key_file: dir.key(),
            min_version: TlsVersion::V1_3,
            reload_check: Duration::from_millis(50),
        };
        let config = tls::load_from_files(
            &tls_settings.cert_file,
            &tls_settings.key_file,
            TlsVersion::V1_3,
        )
        .unwrap();
        let mut settings = Settings::default();
        settings.server.bind = "127.0.0.1:0".parse().unwrap();
        let grace = settings.server.shutdown_grace;
        let state = Arc::new(ServerState::new(settings, TokenSet::new([("test", TOKEN)])));

        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let (done, stopped) = mpsc::channel();
        let serving = server::serve_tls(listener, state, shutdown.clone(), config, tls_settings);
        let token = shutdown.clone();
        runtime.spawn(async move {
            let outcome = shutdown::run(serving, &token, grace).await;
            let _ = done.send(outcome.map_err(|error| error.to_string()));
        });
        Self {
            addr,
            shutdown,
            runtime: Some(runtime),
            stopped,
            dir,
        }
    }
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

fn client_config(trusted_pem: &str) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(trusted_pem.as_bytes()) {
        roots.add(der.unwrap()).unwrap();
    }
    let mut config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// A `wss` connection to `path`, trusting only `trusted_pem`. `Err` when the
/// TLS or the WebSocket handshake fails.
fn connect_wss(
    addr: SocketAddr,
    trusted_pem: &str,
    path: &str,
    authorization: Option<&str>,
) -> Result<WebSocket<Tls>, String> {
    let tcp = TcpStream::connect(addr).map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(WAIT)).unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let connection =
        ClientConnection::new(client_config(trusted_pem), name).map_err(|e| e.to_string())?;
    let stream = StreamOwned::new(connection, tcp);
    let mut request = format!("wss://localhost:{}{path}", addr.port())
        .into_client_request()
        .unwrap();
    if let Some(value) = authorization {
        request
            .headers_mut()
            .insert("authorization", HeaderValue::from_str(value).unwrap());
    }
    client(request, stream)
        .map(|(ws, _)| ws)
        .map_err(|e| e.to_string())
}

fn connect_host(addr: SocketAddr, trusted_pem: &str) -> Result<WebSocket<Tls>, String> {
    connect_wss(
        addr,
        trusted_pem,
        "/v0/host",
        Some(&format!("Bearer {TOKEN}")),
    )
}

/// The certificate the server presented on this connection.
fn presented(ws: &WebSocket<Tls>) -> Vec<u8> {
    ws.get_ref()
        .conn
        .peer_certificates()
        .and_then(<[CertificateDer<'_>]>::first)
        .map(|cert| cert.as_ref().to_vec())
        .expect("the server presented a certificate")
}

fn read_binary(ws: &mut WebSocket<Tls>) -> Bytes {
    loop {
        match ws.read().expect("a frame") {
            Message::Binary(bytes) => return bytes,
            Message::Close(frame) => panic!("closed: {frame:?}"),
            _ => {}
        }
    }
}

/// Reads the `challenge`, registers a host and returns its `registered`
/// answer: proof that the connection carries traffic right now.
fn register(ws: &mut WebSocket<Tls>) {
    let HostEnvelope::Challenge { nonce } = envelope::decode_host(read_binary(ws)).unwrap() else {
        panic!("the first frame was not a challenge");
    };
    let key = SigningKey::from_bytes(&[7; 32]);
    let signature = key.sign(&envelope::register_message(&nonce));
    let frame = envelope::encode_host(&HostEnvelope::Register {
        routing_key: key.verifying_key().to_bytes(),
        signature: signature.to_bytes(),
    });
    ws.send(Message::Binary(Bytes::from(frame))).unwrap();
    assert!(matches!(
        envelope::decode_host(read_binary(ws)),
        Ok(HostEnvelope::Registered { .. })
    ));
}

#[test]
fn a_wss_connection_works_and_shows_the_certificate_of_the_pair() {
    let a = pair();
    let server = TlsServer::start(&a);

    let mut ws = connect_host(server.addr, &a.cert_pem).expect("wss connects");
    assert_eq!(presented(&ws), der(&a.cert_pem));
    register(&mut ws);

    // Plain `GET` over the same TLS, through the same router.
    let tcp = TcpStream::connect(server.addr).unwrap();
    tcp.set_read_timeout(Some(WAIT)).unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    let mut stream = StreamOwned::new(
        ClientConnection::new(client_config(&a.cert_pem), name).unwrap(),
        tcp,
    );
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    // The server closes without a `close_notify`: what arrived is the answer.
    let _ = stream.read_to_string(&mut response);
    assert!(response.starts_with("HTTP/1.1 200"), "{response:?}");
    assert!(response.ends_with("ok"), "{response:?}");
}

#[test]
fn a_client_that_does_not_trust_the_certificate_is_refused() {
    let a = pair();
    let other = pair();
    let server = TlsServer::start(&a);
    assert!(connect_host(server.addr, &other.cert_pem).is_err());
}

#[test]
fn a_new_pair_is_seen_by_the_next_connection_and_the_open_one_stays_alive() {
    let a = pair();
    let b = pair();
    let server = TlsServer::start(&a);

    let mut old = connect_host(server.addr, &a.cert_pem).expect("wss connects with A");
    assert_eq!(presented(&old), der(&a.cert_pem));

    // The pair on disk changes; the watcher polls every 50 ms.
    server.write_pair(&b);

    // The next connection, trusting only B, is accepted once the reload ran.
    let deadline = Instant::now() + WAIT;
    let new = loop {
        match connect_host(server.addr, &b.cert_pem) {
            Ok(ws) => break ws,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "the new certificate was never served: {error}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert_eq!(presented(&new), der(&b.cert_pem));
    // A client that still trusts only A is now refused: the old pair is gone
    // for new connections.
    assert!(connect_host(server.addr, &a.cert_pem).is_err());

    // The connection opened before the swap is still alive and still shows A.
    assert_eq!(presented(&old), der(&a.cert_pem));
    register(&mut old);
}

#[test]
fn a_pair_that_does_not_load_keeps_the_previous_one() {
    let a = pair();
    let server = TlsServer::start(&a);

    fs::write(server.dir.cert(), "not a certificate").unwrap();
    // Longer than a few polls of the watcher: it has seen the change and
    // rejected it.
    std::thread::sleep(Duration::from_millis(400));

    let mut ws = connect_host(server.addr, &a.cert_pem).expect("the old pair still serves");
    assert_eq!(presented(&ws), der(&a.cert_pem));
    register(&mut ws);
}

#[test]
fn the_graceful_shutdown_closes_hosts_with_1001_over_tls() {
    let a = pair();
    let server = TlsServer::start(&a);

    let mut ws = connect_host(server.addr, &a.cert_pem).expect("wss connects");
    register(&mut ws);

    server.shutdown.cancel();
    let code = loop {
        match ws.read() {
            Ok(Message::Close(frame)) => break frame.map(|frame| u16::from(frame.code)),
            Ok(_) => {}
            Err(error) => panic!("the host was not closed politely: {error}"),
        }
    };
    assert_eq!(code, Some(1001));
    // Answer the close, so the server's handshake ends at once.
    let _ = ws.flush();
    drop(ws);

    let outcome = server
        .stopped
        .recv_timeout(Duration::from_secs(15))
        .expect("the server stopped")
        .expect("no server error");
    assert_eq!(outcome, Outcome::Clean);
}
