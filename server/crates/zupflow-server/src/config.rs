//! Configuration from `ZUPFLOW_*` environment variables. A value that's set
//! but unusable (invalid, empty, not UTF-8) stops startup; nothing falls back
//! silently.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// `ZUPFLOW_LISTEN`: where to listen. Defaults to loopback only.
    pub listen: SocketAddr,
    /// `ZUPFLOW_LOG`: a tracing filter, e.g. `info` or `zupflow_server=debug`.
    pub log: String,
    /// `ZUPFLOW_DATABASE`: the SQLite file. Defaults to `zupflow.db` in the
    /// working directory; created if missing.
    pub database: PathBuf,
    /// `ZUPFLOW_IDENTITY`: how the Console signs in, `local` (the default) or `oidc`.
    pub identity: IdentityMode,
    /// `ZUPFLOW_ALLOWED_ORIGINS`: comma-separated origins a browser may call
    /// the API from, like `http://localhost:5173`. Empty by default.
    pub allowed_origins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityMode {
    /// No sign-in: every Console request is the built-in owner. Loopback only.
    Local,
    /// Any OpenID Connect provider: `ZUPFLOW_OIDC_ISSUER` and
    /// `ZUPFLOW_OIDC_AUDIENCE` (the Console's client ID), and optionally
    /// `ZUPFLOW_OIDC_JWKS_URL` instead of discovery.
    Oidc {
        issuer: String,
        audience: String,
        jwks_url: Option<String>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name} must be an address like 127.0.0.1:8080")]
    InvalidAddress { name: &'static str },
    #[error("{name} is set but empty; unset it to use the default")]
    Empty { name: &'static str },
    #[error("{name} isn't valid UTF-8")]
    NotUnicode { name: &'static str },
    #[error("ZUPFLOW_ALLOWED_ORIGINS must be comma-separated origins like http://localhost:5173, with no path")]
    InvalidOrigins,
    #[error("ZUPFLOW_IDENTITY must be `local` or `oidc`")]
    UnknownIdentity,
    #[error("ZUPFLOW_IDENTITY=oidc needs {name}")]
    Missing { name: &'static str },
    /// `local` lets anyone who reaches the server in as the owner.
    #[error(
        "ZUPFLOW_IDENTITY=local signs every Console request in as the owner, so it only runs on a loopback \
         address (127.0.0.1 or ::1); set ZUPFLOW_LISTEN to one"
    )]
    LocalNeedsLoopback,
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
        let database = set("ZUPFLOW_DATABASE")?.map_or_else(|| PathBuf::from("zupflow.db"), PathBuf::from);
        let required = |name: &'static str| set(name)?.ok_or(ConfigError::Missing { name });
        let identity = match set("ZUPFLOW_IDENTITY")?.as_deref() {
            None | Some("local") => IdentityMode::Local,
            Some("oidc") => IdentityMode::Oidc {
                issuer: required("ZUPFLOW_OIDC_ISSUER")?,
                audience: required("ZUPFLOW_OIDC_AUDIENCE")?,
                jwks_url: set("ZUPFLOW_OIDC_JWKS_URL")?,
            },
            Some(_) => return Err(ConfigError::UnknownIdentity),
        };
        if identity == IdentityMode::Local && !listen.ip().is_loopback() {
            return Err(ConfigError::LocalNeedsLoopback);
        }
        let allowed_origins = match set("ZUPFLOW_ALLOWED_ORIGINS")? {
            None => Vec::new(),
            Some(value) => value
                .split(',')
                .map(str::trim)
                .map(|origin| {
                    valid_origin(origin)
                        .then(|| origin.to_owned())
                        .ok_or(ConfigError::InvalidOrigins)
                })
                .collect::<Result<_, _>>()?,
        };
        Ok(Self {
            listen,
            log,
            database,
            identity,
            allowed_origins,
        })
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_vars(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(ConfigError::NotUnicode { name }),
        })
    }
}

/// `http(s)://host[:port]`: a scheme, a host, and nothing after it.
fn valid_origin(origin: &str) -> bool {
    let rest = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"));
    rest.is_some_and(|rest| {
        !rest.is_empty()
            && !rest.contains('/')
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listens_on_loopback_unless_told_otherwise() {
        let config = Config::from_vars(|_| Ok(None)).expect("defaults are valid");
        assert_eq!(config.listen, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(config.log, "info");
        assert_eq!(config.database, PathBuf::from("zupflow.db"));
        assert_eq!(config.identity, IdentityMode::Local);
    }

    #[test]
    fn refuses_local_sign_in_beyond_loopback() {
        for address in ["0.0.0.0:8080", "[::]:8080", "192.168.1.5:8080"] {
            let config = Config::from_vars(|name| Ok((name == "ZUPFLOW_LISTEN").then(|| address.to_owned())));
            assert!(matches!(config, Err(ConfigError::LocalNeedsLoopback)), "{address}");
        }
        for address in ["127.0.0.1:9000", "[::1]:9000"] {
            let config = Config::from_vars(|name| Ok((name == "ZUPFLOW_LISTEN").then(|| address.to_owned())));
            assert!(config.is_ok(), "{address}");
        }
        let origins = |value: &'static str| {
            Config::from_vars(move |name| Ok((name == "ZUPFLOW_ALLOWED_ORIGINS").then(|| value.to_owned())))
        };
        assert_eq!(
            origins("http://localhost:5173, https://console.example")
                .expect("valid origins")
                .allowed_origins,
            ["http://localhost:5173", "https://console.example"]
        );
        for bad in ["localhost:5173", "http://x/path", "*", "http://"] {
            assert!(matches!(origins(bad), Err(ConfigError::InvalidOrigins)), "{bad}");
        }
        let oidc = |vars: &'static [(&'static str, &'static str)]| {
            Config::from_vars(move |name| {
                Ok(vars
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned()))
            })
        };
        let configured = oidc(&[
            ("ZUPFLOW_IDENTITY", "oidc"),
            ("ZUPFLOW_LISTEN", "0.0.0.0:8080"),
            ("ZUPFLOW_OIDC_ISSUER", "https://idp.example"),
            ("ZUPFLOW_OIDC_AUDIENCE", "console"),
        ])
        .expect("oidc may listen beyond loopback");
        assert_eq!(
            configured.identity,
            IdentityMode::Oidc {
                issuer: "https://idp.example".to_owned(),
                audience: "console".to_owned(),
                jwks_url: None,
            }
        );
        assert!(matches!(
            oidc(&[
                ("ZUPFLOW_IDENTITY", "oidc"),
                ("ZUPFLOW_OIDC_ISSUER", "https://idp.example")
            ]),
            Err(ConfigError::Missing {
                name: "ZUPFLOW_OIDC_AUDIENCE"
            })
        ));
        let unknown = Config::from_vars(|name| Ok((name == "ZUPFLOW_IDENTITY").then(|| "none".to_owned())));
        assert!(matches!(unknown, Err(ConfigError::UnknownIdentity)));
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
