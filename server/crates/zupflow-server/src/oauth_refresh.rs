//! Single-use OAuth refresh: a durable, version-bound claim is committed
//! before sending. An interrupted exchange is never attempted again.
use std::time::Duration;

use serde::Deserialize;
use zeroize::{Zeroize, Zeroizing};
use zupflow_core::apps::{AuthTemplate, ClientAuthentication};
use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::{DiscoveredClient, Installation, InstalledApp, NewCredential, RefreshClaim};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::time::Timestamp;

use crate::auth::AppState;
use crate::connect_oauth::{Failed, OAuthCredential, TokenClient, request_tokens};
use crate::mcp_access::Unreachable;

const CLAIM_MAX_MS: i64 = 60_000;

fn unavailable() -> Unreachable {
    Unreachable::Retry("store_unavailable")
}
fn reconnect() -> Unreachable {
    Unreachable::Skip("reconnect_required")
}

pub(crate) async fn require_reconnect(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation_id: InstallationId,
    version: u32,
) -> Result<(), Unreachable> {
    state
        .installations
        .require_reconnect(
            organization_id,
            workspace_id,
            installation_id,
            version,
            Timestamp::now(),
            &|installation| {
                AuditEntry::new(
                    AuditActor::System {
                        name: "Credentials".to_owned(),
                    },
                    AuditAction::InstallationReconnectRequired,
                    AuditTarget::new(AuditObject::Installation(installation.id), &installation.label),
                    AuditPlace::Workspace(workspace_id),
                )
            },
        )
        .await
        .map_err(|_| unavailable())?;
    Ok(())
}

pub(crate) async fn forget_registration(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    app: InstalledApp,
    client: &DiscoveredClient,
) -> Result<(), Unreachable> {
    match app {
        InstalledApp::Catalog(app_id) => {
            state
                .apps
                .forget_organization_client_registration(
                    organization_id,
                    app_id,
                    environment,
                    &client.issuer,
                    &client.client_id,
                )
                .await
        }
        InstalledApp::Workspace(app_id) => {
            state
                .apps
                .forget_workspace_client_registration(
                    organization_id,
                    workspace_id,
                    app_id,
                    &client.issuer,
                    &client.client_id,
                )
                .await
        }
    }
    .map_err(|_| unavailable())
}

#[derive(Deserialize)]
struct StoredTokens {
    access_token: String,
    refresh_token: Option<String>,
}
impl Drop for StoredTokens {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

#[derive(Deserialize)]
struct Issued {
    client_secret: Option<String>,
    registration_access_token: Option<String>,
}
impl Drop for Issued {
    fn drop(&mut self) {
        self.client_secret.zeroize();
        self.registration_access_token.zeroize();
    }
}

/// Refreshes only `version`; callers re-read on every outcome where another
/// worker won. A pending claim may complete, but may never be stolen.
pub(crate) async fn refresh(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    installation: &Installation,
) -> Result<(), Unreachable> {
    let version = installation
        .connection
        .as_ref()
        .ok_or_else(reconnect)?
        .credential_version;
    let Some(snapshot) = &installation.snapshot else {
        return Err(reconnect());
    };
    if matches!(snapshot.auth, AuthTemplate::Token { .. }) {
        require_reconnect(state, organization_id, workspace_id, installation.id, version).await?;
        return Err(reconnect());
    }
    let workspace = state
        .workspaces
        .find_workspace(organization_id, workspace_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(reconnect)?;
    let environment = workspace.environment;
    let material = async {
        let sealed = state
            .installations
            .find_credential(organization_id, workspace_id, installation.id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(reconnect)?;
        if sealed.version != version {
            return Err(Unreachable::Retry("credential_replaced"));
        }
        let context = EncryptionContext::installation_credential(
            organization_id,
            workspace_id,
            environment,
            installation.app.id(),
            installation.id,
            version,
        );
        let opened = keys::open(state.keys.as_ref(), &context, &sealed.sealed)
            .await
            .map_err(|_| Unreachable::Retry("credential_unopenable"))?;
        let tokens: StoredTokens = serde_json::from_slice(opened.expose()).map_err(|_| reconnect())?;
        if tokens.refresh_token.as_deref().is_none_or(str::is_empty) {
            return Err(reconnect());
        }
        let (token_url, client_id, secret) =
            client(state, organization_id, workspace_id, environment, installation).await?;
        Ok((tokens, token_url, client_id, secret))
    }
    .await;
    let (tokens, token_url, client_id, secret) = match material {
        Ok(material) => material,
        // A winner committed while the caller's snapshot was being read.
        // No refresh was sent; the caller reads that new version again.
        Err(Unreachable::Retry("credential_replaced")) => return Ok(()),
        Err(error) if error.needs_reconnect() => {
            require_reconnect(state, organization_id, workspace_id, installation.id, version).await?;
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        match state
            .installations
            .claim_refresh(
                organization_id,
                workspace_id,
                installation.id,
                version,
                Timestamp::now(),
            )
            .await
            .map_err(|_| unavailable())?
        {
            RefreshClaim::Claimed => break,
            RefreshClaim::Changed => return Ok(()),
            RefreshClaim::Pending { started_at } => {
                if Timestamp::now().millis().saturating_sub(started_at.millis()) >= CLAIM_MAX_MS {
                    require_reconnect(state, organization_id, workspace_id, installation.id, version).await?;
                    return Err(reconnect());
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(Unreachable::Retry("refresh_in_progress"));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
    // Everything after the claim may consume the refresh token. Failure,
    // cancellation or a crash leaves the claim spent, never safe to retry.
    let refreshed = async {
        let client = TokenClient {
            client_id: &client_id,
            secret: secret.as_deref().map(String::as_str),
            // Preserve the authorization's audience. Configured OAuth did
            // not request a resource; MCP OAuth saved its original one.
            resource: match &snapshot.auth {
                AuthTemplate::McpOAuth { .. } => snapshot.client.as_ref().map(|client| client.resource.as_str()),
                _ => None,
            },
        };
        let refresh_token = tokens.refresh_token.as_deref().ok_or(Failed("token_exchange_failed"))?;
        let next = request_tokens(
            state,
            installation.app.id(),
            &token_url,
            &client,
            vec![("grant_type", "refresh_token"), ("refresh_token", refresh_token)],
        )
        .await?;
        let new_version = version.checked_add(1).ok_or(Failed("unavailable"))?;
        let plaintext = Zeroizing::new(
            serde_json::to_vec(&OAuthCredential {
                access_token: &next.access_token,
                refresh_token: next
                    .refresh_token
                    .as_deref()
                    .map(String::as_str)
                    .or(Some(refresh_token)),
                token_type: next.token_type.as_deref(),
            })
            .map_err(|_| Failed("unavailable"))?,
        );
        let context = EncryptionContext::installation_credential(
            organization_id,
            workspace_id,
            environment,
            installation.app.id(),
            installation.id,
            new_version,
        );
        let sealed = keys::seal(state.keys.as_ref(), &context, &plaintext)
            .await
            .map_err(|_| Failed("unavailable"))?;
        let now = Timestamp::now();
        let expires_at = next
            .expires_in
            .filter(|seconds| *seconds >= 0)
            .and_then(|seconds| seconds.checked_mul(1000))
            .and_then(|millis| now.millis().checked_add(millis))
            .map(Timestamp::from_millis);
        state
            .installations
            .rotate_credential(
                organization_id,
                workspace_id,
                installation.id,
                &NewCredential {
                    sealed,
                    version: new_version,
                },
                expires_at,
                now,
            )
            .await
            .map_err(|_| Failed("unavailable"))?;
        Ok::<(), Failed>(())
    }
    .await;
    if let Err(Failed(code)) = refreshed {
        if code == "invalid_client"
            && let Some(client) = &snapshot.client
        {
            forget_registration(
                state,
                organization_id,
                workspace_id,
                environment,
                installation.app,
                client,
            )
            .await?;
        }
        require_reconnect(state, organization_id, workspace_id, installation.id, version).await?;
        return Err(reconnect());
    }
    Ok(())
}

async fn client(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    installation: &Installation,
) -> Result<(String, String, Option<Zeroizing<String>>), Unreachable> {
    let snapshot = installation.snapshot.as_ref().ok_or_else(reconnect)?;
    let own_workspace = matches!(installation.app, InstalledApp::Workspace(_)).then_some(workspace_id);
    let app_id = installation.app.id();
    match &snapshot.auth {
        AuthTemplate::McpOAuth { .. } => {
            let client = snapshot.client.as_ref().ok_or_else(reconnect)?;
            let registration = match installation.app {
                InstalledApp::Catalog(_) => {
                    state
                        .apps
                        .find_organization_client_registration(organization_id, app_id, environment, &client.issuer)
                        .await
                }
                InstalledApp::Workspace(_) => {
                    state
                        .apps
                        .find_workspace_client_registration(organization_id, workspace_id, app_id, &client.issuer)
                        .await
                }
            }
            .map_err(|_| unavailable())?
            .filter(|registration| registration.client_id == client.client_id)
            .ok_or_else(reconnect)?;
            let secret = if registration.authentication == ClientAuthentication::None {
                None
            } else {
                let sealed = registration.sealed.as_ref().ok_or_else(reconnect)?;
                let context = EncryptionContext::oauth_client_registration(
                    organization_id,
                    own_workspace,
                    environment,
                    app_id,
                    registration.id,
                );
                let opened = keys::open(state.keys.as_ref(), &context, sealed)
                    .await
                    .map_err(|_| unavailable())?;
                let issued: Issued = serde_json::from_slice(opened.expose()).map_err(|_| unavailable())?;
                Some(Zeroizing::new(
                    issued.client_secret.as_ref().ok_or_else(reconnect)?.clone(),
                ))
            };
            Ok((client.token_endpoint.clone(), client.client_id.clone(), secret))
        }
        AuthTemplate::OAuth2 {
            token_url: Some(token_url),
            ..
        } => {
            let configuration = match installation.app {
                InstalledApp::Catalog(_) => {
                    state
                        .apps
                        .find_organization_configuration(organization_id, app_id, environment)
                        .await
                }
                InstalledApp::Workspace(_) => {
                    state
                        .apps
                        .find_workspace_configuration(organization_id, workspace_id, app_id)
                        .await
                }
            }
            .map_err(|_| unavailable())?
            .filter(|config| config.version == installation.configuration_version)
            .ok_or_else(reconnect)?;
            let client_id = configuration.oauth_client_id.ok_or_else(reconnect)?;
            let stored = match installation.app {
                InstalledApp::Catalog(_) => {
                    state
                        .apps
                        .find_organization_client_secret(organization_id, app_id, environment)
                        .await
                }
                InstalledApp::Workspace(_) => {
                    state
                        .apps
                        .find_workspace_client_secret(organization_id, workspace_id, app_id)
                        .await
                }
            }
            .map_err(|_| unavailable())?
            .filter(|stored| {
                configuration
                    .client_secret
                    .as_ref()
                    .is_some_and(|secret| secret.version == stored.version)
            })
            .ok_or_else(reconnect)?;
            let context = EncryptionContext::oauth_client_secret(
                organization_id,
                own_workspace,
                environment,
                app_id,
                stored.version,
            );
            let opened = keys::open(state.keys.as_ref(), &context, &stored.sealed)
                .await
                .map_err(|_| unavailable())?;
            let secret = Zeroizing::new(
                std::str::from_utf8(opened.expose())
                    .map_err(|_| unavailable())?
                    .to_owned(),
            );
            Ok((token_url.clone(), client_id, Some(secret)))
        }
        _ => Err(reconnect()),
    }
}
