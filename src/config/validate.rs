// SPDX-License-Identifier: GPL-3.0-or-later

//! Validation: turns the raw [`Model`] into [`Settings`], or says why not.
//!
//! The floors come from section 6 (and the frame size of section 1) of the
//! protocol specification; the TLS rule is ADR-0004. The first problem found
//! is reported, with the file and line (or the environment variable) and the
//! key. No message ever contains a token value.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ipnet::IpNet;

use super::model::{Field, Model, Origin, Tls, Token};
use super::{
    ConfigError, LimitSettings, Location, LogSettings, MetricsSettings, ServerSettings, Settings,
    TlsSettings, TokenDigest, line_of,
};

/// Floor of `[limits] max_channels_per_host` (specification section 6).
const MIN_CHANNELS_PER_HOST: u64 = 8;
/// The one accepted `[limits] max_frame_bytes` (specification section 1).
const FRAME_BYTES: u64 = 70_000;
/// Floor of `[limits] channel_bandwidth_kbps`: 2 Mbit/s (specification section 6).
const MIN_CHANNEL_BANDWIDTH_KBPS: u64 = 2000;
/// Floor of `[limits] register_timeout_seconds`.
const MIN_REGISTER_TIMEOUT_SECONDS: u64 = 10;
/// Shortest and longest operator token (specification section 2).
const TOKEN_LENGTH: std::ops::RangeInclusive<usize> = 32..=128;

/// What validation needs to know about where the model came from.
pub(crate) struct Context<'a> {
    /// The configuration file.
    pub(crate) path: &'a Path,
    /// Its text, to turn byte offsets into lines.
    pub(crate) text: &'a str,
}

impl Context<'_> {
    fn location(&self, origin: &Origin) -> Location {
        match origin {
            Origin::Default => Location::File {
                path: self.path.to_owned(),
                line: None,
            },
            Origin::File(span) => Location::File {
                path: self.path.to_owned(),
                line: Some(line_of(self.text, span.start)),
            },
            Origin::Env(variable) => Location::Environment {
                variable: variable.clone(),
            },
        }
    }

    fn invalid(&self, origin: &Origin, key: &str, message: impl Into<String>) -> ConfigError {
        ConfigError::Invalid {
            location: self.location(origin),
            key: key.to_owned(),
            message: message.into(),
        }
    }

    /// An error not tied to a line (a missing section).
    fn invalid_unplaced(&self, key: &str, message: impl Into<String>) -> ConfigError {
        self.invalid(&Origin::Default, key, message)
    }
}

/// `kbit/s` (decimal, as in "2 Mbit/s") to bytes per second.
pub(crate) fn kbps_to_bytes_per_second(kbps: u64) -> u64 {
    kbps.saturating_mul(1000) / 8
}

/// Validates `model` and converts it to the runtime's [`Settings`].
pub(crate) fn validate(model: Model, ctx: &Context<'_>) -> Result<Settings, ConfigError> {
    let server = server(&model, ctx)?;
    let tls = tls(model.tls.as_ref(), ctx)?;
    if tls.is_none() && !server.bind.ip().to_canonical().is_loopback() && !server.behind_proxy {
        return Err(ctx.invalid(
            &model.server.bind.origin,
            "server.bind",
            format!(
                "{} is not a loopback address and no [tls] section is configured; add \
                 [tls] cert_file and key_file, or set [server] behind_proxy = true if a \
                 reverse proxy terminates TLS in front of the relay",
                server.bind
            ),
        ));
    }
    let tokens = tokens(&model.tokens, ctx)?;
    let limits = limits(&model, ctx)?;
    Ok(Settings {
        server,
        tls,
        tokens,
        limits,
        log: LogSettings {
            level: model.log.level.value,
            format: model.log.format.value,
            ip_addresses: model.log.ip_addresses.value,
        },
        metrics: MetricsSettings {
            enabled: model.metrics.enabled.value,
            bind: model.metrics.bind.value,
        },
    })
}

fn server(model: &Model, ctx: &Context<'_>) -> Result<ServerSettings, ConfigError> {
    let section = &model.server;
    let mut trusted_proxies = Vec::new();
    for entry in &section.trusted_proxies.value {
        let network = entry
            .parse::<IpNet>()
            .or_else(|_| entry.parse::<IpAddr>().map(IpNet::from))
            .map_err(|_| {
                ctx.invalid(
                    &section.trusted_proxies.origin,
                    "server.trusted_proxies",
                    format!("\"{entry}\" is neither an IP address nor a CIDR network"),
                )
            })?;
        trusted_proxies.push(network);
    }
    Ok(ServerSettings {
        bind: section.bind.value,
        behind_proxy: section.behind_proxy.value,
        trusted_proxies,
        shutdown_grace: Duration::from_secs(section.shutdown_grace_seconds.value),
    })
}

fn tls(
    section: Option<&Field<Tls>>,
    ctx: &Context<'_>,
) -> Result<Option<TlsSettings>, ConfigError> {
    let Some(section) = section else {
        return Ok(None);
    };
    let tls = &section.value;
    let cert_file = tls.cert_file.as_ref().ok_or_else(|| {
        ctx.invalid(
            &section.origin,
            "tls.cert_file",
            "the [tls] section needs cert_file (and key_file); remove the section to run without TLS",
        )
    })?;
    let key_file = tls.key_file.as_ref().ok_or_else(|| {
        ctx.invalid(
            &section.origin,
            "tls.key_file",
            "the [tls] section needs key_file (and cert_file); remove the section to run without TLS",
        )
    })?;
    for (field, key) in [(cert_file, "tls.cert_file"), (key_file, "tls.key_file")] {
        if field.value.as_os_str().is_empty() {
            return Err(ctx.invalid(&field.origin, key, "must not be empty"));
        }
    }
    let reload = at_least(
        ctx,
        &tls.reload_check_seconds,
        "tls.reload_check_seconds",
        1,
    )?;
    Ok(Some(TlsSettings {
        cert_file: cert_file.value.clone(),
        key_file: key_file.value.clone(),
        min_version: tls.min_version.value,
        reload_check: Duration::from_secs(reload),
    }))
}

fn limits(model: &Model, ctx: &Context<'_>) -> Result<LimitSettings, ConfigError> {
    let limits = &model.limits;
    let max_hosts = at_least(ctx, &limits.max_hosts, "limits.max_hosts", 1)?;
    let max_channels = at_least(
        ctx,
        &limits.max_channels_per_host,
        "limits.max_channels_per_host",
        MIN_CHANNELS_PER_HOST,
    )?;
    if limits.max_frame_bytes.value != FRAME_BYTES {
        return Err(ctx.invalid(
            &limits.max_frame_bytes.origin,
            "limits.max_frame_bytes",
            format!(
                "must be exactly {FRAME_BYTES} (it is both floor and ceiling in the protocol \
                 specification), got {}",
                limits.max_frame_bytes.value
            ),
        ));
    }
    let bandwidth = at_least(
        ctx,
        &limits.channel_bandwidth_kbps,
        "limits.channel_bandwidth_kbps",
        MIN_CHANNEL_BANDWIDTH_KBPS,
    )?;
    let burst = at_least(
        ctx,
        &limits.channel_burst_kib,
        "limits.channel_burst_kib",
        1,
    )?;
    let burst_bytes = burst
        .checked_mul(1024)
        .filter(|bytes| *bytes >= FRAME_BYTES)
        .ok_or_else(|| {
            ctx.invalid(
                &limits.channel_burst_kib.origin,
                "limits.channel_burst_kib",
                format!(
                    "the burst must hold at least one frame ({FRAME_BYTES} bytes, that is {} KiB), \
                     got {burst} KiB",
                    FRAME_BYTES.div_ceil(1024)
                ),
            )
        })?;
    let channel_queue = at_least(
        ctx,
        &limits.channel_queue_frames,
        "limits.channel_queue_frames",
        1,
    )?;
    let host_queue = at_least(
        ctx,
        &limits.host_queue_frames,
        "limits.host_queue_frames",
        1,
    )?;
    let register_timeout = at_least(
        ctx,
        &limits.register_timeout_seconds,
        "limits.register_timeout_seconds",
        MIN_REGISTER_TIMEOUT_SECONDS,
    )?;
    let client_connects = at_least(
        ctx,
        &limits.client_connects_per_ip_per_minute,
        "limits.client_connects_per_ip_per_minute",
        1,
    )?;
    let host_attempts = at_least(
        ctx,
        &limits.host_attempts_per_ip_per_minute,
        "limits.host_attempts_per_ip_per_minute",
        1,
    )?;
    let presence = at_least(
        ctx,
        &limits.presence_per_minute_per_ip,
        "limits.presence_per_minute_per_ip",
        1,
    )?;
    Ok(LimitSettings {
        max_hosts: size(ctx, &limits.max_hosts, "limits.max_hosts", max_hosts)?,
        max_channels_per_host: size(
            ctx,
            &limits.max_channels_per_host,
            "limits.max_channels_per_host",
            max_channels,
        )?,
        max_frame_bytes: size(
            ctx,
            &limits.max_frame_bytes,
            "limits.max_frame_bytes",
            FRAME_BYTES,
        )?,
        channel_bytes_per_second: kbps_to_bytes_per_second(bandwidth),
        channel_burst_bytes: burst_bytes,
        channel_queue_frames: size(
            ctx,
            &limits.channel_queue_frames,
            "limits.channel_queue_frames",
            channel_queue,
        )?,
        host_queue_frames: size(
            ctx,
            &limits.host_queue_frames,
            "limits.host_queue_frames",
            host_queue,
        )?,
        register_timeout: Duration::from_secs(register_timeout),
        client_connects_per_ip_per_minute: client_connects,
        host_attempts_per_ip_per_minute: host_attempts,
        presence_per_minute_per_ip: presence,
    })
}

/// `field` must be at least `floor`.
fn at_least(
    ctx: &Context<'_>,
    field: &Field<u64>,
    key: &str,
    floor: u64,
) -> Result<u64, ConfigError> {
    if field.value < floor {
        return Err(ctx.invalid(
            &field.origin,
            key,
            format!("must be at least {floor}, got {}", field.value),
        ));
    }
    Ok(field.value)
}

/// A count that must fit in `usize`.
fn size(
    ctx: &Context<'_>,
    field: &Field<u64>,
    key: &str,
    value: u64,
) -> Result<usize, ConfigError> {
    usize::try_from(value).map_err(|_| {
        ctx.invalid(
            &field.origin,
            key,
            format!("{value} is too large for this platform"),
        )
    })
}

fn tokens(entries: &[Token], ctx: &Context<'_>) -> Result<Vec<TokenDigest>, ConfigError> {
    if entries.is_empty() {
        return Err(ctx.invalid_unplaced(
            "tokens",
            "at least one [[tokens]] entry is required: without a token nobody can register \
             a host (generate one with `porecatu-relay token new`)",
        ));
    }
    let mut seen = HashSet::new();
    let mut digests = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let key = |field: &str| format!("tokens[{index}].{field}");
        let anchor = entry
            .name
            .as_ref()
            .map(|field| &field.origin)
            .or_else(|| entry.value.as_ref().map(|field| &field.origin))
            .or_else(|| entry.value_file.as_ref().map(|field| &field.origin))
            .unwrap_or(&Origin::Default);

        let name = entry
            .name
            .as_ref()
            .ok_or_else(|| ctx.invalid(anchor, &key("name"), "every token needs a name"))?;
        if name.value.is_empty() || name.value.chars().any(char::is_control) {
            return Err(ctx.invalid(
                &name.origin,
                &key("name"),
                "must not be empty or contain control characters",
            ));
        }
        if !seen.insert(name.value.as_str()) {
            return Err(ctx.invalid(
                &name.origin,
                &key("name"),
                format!("the token name \"{}\" is used more than once", name.value),
            ));
        }

        let (origin, value) = match (&entry.value, &entry.value_file) {
            (Some(_), Some(file)) => {
                return Err(ctx.invalid(
                    &file.origin,
                    &key("value_file"),
                    "use `value` or `value_file`, not both",
                ));
            }
            (None, None) => {
                return Err(ctx.invalid(
                    &name.origin,
                    &key("value"),
                    "the token needs `value` or `value_file`",
                ));
            }
            (Some(value), None) => (&value.origin, value.value.clone()),
            (None, Some(file)) => (
                &file.origin,
                read_value_file(ctx, &file.value, &file.origin, &key("value_file"))?,
            ),
        };
        let value_key = if entry.value_file.is_some() {
            key("value_file")
        } else {
            key("value")
        };
        check_token_value(ctx, origin, &value_key, &value)?;
        digests.push(TokenDigest::new(name.value.clone(), &value));
    }
    Ok(digests)
}

/// Reads a `value_file`, relative paths resolved against the configuration
/// file's directory, dropping one trailing line break.
fn read_value_file(
    ctx: &Context<'_>,
    file: &Path,
    origin: &Origin,
    key: &str,
) -> Result<String, ConfigError> {
    let resolved: PathBuf = if file.is_absolute() {
        file.to_owned()
    } else {
        ctx.path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(file)
    };
    let bytes = std::fs::read(&resolved).map_err(|error| {
        ctx.invalid(
            origin,
            key,
            format!("cannot read {}: {error}", resolved.display()),
        )
    })?;
    let mut text = String::from_utf8(bytes)
        .map_err(|_| ctx.invalid(origin, key, "the file is not valid UTF-8 text"))?;
    if text.ends_with('\n') {
        text.pop();
        if text.ends_with('\r') {
            text.pop();
        }
    }
    Ok(text)
}

/// 32 to 128 printable ASCII characters, none of them at the edges a header
/// would trim.
fn check_token_value(
    ctx: &Context<'_>,
    origin: &Origin,
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let length = value.chars().count();
    if !TOKEN_LENGTH.contains(&length) {
        return Err(ctx.invalid(
            origin,
            key,
            format!(
                "a token must have {} to {} characters, this one has {length}",
                TOKEN_LENGTH.start(),
                TOKEN_LENGTH.end()
            ),
        ));
    }
    if !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(ctx.invalid(
            origin,
            key,
            "a token must be printable ASCII (no control or non-ASCII characters)",
        ));
    }
    if value.starts_with(' ') || value.ends_with(' ') {
        return Err(ctx.invalid(
            origin,
            key,
            "a token must not start or end with a space (an HTTP header would trim it)",
        ));
    }
    Ok(())
}
