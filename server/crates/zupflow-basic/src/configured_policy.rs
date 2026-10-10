//! [`ConfiguredPolicy`]: the built-in [`ApprovalPolicy`] (decision 0040),
//! with its behaviour in the database: the organization's policy record,
//! read through a [`ToolPolicyStore`] on every call, and its level for the
//! call's tool, which the run engine reads into the [`ToolCall`].
//!
//! The floor comes first: it denies a confirmed `deny` and requires approval
//! when the level couldn't be read. Then, in `manual` mode, the strictest of
//! these decides:
//!
//! 1. The level in force is `deny` (unconfirmed, under new tools at `deny`): denied.
//! 2. The tool changed and nobody accepted the change: the policy's level
//!    for new tools (`approve` or `deny`), never `allow`.
//! 3. The grant requires approval for it: approval.
//! 4. The organization's level is `approve`: approval.
//! 5. Otherwise: allowed.
//!
//! A level is in force only once a person confirms it; until then the
//! policy's level for new tools applies. A workspace app's tools have no
//! organization level. **Fail closed:** when the policy or the level can't
//! be read, the call requires approval.

use std::sync::Arc;

use async_trait::async_trait;
use zupflow_core::policy::{
    ApprovalMode, ApprovalPolicy, Decision, GrantedAs, NewToolLevel, OrganizationLevel, Rule, ToolCall, ToolLevel,
    ToolPolicyStore, Verdict, floor,
};
use zupflow_core::tools::ToolStatus;

/// Recorded with each of its decisions.
pub const NAME: &str = "configured";

pub struct ConfiguredPolicy {
    store: Arc<dyn ToolPolicyStore>,
}

impl ConfiguredPolicy {
    pub fn new(store: Arc<dyn ToolPolicyStore>) -> Self {
        Self { store }
    }
}

fn decision(verdict: Verdict, mode: Option<ApprovalMode>, rule: Rule) -> Decision {
    Decision {
        verdict,
        policy: NAME,
        mode,
        rule,
    }
}

fn deny(reason: String) -> Verdict {
    Verdict::Deny { reason }
}

fn approval(reason: String) -> Verdict {
    Verdict::RequireApproval { reason }
}

/// The organization's level for the call's tool, and whether a person set it.
struct Level {
    level: ToolLevel,
    confirmed: bool,
}

/// The organization's level in force for the call's tool: the confirmed
/// one, else the policy's level for new tools; `None` for a workspace app's.
/// An unreadable level never gets here: the floor requires approval for it.
fn level_in_force(call: &ToolCall, new_tool_level: NewToolLevel) -> Option<Level> {
    match call.level {
        OrganizationLevel::NotApplicable | OrganizationLevel::Unreadable => None,
        OrganizationLevel::Set { level, confirmed: true } => Some(Level { level, confirmed: true }),
        OrganizationLevel::Unset | OrganizationLevel::Set { confirmed: false, .. } => Some(Level {
            level: new_tool_level.level(),
            confirmed: false,
        }),
    }
}

#[async_trait]
impl ApprovalPolicy for ConfiguredPolicy {
    fn name(&self) -> &'static str {
        NAME
    }

    async fn decide(&self, call: &ToolCall) -> Decision {
        if let Some(refused) = floor(call, NAME) {
            return refused;
        }
        let unreadable = |mode| {
            decision(
                approval(
                    "Your organization's tool policy couldn't be read, so this call waits for approval.".to_owned(),
                ),
                mode,
                Rule::ConfigurationUnavailable,
            )
        };
        let policy = match self.store.load_policy(call.organization_id).await {
            Ok(policy) => policy,
            Err(error) => {
                tracing::warn!(event = "policy.unreadable", %error);
                return unreadable(None);
            }
        };
        let mode = policy.settings.mode;
        let new_tool_level = policy.settings.new_tool_level;
        let level = level_in_force(call, new_tool_level);
        let tool = &call.tool;
        match mode {
            ApprovalMode::Manual => {
                if let Some(Level {
                    level: ToolLevel::Deny,
                    confirmed,
                }) = level
                {
                    return if confirmed {
                        decision(
                            deny(format!("Your organization blocks {tool}.")),
                            Some(mode),
                            Rule::OrganizationDenies,
                        )
                    } else {
                        decision(
                            deny(format!(
                                "Nobody has set a level for {tool} yet, and your organization blocks new tools."
                            )),
                            Some(mode),
                            Rule::UnconfirmedLevel,
                        )
                    };
                }
                if call.status == Some(ToolStatus::Changed) {
                    let verdict = match new_tool_level {
                        NewToolLevel::Deny => deny(format!(
                            "{tool} changed since it was reviewed, and your organization blocks changed tools until \
                             someone accepts the change."
                        )),
                        NewToolLevel::Approve => approval(format!(
                            "{tool} changed since it was reviewed, so each call waits for approval until someone \
                             accepts the change."
                        )),
                    };
                    return decision(verdict, Some(mode), Rule::ChangedTool);
                }
                if call.grant
                    == (GrantedAs::Granted {
                        approval_required: true,
                    })
                {
                    return decision(
                        approval(format!("This agent's access requires approval for {tool}.")),
                        Some(mode),
                        Rule::GrantRequiresApproval,
                    );
                }
                match level {
                    Some(Level {
                        level: ToolLevel::Approve,
                        confirmed: true,
                    }) => decision(
                        approval(format!("Your organization requires approval for {tool}.")),
                        Some(mode),
                        Rule::OrganizationRequiresApproval,
                    ),
                    Some(Level {
                        level: ToolLevel::Approve,
                        confirmed: false,
                    }) => decision(
                        approval(format!(
                            "Nobody has set a level for {tool} yet, so it waits for approval like every new tool."
                        )),
                        Some(mode),
                        Rule::UnconfirmedLevel,
                    ),
                    _ => decision(Verdict::Allow, Some(mode), Rule::Allowed),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use zupflow_conformance::approval_policy::{Setting, run};
    use zupflow_core::apps::ToolEffect;
    use zupflow_core::ids::{AgentId, AppId, InstallationId, OrganizationId, UserId, WorkspaceId};
    use zupflow_core::policy::{
        AppToolLevel, CallApp, LevelPage, PolicyError, PolicySettings, ToolPolicy, validate_levels,
    };
    use zupflow_core::time::Timestamp;

    use super::*;

    /// One organization's policy and one catalog app's levels, in memory;
    /// `fail` makes every read fail.
    struct Store {
        organization_id: OrganizationId,
        catalog_app: AppId,
        settings: Mutex<PolicySettings>,
        levels: Mutex<BTreeMap<String, (ToolLevel, bool)>>,
        fail_policy: bool,
        fail_levels: bool,
    }

    impl Store {
        fn new(setting: Setting) -> Self {
            Self {
                organization_id: setting.organization_id,
                catalog_app: setting.catalog_app,
                settings: Mutex::new(PolicySettings::default()),
                levels: Mutex::new(BTreeMap::new()),
                fail_policy: false,
                fail_levels: false,
            }
        }

        fn found(&self, organization_id: OrganizationId, app_id: AppId) -> Result<(), PolicyError> {
            if organization_id == self.organization_id && app_id == self.catalog_app {
                Ok(())
            } else {
                Err(PolicyError::AppNotFound)
            }
        }

        fn level(&self, tool: &str, level: ToolLevel, confirmed: bool) {
            self.levels.lock().unwrap().insert(tool.to_owned(), (level, confirmed));
        }

        fn row(&self, tool: &str, (level, confirmed): (ToolLevel, bool)) -> AppToolLevel {
            AppToolLevel {
                app_id: self.catalog_app,
                tool: tool.to_owned(),
                level,
                confirmed,
                destructive: false,
                updated_at: Timestamp::from_millis(0),
                updated_by: None,
            }
        }
    }

    #[async_trait]
    impl ToolPolicyStore for Store {
        async fn load_policy(&self, organization_id: OrganizationId) -> Result<ToolPolicy, PolicyError> {
            if self.fail_policy {
                return Err(PolicyError::Unavailable);
            }
            let settings = if organization_id == self.organization_id {
                *self.settings.lock().unwrap()
            } else {
                PolicySettings::default()
            };
            Ok(ToolPolicy {
                organization_id,
                settings,
                updated_at: None,
                updated_by: None,
            })
        }

        async fn save_policy(
            &self,
            organization_id: OrganizationId,
            settings: PolicySettings,
            _updated_by: Option<UserId>,
            _now: Timestamp,
        ) -> Result<ToolPolicy, PolicyError> {
            *self.settings.lock().unwrap() = settings;
            self.load_policy(organization_id).await
        }

        async fn list_levels(
            &self,
            organization_id: OrganizationId,
            app_id: AppId,
            _after: Option<&str>,
            _limit: u32,
        ) -> Result<LevelPage, PolicyError> {
            self.found(organization_id, app_id)?;
            let items: Vec<_> = self
                .levels
                .lock()
                .unwrap()
                .iter()
                .map(|(tool, l)| self.row(tool, *l))
                .collect();
            Ok(LevelPage {
                total: items.len() as u64,
                items,
                next_after: None,
            })
        }

        async fn find_levels(
            &self,
            organization_id: OrganizationId,
            app_id: AppId,
            tools: &[String],
        ) -> Result<Vec<AppToolLevel>, PolicyError> {
            if self.fail_levels {
                return Err(PolicyError::Unavailable);
            }
            self.found(organization_id, app_id)?;
            let levels = self.levels.lock().unwrap();
            Ok(tools
                .iter()
                .filter_map(|tool| levels.get(tool).map(|l| self.row(tool, *l)))
                .collect())
        }

        async fn set_levels(
            &self,
            organization_id: OrganizationId,
            app_id: AppId,
            levels: &[(String, ToolLevel)],
            _updated_by: Option<UserId>,
            _now: Timestamp,
        ) -> Result<Vec<AppToolLevel>, PolicyError> {
            self.found(organization_id, app_id)?;
            validate_levels(levels)?;
            for (tool, level) in levels {
                self.level(tool, *level, true);
            }
            Ok(levels
                .iter()
                .map(|(tool, level)| self.row(tool, (*level, true)))
                .collect())
        }

        async fn suggest_levels(
            &self,
            organization_id: OrganizationId,
            app_id: AppId,
            listed: &[(String, ToolEffect)],
            _now: Timestamp,
        ) -> Result<u64, PolicyError> {
            self.found(organization_id, app_id)?;
            let mut levels = self.levels.lock().unwrap();
            let mut count = 0;
            for (tool, effect) in listed {
                if !levels.contains_key(tool) {
                    levels.insert(tool.clone(), (ToolLevel::suggested(*effect), false));
                    count += 1;
                }
            }
            Ok(count)
        }
    }

    fn setting() -> Setting {
        Setting {
            organization_id: OrganizationId::generate(),
            workspace_id: WorkspaceId::generate(),
            catalog_app: AppId::generate(),
            workspace_app: AppId::generate(),
        }
    }

    fn deny_new_tools(store: &Store) {
        *store.settings.lock().unwrap() = PolicySettings {
            mode: ApprovalMode::Manual,
            new_tool_level: NewToolLevel::Deny,
        };
    }

    /// The suite's tools with every kind of level: confirmed allow, approve,
    /// deny, and only suggested.
    fn configured(setting: Setting) -> Store {
        let store = Store::new(setting);
        store.level("search", ToolLevel::Allow, true);
        store.level("send", ToolLevel::Approve, true);
        store.level("drop", ToolLevel::Deny, true);
        store.level("never_configured", ToolLevel::Allow, false);
        store
    }

    #[tokio::test]
    async fn conforms_however_configured() {
        for new_tools in [false, true] {
            let (setting, empty) = (setting(), setting());
            let store = configured(setting);
            if new_tools {
                deny_new_tools(&store);
            }
            run(&ConfiguredPolicy::new(Arc::new(store)), setting).await;

            run(&ConfiguredPolicy::new(Arc::new(Store::new(empty))), empty).await;
        }
        for (fail_policy, fail_levels) in [(true, false), (false, true)] {
            let setting = setting();
            let mut store = configured(setting);
            store.fail_policy = fail_policy;
            store.fail_levels = fail_levels;
            run(&ConfiguredPolicy::new(Arc::new(store)), setting).await;
        }
    }

    /// The call as the run engine would resolve it, its level read from `store`.
    fn call(
        store: &Store,
        setting: Setting,
        app: CallApp,
        tool: &str,
        status: ToolStatus,
        approval_required: bool,
    ) -> ToolCall {
        let level = match app {
            CallApp::Workspace(_) => OrganizationLevel::NotApplicable,
            CallApp::Catalog(_) if store.fail_levels => OrganizationLevel::Unreadable,
            CallApp::Catalog(_) => match store.levels.lock().unwrap().get(tool) {
                Some((level, confirmed)) => OrganizationLevel::Set {
                    level: *level,
                    confirmed: *confirmed,
                },
                None => OrganizationLevel::Unset,
            },
        };
        ToolCall {
            organization_id: setting.organization_id,
            workspace_id: setting.workspace_id,
            agent_id: AgentId::generate(),
            installation_id: InstallationId::generate(),
            app,
            tool: tool.to_owned(),
            status: Some(status),
            grant: GrantedAs::Granted { approval_required },
            level,
        }
    }

    /// Each decision as (verdict kind, mode, rule).
    async fn decide(policy: &ConfiguredPolicy, call: &ToolCall) -> (&'static str, Option<ApprovalMode>, Rule) {
        let decision = policy.decide(call).await;
        let kind = match decision.verdict {
            Verdict::Allow => "allow",
            Verdict::RequireApproval { .. } => "approval",
            Verdict::Deny { .. } => "deny",
        };
        (kind, decision.mode, decision.rule)
    }

    #[tokio::test]
    async fn decides_by_the_strictest_layer() {
        let setting = setting();
        let store = Arc::new(configured(setting));
        let policy = ConfiguredPolicy::new(store.clone());
        let catalog = CallApp::Catalog(setting.catalog_app);
        let workspace = CallApp::Workspace(setting.workspace_app);
        let manual = Some(ApprovalMode::Manual);
        use ToolStatus::{Changed, Current};
        for (app, tool, status, approval_required, expected) in [
            (catalog, "search", Current, false, ("allow", manual, Rule::Allowed)),
            (
                catalog,
                "search",
                Current,
                true,
                ("approval", manual, Rule::GrantRequiresApproval),
            ),
            (
                catalog,
                "send",
                Current,
                false,
                ("approval", manual, Rule::OrganizationRequiresApproval),
            ),
            (
                catalog,
                "send",
                Current,
                true,
                ("approval", manual, Rule::GrantRequiresApproval),
            ),
            (catalog, "drop", Current, true, ("deny", None, Rule::OrganizationDenies)),
            (
                catalog,
                "drop",
                Changed,
                false,
                ("deny", None, Rule::OrganizationDenies),
            ),
            (
                catalog,
                "search",
                Changed,
                false,
                ("approval", manual, Rule::ChangedTool),
            ),
            // Only suggested: the level for new tools, approve.
            (
                catalog,
                "never_configured",
                Current,
                false,
                ("approval", manual, Rule::UnconfirmedLevel),
            ),
            (
                catalog,
                "unknown_to_the_policy",
                Current,
                false,
                ("approval", manual, Rule::UnconfirmedLevel),
            ),
            // A workspace app's tools: the grant alone, and the change rule.
            (workspace, "drop", Current, false, ("allow", manual, Rule::Allowed)),
            (
                workspace,
                "drop",
                Current,
                true,
                ("approval", manual, Rule::GrantRequiresApproval),
            ),
            (
                workspace,
                "drop",
                Changed,
                false,
                ("approval", manual, Rule::ChangedTool),
            ),
        ] {
            let call = call(&store, setting, app, tool, status, approval_required);
            assert_eq!(
                decide(&policy, &call).await,
                expected,
                "{tool} {status:?} {approval_required}"
            );
        }

        deny_new_tools(&store);
        for (app, tool, status, expected) in [
            (
                catalog,
                "never_configured",
                Current,
                ("deny", manual, Rule::UnconfirmedLevel),
            ),
            (catalog, "search", Changed, ("deny", manual, Rule::ChangedTool)),
            (catalog, "search", Current, ("allow", manual, Rule::Allowed)),
            (workspace, "drop", Changed, ("deny", manual, Rule::ChangedTool)),
            (workspace, "drop", Current, ("allow", manual, Rule::Allowed)),
        ] {
            let call = call(&store, setting, app, tool, status, false);
            assert_eq!(decide(&policy, &call).await, expected, "{tool} {status:?}");
        }
    }

    #[tokio::test]
    async fn fails_closed_when_the_configuration_is_unreadable() {
        let setting = setting();
        let catalog = CallApp::Catalog(setting.catalog_app);
        for (fail_policy, fail_levels) in [(true, false), (false, true)] {
            let mut store = configured(setting);
            store.fail_policy = fail_policy;
            store.fail_levels = fail_levels;
            let call = call(&store, setting, catalog, "search", ToolStatus::Current, false);
            let policy = ConfiguredPolicy::new(Arc::new(store));
            assert_eq!(
                decide(&policy, &call).await,
                ("approval", None, Rule::ConfigurationUnavailable),
                "the floor, or the policy before its mode is known"
            );
            // The floor needs no configuration.
            let gone = ToolCall {
                status: Some(ToolStatus::Gone),
                ..call
            };
            assert_eq!(decide(&policy, &gone).await, ("deny", None, Rule::ToolGone));
        }
    }
}
