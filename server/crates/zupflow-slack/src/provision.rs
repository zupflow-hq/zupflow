//! [`SlackProvisioner`]: makes, changes and deletes one agent's Slack app in
//! a linked team with the team's app configuration token, and installs it
//! with the consent the person gives on Slack's page.
//!
//! | Trait call | Slack |
//! | --- | --- |
//! | `check_tokens` | `apps.manifest.validate`, with the configuration token |
//! | `rotate_tokens` | `tooling.tokens.rotate`, spending the refresh token once |
//! | `create` | `apps.manifest.create` without event subscriptions: the app's ID, client ID, client and signing secrets |
//! | `update` | `apps.manifest.update`, the whole manifest (which subscribes a new app's events) |
//! | `delete` | `apps.manifest.delete` |
//! | `find` | nothing: Slack can't look an app up with a configuration token |
//! | `consent_url` | `https://slack.com/oauth/v2/authorize`, with the app's own client ID |
//! | `complete_consent` | `oauth.v2.access` with the app's client ID and secret, then `auth.test` |
//! | `revoke_install` | `auth.revoke`, with the installed bot token |
//!
//! `apps.manifest.*` are Slack's tier 1 (about a call a minute per token): a
//! `429` is [`ProvisionError::RateLimited`] with Slack's `Retry-After`, for
//! the job to wait out. Nothing here sleeps or retries.
//!
//! The pasted configuration token's age isn't known (Slack says only when a
//! rotation's token expires), so a newly checked pair has no expiry: the
//! rotation job rotates it first, and learns the real one.

use async_trait::async_trait;
use serde_json::{Map, Value};
use zeroize::Zeroizing;
use zupflow_core::channels::provision::{
    AppManifest, AppProvisioner, AppSecrets, CheckedTokens, ConsentExchange, ConsentRequest, ConsentedInstall,
    CreatedApp, InstallStep, ManagerTokens, ProviderInstall, ProvisionError, RotatedTokens,
};
use zupflow_core::channels::{ChannelProviderKind, ConnectionTeam, Identity, ManagerMethod};
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::time::Timestamp;

use crate::api::{Auth, SlackApi, SlackCode, SlackError, call, object, text};
use crate::manifest::{probe, slack_manifest_text, slack_manifest_unsubscribed};

/// The longest token or secret read from an answer.
const SECRET_MAX: usize = 4096;
/// The longest ID or name read from an answer.
const NAME_MAX: usize = 100;

/// The configuration-token provisioner, reaching Slack's Web API at `api`.
#[derive(Debug, Clone, Default)]
pub struct SlackProvisioner {
    api: SlackApi,
}

impl SlackProvisioner {
    pub fn new(api: SlackApi) -> Self {
        Self { api }
    }

    pub fn api(&self) -> &SlackApi {
        &self.api
    }
}

/// A Slack failure as a provisioner's: a refused token, a code, or "later".
fn provision_error(error: SlackError) -> ProvisionError {
    match error {
        SlackError::RateLimited { retry_after } => ProvisionError::RateLimited {
            retry_after_secs: u32::try_from(retry_after.as_secs()).unwrap_or(u32::MAX),
        },
        SlackError::Refused(SlackCode::InvalidAuth) => ProvisionError::TokensRefused,
        SlackError::Refused(SlackCode::AppNotFound) => ProvisionError::AppNotFound,
        SlackError::Refused(code) if code.transient() => ProvisionError::Unavailable,
        SlackError::Refused(code) => ProvisionError::Refused(code.as_str()),
        SlackError::NoAnswer | SlackError::Unavailable | SlackError::Unreadable => ProvisionError::Unavailable,
    }
}

/// A secret from an answer: present, and of a sane length.
fn secret(answer: &Map<String, Value>, field: &str) -> Option<Zeroizing<String>> {
    text(answer, field)
        .filter(|value| value.len() <= SECRET_MAX)
        .map(|value| Zeroizing::new(value.to_owned()))
}

/// An ID or name from an answer, at most [`NAME_MAX`] characters.
fn name(answer: &Map<String, Value>, field: &str) -> Option<String> {
    text(answer, field)
        .filter(|value| value.chars().count() <= NAME_MAX)
        .map(str::to_owned)
}

/// Seconds since the epoch, as Slack writes `exp`, as a time.
fn at_seconds(seconds: i64) -> Option<Timestamp> {
    seconds.checked_mul(1000).map(Timestamp::from_millis)
}

#[async_trait]
impl AppProvisioner for SlackProvisioner {
    fn method(&self) -> ManagerMethod {
        ManagerMethod::ConfigurationToken
    }

    async fn check_tokens(
        &self,
        http: &dyn OutboundHttp,
        provider: ChannelProviderKind,
        tokens: &ManagerTokens,
    ) -> Result<CheckedTokens, ProvisionError> {
        if provider != ChannelProviderKind::Slack {
            return Err(ProvisionError::NotConfigured);
        }
        let manifest = probe();
        let checked = call(
            http,
            &self.api,
            "apps.manifest.validate",
            Auth::Bearer(&tokens.configuration_token),
            &[("manifest", manifest.as_str())],
        )
        .await;
        match checked {
            // A token that reached validation is one Slack accepts.
            Ok(_) | Err(SlackError::Refused(SlackCode::InvalidManifest)) => Ok(CheckedTokens { expires_at: None }),
            // A token that can't manage apps is no use either.
            Err(SlackError::Refused(SlackCode::InvalidAuth | SlackCode::NoPermission | SlackCode::MissingScope)) => {
                Err(ProvisionError::TokensRefused)
            }
            Err(error) => Err(provision_error(error)),
        }
    }

    async fn rotate_tokens(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
    ) -> Result<RotatedTokens, ProvisionError> {
        let answer = call(
            http,
            &self.api,
            "tooling.tokens.rotate",
            Auth::None,
            &[("refresh_token", tokens.refresh_token.as_str())],
        )
        .await
        .map_err(provision_error)?;
        let (Some(configuration_token), Some(refresh_token)) =
            (secret(&answer, "token"), secret(&answer, "refresh_token"))
        else {
            // The refresh token is spent, and nothing usable came back.
            tracing::warn!(event = "slack.rotation_unreadable");
            return Err(ProvisionError::Unavailable);
        };
        Ok(RotatedTokens {
            tokens: ManagerTokens {
                configuration_token,
                refresh_token,
            },
            expires_at: answer.get("exp").and_then(Value::as_i64).and_then(at_seconds),
        })
    }

    async fn create(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        manifest: &AppManifest,
    ) -> Result<CreatedApp, ProvisionError> {
        // Its events are subscribed by the update that follows, once its
        // signing secret is stored to answer Slack's challenge.
        let text_manifest = slack_manifest_unsubscribed(manifest).to_string();
        let answer = call(
            http,
            &self.api,
            "apps.manifest.create",
            Auth::Bearer(&tokens.configuration_token),
            &[("manifest", text_manifest.as_str())],
        )
        .await
        .map_err(provision_error)?;
        let credentials = object(&answer, "credentials");
        let made = (
            name(&answer, "app_id"),
            credentials.and_then(|credentials| name(credentials, "client_id")),
            credentials.and_then(|credentials| secret(credentials, "client_secret")),
            credentials.and_then(|credentials| secret(credentials, "signing_secret")),
        );
        let (Some(external_app_id), Some(client_id), Some(client_secret), Some(signing_secret)) = made else {
            // Perhaps made all the same; its secrets can't be kept, so it can't be either.
            tracing::warn!(event = "slack.create_unreadable");
            return Err(ProvisionError::Unavailable);
        };
        Ok(CreatedApp {
            external_app_id,
            client_id,
            secrets: AppSecrets {
                client_secret,
                signing_secret,
            },
            install: InstallStep::NeedsConsent,
        })
    }

    /// Slack has no way to look an app up with a configuration token, so
    /// an app whose `create` answer was lost can't be found: it stays in the
    /// team, named after the agent, uninstalled, until someone removes it
    /// there. Logged, so an operator can tell.
    async fn find(
        &self,
        _: &dyn OutboundHttp,
        _: &ManagerTokens,
        _events_url: &str,
    ) -> Result<Option<String>, ProvisionError> {
        tracing::warn!(event = "slack.lost_app_unfindable");
        Ok(None)
    }

    async fn update(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        external_app_id: &str,
        manifest: &AppManifest,
    ) -> Result<(), ProvisionError> {
        let text_manifest = slack_manifest_text(manifest);
        call(
            http,
            &self.api,
            "apps.manifest.update",
            Auth::Bearer(&tokens.configuration_token),
            &[("app_id", external_app_id), ("manifest", text_manifest.as_str())],
        )
        .await
        .map(|_| ())
        .map_err(provision_error)
    }

    async fn delete(
        &self,
        http: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        external_app_id: &str,
    ) -> Result<(), ProvisionError> {
        call(
            http,
            &self.api,
            "apps.manifest.delete",
            Auth::Bearer(&tokens.configuration_token),
            &[("app_id", external_app_id)],
        )
        .await
        .map(|_| ())
        .map_err(provision_error)
    }

    fn consent_url(&self, request: &ConsentRequest<'_>) -> Result<String, ProvisionError> {
        let mut url = url::Url::parse(self.api.authorize_url()).map_err(|_| ProvisionError::NotConfigured)?;
        url.query_pairs_mut()
            .append_pair("client_id", request.client_id)
            // Slack separates scopes with commas.
            .append_pair("scope", &request.scopes.join(","))
            .append_pair("redirect_uri", request.redirect_url)
            .append_pair("state", request.state);
        Ok(url.into())
    }

    async fn complete_consent(
        &self,
        http: &dyn OutboundHttp,
        exchange: &ConsentExchange<'_>,
    ) -> Result<ConsentedInstall, ProvisionError> {
        let answer = call(
            http,
            &self.api,
            "oauth.v2.access",
            Auth::Basic {
                id: exchange.client_id,
                secret: exchange.client_secret,
            },
            &[("code", exchange.code), ("redirect_uri", exchange.redirect_url)],
        )
        .await
        .map_err(|error| match error {
            SlackError::Refused(code) if !code.transient() => ProvisionError::Refused("token_exchange_failed"),
            other => provision_error(other),
        })?;
        let team = object(&answer, "team");
        let token_type = text(&answer, "token_type");
        let read = (
            secret(&answer, "access_token").filter(|_| token_type.is_none_or(|kind| kind == "bot")),
            name(&answer, "bot_user_id"),
            team.and_then(|team| name(team, "id")),
        );
        let (Some(token), Some(bot_user_id), Some(team_id)) = read else {
            tracing::warn!(event = "slack.install_unreadable");
            return Err(ProvisionError::Refused("token_exchange_failed"));
        };
        // Who the bot user is called, and the bot ID its posts carry: from
        // the token itself.
        let (handle, bot_id) = match call(http, &self.api, "auth.test", Auth::Bearer(&token), &[]).await {
            Ok(identity) if text(&identity, "user_id") == Some(bot_user_id.as_str()) => (
                name(&identity, "user").unwrap_or_else(|| bot_user_id.clone()),
                name(&identity, "bot_id"),
            ),
            Ok(_) => {
                tracing::warn!(event = "slack.install_identity_mismatch");
                let _ = self.revoke_install(http, &token).await;
                return Err(ProvisionError::Refused("token_exchange_failed"));
            }
            Err(error) => {
                // Not kept, so not left working.
                let _ = self.revoke_install(http, &token).await;
                return Err(provision_error(error));
            }
        };
        let granted_scopes = text(&answer, "scope")
            .map(|scopes| {
                scopes
                    .split(',')
                    .map(str::trim)
                    .filter(|scope| !scope.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let enterprise = object(&answer, "enterprise").and_then(|enterprise| name(enterprise, "id"));
        let expires_at = answer
            .get("expires_in")
            .and_then(Value::as_i64)
            .and_then(|seconds| seconds.checked_mul(1000))
            .map(|millis| Timestamp::from_millis(Timestamp::now().millis().saturating_add(millis)));
        Ok(ConsentedInstall {
            external_app_id: name(&answer, "app_id"),
            install: ProviderInstall {
                team: ConnectionTeam {
                    id: team_id,
                    name: team.and_then(|team| name(team, "name")),
                    enterprise_id: enterprise,
                },
                identity: Identity {
                    external_id: bot_user_id,
                    handle,
                    bot_id,
                },
                granted_scopes,
                token,
            },
            refresh_token: secret(&answer, "refresh_token"),
            expires_at,
        })
    }

    async fn revoke_install(&self, http: &dyn OutboundHttp, token: &str) -> Result<(), ProvisionError> {
        match call(http, &self.api, "auth.revoke", Auth::Bearer(token), &[]).await {
            // Revoked already: done.
            Ok(_) | Err(SlackError::Refused(SlackCode::InvalidAuth)) => Ok(()),
            Err(error) => Err(provision_error(error)),
        }
    }
}
