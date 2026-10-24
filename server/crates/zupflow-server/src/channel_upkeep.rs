//! Keeping generated apps in step with their provider, as jobs on the
//! channel worker: a linked team's configuration token rotated before it
//! expires, an app's manifest sent again when its agent's name or
//! description changes (queued by the store with the change), and a
//! deleted app removed at its provider: its bot token revoked first, which
//! needs nothing of its team, then the app deleted with the team's tokens,
//! waiting (without using attempts) while those aren't active. Pasted
//! tokens queue the removals again.
//!
//! **Rotation.** A sweep queues one `rotate_channel_manager_tokens` job per
//! manager and credential version, an hour before its tokens expire (at
//! once when the expiry isn't known yet, as for pasted ones); the store
//! claims the version with the job, so however many sweeps run, a version's
//! refresh token, which the provider takes once, is spent by one job. The
//! new pair is sealed as the next version and stored, expecting the version
//! read, before the job finishes. A pair the provider refuses as invalid,
//! or that can't be rotated or stored in time, leaves the manager `token_expired`
//! (announced), so someone pastes new ones. Payloads name the manager and
//! version, never a token.
//!
//! **Retention.** A sweep queues one `sweep_channel_retention` job per
//! workspace with channel data past its organization's retention (one at a
//! time per workspace); the job deletes it in the workspace's own scope
//! ([`ChannelStore::sweep_retention`]). Its payload names nothing: the
//! scope is all it needs.
//!
//! Nothing here sleeps on a rate limit: the job is postponed for as long as
//! the provider asked, without using an attempt.
//!
//! [`ChannelStore::sweep_retention`]: zupflow_core::channels::store::ChannelStore::sweep_retention

use std::time::Duration;

use serde_json::{Value, json};
use zupflow_core::audit::{AuditAction, AuditActor, AuditEntry, AuditObject, AuditPlace, AuditTarget};
use zupflow_core::channels::provision::{ProvisionError, RELEASE_CHANNEL_APP, RotatedTokens, SYNC_CHANNEL_APP};
use zupflow_core::channels::store::{ChannelError, TokenReplacement};
use zupflow_core::channels::{AppStatus, ChannelAppManager, ManagerStatus, RETENTION_JOB, TOKEN_LIFETIME_MS};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ChannelAppId, ChannelAppManagerId, ChannelConnectionId, OrganizationId, WorkspaceId};
use zupflow_core::jobs::{Job, JobOptions};
use zupflow_core::keys::{self, EncryptionContext};
use zupflow_core::runs::QueuedJob;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;

use crate::auth::AppState;
use crate::channels::{OpenedConnection, SealingScope, manifest_for, open_tokens, seal_tokens};
use zeroize::Zeroizing;

/// Rotates one manager's tokens of one version.
pub(crate) const ROTATE_CHANNEL_MANAGER: &str = "rotate_channel_manager_tokens";

/// How far ahead of their expiry tokens are rotated.
const ROTATE_AHEAD_MS: i64 = 60 * 60 * 1000;
/// How many tries a rotation gets within its hour.
const ROTATION_ATTEMPTS: u32 = 10;
/// How long a removal waits for its team's tokens to be active again
/// before it looks again; pasted tokens queue it at once.
const RELEASE_WAIT: Duration = Duration::from_secs(6 * 60 * 60);
/// How many managers one sweep settles.
const SWEEP_LIMIT: u32 = 100;
/// How often a spent refresh token's new pair is tried to the store before
/// it's given up (the provider won't answer it again).
const STORE_TRIES: u32 = 3;

/// What came of one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Next {
    Done,
    /// Later, after the queue's backoff, using an attempt.
    Retry(&'static str),
    /// After the provider's wait, using no attempt.
    Wait(Duration),
}

fn wait(retry_after_secs: u32) -> Next {
    Next::Wait(Duration::from_secs(u64::from(retry_after_secs.max(1))))
}

/// The kinds this module handles.
pub(crate) fn kinds() -> Vec<Name> {
    [
        ROTATE_CHANNEL_MANAGER,
        SYNC_CHANNEL_APP,
        RELEASE_CHANNEL_APP,
        RETENTION_JOB,
    ]
    .into_iter()
    .filter_map(|kind| Name::new(kind).ok())
    .collect()
}

fn rotation_job(manager_id: ChannelAppManagerId, version: u32) -> QueuedJob {
    QueuedJob {
        // A fixed name, valid by construction.
        kind: Name::new(ROTATE_CHANNEL_MANAGER).expect("a valid job kind"),
        payload: json!({ "manager_id": manager_id.to_string(), "version": version })
            .to_string()
            .into_bytes(),
        options: JobOptions {
            delay: Duration::ZERO,
            max_attempts: ROTATION_ATTEMPTS,
        },
    }
}

/// Queues the rotations that are due, and expires tokens that lapsed
/// unrotated. Cross-tenant, like a claim: the store settles each manager in
/// its own scope.
pub(crate) async fn sweep(state: &AppState) {
    let now = Timestamp::now();
    let due_by = Timestamp::from_millis(now.millis().saturating_add(ROTATE_AHEAD_MS));
    match state
        .channels
        .queue_due_rotations_cross_tenant(&rotation_job, due_by, SWEEP_LIMIT, now)
        .await
    {
        Ok(sweep) if sweep.queued > 0 || sweep.expired > 0 => {
            tracing::info!(
                event = "channels.rotations_swept",
                queued = sweep.queued,
                expired = sweep.expired
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(event = "channels.rotation_sweep_failed", %error),
    }
}

/// Queues a retention sweep for each workspace with channel data past its
/// organization's retention. Cross-tenant, like a claim: each job is the
/// workspace's own.
pub(crate) async fn retention_due(state: &AppState) -> usize {
    let due = match state
        .channels
        .retention_due_cross_tenant(state.clock.now(), SWEEP_LIMIT)
        .await
    {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(event = "channels.retention_sweep_failed", %error);
            return 0;
        }
    };
    let Ok(kind) = Name::new(RETENTION_JOB) else {
        return 0;
    };
    let mut queued = 0;
    for (organization_id, workspace_id) in due {
        let options = JobOptions {
            delay: Duration::ZERO,
            max_attempts: 5,
        };
        let scope = Scope::Workspace(organization_id, workspace_id);
        match state.jobs.enqueue(&scope, &kind, b"{}".to_vec(), options).await {
            Ok(_) => queued += 1,
            Err(error) => {
                tracing::warn!(event = "channels.retention_not_queued", %organization_id, %workspace_id, %error);
            }
        }
    }
    queued
}

/// Deletes one workspace's channel data past retention.
async fn sweep_retention(state: &AppState, organization_id: OrganizationId, workspace_id: WorkspaceId) -> Next {
    match state
        .channels
        .sweep_retention(organization_id, workspace_id, state.clock.now())
        .await
    {
        Ok(swept) => {
            tracing::info!(
                event = "channels.retention_swept",
                %organization_id,
                %workspace_id,
                messages = swept.messages,
                inbound_events = swept.inbound_events,
                runs = swept.runs
            );
            Next::Done
        }
        Err(_) => Next::Retry("store_unavailable"),
    }
}

fn field<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload.get(name)?.as_str()
}

/// Handles one of [`kinds`]' jobs, then finishes, retries or postpones it.
pub(crate) async fn handle(state: &AppState, job: Job) {
    let Job {
        id,
        scope,
        kind,
        payload,
        attempt,
        lease,
        ..
    } = job;
    let payload: Value = serde_json::from_slice(&payload).unwrap_or(Value::Null);
    let last = |max: u32| attempt >= max;
    let next = match (scope, kind.as_str()) {
        (Scope::Workspace(organization_id, workspace_id), ROTATE_CHANNEL_MANAGER) => {
            let manager = field(&payload, "manager_id").and_then(|id| ChannelAppManagerId::parse(id).ok());
            let version = payload
                .get("version")
                .and_then(Value::as_u64)
                .and_then(|version| u32::try_from(version).ok());
            match (manager, version) {
                (Some(manager_id), Some(version)) => {
                    rotate(
                        state,
                        organization_id,
                        workspace_id,
                        manager_id,
                        version,
                        last(ROTATION_ATTEMPTS),
                    )
                    .await
                }
                _ => Next::Done,
            }
        }
        (Scope::Workspace(organization_id, workspace_id), SYNC_CHANNEL_APP | RELEASE_CHANNEL_APP) => {
            match field(&payload, "app_id").and_then(|id| ChannelAppId::parse(id).ok()) {
                Some(app_id) if kind.as_str() == SYNC_CHANNEL_APP => {
                    sync(state, organization_id, workspace_id, app_id).await
                }
                Some(app_id) => {
                    let connection_id =
                        field(&payload, "connection_id").and_then(|id| ChannelConnectionId::parse(id).ok());
                    release(state, organization_id, workspace_id, app_id, connection_id).await
                }
                None => Next::Done,
            }
        }
        (Scope::Workspace(organization_id, workspace_id), RETENTION_JOB) => {
            sweep_retention(state, organization_id, workspace_id).await
        }
        _ => {
            tracing::error!(event = "channels.malformed_job", kind = kind.as_str());
            if let Ok(reason) = Name::new("malformed") {
                let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
            }
            return;
        }
    };
    let finished = match next {
        Next::Done => state.jobs.complete(&scope, id, &lease).await,
        Next::Retry(why) => match Name::new(why) {
            Ok(reason) => {
                let after = Duration::from_secs(30 * u64::from(attempt.max(1)));
                state.jobs.retry(&scope, id, &lease, after, &reason).await.map(|_| ())
            }
            Err(_) => state.jobs.complete(&scope, id, &lease).await,
        },
        Next::Wait(after) => state.jobs.postpone(&scope, id, &lease, after).await,
    };
    if let Err(error) = finished {
        tracing::warn!(event = "channels.finish_failed", kind = kind.as_str(), %error);
    }
}

async fn environment_of(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
) -> Result<Option<Environment>, ()> {
    match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(found) => Ok(found.map(|workspace| workspace.environment)),
        Err(_) => Err(()),
    }
}

/// The manager's tokens can't be used or rotated: expired, announced.
async fn expire(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    version: u32,
    why: &'static str,
) -> Next {
    tracing::warn!(event = "channels.manager_token_expired", %organization_id, %workspace_id, %manager_id, why);
    match state
        .channels
        .expire_manager_tokens(organization_id, workspace_id, manager_id, version, Timestamp::now())
        .await
    {
        Ok(_) => Next::Done,
        Err(_) => Next::Retry("store_unavailable"),
    }
}

async fn rotate(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    version: u32,
    last_attempt: bool,
) -> Next {
    let later = |why: &'static str| async move {
        if last_attempt {
            expire(state, organization_id, workspace_id, manager_id, version, why).await
        } else {
            Next::Retry(why)
        }
    };
    let environment = match environment_of(state, organization_id, workspace_id).await {
        Ok(Some(environment)) => environment,
        Ok(None) => return Next::Done,
        Err(()) => return later("store_unavailable").await,
    };
    let (manager, credential) = match tokio::join!(
        state.channels.find_manager(organization_id, workspace_id, manager_id),
        state
            .channels
            .manager_credential(organization_id, workspace_id, manager_id),
    ) {
        (Ok(manager), Ok(credential)) => (manager, credential),
        _ => return later("store_unavailable").await,
    };
    // Replaced, expired or unlinked since it was queued: nothing to rotate.
    let current = matches!(
        (&manager, &credential),
        (Some(manager), Some(credential))
            if manager.status == ManagerStatus::Active && manager.credential_version == version
                && credential.version == version
    );
    let (Some(credential), true) = (credential, current) else {
        return Next::Done;
    };
    let Some(tokens) = open_tokens(
        state,
        organization_id,
        workspace_id,
        environment,
        manager_id,
        &credential,
    )
    .await
    else {
        return later("keys_unavailable").await;
    };
    let rotated = match state
        .channel_provisioner
        .rotate_tokens(state.outbound.as_ref(), &tokens)
        .await
    {
        Ok(rotated) => rotated,
        // Only a refresh token Slack says is invalid ends the pair; any
        // other refusal may be passing, and is tried again.
        Err(ProvisionError::Refused(code)) => {
            tracing::warn!(event = "channels.rotation_refused", %organization_id, %workspace_id, %manager_id, code);
            return later("provider_refused").await;
        }
        Err(ProvisionError::TokensRefused) => {
            return expire(
                state,
                organization_id,
                workspace_id,
                manager_id,
                version,
                "tokens_refused",
            )
            .await;
        }
        Err(ProvisionError::RateLimited { retry_after_secs }) => return wait(retry_after_secs),
        // Perhaps spent all the same: the next attempt finds out.
        Err(ProvisionError::Unavailable) => return later("provider_unavailable").await,
        Err(ProvisionError::NotConfigured | ProvisionError::AppNotFound) => {
            tracing::warn!(event = "channels.rotation_unsupported", %organization_id, %workspace_id, %manager_id);
            return Next::Done;
        }
    };
    let scope = SealingScope {
        organization_id,
        workspace_id,
        environment,
    };
    // The old refresh token is spent: this pair is stored now or never.
    for _ in 0..STORE_TRIES {
        match store(state, scope, manager_id, version, &rotated).await {
            Ok(true) => {
                tracing::info!(event = "channels.manager_tokens_rotated", %organization_id, %workspace_id, %manager_id);
                return Next::Done;
            }
            // Replaced or removed meanwhile: what's stored now stands.
            Ok(false) => return Next::Done,
            Err(()) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    expire(
        state,
        organization_id,
        workspace_id,
        manager_id,
        version,
        "rotation_not_stored",
    )
    .await
}

/// Seals and stores a rotated pair as the next version. `Ok(false)` when
/// the tokens were replaced or the manager removed since.
async fn store(
    state: &AppState,
    scope: SealingScope,
    manager_id: ChannelAppManagerId,
    version: u32,
    rotated: &RotatedTokens,
) -> Result<bool, ()> {
    let next = version.checked_add(1).ok_or(())?;
    let sealed = seal_tokens(state, scope, manager_id, next, &rotated.tokens)
        .await
        .map_err(|_| ())?;
    let now = Timestamp::now();
    let expires_at = rotated
        .expires_at
        .unwrap_or_else(|| Timestamp::from_millis(now.millis().saturating_add(TOKEN_LIFETIME_MS)));
    let replaced = state
        .channels
        .replace_manager_tokens(
            scope.organization_id,
            scope.workspace_id,
            manager_id,
            None,
            &TokenReplacement {
                expected_version: version,
                credential: sealed,
                token_last4: crate::connect::last4(&rotated.tokens.configuration_token),
                token_expires_at: Some(expires_at),
            },
            now,
            // No editor: the store audits only a person's or key's change, so this is never written.
            &|manager: &ChannelAppManager| {
                AuditEntry::new(
                    AuditActor::System {
                        name: "Channels".to_owned(),
                    },
                    AuditAction::ChannelAppManagerUpdated,
                    AuditTarget::new(AuditObject::ChannelAppManager(manager.id), manager.provider.name()),
                    AuditPlace::Workspace(manager.workspace_id),
                )
            },
        )
        .await;
    match replaced {
        Ok(_) => Ok(true),
        Err(ChannelError::TokensChanged | ChannelError::ManagerNotFound) => Ok(false),
        Err(_) => Err(()),
    }
}

/// What's needed to reach the provider for an app: its manager's tokens,
/// opened. `None` when there's nothing to reach it with.
async fn manager_tokens(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
) -> Result<Option<zupflow_core::channels::provision::ManagerTokens>, &'static str> {
    let environment = match environment_of(state, organization_id, workspace_id).await {
        Ok(Some(environment)) => environment,
        Ok(None) => return Ok(None),
        Err(()) => return Err("store_unavailable"),
    };
    let credential = match state
        .channels
        .manager_credential(organization_id, workspace_id, manager_id)
        .await
    {
        Ok(Some(credential)) => credential,
        Ok(None) => return Ok(None),
        Err(_) => return Err("store_unavailable"),
    };
    open_tokens(
        state,
        organization_id,
        workspace_id,
        environment,
        manager_id,
        &credential,
    )
    .await
    .map(Some)
    .ok_or("keys_unavailable")
}

/// Sends the app's manifest again, built from its agent as it is now: also
/// the second step of making it, which subscribes its events.
pub(crate) async fn sync(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
) -> Next {
    let app = match state.channels.find_app(organization_id, workspace_id, app_id).await {
        Ok(Some(app)) => app,
        Ok(None) => return Next::Done,
        Err(_) => return Next::Retry("store_unavailable"),
    };
    let Some(external_app_id) = app.external_app_id.as_deref().filter(|_| app.status.is_installable()) else {
        return Next::Done;
    };
    let (agent, secrets) = match tokio::join!(
        state.agents.find_agent(organization_id, workspace_id, app.agent_id),
        state.channels.app_secrets(organization_id, workspace_id, app_id),
    ) {
        (Ok(Some(agent)), Ok(Some(secrets))) => (agent, secrets),
        (Ok(None), _) | (_, Ok(None)) => return Next::Done,
        _ => return Next::Retry("store_unavailable"),
    };
    let tokens = match manager_tokens(state, organization_id, workspace_id, app.manager_id).await {
        Ok(Some(tokens)) => tokens,
        Ok(None) => return Next::Done,
        Err(why) => return Next::Retry(why),
    };
    let events_url = format!("{}/channels/inbound/{}", state.public_url, secrets.inbound_ref.expose());
    let manifest = manifest_for(
        &agent,
        events_url,
        crate::app_configurations::redirect_uri(&state.public_url),
    );
    let digest = manifest.digest();
    // The provider has it already (a rename and back, a repeated job).
    if secrets.manifest_digest.as_deref() == Some(digest.as_str()) {
        return Next::Done;
    }
    match state
        .channel_provisioner
        .update(state.outbound.as_ref(), &tokens, external_app_id, &manifest)
        .await
    {
        Ok(()) => {
            tracing::info!(event = "channels.app_synced", %organization_id, %workspace_id, %app_id);
            if let Err(error) = state
                .channels
                .record_app_synced(organization_id, workspace_id, app_id, &digest)
                .await
            {
                // Only costs a repeated update next time.
                tracing::warn!(event = "channels.sync_not_recorded", %organization_id, %workspace_id, %app_id, %error);
            }
            Next::Done
        }
        Err(ProvisionError::RateLimited { retry_after_secs }) => wait(retry_after_secs),
        // Refused tokens may be a rotation's moment: tried again after it.
        Err(ProvisionError::Unavailable) => Next::Retry("provider_unavailable"),
        Err(ProvisionError::TokensRefused) => Next::Retry("tokens_refused"),
        Err(error) => {
            tracing::warn!(event = "channels.app_not_synced", %organization_id, %workspace_id, %app_id, %error);
            Next::Done
        }
    }
}

/// Revokes a deleted app's last connection's bot token: needs only the
/// token, so it's done whatever its team's state. A connection or token
/// that's gone has nothing left to revoke.
async fn revoke_connection(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    connection_id: ChannelConnectionId,
) -> Result<(), Next> {
    let sealed = match state
        .channels
        .connection_credential(organization_id, workspace_id, connection_id)
        .await
    {
        Ok(Some(sealed)) => sealed,
        Ok(None) => return Ok(()),
        Err(_) => return Err(Next::Retry("store_unavailable")),
    };
    let context = EncryptionContext::channel_connection_credential(
        organization_id,
        workspace_id,
        environment,
        connection_id,
        sealed.version,
    );
    let Ok(opened) = keys::open(state.keys.as_ref(), &context, &sealed.sealed).await else {
        return Err(Next::Retry("keys_unavailable"));
    };
    let Ok(mut connection) = serde_json::from_slice::<OpenedConnection>(opened.expose()) else {
        tracing::error!(event = "channels.token_unopenable", %organization_id, %workspace_id, %connection_id);
        return Ok(());
    };
    let token = Zeroizing::new(std::mem::take(&mut connection.token));
    match state
        .channel_provisioner
        .revoke_install(state.outbound.as_ref(), &token)
        .await
    {
        Ok(()) => Ok(()),
        Err(ProvisionError::RateLimited { retry_after_secs }) => Err(wait(retry_after_secs)),
        Err(ProvisionError::Unavailable) => Err(Next::Retry("provider_unavailable")),
        Err(error) => {
            // Refused for another reason: the app's deletion revokes it too.
            tracing::warn!(event = "channels.token_not_revoked", %organization_id, %workspace_id, %connection_id, %error);
            Ok(())
        }
    }
}

/// Removes a deleted app at its provider: its bot token revoked, then the
/// app deleted with its team's tokens, then recorded released.
async fn release(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    connection_id: Option<ChannelConnectionId>,
) -> Next {
    let creation = match state.channels.app_creation(organization_id, workspace_id, app_id).await {
        Ok(Some(creation)) => creation,
        Ok(None) => return Next::Done,
        Err(_) => return Next::Retry("store_unavailable"),
    };
    let app = creation.app;
    let Some(external_app_id) = app
        .external_app_id
        .as_deref()
        .filter(|_| app.status == AppStatus::Deleted)
    else {
        return Next::Done;
    };
    let environment = match environment_of(state, organization_id, workspace_id).await {
        Ok(Some(environment)) => environment,
        Ok(None) => return Next::Done,
        Err(()) => return Next::Retry("store_unavailable"),
    };
    if let Some(connection_id) = connection_id
        && let Err(next) = revoke_connection(state, organization_id, workspace_id, environment, connection_id).await
    {
        return next;
    }
    let manager = match state
        .channels
        .find_manager(organization_id, workspace_id, app.manager_id)
        .await
    {
        Ok(Some(manager)) => manager,
        Ok(None) => {
            // Its team can't be unlinked while this is pending; gone all the same, nothing reaches the provider.
            tracing::warn!(event = "channels.release_unreachable", %organization_id, %workspace_id, %app_id);
            return released(state, organization_id, workspace_id, app_id).await;
        }
        Err(_) => return Next::Retry("store_unavailable"),
    };
    // Expired tokens can't remove it: it waits for new ones, which queue it again.
    if manager.status != ManagerStatus::Active {
        return Next::Wait(RELEASE_WAIT);
    }
    let tokens = match manager_tokens(state, organization_id, workspace_id, app.manager_id).await {
        Ok(Some(tokens)) => tokens,
        Ok(None) => return Next::Wait(RELEASE_WAIT),
        Err(why) => return Next::Retry(why),
    };
    match state
        .channel_provisioner
        .delete(state.outbound.as_ref(), &tokens, external_app_id)
        .await
    {
        Ok(()) | Err(ProvisionError::AppNotFound) => {
            tracing::info!(event = "channels.app_released", %organization_id, %workspace_id, %app_id);
            released(state, organization_id, workspace_id, app_id).await
        }
        Err(ProvisionError::RateLimited { retry_after_secs }) => wait(retry_after_secs),
        Err(ProvisionError::Unavailable) => Next::Retry("provider_unavailable"),
        Err(ProvisionError::TokensRefused) => Next::Retry("tokens_refused"),
        Err(error) => {
            // Kept unreleased: its team stays linked, and pasting its tokens tries again.
            tracing::warn!(event = "channels.app_not_released", %organization_id, %workspace_id, %app_id, %error);
            Next::Done
        }
    }
}

async fn released(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
) -> Next {
    match state
        .channels
        .record_app_released(organization_id, workspace_id, app_id, Timestamp::now())
        .await
    {
        Ok(_) => Next::Done,
        Err(_) => Next::Retry("store_unavailable"),
    }
}
