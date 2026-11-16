// SPDX-License-Identifier: GPL-3.0-or-later

//! Hand-written command-line parsing (no CLI crate).
//!
//! Forms:
//!
//! ```text
//! porecatu-relay [--config <file>]
//! porecatu-relay --version
//! porecatu-relay token new [--name <name>]
//! porecatu-relay --help
//! ```

use std::fmt;
use std::path::PathBuf;

/// Name given to a new token when `--name` is omitted (there is no clock here
/// to build a date-based name).
pub const DEFAULT_TOKEN_NAME: &str = "operador";

/// What the command line asks the binary to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run the relay, optionally with an explicit configuration file.
    Run {
        /// Path given by `--config`, if any.
        config: Option<PathBuf>,
    },
    /// Print the version and exit.
    Version,
    /// Print a new operator token and its TOML line.
    TokenNew {
        /// Name given by `--name`, if any (the caller falls back to
        /// [`DEFAULT_TOKEN_NAME`]).
        name: Option<String>,
    },
    /// Print the usage text and exit.
    Help,
}

/// A command line that does not match any form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// An option the relay does not know.
    UnknownArgument(String),
    /// An option that needs a value was last on the line.
    MissingValue(&'static str),
    /// An option appeared twice.
    Duplicate(&'static str),
    /// `token` was not followed by `new`.
    UnknownTokenCommand(Option<String>),
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArgument(arg) => write!(f, "unknown argument `{arg}`"),
            Self::MissingValue(option) => write!(f, "option `{option}` needs a value"),
            Self::Duplicate(option) => write!(f, "option `{option}` given more than once"),
            Self::UnknownTokenCommand(Some(sub)) => {
                write!(f, "unknown token command `{sub}` (expected `new`)")
            }
            Self::UnknownTokenCommand(None) => {
                write!(f, "missing token command (expected `new`)")
            }
        }
    }
}

impl std::error::Error for CliError {}

/// Usage text printed by `--help` and after a usage error.
pub const USAGE: &str = "\
usage:
  porecatu-relay [--config <file>]       run the relay
  porecatu-relay --version               print the relay and envelope versions
  porecatu-relay token new [--name <n>]  print a new operator token and its
                                         [[tokens]] TOML line (default name:
                                         operador)
  porecatu-relay --help                  print this text";

/// Parses the arguments that follow the program name.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Command, CliError> {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Ok(Command::Run { config: None });
    };
    match first.as_str() {
        "--help" | "-h" => {
            reject_rest(args)?;
            Ok(Command::Help)
        }
        "--version" | "-V" => {
            reject_rest(args)?;
            Ok(Command::Version)
        }
        "token" => parse_token(args),
        "--config" => {
            let config = PathBuf::from(args.next().ok_or(CliError::MissingValue("--config"))?);
            reject_rest(args)?;
            Ok(Command::Run {
                config: Some(config),
            })
        }
        other => match other.strip_prefix("--config=") {
            Some(value) if !value.is_empty() => {
                let config = PathBuf::from(value);
                reject_rest(args)?;
                Ok(Command::Run {
                    config: Some(config),
                })
            }
            Some(_) => Err(CliError::MissingValue("--config")),
            None => Err(CliError::UnknownArgument(first)),
        },
    }
}

fn parse_token(mut args: impl Iterator<Item = String>) -> Result<Command, CliError> {
    match args.next().as_deref() {
        Some("new") => {}
        Some(other) => return Err(CliError::UnknownTokenCommand(Some(other.to_owned()))),
        None => return Err(CliError::UnknownTokenCommand(None)),
    }
    let mut name = None;
    while let Some(arg) = args.next() {
        let value = if arg == "--name" {
            args.next().ok_or(CliError::MissingValue("--name"))?
        } else if let Some(value) = arg.strip_prefix("--name=") {
            if value.is_empty() {
                return Err(CliError::MissingValue("--name"));
            }
            value.to_owned()
        } else {
            return Err(CliError::UnknownArgument(arg));
        };
        if name.replace(value).is_some() {
            return Err(CliError::Duplicate("--name"));
        }
    }
    Ok(Command::TokenNew { name })
}

fn reject_rest(mut args: impl Iterator<Item = String>) -> Result<(), CliError> {
    match args.next() {
        Some(extra) => Err(CliError::UnknownArgument(extra)),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command, CliError> {
        parse(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn no_arguments_runs_with_the_default_config() {
        assert_eq!(parse_strs(&[]), Ok(Command::Run { config: None }));
    }

    #[test]
    fn config_option_in_both_spellings() {
        let expected = Ok(Command::Run {
            config: Some(PathBuf::from("relay.toml")),
        });
        assert_eq!(parse_strs(&["--config", "relay.toml"]), expected);
        assert_eq!(parse_strs(&["--config=relay.toml"]), expected);
    }

    #[test]
    fn config_without_value_is_an_error() {
        assert_eq!(
            parse_strs(&["--config"]),
            Err(CliError::MissingValue("--config"))
        );
        assert_eq!(
            parse_strs(&["--config="]),
            Err(CliError::MissingValue("--config"))
        );
    }

    #[test]
    fn version_and_help() {
        assert_eq!(parse_strs(&["--version"]), Ok(Command::Version));
        assert_eq!(parse_strs(&["--help"]), Ok(Command::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Command::Help));
    }

    #[test]
    fn token_new_with_and_without_name() {
        assert_eq!(
            parse_strs(&["token", "new"]),
            Ok(Command::TokenNew { name: None })
        );
        assert_eq!(
            parse_strs(&["token", "new", "--name", "casa"]),
            Ok(Command::TokenNew {
                name: Some("casa".to_owned())
            })
        );
        assert_eq!(
            parse_strs(&["token", "new", "--name=casa"]),
            Ok(Command::TokenNew {
                name: Some("casa".to_owned())
            })
        );
    }

    #[test]
    fn invalid_token_forms() {
        assert_eq!(
            parse_strs(&["token"]),
            Err(CliError::UnknownTokenCommand(None))
        );
        assert_eq!(
            parse_strs(&["token", "list"]),
            Err(CliError::UnknownTokenCommand(Some("list".to_owned())))
        );
        assert_eq!(
            parse_strs(&["token", "new", "--name"]),
            Err(CliError::MissingValue("--name"))
        );
        assert_eq!(
            parse_strs(&["token", "new", "--name", "a", "--name", "b"]),
            Err(CliError::Duplicate("--name"))
        );
        assert_eq!(
            parse_strs(&["token", "new", "--bogus"]),
            Err(CliError::UnknownArgument("--bogus".to_owned()))
        );
    }

    #[test]
    fn unknown_and_extra_arguments_are_errors() {
        assert_eq!(
            parse_strs(&["serve"]),
            Err(CliError::UnknownArgument("serve".to_owned()))
        );
        assert_eq!(
            parse_strs(&["--version", "--help"]),
            Err(CliError::UnknownArgument("--help".to_owned()))
        );
        assert_eq!(
            parse_strs(&["--config", "a.toml", "extra"]),
            Err(CliError::UnknownArgument("extra".to_owned()))
        );
    }

    #[test]
    fn errors_have_readable_messages() {
        assert!(
            CliError::MissingValue("--config")
                .to_string()
                .contains("--config")
        );
        assert!(
            CliError::UnknownArgument("zzz".to_owned())
                .to_string()
                .contains("zzz")
        );
    }
}
