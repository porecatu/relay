// SPDX-License-Identifier: GPL-3.0-or-later

//! Graceful shutdown (PRD-001 RF-1.22).
//!
//! A signal (`SIGINT`/`SIGTERM`; on Windows `Ctrl+C`, console close and system
//! shutdown) cancels a [`CancellationToken`]. [`crate::server::serve`] turns
//! that into the closing of every connection (`1001` for hosts, `closed
//! { normal }` and `1001` for clients) and stops accepting new ones; [`run`]
//! then waits for it to finish, but only up to `shutdown_grace_seconds`: past
//! that the process leaves with connections still open, because a peer that
//! does not answer the close handshake must not keep the relay alive.

use std::future::Future;
use std::io;
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// How the server ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The server finished by itself or within the grace period.
    Clean,
    /// Shutdown began and the connections were not all gone when the grace
    /// period ended.
    GraceElapsed,
}

/// Runs `serve` until it ends or, once `shutdown` is cancelled, until `grace`
/// has passed.
///
/// `serve` is expected to react to the same `shutdown` token (as
/// [`crate::server::serve`] does); this function only bounds the wait.
///
/// # Errors
///
/// The error `serve` ended with.
pub async fn run<F>(serve: F, shutdown: &CancellationToken, grace: Duration) -> io::Result<Outcome>
where
    F: Future<Output = io::Result<()>>,
{
    tokio::pin!(serve);
    tokio::select! {
        result = &mut serve => return result.map(|()| Outcome::Clean),
        () = shutdown.cancelled() => {}
    }
    let deadline = Instant::now() + grace;
    tokio::select! {
        result = &mut serve => result.map(|()| Outcome::Clean),
        () = tokio::time::sleep_until(deadline) => Ok(Outcome::GraceElapsed),
    }
}

/// Waits for the first termination signal and returns its name.
///
/// # Errors
///
/// The operating system refused to install a signal handler.
pub async fn wait_for_signal() -> io::Result<&'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map(|()| "SIGINT"),
            _ = terminate.recv() => Ok("SIGTERM"),
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_close, ctrl_shutdown};
        let mut close = ctrl_close()?;
        let mut shutdown = ctrl_shutdown()?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.map(|()| "ctrl-c"),
            _ = close.recv() => Ok("ctrl-close"),
            _ = shutdown.recv() => Ok("ctrl-shutdown"),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        tokio::signal::ctrl_c().await.map(|()| "ctrl-c")
    }
}

/// Cancels `token` when the first termination signal arrives. Spawn it on the
/// runtime that serves; it ends with the signal.
pub async fn cancel_on_signal(token: CancellationToken) {
    match wait_for_signal().await {
        Ok(signal) => {
            tracing::info!(reason = signal, "shutting down");
            token.cancel();
        }
        Err(error) => {
            // Without a handler the relay still runs; it just only stops when
            // killed.
            tracing::error!(reason = "signal-handler-failed", "{error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_server_that_ends_by_itself_is_clean() {
        let shutdown = CancellationToken::new();
        let outcome = run(async { Ok(()) }, &shutdown, Duration::from_secs(5)).await;
        assert_eq!(outcome.unwrap(), Outcome::Clean);
    }

    #[tokio::test]
    async fn the_error_of_the_server_is_returned() {
        let shutdown = CancellationToken::new();
        let outcome = run(
            async { Err(io::Error::other("boom")) },
            &shutdown,
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(outcome.unwrap_err().to_string(), "boom");
    }

    #[tokio::test]
    async fn a_server_that_hangs_after_shutdown_is_left_at_the_grace_period() {
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            token.cancel();
        });
        let started = Instant::now();
        let outcome = run(
            std::future::pending::<io::Result<()>>(),
            &shutdown,
            Duration::from_millis(300),
        )
        .await;
        assert_eq!(outcome.unwrap(), Outcome::GraceElapsed);
        assert!(started.elapsed() >= Duration::from_millis(340));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn a_server_that_finishes_within_the_grace_period_is_clean() {
        let shutdown = CancellationToken::new();
        let token = shutdown.clone();
        let serve = async move {
            token.cancelled().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        };
        shutdown.cancel();
        let started = Instant::now();
        let outcome = run(serve, &shutdown, Duration::from_secs(10)).await;
        assert_eq!(outcome.unwrap(), Outcome::Clean);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
