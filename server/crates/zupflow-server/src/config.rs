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
    /// `ZUPFLOW_IDENTITY`: how the Console signs in, `oidc` or `local`.
    /// Required: there's no default, so no deployment signs everyone in as
    /// the owner by leaving it out. `local` exists only in a build with the
    /// `local-identity` feature.
    pub identity: IdentityMode,
    /// `ZUPFLOW_SECRET_KEY`: 32 bytes, base64, the server's own key: API keys
    /// are hashed and idempotency fingerprints keyed under it, each use
    /// domain-separated. Required with `oidc`; with `local`, generated and kept
    /// in the database when unset. Changing it orphans every API key.
    pub secret_key: Option<[u8; 32]>,
    /// `ZUPFLOW_ALLOWED_ORIGINS`: comma-separated origins a browser may call
    /// the API from, like `http://localhost:5173`. Empty by default.
    pub allowed_origins: Vec<String>,
    /// `ZUPFLOW_CONSOLE_ORIGIN`: the origin the Console is served from, like
    /// `https://console.zupflow.example`, when it isn't this server's. The
    /// guard admits it and CORS answers it, and no other origin. Unset, no
    /// origin gets CORS: the Console must be served from the API's own origin.
    pub console_origin: Option<String>,
    /// `ZUPFLOW_PUBLIC_URL`: the origin providers and browsers reach this
    /// server at, like `https://zupflow.example`; OAuth redirect URIs are
    /// built on it (decision 0033). Defaults to `http://localhost:{port}`.
    pub public_url: String,
    /// `ZUPFLOW_MODEL_PROVIDER`: what checks organizations' provider keys,
    /// lists their models and answers runs. `providers`, a deployment's
    /// setting, reaches OpenAI, Anthropic and OpenRouter with each
    /// organization's own keys, through the safe outbound path. Unset,
    /// nothing does: connecting a provider answers `501`, and every run
    /// fails `model_unavailable`.
    /// `scripted` is the deterministic stand-in for local work and tests,
    /// never a deployment's: its providers list fixed catalogs and refuse a
    /// key containing `invalid`, and its model answers from a script, turning
    /// `call <tool> {…}` lines of a run's input into calls. Which models an
    /// organization can use is its own: connections and models are added
    /// through the API, never configured here.
    pub model_provider: ModelProviderMode,
    /// `ZUPFLOW_CHANNEL_PROVISIONER`: what makes agents' apps in linked
    /// teams. Unset, nothing does: linking a team answers `501`, and a new
    /// app fails `provisioner_unavailable`. `scripted` is the deterministic
    /// stand-in for local work and tests, never a deployment's: it calls no
    /// provider, refuses tokens containing `invalid`, and makes apps at once.
    /// `slack` makes them in Slack with each linked team's configuration
    /// token: it needs `ZUPFLOW_PUBLIC_URL` to be an `https://` origin Slack
    /// can reach, where it delivers events and sends people back from
    /// installing.
    pub channel_provisioner: ChannelProvisionerMode,
    /// `ZUPFLOW_SLACK_API_URL`: where Slack's Web API is reached, an
    /// `https://` base with no query (`https://slack.com/api` by default).
    /// For a proxy in front of Slack, or a stand-in in tests.
    pub slack_api: zupflow_slack::SlackApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelProvisionerMode {
    /// No provisioner: linking a team answers `501`.
    None,
    /// The scripted provisioner in `zupflow-basic`.
    Scripted,
    /// Slack, with each linked team's configuration token (`zupflow-slack`).
    Slack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelProviderMode {
    /// No provider: connecting one answers `501`, and runs fail `model_unavailable`.
    None,
    /// The provider adapters in `zupflow-providers`: the real providers.
    Providers,
    /// The scripted providers and model in `zupflow-basic`.
    Scripted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityMode {
    /// No sign-in: every Console request is the built-in owner. Loopback only.
    #[cfg(feature = "local-identity")]
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
    /// A variable an earlier version read, renamed: silently ignoring it would
    /// orphan every API key.
    #[error("{old} is now {new}; rename it")]
    Renamed { old: &'static str, new: &'static str },
    /// A variable an earlier version read and none reads now: silently
    /// ignoring it would leave its operator believing it still applies.
    #[error("{name} is no longer read: {instead}; unset it")]
    Retired { name: &'static str, instead: &'static str },
    #[error("ZUPFLOW_SECRET_KEY must be 32 bytes, base64-encoded: generate one with `openssl rand -base64 32`")]
    InvalidSecretKey,
    #[error(
        "ZUPFLOW_IDENTITY=oidc needs ZUPFLOW_SECRET_KEY, kept outside the database: \
         generate one with `openssl rand -base64 32`"
    )]
    SecretKeyRequired,
    #[error("ZUPFLOW_ALLOWED_ORIGINS must be comma-separated origins like http://localhost:5173, with no path")]
    InvalidOrigins,
    #[error("ZUPFLOW_PUBLIC_URL must be an origin like https://zupflow.example, with no path")]
    InvalidPublicUrl,
    #[error("ZUPFLOW_CONSOLE_ORIGIN must be one origin like https://console.zupflow.example, with no path")]
    InvalidConsoleOrigin,
    #[error("ZUPFLOW_MODEL_PROVIDER must be `providers`, `scripted` (local work only), or unset for none")]
    UnknownModelProvider,
    #[error("ZUPFLOW_CHANNEL_PROVISIONER must be `slack`, `scripted` (local work only), or unset for none")]
    UnknownChannelProvisioner,
    #[error(
        "ZUPFLOW_CHANNEL_PROVISIONER=slack needs ZUPFLOW_PUBLIC_URL set to an https:// origin Slack can reach, \
         such as https://zupflow.example (a tunnel's, for local work)"
    )]
    SlackNeedsPublicUrl,
    #[error("ZUPFLOW_SLACK_API_URL must be an https:// URL like https://slack.com/api, with no query")]
    InvalidSlackApiUrl,
    #[error("ZUPFLOW_IDENTITY must be `local` or `oidc`")]
    UnknownIdentity,
    #[error("ZUPFLOW_IDENTITY is required: set it to `oidc`, or to `local` for one person on their own machine")]
    IdentityRequired,
    /// `local` asked of a build without the `local-identity` feature.
    #[error("ZUPFLOW_IDENTITY=local isn't available: this build has no local sign-in; set it to `oidc`")]
    LocalNotBuilt,
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
            None => return Err(ConfigError::IdentityRequired),
            #[cfg(feature = "local-identity")]
            Some("local") => IdentityMode::Local,
            #[cfg(not(feature = "local-identity"))]
            Some("local") => return Err(ConfigError::LocalNotBuilt),
            Some("oidc") => IdentityMode::Oidc {
                issuer: required("ZUPFLOW_OIDC_ISSUER")?,
                audience: required("ZUPFLOW_OIDC_AUDIENCE")?,
                jwks_url: set("ZUPFLOW_OIDC_JWKS_URL")?,
            },
            Some(_) => return Err(ConfigError::UnknownIdentity),
        };
        #[cfg(feature = "local-identity")]
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
        if set("ZUPFLOW_API_KEY_HASH_KEY")?.is_some() {
            return Err(ConfigError::Renamed {
                old: "ZUPFLOW_API_KEY_HASH_KEY",
                new: "ZUPFLOW_SECRET_KEY",
            });
        }
        if get("ZUPFLOW_MODELS")?.is_some() {
            return Err(ConfigError::Retired {
                name: "ZUPFLOW_MODELS",
                instead: "organizations add their own models under Model providers",
            });
        }
        let secret_key = match set("ZUPFLOW_SECRET_KEY")? {
            Some(value) => {
                use base64::Engine;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value.trim())
                    .map_err(|_| ConfigError::InvalidSecretKey)?;
                Some(<[u8; 32]>::try_from(bytes).map_err(|_| ConfigError::InvalidSecretKey)?)
            }
            None if matches!(identity, IdentityMode::Oidc { .. }) => return Err(ConfigError::SecretKeyRequired),
            None => None,
        };
        let public_url = match set("ZUPFLOW_PUBLIC_URL")? {
            Some(value) => public_origin(value.trim()).ok_or(ConfigError::InvalidPublicUrl)?,
            None => format!("http://localhost:{}", listen.port()),
        };
        let console_origin = set("ZUPFLOW_CONSOLE_ORIGIN")?
            .map(|value| public_origin(value.trim()).ok_or(ConfigError::InvalidConsoleOrigin))
            .transpose()?;
        let model_provider = match set("ZUPFLOW_MODEL_PROVIDER")?.as_deref() {
            None => ModelProviderMode::None,
            Some("providers") => ModelProviderMode::Providers,
            Some("scripted") => ModelProviderMode::Scripted,
            Some(_) => return Err(ConfigError::UnknownModelProvider),
        };
        let channel_provisioner = match set("ZUPFLOW_CHANNEL_PROVISIONER")?.as_deref() {
            None => ChannelProvisionerMode::None,
            Some("scripted") => ChannelProvisionerMode::Scripted,
            Some("slack") => ChannelProvisionerMode::Slack,
            Some(_) => return Err(ConfigError::UnknownChannelProvisioner),
        };
        // Slack delivers events and sends people back only to a public HTTPS address.
        if channel_provisioner == ChannelProvisionerMode::Slack && !public_url.starts_with("https://") {
            return Err(ConfigError::SlackNeedsPublicUrl);
        }
        let slack_api = match set("ZUPFLOW_SLACK_API_URL")? {
            Some(value) => zupflow_slack::SlackApi::new(value.trim()).map_err(|_| ConfigError::InvalidSlackApiUrl)?,
            None => zupflow_slack::SlackApi::default(),
        };
        Ok(Self {
            listen,
            log,
            database,
            identity,
            secret_key,
            allowed_origins,
            console_origin,
            public_url,
            model_provider,
            channel_provisioner,
            slack_api,
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

/// `ZUPFLOW_PUBLIC_URL` as an origin: `http` or `https`, a host, an optional
/// port, and nothing else (a trailing `/` is allowed and dropped).
fn public_origin(value: &str) -> Option<String> {
    let url = url::Url::parse(value).ok()?;
    let plain = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none();
    plain.then(|| url.origin().ascii_serialization())
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

    /// What `oidc` needs, so each test sets only what it's about.
    const OIDC: [(&str, &str); 4] = [
        ("ZUPFLOW_IDENTITY", "oidc"),
        ("ZUPFLOW_OIDC_ISSUER", "https://idp.example"),
        ("ZUPFLOW_OIDC_AUDIENCE", "console"),
        ("ZUPFLOW_SECRET_KEY", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="),
    ];

    /// The configuration from exactly `vars`; everything else unset.
    fn only(vars: &[(&'static str, &str)]) -> Result<Config, ConfigError> {
        let vars: Vec<(&'static str, String)> = vars.iter().map(|(name, value)| (*name, (*value).to_owned())).collect();
        Config::from_vars(move |name| {
            Ok(vars
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone()))
        })
    }

    /// A working `oidc` configuration with `vars` set over it.
    fn oidc_with(vars: &[(&'static str, &str)]) -> Result<Config, ConfigError> {
        let mut all = vars.to_vec();
        all.extend(
            OIDC.into_iter()
                .filter(|(name, _)| !vars.iter().any(|(key, _)| key == name)),
        );
        only(&all)
    }

    #[test]
    fn listens_on_loopback_unless_told_otherwise() {
        let config = oidc_with(&[]).expect("defaults are valid");
        assert_eq!(config.listen, SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(config.log, "info");
        assert_eq!(config.database, PathBuf::from("zupflow.db"));
        assert_eq!(config.public_url, "http://localhost:8080");
    }

    #[test]
    fn requires_the_sign_in_to_be_named() {
        let unset = only(&OIDC[1..]);
        assert!(matches!(unset, Err(ConfigError::IdentityRequired)));
        assert!(matches!(
            oidc_with(&[("ZUPFLOW_IDENTITY", "")]),
            Err(ConfigError::Empty {
                name: "ZUPFLOW_IDENTITY"
            })
        ));
        for unknown in ["none", "LOCAL", "Oidc", "oidc "] {
            assert!(
                matches!(
                    oidc_with(&[("ZUPFLOW_IDENTITY", unknown)]),
                    Err(ConfigError::UnknownIdentity)
                ),
                "{unknown}"
            );
        }
    }

    #[cfg(not(feature = "local-identity"))]
    #[test]
    fn refuses_local_sign_in_when_it_is_not_built() {
        assert!(matches!(
            only(&[("ZUPFLOW_IDENTITY", "local")]),
            Err(ConfigError::LocalNotBuilt)
        ));
    }

    #[cfg(feature = "local-identity")]
    #[test]
    fn signs_in_locally_only_when_asked_and_only_on_loopback() {
        let local = |vars: &[(&'static str, &str)]| {
            let mut all = vec![("ZUPFLOW_IDENTITY", "local")];
            all.extend_from_slice(vars);
            only(&all)
        };
        let config = local(&[]).expect("local is valid on its own");
        assert_eq!(config.identity, IdentityMode::Local);
        assert_eq!(config.secret_key, None, "local generates one when unset");
        for address in ["0.0.0.0:8080", "[::]:8080", "192.168.1.5:8080"] {
            let config = local(&[("ZUPFLOW_LISTEN", address)]);
            assert!(matches!(config, Err(ConfigError::LocalNeedsLoopback)), "{address}");
        }
        for address in ["127.0.0.1:9000", "[::1]:9000"] {
            assert!(local(&[("ZUPFLOW_LISTEN", address)]).is_ok(), "{address}");
        }
    }

    #[test]
    fn takes_a_public_url_that_is_an_origin() {
        let public = |value: &'static str| oidc_with(&[("ZUPFLOW_PUBLIC_URL", value)]);
        assert_eq!(
            public("https://zupflow.example/").expect("an origin").public_url,
            "https://zupflow.example"
        );
        assert_eq!(
            public("http://localhost:3000").expect("an origin").public_url,
            "http://localhost:3000"
        );
        for invalid in [
            "zupflow.example",
            "https://zupflow.example/api",
            "ftp://zupflow.example",
            "https://:443",
            "https://user@zupflow.example",
            "https://zupflow.example/?q=1",
        ] {
            assert!(
                matches!(public(invalid), Err(ConfigError::InvalidPublicUrl)),
                "{invalid}"
            );
        }
    }

    #[test]
    fn takes_one_console_origin_and_stops_on_anything_else() {
        let console = |value: &'static str| oidc_with(&[("ZUPFLOW_CONSOLE_ORIGIN", value)]);
        assert_eq!(oidc_with(&[]).expect("defaults").console_origin, None);
        assert_eq!(
            console("https://Console.Zupflow.example/")
                .expect("an origin")
                .console_origin
                .as_deref(),
            Some("https://console.zupflow.example")
        );
        assert_eq!(
            console("http://localhost:5180")
                .expect("an origin")
                .console_origin
                .as_deref(),
            Some("http://localhost:5180")
        );
        for invalid in [
            "*",
            "null",
            "console.zupflow.example",
            "https://console.zupflow.example/app",
            "https://a.example,https://b.example",
            "https://user@console.zupflow.example",
            "file:///console",
        ] {
            assert!(
                matches!(console(invalid), Err(ConfigError::InvalidConsoleOrigin)),
                "{invalid}"
            );
        }
        assert!(matches!(
            console(""),
            Err(ConfigError::Empty {
                name: "ZUPFLOW_CONSOLE_ORIGIN"
            })
        ));
    }

    #[test]
    fn takes_allowed_origins_and_stops_on_anything_else() {
        let origins = |value: &'static str| oidc_with(&[("ZUPFLOW_ALLOWED_ORIGINS", value)]);
        assert_eq!(
            origins("http://localhost:5173, https://console.example")
                .expect("valid origins")
                .allowed_origins,
            ["http://localhost:5173", "https://console.example"]
        );
        for bad in ["localhost:5173", "http://x/path", "*", "http://"] {
            assert!(matches!(origins(bad), Err(ConfigError::InvalidOrigins)), "{bad}");
        }
    }

    #[test]
    fn signs_in_with_oidc_given_its_provider_and_a_secret_key() {
        let configured = oidc_with(&[("ZUPFLOW_LISTEN", "0.0.0.0:8080")]).expect("oidc may listen beyond loopback");
        assert_eq!(
            configured.identity,
            IdentityMode::Oidc {
                issuer: "https://idp.example".to_owned(),
                audience: "console".to_owned(),
                jwks_url: None,
            }
        );
        assert!(matches!(
            only(&OIDC[..2]),
            Err(ConfigError::Missing {
                name: "ZUPFLOW_OIDC_AUDIENCE"
            })
        ));
        assert!(
            matches!(only(&OIDC[..3]), Err(ConfigError::SecretKeyRequired)),
            "oidc keeps the secret key outside the database"
        );
        for bad in ["not base64!", "AAAA"] {
            assert!(matches!(
                oidc_with(&[("ZUPFLOW_SECRET_KEY", bad)]),
                Err(ConfigError::InvalidSecretKey)
            ));
        }
        assert!(matches!(
            oidc_with(&[(
                "ZUPFLOW_API_KEY_HASH_KEY",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            )]),
            Err(ConfigError::Renamed { .. })
        ));
    }

    #[test]
    fn refuses_the_retired_model_list() {
        for value in ["[]", r#"[{"id":"model-a","name":"A","provider":"Example"}]"#, ""] {
            let refused = oidc_with(&[("ZUPFLOW_MODELS", value)]);
            assert!(
                matches!(
                    refused,
                    Err(ConfigError::Retired {
                        name: "ZUPFLOW_MODELS",
                        ..
                    })
                ),
                "{value:?}"
            );
            assert_eq!(
                refused.err().map(|error| error.to_string()).as_deref(),
                Some(
                    "ZUPFLOW_MODELS is no longer read: organizations add their own models under Model providers; \
                     unset it"
                )
            );
        }
    }

    #[test]
    fn makes_channel_apps_with_the_scripted_provisioner_only_when_asked() {
        let provisioner = |value: &'static str| oidc_with(&[("ZUPFLOW_CHANNEL_PROVISIONER", value)]);
        assert_eq!(
            oidc_with(&[]).expect("defaults").channel_provisioner,
            ChannelProvisionerMode::None
        );
        assert_eq!(
            provisioner("scripted").expect("scripted").channel_provisioner,
            ChannelProvisionerMode::Scripted
        );
        for value in ["fake", "Slack", "Scripted"] {
            assert!(
                matches!(provisioner(value), Err(ConfigError::UnknownChannelProvisioner)),
                "{value}"
            );
        }
    }

    #[test]
    fn makes_channel_apps_in_slack_only_from_a_public_https_address() {
        let public = [
            ("ZUPFLOW_CHANNEL_PROVISIONER", "slack"),
            ("ZUPFLOW_PUBLIC_URL", "https://zupflow.example"),
        ];
        let config = oidc_with(&public).expect("slack");
        assert_eq!(config.channel_provisioner, ChannelProvisionerMode::Slack);
        assert_eq!(config.slack_api, zupflow_slack::SlackApi::default());
        assert_eq!(
            config.slack_api.method_url("auth.test"),
            "https://slack.com/api/auth.test"
        );
        for url in [None, Some("http://zupflow.example")] {
            let mut vars = vec![("ZUPFLOW_CHANNEL_PROVISIONER", "slack")];
            if let Some(url) = url {
                vars.push(("ZUPFLOW_PUBLIC_URL", url));
            }
            assert!(
                matches!(oidc_with(&vars), Err(ConfigError::SlackNeedsPublicUrl)),
                "{url:?}"
            );
        }
        let mut proxied = public.to_vec();
        proxied.push(("ZUPFLOW_SLACK_API_URL", "https://slack-proxy.example/api/"));
        let config = oidc_with(&proxied).expect("a proxy");
        assert_eq!(
            config.slack_api.method_url("auth.test"),
            "https://slack-proxy.example/api/auth.test"
        );
        for url in [
            "http://slack-proxy.example/api",
            "https://slack-proxy.example/api?x=1",
            "https://user:pass@slack-proxy.example/api",
            "slack.com",
        ] {
            let mut vars = public.to_vec();
            vars.push(("ZUPFLOW_SLACK_API_URL", url));
            assert!(
                matches!(oidc_with(&vars), Err(ConfigError::InvalidSlackApiUrl)),
                "{url}"
            );
        }
    }

    #[test]
    fn answers_runs_with_the_scripted_model_only_when_asked() {
        let provider = |value: &'static str| oidc_with(&[("ZUPFLOW_MODEL_PROVIDER", value)]);
        assert_eq!(
            oidc_with(&[]).expect("defaults").model_provider,
            ModelProviderMode::None,
            "no provider unless configured: runs fail rather than answer from a script"
        );
        assert_eq!(
            provider("scripted").expect("scripted").model_provider,
            ModelProviderMode::Scripted
        );
        assert_eq!(
            provider("providers").expect("providers").model_provider,
            ModelProviderMode::Providers
        );
        for bad in ["Scripted", "openai", "none", "Providers", "provider", " providers"] {
            assert!(matches!(provider(bad), Err(ConfigError::UnknownModelProvider)), "{bad}");
        }
        assert!(matches!(
            provider(""),
            Err(ConfigError::Empty {
                name: "ZUPFLOW_MODEL_PROVIDER"
            })
        ));
    }

    #[test]
    fn refuses_an_invalid_address_instead_of_falling_back() {
        let error = oidc_with(&[("ZUPFLOW_LISTEN", "everywhere")]);
        assert!(matches!(
            error,
            Err(ConfigError::InvalidAddress { name: "ZUPFLOW_LISTEN" })
        ));
    }

    #[test]
    fn refuses_an_empty_or_unreadable_value_instead_of_treating_it_as_unset() {
        let empty = oidc_with(&[("ZUPFLOW_LOG", "")]);
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
