//! The `ChannelStore` suite: linked teams, generated apps, connections and
//! what they've seen, agent channels, sessions in threads, and the
//! organization's levels and settings, each reached only through its own
//! organization and workspace.
//!
//! It checks the rules that must hold across concurrent changes: one live
//! app per agent per manager, one live connection per app, an agent channel
//! only on its own agent's app, a manager's tokens replaced once per version
//! read, one active run per session, the conversation breaker's count of
//! the runs that start, and each message read once in a run session.
//! A message is read back byte for byte as it was recorded. Every change is
//! made only by an editor who still may, and an agent's apps go with it.
//! An app is made at least once: an attempt marks it before the provider is
//! asked, records the provider's ID at once, and a later attempt forgets it
//! only once it's deleted there; an app whose making job is gone or buried
//! is failed, and one still in flight gets a job to clean up after it.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, two Sandbox
//! workspaces and a Production one, an active admin, developer and viewer
//! with their users, an API key per environment, two agents in the Sandbox
//! workspace and one in each other workspace, and three queued runs of the
//! first agent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use zupflow_core::agents::{AgentChanges, AgentError, AgentStatus, AgentStore};
use zupflow_core::audit::AuditActor;
use zupflow_core::channels::handling::{DispatchReason, INBOUND_JOB, message_of_job};
use zupflow_core::channels::provider::{InboundEvent, NormalizedMessage};
use zupflow_core::channels::store::{
    AppCreation, AppFilter, ChannelEditor, ChannelError, ChannelStore, ConnectionChanges, ConnectionFilter,
    ConversationFilter, DeliveryOutcome, DispatchOutcome, LevelChange, MadeApp, MessageFilter, NewApp, NewConversation,
    NewDelivery, NewDispatch, NewInstall, NewManager, NewMessage, NewPerson, SealedVersion, SessionTemplate,
    TokenReplacement,
};
use zupflow_core::channels::thread::{STEER_HEADER, origin_keys};
use zupflow_core::channels::{
    AgentChannelSettings, AppStatus, BreakerSettings, ChannelMessage, ChannelProviderKind, ConnectTarget,
    ConnectionStatus, ConnectionTeam, ConversationKind, Direction, HandlerKind, Identity, InboundRef, ManagerMethod,
};
use zupflow_core::connect::{ConnectIntent, ConnectSessionStatus, ConnectStore, NewConnectSession, ReferenceHash};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AgentChannelId, AgentId, ApiKeyId, ChannelAppId, ChannelAppManagerId, ChannelConnectionId, ChannelConversationId,
    ChannelMessageId, ConnectSessionId, MemberId, ModelConnectionId, ModelId, OrganizationId, RunId, UserId,
    WorkspaceId,
};
use zupflow_core::jobs::{JobOptions, JobQueue};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::model_store::RunModel;
use zupflow_core::models::Message;
use zupflow_core::policy::ToolLevel;
use zupflow_core::runs::QueuedJob;
use zupflow_core::runs::RunTrigger;
use zupflow_core::runs::names::Offer;
use zupflow_core::runs::reducer::cancel;
use zupflow_core::runs::state::RequestId;
use zupflow_core::runs::store::{Applied, RunStore};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;

use crate::audit_store::{by_suite, by_suite_in};

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    /// Another Sandbox workspace of the same organization.
    pub sibling: WorkspaceId,
    pub production: WorkspaceId,
    pub admin: MemberId,
    pub admin_user: UserId,
    pub developer: MemberId,
    pub developer_user: UserId,
    pub viewer: MemberId,
    pub viewer_user: UserId,
    pub sandbox_key: ApiKeyId,
    pub production_key: ApiKeyId,
    /// Two agents of the Sandbox workspace.
    pub agent: AgentId,
    pub other_agent: AgentId,
    /// An agent of the sibling workspace, and one of the Production workspace.
    pub sibling_agent: AgentId,
    pub production_agent: AgentId,
    /// Queued runs of `agent`.
    pub runs: [RunId; 3],
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct Stores<'a> {
    pub channels: &'a dyn ChannelStore,
    pub agents: &'a dyn AgentStore,
    pub connect: &'a dyn ConnectStore,
    /// The queue `create_app` queues its job on, in the same store.
    pub jobs: &'a dyn JobQueue,
    /// The runs a session starts, in the same store.
    pub runs: &'a dyn RunStore,
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    links_teams_and_replaces_tokens_once_per_version(&stores, &prepare).await;
    lets_only_editors_who_still_may_change(&stores, &prepare).await;
    makes_one_live_app_per_agent_per_manager(&stores, &prepare).await;
    installs_one_live_connection_per_app(&stores, &prepare).await;
    holds_agent_channels_to_their_own_app_and_the_levels(&stores, &prepare).await;
    removes_apps_and_connections_keeping_history(&stores, &prepare).await;
    deletes_an_agents_apps_with_it(&stores, &prepare).await;
    records_messages_once_and_byte_for_byte(&stores, &prepare).await;
    records_deliveries_once(&stores, &prepare).await;
    starts_one_run_per_session(&stores, &prepare).await;
    counts_the_breaker_under_concurrency(&stores, &prepare).await;
    counts_runs_and_reads_each_message_once(&stores, &prepare).await;
    links_a_team_through_a_connect_session(&stores, &prepare).await;
    keeps_workspaces_and_organizations_apart(&stores, &prepare).await;
    makes_an_app_at_least_once(&stores, &prepare).await;
    settles_apps_whose_job_is_gone(&stores, &prepare).await;
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        sandbox: WorkspaceId::generate(),
        sibling: WorkspaceId::generate(),
        production: WorkspaceId::generate(),
        admin: MemberId::generate(),
        admin_user: UserId::generate(),
        developer: MemberId::generate(),
        developer_user: UserId::generate(),
        viewer: MemberId::generate(),
        viewer_user: UserId::generate(),
        sandbox_key: ApiKeyId::generate(),
        production_key: ApiKeyId::generate(),
        agent: AgentId::generate(),
        other_agent: AgentId::generate(),
        sibling_agent: AgentId::generate(),
        production_agent: AgentId::generate(),
        runs: [RunId::generate(), RunId::generate(), RunId::generate()],
    };
    prepare(tenant).await;
    tenant
}

fn now() -> Timestamp {
    Timestamp::now()
}

fn sealed(marker: u8) -> Sealed {
    Sealed {
        wrapped_key: WrappedKey {
            key_id: format!("key-{marker}"),
            bytes: vec![marker; 40],
        },
        nonce: [marker; 12],
        ciphertext: vec![marker; 48],
    }
}

fn new_manager(marker: u8) -> NewManager {
    NewManager {
        provider: ChannelProviderKind::Slack,
        method: ManagerMethod::ConfigurationToken,
        credential: sealed(marker),
        token_last4: format!("{marker:04}"),
        token_expires_at: None,
        created_by: None,
    }
}

fn job() -> QueuedJob {
    QueuedJob {
        kind: Name::new("conformance.channel_app").unwrap(),
        payload: b"{}".to_vec(),
        options: JobOptions::default(),
    }
}

fn developer(tenant: &Tenant) -> ChannelEditor {
    ChannelEditor::Member(tenant.developer)
}

async fn link(stores: &Stores<'_>, tenant: &Tenant, workspace_id: WorkspaceId) -> ChannelAppManagerId {
    let id = ChannelAppManagerId::generate();
    stores
        .channels
        .create_manager(
            tenant.organization_id,
            workspace_id,
            id,
            developer(tenant),
            &new_manager(1),
            now(),
            &|_| by_suite_in(workspace_id),
        )
        .await
        .expect("a developer links a team");
    id
}

async fn make_app(
    stores: &Stores<'_>,
    tenant: &Tenant,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
    manager_id: ChannelAppManagerId,
) -> Result<ChannelAppId, ChannelError> {
    let id = ChannelAppId::generate();
    stores
        .channels
        .create_app(
            tenant.organization_id,
            workspace_id,
            id,
            developer(tenant),
            &NewApp {
                agent_id,
                manager_id,
                inbound_ref: InboundRef::generate().unwrap(),
            },
            &job(),
            now(),
            &|_| by_suite_in(workspace_id),
        )
        .await
        .map(|app| app.id)
}

async fn made(stores: &Stores<'_>, tenant: &Tenant, workspace_id: WorkspaceId, app_id: ChannelAppId) {
    stores
        .channels
        .record_app_made(
            tenant.organization_id,
            workspace_id,
            app_id,
            &MadeApp {
                external_app_id: "A0CONFORM".to_owned(),
                client_id: "1.2".to_owned(),
                secrets: sealed(7),
            },
            now(),
        )
        .await
        .expect("the provider made the app");
}

fn install(connection_id: ChannelConnectionId, team: &str, version: u32) -> NewInstall {
    NewInstall {
        connection_id,
        team: ConnectionTeam {
            id: team.to_owned(),
            name: Some("Northwind".to_owned()),
            enterprise_id: None,
        },
        identity: Identity {
            external_id: "U0CONFORM".to_owned(),
            handle: "scribe".to_owned(),
            bot_id: None,
        },
        granted_scopes: vec!["chat:write".to_owned()],
        label: "Scribe".to_owned(),
        credential: SealedVersion {
            version,
            sealed: sealed(9),
        },
    }
}

/// A linked team, an installed app of `tenant.agent` and its connection.
async fn installed(
    stores: &Stores<'_>,
    tenant: &Tenant,
    workspace_id: WorkspaceId,
    agent_id: AgentId,
) -> (ChannelAppManagerId, ChannelAppId, ChannelConnectionId) {
    let manager = link(stores, tenant, workspace_id).await;
    let app = make_app(stores, tenant, workspace_id, agent_id, manager).await.unwrap();
    made(stores, tenant, workspace_id, app).await;
    let connection = ChannelConnectionId::generate();
    stores
        .channels
        .install_app(
            tenant.organization_id,
            workspace_id,
            app,
            &install(connection, "T0NORTH", 1),
            now(),
        )
        .await
        .expect("the app installs");
    (manager, app, connection)
}

fn message(external_id: &str, conversation: &str, text: &str, sent_at: i64) -> NewMessage {
    NewMessage {
        id: ChannelMessageId::generate(),
        conversation: NewConversation {
            external_id: conversation.to_owned(),
            kind: ConversationKind::Channel,
            name: Some(conversation.to_lowercase()),
            is_external: false,
            identity_is_member: true,
        },
        direction: Direction::Inbound,
        author: Some(NewPerson {
            external_id: "U0DANA".to_owned(),
            handle: Some("dana".to_owned()),
            display_name: Some("Dana".to_owned()),
            email: Some("dana@northwind.example".to_owned()),
            is_automated: false,
            team_id: Some("T0NORTH".to_owned()),
        }),
        external_id: external_id.to_owned(),
        thread_key: "1700000000.000100".to_owned(),
        text: text.to_owned(),
        mentions: vec!["U0CONFORM".to_owned()],
        run_id: None,
        tool_call_id: None,
        agent_hop: 0,
        sent_at: Timestamp::from_millis(sent_at),
    }
}

async fn links_teams_and_replaces_tokens_once_per_version(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let id = link(stores, &t, ws).await;
    let manager = stores.channels.find_manager(org, ws, id).await.unwrap().unwrap();
    assert_eq!(
        (
            manager.credential_version,
            manager.app_count,
            manager.configuration_token.last4.as_deref()
        ),
        (1, 0, Some("0001")),
        "a linked team starts at version 1 with no apps"
    );
    let stored = stores.channels.manager_credential(org, ws, id).await.unwrap().unwrap();
    assert_eq!(
        (stored.version, stored.sealed),
        (1, sealed(1)),
        "the tokens read back as sealed"
    );

    // Two writers that read version 1: one replaces it, the other is refused.
    let replace = |marker: u8| {
        let replacement = TokenReplacement {
            expected_version: 1,
            credential: sealed(marker),
            token_last4: format!("{marker:04}"),
            token_expires_at: None,
        };
        async move {
            stores
                .channels
                .replace_manager_tokens(org, ws, id, None, &replacement, now(), &by_suite)
                .await
        }
    };
    let (first, second) = tokio::join!(replace(2), replace(3));
    let replaced: Vec<_> = [first, second].into_iter().filter_map(Result::ok).collect();
    assert_eq!(
        replaced.len(),
        1,
        "one rotation per manager at a time: one of two concurrent replacements wins"
    );
    let stored = stores.channels.manager_credential(org, ws, id).await.unwrap().unwrap();
    assert_eq!(stored.version, 2, "the winner's tokens are version 2");
    assert_eq!(
        replace(4).await,
        Err(ChannelError::TokensChanged),
        "a replacement from a spent version is refused"
    );

    // Paged newest first, without repeating or skipping.
    let more = [link(stores, &t, ws).await, link(stores, &t, ws).await];
    let first = stores.channels.list_managers(org, ws, None, 2).await.unwrap();
    assert_eq!(first.total, 3);
    let next = first.next_cursor.clone().expect("a second page");
    let second = stores.channels.list_managers(org, ws, Some(&next), 2).await.unwrap();
    let ids: Vec<_> = first.items.iter().chain(&second.items).map(|m| m.id).collect();
    assert_eq!(
        ids,
        vec![more[1], more[0], id],
        "managers list newest first across pages"
    );
    assert!(second.next_cursor.is_none());
    assert_eq!(
        stores
            .channels
            .list_managers(org, ws, Some("not-a-cursor"), 2)
            .await
            .err(),
        Some(ChannelError::InvalidCursor)
    );
}

async fn lets_only_editors_who_still_may_change(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let attempt = |workspace_id: WorkspaceId, editor: ChannelEditor| async move {
        stores
            .channels
            .create_manager(
                org,
                workspace_id,
                ChannelAppManagerId::generate(),
                editor,
                &new_manager(1),
                now(),
                &|_| by_suite_in(workspace_id),
            )
            .await
            .map(|_| ())
    };
    assert_eq!(
        attempt(ws, ChannelEditor::Member(t.viewer)).await,
        Err(ChannelError::NotAllowed)
    );
    assert_eq!(
        attempt(ws, ChannelEditor::ApiKey(t.production_key)).await,
        Err(ChannelError::NotAllowed),
        "a Production key changes no Sandbox workspace's channels"
    );
    assert_eq!(
        attempt(t.production, ChannelEditor::ApiKey(t.sandbox_key)).await,
        Err(ChannelError::NotAllowed)
    );
    assert_eq!(attempt(ws, ChannelEditor::ApiKey(t.sandbox_key)).await, Ok(()));
    assert_eq!(
        attempt(t.production, ChannelEditor::ApiKey(t.production_key)).await,
        Ok(())
    );
    assert_eq!(attempt(ws, ChannelEditor::Member(t.admin)).await, Ok(()));
    assert_eq!(
        stores.channels.list_managers(org, ws, None, 100).await.unwrap().total,
        2,
        "a refused change writes nothing"
    );

    // The organization's levels and settings: admins only.
    let levels = [LevelChange {
        tool: "react".to_owned(),
        level: ToolLevel::Deny,
    }];
    for (member, user, expected) in [
        (t.developer, t.developer_user, Err(ChannelError::NotAllowed)),
        (t.admin, t.admin_user, Ok(())),
    ] {
        let set = stores
            .channels
            .set_tool_levels(org, member, ChannelProviderKind::Slack, &levels, user, now(), &by_suite)
            .await
            .map(|_| ());
        assert_eq!(set, expected);
        let saved = stores
            .channels
            .save_channel_settings(org, member, &BreakerSettings::default(), user, now(), &by_suite)
            .await
            .map(|_| ());
        assert_eq!(saved, expected);
    }
    let set: Vec<_> = stores
        .channels
        .list_tool_levels(org)
        .await
        .unwrap()
        .into_iter()
        .filter(|level| level.confirmed)
        .map(|level| (level.tool, level.level, level.updated_by.map(|user| user.id)))
        .collect();
    assert_eq!(set, vec![("react".to_owned(), ToolLevel::Deny, Some(t.admin_user))]);
    let settings = stores.channels.channel_settings(org).await.unwrap();
    assert_eq!(settings.updated_by.map(|user| user.id), Some(t.admin_user));
}

async fn makes_one_live_app_per_agent_per_manager(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let manager = link(stores, &t, ws).await;
    // Two concurrent creations for one agent in one team: one app.
    let (first, second) = tokio::join!(
        make_app(stores, &t, ws, t.agent, manager),
        make_app(stores, &t, ws, t.agent, manager)
    );
    let mut outcomes = [first.clone().map(|_| ()), second.clone().map(|_| ())];
    outcomes.sort_by_key(Result::is_err);
    assert_eq!(
        outcomes,
        [Ok(()), Err(ChannelError::AppExists)],
        "one live app per agent per manager"
    );
    let app_id = first.or(second).unwrap();
    let app = stores.channels.find_app(org, ws, app_id).await.unwrap().unwrap();
    assert_eq!((app.status, app.connection_id), (AppStatus::Creating, None));
    assert_eq!(
        stores
            .channels
            .find_manager(org, ws, manager)
            .await
            .unwrap()
            .unwrap()
            .app_count,
        1
    );
    let secrets = stores.channels.app_secrets(org, ws, app_id).await.unwrap().unwrap();
    assert!(
        secrets.secrets.is_none(),
        "no secrets before the provider makes the app"
    );
    assert_eq!(
        stores
            .channels
            .attribute_app_by_inbound_ref_cross_tenant(&secrets.inbound_ref)
            .await
            .unwrap(),
        Some((org, ws, app_id)),
        "an app's inbound reference finds its own tenant"
    );

    made(stores, &t, ws, app_id).await;
    let app = stores.channels.find_app(org, ws, app_id).await.unwrap().unwrap();
    assert_eq!(
        (app.status, app.external_app_id.as_deref()),
        (AppStatus::AwaitingInstall, Some("A0CONFORM"))
    );
    let secrets = stores.channels.app_secrets(org, ws, app_id).await.unwrap().unwrap();
    assert_eq!(
        secrets.secrets.map(|s| (s.version, s.sealed)),
        Some((1, sealed(7))),
        "the app's secrets read back sealed"
    );
    let again = stores
        .channels
        .record_app_made(
            org,
            ws,
            app_id,
            &MadeApp {
                external_app_id: "A0OTHER".to_owned(),
                client_id: "3.4".to_owned(),
                secrets: sealed(8),
            },
            now(),
        )
        .await;
    assert_eq!(again.err(), Some(ChannelError::AppNotReady), "an app is made once");

    // A failed app isn't live: the agent can be given another.
    let other = make_app(stores, &t, ws, t.other_agent, manager).await.unwrap();
    stores
        .channels
        .record_app_failed(org, ws, other, "name_taken", now())
        .await
        .unwrap();
    assert_eq!(
        stores
            .channels
            .find_app(org, ws, other)
            .await
            .unwrap()
            .map(|app| app.status),
        Some(AppStatus::Failed)
    );
    assert!(make_app(stores, &t, ws, t.other_agent, manager).await.is_ok());

    // The agent and the manager must both be the workspace's own.
    assert_eq!(
        make_app(stores, &t, ws, t.sibling_agent, manager).await,
        Err(ChannelError::AgentNotFound)
    );
    let sibling_manager = link(stores, &t, t.sibling).await;
    assert_eq!(
        make_app(stores, &t, ws, t.other_agent, sibling_manager).await,
        Err(ChannelError::ManagerNotFound)
    );
    let filtered = stores
        .channels
        .list_apps(
            org,
            ws,
            &AppFilter {
                agent_id: Some(t.agent),
                ..AppFilter::default()
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(filtered.items.iter().map(|a| a.id).collect::<Vec<_>>(), vec![app_id]);
    let nothing = AppFilter {
        matches_nothing: true,
        ..AppFilter::default()
    };
    assert_eq!(
        stores
            .channels
            .list_apps(org, ws, &nothing, None, 100)
            .await
            .unwrap()
            .total,
        0
    );
}

async fn installs_one_live_connection_per_app(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let manager = link(stores, &t, ws).await;
    let app = make_app(stores, &t, ws, t.agent, manager).await.unwrap();
    let early = stores
        .channels
        .install_app(
            org,
            ws,
            app,
            &install(ChannelConnectionId::generate(), "T0NORTH", 1),
            now(),
        )
        .await;
    assert_eq!(
        early.err(),
        Some(ChannelError::AppNotReady),
        "an app is installed once made"
    );
    made(stores, &t, ws, app).await;

    // Two concurrent installs of one app: one connection.
    let (a, b) = (ChannelConnectionId::generate(), ChannelConnectionId::generate());
    let (into_a, into_b) = (install(a, "T0NORTH", 1), install(b, "T0NORTH", 1));
    let (first, second) = tokio::join!(
        stores.channels.install_app(org, ws, app, &into_a, now()),
        stores.channels.install_app(org, ws, app, &into_b, now())
    );
    let won: Vec<_> = [first, second].into_iter().filter_map(Result::ok).collect();
    assert_eq!(won.len(), 1, "one live connection per app");
    let (connection, channel) = won.into_iter().next().unwrap();
    assert_eq!(connection.status, ConnectionStatus::Connected);
    assert_eq!((connection.agent_id, connection.app_id), (Some(t.agent), Some(app)));
    assert_eq!(
        (channel.agent_id, channel.connection_id, channel.settings),
        (t.agent, connection.id, AgentChannelSettings::default()),
        "the agent's channel on it starts at the defaults"
    );
    let read = stores.channels.find_app(org, ws, app).await.unwrap().unwrap();
    assert_eq!(
        (read.status, read.connection_id),
        (AppStatus::Installed, Some(connection.id))
    );
    let manager_read = stores.channels.find_manager(org, ws, manager).await.unwrap().unwrap();
    assert_eq!(
        manager_read.team.id.as_deref(),
        Some("T0NORTH"),
        "the manager learns its team"
    );

    // A reinstall updates the live connection, at the next version only.
    let stale = stores
        .channels
        .install_app(org, ws, app, &install(connection.id, "T0NORTH", 1), now())
        .await;
    assert_eq!(stale.err(), Some(ChannelError::InstallChanged));
    let elsewhere = stores
        .channels
        .install_app(
            org,
            ws,
            app,
            &install(ChannelConnectionId::generate(), "T0NORTH", 2),
            now(),
        )
        .await;
    assert_eq!(elsewhere.err(), Some(ChannelError::InstallChanged));
    let (again, _) = stores
        .channels
        .install_app(org, ws, app, &install(connection.id, "T0NORTH", 2), now())
        .await
        .unwrap();
    assert_eq!((again.id, again.credential_version), (connection.id, Some(2)));

    // Every install of the team's apps goes into the manager's team.
    let other = make_app(stores, &t, ws, t.other_agent, manager).await.unwrap();
    made(stores, &t, ws, other).await;
    let foreign = stores
        .channels
        .install_app(
            org,
            ws,
            other,
            &install(ChannelConnectionId::generate(), "T0OTHER", 1),
            now(),
        )
        .await;
    assert_eq!(foreign.err(), Some(ChannelError::TeamMismatch));
    let listed = stores
        .channels
        .list_connections(
            org,
            ws,
            &ConnectionFilter {
                agent_id: Some(t.agent),
                matches_nothing: false,
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(
        listed.items.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![connection.id]
    );
    let renamed = stores
        .channels
        .update_connection(
            org,
            ws,
            connection.id,
            developer(&t),
            &ConnectionChanges {
                label: Some("Support".to_owned()),
                handler: None,
            },
            now(),
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    assert_eq!(renamed.label, "Support");
}

async fn holds_agent_channels_to_their_own_app_and_the_levels(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, connection) = installed(stores, &t, ws, t.agent).await;
    let (_, _, sibling_connection) = installed(stores, &t, t.sibling, t.sibling_agent).await;
    let recorded = stores
        .channels
        .record_message(org, ws, connection, &message("1.1", "C0SUPPORT", "hi", 1_000), now())
        .await
        .unwrap();
    let foreign = stores
        .channels
        .record_message(
            org,
            t.sibling,
            sibling_connection,
            &message("1.1", "C0SUPPORT", "hi", 1_000),
            now(),
        )
        .await
        .unwrap();
    let put = |agent_id: AgentId, settings: AgentChannelSettings| async move {
        stores
            .channels
            .put_agent_channel(org, ws, agent_id, connection, developer(&t), &settings, now(), &|_| {
                by_suite_in(ws)
            })
            .await
    };
    let with = |tools: &[&str], approval: &[&str]| AgentChannelSettings {
        tools: tools.iter().map(|tool| (*tool).to_owned()).collect(),
        approval_required: approval.iter().map(|tool| (*tool).to_owned()).collect(),
        ..AgentChannelSettings::default()
    };
    assert_eq!(
        put(t.other_agent, with(&[], &[])).await.err(),
        Some(ChannelError::NotAgentsApp),
        "an agent channel only on its own agent's app"
    );
    assert_eq!(
        put(t.agent, with(&["delete_message"], &["delete_message"])).await.err(),
        Some(ChannelError::ToolDenied("delete_message".to_owned()))
    );
    assert_eq!(
        put(t.agent, with(&["send_message"], &[])).await.err(),
        Some(ChannelError::ApprovalRequiredByPolicy("send_message".to_owned()))
    );
    let listed = |conversation: ChannelConversationId| AgentChannelSettings {
        audience_conversations: vec![conversation],
        ..AgentChannelSettings::default()
    };
    assert_eq!(
        put(t.agent, listed(foreign.message.conversation_id)).await.err(),
        Some(ChannelError::UnknownConversation),
        "another workspace's conversation isn't this connection's"
    );
    let saved = put(t.agent, listed(recorded.message.conversation_id)).await.unwrap();
    let mut settings = with(&["reply", "send_message"], &["send_message"]);
    settings.audience_conversations = vec![recorded.message.conversation_id];
    let changed = put(t.agent, settings.clone()).await.unwrap();
    assert_eq!(
        (changed.id, changed.settings),
        (saved.id, settings),
        "a put changes the one channel"
    );

    // The organization's levels decide.
    stores
        .channels
        .set_tool_levels(
            org,
            t.admin,
            ChannelProviderKind::Slack,
            &[LevelChange {
                tool: "reply".to_owned(),
                level: ToolLevel::Deny,
            }],
            t.admin_user,
            now(),
            &by_suite,
        )
        .await
        .unwrap();
    assert_eq!(
        put(t.agent, with(&["reply"], &[])).await.err(),
        Some(ChannelError::ToolDenied("reply".to_owned()))
    );
    let channels = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap();
    assert_eq!(channels.len(), 1);
    let audit = |_: &zupflow_core::channels::AgentChannel| by_suite_in(ws);
    let delete = || {
        stores
            .channels
            .delete_agent_channel(org, ws, t.agent, connection, developer(&t), &audit)
    };
    assert_eq!(
        stores
            .channels
            .delete_agent_channel(org, ws, t.other_agent, connection, developer(&t), &|_| by_suite_in(ws))
            .await,
        Err(ChannelError::AgentChannelNotFound)
    );
    assert_eq!(delete().await, Ok(()));
    assert_eq!(delete().await, Err(ChannelError::AgentChannelNotFound));
    assert!(
        stores
            .channels
            .list_agent_channels(org, ws, t.agent)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn removes_apps_and_connections_keeping_history(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (manager, app, connection) = installed(stores, &t, ws, t.agent).await;
    let recorded = stores
        .channels
        .record_message(org, ws, connection, &message("1.1", "C0SUPPORT", "hi", 1_000), now())
        .await
        .unwrap();
    let audit_manager = |_: &zupflow_core::channels::ChannelAppManager| by_suite_in(ws);
    let audit_app = |_: &zupflow_core::channels::ChannelApp| by_suite_in(ws);
    let unlink = || {
        stores
            .channels
            .delete_manager(org, ws, manager, developer(&t), &audit_manager)
    };
    assert_eq!(unlink().await, Err(ChannelError::ManagerInUse(1)));
    let remove = || {
        stores
            .channels
            .delete_app(org, ws, app, developer(&t), now(), &audit_app)
    };
    assert_eq!(remove().await, Ok(()));
    assert_eq!(remove().await, Err(ChannelError::AppNotFound), "a deleted app is gone");
    assert!(stores.channels.find_app(org, ws, app).await.unwrap().is_none());
    assert!(stores.channels.app_secrets(org, ws, app).await.unwrap().is_none());
    let disconnected = stores
        .channels
        .find_connection(org, ws, connection)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        disconnected.status,
        ConnectionStatus::Disconnected,
        "its connection is kept, disconnected"
    );
    let history = stores
        .channels
        .list_messages(
            org,
            ws,
            connection,
            recorded.message.conversation_id,
            &MessageFilter::default(),
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(history.items, vec![recorded.message], "its history stays readable");
    assert!(
        stores
            .channels
            .list_agent_channels(org, ws, t.agent)
            .await
            .unwrap()
            .is_empty()
    );
    let put = stores
        .channels
        .put_agent_channel(
            org,
            ws,
            t.agent,
            connection,
            developer(&t),
            &AgentChannelSettings::default(),
            now(),
            &|_| by_suite_in(ws),
        )
        .await;
    assert_eq!(put.err(), Some(ChannelError::ConnectionDisconnected));
    assert_eq!(unlink().await, Ok(()), "a team unlinks once its apps are gone");
    assert!(stores.channels.find_manager(org, ws, manager).await.unwrap().is_none());
    assert!(
        stores
            .channels
            .manager_credential(org, ws, manager)
            .await
            .unwrap()
            .is_none()
    );

    // Removing a connection removes what it saw; its app waits for an install.
    let (_, app, connection) = installed(stores, &t, ws, t.other_agent).await;
    stores
        .channels
        .record_message(org, ws, connection, &message("1.1", "C0SUPPORT", "hi", 1_000), now())
        .await
        .unwrap();
    stores
        .channels
        .delete_connection(org, ws, connection, developer(&t), now(), &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert!(
        stores
            .channels
            .find_connection(org, ws, connection)
            .await
            .unwrap()
            .is_none()
    );
    let conversations = stores
        .channels
        .list_conversations(org, ws, connection, &ConversationFilter::default(), None, 100)
        .await
        .unwrap();
    assert_eq!(conversations.total, 0);
    let app = stores.channels.find_app(org, ws, app).await.unwrap().unwrap();
    assert_eq!((app.status, app.connection_id), (AppStatus::AwaitingInstall, None));
}

async fn deletes_an_agents_apps_with_it(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (manager, app, connection) = installed(stores, &t, ws, t.agent).await;
    stores
        .agents
        .delete_agent(org, ws, t.agent, &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert!(stores.channels.find_app(org, ws, app).await.unwrap().is_none());
    assert_eq!(
        stores
            .channels
            .find_connection(org, ws, connection)
            .await
            .unwrap()
            .map(|c| c.status),
        Some(ConnectionStatus::Disconnected)
    );
    assert_eq!(
        stores
            .channels
            .find_manager(org, ws, manager)
            .await
            .unwrap()
            .unwrap()
            .app_count,
        0
    );
    assert_eq!(
        stores.channels.list_agent_channels(org, ws, t.agent).await,
        Err(ChannelError::AgentNotFound)
    );
    assert_eq!(
        stores.agents.delete_agent(org, ws, t.agent, &|_| by_suite_in(ws)).await,
        Err(AgentError::AgentNotFound)
    );
}

async fn records_messages_once_and_byte_for_byte(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, app, connection) = installed(stores, &t, ws, t.agent).await;
    let text = format!(
        "<@U0CONFORM> café\r\n\t\"quoted\" 'single' \\ backslash é (e + combining) עברית 🧪👩🏽‍💻 {}",
        "x".repeat(30_000)
    );
    let first = stores
        .channels
        .record_message(org, ws, connection, &message("1.1", "C0SUPPORT", &text, 2_000), now())
        .await
        .unwrap();
    assert!(first.new);
    assert_eq!(
        first.message.text.as_bytes(),
        text.as_bytes(),
        "a message reads back byte for byte"
    );
    let author = first
        .message
        .author
        .clone()
        .expect("an inbound message names its author");
    assert_eq!(author.display_name.as_deref(), Some("Dana"));
    let again = stores
        .channels
        .record_message(
            org,
            ws,
            connection,
            &message("1.1", "C0SUPPORT", "changed", 3_000),
            now(),
        )
        .await
        .unwrap();
    assert_eq!(
        (again.new, &again.message),
        (false, &first.message),
        "a message is recorded once"
    );
    let reply = stores
        .channels
        .record_message(
            org,
            ws,
            connection,
            &message("1.2", "C0SUPPORT", "second", 4_000),
            now(),
        )
        .await
        .unwrap();
    assert_eq!(
        reply.message.author.map(|a| a.id),
        Some(author.id),
        "a person is saved once"
    );
    let mut dm = message("9.9", "D0DANA", "direct", 1_500);
    dm.conversation.kind = ConversationKind::Dm;
    dm.conversation.name = None;
    stores
        .channels
        .record_message(org, ws, connection, &dm, now())
        .await
        .unwrap();

    let conversation = first.message.conversation_id;
    let page = stores
        .channels
        .list_messages(org, ws, connection, conversation, &MessageFilter::default(), None, 1)
        .await
        .unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.items[0].id, reply.message.id, "newest first");
    let rest = stores
        .channels
        .list_messages(
            org,
            ws,
            connection,
            conversation,
            &MessageFilter::default(),
            page.next_cursor.as_deref(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(
        rest.items,
        vec![first.message.clone()],
        "read back through a page, byte for byte"
    );
    let thread = MessageFilter {
        thread: Some("elsewhere".to_owned()),
    };
    assert_eq!(
        stores
            .channels
            .list_messages(org, ws, connection, conversation, &thread, None, 10)
            .await
            .unwrap()
            .total,
        0
    );
    let all = stores
        .channels
        .list_conversations(org, ws, connection, &ConversationFilter::default(), None, 10)
        .await
        .unwrap();
    assert_eq!(
        all.items.iter().map(|c| c.external_id.as_str()).collect::<Vec<_>>(),
        vec!["C0SUPPORT", "D0DANA"],
        "most recent message first"
    );
    let search = ConversationFilter {
        q: Some("SUPP".to_owned()),
        kind: None,
    };
    let found = stores
        .channels
        .list_conversations(org, ws, connection, &search, None, 10)
        .await
        .unwrap();
    assert_eq!(
        found.items.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
        vec![Some("c0support".to_owned())]
    );
    let dms = ConversationFilter {
        q: None,
        kind: Some(ConversationKind::Dm),
    };
    assert_eq!(
        stores
            .channels
            .list_conversations(org, ws, connection, &dms, None, 10)
            .await
            .unwrap()
            .total,
        1
    );

    // A provider's event is taken once per app.
    let record = || {
        stores
            .channels
            .record_inbound_event(org, ws, app, "Ev01", "message", now())
    };
    assert_eq!(record().await, Ok(true));
    assert_eq!(record().await, Ok(false));
    assert_eq!(
        stores
            .channels
            .record_inbound_event(org, t.sibling, app, "Ev02", "message", now())
            .await,
        Err(ChannelError::AppNotFound)
    );
}

fn template() -> SessionTemplate {
    SessionTemplate {
        model: RunModel {
            model_id: ModelId::generate(),
            connection_id: ModelConnectionId::generate(),
            provider_model_id: "model-a".to_owned(),
        },
        instructions: "You are answering in Slack.\n\nBe brief.".to_owned(),
        offer: Offer::default(),
        history_budget: 1_000,
    }
}

/// A dispatch of the message with `external_id` in conversation C0SUPPORT.
fn dispatch_of(message: ChannelMessageId, agent_channel_id: AgentChannelId, external_id: &str) -> NewDispatch {
    let (origin_key, thread_origin_key) = origin_keys("T0NORTH", "C0SUPPORT", external_id, "1700000000.000100");
    NewDispatch {
        message_id: message,
        agent_channel_id,
        origin_key,
        thread_origin_key,
        agent_hop: 0,
        handler: HandlerKind::Mention,
        reason: DispatchReason::Mentioned,
    }
}

async fn recorded(
    stores: &Stores<'_>,
    t: &Tenant,
    connection: ChannelConnectionId,
    external_id: &str,
    text: &str,
) -> ChannelMessage {
    stores
        .channels
        .record_message(
            t.organization_id,
            t.sandbox,
            connection,
            &message(external_id, "C0SUPPORT", text, 1_000),
            now(),
        )
        .await
        .unwrap()
        .message
}

async fn starts_one_run_per_session(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, connection) = installed(stores, &t, ws, t.agent).await;
    let channel = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap()[0].clone();
    let mut messages = Vec::new();
    for (n, text) in ["first", "second", "third"].iter().enumerate() {
        messages.push(recorded(stores, &t, connection, &format!("1.{n}"), text).await);
    }
    let template = template();
    let (template_ref, messages_ref, channel_id) = (&template, &messages, channel.id);
    let dispatch = |n: usize| {
        let dispatch = dispatch_of(messages_ref[n].id, channel_id, &format!("1.{n}"));
        async move {
            stores
                .channels
                .dispatch(org, ws, &dispatch, Some(template_ref), now())
                .await
        }
    };
    let (a, b, c) = tokio::join!(dispatch(0), dispatch(1), dispatch(2));
    let outcomes = [a.unwrap(), b.unwrap(), c.unwrap()];
    let started: Vec<RunId> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            DispatchOutcome::Started { run_id } => Some(*run_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        started.len(),
        1,
        "one run per session under concurrent dispatches: {outcomes:?}"
    );
    // Each other one waits for the run, or, when the run's turn read it
    // already (its message is older than the one that started it), is read.
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, DispatchOutcome::Waiting | DispatchOutcome::AlreadyRead))
            .count(),
        2,
        "{outcomes:?}"
    );
    assert_eq!(
        dispatch(0).await,
        Ok(DispatchOutcome::Duplicate),
        "one dispatch per agent and message"
    );
    let first = started[0];
    let session = stores
        .channels
        .find_session(org, ws, channel.id, messages[0].conversation_id, "1700000000.000100")
        .await
        .unwrap()
        .expect("the thread's session");
    assert_eq!((session.active_run_id, session.run_count), (Some(first), 1));

    let run = stores.runs.find_run(org, ws, first).await.unwrap().unwrap();
    assert_eq!(run.trigger, RunTrigger::Channel);
    assert_eq!(run.session_id, session.session_id);
    let from = run.channel.clone().expect("where the run came from");
    assert_eq!(
        (from.connection_id, from.thread_key.as_str()),
        (connection, "1700000000.000100")
    );
    assert!(run.input.contains("[Dana] "), "{}", run.input);
    let snapshot = stores.runs.find_snapshot(org, ws, first).await.unwrap().unwrap();
    let context = snapshot.session.clone().expect("a channel run's session");
    assert!(context.history.is_empty(), "the session's first run continues nothing");
    assert_eq!(context.offer, template.offer);
    assert_eq!(snapshot.instructions, template.instructions);

    // The messages that waited are steered into the run at its next turn.
    let steer = stores.channels.steerable(org, ws, first).await.unwrap();
    let waiting = 3 - run.input.matches("[Dana]").count();
    if waiting > 0 {
        let steer = steer.expect("waiting messages to steer");
        assert!(steer.text.starts_with(STEER_HEADER), "{}", steer.text);
        assert_eq!(steer.dispatches.len(), waiting);
    }

    // The run's end frees the session and collects what waited into one run.
    let cancel = cancel(
        org,
        ws,
        &snapshot,
        RequestId::from_uuid(uuid::Uuid::now_v7()),
        RunTrigger::Api,
        AuditActor::ApiKey {
            id: t.sandbox_key,
            name: "Test".to_owned(),
        },
    )
    .unwrap();
    assert!(matches!(
        stores.runs.apply(org, ws, &cancel).await,
        Ok(Applied::Committed(_))
    ));
    let after = stores
        .channels
        .find_session(org, ws, channel.id, messages[0].conversation_id, "1700000000.000100")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.last_run_id, Some(first));
    if waiting == 0 {
        assert_eq!(after.active_run_id, None);
        return;
    }
    let collected = after.active_run_id.expect("a run collecting what waited");
    assert_ne!(collected, first);
    assert_eq!(after.run_count, 2);
    let next = stores.runs.find_snapshot(org, ws, collected).await.unwrap().unwrap();
    assert_eq!(
        next.session.expect("its session").history,
        vec![Message::User {
            text: run.input.clone()
        }],
        "the next run continues the session byte for byte"
    );
    assert_eq!(
        stores
            .runs
            .find_run(org, ws, collected)
            .await
            .unwrap()
            .unwrap()
            .session_id,
        run.session_id
    );
    assert_eq!(
        stores.channels.steerable(org, ws, first).await,
        Ok(None),
        "an ended run takes nothing in"
    );
}

/// A message in its own thread of C0SUPPORT.
async fn threaded(
    stores: &Stores<'_>,
    t: &Tenant,
    connection: ChannelConnectionId,
    external_id: &str,
) -> ChannelMessage {
    let mut new = message(external_id, "C0SUPPORT", "again", 1_000);
    new.thread_key = external_id.to_owned();
    stores
        .channels
        .record_message(t.organization_id, t.sandbox, connection, &new, now())
        .await
        .unwrap()
        .message
}

async fn counts_the_breaker_under_concurrency(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, connection) = installed(stores, &t, ws, t.agent).await;
    let channel = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap()[0].clone();
    // Each in its own thread: each dispatch starts its own session's run.
    let mut messages = Vec::new();
    for n in 0..8 {
        messages.push(threaded(stores, &t, connection, &format!("2.{n}")).await);
    }
    let conversation = messages[0].conversation_id;
    let settings = BreakerSettings {
        breaker_runs: 5,
        ..BreakerSettings::default()
    };
    stores
        .channels
        .save_channel_settings(org, t.admin, &settings, t.admin_user, now(), &by_suite)
        .await
        .unwrap();
    let template = template();
    let dispatches: Vec<NewDispatch> = (0..8)
        .map(|n| dispatch_of(messages[n].id, channel.id, &format!("2.{n}")))
        .collect();
    let outcomes = futures_join_all(
        dispatches
            .iter()
            .map(|dispatch| stores.channels.dispatch(org, ws, dispatch, Some(&template), now())),
    )
    .await;
    let (mut started, mut tripped, mut paused) = (0, 0, 0);
    for outcome in outcomes {
        match outcome.unwrap() {
            DispatchOutcome::Started { .. } => started += 1,
            DispatchOutcome::Paused { tripped: true, .. } => tripped += 1,
            DispatchOutcome::Paused { tripped: false, .. } => paused += 1,
            other => panic!("each dispatch is new, and its session idle: {other:?}"),
        }
    }
    assert_eq!(
        (started, tripped, paused),
        (5, 1, 2),
        "the breaker counts every run once, trips once and holds the pause"
    );
    let read = stores
        .channels
        .find_conversation(org, ws, connection, conversation)
        .await
        .unwrap()
        .unwrap();
    assert!(read.paused_until.is_some());
}

/// The breaker counts the runs that start, collected ones included, never
/// the messages they take in; a message an earlier turn read is settled as
/// read; and nothing is dispatched to an agent that isn't active.
async fn counts_runs_and_reads_each_message_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, connection) = installed(stores, &t, ws, t.agent).await;
    let channel = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap()[0].clone();
    let settings = BreakerSettings {
        breaker_runs: 2,
        ..BreakerSettings::default()
    };
    stores
        .channels
        .save_channel_settings(org, t.admin, &settings, t.admin_user, now(), &by_suite)
        .await
        .unwrap();
    let mut messages = Vec::new();
    for (n, text) in ["first", "second", "third", "fourth"].iter().enumerate() {
        messages.push(recorded(stores, &t, connection, &format!("3.{n}"), text).await);
    }
    let template = template();
    let dispatch = |n: usize| {
        let dispatch = dispatch_of(messages[n].id, channel.id, &format!("3.{n}"));
        let template = &template;
        async move {
            stores
                .channels
                .dispatch(org, ws, &dispatch, Some(template), now())
                .await
        }
    };
    // The second's job runs first: its turn reads the first too.
    let Ok(DispatchOutcome::Started { run_id: first }) = dispatch(1).await else {
        panic!("the session's first run");
    };
    let input = stores.runs.find_run(org, ws, first).await.unwrap().unwrap().input;
    assert_eq!(
        (input.matches("first").count(), input.matches("second").count()),
        (1, 1),
        "{input}"
    );
    assert!(!input.contains("not to you"), "both mention the agent: {input}");
    assert_eq!(
        dispatch(0).await,
        Ok(DispatchOutcome::AlreadyRead),
        "the first's late job finds it read"
    );
    // Two more wait, and one collected run takes both in: one run counted.
    assert_eq!(dispatch(2).await, Ok(DispatchOutcome::Waiting));
    assert_eq!(dispatch(3).await, Ok(DispatchOutcome::Waiting));
    let snapshot = stores.runs.find_snapshot(org, ws, first).await.unwrap().unwrap();
    let cancel = cancel(
        org,
        ws,
        &snapshot,
        RequestId::from_uuid(uuid::Uuid::now_v7()),
        RunTrigger::Api,
        AuditActor::ApiKey {
            id: t.sandbox_key,
            name: "Test".to_owned(),
        },
    )
    .unwrap();
    assert!(matches!(
        stores.runs.apply(org, ws, &cancel).await,
        Ok(Applied::Committed(_))
    ));
    let session = stores
        .channels
        .find_session(org, ws, channel.id, messages[0].conversation_id, "1700000000.000100")
        .await
        .unwrap()
        .unwrap();
    let collected = session.active_run_id.expect("one run collecting both");
    let next = stores.runs.find_run(org, ws, collected).await.unwrap().unwrap();
    assert!(
        next.input.contains("third") && next.input.contains("fourth") && !next.input.contains("second"),
        "{}",
        next.input
    );
    let conversation = stores
        .channels
        .find_conversation(org, ws, connection, messages[0].conversation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conversation.paused_until, None, "two runs, within the breaker");
    // The ended run's transcript continues the next one.
    let context = stores
        .runs
        .find_snapshot(org, ws, collected)
        .await
        .unwrap()
        .unwrap()
        .session
        .unwrap();
    assert_eq!(context.history, vec![Message::User { text: input }]);

    // A third run, in another thread, goes over it.
    let other = threaded(stores, &t, connection, "3.9").await;
    let Ok(DispatchOutcome::Paused { tripped: true, .. }) = stores
        .channels
        .dispatch(
            org,
            ws,
            &dispatch_of(other.id, channel.id, "3.9"),
            Some(&template),
            now(),
        )
        .await
    else {
        panic!("the third run trips the breaker");
    };

    // Nothing reaches an agent that's paused, however the job read it.
    stores
        .agents
        .update_agent(
            org,
            ws,
            t.agent,
            &AgentChanges {
                status: Some(AgentStatus::Paused),
                ..AgentChanges::default()
            },
            &|_| by_suite_in(ws),
        )
        .await
        .unwrap();
    let late = threaded(stores, &t, connection, "3.10").await;
    assert_eq!(
        stores
            .channels
            .dispatch(
                org,
                ws,
                &dispatch_of(late.id, channel.id, "3.10"),
                Some(&template),
                now()
            )
            .await,
        Ok(DispatchOutcome::Inactive)
    );
}

/// A delivery is recorded once per provider event, read only through the
/// app's live connection in its own team, and a new message queues its
/// inbound job in the same transaction.
async fn records_deliveries_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, app, connection) = installed(stores, &t, ws, t.agent).await;
    let normalized = |ts: &str, author: &str| NormalizedMessage {
        conversation_external_id: "C0SUPPORT".to_owned(),
        conversation_kind: ConversationKind::Channel,
        conversation_is_external: false,
        author_external_id: Some(author.to_owned()),
        author_bot_id: None,
        author_team_id: Some("T0NORTH".to_owned()),
        author_is_automated: false,
        external_id: ts.to_owned(),
        thread_key: ts.to_owned(),
        text: "<@U0CONFORM> hi".to_owned(),
        mentions: vec!["U0CONFORM".to_owned()],
        sent_at: Timestamp::from_millis(5_000),
    };
    let delivery = |id: &str, team: Option<&str>, event: InboundEvent| NewDelivery {
        provider_event_id: id.to_owned(),
        kind: "message".to_owned(),
        team_id: team.map(str::to_owned),
        event,
    };
    let deliver =
        |delivery: NewDelivery| async move { stores.channels.record_delivery(org, ws, app, &delivery, now()).await };
    let first = deliver(delivery(
        "Ev1",
        Some("T0NORTH"),
        InboundEvent::Message(Box::new(normalized("5.1", "U0DANA"))),
    ))
    .await;
    let Ok(DeliveryOutcome::Message {
        connection_id,
        message_id,
        new: true,
    }) = first
    else {
        panic!("a new message: {first:?}");
    };
    assert_eq!(connection_id, connection);
    assert_eq!(
        deliver(delivery(
            "Ev1",
            Some("T0NORTH"),
            InboundEvent::Message(Box::new(normalized("5.1", "U0DANA")))
        ))
        .await,
        Ok(DeliveryOutcome::Duplicate)
    );
    assert_eq!(
        deliver(delivery(
            "Ev2",
            Some("T0NORTH"),
            InboundEvent::Message(Box::new(normalized("5.1", "U0DANA")))
        ))
        .await,
        Ok(DeliveryOutcome::Message {
            connection_id,
            message_id,
            new: false
        }),
        "the same message delivered as another event is saved once"
    );
    for team in [Some("T0ELSEWHERE"), None] {
        assert_eq!(
            deliver(delivery(
                &format!("Ev-{team:?}"),
                team,
                InboundEvent::Message(Box::new(normalized("5.2", "U0DANA")))
            ))
            .await,
            Ok(DeliveryOutcome::Ignored),
            "another team's event, or one naming none"
        );
    }
    let own = deliver(delivery(
        "Ev3",
        Some("T0NORTH"),
        InboundEvent::Message(Box::new(normalized("5.3", "U0CONFORM"))),
    ))
    .await;
    let Ok(DeliveryOutcome::Message { message_id: own, .. }) = own else {
        panic!("its own message: {own:?}");
    };
    let context = stores
        .channels
        .inbound_context(org, ws, message_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.message.direction, Direction::Inbound);
    assert_eq!(context.author_external_id.as_deref(), Some("U0DANA"));
    assert!(!context.handled && context.agent_active);
    assert_eq!(context.channel.map(|channel| channel.agent_id), Some(t.agent));
    assert_eq!(
        stores
            .channels
            .inbound_context(org, ws, own)
            .await
            .unwrap()
            .unwrap()
            .message
            .direction,
        Direction::Outbound
    );
    stores.channels.mark_handled(org, ws, message_id, now()).await.unwrap();
    assert!(
        stores
            .channels
            .inbound_context(org, ws, message_id)
            .await
            .unwrap()
            .unwrap()
            .handled
    );
    // Each new message queued its inbound job, naming it alone.
    let kind = Name::new(INBOUND_JOB).unwrap();
    let mut queued = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        assert_eq!(job.scope, Scope::Workspace(org, ws));
        queued.push(message_of_job(&job.payload).unwrap());
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    }
    assert!(queued.contains(&message_id) && queued.contains(&own), "{queued:?}");
    assert_eq!(queued.len(), 2);
    // Its own posts by its bot ID too: learned from the first that carries
    // its user and its bot ID, and known by the bot ID alone after.
    let direction_of = |outcome: Result<DeliveryOutcome, ChannelError>| async move {
        let Ok(DeliveryOutcome::Message { message_id, .. }) = outcome else {
            panic!("a message: {outcome:?}");
        };
        let context = stores
            .channels
            .inbound_context(org, ws, message_id)
            .await
            .unwrap()
            .unwrap();
        (context.message.direction, context)
    };
    let mut both = normalized("5.6", "U0CONFORM");
    both.author_bot_id = Some("B0CONFORM".to_owned());
    both.author_is_automated = true;
    let (direction, _) =
        direction_of(deliver(delivery("Ev6", Some("T0NORTH"), InboundEvent::Message(Box::new(both)))).await).await;
    assert_eq!(direction, Direction::Outbound);
    assert_eq!(
        stores
            .channels
            .find_connection(org, ws, connection)
            .await
            .unwrap()
            .unwrap()
            .identity
            .bot_id
            .as_deref(),
        Some("B0CONFORM")
    );
    let mut alone = normalized("5.7", "B0CONFORM");
    alone.author_bot_id = Some("B0CONFORM".to_owned());
    alone.author_is_automated = true;
    let (direction, _) =
        direction_of(deliver(delivery("Ev7", Some("T0NORTH"), InboundEvent::Message(Box::new(alone)))).await).await;
    assert_eq!(direction, Direction::Outbound, "its own post, by its bot ID alone");
    // Someone of another team, in a shared conversation: their team and the
    // conversation's sharing are kept.
    let mut guest = normalized("5.8", "U0GUEST");
    guest.author_team_id = Some("T0PARTNER".to_owned());
    guest.conversation_is_external = true;
    let (direction, context) =
        direction_of(deliver(delivery("Ev8", Some("T0NORTH"), InboundEvent::Message(Box::new(guest)))).await).await;
    assert_eq!(direction, Direction::Inbound);
    assert_eq!(context.author_team_id.as_deref(), Some("T0PARTNER"));
    assert!(context.conversation.is_external);
    let (_, context) = direction_of(
        deliver(delivery(
            "Ev9",
            Some("T0NORTH"),
            InboundEvent::Message(Box::new(normalized("5.9", "U0DANA"))),
        ))
        .await,
    )
    .await;
    assert!(context.conversation.is_external, "once shared, it stays so");
    assert_eq!(context.author_team_id.as_deref(), Some("T0NORTH"));
    // Uninstalled in the team: the connection needs reconnecting, and reads no more messages.
    assert_eq!(
        deliver(delivery("Ev4", Some("T0NORTH"), InboundEvent::Uninstalled)).await,
        Ok(DeliveryOutcome::Uninstalled)
    );
    assert_eq!(
        stores
            .channels
            .find_connection(org, ws, connection)
            .await
            .unwrap()
            .unwrap()
            .status,
        ConnectionStatus::ReconnectRequired
    );
    assert_eq!(
        deliver(delivery(
            "Ev5",
            Some("T0NORTH"),
            InboundEvent::Message(Box::new(normalized("5.5", "U0DANA")))
        ))
        .await,
        Ok(DeliveryOutcome::Ignored)
    );
}

/// Joins futures without a dependency: polled together on one task.
async fn futures_join_all<F: Future>(futures: impl Iterator<Item = F>) -> Vec<F::Output> {
    let futures: Vec<_> = futures.map(Box::pin).collect();
    let mut outputs: Vec<Option<F::Output>> = futures.iter().map(|_| None).collect();
    let mut futures: Vec<Option<_>> = futures.into_iter().map(Some).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (slot, output) in futures.iter_mut().zip(outputs.iter_mut()) {
            if let Some(future) = slot {
                match future.as_mut().poll(cx) {
                    std::task::Poll::Ready(value) => {
                        *output = Some(value);
                        *slot = None;
                    }
                    std::task::Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    })
    .await;
    outputs.into_iter().map(|output| output.unwrap()).collect()
}

async fn links_a_team_through_a_connect_session(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let session_id = ConnectSessionId::generate();
    let new = |marker: u8| NewConnectSession {
        environment: Environment::Sandbox,
        app: None,
        target: Some(ConnectTarget::ChannelAppManager {
            provider: ChannelProviderKind::Slack,
            manager: None,
        }),
        configuration_version: 0,
        intent: ConnectIntent::Create,
        installation_id: None,
        label: "Slack".to_owned(),
        return_url: None,
        requested_scopes: Vec::new(),
        reference_hash: ReferenceHash([marker; 32]),
    };
    stores
        .connect
        .create_session(org, ws, session_id, &new(1), now())
        .await
        .unwrap();
    let manager_id = ChannelAppManagerId::generate();
    let link = |manager_id: ChannelAppManagerId, workspace_id: WorkspaceId| async move {
        stores
            .channels
            .link_team_through_session(
                org,
                workspace_id,
                session_id,
                manager_id,
                &new_manager(1),
                now(),
                &move |_| by_suite_in(workspace_id),
            )
            .await
    };
    assert_eq!(
        link(ChannelAppManagerId::generate(), t.sibling).await.err(),
        Some(ChannelError::SessionNotFound),
        "a session is found only in its own workspace"
    );
    let (session, manager) = link(manager_id, ws).await.unwrap();
    assert_eq!(session.status, ConnectSessionStatus::Completed);
    assert_eq!(
        session.target,
        Some(ConnectTarget::ChannelAppManager {
            provider: ChannelProviderKind::Slack,
            manager: Some(manager_id),
        })
    );
    assert_eq!((manager.id, manager.created_by), (manager_id, None));
    assert_eq!(
        link(ChannelAppManagerId::generate(), ws).await.err(),
        Some(ChannelError::SessionNotPending),
        "a session links one team"
    );
    assert_eq!(
        stores.channels.list_managers(org, ws, None, 100).await.unwrap().total,
        1
    );
}

async fn keeps_workspaces_and_organizations_apart(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (manager, app, connection) = installed(stores, &t, ws, t.agent).await;
    let recorded = stores
        .channels
        .record_message(org, ws, connection, &message("1.1", "C0SUPPORT", "hi", 1_000), now())
        .await
        .unwrap();
    let conversation = recorded.message.conversation_id;
    let channel = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap()[0].clone();
    let started = stores
        .channels
        .dispatch(
            org,
            ws,
            &dispatch_of(recorded.message.id, channel.id, "1.1"),
            Some(&template()),
            now(),
        )
        .await
        .unwrap();
    let DispatchOutcome::Started { run_id } = started else {
        panic!("a run: {started:?}");
    };
    let c = stores.channels;
    // The same IDs, through the sibling workspace of the same organization
    // and through another organization's own workspace: nothing.
    for (org, ws) in [
        (org, t.sibling),
        (other.organization_id, other.sandbox),
        (other.organization_id, ws),
    ] {
        let place = format!("{org} {ws}");
        assert!(c.find_manager(org, ws, manager).await.unwrap().is_none(), "{place}");
        assert!(
            c.manager_credential(org, ws, manager).await.unwrap().is_none(),
            "{place}"
        );
        assert_eq!(c.list_managers(org, ws, None, 100).await.unwrap().total, 0, "{place}");
        assert!(c.find_app(org, ws, app).await.unwrap().is_none(), "{place}");
        assert!(c.app_secrets(org, ws, app).await.unwrap().is_none(), "{place}");
        assert_eq!(
            c.list_apps(org, ws, &AppFilter::default(), None, 100)
                .await
                .unwrap()
                .total,
            0
        );
        assert!(
            c.find_connection(org, ws, connection).await.unwrap().is_none(),
            "{place}"
        );
        assert_eq!(
            c.list_connections(org, ws, &ConnectionFilter::default(), None, 100)
                .await
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            c.list_conversations(org, ws, connection, &ConversationFilter::default(), None, 100)
                .await
                .unwrap()
                .total,
            0
        );
        assert!(
            c.find_conversation(org, ws, connection, conversation)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            c.list_messages(org, ws, connection, conversation, &MessageFilter::default(), None, 100)
                .await
                .unwrap()
                .total,
            0
        );
        assert_eq!(
            c.list_agent_channels(org, ws, t.agent).await,
            Err(ChannelError::AgentNotFound)
        );
        assert_eq!(
            c.find_session(org, ws, channel.id, conversation, "1700000000.000100")
                .await,
            Ok(None),
            "{place}"
        );
        assert_eq!(
            c.dispatch(
                org,
                ws,
                &dispatch_of(recorded.message.id, channel.id, "9.9"),
                Some(&template()),
                now()
            )
            .await,
            Err(ChannelError::ConversationNotFound),
            "{place}"
        );
        assert_eq!(c.steerable(org, ws, run_id).await, Ok(None), "{place}");
        assert_eq!(
            c.inbound_context(org, ws, recorded.message.id).await,
            Ok(None),
            "{place}"
        );
        assert_eq!(
            c.record_delivery(
                org,
                ws,
                app,
                &NewDelivery {
                    provider_event_id: "Ev-elsewhere".to_owned(),
                    kind: "message".to_owned(),
                    team_id: Some("T0NORTH".to_owned()),
                    event: InboundEvent::Uninstalled,
                },
                now()
            )
            .await,
            Err(ChannelError::AppNotFound),
            "{place}"
        );
        assert_eq!(
            c.install_app(
                org,
                ws,
                app,
                &install(ChannelConnectionId::generate(), "T0NORTH", 1),
                now()
            )
            .await
            .err(),
            Some(ChannelError::AppNotFound)
        );
        assert_eq!(
            c.record_message(org, ws, connection, &message("2.2", "C0SUPPORT", "x", 1), now())
                .await
                .err(),
            Some(ChannelError::ConnectionNotFound)
        );
    }
    // Another organization's editor, through this workspace: refused.
    let theirs = ChannelEditor::Member(other.developer);
    assert_eq!(
        c.delete_manager(org, ws, manager, theirs, &|_| by_suite_in(ws)).await,
        Err(ChannelError::NotAllowed)
    );
    assert_eq!(
        c.delete_app(org, ws, app, ChannelEditor::ApiKey(other.sandbox_key), now(), &|_| {
            by_suite_in(ws)
        })
        .await,
        Err(ChannelError::NotAllowed)
    );
    // The same workspace's editor, on the sibling workspace's nothing.
    let sibling = t.sibling;
    assert_eq!(
        c.delete_app(org, sibling, app, developer(&t), now(), &|_| by_suite_in(sibling))
            .await,
        Err(ChannelError::AppNotFound)
    );
    assert_eq!(
        c.delete_connection(org, sibling, connection, developer(&t), now(), &|_| by_suite_in(
            sibling
        ))
        .await,
        Err(ChannelError::ConnectionNotFound)
    );
    assert_eq!(
        c.put_agent_channel(
            org,
            sibling,
            t.agent,
            connection,
            developer(&t),
            &AgentChannelSettings::default(),
            now(),
            &|_| by_suite_in(sibling)
        )
        .await
        .err(),
        Some(ChannelError::AgentNotFound)
    );
    // The organization's levels and settings stay its own.
    c.set_tool_levels(
        org,
        t.admin,
        ChannelProviderKind::Slack,
        &[LevelChange {
            tool: "react".to_owned(),
            level: ToolLevel::Deny,
        }],
        t.admin_user,
        now(),
        &by_suite,
    )
    .await
    .unwrap();
    assert!(
        !c.list_tool_levels(other.organization_id)
            .await
            .unwrap()
            .iter()
            .any(|l| l.confirmed)
    );
    assert_eq!(
        c.set_tool_levels(
            other.organization_id,
            t.admin,
            ChannelProviderKind::Slack,
            &[],
            t.admin_user,
            now(),
            &by_suite
        )
        .await
        .err(),
        Some(ChannelError::NotAllowed),
        "an admin of one organization changes no other's levels"
    );
    assert!(
        c.channel_settings(other.organization_id)
            .await
            .unwrap()
            .updated_at
            .is_none()
    );
    // Nothing reached through the other workspace changed this one's.
    assert_eq!(c.list_managers(org, ws, None, 100).await.unwrap().total, 1);
    assert_eq!(
        c.find_connection(org, ws, connection).await.unwrap().map(|c| c.status),
        Some(ConnectionStatus::Connected)
    );
}

fn creation_of(creation: Option<AppCreation>) -> (AppStatus, Option<String>, bool) {
    let creation = creation.expect("the app's making is read whatever its status");
    (creation.app.status, creation.app.external_app_id, creation.in_flight)
}

fn made_as(external_app_id: &str) -> MadeApp {
    MadeApp {
        external_app_id: external_app_id.to_owned(),
        client_id: "1.2".to_owned(),
        secrets: sealed(7),
    }
}

async fn makes_an_app_at_least_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let channels = stores.channels;
    let manager = link(stores, &t, ws).await;
    let app = make_app(stores, &t, ws, t.agent, manager).await.unwrap();
    let state = || async { creation_of(channels.app_creation(org, ws, app).await.unwrap()) };
    assert_eq!(state().await, (AppStatus::Creating, None, false));
    assert_eq!(
        channels.record_app_created(org, ws, app, "A0FIRST", now()).await,
        Err(ChannelError::AppNotReady),
        "an ID is recorded only for an attempt that marked the app first"
    );

    channels.begin_app_creation(org, ws, app, now()).await.unwrap();
    assert_eq!(state().await, (AppStatus::Creating, None, true));
    channels
        .record_app_created(org, ws, app, "A0FIRST", now())
        .await
        .unwrap();
    assert_eq!(
        channels.record_app_created(org, ws, app, "A0SECOND", now()).await,
        Err(ChannelError::AppNotReady),
        "one provider ID recorded at a time"
    );
    assert_eq!(
        channels.begin_app_creation(org, ws, app, now()).await,
        Err(ChannelError::AppNotReady),
        "an earlier attempt's app is deleted and forgotten before another is made"
    );
    assert_eq!(
        channels
            .record_app_made(org, ws, app, &made_as("A0OTHER"), now())
            .await
            .err(),
        Some(ChannelError::AppNotReady),
        "made as another app than the one recorded"
    );
    // Forgetting another ID than the recorded one keeps it.
    channels
        .forget_app_creation(org, ws, app, Some("A0OTHER"), now())
        .await
        .unwrap();
    assert_eq!(state().await, (AppStatus::Creating, Some("A0FIRST".to_owned()), true));
    channels
        .forget_app_creation(org, ws, app, Some("A0FIRST"), now())
        .await
        .unwrap();
    assert_eq!(state().await, (AppStatus::Creating, None, false));

    // Another workspace's or organization's IDs reach nothing.
    for (organization_id, workspace_id) in [(org, t.sibling), (OrganizationId::generate(), ws)] {
        assert_eq!(
            channels.app_creation(organization_id, workspace_id, app).await,
            Ok(None)
        );
        assert_eq!(
            channels
                .begin_app_creation(organization_id, workspace_id, app, now())
                .await,
            Err(ChannelError::AppNotReady)
        );
        assert_eq!(
            channels
                .record_app_created(organization_id, workspace_id, app, "A0ELSE", now())
                .await,
            Err(ChannelError::AppNotReady)
        );
        channels
            .forget_app_creation(organization_id, workspace_id, app, None, now())
            .await
            .unwrap();
        assert_eq!(
            channels
                .record_app_made(organization_id, workspace_id, app, &made_as("A0ELSE"), now())
                .await
                .err(),
            Some(ChannelError::AppNotFound)
        );
    }

    channels.begin_app_creation(org, ws, app, now()).await.unwrap();
    channels
        .record_app_created(org, ws, app, "A0SECOND", now())
        .await
        .unwrap();
    channels
        .record_app_made(org, ws, app, &made_as("A0SECOND"), now())
        .await
        .unwrap();
    assert_eq!(
        state().await,
        (AppStatus::AwaitingInstall, Some("A0SECOND".to_owned()), false),
        "made: settled"
    );
    channels.forget_app_creation(org, ws, app, None, now()).await.unwrap();
    assert_eq!(
        state().await,
        (AppStatus::AwaitingInstall, Some("A0SECOND".to_owned()), false),
        "a made app isn't forgotten"
    );

    // Deleted while in flight: still read, so what was made can be deleted.
    let other = make_app(stores, &t, ws, t.other_agent, manager).await.unwrap();
    channels.begin_app_creation(org, ws, other, now()).await.unwrap();
    channels
        .record_app_created(org, ws, other, "A0GONE", now())
        .await
        .unwrap();
    channels
        .delete_app(org, ws, other, developer(&t), now(), &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert_eq!(channels.find_app(org, ws, other).await, Ok(None));
    assert_eq!(
        creation_of(channels.app_creation(org, ws, other).await.unwrap()),
        (AppStatus::Deleted, Some("A0GONE".to_owned()), true)
    );
    assert_eq!(
        channels
            .record_app_made(org, ws, other, &made_as("A0GONE"), now())
            .await
            .err(),
        Some(ChannelError::AppNotFound)
    );
    channels
        .forget_app_creation(org, ws, other, Some("A0GONE"), now())
        .await
        .unwrap();
    assert_eq!(
        creation_of(channels.app_creation(org, ws, other).await.unwrap()),
        (AppStatus::Deleted, None, false)
    );
}

async fn settles_apps_whose_job_is_gone(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let channels = stores.channels;
    let manager = link(stores, &t, ws).await;
    let make = |agent_id: AgentId, kind: &'static str| async move {
        let id = ChannelAppId::generate();
        channels
            .create_app(
                org,
                ws,
                id,
                developer(&t),
                &NewApp {
                    agent_id,
                    manager_id: manager,
                    inbound_ref: InboundRef::generate().unwrap(),
                },
                &QueuedJob {
                    kind: Name::new(kind).unwrap(),
                    payload: b"{}".to_vec(),
                    options: JobOptions::default(),
                },
                now(),
                &|_| by_suite_in(ws),
            )
            .await
            .unwrap();
        id
    };
    let claim = |kind: &'static str| async move {
        stores
            .jobs
            .claim_next(&[Name::new(kind).unwrap()], std::time::Duration::from_secs(60))
            .await
            .unwrap()
    };
    // Its last attempt buried with the app in flight; the other's job
    // finished with the app still being made.
    let buried = make(t.agent, "conformance.reap_buried").await;
    let finished = make(t.other_agent, "conformance.reap_finished").await;
    channels.begin_app_creation(org, ws, buried, now()).await.unwrap();
    let job = claim("conformance.reap_buried").await.unwrap();
    assert_eq!(job.scope, Scope::Workspace(org, ws));
    stores
        .jobs
        .bury(&job.scope, job.id, &job.lease, &Name::new("lease_expired").unwrap())
        .await
        .unwrap();
    let job = claim("conformance.reap_finished").await.unwrap();
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();

    let cleanup = |_: ChannelAppId| QueuedJob {
        kind: Name::new("conformance.reap_cleanup").unwrap(),
        payload: b"{}".to_vec(),
        options: JobOptions::default(),
    };
    let reaped = channels
        .reap_stranded_apps_cross_tenant(&cleanup, 100, now())
        .await
        .unwrap();
    assert!(reaped >= 2, "both settled: {reaped}");
    for (app, in_flight) in [(buried, true), (finished, false)] {
        let creation = channels.app_creation(org, ws, app).await.unwrap().unwrap();
        assert_eq!(
            (
                creation.app.status,
                creation.app.last_error.as_deref(),
                creation.in_flight
            ),
            (AppStatus::Failed, Some("provisioning_abandoned"), in_flight),
            "failed, so the agent can be given another"
        );
    }
    assert!(make_app(stores, &t, ws, t.agent, manager).await.is_ok());
    // Only the one in flight gets a job, in its own workspace, to delete what was made.
    let job = claim("conformance.reap_cleanup").await.expect("a cleanup job");
    assert_eq!(job.scope, Scope::Workspace(org, ws));
    assert!(claim("conformance.reap_cleanup").await.is_none());
    // While that job lives, nothing more; once it's gone with the app still in flight, another.
    channels
        .reap_stranded_apps_cross_tenant(&cleanup, 100, now())
        .await
        .unwrap();
    assert!(claim("conformance.reap_cleanup").await.is_none());
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    channels
        .reap_stranded_apps_cross_tenant(&cleanup, 100, now())
        .await
        .unwrap();
    let job = claim("conformance.reap_cleanup").await.expect("cleaning up again");
    // Cleaned up: settled for good.
    channels
        .forget_app_creation(org, ws, buried, None, now())
        .await
        .unwrap();
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    channels
        .reap_stranded_apps_cross_tenant(&cleanup, 100, now())
        .await
        .unwrap();
    assert!(claim("conformance.reap_cleanup").await.is_none());
    assert_eq!(
        creation_of(channels.app_creation(org, ws, buried).await.unwrap()),
        (AppStatus::Failed, None, false)
    );
}
