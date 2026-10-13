//! The `AgentStore` suite: agents and grants reached only through their own
//! organization and workspace; a grant only of an installation of the same
//! workspace, at most one per agent and installation, naming only tools its
//! installation's registry holds and doesn't hold as gone, with approvals
//! inside it, and held inside the organization's levels as they are when
//! it's written; a grant left as it is when a tool later goes; an agent's
//! grants deleted with it, and an installation's with the installation.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, two workspaces, two
//! installations in the first and one in the second, all of one app. The
//! suite records registry listings through `registry`, sets levels through
//! `policies` and deletes an installation through `installations`, the
//! stores that own them.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::agents::{Agent, AgentChanges, AgentError, AgentStatus, AgentStore, GrantTools, NewAgent};
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{AgentId, AppId, GrantId, InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::installations::InstallationStore;
use zupflow_core::policy::{ApprovalMode, NewToolLevel, PolicySettings, ToolLevel, ToolPolicyStore};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{ListedTool, ToolRegistry, ToolRevision};

use crate::audit_store::{by_suite, by_suite_in};

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    /// The catalog app every installation is of.
    pub app: AppId,
    pub workspace: WorkspaceId,
    pub installation: InstallationId,
    /// Another installation of the same workspace, with a registry of its own.
    pub second_installation: InstallationId,
    /// Another workspace of the organization, with its own installation.
    pub sibling: WorkspaceId,
    pub sibling_installation: InstallationId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The stores the suite drives.
pub struct Stores<'a> {
    pub agents: &'a dyn AgentStore,
    pub policies: &'a dyn ToolPolicyStore,
    pub registry: &'a dyn ToolRegistry,
    pub installations: &'a dyn InstallationStore,
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        app: AppId::generate(),
        workspace: WorkspaceId::generate(),
        installation: InstallationId::generate(),
        second_installation: InstallationId::generate(),
        sibling: WorkspaceId::generate(),
        sibling_installation: InstallationId::generate(),
    };
    prepare(tenant).await;
    tenant
}

fn new_agent(name: &str) -> NewAgent {
    NewAgent {
        name: name.to_owned(),
        description: format!("{name}, described"),
        model_id: None,
        instructions: "Be brief.".to_owned(),
    }
}

fn listed(name: &str, version: u8) -> ListedTool {
    ListedTool {
        name: name.to_owned(),
        revision: ToolRevision {
            definition: format!(r#"{{"description":"v{version}","name":"{name}"}}"#),
            hash: [version; 32],
        },
        effect: ToolEffect::Read,
    }
}

fn grant_of(tools: &[&str], approval_required: &[&str]) -> GrantTools {
    GrantTools {
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        approval_required: approval_required.iter().map(|tool| (*tool).to_owned()).collect(),
    }
}

/// Records two listings of `installation`: `current` as first listed,
/// `changed` listed differently since, `gone` no longer listed.
async fn registry_of(stores: &Stores<'_>, t: &Tenant, workspace: WorkspaceId, installation: InstallationId) {
    let earlier = Timestamp::from_millis(Timestamp::now().millis() - 60_000);
    stores
        .registry
        .record_listing(
            t.organization_id,
            workspace,
            installation,
            &[listed("current", 1), listed("changed", 1), listed("gone", 1)],
            earlier,
        )
        .await
        .unwrap();
    stores
        .registry
        .record_listing(
            t.organization_id,
            workspace,
            installation,
            &[listed("current", 1), listed("changed", 2)],
            Timestamp::now(),
        )
        .await
        .unwrap();
    // The organization allows them, so only the registry's rules decide.
    set_levels(
        stores,
        t,
        &[
            ("current", ToolLevel::Allow),
            ("changed", ToolLevel::Allow),
            ("gone", ToolLevel::Allow),
        ],
    )
    .await;
}

async fn create(stores: &Stores<'_>, t: &Tenant, name: &str) -> Agent {
    stores
        .agents
        .create_agent(
            t.organization_id,
            t.workspace,
            AgentId::generate(),
            &new_agent(name),
            &|_| by_suite_in(t.workspace),
        )
        .await
        .unwrap()
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    creates_reads_changes_and_deletes_agents(&stores, &prepare).await;
    keeps_agents_and_grants_to_their_scope(&stores, &prepare).await;
    grants_only_what_the_registry_holds(&stores, &prepare).await;
    holds_grants_inside_the_organization_levels(&stores, &prepare).await;
    reads_the_levels_when_it_writes_the_grant(&stores, &prepare).await;
    keeps_one_grant_per_agent_and_installation(&stores, &prepare).await;
    keeps_one_grant_when_two_are_made_at_once(&stores, &prepare).await;
    changes_and_deletes_only_the_grant_named(&stores, &prepare).await;
    leaves_a_grant_alone_when_its_tool_goes(&stores, &prepare).await;
    deletes_grants_with_their_agent_or_installation(&stores, &prepare).await;
}

async fn creates_reads_changes_and_deletes_agents(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    let beta = create(stores, &t, "Beta").await;
    let alpha = create(stores, &t, "Alpha").await;
    assert_eq!(
        [
            beta.name.as_str(),
            beta.description.as_str(),
            beta.instructions.as_str()
        ],
        ["Beta", "Beta, described", "Be brief."]
    );
    assert_eq!(beta.model_id, None, "on the environment's default");
    assert_eq!(
        (beta.status, beta.grant_count),
        (AgentStatus::Active, 0),
        "starts active, with no grants"
    );
    assert_eq!((beta.organization_id, beta.workspace_id), (org, ws));
    let names: Vec<String> = store
        .list_agents(org, ws)
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, ["Alpha", "Beta"], "listed by name");
    assert_eq!(store.find_agent(org, ws, alpha.id).await.unwrap(), Some(alpha.clone()));

    let paused = store
        .update_agent(
            org,
            ws,
            beta.id,
            &AgentChanges {
                status: Some(AgentStatus::Paused),
                ..AgentChanges::default()
            },
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    assert_eq!(
        (paused.status, paused.name.as_str(), paused.model_id),
        (AgentStatus::Paused, "Beta", None),
        "an update changes only what it names"
    );
    assert!(paused.updated_at >= beta.updated_at);
    assert_eq!(paused.created_at, beta.created_at);
    let renamed = store
        .update_agent(
            org,
            ws,
            beta.id,
            &AgentChanges {
                name: Some("Gamma".to_owned()),
                description: Some(String::new()),
                model_id: Some(None),
                instructions: Some("Be thorough.".to_owned()),
                status: Some(AgentStatus::Active),
            },
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    assert_eq!(
        [
            renamed.name.as_str(),
            renamed.description.as_str(),
            renamed.instructions.as_str()
        ],
        ["Gamma", "", "Be thorough."]
    );
    assert_eq!(renamed.status, AgentStatus::Active);
    assert_eq!(store.find_agent(org, ws, beta.id).await.unwrap(), Some(renamed));

    store
        .delete_agent(org, ws, alpha.id, &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert_eq!(store.find_agent(org, ws, alpha.id).await.unwrap(), None);
    assert_eq!(
        store.delete_agent(org, ws, alpha.id, &|_| by_suite_in(ws)).await,
        Err(AgentError::AgentNotFound)
    );
    assert_eq!(
        store
            .update_agent(org, ws, alpha.id, &AgentChanges::default(), &|_| by_suite_in(ws))
            .await,
        Err(AgentError::AgentNotFound)
    );
    // Missing before anything about the change: even a model that isn't there.
    assert_eq!(
        store
            .update_agent(
                org,
                ws,
                alpha.id,
                &AgentChanges {
                    model_id: Some(Some(zupflow_core::ids::ModelId::generate())),
                    ..AgentChanges::default()
                },
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::AgentNotFound)
    );
    assert_eq!(
        store.list_grants(org, ws, alpha.id).await,
        Err(AgentError::AgentNotFound)
    );
    assert_eq!(store.list_agents(org, ws).await.unwrap().len(), 1);
}

async fn keeps_agents_and_grants_to_their_scope(stores: &Stores<'_>, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let store = stores.agents;
    registry_of(stores, &a, a.workspace, a.installation).await;
    registry_of(stores, &a, a.sibling, a.sibling_installation).await;
    registry_of(stores, &b, b.workspace, b.installation).await;
    let agent = create(stores, &a, "Scoped").await;
    let tools = grant_of(&["current"], &[]);
    let grant = store
        .create_grant(
            a.organization_id,
            a.workspace,
            agent.id,
            GrantId::generate(),
            a.installation,
            &tools,
            &|_| by_suite_in(a.workspace),
        )
        .await
        .unwrap();
    let before = store
        .list_grants(a.organization_id, a.workspace, agent.id)
        .await
        .unwrap();

    // The same IDs through the sibling workspace, and through another organization.
    for (org, ws, what) in [
        (a.organization_id, a.sibling, "sibling workspace"),
        (b.organization_id, a.workspace, "other organization"),
        (b.organization_id, b.workspace, "other organization's workspace"),
    ] {
        assert_eq!(store.find_agent(org, ws, agent.id).await.unwrap(), None, "{what}");
        assert!(
            store
                .list_agents(org, ws)
                .await
                .unwrap()
                .iter()
                .all(|other| other.id != agent.id),
            "{what}"
        );
        assert_eq!(
            store
                .update_agent(
                    org,
                    ws,
                    agent.id,
                    &AgentChanges {
                        status: Some(AgentStatus::Paused),
                        ..AgentChanges::default()
                    },
                    &|_| by_suite_in(ws)
                )
                .await,
            Err(AgentError::AgentNotFound),
            "{what}"
        );
        assert_eq!(
            store.delete_agent(org, ws, agent.id, &|_| by_suite_in(ws)).await,
            Err(AgentError::AgentNotFound),
            "{what}"
        );
        assert_eq!(
            store.list_grants(org, ws, agent.id).await,
            Err(AgentError::AgentNotFound),
            "{what}"
        );
        assert_eq!(
            store
                .create_grant(org, ws, agent.id, GrantId::generate(), a.installation, &tools, &|_| {
                    by_suite_in(ws)
                })
                .await,
            Err(AgentError::AgentNotFound),
            "{what}"
        );
        assert_eq!(
            store
                .update_grant(org, ws, agent.id, grant.id, &tools, &|_| by_suite_in(ws))
                .await,
            Err(AgentError::AgentNotFound),
            "{what}"
        );
        assert_eq!(
            store
                .delete_grant(org, ws, agent.id, grant.id, &|_| by_suite_in(ws))
                .await,
            Err(AgentError::AgentNotFound),
            "{what}"
        );
        assert!(
            store
                .count_grants_by_installation(org, ws)
                .await
                .unwrap()
                .iter()
                .all(|(installation, _)| *installation != a.installation),
            "{what}"
        );
    }

    // The agent's own scope, with another workspace's or organization's installation.
    for (installation, what) in [
        (a.sibling_installation, "sibling workspace's installation"),
        (b.installation, "other organization's installation"),
        (InstallationId::generate(), "an installation that doesn't exist"),
    ] {
        let other = create(stores, &a, what).await;
        assert_eq!(
            store
                .create_grant(
                    a.organization_id,
                    a.workspace,
                    other.id,
                    GrantId::generate(),
                    installation,
                    &tools,
                    &|_| by_suite_in(a.workspace)
                )
                .await,
            Err(AgentError::InstallationNotFound),
            "{what}"
        );
        assert_eq!(
            store
                .list_grants(a.organization_id, a.workspace, other.id)
                .await
                .unwrap(),
            []
        );
    }
    // A grant is reached only through its own agent.
    let other = create(stores, &a, "Another").await;
    assert_eq!(
        store
            .update_grant(a.organization_id, a.workspace, other.id, grant.id, &tools, &|_| {
                by_suite_in(a.workspace)
            })
            .await,
        Err(AgentError::GrantNotFound)
    );
    assert_eq!(
        store
            .delete_grant(a.organization_id, a.workspace, other.id, grant.id, &|_| by_suite_in(
                a.workspace
            ))
            .await,
        Err(AgentError::GrantNotFound)
    );
    assert_eq!(
        store
            .list_grants(a.organization_id, a.workspace, agent.id)
            .await
            .unwrap(),
        before,
        "nothing changed"
    );
    assert_eq!(
        store
            .find_agent(a.organization_id, a.workspace, agent.id)
            .await
            .unwrap()
            .map(|a| a.status),
        Some(AgentStatus::Active)
    );
}

async fn grants_only_what_the_registry_holds(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    // The second installation lists a tool the first doesn't.
    stores
        .registry
        .record_listing(
            org,
            ws,
            t.second_installation,
            &[listed("elsewhere", 1)],
            Timestamp::now(),
        )
        .await
        .unwrap();
    let agent = create(stores, &t, "Granted").await;
    let attempt = |tools: GrantTools| async move {
        store
            .create_grant(org, ws, agent.id, GrantId::generate(), t.installation, &tools, &|_| {
                by_suite_in(ws)
            })
            .await
    };
    for (tools, refusal) in [
        (
            grant_of(&["missing"], &[]),
            AgentError::UnknownTool("missing".to_owned()),
        ),
        (
            grant_of(&["elsewhere"], &[]),
            AgentError::UnknownTool("elsewhere".to_owned()),
        ),
        (
            grant_of(&["current", "gone"], &[]),
            AgentError::ToolGone("gone".to_owned()),
        ),
        // The first tool that fails decides, in the order given.
        (
            grant_of(&["missing", "gone"], &[]),
            AgentError::UnknownTool("missing".to_owned()),
        ),
        (
            grant_of(&["gone", "missing"], &[]),
            AgentError::ToolGone("gone".to_owned()),
        ),
        // Tools come before approvals.
        (grant_of(&["gone"], &["other"]), AgentError::ToolGone("gone".to_owned())),
        (
            grant_of(&["current"], &["changed"]),
            AgentError::ApprovalOutsideGrant("changed".to_owned()),
        ),
    ] {
        let what = format!("{tools:?}");
        assert_eq!(attempt(tools).await, Err(refusal), "{what}");
    }
    assert_eq!(
        store.list_grants(org, ws, agent.id).await.unwrap(),
        [],
        "a refused grant leaves nothing"
    );

    // A changed tool can be granted: calls are held to its pinned definition.
    let id = GrantId::generate();
    let grant = store
        .create_grant(
            org,
            ws,
            agent.id,
            id,
            t.installation,
            &grant_of(&["current", "changed"], &["changed"]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            grant.id,
            grant.organization_id,
            grant.workspace_id,
            grant.agent_id,
            grant.installation_id
        ),
        (id, org, ws, agent.id, t.installation)
    );
    assert_eq!(grant.tools, ["current", "changed"], "kept as given");
    assert_eq!(grant.approval_required, ["changed"]);

    // Changing it is held to the same rules, and a refused change changes nothing.
    for (tools, refusal) in [
        (
            grant_of(&["current", "gone"], &[]),
            AgentError::ToolGone("gone".to_owned()),
        ),
        (
            grant_of(&["elsewhere"], &[]),
            AgentError::UnknownTool("elsewhere".to_owned()),
        ),
        (
            grant_of(&["current"], &["changed"]),
            AgentError::ApprovalOutsideGrant("changed".to_owned()),
        ),
    ] {
        assert_eq!(
            store
                .update_grant(org, ws, agent.id, grant.id, &tools, &|_| by_suite_in(ws))
                .await,
            Err(refusal),
            "{tools:?}"
        );
    }
    assert_eq!(
        store.list_grants(org, ws, agent.id).await.unwrap(),
        std::slice::from_ref(&grant)
    );
    let updated = store
        .update_grant(org, ws, agent.id, grant.id, &grant_of(&["changed"], &[]), &|_| {
            by_suite_in(ws)
        })
        .await
        .unwrap();
    assert_eq!(
        (updated.tools.as_slice(), updated.approval_required.len()),
        (&["changed".to_owned()][..], 0)
    );
    assert_eq!(
        (updated.created_at, updated.installation_id),
        (grant.created_at, t.installation)
    );
    assert!(updated.updated_at >= grant.updated_at);
    assert_eq!(
        store
            .update_grant(
                org,
                ws,
                agent.id,
                GrantId::generate(),
                &grant_of(&["current"], &[]),
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::GrantNotFound)
    );

    // The second installation's own tool is grantable there.
    store
        .create_grant(
            org,
            ws,
            agent.id,
            GrantId::generate(),
            t.second_installation,
            &grant_of(&["elsewhere"], &["elsewhere"]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    let grants = store.list_grants(org, ws, agent.id).await.unwrap();
    assert_eq!(
        grants.iter().map(|grant| grant.installation_id).collect::<Vec<_>>(),
        [t.installation, t.second_installation],
        "oldest first"
    );
    assert_eq!(
        store
            .find_agent(org, ws, agent.id)
            .await
            .unwrap()
            .map(|agent| agent.grant_count),
        Some(2)
    );
    let mut counts = store.count_grants_by_installation(org, ws).await.unwrap();
    counts.sort_by_key(|(installation, _)| *installation);
    let mut expected = vec![(t.installation, 1), (t.second_installation, 1)];
    expected.sort_by_key(|(installation, _)| *installation);
    assert_eq!(counts, expected);

    store
        .delete_grant(org, ws, agent.id, grant.id, &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert_eq!(
        store
            .delete_grant(org, ws, agent.id, grant.id, &|_| by_suite_in(ws))
            .await,
        Err(AgentError::GrantNotFound)
    );
    assert_eq!(store.list_grants(org, ws, agent.id).await.unwrap().len(), 1);
}

/// Sets the organization's levels for the tenant's app, confirmed.
async fn set_levels(stores: &Stores<'_>, t: &Tenant, levels: &[(&str, ToolLevel)]) {
    let levels: Vec<(String, ToolLevel)> = levels
        .iter()
        .map(|(tool, level)| ((*tool).to_owned(), *level))
        .collect();
    stores
        .policies
        .set_levels(t.organization_id, t.app, &levels, None, Timestamp::now(), &by_suite)
        .await
        .unwrap();
}

async fn holds_grants_inside_the_organization_levels(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    set_levels(
        stores,
        &t,
        &[("current", ToolLevel::Deny), ("changed", ToolLevel::Approve)],
    )
    .await;
    let agent = create(stores, &t, "Held").await;
    let attempt = |tools: GrantTools| async move {
        store
            .create_grant(org, ws, agent.id, GrantId::generate(), t.installation, &tools, &|_| {
                by_suite_in(ws)
            })
            .await
    };
    for (tools, refusal) in [
        (
            grant_of(&["changed", "current"], &["changed"]),
            AgentError::ToolDenied("current".to_owned()),
        ),
        (
            grant_of(&["changed"], &[]),
            AgentError::ApprovalRequiredByPolicy("changed".to_owned()),
        ),
        // A denied tool comes before a missing approval, whatever the order given.
        (
            grant_of(&["changed", "current"], &[]),
            AgentError::ToolDenied("current".to_owned()),
        ),
        // The registry's rules and the grant's own come first.
        (
            grant_of(&["missing", "current"], &[]),
            AgentError::UnknownTool("missing".to_owned()),
        ),
        (
            grant_of(&["current", "gone"], &[]),
            AgentError::ToolGone("gone".to_owned()),
        ),
        (
            grant_of(&["current"], &["changed"]),
            AgentError::ApprovalOutsideGrant("changed".to_owned()),
        ),
    ] {
        let what = format!("{tools:?}");
        assert_eq!(attempt(tools).await, Err(refusal), "{what}");
    }
    assert_eq!(
        store.list_grants(org, ws, agent.id).await.unwrap(),
        [],
        "a grant outside the levels leaves nothing"
    );

    let held = grant_of(&["changed"], &["changed"]);
    let grant = attempt(held.clone()).await.unwrap();
    assert_eq!(
        (grant.tools.clone(), grant.approval_required.clone()),
        (held.tools, held.approval_required)
    );
    // The levels come before one grant per agent and installation.
    assert_eq!(
        attempt(grant_of(&["current"], &[])).await,
        Err(AgentError::ToolDenied("current".to_owned()))
    );
    assert_eq!(
        attempt(grant_of(&["changed"], &["changed"])).await,
        Err(AgentError::GrantExists)
    );
    // An update is held to them too, and a refused one changes nothing.
    for (tools, refusal) in [
        (
            grant_of(&["changed", "current"], &["changed", "current"]),
            AgentError::ToolDenied("current".to_owned()),
        ),
        (
            grant_of(&["changed"], &[]),
            AgentError::ApprovalRequiredByPolicy("changed".to_owned()),
        ),
    ] {
        assert_eq!(
            store
                .update_grant(org, ws, agent.id, grant.id, &tools, &|_| by_suite_in(ws))
                .await,
            Err(refusal)
        );
    }
    assert_eq!(store.list_grants(org, ws, agent.id).await.unwrap(), [grant]);

    // Unconfirmed, a tool takes the policy's level for new tools: approve by default.
    let other = create(stores, &t, "Other").await;
    let other_attempt = |tools: GrantTools| async move {
        store
            .create_grant(
                org,
                ws,
                other.id,
                GrantId::generate(),
                t.second_installation,
                &tools,
                &|_| by_suite_in(ws),
            )
            .await
    };
    stores
        .registry
        .record_listing(org, ws, t.second_installation, &[listed("fresh", 1)], Timestamp::now())
        .await
        .unwrap();
    assert_eq!(
        other_attempt(grant_of(&["fresh"], &[])).await,
        Err(AgentError::ApprovalRequiredByPolicy("fresh".to_owned()))
    );
    stores
        .policies
        .save_policy(
            org,
            PolicySettings {
                mode: ApprovalMode::Manual,
                new_tool_level: NewToolLevel::Deny,
            },
            None,
            Timestamp::now(),
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(
        other_attempt(grant_of(&["fresh"], &["fresh"])).await,
        Err(AgentError::ToolDenied("fresh".to_owned()))
    );
}

/// The store reads the levels in the transaction that writes the grant, not
/// a snapshot taken before it: a level that tightens after a caller would
/// have read it still refuses the grant.
async fn reads_the_levels_when_it_writes_the_grant(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    let agent = create(stores, &t, "Raced").await;
    // A caller reads `current` and `changed` as allowed, then they tighten.
    let found = stores
        .policies
        .find_levels(org, t.app, &["current".to_owned(), "changed".to_owned()])
        .await
        .unwrap();
    assert!(
        found
            .iter()
            .all(|level| level.confirmed && level.level == ToolLevel::Allow)
    );
    set_levels(
        stores,
        &t,
        &[("current", ToolLevel::Deny), ("changed", ToolLevel::Approve)],
    )
    .await;
    assert_eq!(
        store
            .create_grant(
                org,
                ws,
                agent.id,
                GrantId::generate(),
                t.installation,
                &grant_of(&["current"], &[]),
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::ToolDenied("current".to_owned()))
    );
    assert_eq!(
        store
            .create_grant(
                org,
                ws,
                agent.id,
                GrantId::generate(),
                t.installation,
                &grant_of(&["changed"], &[]),
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::ApprovalRequiredByPolicy("changed".to_owned()))
    );
    // And an update, after the grant was made under the looser levels.
    set_levels(stores, &t, &[("changed", ToolLevel::Allow)]).await;
    let grant = store
        .create_grant(
            org,
            ws,
            agent.id,
            GrantId::generate(),
            t.installation,
            &grant_of(&["changed"], &[]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    set_levels(stores, &t, &[("changed", ToolLevel::Approve)]).await;
    assert_eq!(
        store
            .update_grant(org, ws, agent.id, grant.id, &grant_of(&["changed"], &[]), &|_| {
                by_suite_in(ws)
            })
            .await,
        Err(AgentError::ApprovalRequiredByPolicy("changed".to_owned()))
    );
    set_levels(stores, &t, &[("changed", ToolLevel::Deny)]).await;
    assert_eq!(
        store
            .update_grant(
                org,
                ws,
                agent.id,
                grant.id,
                &grant_of(&["changed"], &["changed"]),
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::ToolDenied("changed".to_owned()))
    );
    assert_eq!(
        store.list_grants(org, ws, agent.id).await.unwrap(),
        [grant],
        "rewritten by no tightening"
    );
}

async fn keeps_one_grant_per_agent_and_installation(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    let first = create(stores, &t, "First").await;
    let second = create(stores, &t, "Second").await;
    let tools = grant_of(&["current"], &[]);
    store
        .create_grant(org, ws, first.id, GrantId::generate(), t.installation, &tools, &|_| {
            by_suite_in(ws)
        })
        .await
        .unwrap();
    assert_eq!(
        store
            .create_grant(
                org,
                ws,
                first.id,
                GrantId::generate(),
                t.installation,
                &grant_of(&["changed"], &[]),
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::GrantExists)
    );
    // Another agent may hold its own grant of the same installation.
    store
        .create_grant(org, ws, second.id, GrantId::generate(), t.installation, &tools, &|_| {
            by_suite_in(ws)
        })
        .await
        .unwrap();
    assert_eq!(store.list_grants(org, ws, first.id).await.unwrap().len(), 1);
    assert_eq!(
        store.count_grants_by_installation(org, ws).await.unwrap(),
        [(t.installation, 2)]
    );
}

async fn keeps_one_grant_when_two_are_made_at_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    let agent = create(stores, &t, "Racer").await;
    let tools = grant_of(&["current"], &[]);
    let placed = |_: &zupflow_core::agents::Grant| by_suite_in(ws);
    let (one, other) = tokio::join!(
        store.create_grant(org, ws, agent.id, GrantId::generate(), t.installation, &tools, &placed),
        store.create_grant(org, ws, agent.id, GrantId::generate(), t.installation, &tools, &placed),
    );
    // One is made; the other is told so, never that the store failed.
    let mut outcomes = [one.map(|_| ()), other.map(|_| ())];
    outcomes.sort_by_key(Result::is_err);
    assert_eq!(outcomes, [Ok(()), Err(AgentError::GrantExists)]);
    assert_eq!(store.list_grants(org, ws, agent.id).await.unwrap().len(), 1);
}

async fn changes_and_deletes_only_the_grant_named(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    registry_of(stores, &t, ws, t.second_installation).await;
    let agent = create(stores, &t, "Holder of two").await;
    let first = store
        .create_grant(
            org,
            ws,
            agent.id,
            GrantId::generate(),
            t.installation,
            &grant_of(&["current"], &[]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    let second = store
        .create_grant(
            org,
            ws,
            agent.id,
            GrantId::generate(),
            t.second_installation,
            &grant_of(&["current", "changed"], &["changed"]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    let changed = store
        .update_grant(
            org,
            ws,
            agent.id,
            first.id,
            &grant_of(&["current", "changed"], &["current"]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    let grants = store.list_grants(org, ws, agent.id).await.unwrap();
    assert_eq!(grants.len(), 2);
    assert!(grants.contains(&changed));
    assert!(grants.contains(&second), "the agent's other grant is unchanged");
    store
        .delete_grant(org, ws, agent.id, first.id, &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert_eq!(store.list_grants(org, ws, agent.id).await.unwrap(), [second]);
}

async fn leaves_a_grant_alone_when_its_tool_goes(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    let agent = create(stores, &t, "Holder").await;
    let grant = store
        .create_grant(
            org,
            ws,
            agent.id,
            GrantId::generate(),
            t.installation,
            &grant_of(&["current", "changed"], &["current"]),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    // The server stops listing `current`.
    stores
        .registry
        .record_listing(org, ws, t.installation, &[listed("changed", 2)], Timestamp::now())
        .await
        .unwrap();
    assert_eq!(
        store.list_grants(org, ws, agent.id).await.unwrap(),
        std::slice::from_ref(&grant),
        "the grant isn't rewritten"
    );
    assert_eq!(
        store
            .update_grant(
                org,
                ws,
                agent.id,
                grant.id,
                &grant_of(&["current", "changed"], &["current"]),
                &|_| by_suite_in(ws)
            )
            .await,
        Err(AgentError::ToolGone("current".to_owned())),
        "but the gone tool can't be granted again"
    );
    let trimmed = store
        .update_grant(org, ws, agent.id, grant.id, &grant_of(&["changed"], &[]), &|_| {
            by_suite_in(ws)
        })
        .await
        .unwrap();
    assert_eq!(trimmed.tools, ["changed"]);
}

async fn deletes_grants_with_their_agent_or_installation(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.workspace);
    let store = stores.agents;
    registry_of(stores, &t, ws, t.installation).await;
    registry_of(stores, &t, ws, t.second_installation).await;
    registry_of(stores, &t, t.sibling, t.sibling_installation).await;
    let tools = grant_of(&["current"], &[]);
    let gone = create(stores, &t, "Deleted").await;
    let kept = create(stores, &t, "Kept").await;
    for agent in [gone.id, kept.id] {
        for installation in [t.installation, t.second_installation] {
            store
                .create_grant(org, ws, agent, GrantId::generate(), installation, &tools, &|_| {
                    by_suite_in(ws)
                })
                .await
                .unwrap();
        }
    }
    let sibling_agent = stores
        .agents
        .create_agent(org, t.sibling, AgentId::generate(), &new_agent("Sibling"), &|_| {
            by_suite_in(t.sibling)
        })
        .await
        .unwrap();
    store
        .create_grant(
            org,
            t.sibling,
            sibling_agent.id,
            GrantId::generate(),
            t.sibling_installation,
            &tools,
            &|_| by_suite_in(t.sibling),
        )
        .await
        .unwrap();

    store
        .delete_agent(org, ws, gone.id, &|_| by_suite_in(ws))
        .await
        .unwrap();
    let mut counts = store.count_grants_by_installation(org, ws).await.unwrap();
    counts.sort_by_key(|(installation, _)| *installation);
    let mut expected = vec![(t.installation, 1), (t.second_installation, 1)];
    expected.sort_by_key(|(installation, _)| *installation);
    assert_eq!(counts, expected, "the deleted agent's grants went with it");
    assert_eq!(store.list_grants(org, ws, kept.id).await.unwrap().len(), 2);

    stores
        .installations
        .delete_installation(org, ws, t.installation, &|_| by_suite_in(ws))
        .await
        .unwrap();
    let grants = store.list_grants(org, ws, kept.id).await.unwrap();
    assert_eq!(
        grants.iter().map(|grant| grant.installation_id).collect::<Vec<_>>(),
        [t.second_installation],
        "the deleted installation's grant went with it"
    );
    assert_eq!(
        store
            .find_agent(org, ws, kept.id)
            .await
            .unwrap()
            .map(|agent| agent.grant_count),
        Some(1)
    );
    assert_eq!(
        store.list_grants(org, t.sibling, sibling_agent.id).await.unwrap().len(),
        1,
        "the sibling workspace's grants stay"
    );
}
