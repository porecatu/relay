// SPDX-License-Identifier: GPL-3.0-or-later

//! Embedded TLS: building the `rustls` server configuration from the
//! certificate and key files, and reloading it when the files change
//! (ADR-0004, PRD-001 RF-1.11).
//!
//! Nothing here touches a socket: the server (another stage) hands the
//! configuration to `axum-server`. A reload only affects connections made
//! afterwards; open connections keep the certificate they started with.
//!
//! Errors say which file failed and why, never what the file held: no PEM
//! text, line or key byte reaches an error message or a log line.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use rustls::ServerConfig;
use rustls::SupportedProtocolVersion;
use rustls::crypto::ring::default_provider;
use rustls::pki_types::pem::{self, PemObject as _};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::time::{self, Instant, MissedTickBehavior};

use crate::config::{TlsSettings, TlsVersion};

/// The only ALPN protocol offered: WebSocket runs over HTTP/1.1.
const ALPN_HTTP_1_1: &[u8] = b"http/1.1";

/// Why a PEM file could not be used. Carries no text from the file itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PemProblem {
    /// The file holds no item of the expected kind.
    NoItems,
    /// The PEM syntax or its base64 body is malformed.
    Malformed,
    /// A PEM section is implausibly large.
    TooLarge,
}

impl PemProblem {
    fn from_pem(error: &pem::Error) -> Self {
        match error {
            pem::Error::NoItemsFound => Self::NoItems,
            pem::Error::SectionTooLarge => Self::TooLarge,
            _ => Self::Malformed,
        }
    }
}

impl fmt::Display for PemProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoItems => "no PEM item of the expected kind was found",
            Self::Malformed => "the PEM data is malformed",
            Self::TooLarge => "a PEM section is too large",
        })
    }
}

/// Failure to build the TLS configuration.
#[derive(Debug)]
pub enum TlsError {
    /// A file could not be read.
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The operating system error.
        source: io::Error,
    },
    /// The certificate chain is not usable PEM.
    Certificate(PemProblem),
    /// The private key is not usable PEM.
    PrivateKey(PemProblem),
    /// `rustls` rejected the protocol versions (cannot happen with the two
    /// supported minimums, kept so no path needs an `unwrap`).
    Protocol(rustls::Error),
    /// `rustls` rejected the pair: the key does not match the certificate, or
    /// it is of a kind the `ring` provider does not support.
    KeyRejected(rustls::Error),
    /// One of the two errors above, tagged with the files that were being
    /// loaded.
    InFiles {
        /// The `cert_file`.
        cert: PathBuf,
        /// The `key_file`.
        key: PathBuf,
        /// What went wrong.
        source: Box<TlsError>,
    },
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::Certificate(problem) => write!(f, "certificate: {problem}"),
            Self::PrivateKey(problem) => write!(f, "private key: {problem}"),
            Self::Protocol(error) => write!(f, "TLS protocol versions: {error}"),
            Self::KeyRejected(error) => {
                write!(f, "certificate and private key rejected: {error}")
            }
            Self::InFiles { cert, key, source } => write!(
                f,
                "{source} (tls.cert_file = {}, tls.key_file = {})",
                cert.display(),
                key.display()
            ),
        }
    }
}

impl std::error::Error for TlsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Protocol(error) | Self::KeyRejected(error) => Some(error),
            Self::InFiles { source, .. } => Some(source.as_ref()),
            Self::Certificate(_) | Self::PrivateKey(_) => None,
        }
    }
}

/// Builds the server configuration from PEM bytes.
///
/// `cert_pem` is the full chain (leaf first); `key_pem` the private key
/// (PKCS#8, PKCS#1 or SEC1). The provider is `ring`, chosen explicitly so the
/// process never depends on a default installed elsewhere. TLS 1.3 only for
/// [`TlsVersion::V1_3`], 1.2 and 1.3 for [`TlsVersion::V1_2`]; no client
/// authentication; ALPN offers `http/1.1` and nothing else.
///
/// # Errors
///
/// [`TlsError::Certificate`] or [`TlsError::PrivateKey`] when a PEM is empty or
/// malformed, [`TlsError::KeyRejected`] when the key does not match the
/// certificate.
pub fn build_server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    min_version: TlsVersion,
) -> Result<ServerConfig, TlsError> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError::Certificate(PemProblem::from_pem(&error)))?;
    if chain.is_empty() {
        return Err(TlsError::Certificate(PemProblem::NoItems));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|error| TlsError::PrivateKey(PemProblem::from_pem(&error)))?;

    let versions: &[&SupportedProtocolVersion] = match min_version {
        TlsVersion::V1_3 => &[&rustls::version::TLS13],
        TlsVersion::V1_2 => &[&rustls::version::TLS12, &rustls::version::TLS13],
    };
    let mut config = ServerConfig::builder_with_provider(Arc::new(default_provider()))
        .with_protocol_versions(versions)
        .map_err(TlsError::Protocol)?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(TlsError::KeyRejected)?;
    config.alpn_protocols = vec![ALPN_HTTP_1_1.to_vec()];
    Ok(config)
}

/// Reads the two files and builds the configuration.
///
/// # Errors
///
/// [`TlsError::InFiles`] wrapping either [`TlsError::Read`] (naming the file
/// that could not be read) or the build error, with both file names and the
/// keys (`tls.cert_file`, `tls.key_file`) they came from.
pub fn load_from_files(
    cert: &Path,
    key: &Path,
    min_version: TlsVersion,
) -> Result<ServerConfig, TlsError> {
    let read = |path: &Path| {
        fs::read(path).map_err(|source| TlsError::Read {
            path: path.to_path_buf(),
            source,
        })
    };
    let in_files = |source: TlsError| TlsError::InFiles {
        cert: cert.to_path_buf(),
        key: key.to_path_buf(),
        source: Box::new(source),
    };
    let cert_pem = read(cert).map_err(in_files)?;
    let key_pem = read(key).map_err(in_files)?;
    build_server_config(&cert_pem, &key_pem, min_version).map_err(in_files)
}

/// What the watcher remembers about one file: its modification time and
/// length. A file that cannot be stat'ed (for instance, in the middle of a
/// renewal) is a stamp of its own, so it counts as a change both ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    mtime: Option<SystemTime>,
    len: Option<u64>,
}

impl FileStamp {
    /// Builds a stamp from values already read.
    #[must_use]
    pub fn new(mtime: SystemTime, len: u64) -> Self {
        Self {
            mtime: Some(mtime),
            len: Some(len),
        }
    }

    /// The stamp of a file that could not be stat'ed.
    #[must_use]
    pub fn missing() -> Self {
        Self {
            mtime: None,
            len: None,
        }
    }

    /// Reads the stamp of `path` from the file system.
    #[must_use]
    pub fn of(path: &Path) -> Self {
        match fs::metadata(path) {
            Ok(meta) => Self {
                mtime: meta.modified().ok(),
                len: Some(meta.len()),
            },
            Err(_) => Self::missing(),
        }
    }
}

/// Decides whether the certificate pair changed. Pure: whoever calls it reads
/// the metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CertWatcher {
    cert: FileStamp,
    key: FileStamp,
}

impl CertWatcher {
    /// Starts from the stamps of the pair that is currently loaded.
    #[must_use]
    pub fn new(cert: FileStamp, key: FileStamp) -> Self {
        Self { cert, key }
    }

    /// Records the new stamps and says whether either file differs from the
    /// last ones seen. The stamps are updated even on a change, so a pair that
    /// fails to load is reported once, not at every poll.
    pub fn check(&mut self, cert: FileStamp, key: FileStamp) -> bool {
        let changed = self.cert != cert || self.key != key;
        self.cert = cert;
        self.key = key;
        changed
    }
}

/// Polls the certificate files every `settings.reload_check` and calls
/// `on_change` with a fresh configuration each time the **new** pair loads.
///
/// A pair that fails to load keeps the previous configuration (the callback is
/// not called) and logs `tls_reload_failed` with the reason, which names the
/// file but never its content. Never returns: run it in a task and drop or
/// abort the task at shutdown. The stamps are taken when the future first
/// runs, so the initial load should happen right before spawning it.
pub async fn watch<F>(settings: &TlsSettings, mut on_change: F)
where
    F: FnMut(ServerConfig),
{
    let period = settings.reload_check.max(Duration::from_millis(1));
    let mut watcher = CertWatcher::new(
        FileStamp::of(&settings.cert_file),
        FileStamp::of(&settings.key_file),
    );
    let mut ticker = time::interval_at(Instant::now() + period, period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let cert_stamp = FileStamp::of(&settings.cert_file);
        let key_stamp = FileStamp::of(&settings.key_file);
        if !watcher.check(cert_stamp, key_stamp) {
            continue;
        }
        let (cert, key, min) = (
            settings.cert_file.clone(),
            settings.key_file.clone(),
            settings.min_version,
        );
        let loaded = tokio::task::spawn_blocking(move || load_from_files(&cert, &key, min)).await;
        match loaded {
            Ok(Ok(config)) => {
                tracing::info!(event = "tls_reloaded", "certificate pair reloaded");
                on_change(config);
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    event = "tls_reload_failed",
                    reason = %error,
                    "new certificate pair rejected, keeping the previous one"
                );
            }
            Err(_) => {
                tracing::warn!(
                    event = "tls_reload_failed",
                    reason = "the loading task did not finish",
                    "new certificate pair not loaded, keeping the previous one"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConnection};
    use tokio::sync::mpsc;

    use super::*;

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

    /// Runs an in-memory handshake; returns the negotiated version and ALPN.
    fn handshake(
        server: ServerConfig,
        cert_pem: &str,
        client_versions: &[&'static SupportedProtocolVersion],
    ) -> Result<(rustls::ProtocolVersion, Option<Vec<u8>>), rustls::Error> {
        let mut roots = RootCertStore::empty();
        for der in CertificateDer::pem_slice_iter(cert_pem.as_bytes()) {
            roots.add(der.unwrap()).unwrap();
        }
        let mut client_config = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_protocol_versions(client_versions)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec(), b"h2".to_vec()];
        let name = ServerName::try_from("localhost").unwrap();
        let mut client = ClientConnection::new(Arc::new(client_config), name)?;
        let mut server = ServerConnection::new(Arc::new(server))?;

        for _ in 0..32 {
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).unwrap();
                server.read_tls(&mut &buf[..]).unwrap();
                server.process_new_packets()?;
            }
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).unwrap();
                client.read_tls(&mut &buf[..]).unwrap();
                client.process_new_packets()?;
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
        }
        assert!(!server.is_handshaking(), "handshake did not finish");
        Ok((
            server.protocol_version().unwrap(),
            server.alpn_protocol().map(<[u8]>::to_vec),
        ))
    }

    #[test]
    fn config_offers_only_http_1_1() {
        let p = pair();
        let config = build_server_config(
            p.cert_pem.as_bytes(),
            p.key_pem.as_bytes(),
            TlsVersion::V1_3,
        )
        .unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
        let (_, alpn) = handshake(config, &p.cert_pem, &[&rustls::version::TLS13]).unwrap();
        assert_eq!(alpn.as_deref(), Some(&b"http/1.1"[..]));
    }

    #[test]
    fn min_version_1_3_refuses_a_tls_1_2_client() {
        let p = pair();
        let config = build_server_config(
            p.cert_pem.as_bytes(),
            p.key_pem.as_bytes(),
            TlsVersion::V1_3,
        )
        .unwrap();
        assert!(handshake(config, &p.cert_pem, &[&rustls::version::TLS12]).is_err());
    }

    #[test]
    fn min_version_1_3_negotiates_1_3() {
        let p = pair();
        let config = build_server_config(
            p.cert_pem.as_bytes(),
            p.key_pem.as_bytes(),
            TlsVersion::V1_3,
        )
        .unwrap();
        let both = [&rustls::version::TLS12, &rustls::version::TLS13];
        let (version, _) = handshake(config, &p.cert_pem, &both).unwrap();
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_3);
    }

    #[test]
    fn min_version_1_2_accepts_a_tls_1_2_client() {
        let p = pair();
        let config = build_server_config(
            p.cert_pem.as_bytes(),
            p.key_pem.as_bytes(),
            TlsVersion::V1_2,
        )
        .unwrap();
        let (version, _) = handshake(config, &p.cert_pem, &[&rustls::version::TLS12]).unwrap();
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_2);
    }

    #[test]
    fn invalid_pem_is_an_error_without_the_content() {
        let p = pair();
        let garbage = b"-----BEGIN CERTIFICATE-----\nSECRET-CONTENT!!\n-----END CERTIFICATE-----\n";
        let error =
            build_server_config(garbage, p.key_pem.as_bytes(), TlsVersion::V1_3).unwrap_err();
        assert!(matches!(error, TlsError::Certificate(_)), "{error}");
        assert!(!error.to_string().contains("SECRET"), "{error}");

        let error = build_server_config(b"not pem at all", p.key_pem.as_bytes(), TlsVersion::V1_3)
            .unwrap_err();
        assert!(matches!(error, TlsError::Certificate(PemProblem::NoItems)));

        let error = build_server_config(p.cert_pem.as_bytes(), b"", TlsVersion::V1_3).unwrap_err();
        assert!(matches!(error, TlsError::PrivateKey(PemProblem::NoItems)));

        // A certificate file where the key should be.
        let error = build_server_config(
            p.cert_pem.as_bytes(),
            p.cert_pem.as_bytes(),
            TlsVersion::V1_3,
        )
        .unwrap_err();
        assert!(matches!(error, TlsError::PrivateKey(_)), "{error}");
    }

    #[test]
    fn key_that_does_not_match_the_certificate_is_rejected() {
        let a = pair();
        let b = pair();
        let error = build_server_config(
            a.cert_pem.as_bytes(),
            b.key_pem.as_bytes(),
            TlsVersion::V1_3,
        )
        .unwrap_err();
        assert!(matches!(error, TlsError::KeyRejected(_)), "{error}");
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A scratch directory removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("porecatu-relay-tls-{}-{n}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn cert(&self) -> PathBuf {
            self.0.join("cert.pem")
        }
        fn key(&self) -> PathBuf {
            self.0.join("key.pem")
        }
        fn write(&self, p: &Pair) {
            fs::write(self.cert(), &p.cert_pem).unwrap();
            fs::write(self.key(), &p.key_pem).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn load_from_files_names_the_missing_file() {
        let dir = Dir::new();
        let p = pair();
        dir.write(&p);
        let ok = load_from_files(&dir.cert(), &dir.key(), TlsVersion::V1_3);
        assert!(ok.is_ok());

        let missing = dir.0.join("absent.pem");
        let error = load_from_files(&missing, &dir.key(), TlsVersion::V1_3).unwrap_err();
        assert!(error.to_string().contains("absent.pem"), "{error}");

        fs::write(dir.key(), "junk").unwrap();
        let error = load_from_files(&dir.cert(), &dir.key(), TlsVersion::V1_3).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("private key"), "{text}");
        assert!(text.contains("key.pem"), "{text}");
        assert!(!text.contains("junk"), "{text}");
    }

    #[test]
    fn watcher_fires_only_on_a_change() {
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let t1 = t0 + Duration::from_secs(5);
        let a = FileStamp::new(t0, 100);
        let mut w = CertWatcher::new(a, a);
        assert!(!w.check(a, a), "no change");
        assert!(!w.check(a, a), "still no change");
        assert!(w.check(FileStamp::new(t1, 100), a), "cert mtime");
        assert!(!w.check(FileStamp::new(t1, 100), a), "reported once");
        assert!(w.check(FileStamp::new(t1, 101), a), "cert length");
        assert!(
            w.check(FileStamp::new(t1, 101), FileStamp::new(t1, 100)),
            "key"
        );
        assert!(
            w.check(FileStamp::new(t1, 101), FileStamp::missing()),
            "key gone"
        );
        assert!(
            w.check(FileStamp::new(t1, 101), FileStamp::new(t1, 100)),
            "key back"
        );
    }

    fn settings(dir: &Dir, reload: Duration) -> TlsSettings {
        TlsSettings {
            cert_file: dir.cert(),
            key_file: dir.key(),
            min_version: TlsVersion::V1_3,
            reload_check: reload,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn watch_reloads_a_valid_pair_and_keeps_the_old_one_on_an_invalid_pair() {
        let dir = Dir::new();
        dir.write(&pair());
        let s = settings(&dir, Duration::from_millis(20));
        let (tx, mut rx) = mpsc::unbounded_channel::<ServerConfig>();
        let task = tokio::spawn(async move {
            watch(&s, move |config| {
                let _ = tx.send(config);
            })
            .await;
        });
        // Let the watcher take its first stamps and tick without changes.
        time::sleep(Duration::from_millis(120)).await;
        assert!(rx.try_recv().is_err(), "no change, no callback");

        // A new valid pair is delivered.
        dir.write(&pair());
        let delivered = time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(delivered, Ok(Some(_))), "valid pair delivered");

        // An invalid pair is not.
        fs::write(dir.cert(), "garbage").unwrap();
        time::sleep(Duration::from_millis(250)).await;
        assert!(rx.try_recv().is_err(), "invalid pair must not be delivered");

        // A mismatching pair is not either.
        let (a, b) = (pair(), pair());
        fs::write(dir.cert(), &a.cert_pem).unwrap();
        fs::write(dir.key(), &b.key_pem).unwrap();
        time::sleep(Duration::from_millis(250)).await;
        assert!(
            rx.try_recv().is_err(),
            "mismatching pair must not be delivered"
        );

        // Fixed, it is delivered again.
        dir.write(&pair());
        let delivered = time::timeout(Duration::from_secs(5), rx.recv()).await;
        assert!(matches!(delivered, Ok(Some(_))), "recovered pair delivered");

        task.abort();
    }
}
