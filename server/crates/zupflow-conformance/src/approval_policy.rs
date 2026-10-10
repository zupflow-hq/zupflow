//! The `ApprovalPolicy` suite: what every policy answers, whatever it's
//! configured to do. The floor holds: a tool not granted, not in the
//! registry, gone or that the organization denies (a confirmed `deny`) is
//! denied, and one whose organization level couldn't be read is never
//! allowed; a tool the grant requires approval for, or
//! one changed and not reviewed, is never allowed; every decision names its
//! policy, a denial or an approval carries a reason; and the same call gets
//! the same answer.
//!
//! The caller passes the [`Setting`] its policy is configured for: an
//! organization, a workspace, one of its catalog apps and a workspace app.
//! The suite tries every combination of app, registry status, grant and
//! organization level on a few tool names, so it holds for any
//! configuration.

use zupflow_core::ids::{AgentId, AppId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::policy::{
    ApprovalPolicy, CallApp, Decision, GrantedAs, OrganizationLevel, Rule, ToolCall, ToolLevel, Verdict,
};
use zupflow_core::tools::ToolStatus;

#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub organization_id: OrganizationId,
    pub workspace_id: WorkspaceId,
    pub catalog_app: AppId,
    pub workspace_app: AppId,
}

/// Tool names the setting may hold levels for, and one it can't.
pub const TOOLS: [&str; 4] = ["search", "send", "drop", "never_configured"];

fn calls(setting: Setting) -> Vec<ToolCall> {
    let agent = AgentId::generate();
    let installation = InstallationId::generate();
    let mut calls = Vec::new();
    for app in [
        CallApp::Catalog(setting.catalog_app),
        CallApp::Workspace(setting.workspace_app),
    ] {
        for status in [
            None,
            Some(ToolStatus::Current),
            Some(ToolStatus::Changed),
            Some(ToolStatus::Gone),
        ] {
            for grant in [
                GrantedAs::NotGranted,
                GrantedAs::Granted {
                    approval_required: false,
                },
                GrantedAs::Granted {
                    approval_required: true,
                },
            ] {
                let levels: Vec<OrganizationLevel> = match app {
                    CallApp::Workspace(_) => vec![OrganizationLevel::NotApplicable],
                    CallApp::Catalog(_) => {
                        let mut levels = vec![OrganizationLevel::Unset, OrganizationLevel::Unreadable];
                        for level in [ToolLevel::Allow, ToolLevel::Approve, ToolLevel::Deny] {
                            for confirmed in [false, true] {
                                levels.push(OrganizationLevel::Set { level, confirmed });
                            }
                        }
                        levels
                    }
                };
                for level in levels {
                    for tool in TOOLS {
                        calls.push(ToolCall {
                            organization_id: setting.organization_id,
                            workspace_id: setting.workspace_id,
                            agent_id: agent,
                            installation_id: installation,
                            app,
                            tool: tool.to_owned(),
                            status,
                            grant,
                            level,
                        });
                    }
                }
            }
        }
    }
    calls
}

fn reason(decision: &Decision) -> Option<&str> {
    match &decision.verdict {
        Verdict::Allow => None,
        Verdict::RequireApproval { reason } | Verdict::Deny { reason } => Some(reason),
    }
}

pub async fn run(policy: &dyn ApprovalPolicy, setting: Setting) {
    assert!(!policy.name().is_empty(), "a policy has a name");
    for call in calls(setting) {
        let decision = policy.decide(&call).await;
        let what = format!("{call:?} -> {decision:?}");
        assert_eq!(decision.policy, policy.name(), "a decision names its policy: {what}");
        if let Some(reason) = reason(&decision) {
            assert!(!reason.trim().is_empty(), "a reason is written: {what}");
        }
        assert_eq!(
            policy.decide(&call).await,
            decision,
            "the same call, the same answer: {what}"
        );

        // The floor.
        let floor = match (call.grant, call.status, call.level) {
            (GrantedAs::NotGranted, _, _) => Some(Rule::NotGranted),
            (_, None, _) => Some(Rule::UnknownTool),
            (_, Some(ToolStatus::Gone), _) => Some(Rule::ToolGone),
            (
                _,
                _,
                OrganizationLevel::Set {
                    level: ToolLevel::Deny,
                    confirmed: true,
                },
            ) => Some(Rule::OrganizationDenies),
            _ => None,
        };
        if let Some(rule) = floor {
            assert!(
                matches!(decision.verdict, Verdict::Deny { .. }),
                "the floor denies: {what}"
            );
            assert_eq!(decision.rule, rule, "the floor's rule: {what}");
            continue;
        }
        assert!(
            !matches!(decision.rule, Rule::NotGranted | Rule::UnknownTool | Rule::ToolGone),
            "a floor rule only where the floor applies: {what}"
        );
        if call.level == OrganizationLevel::Unreadable {
            assert!(
                matches!(decision.verdict, Verdict::RequireApproval { .. }),
                "a level that couldn't be read requires approval: {what}"
            );
            continue;
        }
        if call.grant
            == (GrantedAs::Granted {
                approval_required: true,
            })
        {
            assert!(
                !matches!(decision.verdict, Verdict::Allow),
                "a tool the grant requires approval for is never allowed: {what}"
            );
        }
        if call.status == Some(ToolStatus::Changed) {
            assert!(
                !matches!(decision.verdict, Verdict::Allow),
                "a changed tool nobody reviewed is never allowed: {what}"
            );
        }
        if matches!(decision.verdict, Verdict::Allow) {
            assert_eq!(decision.rule, Rule::Allowed, "an allowed call says so: {what}");
        }
    }
}
