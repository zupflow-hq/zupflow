//! [`SqliteIdempotencyStore`]: idempotency keys, in `idempotency_keys`
//! (every statement repeats the organization, the workspace, the caller and
//! its environment, and the key, a missing workspace or environment matching
//! only a missing one) and `user_idempotency_keys` (the user and the key). `begin` runs in
//! an immediate transaction, so two requests with one key can't both start.

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use uuid::Uuid;
use zupflow_core::environment::Environment;
use zupflow_core::idempotency::{
    Begun, IdempotencyCaller, IdempotencyError, IdempotencyScope, IdempotencyStore, KEY_TTL_MS, LOCK_MS, LockToken,
};
use zupflow_core::time::Timestamp;

use crate::Database;

pub struct SqliteIdempotencyStore {
    database: Database,
}

impl SqliteIdempotencyStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> IdempotencyError {
    move |error| {
        tracing::error!(event = "idempotency.database_error", operation, %error);
        IdempotencyError::Unavailable
    }
}

/// One statement in each table's form. Static strings, chosen by scope.
struct Statements {
    sweep: &'static str,
    find: &'static str,
    take_over: &'static str,
    insert: &'static str,
    complete: &'static str,
    abandon: &'static str,
}

/// A key's own row: the organization, the workspace and the caller's
/// environment (a missing one counting as one value) and the caller and the
/// key, written exactly as `idempotency_keys_by_key` indexes them, so every
/// lookup uses that index.
macro_rules! organization_key {
    () => {
        "organization_id = ? AND coalesce(workspace_id, x'') = coalesce(?, x'') AND caller_kind = ? \
         AND caller_id = ? AND coalesce(caller_environment, '') = coalesce(?, '') AND key = ?"
    };
}

const ORGANIZATION: Statements = Statements {
    sweep: "DELETE FROM idempotency_keys WHERE organization_id = ? AND created_at <= ?",
    find: concat!(
        "SELECT fingerprint, state, status, body, locked_until FROM idempotency_keys WHERE ",
        organization_key!()
    ),
    take_over: concat!(
        "UPDATE idempotency_keys SET lock_token = ?, locked_until = ? WHERE ",
        organization_key!(),
        " AND state = 'running'"
    ),
    insert: "INSERT INTO idempotency_keys (id, organization_id, workspace_id, caller_kind, caller_id, \
             caller_environment, key, fingerprint, state, lock_token, locked_until, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'running', ?, ?, ?)",
    complete: concat!(
        "UPDATE idempotency_keys SET state = 'completed', status = ?, body = ?, lock_token = NULL, \
         locked_until = NULL WHERE ",
        organization_key!(),
        " AND lock_token = ?"
    ),
    abandon: concat!(
        "DELETE FROM idempotency_keys WHERE ",
        organization_key!(),
        " AND lock_token = ?"
    ),
};

const USER: Statements = Statements {
    sweep: "DELETE FROM user_idempotency_keys WHERE user_id = ? AND created_at <= ?",
    find: "SELECT fingerprint, state, status, body, locked_until FROM user_idempotency_keys \
           WHERE user_id = ? AND key = ?",
    take_over: "UPDATE user_idempotency_keys SET lock_token = ?, locked_until = ? \
                WHERE user_id = ? AND key = ? AND state = 'running'",
    insert: "INSERT INTO user_idempotency_keys (user_id, key, fingerprint, state, lock_token, locked_until, \
             created_at) VALUES (?, ?, ?, 'running', ?, ?, ?)",
    complete: "UPDATE user_idempotency_keys SET state = 'completed', status = ?, body = ?, lock_token = NULL, \
               locked_until = NULL WHERE user_id = ? AND key = ? AND lock_token = ?",
    abandon: "DELETE FROM user_idempotency_keys WHERE user_id = ? AND key = ? AND lock_token = ?",
};

fn statements(scope: &IdempotencyScope) -> &'static Statements {
    match scope {
        IdempotencyScope::Organization { .. } => &ORGANIZATION,
        IdempotencyScope::User { .. } => &USER,
    }
}

/// Binds the scope's own columns, in the order every statement names them.
fn bind_scope<'q>(
    query: sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments>,
    scope: &'q IdempotencyScope,
) -> sqlx::query::Query<'q, Sqlite, sqlx::sqlite::SqliteArguments> {
    match scope {
        IdempotencyScope::Organization {
            organization_id,
            workspace_id,
            caller,
        } => {
            let (kind, id, environment) = caller_columns(caller);
            query
                .bind(organization_id.uuid())
                .bind(workspace_id.map(|id| id.uuid()))
                .bind(kind)
                .bind(id)
                .bind(environment)
        }
        IdempotencyScope::User { user_id } => query.bind(user_id.uuid()),
    }
}

/// A caller as its three columns: kind, UUID and environment.
fn caller_columns(caller: &IdempotencyCaller) -> (&'static str, Uuid, Option<&'static str>) {
    match *caller {
        IdempotencyCaller::Member { user_id, environment } => {
            ("member", user_id.uuid(), environment.map(Environment::as_str))
        }
        IdempotencyCaller::ApiKey {
            api_key_id,
            environment,
        } => ("api_key", api_key_id.uuid(), Some(environment.as_str())),
        IdempotencyCaller::ConnectSession {
            session_id,
            environment,
        } => ("connect_session", session_id.uuid(), Some(environment.as_str())),
    }
}

/// The first column the sweep filters by: the organization or the user.
fn owner(scope: &IdempotencyScope) -> Uuid {
    match scope {
        IdempotencyScope::Organization { organization_id, .. } => organization_id.uuid(),
        IdempotencyScope::User { user_id } => user_id.uuid(),
    }
}

struct Existing {
    fingerprint: String,
    completed: bool,
    status: Option<i64>,
    body: Option<Vec<u8>>,
    locked_until: Option<i64>,
}

fn existing_from(row: &SqliteRow) -> Result<Existing, IdempotencyError> {
    let read = |error: sqlx::Error| {
        tracing::error!(event = "idempotency.unreadable_row", %error);
        IdempotencyError::Unavailable
    };
    let state: String = row.try_get("state").map_err(read)?;
    Ok(Existing {
        fingerprint: row.try_get("fingerprint").map_err(read)?,
        completed: state == "completed",
        status: row.try_get("status").map_err(read)?,
        body: row.try_get("body").map_err(read)?,
        locked_until: row.try_get("locked_until").map_err(read)?,
    })
}

async fn insert(
    connection: &mut SqliteConnection,
    scope: &IdempotencyScope,
    key: &str,
    fingerprint: &str,
    lock: &LockToken,
    now: i64,
) -> Result<(), IdempotencyError> {
    let sql = statements(scope).insert;
    let query = match scope {
        IdempotencyScope::Organization {
            organization_id,
            workspace_id,
            caller,
        } => {
            let (kind, id, environment) = caller_columns(caller);
            sqlx::query(sql)
                .bind(Uuid::now_v7())
                .bind(organization_id.uuid())
                .bind(workspace_id.map(|id| id.uuid()))
                .bind(kind)
                .bind(id)
                .bind(environment)
        }
        IdempotencyScope::User { user_id } => sqlx::query(sql).bind(user_id.uuid()),
    };
    query
        .bind(key)
        .bind(fingerprint)
        .bind(lock.as_bytes().to_vec())
        .bind(now + LOCK_MS)
        .bind(now)
        .execute(&mut *connection)
        .await
        .map_err(failed("insert"))?;
    Ok(())
}

#[async_trait]
impl IdempotencyStore for SqliteIdempotencyStore {
    async fn begin(
        &self,
        scope: &IdempotencyScope,
        key: &str,
        fingerprint: &str,
        now: Timestamp,
    ) -> Result<Begun, IdempotencyError> {
        let sql = statements(scope);
        let now = now.millis();
        let mut transaction: Transaction<'static, Sqlite> = self
            .database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin"))?;
        // Expired keys count as unused: this scope's owner sheds them as it goes.
        sqlx::query(sql.sweep)
            .bind(owner(scope))
            .bind(now - KEY_TTL_MS)
            .execute(&mut *transaction)
            .await
            .map_err(failed("sweep"))?;
        let row = bind_scope(sqlx::query(sql.find), scope)
            .bind(key)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(failed("find"))?;
        let begun = match row.as_ref().map(existing_from).transpose()? {
            None => {
                let lock = LockToken::generate()?;
                insert(&mut transaction, scope, key, fingerprint, &lock, now).await?;
                Begun::Run(lock)
            }
            Some(existing) if existing.fingerprint != fingerprint => Begun::Reused,
            Some(existing) if existing.completed => Begun::Replay {
                status: existing
                    .status
                    .and_then(|status| u16::try_from(status).ok())
                    .ok_or(IdempotencyError::Unavailable)?,
                body: existing.body,
            },
            Some(existing) if existing.locked_until.is_some_and(|until| until > now) => Begun::InProgress,
            Some(_) => {
                // A request that never finished: its lock lapsed, so this one takes the key over.
                let lock = LockToken::generate()?;
                bind_scope(
                    sqlx::query(sql.take_over)
                        .bind(lock.as_bytes().to_vec())
                        .bind(now + LOCK_MS),
                    scope,
                )
                .bind(key)
                .execute(&mut *transaction)
                .await
                .map_err(failed("take_over"))?;
                Begun::Run(lock)
            }
        };
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(begun)
    }

    async fn complete(
        &self,
        scope: &IdempotencyScope,
        key: &str,
        lock: &LockToken,
        status: u16,
        body: Option<Vec<u8>>,
    ) -> Result<(), IdempotencyError> {
        bind_scope(
            sqlx::query(statements(scope).complete)
                .bind(i64::from(status))
                .bind(body),
            scope,
        )
        .bind(key)
        .bind(lock.as_bytes().to_vec())
        .execute(self.database.pool())
        .await
        .map_err(failed("complete"))?;
        Ok(())
    }

    async fn abandon(&self, scope: &IdempotencyScope, key: &str, lock: &LockToken) -> Result<(), IdempotencyError> {
        bind_scope(sqlx::query(statements(scope).abandon), scope)
            .bind(key)
            .bind(lock.as_bytes().to_vec())
            .execute(self.database.pool())
            .await
            .map_err(failed("abandon"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use super::*;
    use crate::schema::{Temporary, temporary};

    #[tokio::test]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let seeding = database.clone();
        let prepare: zupflow_conformance::idempotency_store::Prepare = Arc::new(move |org, workspaces, user| {
            let database = seeding.clone();
            Box::pin(async move {
                let pool = database.pool();
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org.uuid())
                    .bind(format!("org-{}", org.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                for ws in workspaces {
                    sqlx::query(
                        "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
                         VALUES (?, ?, 'sandbox', 'Test', ?, 0)",
                    )
                    .bind(ws.uuid())
                    .bind(org.uuid())
                    .bind(format!("ws-{}", ws.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
                }
                sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(user.uuid())
                    .bind(format!("{}@people.example", user.uuid().simple()))
                    .execute(pool)
                    .await
                    .unwrap();
            })
        });
        zupflow_conformance::idempotency_store::run(&SqliteIdempotencyStore::new(database), prepare).await;
    }

    /// A workspace route's keys go with the workspace.
    #[tokio::test]
    async fn deletes_a_workspaces_keys_with_it() {
        use zupflow_core::ids::{OrganizationId, WorkspaceId};

        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let (org, ws) = (OrganizationId::generate(), WorkspaceId::generate());
        sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', 'test', 0)")
            .bind(org.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
             VALUES (?, ?, 'sandbox', 'Test', 'test', 0)",
        )
        .bind(ws.uuid())
        .bind(org.uuid())
        .execute(database.pool())
        .await
        .unwrap();
        let store = SqliteIdempotencyStore::new(database.clone());
        // One caller throughout, so each key is completed, not left running.
        let user = zupflow_core::ids::UserId::generate();
        let scope = |workspace_id| IdempotencyScope::Organization {
            organization_id: org,
            workspace_id,
            caller: IdempotencyCaller::Member {
                user_id: user,
                environment: Some(Environment::Sandbox),
            },
        };
        for workspace in [Some(ws), None] {
            let Begun::Run(lock) = store
                .begin(&scope(workspace), "k", "fp", Timestamp::from_millis(0))
                .await
                .unwrap()
            else {
                panic!("runs");
            };
            store
                .complete(&scope(workspace), "k", &lock, 201, Some(vec![]))
                .await
                .unwrap();
        }
        sqlx::query("DELETE FROM workspaces WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(ws.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency_keys WHERE organization_id = ?")
            .bind(org.uuid())
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(left, 1, "only the organization route's key is left");
        let state: String = sqlx::query_scalar("SELECT state FROM idempotency_keys WHERE organization_id = ?")
            .bind(org.uuid())
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(state, "completed");
    }
}
