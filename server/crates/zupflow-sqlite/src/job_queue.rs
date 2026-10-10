//! [`SqliteJobQueue`]: the job queue in the database's `jobs` table.
//!
//! Every statement after a claim repeats the job's whole scope: its
//! organization, and its workspace and environment, each matched with `IS` so
//! that a missing one matches only a missing one, and holds only while the
//! lease hasn't run out. Claiming is the one
//! cross-tenant statement, in [`SqliteJobQueue::claim_next`] and the sweep
//! beside it.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use sqlx::{QueryBuilder, Row, Sqlite};
use uuid::Uuid;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::jobs::{
    Job, JobError, JobId, JobOptions, JobQueue, LeaseToken, MAX_ATTEMPTS, MAX_CLAIM_KINDS, MAX_JOB_BYTES, MAX_LEASE,
    MIN_LEASE, Retry,
};
use zupflow_core::scope::{Name, Scope};

use crate::database::{Database, duration_millis, millis};

pub struct SqliteJobQueue {
    database: Database,
}

impl SqliteJobQueue {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

/// A scope as its three columns.
struct ScopeColumns {
    organization_id: Uuid,
    workspace_id: Option<Uuid>,
    environment: Option<&'static str>,
}

fn columns(scope: &Scope) -> ScopeColumns {
    ScopeColumns {
        organization_id: scope.organization_id().uuid(),
        workspace_id: scope.workspace_id().map(|id| id.uuid()),
        environment: scope.environment().map(Environment::as_str),
    }
}

fn scope_from(organization_id: Uuid, workspace_id: Option<Uuid>, environment: Option<&str>) -> Result<Scope, JobError> {
    let organization_id = OrganizationId::from_uuid(organization_id);
    match (workspace_id, environment) {
        (None, None) => Ok(Scope::Organization(organization_id)),
        (Some(workspace_id), None) => Ok(Scope::Workspace(organization_id, WorkspaceId::from_uuid(workspace_id))),
        (None, Some("sandbox")) => Ok(Scope::Environment(organization_id, Environment::Sandbox)),
        (None, Some("production")) => Ok(Scope::Environment(organization_id, Environment::Production)),
        // The table's checks rule these out.
        _ => Err(unreadable("scope")),
    }
}

fn unreadable(what: &'static str) -> JobError {
    tracing::error!(event = "jobs.unreadable_row", what);
    JobError::Unavailable
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> JobError {
    move |error| {
        tracing::error!(event = "jobs.database_error", operation, %error);
        JobError::Unavailable
    }
}

fn lease_duration(lease: Duration) -> Duration {
    lease.clamp(MIN_LEASE, MAX_LEASE)
}

#[async_trait]
impl JobQueue for SqliteJobQueue {
    async fn enqueue(
        &self,
        scope: &Scope,
        kind: &Name,
        payload: Vec<u8>,
        options: JobOptions,
    ) -> Result<JobId, JobError> {
        if payload.len() > MAX_JOB_BYTES {
            return Err(JobError::TooLarge);
        }
        let id = JobId::generate();
        let scope = columns(scope);
        let now = SystemTime::now();
        let run_at = millis(now).saturating_add(duration_millis(options.delay));
        sqlx::query(
            "INSERT INTO jobs (id, organization_id, workspace_id, environment, kind, payload, state, attempts, \
             max_attempts, run_at, created_at) VALUES (?, ?, ?, ?, ?, ?, 'queued', 0, ?, ?, ?)",
        )
        .bind(id.uuid())
        .bind(scope.organization_id)
        .bind(scope.workspace_id)
        .bind(scope.environment)
        .bind(kind.as_str())
        .bind(payload)
        .bind(options.max_attempts.clamp(1, MAX_ATTEMPTS))
        .bind(run_at)
        .bind(millis(now))
        .execute(self.database.pool())
        .await
        .map_err(failed("enqueue"))?;
        Ok(id)
    }

    /// **Cross-tenant**, by design: a worker serves every organization. It
    /// reads only `jobs`, and the scope it returns is the job's own.
    async fn claim_next(&self, kinds: &[Name], lease: Duration) -> Result<Option<Job>, JobError> {
        if kinds.is_empty() {
            return Ok(None);
        }
        let kinds = &kinds[..kinds.len().min(MAX_CLAIM_KINDS)];
        let now = millis(SystemTime::now());
        let token = LeaseToken::generate()?;
        let expires_at = now.saturating_add(duration_millis(lease_duration(lease)));

        // A job whose last attempt's lease ran out is dead, not claimable.
        sqlx::query(
            "UPDATE jobs SET state = 'dead', reason = 'lease_expired', lease_token = NULL, lease_expires_at = NULL \
             WHERE state = 'running' AND lease_expires_at <= ? AND attempts >= max_attempts",
        )
        .bind(now)
        .execute(self.database.pool())
        .await
        .map_err(failed("bury_expired"))?;

        // One statement, so two workers can't both take the same job.
        let mut query =
            QueryBuilder::<Sqlite>::new("UPDATE jobs SET state = 'running', attempts = attempts + 1, lease_token = ");
        query.push_bind(token.as_bytes().to_vec());
        query.push(", lease_expires_at = ");
        query.push_bind(expires_at);
        query.push(" WHERE id = (SELECT id FROM jobs WHERE kind IN (");
        let mut separated = query.separated(", ");
        for kind in kinds {
            separated.push_bind(kind.as_str());
        }
        query.push(") AND ((state = 'queued' AND run_at <= ");
        query.push_bind(now);
        query.push(") OR (state = 'running' AND lease_expires_at <= ");
        query.push_bind(now);
        query.push(
            " AND attempts < max_attempts)) ORDER BY run_at, id LIMIT 1) \
             RETURNING id, organization_id, workspace_id, environment, kind, payload, attempts",
        );
        let Some(row) = query
            .build()
            .fetch_optional(self.database.pool())
            .await
            .map_err(failed("claim_next"))?
        else {
            return Ok(None);
        };

        let read = |error: sqlx::Error| {
            tracing::error!(event = "jobs.unreadable_row", %error);
            JobError::Unavailable
        };
        let environment: Option<String> = row.try_get("environment").map_err(read)?;
        let scope = scope_from(
            row.try_get("organization_id").map_err(read)?,
            row.try_get("workspace_id").map_err(read)?,
            environment.as_deref(),
        )?;
        let kind: String = row.try_get("kind").map_err(read)?;
        let attempts: i64 = row.try_get("attempts").map_err(read)?;
        Ok(Some(Job {
            id: JobId::from_uuid(row.try_get("id").map_err(read)?),
            scope,
            kind: Name::new(&kind).map_err(|_| unreadable("kind"))?,
            payload: row.try_get("payload").map_err(read)?,
            attempt: u32::try_from(attempts).map_err(|_| unreadable("attempts"))?,
            lease: token,
        }))
    }

    async fn complete(&self, scope: &Scope, id: JobId, lease: &LeaseToken) -> Result<(), JobError> {
        let scope = columns(scope);
        let done = sqlx::query(
            "DELETE FROM jobs WHERE organization_id = ? AND workspace_id IS ? AND environment IS ? AND id = ? \
             AND state = 'running' AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(scope.organization_id)
        .bind(scope.workspace_id)
        .bind(scope.environment)
        .bind(id.uuid())
        .bind(lease.as_bytes().to_vec())
        .bind(millis(SystemTime::now()))
        .execute(self.database.pool())
        .await
        .map_err(failed("complete"))?;
        if done.rows_affected() == 0 {
            return Err(JobError::LeaseLost);
        }
        Ok(())
    }

    async fn retry(
        &self,
        scope: &Scope,
        id: JobId,
        lease: &LeaseToken,
        after: Duration,
        reason: &Name,
    ) -> Result<Retry, JobError> {
        let scope = columns(scope);
        let run_at = millis(SystemTime::now()).saturating_add(duration_millis(after));
        let state: Option<String> = sqlx::query_scalar(
            "UPDATE jobs SET \
               state = CASE WHEN attempts >= max_attempts THEN 'dead' ELSE 'queued' END, \
               reason = CASE WHEN attempts >= max_attempts THEN ? ELSE NULL END, \
               run_at = ?, lease_token = NULL, lease_expires_at = NULL \
             WHERE organization_id = ? AND workspace_id IS ? AND environment IS ? AND id = ? \
               AND state = 'running' AND lease_token = ? AND lease_expires_at > ? \
             RETURNING state",
        )
        .bind(reason.as_str())
        .bind(run_at)
        .bind(scope.organization_id)
        .bind(scope.workspace_id)
        .bind(scope.environment)
        .bind(id.uuid())
        .bind(lease.as_bytes().to_vec())
        .bind(millis(SystemTime::now()))
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("retry"))?;
        match state.as_deref() {
            Some("queued") => Ok(Retry::Scheduled),
            Some(_) => Ok(Retry::Exhausted),
            None => Err(JobError::LeaseLost),
        }
    }

    async fn bury(&self, scope: &Scope, id: JobId, lease: &LeaseToken, reason: &Name) -> Result<(), JobError> {
        let scope = columns(scope);
        let buried = sqlx::query(
            "UPDATE jobs SET state = 'dead', reason = ?, lease_token = NULL, lease_expires_at = NULL \
             WHERE organization_id = ? AND workspace_id IS ? AND environment IS ? AND id = ? \
               AND state = 'running' AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(reason.as_str())
        .bind(scope.organization_id)
        .bind(scope.workspace_id)
        .bind(scope.environment)
        .bind(id.uuid())
        .bind(lease.as_bytes().to_vec())
        .bind(millis(SystemTime::now()))
        .execute(self.database.pool())
        .await
        .map_err(failed("bury"))?;
        if buried.rows_affected() == 0 {
            return Err(JobError::LeaseLost);
        }
        Ok(())
    }

    async fn extend(
        &self,
        scope: &Scope,
        id: JobId,
        lease_token: &LeaseToken,
        lease: Duration,
    ) -> Result<(), JobError> {
        let scope = columns(scope);
        let expires_at = millis(SystemTime::now()).saturating_add(duration_millis(lease_duration(lease)));
        let extended = sqlx::query(
            "UPDATE jobs SET lease_expires_at = ? \
             WHERE organization_id = ? AND workspace_id IS ? AND environment IS ? AND id = ? \
               AND state = 'running' AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(expires_at)
        .bind(scope.organization_id)
        .bind(scope.workspace_id)
        .bind(scope.environment)
        .bind(id.uuid())
        .bind(lease_token.as_bytes().to_vec())
        .bind(millis(SystemTime::now()))
        .execute(self.database.pool())
        .await
        .map_err(failed("extend"))?;
        if extended.rows_affected() == 0 {
            return Err(JobError::LeaseLost);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use zupflow_conformance::job_queue::Prepare;

    use super::*;
    use crate::schema::{Temporary, TemporaryDirectory, temporary};

    /// Inserts the scope's organization and workspace, as the domains will.
    async fn insert_scope(database: &Database, scope: Scope) {
        let org = scope.organization_id().uuid();
        sqlx::query("INSERT OR IGNORE INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
            .bind(org)
            .bind(org.simple().to_string())
            .execute(database.pool())
            .await
            .unwrap();
        if let Some(ws) = scope.workspace_id() {
            sqlx::query(
                "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                 VALUES (?, ?, 'sandbox', 'Test', ?, 0)",
            )
            .bind(ws.uuid())
            .bind(org)
            .bind(ws.uuid().simple().to_string())
            .execute(database.pool())
            .await
            .unwrap();
        }
    }

    fn prepare(database: &Database) -> Prepare {
        let database = database.clone();
        Arc::new(move |scope| {
            let database = database.clone();
            Box::pin(async move { insert_scope(&database, scope).await })
        })
    }

    fn kind() -> Name {
        Name::new("test.kind").unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let queue = SqliteJobQueue::new(database.clone());
        zupflow_conformance::job_queue::run(&queue, prepare(&database)).await;
    }

    /// The foreign keys refuse a scope whose workspace isn't its organization's.
    #[tokio::test]
    async fn refuses_a_workspace_of_another_organization() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let queue = SqliteJobQueue::new(database.clone());
        let (org, other) = (OrganizationId::generate(), OrganizationId::generate());
        let ws = WorkspaceId::generate();
        insert_scope(&database, Scope::Workspace(org, ws)).await;
        insert_scope(&database, Scope::Organization(other)).await;
        let mismatched = Scope::Workspace(other, ws);
        assert_eq!(
            queue.enqueue(&mismatched, &kind(), vec![], JobOptions::default()).await,
            Err(JobError::Unavailable)
        );
        assert_eq!(
            queue
                .enqueue(
                    &Scope::Organization(OrganizationId::generate()),
                    &kind(),
                    vec![],
                    JobOptions::default()
                )
                .await,
            Err(JobError::Unavailable),
            "nor an organization that doesn't exist"
        );
        assert!(queue.claim_next(&[kind()], MIN_LEASE).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn deletes_a_workspaces_jobs_with_it() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let queue = SqliteJobQueue::new(database.clone());
        let (org, ws, sibling) = (
            OrganizationId::generate(),
            WorkspaceId::generate(),
            WorkspaceId::generate(),
        );
        insert_scope(&database, Scope::Workspace(org, ws)).await;
        insert_scope(&database, Scope::Workspace(org, sibling)).await;
        for scope in [Scope::Workspace(org, ws), Scope::Workspace(org, sibling)] {
            queue
                .enqueue(&scope, &kind(), vec![], JobOptions::default())
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM workspaces WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(ws.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        // A long lease: a slow runner must not see the first claim lapse and its job come back.
        let job = queue.claim_next(&[kind()], MAX_LEASE).await.unwrap().unwrap();
        assert_eq!(
            job.scope,
            Scope::Workspace(org, sibling),
            "only the sibling's job is left"
        );
        assert!(queue.claim_next(&[kind()], MAX_LEASE).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn keeps_jobs_across_a_restart() {
        let directory = TemporaryDirectory::new();
        let path = directory.file();
        let scope = Scope::Organization(OrganizationId::generate());
        {
            let database = Database::open(&path).await.unwrap();
            insert_scope(&database, scope).await;
            SqliteJobQueue::new(database)
                .enqueue(&scope, &kind(), b"kept".to_vec(), JobOptions::default())
                .await
                .unwrap();
        }
        let queue = SqliteJobQueue::new(Database::open(&path).await.unwrap());
        let job = queue.claim_next(&[kind()], MIN_LEASE).await.unwrap().unwrap();
        assert_eq!((job.scope, job.payload), (scope, b"kept".to_vec()));
    }
}
