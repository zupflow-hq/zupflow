//! Commits a competing refresh at a chosen read boundary.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::*;
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Read {
    Installation,
    Credential,
}

pub struct RotateOnRead {
    pub inner: Arc<dyn InstallationStore>,
    pub scope: (OrganizationId, WorkspaceId, InstallationId),
    pub credential: NewCredential,
    pub read: Read,
    pub at: usize,
    pub reads: AtomicUsize,
}

impl RotateOnRead {
    async fn rotate(
        &self,
        read: Read,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) {
        if self.read != read
            || self.scope != (organization_id, workspace_id, installation_id)
            || self.reads.fetch_add(1, Ordering::SeqCst) + 1 != self.at
        {
            return;
        }
        let now = Timestamp::now();
        assert_eq!(
            self.inner
                .claim_refresh(
                    organization_id,
                    workspace_id,
                    installation_id,
                    self.credential.version - 1,
                    now
                )
                .await
                .unwrap(),
            RefreshClaim::Claimed
        );
        assert!(
            self.inner
                .rotate_credential(
                    organization_id,
                    workspace_id,
                    installation_id,
                    &self.credential,
                    Some(Timestamp::from_millis(now.millis() + 3_600_000)),
                    now
                )
                .await
                .unwrap()
        );
    }
}

#[async_trait]
impl InstallationStore for RotateOnRead {
    async fn list_installations(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
    ) -> Result<Vec<Installation>, InstallationError> {
        self.inner.list_installations(organization_id, workspace_id).await
    }

    async fn find_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) -> Result<Option<Installation>, InstallationError> {
        let snapshot = self
            .inner
            .find_installation(organization_id, workspace_id, installation_id)
            .await?;
        self.rotate(Read::Installation, organization_id, workspace_id, installation_id)
            .await;
        Ok(snapshot)
    }

    async fn create_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        installation: &NewInstallation,
    ) -> Result<Installation, InstallationError> {
        self.inner
            .create_installation(organization_id, workspace_id, installation_id, installation)
            .await
    }

    async fn rename_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        label: &str,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError> {
        self.inner
            .rename_installation(organization_id, workspace_id, installation_id, label, audit)
            .await
    }

    async fn disconnect_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<Installation, InstallationError> {
        self.inner
            .disconnect_installation(organization_id, workspace_id, installation_id, audit)
            .await
    }

    async fn delete_installation(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        audit: &Audit<'_, Installation>,
    ) -> Result<(), InstallationError> {
        self.inner
            .delete_installation(organization_id, workspace_id, installation_id, audit)
            .await
    }

    async fn find_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
    ) -> Result<Option<NewCredential>, InstallationError> {
        self.rotate(Read::Credential, organization_id, workspace_id, installation_id)
            .await;
        self.inner
            .find_credential(organization_id, workspace_id, installation_id)
            .await
    }

    async fn claim_refresh(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        version: u32,
        now: Timestamp,
    ) -> Result<RefreshClaim, InstallationError> {
        self.inner
            .claim_refresh(organization_id, workspace_id, installation_id, version, now)
            .await
    }

    async fn rotate_credential(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        credential: &NewCredential,
        expires_at: Option<Timestamp>,
        now: Timestamp,
    ) -> Result<bool, InstallationError> {
        self.inner
            .rotate_credential(
                organization_id,
                workspace_id,
                installation_id,
                credential,
                expires_at,
                now,
            )
            .await
    }

    async fn require_reconnect(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        version: u32,
        now: Timestamp,
        audit: &Audit<'_, Installation>,
    ) -> Result<bool, InstallationError> {
        self.inner
            .require_reconnect(organization_id, workspace_id, installation_id, version, now, audit)
            .await
    }

    async fn list_app_installations(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        environment: Option<Environment>,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<AppInstallation>, InstallationError> {
        self.inner
            .list_app_installations(organization_id, app_id, environment, cursor, limit)
            .await
    }

    async fn count_by_app(
        &self,
        organization_id: OrganizationId,
        environment: Option<Environment>,
    ) -> Result<Vec<(AppId, u64)>, InstallationError> {
        self.inner.count_by_app(organization_id, environment).await
    }
}
