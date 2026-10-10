//! Tool permissions (decision 0040): the organization's tool policy, its
//! level for each tool of its catalog apps, and the [`ApprovalPolicy`] that
//! decides each call.
//!
//! Three layers decide a call, and the strictest wins: the installation's
//! registry ([`crate::tools`]), the organization's level for the tool, and
//! the agent's grant. A fixed **floor** comes first and no policy can widen
//! it: a tool that isn't granted, isn't in the registry, is gone or that the
//! organization denies is refused, and one whose level can't be read waits
//! for approval ([`floor`]).
//!
//! The organization keeps one policy record (`mode`, and the level of a tool
//! nobody confirmed a level for) and one level per catalog app and tool name.
//! A level is suggested from a tool's hints when an installation first lists
//! it and is in force only once a person confirms it; until then, and while
//! a tool's change waits for review, the policy's `new_tool_level` applies,
//! which is never `allow`. A workspace app's tools have no organization
//! level: its grants alone decide, inside the floor.
//!
//! Approval is a replaceable strategy: [`ApprovalPolicy`]. The built-in one,
//! `ConfiguredPolicy` in `zupflow-basic`, reads this module's
//! [`ToolPolicyStore`]; a deployment can replace it whole.

use std::collections::BTreeMap;

use async_trait::async_trait;

use crate::apps::ToolEffect;
use crate::audit::Audit;
use crate::ids::{AgentId, AppId, InstallationId, OrganizationId, UserId, WorkspaceId};
use crate::organizations::User;
use crate::time::Timestamp;
use crate::tools::ToolStatus;

/// The most levels one change sets.
pub const MAX_LEVELS_PER_CHANGE: usize = 100;

/// The longest tool name a level is kept for, as the registry's.
pub const MAX_TOOL_NAME: usize = crate::tools::MAX_NAME;

/// The largest page [`ToolPolicyStore::list_levels`] answers.
pub const MAX_PAGE: u32 = 100;

/// What the organization lets a tool do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLevel {
    /// Runs without asking.
    Allow,
    /// Pauses the run until a person decides.
    Approve,
    /// Kept out of every grant and refused when called.
    Deny,
}

impl ToolLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "allow" => Some(Self::Allow),
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    /// The level suggested from a tool's hints: read-only runs, anything else asks.
    pub fn suggested(effect: ToolEffect) -> Self {
        match effect {
            ToolEffect::Read => Self::Allow,
            ToolEffect::Write | ToolEffect::Destructive => Self::Approve,
        }
    }
}

/// Who decides a call that needs approval. Only `manual` so far; `auto` later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// A person, in the customer's product.
    Manual,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }
}

/// The level of a tool nobody confirmed a level for: never `allow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewToolLevel {
    Approve,
    Deny,
}

impl NewToolLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Deny => "deny",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }

    pub fn level(self) -> ToolLevel {
        match self {
            Self::Approve => ToolLevel::Approve,
            Self::Deny => ToolLevel::Deny,
        }
    }
}

/// The policy's settings, as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicySettings {
    pub mode: ApprovalMode,
    pub new_tool_level: NewToolLevel,
}

impl Default for PolicySettings {
    /// What an organization that never saved its policy reads.
    fn default() -> Self {
        Self {
            mode: ApprovalMode::Manual,
            new_tool_level: NewToolLevel::Approve,
        }
    }
}

/// The organization's tool policy, one for both environments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPolicy {
    pub organization_id: OrganizationId,
    pub settings: PolicySettings,
    /// `None` until someone saves it.
    pub updated_at: Option<Timestamp>,
    /// Who saved it last; `None` before that, or once they're gone.
    pub updated_by: Option<User>,
}

/// The organization's level for one tool of a catalog app, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppToolLevel {
    pub app_id: AppId,
    pub tool: String,
    pub level: ToolLevel,
    /// Set by a person; only a confirmed level is in force.
    pub confirmed: bool,
    /// The tool's hints said it may destroy data.
    pub destructive: bool,
    pub updated_at: Timestamp,
    /// Who confirmed it; `None` while suggested, or once they're gone.
    pub updated_by: Option<User>,
}

impl AppToolLevel {
    /// The level in force: the confirmed one, else the policy's level for new tools.
    pub fn in_force(&self, new_tool_level: NewToolLevel) -> ToolLevel {
        if self.confirmed {
            self.level
        } else {
            new_tool_level.level()
        }
    }
}

/// The level in force for a catalog app's tool the organization may have a
/// level for: its confirmed level, else the policy's level for new tools.
pub fn level_in_force(level: Option<&AppToolLevel>, new_tool_level: NewToolLevel) -> ToolLevel {
    level.map_or(new_tool_level.level(), |level| level.in_force(new_tool_level))
}

/// One page of an app's levels, by tool name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelPage {
    pub items: Vec<AppToolLevel>,
    /// The last tool's name when more follow: the next page's `after`.
    pub next_after: Option<String>,
    /// How many levels the app has in all.
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("the store is unavailable")]
    Unavailable,
    /// The app isn't one of the organization's catalog apps.
    #[error("app not found")]
    AppNotFound,
    /// No levels, more than [`MAX_LEVELS_PER_CHANGE`], a name empty, longer
    /// than [`MAX_TOOL_NAME`] or repeated.
    #[error("the levels can't be set")]
    InvalidLevels,
}

/// The organization's tool policy and levels. Every method takes the
/// organization first; levels are kept only for the organization's catalog
/// apps.
#[async_trait]
pub trait ToolPolicyStore: Send + Sync {
    /// The organization's policy; the defaults when it was never saved.
    async fn load_policy(&self, organization_id: OrganizationId) -> Result<ToolPolicy, PolicyError>;

    /// Saves the policy, as written by `updated_by` (`None` for an API key),
    /// recording `audit` of it as saved.
    async fn save_policy(
        &self,
        organization_id: OrganizationId,
        settings: PolicySettings,
        updated_by: Option<UserId>,
        now: Timestamp,
        audit: &Audit<'_, ToolPolicy>,
    ) -> Result<ToolPolicy, PolicyError>;

    /// A page of the app's levels, by tool name (byte order): those whose
    /// name sorts after `after`, at most `limit` (clamped to 1 to
    /// [`MAX_PAGE`]). `AppNotFound` unless it's one of the organization's
    /// catalog apps.
    async fn list_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<LevelPage, PolicyError>;

    /// The app's levels for these tool names, those it has. `AppNotFound`
    /// unless it's one of the organization's catalog apps.
    async fn find_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        tools: &[String],
    ) -> Result<Vec<AppToolLevel>, PolicyError>;

    /// Sets levels by tool name, each confirmed by `updated_by`, in one
    /// transaction; answers the levels written, by tool name. A name no
    /// installation listed yet can be set ahead of it. Checked with
    /// [`validate_levels`] first (`InvalidLevels`). Records `audit` of the
    /// levels written.
    async fn set_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        levels: &[(String, ToolLevel)],
        updated_by: Option<UserId>,
        now: Timestamp,
        audit: &Audit<'_, Vec<AppToolLevel>>,
    ) -> Result<Vec<AppToolLevel>, PolicyError>;

    /// Suggests a level from its hints for each listed tool the app has no
    /// level for yet, unconfirmed; never changes an existing one. Answers
    /// how many it suggested. `AppNotFound` unless it's one of the
    /// organization's catalog apps.
    async fn suggest_levels(
        &self,
        organization_id: OrganizationId,
        app_id: AppId,
        listed: &[(String, ToolEffect)],
        now: Timestamp,
    ) -> Result<u64, PolicyError>;
}

/// Checks a change of levels against the limits every store holds it to.
pub fn validate_levels(levels: &[(String, ToolLevel)]) -> Result<(), PolicyError> {
    if levels.is_empty() || levels.len() > MAX_LEVELS_PER_CHANGE {
        return Err(PolicyError::InvalidLevels);
    }
    let mut names = std::collections::HashSet::new();
    for (tool, _) in levels {
        if tool.is_empty() || tool.chars().count() > MAX_TOOL_NAME || !names.insert(tool.as_str()) {
            return Err(PolicyError::InvalidLevels);
        }
    }
    Ok(())
}

/// The organization's levels a grant is held to, by tool name: empty for a
/// workspace app's installation, whose grants alone decide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrantLevels {
    levels: BTreeMap<String, ToolLevel>,
}

impl GrantLevels {
    /// No organization levels: a workspace app's.
    pub fn none() -> Self {
        Self::default()
    }

    /// The level in force for each of `tools`, given the levels the
    /// organization has for them and its policy's level for new tools.
    pub fn in_force(tools: &[String], levels: &[AppToolLevel], new_tool_level: NewToolLevel) -> Self {
        let levels = tools
            .iter()
            .map(|tool| {
                let level = levels.iter().find(|level| &level.tool == tool);
                (tool.clone(), level_in_force(level, new_tool_level))
            })
            .collect();
        Self { levels }
    }

    pub fn of(&self, tool: &str) -> Option<ToolLevel> {
        self.levels.get(tool).copied()
    }
}

/// Why a grant isn't held inside the organization's levels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LevelRefusal {
    /// The first tool given that the organization denies.
    Denied(String),
    /// The first tool given that the organization sets to approve but the grant doesn't require approval for.
    ApprovalRequired(String),
}

/// Holds a grant's tools inside the organization's levels: every store
/// checks it, after the registry's rules and before one grant per agent and
/// installation.
pub fn level_refusal(tools: &[String], approval_required: &[String], levels: &GrantLevels) -> Option<LevelRefusal> {
    if let Some(tool) = tools.iter().find(|tool| levels.of(tool) == Some(ToolLevel::Deny)) {
        return Some(LevelRefusal::Denied(tool.clone()));
    }
    tools
        .iter()
        .find(|tool| levels.of(tool) == Some(ToolLevel::Approve) && !approval_required.contains(tool))
        .map(|tool| LevelRefusal::ApprovalRequired(tool.clone()))
}

/// What kind of app a call's installation is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallApp {
    /// One of the organization's catalog apps: its levels apply.
    Catalog(AppId),
    /// A workspace's own app: no organization levels.
    Workspace(AppId),
}

/// The tool as the agent's grant holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantedAs {
    NotGranted,
    Granted {
        /// The grant's `approval_required` names it.
        approval_required: bool,
    },
}

/// One call an agent's run is about to make, as the run engine resolved it
/// within its organization and workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub agent_id: AgentId,
    pub installation_id: InstallationId,
    pub app: CallApp,
    pub tool: String,
    /// The tool in the installation's registry; `None` when it isn't there.
    pub status: Option<ToolStatus>,
    pub grant: GrantedAs,
    /// The organization's level for the tool, as the run engine read it.
    pub level: OrganizationLevel,
}

/// The organization's level for a call's tool, as the run engine read it
/// from the [`ToolPolicyStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrganizationLevel {
    /// A workspace app's tool: the organization keeps no level for it.
    NotApplicable,
    /// A catalog app's tool nobody set or suggested a level for yet.
    Unset,
    /// A catalog app's tool with a level, in force only once confirmed.
    Set { level: ToolLevel, confirmed: bool },
    /// It couldn't be read: the floor requires approval.
    Unreadable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    RequireApproval { reason: String },
    Deny { reason: String },
}

/// The rule that made a decision, recorded with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Floor: the grant doesn't name the tool.
    NotGranted,
    /// Floor: the installation's registry doesn't hold the tool.
    UnknownTool,
    /// Floor: the installation's server no longer lists the tool.
    ToolGone,
    /// The organization's level for the tool is `deny`.
    OrganizationDenies,
    /// The tool changed and nobody accepted the change yet.
    ChangedTool,
    /// Nobody confirmed a level for the tool: the policy's level for new tools.
    UnconfirmedLevel,
    /// The organization's level for the tool is `approve`.
    OrganizationRequiresApproval,
    /// The grant requires approval for the tool.
    GrantRequiresApproval,
    /// The policy's configuration couldn't be read: fail closed.
    ConfigurationUnavailable,
    /// Nothing stops it.
    Allowed,
}

impl Rule {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotGranted => "not_granted",
            Self::UnknownTool => "unknown_tool",
            Self::ToolGone => "tool_gone",
            Self::OrganizationDenies => "organization_denies",
            Self::ChangedTool => "changed_tool",
            Self::UnconfirmedLevel => "unconfirmed_level",
            Self::OrganizationRequiresApproval => "organization_requires_approval",
            Self::GrantRequiresApproval => "grant_requires_approval",
            Self::ConfigurationUnavailable => "configuration_unavailable",
            Self::Allowed => "allowed",
        }
    }
}

/// A policy's answer for one call, recorded with the policy, the mode and
/// the rule that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    /// The deciding policy's [`ApprovalPolicy::name`].
    pub policy: &'static str,
    /// The mode in force; `None` when the decision didn't need it (the
    /// floor), or it couldn't be read.
    pub mode: Option<ApprovalMode>,
    pub rule: Rule,
}

/// Decides each tool call: allow it, wait for a person's approval, or deny
/// it. Replaceable whole; `ConfiguredPolicy` is the built-in one.
///
/// Every implementation holds to the floor ([`floor`]) and never answers
/// `Allow` for a tool the grant requires approval for, or one that changed
/// and wasn't reviewed. A reason is written for the model and the person
/// deciding: plain, and never empty.
#[async_trait]
pub trait ApprovalPolicy: Send + Sync {
    /// Recorded with every decision.
    fn name(&self) -> &'static str;

    /// Never fails: a policy that can't decide requires approval.
    async fn decide(&self, call: &ToolCall) -> Decision;
}

/// The fixed floor, whatever the policy: a call to a tool that isn't
/// granted, isn't in the installation's registry, is gone or that the
/// organization denies (a confirmed `deny`) is denied; one whose
/// organization level couldn't be read requires approval, so the floor fails
/// closed. `None` when the floor leaves the call to the policy.
pub fn floor(call: &ToolCall, policy: &'static str) -> Option<Decision> {
    let tool = &call.tool;
    let denied = match (call.grant, call.status, call.level) {
        (GrantedAs::NotGranted, _, _) => Some((Rule::NotGranted, format!("{tool} isn't granted to this agent."))),
        (_, None, _) => Some((
            Rule::UnknownTool,
            format!("{tool} isn't one of this installation's tools."),
        )),
        (_, Some(ToolStatus::Gone), _) => Some((Rule::ToolGone, format!("{tool} is no longer offered by the app."))),
        (
            _,
            _,
            OrganizationLevel::Set {
                level: ToolLevel::Deny,
                confirmed: true,
            },
        ) => Some((Rule::OrganizationDenies, format!("Your organization blocks {tool}."))),
        _ => None,
    };
    if let Some((rule, reason)) = denied {
        return Some(Decision {
            verdict: Verdict::Deny { reason },
            policy,
            mode: None,
            rule,
        });
    }
    (call.level == OrganizationLevel::Unreadable).then(|| Decision {
        verdict: Verdict::RequireApproval {
            reason: format!("Your organization's level for {tool} couldn't be read, so this call waits for approval."),
        },
        policy,
        mode: None,
        rule: Rule::ConfigurationUnavailable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(tool: &str, level: ToolLevel, confirmed: bool) -> AppToolLevel {
        AppToolLevel {
            app_id: AppId::generate(),
            tool: tool.to_owned(),
            level,
            confirmed,
            destructive: false,
            updated_at: Timestamp::from_millis(0),
            updated_by: None,
        }
    }

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn suggests_from_hints() {
        assert_eq!(ToolLevel::suggested(ToolEffect::Read), ToolLevel::Allow);
        assert_eq!(ToolLevel::suggested(ToolEffect::Write), ToolLevel::Approve);
        assert_eq!(ToolLevel::suggested(ToolEffect::Destructive), ToolLevel::Approve);
    }

    #[test]
    fn only_a_confirmed_level_is_in_force() {
        let tools = names(&["a", "b", "c"]);
        let levels = [level("a", ToolLevel::Allow, true), level("b", ToolLevel::Allow, false)];
        let held = GrantLevels::in_force(&tools, &levels, NewToolLevel::Deny);
        assert_eq!(held.of("a"), Some(ToolLevel::Allow));
        assert_eq!(held.of("b"), Some(ToolLevel::Deny));
        assert_eq!(held.of("c"), Some(ToolLevel::Deny));
        assert_eq!(held.of("d"), None);
    }

    #[test]
    fn refuses_denied_tools_before_missing_approvals() {
        let tools = names(&["read", "send", "drop"]);
        let levels = [
            level("read", ToolLevel::Allow, true),
            level("send", ToolLevel::Approve, true),
            level("drop", ToolLevel::Deny, true),
        ];
        let held = GrantLevels::in_force(&tools, &levels, NewToolLevel::Approve);
        assert_eq!(
            level_refusal(&tools, &[], &held),
            Some(LevelRefusal::Denied("drop".to_owned()))
        );
        let tools = names(&["read", "send"]);
        assert_eq!(
            level_refusal(&tools, &[], &held),
            Some(LevelRefusal::ApprovalRequired("send".to_owned()))
        );
        assert_eq!(level_refusal(&tools, &names(&["send"]), &held), None);
        assert_eq!(level_refusal(&tools, &[], &GrantLevels::none()), None);
    }

    #[test]
    fn validates_a_change_of_levels() {
        let one = |tool: &str| vec![(tool.to_owned(), ToolLevel::Allow)];
        assert_eq!(validate_levels(&[]), Err(PolicyError::InvalidLevels));
        assert_eq!(validate_levels(&one("")), Err(PolicyError::InvalidLevels));
        assert_eq!(validate_levels(&one(&"x".repeat(129))), Err(PolicyError::InvalidLevels));
        assert_eq!(validate_levels(&one(&"x".repeat(128))), Ok(()));
        let twice = [one("a"), one("a")].concat();
        assert_eq!(validate_levels(&twice), Err(PolicyError::InvalidLevels));
        let many: Vec<_> = (0..=MAX_LEVELS_PER_CHANGE)
            .map(|i| (format!("t{i}"), ToolLevel::Allow))
            .collect();
        assert_eq!(validate_levels(&many), Err(PolicyError::InvalidLevels));
        assert_eq!(validate_levels(&many[1..]), Ok(()));
    }
}
