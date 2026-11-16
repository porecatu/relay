// SPDX-License-Identifier: GPL-3.0-or-later

//! The raw configuration model: what `relay.toml` and the environment say,
//! before validation.
//!
//! Names and defaults mirror `docs/reference/relay.example.toml`. Every value
//! remembers where it came from ([`Origin`]) so that validation errors can
//! point at a file line or at an environment variable.

use std::net::SocketAddr;
use std::ops::Range;
use std::path::PathBuf;

use serde::{Deserialize, Deserializer};

use super::{IpLogging, LogFormat, LogLevel, TlsVersion};

/// Default `[server] bind`.
pub(crate) const DEFAULT_BIND: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 443);
/// Default `[server] shutdown_grace_seconds`.
pub(crate) const DEFAULT_SHUTDOWN_GRACE_SECONDS: u64 = 10;
/// Default `[tls] reload_check_seconds`.
pub(crate) const DEFAULT_TLS_RELOAD_CHECK_SECONDS: u64 = 300;
/// Default `[limits] max_hosts`.
pub(crate) const DEFAULT_MAX_HOSTS: u64 = 1000;
/// Default `[limits] max_channels_per_host`.
pub(crate) const DEFAULT_MAX_CHANNELS_PER_HOST: u64 = 16;
/// Default `[limits] max_frame_bytes` (also the only accepted value).
pub(crate) const DEFAULT_MAX_FRAME_BYTES: u64 = 70_000;
/// Default `[limits] channel_bandwidth_kbps`.
pub(crate) const DEFAULT_CHANNEL_BANDWIDTH_KBPS: u64 = 8000;
/// Default `[limits] channel_burst_kib`.
pub(crate) const DEFAULT_CHANNEL_BURST_KIB: u64 = 1024;
/// Default `[limits] channel_queue_frames`.
pub(crate) const DEFAULT_CHANNEL_QUEUE_FRAMES: u64 = 64;
/// Default `[limits] host_queue_frames`.
pub(crate) const DEFAULT_HOST_QUEUE_FRAMES: u64 = 256;
/// Default `[limits] register_timeout_seconds`.
pub(crate) const DEFAULT_REGISTER_TIMEOUT_SECONDS: u64 = 10;
/// Default `[limits] client_connects_per_ip_per_minute`.
pub(crate) const DEFAULT_CLIENT_CONNECTS_PER_IP_PER_MINUTE: u64 = 30;
/// Default `[limits] host_attempts_per_ip_per_minute`.
pub(crate) const DEFAULT_HOST_ATTEMPTS_PER_IP_PER_MINUTE: u64 = 10;
/// Default `[limits] presence_per_minute_per_ip`.
pub(crate) const DEFAULT_PRESENCE_PER_MINUTE_PER_IP: u64 = 120;
/// Default `[metrics] bind`.
pub(crate) const DEFAULT_METRICS_BIND: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9464);

/// Where a configuration value came from.
#[derive(Debug, Clone)]
pub(crate) enum Origin {
    /// Not set anywhere: the built-in default.
    Default,
    /// A byte range of the configuration file.
    File(Range<usize>),
    /// An environment variable, by name.
    Env(String),
}

/// A value together with the place it was set.
#[derive(Debug, Clone)]
pub(crate) struct Field<T> {
    pub(crate) value: T,
    pub(crate) origin: Origin,
}

impl<T> Field<T> {
    /// A field holding a built-in default.
    pub(crate) fn default_value(value: T) -> Self {
        Self {
            value,
            origin: Origin::Default,
        }
    }

    /// A field set by an environment variable.
    pub(crate) fn from_env(value: T, variable: &str) -> Self {
        Self {
            value,
            origin: Origin::Env(variable.to_owned()),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spanned = toml::Spanned::<T>::deserialize(deserializer)?;
        Ok(Self {
            origin: Origin::File(spanned.span()),
            value: spanned.into_inner(),
        })
    }
}

/// The whole file. No `Debug`: it holds token values.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Model {
    pub(crate) server: Server,
    pub(crate) tls: Option<Field<Tls>>,
    pub(crate) tokens: Vec<Token>,
    pub(crate) limits: Limits,
    pub(crate) log: Log,
    pub(crate) metrics: Metrics,
}

/// `[server]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Server {
    pub(crate) bind: Field<SocketAddr>,
    pub(crate) behind_proxy: Field<bool>,
    pub(crate) trusted_proxies: Field<Vec<String>>,
    pub(crate) shutdown_grace_seconds: Field<u64>,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            bind: Field::default_value(DEFAULT_BIND),
            behind_proxy: Field::default_value(false),
            trusted_proxies: Field::default_value(Vec::new()),
            shutdown_grace_seconds: Field::default_value(DEFAULT_SHUTDOWN_GRACE_SECONDS),
        }
    }
}

/// `[tls]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Tls {
    pub(crate) cert_file: Option<Field<PathBuf>>,
    pub(crate) key_file: Option<Field<PathBuf>>,
    pub(crate) min_version: Field<TlsVersion>,
    pub(crate) reload_check_seconds: Field<u64>,
}

impl Default for Tls {
    fn default() -> Self {
        Self {
            cert_file: None,
            key_file: None,
            min_version: Field::default_value(TlsVersion::V1_3),
            reload_check_seconds: Field::default_value(DEFAULT_TLS_RELOAD_CHECK_SECONDS),
        }
    }
}

/// One `[[tokens]]` entry. No `Debug`: `value` is a secret.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Token {
    pub(crate) name: Option<Field<String>>,
    pub(crate) value: Option<Field<String>>,
    pub(crate) value_file: Option<Field<PathBuf>>,
}

/// `[limits]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Limits {
    pub(crate) max_hosts: Field<u64>,
    pub(crate) max_channels_per_host: Field<u64>,
    pub(crate) max_frame_bytes: Field<u64>,
    pub(crate) channel_bandwidth_kbps: Field<u64>,
    pub(crate) channel_burst_kib: Field<u64>,
    pub(crate) channel_queue_frames: Field<u64>,
    pub(crate) host_queue_frames: Field<u64>,
    pub(crate) register_timeout_seconds: Field<u64>,
    pub(crate) client_connects_per_ip_per_minute: Field<u64>,
    pub(crate) host_attempts_per_ip_per_minute: Field<u64>,
    pub(crate) presence_per_minute_per_ip: Field<u64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_hosts: Field::default_value(DEFAULT_MAX_HOSTS),
            max_channels_per_host: Field::default_value(DEFAULT_MAX_CHANNELS_PER_HOST),
            max_frame_bytes: Field::default_value(DEFAULT_MAX_FRAME_BYTES),
            channel_bandwidth_kbps: Field::default_value(DEFAULT_CHANNEL_BANDWIDTH_KBPS),
            channel_burst_kib: Field::default_value(DEFAULT_CHANNEL_BURST_KIB),
            channel_queue_frames: Field::default_value(DEFAULT_CHANNEL_QUEUE_FRAMES),
            host_queue_frames: Field::default_value(DEFAULT_HOST_QUEUE_FRAMES),
            register_timeout_seconds: Field::default_value(DEFAULT_REGISTER_TIMEOUT_SECONDS),
            client_connects_per_ip_per_minute: Field::default_value(
                DEFAULT_CLIENT_CONNECTS_PER_IP_PER_MINUTE,
            ),
            host_attempts_per_ip_per_minute: Field::default_value(
                DEFAULT_HOST_ATTEMPTS_PER_IP_PER_MINUTE,
            ),
            presence_per_minute_per_ip: Field::default_value(DEFAULT_PRESENCE_PER_MINUTE_PER_IP),
        }
    }
}

/// `[log]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Log {
    pub(crate) level: Field<LogLevel>,
    pub(crate) format: Field<LogFormat>,
    pub(crate) ip_addresses: Field<IpLogging>,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: Field::default_value(LogLevel::Info),
            format: Field::default_value(LogFormat::Text),
            ip_addresses: Field::default_value(IpLogging::Truncated),
        }
    }
}

/// `[metrics]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(crate) struct Metrics {
    pub(crate) enabled: Field<bool>,
    pub(crate) bind: Field<SocketAddr>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            enabled: Field::default_value(false),
            bind: Field::default_value(DEFAULT_METRICS_BIND),
        }
    }
}
