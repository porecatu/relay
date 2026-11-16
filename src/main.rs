// SPDX-License-Identifier: GPL-3.0-or-later

//! Entry point of the `porecatu-relay` binary.
//!
//! Exit codes: 0 success, 1 runtime failure (the listener, the server), 2 usage
//! or configuration error (the relay did not start).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::Arc;

use porecatu_relay::cli::{self, Command, DEFAULT_TOKEN_NAME};
use porecatu_relay::config::{self, LogFormat, LogLevel};
use porecatu_relay::server::{self, ServerState};
use porecatu_relay::shutdown::{self, Outcome};
use porecatu_relay::token::{TokenSet, generate_token};
use porecatu_relay::{ENVELOPE_PATH_VERSION, logging};
use porecatu_relay::{metrics, tls};
use tokio_util::sync::CancellationToken;

fn main() -> ExitCode {
    match cli::parse(std::env::args().skip(1)) {
        Ok(Command::Version) => {
            println!(
                "porecatu-relay {} (envelope /{ENVELOPE_PATH_VERSION}/)",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
        Ok(Command::Help) => {
            println!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        Ok(Command::TokenNew { name }) => {
            let name = name.as_deref().unwrap_or(DEFAULT_TOKEN_NAME);
            let value = generate_token();
            println!(
                "[[tokens]]\nname = {}\nvalue = \"{value}\"",
                toml_string(name)
            );
            ExitCode::SUCCESS
        }
        Ok(Command::Run { config }) => run(config),
        Err(error) => {
            eprintln!("porecatu-relay: {error}\n\n{}", cli::USAGE);
            ExitCode::from(2)
        }
    }
}

/// Loads the configuration, starts logging and serves until a signal arrives.
fn run(config_path: Option<PathBuf>) -> ExitCode {
    let path = config_path.unwrap_or_else(config::default_path);
    // Every `PORECATU_RELAY_*` variable is a configuration key (and nothing
    // else of the relay reads the environment), so it is passed on whole.
    let config = match config::load(&path, std::env::vars()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("porecatu-relay: {error}");
            return ExitCode::from(2);
        }
    };
    let settings = config.settings();
    // The pair is read before anything starts: a certificate that cannot be
    // used is a configuration error (exit 2), not a relay that listens and
    // fails every handshake. The error names the two files, never their text.
    let tls = match settings.tls.clone() {
        Some(tls) => match tls::load_from_files(&tls.cert_file, &tls.key_file, tls.min_version) {
            Ok(server_config) => Some((server_config, tls)),
            Err(error) => {
                eprintln!("porecatu-relay: {}: {error}", path.display());
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    if let Err(error) = logging::init(
        log_level(settings.log.level),
        log_format(settings.log.format),
    ) {
        eprintln!("porecatu-relay: cannot start logging: {error}");
        return ExitCode::from(1);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("porecatu-relay: cannot start the runtime: {error}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(serve(settings, tls))
}

/// The TLS configuration loaded at startup and the `[tls]` settings it came
/// from (the latter drive the reload).
type LoadedTls = (rustls::ServerConfig, config::TlsSettings);

async fn serve(settings: config::Settings, tls: Option<LoadedTls>) -> ExitCode {
    let grace = settings.server.shutdown_grace;
    let bind = settings.server.bind;
    let metrics = settings.metrics.clone();
    let tokens = TokenSet::from_digests(&settings.tokens);
    let state = Arc::new(ServerState::new(settings, tokens));
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("porecatu-relay: cannot listen on {bind} (server.bind): {error}");
            return ExitCode::from(1);
        }
    };
    let metrics_listener = if metrics.enabled {
        match tokio::net::TcpListener::bind(metrics.bind).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                eprintln!(
                    "porecatu-relay: cannot listen on {} (metrics.bind): {error}",
                    metrics.bind
                );
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };
    let shutdown = CancellationToken::new();
    tokio::spawn(shutdown::cancel_on_signal(shutdown.clone()));
    if let Some(listener) = metrics_listener {
        let (state, shutdown) = (Arc::clone(&state), shutdown.clone());
        tokio::spawn(async move {
            if let Err(error) = metrics::serve(listener, state, shutdown).await {
                tracing::error!(reason = "metrics-failed", "{error}");
            }
        });
        tracing::info!("metrics listening");
    }
    tracing::info!(tls = tls.is_some(), "relay listening");
    // The two ways of serving end the same way: `shutdown` closes the
    // connections, and `shutdown::run` bounds the wait with the grace period.
    let serving: Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>> = match tls {
        Some((server_config, tls_settings)) => Box::pin(server::serve_tls(
            listener,
            state,
            shutdown.clone(),
            server_config,
            tls_settings,
        )),
        None => Box::pin(server::serve(listener, state, shutdown.clone())),
    };
    match shutdown::run(serving, &shutdown, grace).await {
        Ok(Outcome::Clean) => {
            tracing::info!(reason = "shutdown", "relay stopped");
            ExitCode::SUCCESS
        }
        Ok(Outcome::GraceElapsed) => {
            tracing::warn!(
                reason = "grace-elapsed",
                "relay stopped with connections open"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(reason = "server-failed", "{error}");
            ExitCode::from(1)
        }
    }
}

fn log_level(level: LogLevel) -> logging::LogLevel {
    match level {
        LogLevel::Error => logging::LogLevel::Error,
        LogLevel::Warn => logging::LogLevel::Warn,
        LogLevel::Info => logging::LogLevel::Info,
        LogLevel::Debug => logging::LogLevel::Debug,
        LogLevel::Trace => logging::LogLevel::Trace,
    }
}

fn log_format(format: LogFormat) -> logging::LogFormat {
    match format {
        LogFormat::Text => logging::LogFormat::Text,
        LogFormat::Json => logging::LogFormat::Json,
    }
}

/// Renders `value` as a TOML basic string, escaping what must be escaped.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
