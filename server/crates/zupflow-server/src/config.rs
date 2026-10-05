//! Configuration from `ZUPFLOW_*` environment variables. A value that's set
//! but unusable (invalid, empty, not UTF-8) stops startup; nothing falls back
//! silently.

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    /// `ZUPFLOW_LISTEN`: where to listen. Defaults to loopback only.
    pub listen: SocketAddr,
    /// `ZUPFLOW_LOG`: a tracing filter, e.g. `info` or `zupflow_server=debug`.
    pub log: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name} must be an address like 127.0.0.1:8080")]
    InvalidAddress { name: &'static str },
    #[error("{name} is set but empty; unset it to use the default")]
    Empty { name: &'static str },
    #[error("{name} isn't valid UTF-8")]
    NotUnicode { name: &'static str },
}

/// A variable's value: `None` when unset, an error when set to something unreadable.
pub type Lookup = Result<Option<String>, ConfigError>;

impl Config {
    /// Reads the configuration through `get`, so tests don't touch the process environment.
    pub fn from_vars(get: impl Fn(&'static str) -> Lookup) -> Result<Self, ConfigError> {
        let set = |name: &'static str| -> Lookup {
            match get(name)? {
                Some(value) if value.is_empty() => Err(ConfigError::Empty { name }),
                other => Ok(other),
            }
        };
        let listen = match set("ZUPFLOW_LISTEN")? {
            Some(value) => value
                .parse()
                .map_err(|_| ConfigError::InvalidAddress { name: "ZUPFLOW_LISTEN" })?,
            None => SocketAddr::from(([127, 0, 0, 1], 8080)),
        };
        let log = set("ZUPFLOW_LOG")?.unwrap_or_else(|| "info".to_owned());
        Ok(Self { listen, log })
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode { name }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listens_on_loopback_unless_told_otherwise() {
        let config = Config::from_vars(|_| Ok(None)).expect("defaults are valid");
        assert_eq!(config.listen, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(config.log, "info");
    }

    #[test]
    fn refuses_an_invalid_address_instead_of_falling_back() {
        let error = Config::from_vars(|name| Ok((name == "ZUPFLOW_LISTEN").then(|| "everywhere".to_owned())));
        assert!(matches!(
            error,
            Err(ConfigError::InvalidAddress { name: "ZUPFLOW_LISTEN" })
        ));
    }

    #[test]
    fn refuses_an_empty_or_unreadable_value_instead_of_treating_it_as_unset() {
        let empty = Config::from_vars(|name| Ok((name == "ZUPFLOW_LOG").then(String::new)));
        assert!(matches!(empty, Err(ConfigError::Empty { name: "ZUPFLOW_LOG" })));
        let unreadable = Config::from_vars(|name| {
            if name == "ZUPFLOW_LISTEN" {
                Err(ConfigError::NotUnicode { name })
            } else {
                Ok(None)
            }
        });
        assert!(matches!(
            unreadable,
            Err(ConfigError::NotUnicode { name: "ZUPFLOW_LISTEN" })
        ));
    }
}
