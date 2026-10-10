//! [`SqliteOrganizationStore`]: organizations, members and users.
//!
//! Every statement on `members` repeats `organization_id`, even beside a
//! unique member ID. The user-keyed reads and changes (`user_for_identity`, a
//! user's own memberships and invitations) read global tables or the user's
//! own rows only: they find a row by the user and its ID, and every statement
//! after that repeats the organization it belongs to as well. Changes that
//! check before they write (the last-owner rule) run in an immediate
//! transaction, which takes SQLite's write lock first, so two changes can't
//! both pass the check.

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, Sqlite, SqliteConnection, Transaction};
use zupflow_core::identity::VerifiedIdentity;
use zupflow_core::ids::{MemberId, OrganizationId, UserId};
use zupflow_core::organizations::{
    EXPIRED_INVITATION_SHOWN_MS, INVITATION_LIST_LIMIT, INVITATION_TTL_MS, Invitation, Inviter, Member, MemberRole,
    MemberStatus, Organization, OrganizationSetup, OrganizationStore, StoreError, User, invitation_expires_at,
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

/// Whether the invitation `m` still stands: its grantor is an active member
/// of its organization who can grant its role (`MemberRole::can_grant`). A
/// null grantor matches no member, so it grants nothing.
macro_rules! granted {
    () => {
        "EXISTS (SELECT 1 FROM members g WHERE g.organization_id = m.organization_id \
         AND g.user_id = m.invited_by_user_id AND g.status = 'active' \
         AND (g.role = 'owner' OR (g.role = 'admin' AND m.role <> 'owner')))"
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

    /// Reads the invitation without the write lock first, so an ID that isn't
    /// the caller's to answer never contends for it. Only a hint: the answer
    /// is decided by the same read inside the transaction.
    async fn answerable(
        &self,
        user_id: UserId,
        invitation_id: MemberId,
        now: Timestamp,
        expiring: bool,
    ) -> Result<(), StoreError> {
        let mut connection = self.database.pool().acquire().await.map_err(failed("acquire"))?;
        let invitation = own_invitation(&mut connection, user_id, invitation_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        invitation.refusal(now, expiring).map_or(Ok(()), Err)
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
    let status = MemberStatus::parse(&status).ok_or(StoreError::Unavailable)?;
    let invited_at = Timestamp::from_millis(row.try_get("invited_at").map_err(read)?);
    Ok(Member {
        id: MemberId::from_uuid(row.try_get("member_id").map_err(read)?),
        organization_id: OrganizationId::from_uuid(row.try_get("member_organization_id").map_err(read)?),
        user: user_from(row)?,
        role: MemberRole::parse(&role).ok_or(StoreError::Unavailable)?,
        status,
        invited_at,
        expires_at: (status == MemberStatus::Invited).then(|| invitation_expires_at(invited_at)),
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

fn invitation_from(row: &SqliteRow) -> Result<Invitation, StoreError> {
    let role: String = row.try_get("role").map_err(read)?;
    let invited_at = Timestamp::from_millis(row.try_get("invited_at").map_err(read)?);
    Ok(Invitation {
        id: MemberId::from_uuid(row.try_get("member_id").map_err(read)?),
        organization: organization_from(row)?,
        role: MemberRole::parse(&role).ok_or(StoreError::Unavailable)?,
        invited_by: Inviter {
            name: row.try_get("inviter_name").map_err(read)?,
            email: row.try_get("inviter_email").map_err(read)?,
        },
        invited_at,
        expires_at: invitation_expires_at(invited_at),
    })
}

/// The user's own membership or invitation with this ID: its organization,
/// status, when it was sent and whether its grantor still grants it. Keyed by
/// the user, the trust context: anyone else's is `None`, like a missing one.
struct OwnInvitation {
    organization_id: OrganizationId,
    status: MemberStatus,
    invited_at: Timestamp,
    granted: bool,
}

impl OwnInvitation {
    /// What answering it would meet as of `now`: accepted already, withdrawn
    /// (its grantor can't grant it), or, when `expiring`, expired.
    fn refusal(&self, now: Timestamp, expiring: bool) -> Option<StoreError> {
        if self.status == MemberStatus::Active {
            Some(StoreError::InvitationAccepted)
        } else if !self.granted {
            Some(StoreError::NotFound)
        } else if expiring && invitation_expires_at(self.invited_at) <= now {
            Some(StoreError::InvitationExpired)
        } else {
            None
        }
    }
}

async fn own_invitation(
    connection: &mut SqliteConnection,
    user_id: UserId,
    invitation_id: MemberId,
) -> Result<Option<OwnInvitation>, StoreError> {
    let Some(row) = sqlx::query(concat!(
        "SELECT m.organization_id, m.status, m.invited_at, ",
        granted!(),
        " AS granted FROM members m WHERE m.user_id = ? AND m.id = ?"
    ))
    .bind(user_id.uuid())
    .bind(invitation_id.uuid())
    .fetch_optional(&mut *connection)
    .await
    .map_err(failed("find_invitation"))?
    else {
        return Ok(None);
    };
    let status: String = row.try_get("status").map_err(read)?;
    Ok(Some(OwnInvitation {
        organization_id: OrganizationId::from_uuid(row.try_get("organization_id").map_err(read)?),
        status: MemberStatus::parse(&status).ok_or(StoreError::Unavailable)?,
        invited_at: Timestamp::from_millis(row.try_get("invited_at").map_err(read)?),
        granted: row.try_get("granted").map_err(read)?,
    }))
}

/// The acting member as of this transaction: [`StoreError::NotAllowed`] if
/// they're no longer an active member of the organization.
async fn acting_member(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    acting: MemberId,
) -> Result<Member, StoreError> {
    match member_in(connection, organization_id, acting).await? {
        Some(member) if member.status == MemberStatus::Active => Ok(member),
        _ => Err(StoreError::NotAllowed),
    }
}

/// Withdraws the pending invitations `grantor` granted in the organization
/// that `role` can't grant: every one of them once they're removed (`None`).
async fn withdraw_ungrantable(
    connection: &mut SqliteConnection,
    organization_id: OrganizationId,
    grantor: UserId,
    role: Option<MemberRole>,
) -> Result<(), StoreError> {
    for granted in MemberRole::ALL {
        if role.is_some_and(|role| role.can_grant(granted)) {
            continue;
        }
        sqlx::query(
            "DELETE FROM members WHERE organization_id = ? AND invited_by_user_id = ? \
             AND status = 'invited' AND role = ?",
        )
        .bind(organization_id.uuid())
        .bind(grantor.uuid())
        .bind(granted.as_str())
        .execute(&mut *connection)
        .await
        .map_err(failed("withdraw_invitations"))?;
    }
    Ok(())
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
             AS has_ready_configuration, \
             EXISTS (SELECT 1 FROM installations WHERE organization_id = ?1 AND status = 'connected') \
             AS has_installation",
        )
        .bind(organization_id.uuid())
        .fetch_one(self.database.pool())
        .await
        .map_err(failed("load_setup"))?;
        // The rest arrive with their tables: agents and runs.
        Ok(OrganizationSetup {
            has_workspace: row.try_get("has_workspace").map_err(read)?,
            has_api_key: row.try_get("has_api_key").map_err(read)?,
            has_app: row.try_get("has_app").map_err(read)?,
            has_ready_configuration: row.try_get("has_ready_configuration").map_err(read)?,
            has_installation: row.try_get("has_installation").map_err(read)?,
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
        acting: MemberId,
    ) -> Result<Member, StoreError> {
        let email = email.to_lowercase();
        let mut transaction = self.immediate().await?;
        let grantor = acting_member(&mut transaction, organization_id, acting).await?;
        if !grantor.role.can_grant(role) {
            return Err(StoreError::NotAllowed);
        }
        let (user, _) =
            user_with_email(&mut transaction, &email, &bounded(&name_from_email(&email), 200), None).await?;
        let now = Timestamp::now();
        let existing =
            sqlx::query("SELECT id, status, invited_at FROM members WHERE organization_id = ? AND user_id = ?")
                .bind(organization_id.uuid())
                .bind(user.id.uuid())
                .fetch_optional(&mut *transaction)
                .await
                .map_err(failed("find_membership"))?;
        let member_id = match existing {
            None => {
                let member_id = MemberId::generate();
                sqlx::query(
                    "INSERT INTO members (id, organization_id, user_id, role, status, invited_at, invited_by_user_id) \
                     VALUES (?, ?, ?, ?, 'invited', ?, ?)",
                )
                .bind(member_id.uuid())
                .bind(organization_id.uuid())
                .bind(user.id.uuid())
                .bind(role.as_str())
                .bind(now.millis())
                .bind(grantor.user.id.uuid())
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    if is_unique_violation(&error) {
                        StoreError::AlreadyMember
                    } else {
                        failed("insert_member")(error)
                    }
                })?;
                member_id
            }
            Some(row) => {
                let status: String = row.try_get("status").map_err(read)?;
                let invited_at = Timestamp::from_millis(row.try_get("invited_at").map_err(read)?);
                if MemberStatus::parse(&status) != Some(MemberStatus::Invited)
                    || invitation_expires_at(invited_at) > now
                {
                    return Err(StoreError::AlreadyMember);
                }
                // Expired: sent again, as new, with this role and grantor.
                let member_id = MemberId::from_uuid(row.try_get("id").map_err(read)?);
                sqlx::query(
                    "UPDATE members SET role = ?, invited_at = ?, invited_by_user_id = ? \
                     WHERE organization_id = ? AND id = ? AND status = 'invited'",
                )
                .bind(role.as_str())
                .bind(now.millis())
                .bind(grantor.user.id.uuid())
                .bind(organization_id.uuid())
                .bind(member_id.uuid())
                .execute(&mut *transaction)
                .await
                .map_err(failed("renew_invitation"))?;
                member_id
            }
        };
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::Unavailable)?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(member)
    }

    async fn list_invitations_of_user(&self, user_id: UserId, now: Timestamp) -> Result<Vec<Invitation>, StoreError> {
        // Sent after this, it expired at most EXPIRED_INVITATION_SHOWN_MS ago.
        let sent_after = now
            .millis()
            .saturating_sub(INVITATION_TTL_MS)
            .saturating_sub(EXPIRED_INVITATION_SHOWN_MS);
        sqlx::query(concat!(
            "SELECT ",
            organization_columns!(),
            ", m.id AS member_id, m.role, m.invited_at, i.name AS inviter_name, i.email AS inviter_email \
             FROM members m JOIN organizations o ON o.id = m.organization_id \
             JOIN users i ON i.id = m.invited_by_user_id \
             WHERE m.user_id = ? AND m.status = 'invited' AND m.invited_at > ? AND ",
            granted!(),
            " ORDER BY m.invited_at DESC, m.id DESC LIMIT ?"
        ))
        .bind(user_id.uuid())
        .bind(sent_after)
        .bind(INVITATION_LIST_LIMIT)
        .fetch_all(self.database.pool())
        .await
        .map_err(failed("list_invitations"))?
        .iter()
        .map(invitation_from)
        .collect()
    }

    async fn accept_invitation_of_user(
        &self,
        user_id: UserId,
        invitation_id: MemberId,
        now: Timestamp,
    ) -> Result<(Organization, Member), StoreError> {
        self.answerable(user_id, invitation_id, now, true).await?;
        let mut transaction = self.immediate().await?;
        let invitation = own_invitation(&mut transaction, user_id, invitation_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if let Some(refusal) = invitation.refusal(now, true) {
            return Err(refusal);
        }
        let organization_id = invitation.organization_id;
        // The role stays the invitation's own as of now: an owner may have changed it since it was sent.
        let accepted = sqlx::query(
            "UPDATE members SET status = 'active', joined_at = ?, last_active_at = ? \
             WHERE organization_id = ? AND id = ? AND user_id = ? AND status = 'invited'",
        )
        .bind(now.millis())
        .bind(now.millis())
        .bind(organization_id.uuid())
        .bind(invitation_id.uuid())
        .bind(user_id.uuid())
        .execute(&mut *transaction)
        .await
        .map_err(failed("accept_invitation"))?;
        if accepted.rows_affected() != 1 {
            // The transaction holds the write lock since the read above, so this can't happen.
            tracing::error!(event = "organizations.invitation_changed_mid_transaction");
            return Err(StoreError::Unavailable);
        }
        let row = sqlx::query(concat!(
            "SELECT ",
            organization_columns!(),
            ", ",
            member_columns!(),
            " FROM members m \
             JOIN organizations o ON o.id = m.organization_id JOIN users u ON u.id = m.user_id \
             WHERE m.organization_id = ? AND o.id = ? AND m.id = ? AND m.user_id = ?"
        ))
        .bind(organization_id.uuid())
        .bind(organization_id.uuid())
        .bind(invitation_id.uuid())
        .bind(user_id.uuid())
        .fetch_one(&mut *transaction)
        .await
        .map_err(failed("read_membership"))?;
        let joined = (organization_from(&row)?, member_from(&row)?);
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(joined)
    }

    async fn decline_invitation_of_user(&self, user_id: UserId, invitation_id: MemberId) -> Result<(), StoreError> {
        let now = Timestamp::now();
        self.answerable(user_id, invitation_id, now, false).await?;
        let mut transaction = self.immediate().await?;
        let invitation = own_invitation(&mut transaction, user_id, invitation_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if let Some(refusal) = invitation.refusal(now, false) {
            return Err(refusal);
        }
        let organization_id = invitation.organization_id;
        sqlx::query("DELETE FROM members WHERE organization_id = ? AND id = ? AND user_id = ? AND status = 'invited'")
            .bind(organization_id.uuid())
            .bind(invitation_id.uuid())
            .bind(user_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("decline_invitation"))?;
        transaction.commit().await.map_err(failed("commit"))?;
        Ok(())
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
        let acting = acting_member(&mut transaction, organization_id, acting).await?;
        if acting.role != MemberRole::Owner {
            return Err(StoreError::NotAllowed);
        }
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if role != MemberRole::Owner && is_last_owner(&mut transaction, organization_id, &member).await? {
            return Err(StoreError::LastOwner);
        }
        if member.status == MemberStatus::Invited {
            // Whoever changes an invitation's role grants it now.
            sqlx::query(
                "UPDATE members SET role = ?, invited_by_user_id = ? WHERE organization_id = ? AND id = ? \
                 AND status = 'invited'",
            )
            .bind(role.as_str())
            .bind(acting.user.id.uuid())
            .bind(organization_id.uuid())
            .bind(member_id.uuid())
            .execute(&mut *transaction)
            .await
            .map_err(failed("update_role"))?;
        } else {
            sqlx::query("UPDATE members SET role = ? WHERE organization_id = ? AND id = ?")
                .bind(role.as_str())
                .bind(organization_id.uuid())
                .bind(member_id.uuid())
                .execute(&mut *transaction)
                .await
                .map_err(failed("update_role"))?;
            // A demotion withdraws what they can't grant any more.
            withdraw_ungrantable(&mut transaction, organization_id, member.user.id, Some(role)).await?;
        }
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
        only_invited: bool,
    ) -> Result<(), StoreError> {
        let mut transaction = self.immediate().await?;
        let acting_role = acting_member(&mut transaction, organization_id, acting).await?.role;
        if !acting_role.is_admin() {
            return Err(StoreError::NotAllowed);
        }
        let member = member_in(&mut transaction, organization_id, member_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        if only_invited && member.status != MemberStatus::Invited {
            return Err(StoreError::InvitationAccepted);
        }
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
        if member.status == MemberStatus::Active {
            // What they granted goes with them.
            withdraw_ungrantable(&mut transaction, organization_id, member.user.id, None).await?;
        }
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
        // Bob is invited as an owner and accepts.
        let invitation = store
            .invite_member(org.id, &bob.email, MemberRole::Owner, alice_member.id)
            .await
            .unwrap();
        let (_, bob_member) = store
            .accept_invitation_of_user(bob.id, invitation.id, Timestamp::now())
            .await
            .unwrap();
        let bob_member = bob_member.id;

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

    /// An invitation with no grantor recorded (sent before grantors were, or
    /// its grantor's user is gone) grants nothing: it isn't listed, and
    /// answering it meets the same `NotFound` as a withdrawn one, whatever
    /// its role.
    #[tokio::test]
    async fn treats_an_invitation_without_a_grantor_as_withdrawn() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteOrganizationStore::new(database.clone());
        let alice = store.user_for_identity(&person("alice")).await.unwrap();
        let bob = store.user_for_identity(&person("bob")).await.unwrap();
        let (org, alice_member) = store
            .create_organization(OrganizationId::generate(), "A", "org-a", alice.id)
            .await
            .unwrap();
        for role in [MemberRole::Owner, MemberRole::Viewer] {
            let invitation = store
                .invite_member(org.id, &bob.email, role, alice_member.id)
                .await
                .unwrap();
            sqlx::query("UPDATE members SET invited_by_user_id = NULL WHERE organization_id = ? AND id = ?")
                .bind(org.id.uuid())
                .bind(invitation.id.uuid())
                .execute(database.pool())
                .await
                .unwrap();
            assert!(
                store
                    .list_invitations_of_user(bob.id, Timestamp::now())
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                store
                    .accept_invitation_of_user(bob.id, invitation.id, Timestamp::now())
                    .await,
                Err(StoreError::NotFound)
            );
            assert_eq!(
                store.decline_invitation_of_user(bob.id, invitation.id).await,
                Err(StoreError::NotFound)
            );
            assert_eq!(store.find_active_membership(org.id, bob.id).await.unwrap(), None);
            // An admin still withdraws it.
            store
                .remove_member(org.id, invitation.id, alice_member.id, true)
                .await
                .unwrap();
        }
    }

    /// Inviting an address whose invitation expired sends it again, as new:
    /// this role, this grantor, from now. A pending one isn't sent twice.
    #[tokio::test]
    async fn renews_an_expired_invitation() {
        let Temporary {
            database,
            directory: _directory,
        } = temporary().await;
        let store = SqliteOrganizationStore::new(database.clone());
        let alice = store.user_for_identity(&person("alice")).await.unwrap();
        let bob = store.user_for_identity(&person("bob")).await.unwrap();
        let carol = store.user_for_identity(&person("carol")).await.unwrap();
        let (org, alice_member) = store
            .create_organization(OrganizationId::generate(), "A", "org-a", alice.id)
            .await
            .unwrap();
        let invitation = store
            .invite_member(org.id, &carol.email, MemberRole::Viewer, alice_member.id)
            .await
            .unwrap();
        assert_eq!(
            store
                .invite_member(org.id, &carol.email, MemberRole::Admin, alice_member.id)
                .await,
            Err(StoreError::AlreadyMember)
        );
        let invitation_b = store
            .invite_member(org.id, &bob.email, MemberRole::Admin, alice_member.id)
            .await
            .unwrap();
        let (_, bob_member) = store
            .accept_invitation_of_user(bob.id, invitation_b.id, Timestamp::now())
            .await
            .unwrap();
        sqlx::query("UPDATE members SET invited_at = 0 WHERE organization_id = ? AND id = ?")
            .bind(org.id.uuid())
            .bind(invitation.id.uuid())
            .execute(database.pool())
            .await
            .unwrap();
        let before = Timestamp::now();
        let renewed = store
            .invite_member(org.id, &carol.email, MemberRole::Developer, bob_member.id)
            .await
            .unwrap();
        assert_eq!(
            (renewed.id, renewed.role, renewed.status),
            (invitation.id, MemberRole::Developer, MemberStatus::Invited)
        );
        assert!(renewed.invited_at >= before);
        let listed = store
            .list_invitations_of_user(carol.id, Timestamp::now())
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].invited_by.email, bob.email, "bob granted it now");
        store
            .accept_invitation_of_user(carol.id, invitation.id, Timestamp::now())
            .await
            .unwrap();
        // A member isn't renewed into an invitation.
        assert_eq!(
            store
                .invite_member(org.id, &carol.email, MemberRole::Viewer, alice_member.id)
                .await,
            Err(StoreError::AlreadyMember)
        );
    }
}
