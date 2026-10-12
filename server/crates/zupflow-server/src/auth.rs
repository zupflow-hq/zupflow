//! The trust context: who is calling, then which organization (and
//! environment) they act in. Both are established before anything else, so a
//! caller without a valid credential gets `401` before any ID is parsed, and an
//! organization the caller can't act in is `404`, exactly like one that
//! doesn't exist.
//!
//! Two callers: a Console user, signed in through the identity adapter, and an
//! organization's backend with an API key (`zf_…`). A key belongs to one
//! organization and one environment and acts with every permission inside
//! them; it sees nothing of the other environment.

use std::sync::Arc;

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use zupflow_core::api_keys::{ApiKey, ApiKeyHasher, ApiKeyStore, SECRET_PREFIX};
use zupflow_core::apps::AppStore;
use zupflow_core::audit::{AuditActor, AuditStore};
use zupflow_core::connect::ConnectStore;
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::idempotency::{Fingerprinter, IdempotencyStore};
use zupflow_core::identity::{Identity, IdentityError};
use zupflow_core::ids::OrganizationId;
use zupflow_core::ids::WorkspaceId;
use zupflow_core::installations::InstallationStore;
use zupflow_core::keys::KeyService;
use zupflow_core::organizations::{Member, MemberRole, Organization, OrganizationStore, StoreError, User};
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::workspaces::{Workspace, WorkspaceError, WorkspaceStore};

use crate::http::Failure;

/// What every route reaches its adapters through.
#[derive(Clone)]
pub struct AppState {
    pub identity: Arc<dyn Identity>,
    pub organizations: Arc<dyn OrganizationStore>,
    pub workspaces: Arc<dyn WorkspaceStore>,
    pub apps: Arc<dyn AppStore>,
    pub installations: Arc<dyn InstallationStore>,
    /// Agents and their grants, held to the tool registry (decision 0040).
    pub agents: Arc<dyn zupflow_core::agents::AgentStore>,
    /// The organization's tool policy and its levels (decision 0040).
    pub policies: Arc<dyn zupflow_core::policy::ToolPolicyStore>,
    /// The models agents can use.
    pub models: Arc<dyn zupflow_core::models::ModelCatalog>,
    pub connect: Arc<dyn ConnectStore>,
    /// Seals the secrets Zupflow holds (decision 0033).
    pub keys: Arc<dyn KeyService>,
    /// Background work: tool discovery and runs.
    pub jobs: Arc<dyn zupflow_core::jobs::JobQueue>,
    /// The tools apps' MCP servers list, pinned (decision 0038).
    pub tools: Arc<dyn zupflow_core::tools::ToolRegistry>,
    /// Short-lived values that can be rebuilt: refresh pacing, for now.
    pub cache: Arc<dyn zupflow_core::storage::Cache>,
    /// Every call to a provider goes through it (`SafeHttpClient` in a deployment).
    pub outbound: Arc<dyn OutboundHttp>,
    /// The origin providers and browsers reach this server at, with no
    /// trailing slash: OAuth redirect URIs are built on it.
    pub public_url: String,
    pub api_keys: Arc<dyn ApiKeyStore>,
    pub api_key_hasher: Arc<ApiKeyHasher>,
    pub idempotency: Arc<dyn IdempotencyStore>,
    pub fingerprinter: Arc<Fingerprinter>,
    /// Runs, their timelines and their calls (milestone 4).
    pub runs: Arc<dyn zupflow_core::runs::RunStore>,
    /// Answers each turn of a run: the scripted model, unless a deployment
    /// brings another.
    pub provider: Arc<dyn zupflow_core::models::ModelProvider>,
    /// Decides each tool call, inside the floor (decision 0040).
    pub approvals: Arc<dyn zupflow_core::policy::ApprovalPolicy>,
    /// Announces run events to listeners in this process.
    pub events: Arc<dyn zupflow_core::events::EventBus>,
    /// Webhook endpoints, their events and deliveries (decision 0017).
    pub webhooks: Arc<dyn zupflow_core::webhooks::WebhookStore>,
    /// The audit log, which the other stores write with each change.
    pub audit: Arc<dyn AuditStore>,
    /// How the Console signs in, as `GET /v1/sign-in-settings` tells it:
    /// the same mode `identity` verifies.
    pub sign_in: crate::sign_in::SignInSettings,
    /// The time the run engine works by: the system's, except in tests that
    /// move it themselves.
    pub clock: Arc<dyn zupflow_core::time::Clock>,
}

/// A store failure the caller can do nothing about: the database is a
/// required dependency, so `503`. The store has logged the cause.
pub fn unavailable(_: StoreError) -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// Who is calling.
pub enum Caller {
    Console(User),
    ApiKey(ApiKey),
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if let Some(secret) = bearer.filter(|bearer| bearer.starts_with(SECRET_PREFIX)) {
            // The one cross-tenant lookup: the key names its organization.
            let hash = state.api_key_hasher.hash(secret);
            return match state.api_keys.find_active_api_key_by_hash(&hash).await {
                Ok(Some(key)) => Ok(Self::ApiKey(key)),
                Ok(None) => Err(ApiError::unauthenticated().into()),
                Err(_) => Err(ApiError::new(
                    503,
                    "unavailable",
                    "The service is unavailable right now. Try again shortly.",
                )
                .into()),
            };
        }
        console_session(bearer, state)
            .await
            .map(|session| Self::Console(session.user))
    }
}

/// A signed-in Console user, with what this sign-in says of their address
/// now. Invitations go by the user's address, so only a sign-in that vouches
/// for it sees or answers one.
pub struct ConsoleSession {
    pub user: User,
    pub address: SignInAddress,
}

/// What a sign-in says of its user's address, the one invitations go to.
pub enum SignInAddress {
    /// The provider verified the email it sent, and it's the user's own.
    Vouched,
    /// The provider sent the user's address, but hasn't verified it.
    Unverified,
    /// The provider sends another address now: the identity was linked under
    /// the user's, which never follows the profile.
    Changed,
}

impl FromRequestParts<AppState> for ConsoleSession {
    type Rejection = Failure;

    /// Like [`ConsoleUser`]: `401` without a valid credential, `403
    /// console_only` for a valid API key.
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        if bearer.is_some_and(|bearer| bearer.starts_with(SECRET_PREFIX)) {
            // A key is never a Console session: an unknown one is `401`, a valid one refused.
            Caller::from_request_parts(parts, state).await?;
            return Err(console_only());
        }
        console_session(bearer, state).await
    }
}

async fn console_session(bearer: Option<&str>, state: &AppState) -> Result<ConsoleSession, Failure> {
    let identity = state.identity.verify(bearer).await.map_err(|error| match error {
        IdentityError::Unauthenticated => Failure(ApiError::unauthenticated()),
        IdentityError::Unavailable => {
            tracing::error!(event = "auth.identity_unavailable");
            Failure(ApiError::new(
                503,
                "identity_unavailable",
                "Sign-in is unavailable right now. Try again shortly.",
            ))
        }
    })?;
    match state.organizations.user_for_identity(&identity).await {
        Ok(user) => {
            let address = if identity.profile.email.to_lowercase() != user.email {
                SignInAddress::Changed
            } else if identity.profile.email_verified {
                SignInAddress::Vouched
            } else {
                SignInAddress::Unverified
            };
            Ok(ConsoleSession { user, address })
        }
        Err(StoreError::EmailUnverified) => {
            tracing::warn!(event = "auth.email_unverified", provider = %identity.provider);
            Err(ApiError::new(
                401,
                "email_unverified",
                "Verify your email address with your sign-in provider, then sign in again.",
            )
            .into())
        }
        Err(StoreError::EmailInUse) => {
            tracing::warn!(event = "auth.email_in_use", provider = %identity.provider);
            Err(ApiError::new(
                401,
                "email_in_use",
                "Another sign-in already uses this email address. Sign in the way you did before.",
            )
            .into())
        }
        Err(error) => Err(unavailable(error)),
    }
}

fn console_only() -> Failure {
    ApiError::new(
        403,
        "console_only",
        "This route is for Console sessions; an API key can't call it.",
    )
    .into()
}

/// The signed-in Console user, for Console-only routes. A valid API key gets
/// `403`: its credential is fine, its type isn't allowed here.
pub struct ConsoleUser(pub User);

impl FromRequestParts<AppState> for ConsoleUser {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match Caller::from_request_parts(parts, state).await? {
            Caller::Console(user) => Ok(Self(user)),
            Caller::ApiKey(_) => Err(console_only()),
        }
    }
}

/// A Console user acting in one organization they're an active member of.
pub struct OrganizationAccess {
    pub user: User,
    pub organization: Organization,
    pub member: Member,
}

impl OrganizationAccess {
    /// Resolves the organization a route names. A malformed ID, a missing
    /// organization, and one the user isn't an active member of (an
    /// invitation included) all answer the same `404`.
    pub async fn resolve(state: &AppState, user: User, organization: &str) -> Result<Self, Failure> {
        Self::resolve_hiding_as(state, user, organization, "Organization").await
    }

    /// Like [`OrganizationAccess::resolve`], with the `404` naming `what`: a
    /// workspace route says the workspace wasn't found, whichever was missing.
    async fn resolve_hiding_as(state: &AppState, user: User, organization: &str, what: &str) -> Result<Self, Failure> {
        let not_found = || Failure(ApiError::not_found(what));
        let organization_id = OrganizationId::parse(organization).map_err(|_| not_found())?;
        let (organization, member) = state
            .organizations
            .find_active_membership(organization_id, user.id)
            .await
            .map_err(unavailable)?
            .ok_or_else(not_found)?;
        Ok(Self {
            user,
            organization,
            member,
        })
    }

    /// Who the audit log says made a change: this person, by their name now.
    pub fn audit_actor(&self) -> AuditActor {
        user_actor(&self.user)
    }

    /// `403` unless the caller's role is one of `roles`.
    pub fn require(&self, allowed: impl Fn(MemberRole) -> bool, action: &str) -> Result<(), Failure> {
        if allowed(self.member.role) {
            return Ok(());
        }
        Err(ApiError::forbidden(format!(
            "Your role ({}) can't {action}. Ask an organization owner.",
            self.member.role.as_str()
        ))
        .into())
    }
}

/// A signed-in person as the audit log names them.
pub fn user_actor(user: &User) -> AuditActor {
    AuditActor::User {
        id: user.id,
        name: user.name.clone(),
    }
}

/// The header a Console request names its organization with on workspace
/// routes (decision 0023).
pub const ORGANIZATION_HEADER: &str = "Zupflow-Organization";

/// A store failure the caller can do nothing about. The store has logged it.
pub fn workspace_unavailable(_: WorkspaceError) -> Failure {
    ApiError::new(
        503,
        "unavailable",
        "The service is unavailable right now. Try again shortly.",
    )
    .into()
}

/// Who acts, once the organization is established.
pub enum Actor {
    Member(OrganizationAccess),
    /// Every permission, in its organization and environment only.
    ApiKey(ApiKey),
}

/// Any caller acting in one organization: a member in it, or a key of it.
pub struct Access {
    pub organization_id: OrganizationId,
    pub actor: Actor,
}

impl Access {
    /// The organization a route names. A key reaches only its own; anything
    /// else answers the same `404` as for a non-member.
    pub async fn organization(state: &AppState, caller: Caller, organization: &str) -> Result<Self, Failure> {
        Self::organization_hiding_as(state, caller, organization, "Organization").await
    }

    async fn organization_hiding_as(
        state: &AppState,
        caller: Caller,
        organization: &str,
        what: &str,
    ) -> Result<Self, Failure> {
        match caller {
            Caller::Console(user) => {
                let access = OrganizationAccess::resolve_hiding_as(state, user, organization, what).await?;
                Ok(Self {
                    organization_id: access.organization.id,
                    actor: Actor::Member(access),
                })
            }
            Caller::ApiKey(key) => {
                if OrganizationId::parse(organization).ok() != Some(key.organization_id) {
                    return Err(ApiError::not_found(what).into());
                }
                Ok(Self {
                    organization_id: key.organization_id,
                    actor: Actor::ApiKey(key),
                })
            }
        }
    }

    /// `403` unless a member's role is allowed; a key has every permission.
    pub fn require(&self, allowed: impl Fn(MemberRole) -> bool, action: &str) -> Result<(), Failure> {
        match &self.actor {
            Actor::Member(access) => access.require(allowed, action),
            Actor::ApiKey(_) => Ok(()),
        }
    }

    /// Who the audit log says made a change: the member, or the key by its name.
    pub fn audit_actor(&self) -> AuditActor {
        match &self.actor {
            Actor::Member(access) => access.audit_actor(),
            Actor::ApiKey(key) => AuditActor::ApiKey {
                id: key.id,
                name: key.name.clone(),
            },
        }
    }

    /// The one environment a key acts in; `None` for a member, who acts in both.
    pub fn environment(&self) -> Option<Environment> {
        match &self.actor {
            Actor::Member(_) => None,
            Actor::ApiKey(key) => Some(key.environment),
        }
    }
}

/// Any caller acting in one workspace.
pub struct WorkspaceAccess {
    pub access: Access,
    pub workspace: Workspace,
}

impl WorkspaceAccess {
    /// Establishes the organization from the credential, never from the
    /// workspace. A Console user names it with the `Zupflow-Organization`
    /// header (required: `400`) and must be an active member of it; a key
    /// belongs to one (a header naming another answers `404`). Then the
    /// workspace must be in that organization, and for a key in its
    /// environment. Every other case answers the same `404`.
    pub async fn resolve(
        state: &AppState,
        caller: Caller,
        headers: &axum::http::HeaderMap,
        workspace: &str,
    ) -> Result<Self, Failure> {
        let named = headers
            .get(ORGANIZATION_HEADER)
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.trim().is_empty());
        let access = match (&caller, named) {
            (Caller::Console(_), None) => {
                return Err(ApiError::new(
                    400,
                    "organization_required",
                    format!("Name the organization this request acts in with the {ORGANIZATION_HEADER} header."),
                )
                .into());
            }
            (Caller::Console(_), Some(organization)) => {
                Access::organization_hiding_as(state, caller, organization, "Workspace").await?
            }
            (Caller::ApiKey(key), _) => {
                // A key needn't name its organization; if it sends the header
                // anyway, it must name the key's own, or nothing is found.
                let own = key.organization_id.to_string();
                let named = match headers.get(ORGANIZATION_HEADER) {
                    None => own,
                    Some(value) => value.to_str().map(str::trim).unwrap_or_default().to_owned(),
                };
                Access::organization_hiding_as(state, caller, &named, "Workspace").await?
            }
        };
        let not_found = || Failure(ApiError::not_found("Workspace"));
        let workspace_id = WorkspaceId::parse(workspace).map_err(|_| not_found())?;
        let workspace = state
            .workspaces
            .find_workspace(access.organization_id, workspace_id)
            .await
            .map_err(workspace_unavailable)?
            .filter(|workspace| access.environment().is_none_or(|own| own == workspace.environment))
            .ok_or_else(not_found)?;
        Ok(Self { access, workspace })
    }
}
