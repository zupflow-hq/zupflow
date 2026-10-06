//! [`SqliteOrganizationStore`]: organizations, members and users.
//!
//! Every statement on `members` repeats `organization_id`, even beside a
//! unique member ID. The user-keyed reads (`user_for_identity`, a user's own
//! memberships) read global tables or the user's own rows only. Changes that
//! check before they write (the last-owner rule) run in an immediate
//! transaction, which takes SQLite's write lock first, so two changes can't
//! both pass the check.

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use zupflow_core::identity::VerifiedIdentity;
use zupflow_core::ids::{MemberId, OrganizationId, UserId};
use zupflow_core::organizations::{
    Member, MemberRole, MemberStatus, Organization, OrganizationSetup, OrganizationStore, StoreError, User,
};
use zupflow_core::time::Timestamp;

use crate::Database;

/// How often a member's `last_active_at` is written at most.
const ACTIVITY_GRANULARITY_MS: i64 = 60_000;

/// The columns `member_from` reads. Macros, so every query stays a static string.
macro_rules! member_columns {
    () => {
        "m.id AS member_id, m.organization_id AS member_organization_id, m.role, m.status, \
     m.invited_at, m.joined_at, m.last_active_at, \
     u.id AS user_id, u.name AS user_name, u.email AS user_email, u.avatar_url AS user_avatar_url"
    };
}

/// The columns `organization_from` reads.
macro_rules! organization_columns {
    () => {
        "o.id AS organization_id, o.name AS organization_name, o.slug AS organization_slug, \
         o.created_at AS organization_created_at"
    };
}

pub struct SqliteOrganizationStore {
    database: Database,
}

impl SqliteOrganizationStore {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn immediate(&self) -> Result<Transaction<'static, Sqlite>, StoreError> {
        self.database
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(failed("begin"))
    }
}

fn failed(operation: &'static str) -> impl FnOnce(sqlx::Error) -> StoreError {
    move |error| {
        tracing::error!(event = "organizations.database_error", operation, %error);
        StoreError::Unavailable
    }
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|error| error.is_unique_violation())
}

fn read(error: sqlx::Error) -> StoreError {
    tracing::error!(event = "organizations.unreadable_row", %error);
    StoreError::Unavailable
}

fn user_from(row: &SqliteRow) -> Result<User, StoreError> {
    Ok(User {
        id: UserId::from_uuid(row.try_get("user_id").map_err(read)?),
        name: row.try_get("user_name").map_err(read)?,
        email: row.try_get("user_email").map_err(read)?,
        avatar_url: row.try_get("user_avatar_url").map_err(read)?,
    })
}

fn member_from(row: &SqliteRow) -> Result<Member, StoreError> {
    let role: String = row.try_get("role").map_err(read)?;
    let status: String = row.try_get("status").map_err(read)?;
    let optional = |column: &str| -> Result<Option<Timestamp>, StoreError> {
        Ok(row
            .try_get::<Option<i64>, _>(column)
            .map_err(read)?
            .map(Timestamp::from_millis))
    };
    Ok(Member {
        id: MemberId::from_uuid(row.try_get("member_id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("member_organization_id").map_err(read)?),
        user: user_from(row)?,
        role: MemberRole::parse(&role).ok_or(StoreError::Unavailable)?,
        status: MemberStatus::parse(&status).ok_or(StoreError::Unavailable)?,
        invited_at: Timestamp::from_millis(row.try_get("invited_at").map_err(read)?),
        joined_at: optional("joined_at")?,
        last_active_at: optional("last_active_at")?,
    })
}

fn organization_from(row: &SqliteRow) -> Result<Organization, StoreError> {
    Ok(Organization {
        id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        name: row.try_get("organization_name").map_err(read)?,
        slug: row.try_get("organization_slug").map_err(read)?,
        created_at: Timestamp::from_millis(row.try_get("organization_created_at").map_err(read)?),
    })
}

/// A member, read inside a transaction.
async fn member_in(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    member_id: MemberId,
) -> Result<Option<Member>, StoreError> {
    sqlx::query(concat!(
        "SELECT ",
        member_columns!(),
        " FROM members m JOIN users u ON u.id = m.user_id \
         WHERE m.organization_id = ? AND m.id = ?"
    ))
    .bind(organization_id.uuid())
    .bind(member_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("find_member"))?
    .as_ref()
    .map(member_from)
    .transpose()
}

/// The acting member's role as of this transaction: [`StoreError::NotAllowed`]
/// if they're no longer an active member of the organization.
async fn acting_role(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    acting: MemberId,
) -> Result<MemberRole, StoreError> {
    match member_in(connection, organization_id, acting).await? {
        Some(member) if member.status == MemberStatus::Active => Ok(member.role),
        _ => Err(StoreError::NotAllowed),
    }
}

/// Whether taking `member` out of the owners would leave its organization
/// without an active owner.
async fn is_last_owner(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    member: &Member,
) -> Result<bool, StoreError> {
    if member.role != MemberRole::Owner || member.status != MemberStatus::Active {
        return Ok(false);
    }
    let owners: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM members WHERE organization_id = ? AND role = 'owner' AND status = 'active'",
    )
    .bind(organization_id.uuid())
    .fetch_one(&mut *connection)
    .await
    .map_err(failed("count_owners"))?;
    Ok(owners <= 1)
}

/// The user an identity is linked to, if any.
async fn linked_user(
    connection: &mut SqliteConnection,
    identity: &VerifiedIdentity,
) -> Result<Option<User>, StoreError> {
    sqlx::query(
        "SELECT u.id AS user_id, u.name AS user_name, u.email AS user_email, u.avatar_url AS user_avatar_url \
         FROM user_identities i JOIN users u ON u.id = i.user_id WHERE i.provider = ? AND i.subject = ?",
    )
    .bind(&identity.provider)
    .bind(&identity.subject)
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("find_identity"))?
    .as_ref()
    .map(user_from)
    .transpose()
}

/// The user with this email, or a new one named after its local part.
async fn user_with_email(
    connection: &mut SqliteConnection,
    email: &str,
    name: &str,
    avatar_url: Option<&str>,
) -> Result<(User, bool), StoreError> {
    let existing = sqlx::query(
        "SELECT id AS user_id, name AS user_name, email AS user_email, avatar_url AS user_avatar_url \
         FROM users WHERE email = ?",
    )
    .bind(email)
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("find_user"))?;
    if let Some(row) = existing {
        return Ok((user_from(&row)?, false));
    }
    let user = User {
        id: UserId::generate(),
        name: name.to_owned(),
        email: email.to_owned(),
        avatar_url: avatar_url.map(str::to_owned),
    };
    sqlx::query("INSERT INTO users (id, name, email, avatar_url, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(user.id.uuid())
        .bind(&user.name)
        .bind(&user.email)
        .bind(&user.avatar_url)
        .bind(Timestamp::now().millis())
        .execute(&mut *connection)
        .await
        .map_err(failed("insert_user"))?;
    Ok((user, true))
}

/// A display name for a user who has none yet: their address's local part.
fn name_from_email(email: &str) -> String {
    let local = email.split('@').next().unwrap_or(email);
    if local.is_empty() {
        email.to_owned()
    } else {
        local.to_owned()
    }
}

fn bounded(name: &str, max: usize) -> String {
    name.chars().take(max).collect()
}

#[async_trait]
impl OrganizationStore for SqliteOrganizationStore {
    async fn user_for_identity(&self, identity: &VerifiedIdentity) -> Result<User, StoreError> {
        let name = match identity.profile.name.trim() {
            "" => name_from_email(&identity.profile.email.to_lowercase()),
            name => bounded(name, 200),
        };
        let avatar_url = identity
            .profile
            .avatar_url
            .clone()
            .filter(|url| (1..=2000).contains(&url.len()));

        // Every Console request lands here, so a linked identity is read
        // without SQLite's write lock, and written only when its profile changed.
        let mut connection = self.database.pool().acquire().await.map_err(failed("acquire"))?;
        if let Some(user) = linked_user(&mut connection, identity).await? {
            if user.name == name && user.avatar_url == avatar_url {
                return Ok(user);
            }
            sqlx::query("UPDATE users SET name = ?, avatar_url = ? WHERE id = ?")
                .bind(&name)
                .bind(&avatar_url)
                .bind(user.id.uuid())
                .execute(&mut *connection)
                .await
                .map_err(failed("refresh_profile"))?;
            return Ok(User {
                name,
                avatar_url,
                ..user
            });
        }
        drop(connection);

        // A new identity never creates or claims a user on an email nobody verified.
        if !identity.profile.email_verified {
            return Err(StoreError::EmailUnverified);
        }
        let email = identity.profile.email.to_lowercase();
        let mut transaction = self.immediate().await?;
        // Another request may have linked it since the read above.
        if let Some(user) = linked_user(&mut transaction, identity).await? {
            return Ok(user);
        }
        let (user, created) = user_with_email(&mut transaction, &email, &name, avatar_url.as_deref()).await?;
        if !created {
            // Only an invitee nobody signs in as yet can be claimed.
            let linked: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_identities WHERE user_id = ?)")
                .bind(user.id.uuid())
                .fetch_one(&mut *transaction)
                .await
                .map_err(failed("find_links"))?;
            if linked {
                return Err(StoreError::EmailInUse);
            }
            sqlx::query("UPDATE users SET name = ?, avatar_url = ? WHERE id = ?")
                .bind(&name)
                .bind(&avatar_url)
                .bind(user.id.uuid())
                .execute(&mut *transaction)
                .await
                .map_err(failed("claim_user"))?;
        }
        sqlx::query("INSERT INTO user_identities (provider, subject, user_id, created_at) VALUES (?, ?, ?, ?)")
            .bind(&identity.provider)
            .bind(&identity.subject)
            .bind(user.id.uuid())
            .bind(Timestamp::now().millis())
            .execute(&mut *transaction)
            .await
            .map_err(failed("link_identity"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(User {
            name,
            avatar_url,
            ..user
        })
    }

    async fn list_active_memberships_of_user(
        &self,
        user_id: UserId,
    ) -> Result<Vec<(Organization, Member)>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            organization_columns!(),
            ", ",
            member_columns!(),
            " FROM members m \
             JOIN organizations o ON o.id = m.organization_id JOIN users u ON u.id = m.user_id \
             WHERE m.user_id = ? AND m.status = 'active' ORDER BY o.created_at, o.id"
        ))
        .bind(user_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_memberships"))?;
        rows.iter()
            .map(|row| Ok((organization_from(row)?, member_from(row)?)))
            .collect()
    }

    async fn find_active_membership(
        &self,
        organization_id: OrganizationId,
        user_id: UserId,
    ) -> Result<Option<(Organization, Member)>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            organization_columns!(),
            ", ",
            member_columns!(),
            " FROM members m \
             JOIN organizations o ON o.id = m.organization_id JOIN users u ON u.id = m.user_id \
             WHERE m.organization_id = ? AND o.id = ? AND m.user_id = ? AND m.status = 'active'"
        ))
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(user_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(failed("find_membership"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let (organization, mut member) = (organization_from(&row)?, member_from(&row)?);
        // Activity is kept to the minute, so most requests write nothing.
        let now = Timestamp::now();
        let stale = member
            .last_active_at
            .is_none_or(|at| at.millis() < now.millis() - ACTIVITY_GRANULARITY_MS);
        if stale {
            sqlx::query("UPDATE members SET last_active_at = ? WHERE organization_id = ? AND id = ?")
                .bind(now.millis())
                .bind(organization_id.uuid())
                .bind(member.id.uuid())
                .execute(self.database.pool())
                .await
                .map_err(failed("touch_member"))?;
            member.last_active_at = Some(now);
        }
        Ok(Some((organization, member)))
    }

    async fn create_organization(
        &self,
        organization_id: OrganizationId,
        name: &str,
        slug: &str,
        user_id: UserId,
    ) -> Result<(Organization, Member), StoreError> {
        let now = Timestamp::now();
        let mut transaction = self.immediate().await?;
        sqlx::query("INSERT INTO organizations (id, name, slug, created_at) VALUES (?, ?, ?, ?)")
            .bind(organization_id.uuid())
            .bind(name)
            .bind(slug)
            .bind(now.millis())
            .execute(&mut *transaction)
            .await
            .map_err(|error| {
                if is_unique_violation(&error) {
                    StoreError::SlugTaken
                } else {
                    failed("insert_organization")(error)
                }
            })?;
        let member_id = MemberId::generate();
        sqlx::query(
            "INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at, last_active_at) \
             VALUES (?, ?, ?, 'owner', 'active', ?, ?, ?)",
        )
        .bind(member_id.uuid())
        .bind(organization_id.uuid())
        .bind(user_id.uuid())
        .bind(now.millis())
        .bind(now.millis())
        .bind(now.millis())
        .execute(&mut *transaction)
        .await
        .map_err(failed("insert_owner"))?;
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::Unavailable)?;
        transaction.commit().await.map_err(failed("commit"))?;
        let organization = Organization {
            id: organization_id,
            name: name.to_owned(),
            slug: slug.to_owned(),
            created_at: now,
        };
        Ok((organization, member))
    }

    async fn update_organization(
        &self,
        organization_id: OrganizationId,
        name: Option<&str>,
        slug: Option<&str>,
    ) -> Result<Organization, StoreError> {
        let row = sqlx::query(
            "UPDATE organizations SET name = coalesce(?, name), slug = coalesce(?, slug) WHERE id = ? \
             RETURNING id AS organization_id, name AS organization_name, slug AS organization_slug, \
             created_at AS organization_created_at",
        )
        .bind(name)
        .bind(slug)
        .bind(organization_id.uuid())
        .fetch_optional(self.database.pool())
        .await
        .map_err(|error| {
            if is_unique_violation(&error) {
                StoreError::SlugTaken
            } else {
                failed("update_organization")(error)
            }
        })?;
        organization_from(&row.ok_or(StoreError::NotFound)?)
    }

    async fn load_setup(&self, organization_id: OrganizationId) -> Result<OrganizationSetup, StoreError> {
        // A configuration is ready with a server, and for an OAuth app a client
        // ID and secret, as `Configuration::status` says.
        let row = sqlx::query(
            "SELECT \
             EXISTS (SELECT 1 FROM workspaces WHERE organization_id = ?1) AS has_workspace, \
             EXISTS (SELECT 1 FROM api_keys WHERE organization_id = ?1 AND revoked_at IS NULL) AS has_api_key, \
             (EXISTS (SELECT 1 FROM apps WHERE organization_id = ?1) \
              OR EXISTS (SELECT 1 FROM workspace_apps WHERE organization_id = ?1)) AS has_app, \
             (EXISTS (SELECT 1 FROM app_configurations c \
                JOIN apps a ON a.organization_id = c.organization_id AND a.id = c.app_id \
                WHERE c.organization_id = ?1 AND c.mcp_server_url IS NOT NULL \
                AND (json_extract(a.auth, '$.type') = 'token' \
                     OR (c.oauth_client_id IS NOT NULL AND c.secret_ciphertext IS NOT NULL))) \
              OR EXISTS (SELECT 1 FROM workspace_app_configurations c \
                JOIN workspace_apps a ON a.organization_id = c.organization_id \
                  AND a.workspace_id = c.workspace_id AND a.id = c.app_id \
                WHERE c.organization_id = ?1 AND c.mcp_server_url IS NOT NULL \
                AND (json_extract(a.auth, '$.type') = 'token' \
                     OR (c.oauth_client_id IS NOT NULL AND c.secret_ciphertext IS NOT NULL)))) \
             AS has_ready_configuration",
        )
        .bind(organization_id.uuid())
        .fetch_one(self.database.pool())
        .await
        .map_err(failed("load_setup"))?;
        // The rest arrive with their tables: installations, agents and runs.
        Ok(OrganizationSetup {
            has_workspace: row.try_get("has_workspace").map_err(read)?,
            has_api_key: row.try_get("has_api_key").map_err(read)?,
            has_app: row.try_get("has_app").map_err(read)?,
            has_ready_configuration: row.try_get("has_ready_configuration").map_err(read)?,
            ..OrganizationSetup::default()
        })
    }

    async fn list_members(&self, organization_id: OrganizationId) -> Result<Vec<Member>, StoreError> {
        sqlx::query(concat!(
            "SELECT ",
            member_columns!(),
            " FROM members m JOIN users u ON u.id = m.user_id \
             WHERE m.organization_id = ? ORDER BY m.invited_at, m.id"
        ))
        .bind(organization_id.uuid())
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_members"))?
        .iter()
        .map(member_from)
        .collect()
    }

    async fn invite_member(
        &self,
        organization_id: OrganizationId,
        email: &str,
        role: MemberRole,
    ) -> Result<Member, StoreError> {
        let email = email.to_lowercase();
        let mut transaction = self.immediate().await?;
        let (user, _) =
            user_with_email(&mut transaction, &email, &bounded(&name_from_email(&email), 200), None).await?;
        let member_id = MemberId::generate();
        sqlx::query(
            "INSERT INTO members (id, organization_id, user_id, role, status, invited_at) \
             VALUES (?, ?, ?, ?, 'invited', ?)",
        )
        .bind(member_id.uuid())
        .bind(organization_id.uuid())
        .bind(user.id.uuid())
        .bind(role.as_str())
        .bind(Timestamp::now().millis())
        .execute(&mut *transaction)
        .await
        .map_err(|error| {
            if is_unique_violation(&error) {
                StoreError::AlreadyMember
            } else {
                failed("insert_member")(error)
            }
        })?;
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::Unavailable)?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(member)
    }

    async fn find_member(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
    ) -> Result<Option<Member>, StoreError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("acquire"))?;
        member_in(&mut connection, organization_id, member_id).await
    }

    async fn update_member_role(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
        role: MemberRole,
        acting: MemberId,
    ) -> Result<Member, StoreError> {
        let mut transaction = self.immediate().await?;
        if acting_role(&mut transaction, organization_id, acting).await? != MemberRole::Owner {
            return Err(StoreError::NotAllowed);
        }
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if role != MemberRole::Owner && is_last_owner(&mut transaction, organization_id, &member).await? {
            return Err(StoreError::LastOwner);
        }
        sqlx::query("UPDATE members SET role = ? WHERE organization_id = ? AND id = ?")
            .bind(role.as_str())
            .bind(organization_id.uuid())
            .bind(member_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("update_role"))?;
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::Unavailable)?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(member)
    }

    async fn remove_member(
        &self,
        organization_id: OrganizationId,
        member_id: MemberId,
        acting: MemberId,
    ) -> Result<(), StoreError> {
        let mut transaction = self.immediate().await?;
        let acting_role = acting_role(&mut transaction, organization_id, acting).await?;
        if !acting_role.is_admin() {
            return Err(StoreError::NotAllowed);
        }
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if member.role == MemberRole::Owner && acting_role != MemberRole::Owner {
            return Err(StoreError::NotAllowed);
        }
        if is_last_owner(&mut transaction, organization_id, &member).await? {
            return Err(StoreError::LastOwner);
        }
        sqlx::query("DELETE FROM members WHERE organization_id = ? AND id = ?")
            .bind(organization_id.uuid())
            .bind(member_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("remove_member"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::schema::{Temporary, temporary};

    #[tokio::test(flavor = "multi_thread")]
    async fn conforms() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        zupflow_conformance::organization_store::run(&SqliteOrganizationStore::new(database)).await;
    }

    fn person(subject: &str) -> VerifiedIdentity {
        VerifiedIdentity {
            provider: "test".to_owned(),
            subject: subject.to_owned(),
            profile: zupflow_core::identity::Profile {
                name: subject.to_owned(),
                email: format!("{subject}@people.example"),
                email_verified: true,
                avatar_url: None,
            },
        }
    }

    /// Resolving one organization records activity there only, never in the
    /// user's other organizations.
    #[tokio::test]
    async fn records_activity_in_one_organization_only() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteOrganizationStore::new(database.clone());
        let alice = store.user_for_identity(&person("alice")).await.unwrap();
        let (a, _) = store
            .create_organization(OrganizationId::generate(), "A", "org-a", alice.id)
            .await
            .unwrap();
        let (b, _) = store
            .create_organization(OrganizationId::generate(), "B", "org-b", alice.id)
            .await
            .unwrap();
        sqlx::query("UPDATE members SET last_active_at = 0")
            .execute(database.pool())
            .await
            .unwrap();
        let (_, member) = store.find_active_membership(a.id, alice.id).await.unwrap().unwrap();
        assert!(member.last_active_at.unwrap().millis() > 0, "a's activity is recorded");
        let at = |org: OrganizationId| {
            let database = database.clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT last_active_at FROM members WHERE organization_id = ?")
                    .bind(org.uuid())
                    .fetch_one(database.pool())
                    .await
                    .unwrap()
            }
        };
        assert!(at(a.id).await > 0);
        assert_eq!(at(b.id).await, 0, "b's is untouched");
    }

    /// Another organization's workspace never counts towards this one's setup.
    #[tokio::test]
    async fn counts_only_its_own_workspaces_in_setup() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteOrganizationStore::new(database.clone());
        let alice = store.user_for_identity(&person("alice")).await.unwrap();
        let (a, _) = store
            .create_organization(OrganizationId::generate(), "A", "org-a", alice.id)
            .await
            .unwrap();
        let (b, _) = store
            .create_organization(OrganizationId::generate(), "B", "org-b", alice.id)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workspaces (id, organization_id, environment, name, slug, created_at) \
             VALUES (?, ?, 'sandbox', 'B', 'b', 0)",
        )
        .bind(uuid::Uuid::now_v7())
        .bind(b.id.uuid())
        .execute(database.pool())
        .await
        .unwrap();
        assert!(store.load_setup(b.id).await.unwrap().has_workspace);
        assert!(!store.load_setup(a.id).await.unwrap().has_workspace);
    }

    /// Two owners demoting each other at once: the immediate transactions
    /// serialize them, so one wins and the organization keeps an owner.
    #[tokio::test(flavor = "multi_thread")]
    async fn keeps_an_owner_when_two_demote_each_other_at_once() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteOrganizationStore::new(database.clone());
        let identity = |subject: &str| VerifiedIdentity {
            provider: "test".to_owned(),
            subject: subject.to_owned(),
            profile: zupflow_core::identity::Profile {
                name: subject.to_owned(),
                email: format!("{subject}@example.com"),
                email_verified: true,
                avatar_url: None,
            },
        };
        let alice = store.user_for_identity(&identity("alice")).await.unwrap();
        let bob = store.user_for_identity(&identity("bob")).await.unwrap();
        let (org, alice_member) = store
            .create_organization(OrganizationId::generate(), "Two owners", "two-owners", alice.id)
            .await
            .unwrap();
        // Make bob an active owner directly: invitations have no acceptance yet.
        let bob_member = MemberId::generate();
        sqlx::query(
            "INSERT INTO members (id, organization_id, user_id, role, status, invited_at, joined_at) \
             VALUES (?, ?, ?, 'owner', 'active', 0, 0)",
        )
        .bind(bob_member.uuid())
        .bind(org.id.uuid())
        .bind(bob.id.uuid())
        .execute(database.pool())
        .await
        .unwrap();

        let a = alice_member.id;
        let owners = || async {
            store
                .list_members(org.id)
                .await
                .unwrap()
                .into_iter()
                .filter(|member| member.role == MemberRole::Owner)
                .count()
        };
        let restore = || async {
            sqlx::query("UPDATE members SET role = 'owner' WHERE organization_id = ?")
                .bind(org.id.uuid())
                .execute(database.pool())
                .await
                .unwrap();
        };
        for _ in 0..20 {
            // Each steps down: one wins, and the other is then the last owner.
            let (first, second) = tokio::join!(
                store.update_member_role(org.id, a, MemberRole::Admin, a),
                store.update_member_role(org.id, bob_member, MemberRole::Admin, bob_member),
            );
            let mut outcomes = [first.map(|_| ()), second.map(|_| ())];
            outcomes.sort_by_key(Result::is_err);
            assert_eq!(
                outcomes,
                [Ok(()), Err(StoreError::LastOwner)],
                "refused by the rule, not a conflict"
            );
            assert_eq!(owners().await, 1);
            restore().await;

            // Each demotes the other: the loser is no longer an owner when its
            // change runs, which the transaction sees.
            let (first, second) = tokio::join!(
                store.update_member_role(org.id, bob_member, MemberRole::Admin, a),
                store.update_member_role(org.id, a, MemberRole::Admin, bob_member),
            );
            let mut outcomes = [first.map(|_| ()), second.map(|_| ())];
            outcomes.sort_by_key(Result::is_err);
            assert_eq!(outcomes, [Ok(()), Err(StoreError::NotAllowed)]);
            assert_eq!(owners().await, 1);
            restore().await;
        }
    }
}
