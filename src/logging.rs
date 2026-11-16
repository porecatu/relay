// SPDX-License-Identifier: GPL-3.0-or-later

//! Structured logging to `stderr` (PRD-001 RF-1.17, RF-1.18).
//!
//! The configured level applies to this crate's own targets only. The crates
//! that handle the bytes of the wire (WebSocket, HTTP, TLS, tower) are held at
//! `info` at most, whatever the level: at `debug` and `trace` some of them
//! would log frames, which the relay must never do (ADR-0002). `RUST_LOG` is
//! ignored on purpose, and no `log` to `tracing` bridge is installed, so a
//! dependency that still logs through `log` reaches no output.

use std::fmt;
use std::io;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use tracing::Subscriber;
use tracing::subscriber::{DefaultGuard, SetGlobalDefaultError};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::{Layer as _, fmt as tracing_fmt};

/// Targets of the dependencies that touch the wire, capped at `info`.
const CAPPED_TARGETS: &[&str] = &[
    "tungstenite",
    "tokio_tungstenite",
    "hyper",
    "hyper_util",
    "h2",
    "rustls",
    "axum",
    "axum_server",
    "tower",
    "tower_http",
];

/// The target this crate logs under.
const OWN_TARGET: &str = "porecatu_relay";

/// Verbosity of the relay's own logs (`[log] level`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    /// Errors only.
    Error,
    /// Errors and warnings.
    Warn,
    /// The default: operational events.
    #[default]
    Info,
    /// Detail for diagnosis.
    Debug,
    /// Everything the relay can say (still never a payload).
    Trace,
}

impl LogLevel {
    fn filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
            Self::Trace => LevelFilter::TRACE,
        }
    }

    /// The level applied to the capped dependencies: the configured one when
    /// it is quieter than `info`, `info` otherwise.
    fn capped_filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info | Self::Debug | Self::Trace => LevelFilter::INFO,
        }
    }
}

/// Layout of each log line (`[log] format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human readable.
    #[default]
    Text,
    /// One JSON object per line, for a log collector.
    Json,
}

/// Error of parsing a [`LogLevel`] or a [`LogFormat`] from text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseLogSettingError {
    what: &'static str,
    value: String,
    expected: &'static str,
}

impl fmt::Display for ParseLogSettingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown log {} {:?}: expected {}",
            self.what, self.value, self.expected
        )
    }
}

impl std::error::Error for ParseLogSettingError {}

impl FromStr for LogLevel {
    type Err = ParseLogSettingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            other => Err(ParseLogSettingError {
                what: "level",
                value: other.to_owned(),
                expected: "\"error\", \"warn\", \"info\", \"debug\" or \"trace\"",
            }),
        }
    }
}

impl FromStr for LogFormat {
    type Err = ParseLogSettingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(ParseLogSettingError {
                what: "format",
                value: other.to_owned(),
                expected: "\"text\" or \"json\"",
            }),
        }
    }
}

/// Installs the process-wide subscriber, writing to `stderr`.
///
/// # Errors
///
/// Fails if a global subscriber is already installed.
pub fn init(level: LogLevel, format: LogFormat) -> Result<(), SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(build(level, format, io::stderr))
}

/// Installs a subscriber for the current thread only, writing to `writer`,
/// and returns the guard that removes it on drop.
///
/// It applies the same filter as [`init`], so a test sees what production
/// would write. Being per thread, it is meant for tests running on a
/// single-threaded runtime (`#[tokio::test]`'s default) or without one.
#[must_use = "the subscriber is removed when the guard is dropped"]
pub fn init_for_test<W>(level: LogLevel, format: LogFormat, writer: W) -> DefaultGuard
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    tracing::subscriber::set_default(build(level, format, writer))
}

fn build<W>(level: LogLevel, format: LogFormat, writer: W) -> impl Subscriber + Send + Sync
where
    W: for<'w> MakeWriter<'w> + Send + Sync + 'static,
{
    let mut targets = Targets::new()
        .with_default(level.capped_filter())
        .with_target(OWN_TARGET, level.filter());
    for target in CAPPED_TARGETS {
        targets = targets.with_target(*target, level.capped_filter());
    }
    let layer = match format {
        LogFormat::Text => tracing_fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(targets)
            .boxed(),
        LogFormat::Json => tracing_fmt::layer()
            .json()
            .with_writer(writer)
            .with_filter(targets)
            .boxed(),
    };
    Registry::default().with(layer)
}

/// A shared in-memory buffer to capture the log in tests.
///
/// Clones share the same bytes, so one clone goes to [`init_for_test`] and
/// another stays with the test to read what was written.
#[derive(Debug, Clone, Default)]
pub struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl CapturedLog {
    /// An empty buffer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything written so far, as text (invalid UTF-8 is replaced).
    #[must_use]
    pub fn contents(&self) -> String {
        let bytes = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

impl io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl MakeWriter<'_> for CapturedLog {
    type Writer = CapturedLog;

    fn make_writer(&self) -> Self::Writer {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    /// Emits one event per capped target and one from the crate, at `level`.
    fn emit_all(level: Level) {
        macro_rules! at {
            ($target:literal) => {
                match level {
                    Level::ERROR => tracing::event!(target: $target, Level::ERROR, "from {}", $target),
                    Level::WARN => tracing::event!(target: $target, Level::WARN, "from {}", $target),
                    Level::INFO => tracing::event!(target: $target, Level::INFO, "from {}", $target),
                    Level::DEBUG => tracing::event!(target: $target, Level::DEBUG, "from {}", $target),
                    Level::TRACE => tracing::event!(target: $target, Level::TRACE, "from {}", $target),
                }
            };
        }
        at!("tungstenite");
        at!("tokio_tungstenite");
        at!("tokio_tungstenite::compat");
        at!("hyper");
        at!("hyper_util");
        at!("h2");
        at!("rustls");
        at!("axum");
        at!("tower");
        at!("tower_http");
        at!("porecatu_relay");
        at!("porecatu_relay::server");
    }

    #[test]
    fn dependencies_stay_quiet_below_info_even_at_trace() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Trace, LogFormat::Text, log.clone());
        emit_all(Level::TRACE);
        emit_all(Level::DEBUG);
        let out = log.contents();
        for target in CAPPED_TARGETS {
            assert!(
                !out.contains(&format!("from {target}")),
                "{target} leaked:\n{out}"
            );
        }
        assert!(!out.contains("from tokio_tungstenite::compat"), "{out}");
        assert!(out.contains("from porecatu_relay\n"), "{out}");
        assert!(out.contains("from porecatu_relay::server"), "{out}");
    }

    #[test]
    fn dependencies_still_log_at_info_and_above() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Trace, LogFormat::Text, log.clone());
        emit_all(Level::INFO);
        emit_all(Level::WARN);
        let out = log.contents();
        assert!(out.contains("from tungstenite\n"), "{out}");
        assert!(out.contains("from hyper\n"), "{out}");
    }

    #[test]
    fn own_events_follow_the_configured_level() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Warn, LogFormat::Text, log.clone());
        emit_all(Level::INFO);
        assert_eq!(log.contents(), "");
        emit_all(Level::WARN);
        let out = log.contents();
        assert!(out.contains("from porecatu_relay\n"), "{out}");
        // A dependency is capped at the configured level when it is quieter.
        assert!(out.contains("from tungstenite\n"), "{out}");
    }

    #[test]
    fn dependencies_follow_a_level_quieter_than_info() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Error, LogFormat::Text, log.clone());
        emit_all(Level::WARN);
        assert_eq!(log.contents(), "");
    }

    #[test]
    fn unknown_targets_do_not_exceed_info() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Trace, LogFormat::Text, log.clone());
        tracing::event!(target: "some_other_crate", Level::DEBUG, "from other debug");
        tracing::event!(target: "some_other_crate", Level::INFO, "from other info");
        let out = log.contents();
        assert!(!out.contains("from other debug"), "{out}");
        assert!(out.contains("from other info"), "{out}");
    }

    #[test]
    fn json_format_is_one_object_per_line() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Info, LogFormat::Json, log.clone());
        tracing::info!(host_id = "abc", bytes_in = 7_u64, "registered");
        let out = log.contents();
        let line = out.lines().next().expect("one line");
        assert!(line.starts_with('{') && line.ends_with('}'), "{out}");
        assert!(line.contains("\"host_id\":\"abc\""), "{out}");
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn text_output_has_no_ansi_escapes() {
        let log = CapturedLog::new();
        let _guard = init_for_test(LogLevel::Info, LogFormat::Text, log.clone());
        tracing::info!("hello");
        assert!(!log.contents().contains('\u{1b}'));
    }

    #[test]
    fn rust_log_is_ignored() {
        // The filter is built from `Targets`, never from the environment, so
        // there is no code path that reads `RUST_LOG`; this fixes it in a test.
        let source = include_str!("logging.rs");
        let needle = ["Env", "Filter"].concat();
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains(&needle));
        assert!(!production.contains(&["from_", "default_env"].concat()));
    }

    #[test]
    fn level_and_format_parse_from_the_config_names() {
        assert_eq!("error".parse(), Ok(LogLevel::Error));
        assert_eq!("warn".parse(), Ok(LogLevel::Warn));
        assert_eq!("info".parse(), Ok(LogLevel::Info));
        assert_eq!("debug".parse(), Ok(LogLevel::Debug));
        assert_eq!("trace".parse(), Ok(LogLevel::Trace));
        assert_eq!("text".parse(), Ok(LogFormat::Text));
        assert_eq!("json".parse(), Ok(LogFormat::Json));
        let err = "verbose".parse::<LogLevel>().unwrap_err();
        assert!(err.to_string().contains("\"verbose\""));
        let err = "xml".parse::<LogFormat>().unwrap_err();
        assert!(err.to_string().contains("\"xml\""));
    }
}
