//! Installing an agent's app in its team through hosted Connect: a
//! `channel_app` session's page sends the person to the provider's consent
//! page with the app's own client, and the provider sends them back to the
//! shared callback, which exchanges the code for the app's token and
//! installs it.
//!
//! **State.** Each start is a new attempt with a fresh `state`, kept only
//! as its hash beside the session (the same single-use attempts an OAuth
//! app's session uses): the callback finds the session by it and spends it
//! in one statement, so a `state` works once, only for the newest attempt,
//! and only while its session is pending. What the callback needs back (the
//! link's reference) is sealed under the attempt's own context, which names
//! the organization, the workspace and the session. Everything after the
//! claim uses the session's own organization and workspace, and the app is
//! the one the session was made for: a callback can't attach an install to
//! another workspace's app.
//!
//! **Install.** The code is exchanged with the app's own client ID and
//! secret (opened only here), the team checked against the manager's (the
//! first install teaches it), the token sealed as the connection's next
//! version, and the connection, the agent's channel and the session's
//! completion written as one. A token that isn't kept is revoked at the
//! provider, unless it may be the live connection's own (a reinstall).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};
use zupflow_core::channels::provision::{
    ConsentExchange, ConsentRequest, ConsentedInstall, ProvisionError, SLACK_SCOPES,
};
use zupflow_core::channels::store::{ChannelError, NewInstall, SealedVersion};
use zupflow_core::channels::{ChannelApp, ChannelProviderKind, ConnectTarget, FIRST_VERSION, MAX_LABEL, ManagerStatus};
use zupflow_core::connect::{Authorization, ConnectError, ConnectSession, ConnectSessionStatus, OAuthState};
use zupflow_core::error::ApiError;
use zupflow_core::ids::ChannelConnectionId;
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::time::Timestamp;

use crate::auth::AppState;
use crate::connect::{connect_failure, not_pending};
use crate::connect_oauth::{Failed, back_to};
use crate::http::{Failure, envelope};

/// What the callback needs back, sealed while the provider has the browser.
#[derive(Serialize)]
struct Sealing<'a> {
    reference: &'a str,
}

/// [`Sealing`], opened. Wiped when dropped.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct Pending {
    reference: String,
}

/// An app's own secrets, opened for the exchange. Wiped when dropped:
/// what's kept is moved out, never copied.
#[derive(Deserialize, Zeroize, ZeroizeOnDrop)]
struct OpenedSecrets {
    client_secret: String,
    signing_secret: String,
}

fn attempt_context(session: &ConnectSession, attempt: u32) -> EncryptionContext {
    EncryptionContext::channel_connect_authorization(
        session.organization_id,
        session.workspace_id,
        session.environment,
        session.id,
        attempt,
    )
}

fn wrong_auth_type() -> Failure {
    ApiError::new(
        422,
        "wrong_auth_type",
        "Linking a team takes a configuration token, pasted on this page.",
    )
    .into()
}

/// An app that can be installed now, with its client ID: made, its manager
/// active. Read in the session's own scope.
async fn installable(state: &AppState, session: &ConnectSession) -> Result<(ChannelApp, String), Failed> {
    let Some(ConnectTarget::ChannelApp { app, .. }) = session.target else {
        return Err(Failed("app_not_eligible"));
    };
    let (organization_id, workspace_id) = (session.organization_id, session.workspace_id);
    let app = state
        .channels
        .find_app(organization_id, workspace_id, app)
        .await
        .map_err(|_| Failed("unavailable"))?
        .filter(|app| app.status.is_installable() && app.external_app_id.is_some())
        .ok_or(Failed("app_not_eligible"))?;
    let active = state
        .channels
        .find_manager(organization_id, workspace_id, app.manager_id)
        .await
        .map_err(|_| Failed("unavailable"))?
        .is_some_and(|manager| manager.status == ManagerStatus::Active);
    if !active {
        return Err(Failed("app_not_eligible"));
    }
    let client_id = state
        .channels
        .app_secrets(organization_id, workspace_id, app.id)
        .await
        .map_err(|_| Failed("unavailable"))?
        .and_then(|secrets| secrets.client_id)
        .ok_or(Failed("app_not_eligible"))?;
    Ok((app, client_id))
}

/// Starts the consent for a pending `channel_app` session: a new attempt
/// with a fresh `state`, and the provider's consent page for the app's own
/// client. Only the newest attempt's `state` completes the session.
pub(crate) async fn authorize(
    state: &AppState,
    session: &ConnectSession,
    reference: &str,
) -> Result<Response, Failure> {
    let Some(ConnectTarget::ChannelApp { provider, .. }) = session.target else {
        return Err(wrong_auth_type());
    };
    let (app, client_id) = match installable(state, session).await {
        Ok(found) => found,
        Err(Failed("app_not_eligible")) => return Err(crate::channels::app_not_eligible()),
        Err(_) => return Err(connect_failure(ConnectError::Unavailable)),
    };
    let Some(attempt) = session.authorization_attempt.checked_add(1) else {
        return Err(ApiError::internal().into());
    };
    let oauth_state = OAuthState::generate().map_err(connect_failure)?;
    let plaintext =
        Zeroizing::new(serde_json::to_vec(&Sealing { reference }).map_err(|_| Failure(ApiError::internal()))?);
    let sealed = keys::seal(state.keys.as_ref(), &attempt_context(session, attempt), &plaintext)
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.seal_failed", session_id = %session.id, %error);
            connect_failure(ConnectError::Unavailable)
        })?;
    let begun = state
        .connect
        .begin_authorization(
            session.organization_id,
            session.workspace_id,
            session.id,
            &Authorization {
                state_hash: oauth_state.hash(),
                attempt,
                sealed,
            },
            Timestamp::now(),
        )
        .await;
    match begun {
        Ok(()) => {}
        Err(ConnectError::NotPending) => {
            let now = state
                .connect
                .find_session(session.organization_id, session.workspace_id, session.id)
                .await
                .map_err(connect_failure)?
                .map_or(ConnectSessionStatus::Expired, |session| {
                    session.status_at(Timestamp::now())
                });
            return Err(not_pending(now));
        }
        Err(error) => return Err(connect_failure(error)),
    }
    let redirect_url = crate::app_configurations::redirect_uri(&state.public_url);
    let url = state
        .channel_providers
        .provisioner(provider)
        .consent_url(&ConsentRequest {
            client_id: &client_id,
            scopes: SLACK_SCOPES,
            redirect_url: &redirect_url,
            state: oauth_state.expose(),
        })
        .map_err(|error| crate::channels::provision_failure(error, provider))?;
    tracing::info!(event = "channels.install_started", organization_id = %session.organization_id, workspace_id = %session.workspace_id, app_id = %app.id, session_id = %session.id);
    Ok(envelope(
        StatusCode::OK,
        serde_json::json!({ "authorization_url": url }),
    ))
}

/// The provider sent the person back for a `channel_app` session whose
/// attempt `authorization` was just claimed (and so spent): installs the
/// app, or fails the session with why, then back to the hosted page.
pub(crate) async fn callback(
    state: &AppState,
    session: ConnectSession,
    authorization: Authorization,
    params: &BTreeMap<String, String>,
) -> Response {
    let opened = keys::open(
        state.keys.as_ref(),
        &attempt_context(&session, authorization.attempt),
        &authorization.sealed,
    )
    .await;
    let mut pending: Pending = match opened.map(|plaintext| serde_json::from_slice(plaintext.expose())) {
        Ok(Ok(pending)) => pending,
        _ => {
            tracing::error!(event = "channels.authorization_unopenable", session_id = %session.id);
            return Failure(ApiError::internal()).into_response();
        }
    };
    let reference = Zeroizing::new(std::mem::take(&mut pending.reference));
    let outcome = match (params.get("code"), params.get("error")) {
        // Slack names no issuer; one that does isn't Slack answering.
        _ if params.contains_key("iss") => Err(Failed("authorization_failed")),
        (_, Some(error)) => {
            // The provider's own words are never passed on, only whether consent was refused.
            tracing::info!(event = "channels.consent_error", session_id = %session.id, denied = error == "access_denied");
            Err(Failed(if error == "access_denied" {
                "consent_denied"
            } else {
                "authorization_failed"
            }))
        }
        (Some(code), None) if !code.is_empty() && code.len() <= 512 => {
            install(state, &session, &Zeroizing::new(code.clone())).await
        }
        _ => Err(Failed("authorization_failed")),
    };
    if let Err(Failed(failure_code)) = outcome {
        tracing::info!(event = "channels.install_failed", session_id = %session.id, failure_code);
        match state
            .connect
            .fail_session(
                session.organization_id,
                session.workspace_id,
                session.id,
                failure_code,
                Timestamp::now(),
            )
            .await
        {
            // Expired or already finished: the page says so.
            Ok(_) | Err(ConnectError::NotPending) => {}
            Err(error) => {
                tracing::warn!(event = "connect.fail_session_failed", session_id = %session.id, ?error);
            }
        }
    }
    back_to(state, &reference)
}

/// The app's client secret, opened for the exchange and nothing else.
async fn client_secret(
    state: &AppState,
    session: &ConnectSession,
    app: &ChannelApp,
) -> Result<Zeroizing<String>, Failed> {
    let sealed = state
        .channels
        .app_secrets(session.organization_id, session.workspace_id, app.id)
        .await
        .map_err(|_| Failed("unavailable"))?
        .and_then(|record| record.secrets)
        .ok_or(Failed("app_not_eligible"))?;
    let context = EncryptionContext::channel_app_secret(
        session.organization_id,
        session.workspace_id,
        session.environment,
        app.id,
        sealed.version,
    );
    let opened = keys::open(state.keys.as_ref(), &context, &sealed.sealed)
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.secrets_unopenable", app_id = %app.id, %error);
            Failed("unavailable")
        })?;
    let mut secrets: OpenedSecrets = serde_json::from_slice(opened.expose()).map_err(|_| Failed("unavailable"))?;
    Ok(Zeroizing::new(std::mem::take(&mut secrets.client_secret)))
}

/// A connection's token, with what rotates it when the provider does, sealed as one value.
#[derive(Serialize)]
pub(crate) struct SealingConnection<'a> {
    pub(crate) token: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) refresh_token: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<i64>,
}

/// Revokes a token Zupflow won't keep, so it doesn't keep working at the provider.
async fn discard(state: &AppState, session: &ConnectSession, provider: ChannelProviderKind, token: &str) {
    if let Err(error) = state
        .channel_providers
        .provisioner(provider)
        .revoke_install(state.outbound.as_ref(), token)
        .await
    {
        tracing::warn!(event = "channels.unkept_token_not_revoked", session_id = %session.id, %error);
    }
}

/// Exchanges the code and installs the app the session was made for.
async fn install(state: &AppState, session: &ConnectSession, code: &str) -> Result<(), Failed> {
    let now = Timestamp::now();
    // An expired session isn't completed: its code isn't even exchanged.
    if session.status_at(now) != ConnectSessionStatus::Pending {
        return Err(Failed("session_expired"));
    }
    let (organization_id, workspace_id) = (session.organization_id, session.workspace_id);
    let (app, client_id) = installable(state, session).await?;
    let provider = app.provider;
    let client_secret = client_secret(state, session, &app).await?;
    let redirect_url = crate::app_configurations::redirect_uri(&state.public_url);
    let consented = state
        .channel_providers
        .provisioner(provider)
        .complete_consent(
            state.outbound.as_ref(),
            &ConsentExchange {
                client_id: &client_id,
                client_secret: &client_secret,
                code,
                redirect_url: &redirect_url,
            },
        )
        .await
        .map_err(|error| match error {
            ProvisionError::Refused(_) | ProvisionError::TokensRefused | ProvisionError::AppNotFound => {
                Failed("token_exchange_failed")
            }
            ProvisionError::Unavailable | ProvisionError::RateLimited { .. } | ProvisionError::NotConfigured => {
                Failed("provider_unavailable")
            }
        })?;
    let ConsentedInstall {
        external_app_id,
        install,
        refresh_token,
        expires_at,
    } = consented;
    // The app the provider says it installed is the one the session is for.
    if external_app_id.is_some() && external_app_id != app.external_app_id {
        tracing::warn!(event = "channels.install_of_another_app", %organization_id, %workspace_id, app_id = %app.id);
        discard(state, session, provider, &install.token).await;
        return Err(Failed("token_exchange_failed"));
    }
    let manager = state
        .channels
        .find_manager(organization_id, workspace_id, app.manager_id)
        .await
        .map_err(|_| Failed("unavailable"))?
        .ok_or(Failed("app_not_eligible"))?;
    // Checked here so a token for another team is revoked at once; the store checks it again.
    if manager.team.id.as_deref().is_some_and(|team| team != install.team.id) {
        tracing::warn!(event = "channels.install_team_mismatch", %organization_id, %workspace_id, app_id = %app.id);
        discard(state, session, provider, &install.token).await;
        return Err(Failed("team_mismatch"));
    }
    // A reinstall writes the live connection's next version; the provider
    // may answer it with the same token, which mustn't be revoked then.
    let live = match app.connection_id {
        Some(connection_id) => state
            .channels
            .find_connection(organization_id, workspace_id, connection_id)
            .await
            .map_err(|_| Failed("unavailable"))?,
        None => None,
    };
    let (connection_id, version) = match &live {
        Some(live) => (
            live.id,
            live.credential_version
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Failed("unavailable"))?,
        ),
        None => (ChannelConnectionId::generate(), FIRST_VERSION),
    };
    let first = live.is_none();
    let token: &str = &install.token;
    let unkept = |why: &'static str| async move {
        if first || why == "team_mismatch" {
            discard(state, session, provider, token).await;
        }
        Failed(why)
    };
    let plaintext = match serde_json::to_vec(&SealingConnection {
        token: &install.token,
        refresh_token: refresh_token.as_deref().map(String::as_str),
        expires_at: expires_at.map(Timestamp::millis),
    }) {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(_) => return Err(unkept("unavailable").await),
    };
    let context = EncryptionContext::channel_connection_credential(
        organization_id,
        workspace_id,
        session.environment,
        connection_id,
        version,
    );
    let sealed = match keys::seal(state.keys.as_ref(), &context, &plaintext).await {
        Ok(sealed) => sealed,
        Err(error) => {
            tracing::error!(event = "channels.seal_failed", %organization_id, %workspace_id, app_id = %app.id, %error);
            return Err(unkept("unavailable").await);
        }
    };
    let label = match state
        .agents
        .find_agent(organization_id, workspace_id, app.agent_id)
        .await
    {
        Ok(Some(agent)) => agent.name.chars().take(MAX_LABEL).collect(),
        Ok(None) => return Err(unkept("app_not_eligible").await),
        Err(_) => return Err(unkept("unavailable").await),
    };
    let installed = state
        .channels
        .install_app_through_session(
            organization_id,
            workspace_id,
            session.id,
            app.id,
            &NewInstall {
                connection_id,
                team: install.team.clone(),
                identity: install.identity.clone(),
                granted_scopes: install.granted_scopes.clone(),
                label,
                credential: SealedVersion { version, sealed },
            },
            Timestamp::now(),
        )
        .await;
    match installed {
        Ok((_, connection, _)) => {
            tracing::info!(event = "channels.app_installed", %organization_id, %workspace_id, app_id = %app.id, connection_id = %connection.id, session_id = %session.id);
            Ok(())
        }
        Err(error) => {
            let why = match error {
                ChannelError::TeamMismatch => "team_mismatch",
                ChannelError::InstallChanged => "installation_changed",
                ChannelError::SessionNotPending | ChannelError::SessionNotFound => "session_expired",
                ChannelError::AppNotReady | ChannelError::AppNotFound | ChannelError::ManagerInactive => {
                    "app_not_eligible"
                }
                _ => "unavailable",
            };
            tracing::warn!(event = "channels.install_not_recorded", %organization_id, %workspace_id, app_id = %app.id, %error);
            Err(unkept(why).await)
        }
    }
}
