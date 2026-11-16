// SPDX-License-Identifier: GPL-3.0-or-later

//! Environment overrides: `PORECATU_RELAY_<SECTION>_<KEY>` replaces one scalar.
//!
//! The variable name is split at the first `_` after the prefix, so section
//! names must not contain one (they do not). Tokens are never read from the
//! environment: a secret in a process environment leaks to children, `/proc`
//! and crash reports. `trusted_proxies` takes a comma-separated list.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use serde::de::IntoDeserializer;
use serde::de::value::StrDeserializer;

use super::model::{Field, Model, Tls};
use super::{ConfigError, Location};

/// Prefix of every variable the relay reads.
const PREFIX: &str = "PORECATU_RELAY_";

/// Applies every `PORECATU_RELAY_*` variable of `env` to `model`.
///
/// Variables are applied in name order, so the result does not depend on the
/// iteration order of the operating system's environment.
pub(crate) fn apply(
    model: &mut Model,
    env: impl Iterator<Item = (String, String)>,
) -> Result<(), ConfigError> {
    let mut vars: Vec<(String, String)> =
        env.filter(|(name, _)| name.starts_with(PREFIX)).collect();
    vars.sort();
    for (name, raw) in vars {
        apply_one(model, &name, &raw)?;
    }
    Ok(())
}

fn apply_one(model: &mut Model, variable: &str, raw: &str) -> Result<(), ConfigError> {
    let rest = &variable[PREFIX.len()..];
    let Some((section, key)) = rest.split_once('_') else {
        return Err(ConfigError::UnknownVariable {
            variable: variable.to_owned(),
        });
    };
    let section = section.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    let full_key = format!("{section}.{key}");
    let set = Setter {
        variable,
        raw,
        key: &full_key,
    };

    match (section.as_str(), key.as_str()) {
        ("server", "bind") => set.scalar::<SocketAddr>(
            &mut model.server.bind,
            "a socket address such as 0.0.0.0:443",
        ),
        ("server", "behind_proxy") => set.boolean(&mut model.server.behind_proxy),
        ("server", "trusted_proxies") => {
            model.server.trusted_proxies = Field::from_env(split_list(raw), variable);
            Ok(())
        }
        ("server", "shutdown_grace_seconds") => {
            set.integer(&mut model.server.shutdown_grace_seconds)
        }

        ("tls", "cert_file") => {
            let tls = model.tls_for_env();
            tls.cert_file = Some(Field::from_env(PathBuf::from(raw), variable));
            Ok(())
        }
        ("tls", "key_file") => {
            let tls = model.tls_for_env();
            tls.key_file = Some(Field::from_env(PathBuf::from(raw), variable));
            Ok(())
        }
        ("tls", "min_version") => {
            let tls = model.tls_for_env();
            set.enumeration(&mut tls.min_version, "\"1.3\" or \"1.2\"")
        }
        ("tls", "reload_check_seconds") => {
            let tls = model.tls_for_env();
            set.integer(&mut tls.reload_check_seconds)
        }

        ("tokens", _) => Err(ConfigError::Invalid {
            location: Location::Environment {
                variable: variable.to_owned(),
            },
            key: "tokens".to_owned(),
            message: "tokens can only be set in the configuration file".to_owned(),
        }),

        ("limits", "max_hosts") => set.integer(&mut model.limits.max_hosts),
        ("limits", "max_channels_per_host") => set.integer(&mut model.limits.max_channels_per_host),
        ("limits", "max_frame_bytes") => set.integer(&mut model.limits.max_frame_bytes),
        ("limits", "channel_bandwidth_kbps") => {
            set.integer(&mut model.limits.channel_bandwidth_kbps)
        }
        ("limits", "channel_burst_kib") => set.integer(&mut model.limits.channel_burst_kib),
        ("limits", "channel_queue_frames") => set.integer(&mut model.limits.channel_queue_frames),
        ("limits", "host_queue_frames") => set.integer(&mut model.limits.host_queue_frames),
        ("limits", "register_timeout_seconds") => {
            set.integer(&mut model.limits.register_timeout_seconds)
        }
        ("limits", "client_connects_per_ip_per_minute") => {
            set.integer(&mut model.limits.client_connects_per_ip_per_minute)
        }
        ("limits", "host_attempts_per_ip_per_minute") => {
            set.integer(&mut model.limits.host_attempts_per_ip_per_minute)
        }
        ("limits", "presence_per_minute_per_ip") => {
            set.integer(&mut model.limits.presence_per_minute_per_ip)
        }

        ("log", "level") => set.enumeration(
            &mut model.log.level,
            "one of error, warn, info, debug, trace",
        ),
        ("log", "format") => set.enumeration(&mut model.log.format, "\"text\" or \"json\""),
        ("log", "ip_addresses") => {
            set.enumeration(&mut model.log.ip_addresses, "one of full, truncated, none")
        }

        ("metrics", "enabled") => set.boolean(&mut model.metrics.enabled),
        ("metrics", "bind") => set.scalar::<SocketAddr>(
            &mut model.metrics.bind,
            "a socket address such as 127.0.0.1:9464",
        ),

        _ => Err(ConfigError::UnknownVariable {
            variable: variable.to_owned(),
        }),
    }
}

impl Model {
    /// The `[tls]` section, created empty when only the environment mentions it.
    fn tls_for_env(&mut self) -> &mut Tls {
        let field = self
            .tls
            .get_or_insert_with(|| Field::default_value(Tls::default()));
        &mut field.value
    }
}

/// Splits `a, b ,c` into `["a", "b", "c"]`; an empty or blank string is an
/// empty list.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Parses one raw value into a model field, with errors that name the variable.
struct Setter<'a> {
    variable: &'a str,
    raw: &'a str,
    key: &'a str,
}

impl Setter<'_> {
    fn error(&self, expected: &str) -> ConfigError {
        ConfigError::Invalid {
            location: Location::Environment {
                variable: self.variable.to_owned(),
            },
            key: self.key.to_owned(),
            message: format!("expected {expected}"),
        }
    }

    fn scalar<T: FromStr>(&self, slot: &mut Field<T>, expected: &str) -> Result<(), ConfigError> {
        let value = self
            .raw
            .trim()
            .parse::<T>()
            .map_err(|_| self.error(expected))?;
        *slot = Field::from_env(value, self.variable);
        Ok(())
    }

    fn integer(&self, slot: &mut Field<u64>) -> Result<(), ConfigError> {
        self.scalar(slot, "a non-negative integer")
    }

    fn boolean(&self, slot: &mut Field<bool>) -> Result<(), ConfigError> {
        let value = match self.raw.trim().to_ascii_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => return Err(self.error("\"true\" or \"false\"")),
        };
        *slot = Field::from_env(value, self.variable);
        Ok(())
    }

    /// Parses through the same `serde` names the TOML file uses.
    fn enumeration<T>(&self, slot: &mut Field<T>, expected: &str) -> Result<(), ConfigError>
    where
        T: for<'de> serde::Deserialize<'de>,
    {
        let deserializer: StrDeserializer<'_, serde::de::value::Error> =
            self.raw.trim().into_deserializer();
        let value = T::deserialize(deserializer).map_err(|_| self.error(expected))?;
        *slot = Field::from_env(value, self.variable);
        Ok(())
    }
}
