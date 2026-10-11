//! The audit log: who changed what in an organization, and when.
//!
//! An event is written by the store that makes the change, in the change's
//! own transaction, so a committed change always has its event and a refused
//! or rolled-back one never does. A store's changing methods take an
//! [`Audit`]: what to record, built by the caller (who knows the actor and
//! the product's vocabulary) from what the change produced (the IDs and
//! names as committed). The store records it with the organization it was
//! given and, for an event in a workspace, the workspace's environment as
//! stored.
//!
//! The log keeps history: an event of a workspace outlives the workspace,
//! still naming its ID (the workspace reference is then `null`). Events go
//! only with their organization.
//!
//! Events never hold secrets or content: an actor, an action, a target's
//! type, ID and display name, and where it happened.

use async_trait::async_trait;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use uuid::Uuid;

use crate::environment::Environment;
use crate::ids::{
    AgentId, ApiKeyId, AppId, AuditEventId, InstallationId, MemberId, OrganizationId, RunId, UserId, WebhookDeliveryId,
    WebhookEndpointId, WorkspaceId,
};
use crate::time::Timestamp;
use crate::workspaces::Page;

/// The most events one page holds, and how many it holds by default.
pub const MAX_PAGE: u32 = 100;
pub const DEFAULT_PAGE: u32 = 50;

/// What an event records, from what the change produced. Called once, inside
/// the change's transaction, after the change and before it commits.
pub type Audit<'a, T> = dyn Fn(&T) -> AuditEntry + Send + Sync + 'a;

/// Who made a change. The name is kept as it was then, so the log still
/// reads after a person is renamed or leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditActor {
    User {
        id: UserId,
        name: String,
    },
    ApiKey {
        id: ApiKeyId,
        name: String,
    },
    /// Zupflow itself, named by the part that acted (`Connect`).
    System {
        name: String,
    },
}

impl AuditActor {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::ApiKey { .. } => "api_key",
            Self::System { .. } => "system",
        }
    }

    /// The user's or key's UUID, for storage; `None` for Zupflow itself.
    pub fn uuid(&self) -> Option<Uuid> {
        match self {
            Self::User { id, .. } => Some(id.uuid()),
            Self::ApiKey { id, .. } => Some(id.uuid()),
            Self::System { .. } => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::User { name, .. } | Self::ApiKey { name, .. } | Self::System { name } => name,
        }
    }

    /// From storage: `None` for a combination no store writes.
    pub fn from_stored(kind: &str, uuid: Option<Uuid>, name: String) -> Option<Self> {
        match (kind, uuid) {
            ("user", Some(uuid)) => Some(Self::User {
                id: UserId::from_uuid(uuid),
                name,
            }),
            ("api_key", Some(uuid)) => Some(Self::ApiKey {
                id: ApiKeyId::from_uuid(uuid),
                name,
            }),
            ("system", None) => Some(Self::System { name }),
            _ => None,
        }
    }
}

/// The contract's `AuditActor`: `{ type, id, name }`.
impl Serialize for AuditActor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut actor = serializer.serialize_struct("AuditActor", 3)?;
        actor.serialize_field("type", self.kind())?;
        match self {
            Self::User { id, .. } => actor.serialize_field("id", id)?,
            Self::ApiKey { id, .. } => actor.serialize_field("id", id)?,
            Self::System { .. } => actor.serialize_field("id", &None::<()>)?,
        }
        actor.serialize_field("name", self.name())?;
        actor.end()
    }
}

/// What a change was made to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditObject {
    Organization(OrganizationId),
    /// A member or an invitation.
    Member(MemberId),
    Workspace(WorkspaceId),
    App(AppId),
    Installation(InstallationId),
    Agent(AgentId),
    ApiKey(ApiKeyId),
    Webhook(WebhookEndpointId),
    WebhookDelivery(WebhookDeliveryId),
    Run(RunId),
    /// The organization's tool policy, which has no ID.
    ToolPolicy,
    /// An environment's Connect settings, which have no ID.
    ConnectSettings,
}

impl AuditObject {
    /// The target's `type`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Organization(_) => "organization",
            Self::Member(_) => "member",
            Self::Workspace(_) => "workspace",
            Self::App(_) => "app",
            Self::Installation(_) => "installation",
            Self::Agent(_) => "agent",
            Self::ApiKey(_) => "api_key",
            Self::Webhook(_) => "webhook",
            Self::WebhookDelivery(_) => "webhook_delivery",
            Self::Run(_) => "run",
            Self::ToolPolicy => "tool_policy",
            Self::ConnectSettings => "connect_settings",
        }
    }

    /// The object's UUID, for storage; `None` for one without an ID.
    pub fn uuid(&self) -> Option<Uuid> {
        match self {
            Self::Organization(id) => Some(id.uuid()),
            Self::Member(id) => Some(id.uuid()),
            Self::Workspace(id) => Some(id.uuid()),
            Self::App(id) => Some(id.uuid()),
            Self::Installation(id) => Some(id.uuid()),
            Self::Agent(id) => Some(id.uuid()),
            Self::ApiKey(id) => Some(id.uuid()),
            Self::Webhook(id) => Some(id.uuid()),
            Self::WebhookDelivery(id) => Some(id.uuid()),
            Self::Run(id) => Some(id.uuid()),
            Self::ToolPolicy | Self::ConnectSettings => None,
        }
    }

    /// From storage: `None` for a combination no store writes.
    pub fn from_stored(kind: &str, uuid: Option<Uuid>) -> Option<Self> {
        Some(match (kind, uuid) {
            ("organization", Some(uuid)) => Self::Organization(OrganizationId::from_uuid(uuid)),
            ("member", Some(uuid)) => Self::Member(MemberId::from_uuid(uuid)),
            ("workspace", Some(uuid)) => Self::Workspace(WorkspaceId::from_uuid(uuid)),
            ("app", Some(uuid)) => Self::App(AppId::from_uuid(uuid)),
            ("installation", Some(uuid)) => Self::Installation(InstallationId::from_uuid(uuid)),
            ("agent", Some(uuid)) => Self::Agent(AgentId::from_uuid(uuid)),
            ("api_key", Some(uuid)) => Self::ApiKey(ApiKeyId::from_uuid(uuid)),
            ("webhook", Some(uuid)) => Self::Webhook(WebhookEndpointId::from_uuid(uuid)),
            ("webhook_delivery", Some(uuid)) => Self::WebhookDelivery(WebhookDeliveryId::from_uuid(uuid)),
            ("run", Some(uuid)) => Self::Run(RunId::from_uuid(uuid)),
            ("tool_policy", None) => Self::ToolPolicy,
            ("connect_settings", None) => Self::ConnectSettings,
            _ => return None,
        })
    }
}

/// The target of a change and the name it went by then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditTarget {
    pub object: AuditObject,
    pub name: String,
}

impl AuditTarget {
    pub fn new(object: AuditObject, name: impl Into<String>) -> Self {
        Self {
            object,
            name: name.into(),
        }
    }

    pub fn tool_policy() -> Self {
        Self::new(AuditObject::ToolPolicy, "Tool policy")
    }

    pub fn connect_settings(environment: Environment) -> Self {
        Self::new(
            AuditObject::ConnectSettings,
            format!("Connect ({})", environment.as_str()),
        )
    }

    /// An installation, named by its app and label as the Console shows it.
    pub fn installation(id: InstallationId, app_name: &str, label: &str) -> Self {
        Self::new(AuditObject::Installation(id), format!("{app_name} · {label}"))
    }
}

/// The contract's `AuditTarget`: `{ type, id, name }`.
impl Serialize for AuditTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut target = serializer.serialize_struct("AuditTarget", 3)?;
        target.serialize_field("type", self.object.kind())?;
        match &self.object {
            AuditObject::Organization(id) => target.serialize_field("id", id)?,
            AuditObject::Member(id) => target.serialize_field("id", id)?,
            AuditObject::Workspace(id) => target.serialize_field("id", id)?,
            AuditObject::App(id) => target.serialize_field("id", id)?,
            AuditObject::Installation(id) => target.serialize_field("id", id)?,
            AuditObject::Agent(id) => target.serialize_field("id", id)?,
            AuditObject::ApiKey(id) => target.serialize_field("id", id)?,
            AuditObject::Webhook(id) => target.serialize_field("id", id)?,
            AuditObject::WebhookDelivery(id) => target.serialize_field("id", id)?,
            AuditObject::Run(id) => target.serialize_field("id", id)?,
            AuditObject::ToolPolicy | AuditObject::ConnectSettings => target.serialize_field("id", &None::<()>)?,
        }
        target.serialize_field("name", &self.name)?;
        target.end()
    }
}

macro_rules! actions {
    ($($(#[$doc:meta])* $variant:ident => $name:literal,)*) => {
        /// What was done. The names are the API's, `{object}.{verb}`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum AuditAction {
            $($(#[$doc])* $variant,)*
        }

        impl AuditAction {
            pub const ALL: &'static [AuditAction] = &[$(Self::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                match value {
                    $($name => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }
    };
}

actions! {
    OrganizationCreated => "organization.created",
    OrganizationUpdated => "organization.updated",
    MemberInvited => "member.invited",
    /// An invitation accepted, by the invited person.
    MemberJoined => "member.joined",
    /// An invitation declined, by the invited person.
    MemberDeclined => "member.declined",
    MemberRoleChanged => "member.role_changed",
    /// A member removed or an invitation withdrawn.
    MemberRemoved => "member.removed",
    WorkspaceCreated => "workspace.created",
    WorkspaceUpdated => "workspace.updated",
    WorkspaceDeleted => "workspace.deleted",
    AppCreated => "app.created",
    AppUpdated => "app.updated",
    AppToolLevelsUpdated => "app.tool_levels_updated",
    AppConfigurationUpdated => "app_configuration.updated",
    ToolPolicyUpdated => "tool_policy.updated",
    /// Renamed.
    InstallationUpdated => "installation.updated",
    InstallationConnected => "installation.connected",
    InstallationReconnected => "installation.reconnected",
    InstallationReconnectRequired => "installation.reconnect_required",
    InstallationDisconnected => "installation.disconnected",
    InstallationToolChangeAccepted => "installation.tool_change_accepted",
    InstallationDeleted => "installation.deleted",
    AgentCreated => "agent.created",
    AgentUpdated => "agent.updated",
    AgentPaused => "agent.paused",
    AgentResumed => "agent.resumed",
    AgentDeleted => "agent.deleted",
    AgentGrantCreated => "agent_grant.created",
    AgentGrantUpdated => "agent_grant.updated",
    AgentGrantRevoked => "agent_grant.revoked",
    ConnectSettingsUpdated => "connect_settings.updated",
    ApiKeyCreated => "api_key.created",
    ApiKeyRevoked => "api_key.revoked",
    WebhookCreated => "webhook.created",
    /// Changed: its URL, events, description, or enabled.
    WebhookUpdated => "webhook.updated",
    WebhookDeleted => "webhook.deleted",
    WebhookDeliveryResent => "webhook.delivery_resent",
    /// A decision on a call a run waited for, recorded by an API key.
    ApprovalResolved => "approval.resolved",
    RunCancelled => "run.cancelled",
}

impl Serialize for AuditAction {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Where a change happened, below its organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditPlace {
    /// The organization as a whole.
    Organization,
    /// One of the organization's environments.
    Environment(Environment),
    /// A workspace of the organization; the event takes its environment
    /// from the workspace as stored. A workspace the organization doesn't
    /// hold fails the change, and so does any place but its own workspace
    /// for a change made in one.
    Workspace(WorkspaceId),
}

/// An event to record, without its organization: the store takes that from
/// its own scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub actor: AuditActor,
    pub action: AuditAction,
    pub target: AuditTarget,
    pub place: AuditPlace,
}

impl AuditEntry {
    pub fn new(actor: AuditActor, action: AuditAction, target: AuditTarget, place: AuditPlace) -> Self {
        Self {
            actor,
            action,
            target,
            place,
        }
    }
}

/// The contract's `WorkspaceRef`, for a workspace that still exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditWorkspace {
    pub id: WorkspaceId,
    pub name: String,
    pub slug: String,
    pub environment: Environment,
}

/// The contract's `AuditEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEvent {
    pub id: AuditEventId,
    pub organization_id: OrganizationId,
    /// Kept after the workspace is deleted.
    pub workspace_id: Option<WorkspaceId>,
    /// The workspace as it is now; `None` once it's deleted.
    pub workspace: Option<AuditWorkspace>,
    pub environment: Option<Environment>,
    pub actor: AuditActor,
    pub action: AuditAction,
    pub target: AuditTarget,
    pub created_at: Timestamp,
}

/// What a list asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Only this workspace's events, whether it still exists or not.
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    #[error("the store is unavailable")]
    Unavailable,
    /// A cursor this store didn't issue.
    #[error("not a cursor from this list")]
    InvalidCursor,
}

#[async_trait]
pub trait AuditStore: Send + Sync {
    /// A page of the organization's events, newest first, at most `limit`
    /// (clamped to 1–[`MAX_PAGE`]). A cursor names a position only: used in
    /// another organization it starts there among that organization's own.
    async fn list_audit_events(
        &self,
        organization_id: OrganizationId,
        filter: &AuditFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<AuditEvent>, AuditError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_every_action_once() {
        let mut names: Vec<&str> = AuditAction::ALL.iter().map(|action| action.as_str()).collect();
        for action in AuditAction::ALL {
            assert_eq!(AuditAction::parse(action.as_str()), Some(*action));
        }
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), AuditAction::ALL.len());
        assert_eq!(AuditAction::parse("run.started"), None);
    }

    #[test]
    fn serializes_as_the_contract_writes_it() {
        let user = UserId::from_uuid(Uuid::from_u128(1));
        let actor = AuditActor::User {
            id: user,
            name: "Jordan".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(&actor).ok(),
            Some(serde_json::json!({ "type": "user", "id": user.to_string(), "name": "Jordan" }))
        );
        assert_eq!(
            serde_json::to_value(AuditActor::System {
                name: "Connect".to_owned()
            })
            .ok(),
            Some(serde_json::json!({ "type": "system", "id": null, "name": "Connect" }))
        );
        assert_eq!(
            serde_json::to_value(AuditTarget::connect_settings(Environment::Sandbox)).ok(),
            Some(serde_json::json!({ "type": "connect_settings", "id": null, "name": "Connect (sandbox)" }))
        );
        let hook = WebhookEndpointId::from_uuid(Uuid::from_u128(2));
        assert_eq!(
            serde_json::to_value(AuditTarget::new(AuditObject::Webhook(hook), "https://hooks.example")).ok(),
            Some(serde_json::json!({ "type": "webhook", "id": hook.to_string(), "name": "https://hooks.example" }))
        );
    }

    #[test]
    fn reads_back_what_it_stores() {
        let uuid = Uuid::from_u128(7);
        for object in [
            AuditObject::Organization(OrganizationId::from_uuid(uuid)),
            AuditObject::Member(MemberId::from_uuid(uuid)),
            AuditObject::Workspace(WorkspaceId::from_uuid(uuid)),
            AuditObject::App(AppId::from_uuid(uuid)),
            AuditObject::Installation(InstallationId::from_uuid(uuid)),
            AuditObject::Agent(AgentId::from_uuid(uuid)),
            AuditObject::ApiKey(ApiKeyId::from_uuid(uuid)),
            AuditObject::Webhook(WebhookEndpointId::from_uuid(uuid)),
            AuditObject::WebhookDelivery(WebhookDeliveryId::from_uuid(uuid)),
            AuditObject::Run(RunId::from_uuid(uuid)),
            AuditObject::ToolPolicy,
            AuditObject::ConnectSettings,
        ] {
            assert_eq!(AuditObject::from_stored(object.kind(), object.uuid()), Some(object));
        }
        assert_eq!(AuditObject::from_stored("tool_policy", Some(uuid)), None);
        assert_eq!(AuditObject::from_stored("member", None), None);
        assert_eq!(AuditActor::from_stored("system", Some(uuid), "x".to_owned()), None);
    }
}
