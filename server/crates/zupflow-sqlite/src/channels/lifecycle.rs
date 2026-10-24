//! A generated app's life at its provider, beside the rows: installing it
//! through the Connect session that asked, reading a connection's token to
//! revoke it, queueing a deleted app's removal at the provider (and again
//! when its team's tokens are pasted) until it's recorded released, queueing
//! a manifest sync with the agent change that needs it, and keeping a
//! linked team's configuration token rotated.
//!
//! The same rules as the rest of the store hold here: every statement after
//! a sweep's first read repeats the organization and the workspace, and the
//! one read that starts without them is named `_cross_tenant`.

use sqlx::{Row, Sqlite, Transaction};
use uuid::Uuid;
use zupflow_core::channels::TOKEN_LIFETIME_MS;
use zupflow_core::channels::provision::{RELEASE_CHANNEL_APP, SYNC_CHANNEL_APP};
use zupflow_core::channels::store::{ChannelError, NewInstall, RotationSweep, SealedVersion};
use zupflow_core::channels::{AgentChannel, ChannelConnection, ConnectTarget, ManagerStatus};
use zupflow_core::connect::{ConnectSession, ConnectSessionStatus};
use zupflow_core::ids::{
    AgentId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ConnectSessionId, OrganizationId, WorkspaceId,
};
use zupflow_core::jobs::JobOptions;
use zupflow_core::runs::QueuedJob;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::webhooks::WebhookEventType;

use super::{SqliteChannelStore, announce, failed, find_manager_in, install_app_in, read, sealed_from};
use crate::connect::session_columns;

/// How many times a release is tried before it's left, logged.
const RELEASE_ATTEMPTS: u32 = 8;

/// How many times a sync is tried before it's left, logged.
const SYNC_ATTEMPTS: u32 = 8;

/// Queues a job of `kind` for the workspace, at once.
async fn queue_job(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    kind: &str,
    payload: serde_json::Value,
    max_attempts: u32,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let kind = Name::new(kind).map_err(|_| ChannelError::Unavailable)?;
    crate::job_queue::insert_job_at(
        &mut **transaction,
        &Scope::Workspace(organization_id, workspace_id),
        &kind,
        payload.to_string().into_bytes(),
        JobOptions {
            delay: std::time::Duration::ZERO,
            max_attempts,
        },
        now.millis(),
    )
    .await
    .map_err(|error| {
        tracing::error!(event = "channels.job_not_queued", %error);
        ChannelError::Unavailable
    })?;
    Ok(())
}

/// Queues the job that removes a deleted app at its provider, in the
/// transaction that deletes it: its payload names the app and the
/// connection whose token is revoked first, never a secret.
pub(super) async fn queue_release(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    connection_id: Option<ChannelConnectionId>,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let mut payload = serde_json::json!({ "app_id": app_id.to_string() });
    if let Some(connection_id) = connection_id {
        payload["connection_id"] = serde_json::json!(connection_id.to_string());
    }
    queue_job(
        transaction,
        organization_id,
        workspace_id,
        RELEASE_CHANNEL_APP,
        payload,
        RELEASE_ATTEMPTS,
        now,
    )
    .await
}

/// A deleted app that had nothing at its provider to remove is released as
/// it's deleted.
pub(super) async fn released_at_once(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    now: Timestamp,
) -> Result<(), ChannelError> {
    sqlx::query(
        "UPDATE channel_apps SET released_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND released_at IS NULL",
    )
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(app_id.uuid())
    .execute(&mut **transaction)
    .await
    .map_err(failed("remove_app"))?;
    Ok(())
}

/// Queues the release of each of the manager's deleted apps not yet
/// released: pasted tokens may be what their removal was waiting for.
pub(super) async fn requeue_releases(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    now: Timestamp,
) -> Result<u32, ChannelError> {
    let rows = sqlx::query(
        "SELECT a.id, (SELECT c.id FROM channel_connections c WHERE c.organization_id = a.organization_id \
         AND c.workspace_id = a.workspace_id AND c.app_id = a.id ORDER BY c.created_at DESC, c.id DESC LIMIT 1) \
         AS connection_id FROM channel_apps a \
         WHERE a.organization_id = ? AND a.workspace_id = ? AND a.manager_id = ? AND a.status = 'deleted' \
         AND a.released_at IS NULL AND a.external_app_id IS NOT NULL",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(manager_id.uuid())
    .fetch_all(&mut **transaction)
    .await
    .map_err(failed("requeue_releases"))?;
    let mut queued = 0;
    for row in &rows {
        let app_id = ChannelAppId::from_uuid(row.try_get::<Uuid, _>("id").map_err(read)?);
        let connection_id = row
            .try_get::<Option<Uuid>, _>("connection_id")
            .map_err(read)?
            .map(ChannelConnectionId::from_uuid);
        queue_release(transaction, organization_id, workspace_id, app_id, connection_id, now).await?;
        queued += 1;
    }
    Ok(queued)
}

/// Queues a manifest sync for each of the agent's made apps, in the
/// transaction that changes the agent's name or description, so the change
/// can't commit without it.
pub(crate) async fn queue_agent_app_syncs(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    now: Timestamp,
) -> Result<(), ChannelError> {
    let rows = sqlx::query(
        "SELECT id FROM channel_apps WHERE organization_id = ? AND workspace_id = ? AND agent_id = ? \
         AND status IN ('awaiting_install', 'installed') AND external_app_id IS NOT NULL",
    )
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(agent_id.uuid())
    .fetch_all(&mut **transaction)
    .await
    .map_err(failed("queue_agent_app_syncs"))?;
    for row in &rows {
        let app_id = ChannelAppId::from_uuid(row.try_get::<Uuid, _>("id").map_err(read)?);
        queue_job(
            transaction,
            organization_id,
            workspace_id,
            SYNC_CHANNEL_APP,
            serde_json::json!({ "app_id": app_id.to_string() }),
            SYNC_ATTEMPTS,
            now,
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn record_app_released(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    now: Timestamp,
) -> Result<bool, ChannelError> {
    let released = sqlx::query(
        "UPDATE channel_apps SET released_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'deleted' AND released_at IS NULL",
    )
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(app_id.uuid())
    .execute(store.database.pool())
    .await
    .map_err(failed("record_app_released"))?;
    Ok(released.rows_affected() == 1)
}

pub(super) async fn record_app_synced(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    app_id: ChannelAppId,
    digest: &str,
) -> Result<bool, ChannelError> {
    if digest.len() != 64 || !digest.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(ChannelError::Unavailable);
    }
    let synced = sqlx::query(
        "UPDATE channel_apps SET manifest_digest = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status IN ('awaiting_install', 'installed')",
    )
    .bind(digest)
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(app_id.uuid())
    .execute(store.database.pool())
    .await
    .map_err(failed("record_app_synced"))?;
    Ok(synced.rows_affected() == 1)
}

const FIND_SESSION: &str = concat!(
    "SELECT ",
    session_columns!(),
    " FROM connect_sessions WHERE organization_id = ? AND workspace_id = ? AND id = ?"
);

async fn find_session_in(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session_id: ConnectSessionId,
) -> Result<Option<ConnectSession>, ChannelError> {
    let row = sqlx::query(FIND_SESSION)
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(session_id.uuid())
        .fetch_optional(&mut **transaction)
        .await
        .map_err(failed("install_app_through_session"))?;
    row.as_ref()
        .map(crate::connect::session_from)
        .transpose()
        .map_err(|_| ChannelError::Unavailable)
}

pub(super) async fn install_app_through_session(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    session_id: ConnectSessionId,
    app_id: ChannelAppId,
    install: &NewInstall,
    now: Timestamp,
) -> Result<(ConnectSession, ChannelConnection, AgentChannel), ChannelError> {
    let mut transaction = store.immediate("install_app_through_session").await?;
    let session = find_session_in(&mut transaction, organization_id, workspace_id, session_id)
        .await?
        .ok_or(ChannelError::SessionNotFound)?;
    match session.target {
        Some(ConnectTarget::ChannelApp { app, .. }) if app == app_id => {}
        _ => return Err(ChannelError::SessionNotFound),
    }
    if session.status_at(now) != ConnectSessionStatus::Pending {
        return Err(ChannelError::SessionNotPending);
    }
    let (connection, channel) =
        install_app_in(&mut transaction, organization_id, workspace_id, app_id, install, now).await?;
    let completed = sqlx::query(
        "UPDATE connect_sessions SET status = 'completed', completed_at = ?, target_connection_id = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND status = 'pending' \
         AND target_kind = 'channel_app' AND target_app_id = ?",
    )
    .bind(now.millis())
    .bind(connection.id.uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(session_id.uuid())
    .bind(app_id.uuid())
    .execute(&mut *transaction)
    .await
    .map_err(failed("install_app_through_session"))?;
    if completed.rows_affected() != 1 {
        return Err(ChannelError::SessionNotPending);
    }
    let session = find_session_in(&mut transaction, organization_id, workspace_id, session_id)
        .await?
        .ok_or_else(|| read("a session gone in its transaction"))?;
    crate::connect::record_outcome(&mut transaction, organization_id, workspace_id, &session, now)
        .await
        .map_err(|_| ChannelError::Unavailable)?;
    transaction
        .commit()
        .await
        .map_err(failed("install_app_through_session"))?;
    Ok((session, connection, channel))
}

pub(super) async fn connection_credential(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    connection_id: ChannelConnectionId,
) -> Result<Option<SealedVersion>, ChannelError> {
    let row = sqlx::query(
        "SELECT k.version, k.key_id, k.wrapped_key, k.nonce, k.ciphertext \
         FROM channel_connection_credentials k JOIN channel_connections c \
         ON c.organization_id = k.organization_id AND c.workspace_id = k.workspace_id AND c.id = k.connection_id \
         AND c.credential_version = k.version \
         WHERE k.organization_id = ? AND c.organization_id = ? AND k.workspace_id = ? AND c.workspace_id = ? \
         AND k.connection_id = ?",
    )
    .bind(organization_id.uuid())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(workspace_id.uuid())
    .bind(connection_id.uuid())
    .fetch_optional(store.database.pool())
    .await
    .map_err(failed("connection_credential"))?;
    row.as_ref().map(sealed_from).transpose()
}

/// Marks a manager's tokens of `expected_version` expired, announced.
/// Whether it changed.
async fn expire_in(
    transaction: &mut Transaction<'static, Sqlite>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    expected_version: u32,
    now: Timestamp,
) -> Result<bool, ChannelError> {
    let expired = sqlx::query(
        "UPDATE channel_app_managers SET status = 'token_expired', updated_at = ? \
         WHERE organization_id = ? AND workspace_id = ? AND id = ? AND credential_version = ? AND status = 'active'",
    )
    .bind(now.millis())
    .bind(organization_id.uuid())
    .bind(workspace_id.uuid())
    .bind(manager_id.uuid())
    .bind(i64::from(expected_version))
    .execute(&mut **transaction)
    .await
    .map_err(failed("expire_manager_tokens"))?;
    if expired.rows_affected() != 1 {
        return Ok(false);
    }
    announce(
        transaction,
        organization_id,
        workspace_id,
        WebhookEventType::ChannelAppManagerStatusChanged,
        serde_json::json!({ "object": "channel_app_manager", "id": manager_id, "status": ManagerStatus::TokenExpired }),
        now,
    )
    .await?;
    Ok(true)
}

pub(super) async fn expire_manager_tokens(
    store: &SqliteChannelStore,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    manager_id: ChannelAppManagerId,
    expected_version: u32,
    now: Timestamp,
) -> Result<bool, ChannelError> {
    let mut transaction = store.immediate("expire_manager_tokens").await?;
    // Read in its scope first: a manager of another workspace is no manager here.
    if find_manager_in(&mut transaction, organization_id, workspace_id, manager_id)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    let changed = expire_in(
        &mut transaction,
        organization_id,
        workspace_id,
        manager_id,
        expected_version,
        now,
    )
    .await?;
    transaction.commit().await.map_err(failed("expire_manager_tokens"))?;
    Ok(changed)
}

/// One manager a sweep found: its scope, ID and credential version.
fn found(
    row: &sqlx::sqlite::SqliteRow,
) -> Result<(OrganizationId, WorkspaceId, ChannelAppManagerId, u32), ChannelError> {
    let version: i64 = row.try_get("credential_version").map_err(read)?;
    Ok((
        OrganizationId::from_uuid(row.try_get::<Uuid, _>("organization_id").map_err(read)?),
        WorkspaceId::from_uuid(row.try_get::<Uuid, _>("workspace_id").map_err(read)?),
        ChannelAppManagerId::from_uuid(row.try_get::<Uuid, _>("id").map_err(read)?),
        u32::try_from(version).map_err(read)?,
    ))
}

pub(super) async fn queue_due_rotations_cross_tenant(
    store: &SqliteChannelStore,
    rotate: &(dyn Fn(ChannelAppManagerId, u32) -> QueuedJob + Send + Sync),
    due_by: Timestamp,
    limit: u32,
    now: Timestamp,
) -> Result<RotationSweep, ChannelError> {
    let limit = i64::from(limit.clamp(1, 500));
    let mut transaction = store.immediate("queue_due_rotations").await?;
    let mut sweep = RotationSweep::default();
    // Cross-tenant by design: a sweep over every workspace's linked teams,
    // as a worker's claim is. Each is then settled in its own scope.
    let lapsed = sqlx::query(
        "SELECT organization_id, workspace_id, id, credential_version FROM channel_app_managers \
         WHERE status = 'active' AND token_expires_at IS NOT NULL AND token_expires_at <= ? LIMIT ?",
    )
    .bind(now.millis())
    .bind(limit)
    .fetch_all(&mut *transaction)
    .await
    .map_err(failed("queue_due_rotations"))?;
    for row in &lapsed {
        let (organization_id, workspace_id, manager_id, version) = found(row)?;
        if expire_in(
            &mut transaction,
            organization_id,
            workspace_id,
            manager_id,
            version,
            now,
        )
        .await?
        {
            tracing::warn!(event = "channels.manager_token_lapsed", %organization_id, %workspace_id, %manager_id);
            sweep.expired += 1;
        }
    }
    let due = sqlx::query(
        "SELECT organization_id, workspace_id, id, credential_version FROM channel_app_managers \
         WHERE status = 'active' AND (token_expires_at IS NULL OR token_expires_at <= ?) \
         AND (rotation_version IS NULL OR rotation_version <> credential_version) LIMIT ?",
    )
    .bind(due_by.millis())
    .bind(limit)
    .fetch_all(&mut *transaction)
    .await
    .map_err(failed("queue_due_rotations"))?;
    for row in &due {
        let (organization_id, workspace_id, manager_id, version) = found(row)?;
        // Once per version: the claim and the job commit together.
        // Tokens whose expiry isn't known (pasted ones) expire within their
        // lifetime from now at the latest: if this rotation never happens,
        // the lapse above expires them then rather than never.
        let claimed = sqlx::query(
            "UPDATE channel_app_managers SET rotation_version = credential_version, \
             token_expires_at = coalesce(token_expires_at, ?) \
             WHERE organization_id = ? AND workspace_id = ? AND id = ? AND credential_version = ? \
             AND status = 'active' AND (rotation_version IS NULL OR rotation_version <> credential_version)",
        )
        .bind(now.millis().saturating_add(TOKEN_LIFETIME_MS))
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(manager_id.uuid())
        .bind(i64::from(version))
        .execute(&mut *transaction)
        .await
        .map_err(failed("queue_due_rotations"))?;
        if claimed.rows_affected() != 1 {
            continue;
        }
        let job = rotate(manager_id, version);
        crate::job_queue::insert_job_at(
            &mut *transaction,
            &Scope::Workspace(organization_id, workspace_id),
            &job.kind,
            job.payload,
            job.options,
            now.millis(),
        )
        .await
        .map_err(|error| {
            tracing::error!(event = "channels.job_not_queued", %error);
            ChannelError::Unavailable
        })?;
        sweep.queued += 1;
    }
    transaction.commit().await.map_err(failed("queue_due_rotations"))?;
    Ok(sweep)
}
