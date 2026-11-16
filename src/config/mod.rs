// SPDX-License-Identifier: GPL-3.0-or-later

//! Configuration: reading `relay.toml`, applying environment overrides and
//! validating the result against the floors of the protocol specification.
//!
//! [`load`] is the only way to obtain a [`Config`] from outside. It does no
//! network I/O and never reads `std::env` itself: the caller hands in the
//! environment, which keeps every rule testable.
//!
//! The text of [`ConfigError`] is operator-facing terminal output and is in
//! English, like the rest of the relay's diagnostics.

mod env;
mod model;
mod validate;

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use blake2::{Blake2s256, Digest};
use ipnet::IpNet;
use serde::Deserialize;

use model::Model;

/// The file name looked up when `--config` is not given.
const DEFAULT_FILE_NAME: &str = "relay.toml";

/// Where the relay looks for its configuration when `--config` is not given.
///
/// - Linux: `/etc/porecatu-relay/relay.toml`, the conventional place for a
///   system service.
/// - Windows and everything else: `relay.toml` **next to the executable**,
///   not in the current directory. A Windows service starts with
///   `C:\Windows\System32` as its current directory, so a path relative to it
///   would never find the file the operator put beside the binary. If the
///   executable's own path cannot be determined, it falls back to
///   `relay.toml` in the current directory.
///
/// Note: PRD-001 RF-1.1 says "the current directory" for the non-Linux case;
/// this function follows the implementation plan (next to the executable).
#[must_use]
pub fn default_path() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        PathBuf::from("/etc/porecatu-relay").join(DEFAULT_FILE_NAME)
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(DEFAULT_FILE_NAME)))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_FILE_NAME))
    }
}

/// Reads, overrides and validates the configuration.
///
/// `env` is the process environment as `(name, value)` pairs (typically
/// `std::env::vars()`); only `PORECATU_RELAY_*` entries are looked at.
///
/// # Errors
///
/// Any problem makes the relay refuse to start: an unreadable or malformed
/// file, an unknown key, an invalid environment variable, a value below the
/// protocol floors, a missing token, or a public `bind` without TLS. The
/// message names the file and line (or the variable) and the key.
pub fn load(
    path: &Path,
    env: impl Iterator<Item = (String, String)>,
) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_owned(),
        source,
    })?;
    let mut model: Model =
        toml::from_str(&text).map_err(|error| syntax_error(path, &text, &error))?;
    env::apply(&mut model, env)?;
    let settings = validate::validate(model, &validate::Context { path, text: &text })?;
    Ok(Config { settings })
}

/// A validated configuration.
///
/// Obtained from [`load`]; the runtime reads it through [`Config::settings`].
#[derive(Debug, Clone)]
pub struct Config {
    settings: Settings,
}

impl Config {
    /// The validated settings, with durations as [`Duration`] and rates in
    /// bytes per second.
    ///
    /// The returned value is a plain struct with public fields: the runtime
    /// and the tests may shorten timeouts on it without going through the
    /// validation (which has floors a test must not be bound by).
    #[must_use]
    pub fn settings(&self) -> Settings {
        self.settings.clone()
    }
}

/// Everything the runtime needs, already converted.
#[derive(Debug, Clone)]
pub struct Settings {
    /// `[server]`.
    pub server: ServerSettings,
    /// `[tls]`; `None` means plain HTTP/WebSocket (loopback or behind a proxy).
    pub tls: Option<TlsSettings>,
    /// `[[tokens]]`: only digests are kept.
    pub tokens: Vec<TokenDigest>,
    /// `[limits]`.
    pub limits: LimitSettings,
    /// `[log]`.
    pub log: LogSettings,
    /// `[metrics]`.
    pub metrics: MetricsSettings,
}

impl Default for Settings {
    /// The built-in defaults of `relay.example.toml`, with **no tokens**.
    ///
    /// That is not a configuration the relay would start with: it exists so
    /// tests can build a `Settings` and adjust the fields they care about.
    fn default() -> Self {
        Self {
            server: ServerSettings {
                bind: model::DEFAULT_BIND,
                behind_proxy: false,
                trusted_proxies: Vec::new(),
                shutdown_grace: Duration::from_secs(model::DEFAULT_SHUTDOWN_GRACE_SECONDS),
            },
            tls: None,
            tokens: Vec::new(),
            limits: LimitSettings {
                max_hosts: 1000,
                max_channels_per_host: 16,
                max_frame_bytes: 70_000,
                channel_bytes_per_second: validate::kbps_to_bytes_per_second(
                    model::DEFAULT_CHANNEL_BANDWIDTH_KBPS,
                ),
                channel_burst_bytes: model::DEFAULT_CHANNEL_BURST_KIB * 1024,
                channel_queue_frames: 64,
                host_queue_frames: 256,
                register_timeout: Duration::from_secs(model::DEFAULT_REGISTER_TIMEOUT_SECONDS),
                client_connects_per_ip_per_minute: model::DEFAULT_CLIENT_CONNECTS_PER_IP_PER_MINUTE,
                host_attempts_per_ip_per_minute: model::DEFAULT_HOST_ATTEMPTS_PER_IP_PER_MINUTE,
                presence_per_minute_per_ip: model::DEFAULT_PRESENCE_PER_MINUTE_PER_IP,
            },
            log: LogSettings {
                level: LogLevel::Info,
                format: LogFormat::Text,
                ip_addresses: IpLogging::Truncated,
            },
            metrics: MetricsSettings {
                enabled: false,
                bind: model::DEFAULT_METRICS_BIND,
            },
        }
    }
}

/// `[server]`.
#[derive(Debug, Clone)]
pub struct ServerSettings {
    /// Address and port the relay listens on.
    pub bind: SocketAddr,
    /// The operator declares that a reverse proxy terminates TLS in front.
    pub behind_proxy: bool,
    /// Proxies whose `X-Forwarded-For` is believed (a bare address is a `/32`
    /// or `/128` network).
    pub trusted_proxies: Vec<IpNet>,
    /// How long shutdown waits for connections to close.
    pub shutdown_grace: Duration,
}

/// `[tls]`, present only when the relay terminates TLS itself.
#[derive(Debug, Clone)]
pub struct TlsSettings {
    /// Certificate chain, PEM. The file is not read by the configuration.
    pub cert_file: PathBuf,
    /// Private key, PEM. The file is not read by the configuration.
    pub key_file: PathBuf,
    /// Lowest TLS version accepted.
    pub min_version: TlsVersion,
    /// How often the certificate files are checked for changes.
    pub reload_check: Duration,
}

/// Minimum TLS version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum TlsVersion {
    /// TLS 1.3 only (default).
    #[serde(rename = "1.3")]
    V1_3,
    /// TLS 1.2 and 1.3.
    #[serde(rename = "1.2")]
    V1_2,
}

/// An operator token as the relay keeps it: a name and the BLAKE2s-256 digest
/// of the value. The value itself is dropped right after loading.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenDigest {
    /// Name that appears in logs.
    pub name: String,
    digest: [u8; 32],
}

impl TokenDigest {
    /// Digests `value` (BLAKE2s-256) under `name`.
    #[must_use]
    pub fn new(name: impl Into<String>, value: &str) -> Self {
        Self {
            name: name.into(),
            digest: Blake2s256::digest(value.as_bytes()).into(),
        }
    }

    /// The BLAKE2s-256 digest of the token value.
    #[must_use]
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenDigest")
            .field("name", &self.name)
            .field("digest", &"<redacted>")
            .finish()
    }
}

/// `[limits]`, converted for the runtime.
#[derive(Debug, Clone)]
pub struct LimitSettings {
    /// Hosts registered at once, in total.
    pub max_hosts: usize,
    /// Simultaneous channels per host.
    pub max_channels_per_host: usize,
    /// Largest WebSocket frame, in bytes (floor and ceiling: 70 000).
    pub max_frame_bytes: usize,
    /// Sustained bandwidth per channel and direction, in bytes per second.
    pub channel_bytes_per_second: u64,
    /// Burst allowed above the sustained rate, in bytes.
    pub channel_burst_bytes: u64,
    /// Frames that may wait in the host-to-client queue of one channel.
    pub channel_queue_frames: usize,
    /// Frames that may wait in the clients-to-host queue.
    pub host_queue_frames: usize,
    /// Time a host has to answer the registration challenge.
    pub register_timeout: Duration,
    /// Client connections accepted per IP per minute.
    pub client_connects_per_ip_per_minute: u64,
    /// Host registration attempts per IP per minute.
    pub host_attempts_per_ip_per_minute: u64,
    /// Presence queries per IP per minute.
    pub presence_per_minute_per_ip: u64,
}

/// `[log]`.
#[derive(Debug, Clone)]
pub struct LogSettings {
    /// Verbosity of the relay's own messages.
    pub level: LogLevel,
    /// Output format.
    pub format: LogFormat,
    /// How peer IPs appear in logs.
    pub ip_addresses: IpLogging,
}

/// Log verbosity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Warnings and errors.
    Warn,
    /// Normal operation (default).
    Info,
    /// Debugging detail.
    Debug,
    /// Everything.
    Trace,
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Human-readable lines (default).
    Text,
    /// One JSON object per line.
    Json,
}

/// How a peer IP is written to the logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IpLogging {
    /// The full address.
    Full,
    /// `/24` for IPv4, `/48` for IPv6 (default).
    Truncated,
    /// Not logged.
    None,
}

/// `[metrics]`.
#[derive(Debug, Clone)]
pub struct MetricsSettings {
    /// Whether the metrics listener runs.
    pub enabled: bool,
    /// Address of the metrics listener.
    pub bind: SocketAddr,
}

/// Where in the configuration a problem is.
#[derive(Debug)]
pub enum Location {
    /// The configuration file; `line` is 1-based, absent when the problem is
    /// not tied to one line (a missing section, a default value).
    File {
        /// The configuration file.
        path: PathBuf,
        /// The line of the offending key, when known.
        line: Option<usize>,
    },
    /// An environment variable.
    Environment {
        /// Name of the variable.
        variable: String,
    },
}

/// Why the configuration could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// The file (or a `value_file`) could not be read.
    Read {
        /// The file that failed.
        path: PathBuf,
        /// The operating system's reason.
        source: std::io::Error,
    },
    /// The file is not valid TOML, has an unknown key, or a value of the
    /// wrong type.
    Syntax {
        /// The configuration file.
        path: PathBuf,
        /// 1-based line of the problem, when known.
        line: Option<usize>,
        /// The key concerned, when it could be told.
        key: Option<String>,
        /// What is wrong.
        message: String,
    },
    /// A value is out of range or the combination is not allowed.
    Invalid {
        /// File line or environment variable.
        location: Location,
        /// The key, as `section.key` (`tokens[N].key` for tokens).
        key: String,
        /// What is wrong and, when useful, how to fix it.
        message: String,
    },
    /// A `PORECATU_RELAY_*` variable that names no key.
    UnknownVariable {
        /// Name of the variable.
        variable: String,
    },
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File {
                path,
                line: Some(line),
            } => write!(f, "{}:{line}", path.display()),
            Self::File { path, line: None } => write!(f, "{}", path.display()),
            Self::Environment { variable } => write!(f, "environment variable {variable}"),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "cannot read {}: {source}", path.display())
            }
            Self::Syntax {
                path,
                line,
                key,
                message,
            } => {
                write!(f, "{}", path.display())?;
                if let Some(line) = line {
                    write!(f, ":{line}")?;
                }
                if let Some(key) = key {
                    write!(f, ": {key}")?;
                }
                write!(f, ": {message}")
            }
            Self::Invalid {
                location,
                key,
                message,
            } => write!(f, "{location}: {key}: {message}"),
            Self::UnknownVariable { variable } => write!(
                f,
                "environment variable {variable}: no such setting (the format is \
                 PORECATU_RELAY_<SECTION>_<KEY>)"
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 1-based line of a byte offset in `text`.
pub(crate) fn line_of(text: &str, offset: usize) -> usize {
    let end = offset.min(text.len());
    text.as_bytes()[..end]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

/// Turns a TOML error into a [`ConfigError::Syntax`] without echoing source
/// text: a line can hold a token, and an error message must never print one.
fn syntax_error(path: &Path, text: &str, error: &toml::de::Error) -> ConfigError {
    let span = error.span();
    let line = span.as_ref().map(|span| line_of(text, span.start));
    let key = span.as_ref().and_then(|span| key_before(text, span.start));
    // The detail of an error on a secret value is not shown: `toml` quotes the
    // offending value in some messages.
    let message = if key.as_deref().is_some_and(|key| key.ends_with("value")) {
        "invalid value (not shown)".to_owned()
    } else {
        error.message().to_owned()
    };
    ConfigError::Syntax {
        path: path.to_owned(),
        line,
        key,
        message,
    }
}

/// The key written before `offset` on its line (`bind = ` gives `bind`), if
/// the offset is a value position.
fn key_before(text: &str, offset: usize) -> Option<String> {
    let offset = offset.min(text.len());
    let line_start = text[..offset].rfind('\n').map_or(0, |index| index + 1);
    let before = text.get(line_start..offset)?.trim_end();
    let key = before.strip_suffix('=')?.trim();
    (!key.is_empty()).then(|| key.to_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const EXAMPLE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/reference/relay.example.toml"
    );
    const TOKEN: &str = "token-de-desenvolvimento-nao-use-em-producao";

    /// A scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "porecatu-relay-config-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, name: &str, content: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, content).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn no_env() -> std::iter::Empty<(String, String)> {
        std::iter::empty()
    }

    fn env(pairs: &[(&str, &str)]) -> std::vec::IntoIter<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn load_text(text: &str) -> Result<Config, ConfigError> {
        let scratch = Scratch::new();
        let path = scratch.write("relay.toml", text);
        load(&path, no_env())
    }

    fn token_block() -> String {
        format!("[[tokens]]\nname = \"a\"\nvalue = \"{TOKEN}\"\n")
    }

    fn error_text(result: Result<Config, ConfigError>) -> String {
        result
            .expect_err("the configuration must be refused")
            .to_string()
    }

    #[test]
    fn example_file_loads_with_the_documented_values() {
        let config = load(Path::new(EXAMPLE), no_env()).expect("the example file is valid");
        let s = config.settings();
        assert_eq!(s.server.bind, "127.0.0.1:8080".parse().unwrap());
        assert!(!s.server.behind_proxy);
        assert!(s.server.trusted_proxies.is_empty());
        assert_eq!(s.server.shutdown_grace, Duration::from_secs(10));
        assert!(s.tls.is_none());
        assert_eq!(s.tokens.len(), 1);
        assert_eq!(s.tokens[0].name, "desenvolvimento");
        assert_eq!(s.tokens[0], TokenDigest::new("desenvolvimento", TOKEN));
        let l = &s.limits;
        assert_eq!(l.max_hosts, 1000);
        assert_eq!(l.max_channels_per_host, 16);
        assert_eq!(l.max_frame_bytes, 70_000);
        assert_eq!(l.channel_bytes_per_second, 1_000_000);
        assert_eq!(l.channel_burst_bytes, 1024 * 1024);
        assert_eq!(l.channel_queue_frames, 64);
        assert_eq!(l.host_queue_frames, 256);
        assert_eq!(l.register_timeout, Duration::from_secs(10));
        assert_eq!(l.client_connects_per_ip_per_minute, 30);
        assert_eq!(l.host_attempts_per_ip_per_minute, 10);
        assert_eq!(l.presence_per_minute_per_ip, 120);
        assert_eq!(s.log.level, LogLevel::Info);
        assert_eq!(s.log.format, LogFormat::Text);
        assert_eq!(s.log.ip_addresses, IpLogging::Truncated);
        assert!(!s.metrics.enabled);
        assert_eq!(s.metrics.bind, "127.0.0.1:9464".parse().unwrap());
    }

    #[test]
    fn built_in_defaults_match_the_default_settings() {
        let config = load_text(&format!(
            "[server]\nbind = \"0.0.0.0:443\"\nbehind_proxy = true\n{}",
            token_block()
        ))
        .unwrap();
        let loaded = config.settings();
        let default = Settings::default();
        assert_eq!(loaded.server.bind, default.server.bind);
        assert_eq!(loaded.limits.max_hosts, default.limits.max_hosts);
        assert_eq!(
            loaded.limits.channel_bytes_per_second,
            default.limits.channel_bytes_per_second
        );
        assert_eq!(
            loaded.limits.channel_burst_bytes,
            default.limits.channel_burst_bytes
        );
        assert_eq!(loaded.log.level, default.log.level);
        assert_eq!(loaded.metrics.bind, default.metrics.bind);
        assert_eq!(loaded.server.shutdown_grace, default.server.shutdown_grace);
    }

    #[test]
    fn unknown_key_is_an_error_with_the_line() {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:8080\"\nbihnd = 1\n{}",
            token_block()
        );
        let message = error_text(load_text(&text));
        assert!(message.contains(":3"), "{message}");
        assert!(message.contains("bihnd"), "{message}");
    }

    #[test]
    fn unknown_section_is_an_error() {
        let text = format!("[serverr]\nx = 1\n{}", token_block());
        let message = error_text(load_text(&text));
        assert!(message.contains("serverr"), "{message}");
    }

    #[test]
    fn bad_bind_is_an_error_with_line_and_key() {
        let text = format!("[server]\nbind = \"not-an-address\"\n{}", token_block());
        let message = error_text(load_text(&text));
        assert!(message.contains(":2"), "{message}");
        assert!(message.contains("bind"), "{message}");
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let error = load(Path::new("no-such-dir/relay.toml"), no_env()).unwrap_err();
        assert!(matches!(error, ConfigError::Read { .. }));
        assert!(error.to_string().contains("relay.toml"));
    }

    fn assert_floor(limit_line: &str, key: &str) {
        // Lines: 1 [server], 2 bind, 3-5 token, 6 [limits], 7 the limit.
        let text = format!(
            "[server]\nbind = \"127.0.0.1:8080\"\n{}[limits]\n{limit_line}\n",
            token_block()
        );
        let message = error_text(load_text(&text));
        assert!(message.contains(key), "{key}: {message}");
        assert!(message.contains(":7"), "{key}: {message}");
    }

    #[test]
    fn every_floor_is_enforced_with_its_key_and_line() {
        assert_floor("max_channels_per_host = 7", "limits.max_channels_per_host");
        assert_floor("max_frame_bytes = 69999", "limits.max_frame_bytes");
        assert_floor("max_frame_bytes = 70001", "limits.max_frame_bytes");
        assert_floor(
            "channel_bandwidth_kbps = 1999",
            "limits.channel_bandwidth_kbps",
        );
        assert_floor(
            "register_timeout_seconds = 9",
            "limits.register_timeout_seconds",
        );
        assert_floor("channel_burst_kib = 68", "limits.channel_burst_kib");
        assert_floor("max_hosts = 0", "limits.max_hosts");
        assert_floor("channel_queue_frames = 0", "limits.channel_queue_frames");
        assert_floor("host_queue_frames = 0", "limits.host_queue_frames");
        assert_floor(
            "client_connects_per_ip_per_minute = 0",
            "limits.client_connects_per_ip_per_minute",
        );
        assert_floor(
            "host_attempts_per_ip_per_minute = 0",
            "limits.host_attempts_per_ip_per_minute",
        );
        assert_floor(
            "presence_per_minute_per_ip = 0",
            "limits.presence_per_minute_per_ip",
        );
    }

    #[test]
    fn floors_are_accepted_exactly_at_the_floor() {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:8080\"\n{}[limits]\nmax_channels_per_host = 8\n\
             channel_bandwidth_kbps = 2000\nregister_timeout_seconds = 10\nchannel_burst_kib = 69\n",
            token_block()
        );
        let settings = load_text(&text).unwrap().settings();
        assert_eq!(settings.limits.channel_bytes_per_second, 250_000);
        assert_eq!(settings.limits.channel_burst_bytes, 69 * 1024);
    }

    #[test]
    fn public_bind_without_tls_is_refused() {
        let text = format!("[server]\nbind = \"0.0.0.0:443\"\n{}", token_block());
        let message = error_text(load_text(&text));
        assert!(message.contains("server.bind"), "{message}");
        assert!(message.contains(":2"), "{message}");
        assert!(message.contains("behind_proxy"), "{message}");
        assert!(message.contains("[tls]"), "{message}");
    }

    #[test]
    fn default_bind_without_tls_is_refused() {
        let message = error_text(load_text(&token_block()));
        assert!(message.contains("server.bind"), "{message}");
    }

    #[test]
    fn behind_proxy_allows_a_public_bind() {
        let text = format!(
            "[server]\nbind = \"0.0.0.0:8080\"\nbehind_proxy = true\n{}",
            token_block()
        );
        assert!(load_text(&text).is_ok());
    }

    #[test]
    fn loopback_bind_without_tls_is_fine_in_both_families() {
        for bind in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
            let text = format!("[server]\nbind = \"{bind}\"\n{}", token_block());
            assert!(load_text(&text).is_ok(), "{bind}");
        }
    }

    #[test]
    fn tls_section_needs_both_files() {
        let text = format!(
            "[server]\nbind = \"0.0.0.0:443\"\n{}[tls]\ncert_file = \"c.pem\"\n",
            token_block()
        );
        let message = error_text(load_text(&text));
        assert!(message.contains("tls.key_file"), "{message}");

        let text = format!(
            "[server]\nbind = \"0.0.0.0:443\"\n{}[tls]\nkey_file = \"k.pem\"\n",
            token_block()
        );
        assert!(error_text(load_text(&text)).contains("tls.cert_file"));
    }

    #[test]
    fn tls_section_makes_a_public_bind_valid() {
        let text = format!(
            "[server]\nbind = \"0.0.0.0:443\"\n{}[tls]\ncert_file = \"c.pem\"\nkey_file = \"k.pem\"\n\
             min_version = \"1.2\"\nreload_check_seconds = 60\n",
            token_block()
        );
        let tls = load_text(&text).unwrap().settings().tls.unwrap();
        assert_eq!(tls.cert_file, PathBuf::from("c.pem"));
        assert_eq!(tls.key_file, PathBuf::from("k.pem"));
        assert_eq!(tls.min_version, TlsVersion::V1_2);
        assert_eq!(tls.reload_check, Duration::from_secs(60));
    }

    #[test]
    fn tls_min_version_below_1_2_is_refused() {
        let text = format!(
            "[server]\nbind = \"0.0.0.0:443\"\n{}[tls]\ncert_file = \"c\"\nkey_file = \"k\"\nmin_version = \"1.1\"\n",
            token_block()
        );
        assert!(error_text(load_text(&text)).contains("min_version"));
    }

    #[test]
    fn no_token_is_refused() {
        let message = error_text(load_text("[server]\nbind = \"127.0.0.1:1\"\n"));
        assert!(message.contains("tokens"), "{message}");
    }

    #[test]
    fn short_and_long_tokens_are_refused() {
        for value in ["curto".to_owned(), "x".repeat(129)] {
            let text = format!(
                "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue = \"{value}\"\n"
            );
            let message = error_text(load_text(&text));
            assert!(message.contains("tokens[0].value"), "{message}");
            assert!(message.contains(":5"), "{message}");
            assert!(
                !message.contains(&value),
                "the message must not echo the token"
            );
        }
        let edge = |len: usize| {
            format!(
                "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue = \"{}\"\n",
                "x".repeat(len)
            )
        };
        assert!(load_text(&edge(32)).is_ok());
        assert!(load_text(&edge(128)).is_ok());
        assert!(load_text(&edge(31)).is_err());
    }

    #[test]
    fn non_ascii_token_is_refused() {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue = \"{}\"\n",
            "á".repeat(40)
        );
        assert!(error_text(load_text(&text)).contains("ASCII"));
    }

    #[test]
    fn token_of_the_wrong_type_does_not_echo_the_value() {
        let text =
            "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue = 123456789\n";
        let message = error_text(load_text(text));
        assert!(message.contains(":5"), "{message}");
        assert!(!message.contains("123456789"), "{message}");
    }

    #[test]
    fn duplicate_token_names_are_refused() {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:1\"\n{}[[tokens]]\nname = \"a\"\nvalue = \"{TOKEN}2\"\n",
            token_block()
        );
        let message = error_text(load_text(&text));
        assert!(message.contains("tokens[1].name"), "{message}");
        assert!(message.contains(":7"), "{message}");
    }

    #[test]
    fn value_and_value_file_together_are_refused() {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue = \"{TOKEN}\"\nvalue_file = \"t.txt\"\n"
        );
        let message = error_text(load_text(&text));
        assert!(message.contains("value_file"), "{message}");
        assert!(message.contains("not both"), "{message}");
    }

    #[test]
    fn token_without_value_is_refused() {
        let text = "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\n";
        assert!(error_text(load_text(text)).contains("tokens[0].value"));
    }

    #[test]
    fn value_file_is_read_relative_to_the_config_with_the_final_newline_removed() {
        let scratch = Scratch::new();
        scratch.write("secret.txt", &format!("{TOKEN}\r\n"));
        let path = scratch.write(
            "relay.toml",
            "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue_file = \"secret.txt\"\n",
        );
        let settings = load(&path, no_env()).unwrap().settings();
        assert_eq!(settings.tokens[0], TokenDigest::new("a", TOKEN));
    }

    #[test]
    fn missing_value_file_is_refused_with_its_line() {
        let text = "[server]\nbind = \"127.0.0.1:1\"\n[[tokens]]\nname = \"a\"\nvalue_file = \"nope.txt\"\n";
        let message = error_text(load_text(text));
        assert!(message.contains("tokens[0].value_file"), "{message}");
        assert!(message.contains(":5"), "{message}");
    }

    #[test]
    fn token_debug_never_shows_the_digest() {
        let digest = TokenDigest::new("a", TOKEN);
        assert!(format!("{digest:?}").contains("<redacted>"));
    }

    #[test]
    fn trusted_proxies_accept_addresses_and_networks() {
        let text = format!(
            "[server]\nbind = \"127.0.0.1:1\"\ntrusted_proxies = [\"10.0.0.1\", \"192.168.0.0/16\", \"::1\"]\n{}",
            token_block()
        );
        let proxies = load_text(&text).unwrap().settings().server.trusted_proxies;
        assert_eq!(proxies.len(), 3);
        assert_eq!(proxies[0].to_string(), "10.0.0.1/32");
        assert_eq!(proxies[1].to_string(), "192.168.0.0/16");

        let text = format!(
            "[server]\nbind = \"127.0.0.1:1\"\ntrusted_proxies = [\"banana\"]\n{}",
            token_block()
        );
        assert!(error_text(load_text(&text)).contains("server.trusted_proxies"));
    }

    fn loopback_file() -> (Scratch, PathBuf) {
        let scratch = Scratch::new();
        let path = scratch.write(
            "relay.toml",
            &format!("[server]\nbind = \"127.0.0.1:8080\"\n{}", token_block()),
        );
        (scratch, path)
    }

    #[test]
    fn environment_overrides_a_scalar() {
        let (_scratch, path) = loopback_file();
        let settings = load(
            &path,
            env(&[
                ("PORECATU_RELAY_SERVER_BIND", "127.0.0.1:9999"),
                ("PORECATU_RELAY_LIMITS_MAX_CHANNELS_PER_HOST", "32"),
                ("PORECATU_RELAY_LOG_FORMAT", "json"),
                ("PORECATU_RELAY_METRICS_ENABLED", "true"),
                ("UNRELATED", "ignored"),
            ]),
        )
        .unwrap()
        .settings();
        assert_eq!(settings.server.bind, "127.0.0.1:9999".parse().unwrap());
        assert_eq!(settings.limits.max_channels_per_host, 32);
        assert_eq!(settings.log.format, LogFormat::Json);
        assert!(settings.metrics.enabled);
    }

    #[test]
    fn environment_overrides_trusted_proxies_with_a_comma_list() {
        let (_scratch, path) = loopback_file();
        let settings = load(
            &path,
            env(&[(
                "PORECATU_RELAY_SERVER_TRUSTED_PROXIES",
                "10.0.0.1, 172.16.0.0/12",
            )]),
        )
        .unwrap()
        .settings();
        let shown: Vec<String> = settings
            .server
            .trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(shown, ["10.0.0.1/32", "172.16.0.0/12"]);

        let cleared = load(&path, env(&[("PORECATU_RELAY_SERVER_TRUSTED_PROXIES", "")]))
            .unwrap()
            .settings();
        assert!(cleared.server.trusted_proxies.is_empty());
    }

    #[test]
    fn environment_feeds_the_validation() {
        let (_scratch, path) = loopback_file();
        let message = error_text(load(
            &path,
            env(&[("PORECATU_RELAY_SERVER_BIND", "0.0.0.0:443")]),
        ));
        assert!(message.contains("PORECATU_RELAY_SERVER_BIND"), "{message}");

        let behind = load(
            &path,
            env(&[
                ("PORECATU_RELAY_SERVER_BIND", "0.0.0.0:443"),
                ("PORECATU_RELAY_SERVER_BEHIND_PROXY", "true"),
            ]),
        );
        assert!(behind.is_ok());
    }

    #[test]
    fn environment_errors_name_the_variable() {
        let (_scratch, path) = loopback_file();
        for (name, value) in [
            ("PORECATU_RELAY_LIMITS_MAX_HOSTS", "many"),
            ("PORECATU_RELAY_LIMITS_MAX_HOSTS", "0"),
            ("PORECATU_RELAY_SERVER_BEHIND_PROXY", "maybe"),
            ("PORECATU_RELAY_SERVER_BIND", "nope"),
            ("PORECATU_RELAY_LOG_LEVEL", "loud"),
            ("PORECATU_RELAY_LIMITS_NO_SUCH_KEY", "1"),
            ("PORECATU_RELAY_NOSECTION", "1"),
        ] {
            let message = error_text(load(&path, env(&[(name, value)])));
            assert!(message.contains(name), "{name}={value}: {message}");
        }
    }

    #[test]
    fn tokens_cannot_come_from_the_environment() {
        let (_scratch, path) = loopback_file();
        let message = error_text(load(&path, env(&[("PORECATU_RELAY_TOKENS_VALUE", TOKEN)])));
        assert!(message.contains("PORECATU_RELAY_TOKENS_VALUE"), "{message}");
        assert!(message.contains("only"), "{message}");
        assert!(!message.contains(TOKEN));
    }

    #[test]
    fn environment_can_configure_tls_when_the_file_has_none() {
        let scratch = Scratch::new();
        let path = scratch.write(
            "relay.toml",
            &format!("[server]\nbind = \"0.0.0.0:443\"\n{}", token_block()),
        );
        let settings = load(
            &path,
            env(&[
                ("PORECATU_RELAY_TLS_CERT_FILE", "/certs/fullchain.pem"),
                ("PORECATU_RELAY_TLS_KEY_FILE", "/certs/privkey.pem"),
            ]),
        )
        .unwrap()
        .settings();
        assert_eq!(
            settings.tls.unwrap().cert_file,
            PathBuf::from("/certs/fullchain.pem")
        );
    }

    #[test]
    fn settings_default_can_be_tweaked_without_validation() {
        let mut settings = Settings::default();
        settings.limits.register_timeout = Duration::from_millis(50);
        assert_eq!(settings.limits.register_timeout, Duration::from_millis(50));
    }

    #[test]
    fn default_path_names_relay_toml() {
        let path = default_path();
        assert_eq!(path.file_name().unwrap(), "relay.toml");
        #[cfg(target_os = "linux")]
        assert_eq!(path, PathBuf::from("/etc/porecatu-relay/relay.toml"));
    }
}
