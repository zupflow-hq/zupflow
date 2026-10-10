//! [`SqliteApiKeyStore`]: API keys. Every statement repeats `organization_id`,
//! except the one lookup by hash that learns it
//! ([`ApiKeyStore::find_active_api_key_by_hash`], cross-tenant by design).

use async_trait::async_trait;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
use zupflow_core::api_keys::{ApiKey, ApiKeyError, ApiKeyStore, NewApiKey};
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ApiKeyId, OrganizationId, UserId};
use zupflow_core::organizations::User;
use zupflow_core::time::Timestamp;

use crate::Database;

/// How often a key's `last_used_at` is written at most.
const USE_GRANULARITY_MS: i64 = 60_000;

/// The columns `api_key_from` reads, with the creator. A macro, so queries
/// stay static strings.
macro_rules! select_keys {
    () => {
        "SELECT k.id, k.organization_id, k.environment, k.name, k.prefix, k.last4, k.created_at, \
         k.last_used_at, k.revoked_at, u.id AS user_id, u.name AS user_name, u.email AS user_email, \
         u.avatar_url AS user_avatar_url FROM api_keys k JOIN users u ON u.id = k.created_by_user_id"
    };
}

pub struct SqliteApiKeyStore {
    database: Database,
}

impl SqliteApiKeyStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, ApiKeyError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin"))
    }
}

/// The key, in this organization only, on the connection given.
async fn key_in(
    connection: &mut sqlx::SqliteConnection,
    organization_id: OrganizationId,
    api_key_id: ApiKeyId,
) -> Result<Option<ApiKey>, ApiKeyError> {
    sqlx::query(concat!(select_keys!(), " WHERE k.organization_id = ? AND k.id = ?"))
        .bind(organization_id.uuid())
        .bind(api_key_id.uuid())
        .fetch_optional(&mut *connection)
        .await
        .map_err(failed("find_api_key"))?
        .as_ref()
        .map(api_key_from)
        .transpose()
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> ApiKeyError {
    move |error| {
        tracing::error!(event = "api_keys.database_error", operation, %error);
        ApiKeyError::Unavailable
    }
}

fn read(error: sqlx::Error) -> ApiKeyError {
    tracing::error!(event = "api_keys.unreadable_row", %error);
    ApiKeyError::Unavailable
}

fn api_key_from(row: &SqliteRow) -> Result<ApiKey, ApiKeyError> {
    let environment: String = row.try_get("environment").map_err(read)?;
    let instant = |column: &str| -> Result<Option<Timestamp>, ApiKeyError> {
        Ok(row
            .try_get::<Option<i64>, _>(column)
            .map_err(read)?
            .map(Timestamp::from_millis))
    };
    Ok(ApiKey {
        id: ApiKeyId::from_uuid(row.try_get("id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        environment: Environment::parse(&environment).ok_or(ApiKeyError::Unavailable)?,
        name: row.try_get("name").map_err(read)?,
        prefix: row.try_get("prefix").map_err(read)?,
        last4: row.try_get("last4").map_err(read)?,
        created_by: User {
            id: UserId::from_uuid(row.try_get("user_id").map_err(read)?),
            name: row.try_get("user_name").map_err(read)?,
            email: row.try_get("user_email").map_err(read)?,
            avatar_url: row.try_get("user_avatar_url").map_err(read)?,
        },
        created_at: Timestamp::from_millis(row.try_get("created_at").map_err(read)?),
        last_used_at: instant("last_used_at")?,
        revoked_at: instant("revoked_at")?,
    })
}

#[async_trait]
impl ApiKeyStore for SqliteApiKeyStore {
    async fn list_api_keys(&self, organization_id: OrganizationId) -> Result<Vec<ApiKey>, ApiKeyError> {
        sqlx::query(concat!(
            select_keys!(),
            " WHERE k.organization_id = ? ORDER BY k.created_at DESC, k.id DESC"
        ))
        .bind(organization_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_api_keys"))?
        .iter()
        .map(api_key_from)
        .collect()
    }

    async fn create_api_key(
        &self,
        organization_id: OrganizationId,
        api_key_id: ApiKeyId,
        key: &NewApiKey,
        audit: &Audit<'_, ApiKey>,
    ) -> Result<ApiKey, ApiKeyError> {
        let mut transaction = self.immediate().await?;
        sqlx::query(
            "INSERT INTO api_keys (id, organization_id, environment, name, prefix, last4, secret_hash, \
             created_by_user_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(api_key_id.uuid())
        .bind(organization_id.uuid())
        .bind(key.environment.as_str())
        .bind(&key.name)
        .bind(&key.prefix)
        .bind(&key.last4)
        .bind(key.secret_hash.to_vec())
        .bind(key.created_by.uuid())
        .bind(Timestamp::now().millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("insert_api_key"))?;
        let created = key_in(&mut transaction, organization_id, api_key_id)
            .await?
            .ok_or(ApiKeyError::Unavailable)?;
        crate::audit::record(&mut transaction, organization_id, &audit(&created))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(created)
    }

    async fn find_api_key(
        &self,
        organization_id: OrganizationId,
        api_key_id: ApiKeyId,
    ) -> Result<Option<ApiKey>, ApiKeyError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("acquire"))?;
        key_in(&mut connection, organization_id, api_key_id).await
    }

    async fn revoke_api_key(
        &self,
        organization_id: OrganizationId,
        api_key_id: ApiKeyId,
        audit: &Audit<'_, ApiKey>,
    ) -> Result<ApiKey, ApiKeyError> {
        let mut transaction = self.immediate().await?;
        let revoked = sqlx::query(
            "UPDATE api_keys SET revoked_at = ? WHERE organization_id = ? AND id = ? AND revoked_at IS NULL",
        )
        .bind(Timestamp::now().millis())
        .bind(organization_id.uuid())
        .bind(api_key_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("revoke_api_key"))?;
        let key = key_in(&mut transaction, organization_id, api_key_id)
            .await?
            .ok_or(ApiKeyError::NotFound)?;
        if revoked.rows_affected() == 0 {
            return Err(ApiKeyError::AlreadyRevoked);
        }
        crate::audit::record(&mut transaction, organization_id, &audit(&key))
            .await
            .map_err(failed("audit"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(key)
    }

    async fn find_active_api_key_by_hash(&self, secret_hash: &[u8; 32]) -> Result<Option<ApiKey>, ApiKeyError> {
        let row = sqlx::query(concat!(
            select_keys!(),
            " WHERE k.secret_hash = ? AND k.revoked_at IS NULL"
        ))
        .bind(secret_hash.to_vec())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_api_key_by_hash"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut key = api_key_from(&row)?;
        // Use is kept to the minute, so most requests write nothing. From here
        // on the key's organization is known, and the write names it.
        let now = Timestamp::now();
        if key
            .last_used_at
            .is_none_or(|at| at.millis() < now.millis() - USE_GRANULARITY_MS)
        {
            // Recording use never decides whether the request runs: a failed
            // write is logged and the key still works.
            let touched = sqlx::query("UPDATE api_keys SET last_used_at = ? WHERE organization_id = ? AND id = ?")
                .bind(now.millis())
                .bind(key.organization_id.uuid())
                .bind(key.id.uuid())
                .execute(self.database.pool())
                .await;
            match touched {
                Ok(_) => key.last_used_at = Some(now),
                Err(error) => tracing::warn!(event = "api_keys.touch_failed", %error),
            }
        }
        Ok(Some(key))
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
        let prepare: zupflow_conformance::api_key_store::Prepare = Arc::new(move |org, user| {
            let database = seeding.clone();
            Box::pin(async move {
                sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(org.uuid())
                    .bind(format!("org-{}", org.uuid().simple()))
                    .execute(database.pool())
                    .await
                    .unwrap();
                sqlx::query("INSERT INTO users (id, name, email, created_at) VALUES (?, 'Test', ?, 0)")
                    .bind(user.uuid())
                    .bind(format!("{}@people.example", user.uuid().simple()))
                    .execute(database.pool())
                    .await
                    .unwrap();
            })
        });
        zupflow_conformance::api_key_store::run(&SqliteApiKeyStore::new(database), prepare).await;
    }

    #[tokio::test]
    async fn keeps_one_instance_secret() {
        let Temporary { database, directory } = temporary().await;
        let first = database.instance_secret("secret_key").await.unwrap();
        assert_eq!(database.instance_secret("secret_key").await.unwrap(), first);
        assert_ne!(database.instance_secret("another").await.unwrap(), first);
        database.pool().close().await;
        let reopened = Database::open(&directory.file()).await.unwrap();
        assert_eq!(
            reopened.instance_secret("secret_key").await.unwrap(),
            first,
            "across restarts"
        );
    }
}
