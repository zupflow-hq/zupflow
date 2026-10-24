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
//! An app is installed through the Connect session that asked for it, as
//! one with the session's completion; a made app that's deleted gets a job
//! to remove it at its provider; and a team's tokens get one rotation job
//! per version, or expire when they lapse unrotated.
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
use zupflow_core::channels::handling::{CATCH_UP_JOB, DispatchReason, INBOUND_JOB, catch_up_of_job, message_of_job};
use zupflow_core::channels::provider::{
    DeletedMessage, EditedMessage, InboundEvent, JoinedConversation, NormalizedMessage, ProviderPerson,
};
use zupflow_core::channels::reply::{ChannelReply, REPLY_JOB};
use zupflow_core::channels::store::{
    AppCreation, AppFilter, ChannelEditor, ChannelError, ChannelStore, ConnectionChanges, ConnectionFilter,
    ConversationFilter, DeliveryOutcome, DispatchOutcome, LevelChange, MadeApp, MessageFilter, NewApp, NewConversation,
    NewDelivery, NewDispatch, NewInstall, NewManager, NewMessage, NewPerson, PersonLookup, SealedVersion, SendSlot,
    SessionTemplate, TokenReplacement,
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
    ChannelMessageId, ConnectSessionId, MemberId, ModelConnectionId, ModelId, OrganizationId, RunId, ToolCallId,
    UserId, WorkspaceId,
};
use zupflow_core::jobs::{JobOptions, JobQueue};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::model_store::{RouteState, RunModel};
use zupflow_core::models::Message;
use zupflow_core::models::{Completion, Reply, TokenUsage};
use zupflow_core::policy::ToolLevel;
use zupflow_core::runs::QueuedJob;
use zupflow_core::runs::RunTrigger;
use zupflow_core::runs::names::Offer;
use zupflow_core::runs::reducer::{Facts, ModelAnswer, ModelResult, advance, cancel};
use zupflow_core::runs::state::{ClaimantId, Continuation, InvocationId, RequestId};
use zupflow_core::runs::store::{Applied, RunStore};
use zupflow_core::runs::transition::Operation;
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
    catches_up_only_on_its_own_join(&stores, &prepare).await;
    starts_one_run_per_session(&stores, &prepare).await;
    counts_the_breaker_under_concurrency(&stores, &prepare).await;
    counts_runs_and_reads_each_message_once(&stores, &prepare).await;
    links_a_team_through_a_connect_session(&stores, &prepare).await;
    keeps_workspaces_and_organizations_apart(&stores, &prepare).await;
    makes_an_app_at_least_once(&stores, &prepare).await;
    settles_apps_whose_job_is_gone(&stores, &prepare).await;
    installs_an_app_through_its_connect_session(&stores, &prepare).await;
    removes_a_made_app_at_its_provider_too(&stores, &prepare).await;
    syncs_an_agents_apps_with_its_changes(&stores, &prepare).await;
    rotates_a_teams_tokens_once_per_version(&stores, &prepare).await;
    reads_people_once_and_replies_only_to_answers(&stores, &prepare).await;
    replies_to_an_answer_until_it_is_sent(&stores, &prepare).await;
    acts_only_within_its_connection_and_sweeps_past_retention(&stores, &prepare).await;
    reserves_send_slots_a_second_apart_per_connection(&stores, &prepare).await;
    searches_direct_messages_only_where_readable(&stores, &prepare).await;
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
        secrets.provider,
        ChannelProviderKind::Slack,
        "an app's secrets name the provider that reads its deliveries"
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
    // Made at its provider: the team is what removes it there, so it stays
    // linked until the app is released.
    assert_eq!(unlink().await, Err(ChannelError::RemovalsPending(1)));
    assert_eq!(stores.channels.record_app_released(org, ws, app, now()).await, Ok(true));
    assert_eq!(
        stores.channels.record_app_released(org, ws, app, now()).await,
        Ok(false)
    );
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
    // A run that ended without an answer has nothing to post.
    assert_eq!(stores.channels.reply_of(org, ws, first).await, Ok(None));
    assert!(
        claim_in(stores, REPLY_JOB, Scope::Workspace(org, ws)).await.is_none(),
        "no reply queued for a cancelled run"
    );
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

/// A connection's own join to a conversation queues its catch-up job, in its
/// own workspace and naming its own app and connection, in the transaction
/// that records it; anyone else's join, another team's, a repeated event,
/// and a join to a connection that isn't connected queue nothing; and a
/// join is never recorded through another workspace's scope.
async fn catches_up_only_on_its_own_join(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, app, connection) = installed(stores, &t, ws, t.agent).await;
    let (_, sibling_app, _) = installed(stores, &t, t.sibling, t.sibling_agent).await;
    let joined = |member: &str| JoinedConversation {
        conversation_external_id: "C0NEW".to_owned(),
        conversation_kind: ConversationKind::PrivateChannel,
        conversation_is_external: false,
        member_external_id: member.to_owned(),
        joined_at: Timestamp::from_millis(9_000),
    };
    let delivery = |id: &str, team: &str, member: &str| NewDelivery {
        provider_event_id: id.to_owned(),
        kind: "member_joined_channel".to_owned(),
        team_id: Some(team.to_owned()),
        event: InboundEvent::Joined(Box::new(joined(member))),
    };
    let deliver = |workspace_id: WorkspaceId, app_id: ChannelAppId, delivery: NewDelivery| async move {
        stores
            .channels
            .record_delivery(org, workspace_id, app_id, &delivery, now())
            .await
    };
    assert_eq!(
        deliver(ws, app, delivery("EvJ1", "T0NORTH", "U0DANA")).await,
        Ok(DeliveryOutcome::Ignored),
        "someone else's join"
    );
    assert_eq!(
        deliver(ws, app, delivery("EvJ2", "T0ELSEWHERE", "U0CONFORM")).await,
        Ok(DeliveryOutcome::Ignored),
        "another team's"
    );
    assert_eq!(
        deliver(t.sibling, app, delivery("EvJ3", "T0NORTH", "U0CONFORM")).await,
        Err(ChannelError::AppNotFound),
        "never through another workspace"
    );
    assert_eq!(
        deliver(ws, app, delivery("EvJ4", "T0NORTH", "U0CONFORM")).await,
        Ok(DeliveryOutcome::Joined {
            connection_id: connection
        })
    );
    assert_eq!(
        deliver(ws, app, delivery("EvJ4", "T0NORTH", "U0CONFORM")).await,
        Ok(DeliveryOutcome::Duplicate)
    );
    let job = claim_in(stores, CATCH_UP_JOB, Scope::Workspace(org, ws))
        .await
        .expect("its catch-up job");
    let catch_up = catch_up_of_job(&job.payload).expect("names its join");
    assert_eq!(
        (catch_up.app_id, catch_up.connection_id, &catch_up.conversation),
        (app, connection, &joined("U0CONFORM"))
    );
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    assert!(
        claim_in(stores, CATCH_UP_JOB, Scope::Workspace(org, ws))
            .await
            .is_none(),
        "one job, for its own join alone"
    );
    assert!(
        claim_in(stores, CATCH_UP_JOB, Scope::Workspace(org, t.sibling))
            .await
            .is_none(),
        "nothing in the sibling workspace"
    );
    // The sibling's own join is its own: in its workspace, naming its app.
    assert!(matches!(
        deliver(t.sibling, sibling_app, delivery("EvJ5", "T0NORTH", "U0CONFORM")).await,
        Ok(DeliveryOutcome::Joined { .. })
    ));
    let job = claim_in(stores, CATCH_UP_JOB, Scope::Workspace(org, t.sibling))
        .await
        .expect("the sibling's catch-up job");
    assert_eq!(
        catch_up_of_job(&job.payload).map(|catch_up| catch_up.app_id),
        Some(sibling_app)
    );
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    // Not connected any more: its join queues nothing.
    assert_eq!(
        deliver(
            ws,
            app,
            NewDelivery {
                provider_event_id: "EvJ7".to_owned(),
                kind: "app_uninstalled".to_owned(),
                team_id: Some("T0NORTH".to_owned()),
                event: InboundEvent::Uninstalled,
            }
        )
        .await,
        Ok(DeliveryOutcome::Uninstalled)
    );
    assert_eq!(
        deliver(ws, app, delivery("EvJ8", "T0NORTH", "U0CONFORM")).await,
        Ok(DeliveryOutcome::Ignored)
    );
    assert!(
        claim_in(stores, CATCH_UP_JOB, Scope::Workspace(org, ws))
            .await
            .is_none()
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

/// Claims jobs of `kind` until one of `scope`'s, finishing every other: the
/// suite's tenants share one queue. The job claimed, if any.
async fn claim_in(stores: &Stores<'_>, kind: &str, scope: Scope) -> Option<zupflow_core::jobs::Job> {
    let kinds = [Name::new(kind).unwrap()];
    while let Some(job) = stores
        .jobs
        .claim_next(&kinds, std::time::Duration::from_secs(60))
        .await
        .unwrap()
    {
        if job.scope == scope {
            return Some(job);
        }
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    }
    None
}

async fn installs_an_app_through_its_connect_session(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let manager = link(stores, &t, ws).await;
    let app = make_app(stores, &t, ws, t.agent, manager).await.unwrap();
    made(stores, &t, ws, app).await;
    let other_app = make_app(stores, &t, ws, t.other_agent, manager).await.unwrap();
    made(stores, &t, ws, other_app).await;
    let session_for = |app: ChannelAppId, marker: u8| async move {
        let session_id = ConnectSessionId::generate();
        stores
            .connect
            .create_session(
                org,
                ws,
                session_id,
                &NewConnectSession {
                    environment: Environment::Sandbox,
                    app: None,
                    target: Some(ConnectTarget::ChannelApp {
                        provider: ChannelProviderKind::Slack,
                        app,
                        connection: None,
                    }),
                    configuration_version: 0,
                    intent: ConnectIntent::Create,
                    installation_id: None,
                    label: "Scribe".to_owned(),
                    return_url: None,
                    requested_scopes: Vec::new(),
                    reference_hash: ReferenceHash([marker; 32]),
                },
                now(),
            )
            .await
            .unwrap();
        session_id
    };
    let session = session_for(app, 41).await;
    let through = |workspace_id: WorkspaceId, session: ConnectSessionId, app: ChannelAppId, team: &'static str| {
        let connection = ChannelConnectionId::generate();
        async move {
            stores
                .channels
                .install_app_through_session(org, workspace_id, session, app, &install(connection, team, 1), now())
                .await
                .map(|(session, connection, _)| (session, connection))
        }
    };
    assert_eq!(
        through(t.sibling, session, app, "T0NORTH").await.err(),
        Some(ChannelError::SessionNotFound),
        "a session is found only in its own workspace"
    );
    assert_eq!(
        through(ws, session, other_app, "T0NORTH").await.err(),
        Some(ChannelError::SessionNotFound),
        "a session installs only the app it was made for"
    );
    let (completed, connection) = through(ws, session, app, "T0NORTH").await.unwrap();
    assert_eq!(completed.status, ConnectSessionStatus::Completed);
    assert_eq!(
        completed.target,
        Some(ConnectTarget::ChannelApp {
            provider: ChannelProviderKind::Slack,
            app,
            connection: Some(connection.id),
        })
    );
    assert_eq!(connection.status, ConnectionStatus::Connected);
    assert_eq!(
        through(ws, session, app, "T0NORTH").await.err(),
        Some(ChannelError::SessionNotPending),
        "a session installs once"
    );
    let credential = stores
        .channels
        .connection_credential(org, ws, connection.id)
        .await
        .unwrap()
        .expect("its token, sealed");
    assert_eq!(credential.version, 1);
    for (organization_id, workspace_id) in [(org, t.sibling), (OrganizationId::generate(), ws)] {
        assert_eq!(
            stores
                .channels
                .connection_credential(organization_id, workspace_id, connection.id)
                .await,
            Ok(None),
            "a token is read only in its own workspace"
        );
    }
    // The manager learned its team: another team's install is refused, and nothing is written.
    let other_session = session_for(other_app, 42).await;
    assert_eq!(
        through(ws, other_session, other_app, "T0ELSEWHERE").await.err(),
        Some(ChannelError::TeamMismatch)
    );
    let pending = stores
        .connect
        .find_session(org, ws, other_session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.status, ConnectSessionStatus::Pending, "still pending");
    let other = stores.channels.find_app(org, ws, other_app).await.unwrap().unwrap();
    assert_eq!(other.status, AppStatus::AwaitingInstall);
}

async fn removes_a_made_app_at_its_provider_too(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let release = zupflow_core::channels::provision::RELEASE_CHANNEL_APP;
    let (manager, app, connection) = installed(stores, &t, ws, t.agent).await;
    // One never made at the provider has nothing to remove there, and is released as it's deleted.
    let unmade = make_app(stores, &t, ws, t.other_agent, manager).await.unwrap();
    for removed in [app, unmade] {
        stores
            .channels
            .delete_app(org, ws, removed, developer(&t), now(), &|_| by_suite_in(ws))
            .await
            .unwrap();
    }
    let job = claim_in(stores, release, Scope::Workspace(org, ws))
        .await
        .expect("a job removes it at its provider");
    let payload: serde_json::Value = serde_json::from_slice(&job.payload).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({ "app_id": app.to_string(), "connection_id": connection.to_string() }),
        "naming the app and the connection whose token it revokes, never a secret"
    );
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    assert!(claim_in(stores, release, Scope::Workspace(org, ws)).await.is_none());
    // Its token stays readable, sealed, for the job to revoke.
    assert!(
        stores
            .channels
            .connection_credential(org, ws, connection)
            .await
            .unwrap()
            .is_some()
    );

    // Until it's released, its team stays linked, and pasted tokens queue
    // its removal again (a rotation's don't); never another workspace's.
    let audit_manager = |_: &zupflow_core::channels::ChannelAppManager| by_suite_in(ws);
    let unlink = || {
        stores
            .channels
            .delete_manager(org, ws, manager, developer(&t), &audit_manager)
    };
    assert_eq!(unlink().await, Err(ChannelError::RemovalsPending(1)));
    let replacement = |version: u32| TokenReplacement {
        expected_version: version,
        credential: sealed(5),
        token_last4: "0005".to_owned(),
        token_expires_at: None,
    };
    stores
        .channels
        .replace_manager_tokens(org, ws, manager, None, &replacement(1), now(), &|_| by_suite_in(ws))
        .await
        .unwrap();
    assert!(
        claim_in(stores, release, Scope::Workspace(org, ws)).await.is_none(),
        "not for a rotation"
    );
    stores
        .channels
        .replace_manager_tokens(org, ws, manager, Some(developer(&t)), &replacement(2), now(), &|_| {
            by_suite_in(ws)
        })
        .await
        .unwrap();
    let job = claim_in(stores, release, Scope::Workspace(org, ws))
        .await
        .expect("queued again by pasted tokens");
    let payload: serde_json::Value = serde_json::from_slice(&job.payload).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({ "app_id": app.to_string(), "connection_id": connection.to_string() })
    );
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    assert!(
        claim_in(stores, release, Scope::Workspace(org, ws)).await.is_none(),
        "only the unreleased"
    );
    for (organization_id, workspace_id) in [(org, t.sibling), (OrganizationId::generate(), ws)] {
        assert_eq!(
            stores
                .channels
                .record_app_released(organization_id, workspace_id, app, now())
                .await,
            Ok(false),
            "released only in its own workspace"
        );
    }
    assert_eq!(stores.channels.record_app_released(org, ws, app, now()).await, Ok(true));
    assert_eq!(unlink().await, Ok(()));

    // An agent's apps go with it, at the provider too.
    let (_, sibling_app, sibling_connection) = installed(stores, &t, t.sibling, t.sibling_agent).await;
    stores
        .agents
        .delete_agent(org, t.sibling, t.sibling_agent, &|_| by_suite_in(t.sibling))
        .await
        .unwrap();
    let job = claim_in(stores, release, Scope::Workspace(org, t.sibling))
        .await
        .expect("removed with its agent");
    let payload: serde_json::Value = serde_json::from_slice(&job.payload).unwrap();
    assert_eq!(payload["app_id"], sibling_app.to_string());
    assert_eq!(payload["connection_id"], sibling_connection.to_string());
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
}

/// A change to an agent's name or description queues a manifest sync for
/// each of its made apps in the same transaction; any other change, none.
/// A sync records the digest it sent, in the app's own workspace only.
async fn syncs_an_agents_apps_with_its_changes(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let sync = zupflow_core::channels::provision::SYNC_CHANNEL_APP;
    let (_, app, _) = installed(stores, &t, ws, t.agent).await;
    let change = |changes: AgentChanges| async move {
        stores
            .agents
            .update_agent(org, ws, t.agent, &changes, &|_| by_suite_in(ws))
            .await
            .unwrap();
    };
    change(AgentChanges {
        instructions: Some("Be briefer.".to_owned()),
        ..AgentChanges::default()
    })
    .await;
    assert!(
        claim_in(stores, sync, Scope::Workspace(org, ws)).await.is_none(),
        "nothing the app shows"
    );
    change(AgentChanges {
        name: Some("Renamed".to_owned()),
        ..AgentChanges::default()
    })
    .await;
    let job = claim_in(stores, sync, Scope::Workspace(org, ws))
        .await
        .expect("a sync, committed with the rename");
    let payload: serde_json::Value = serde_json::from_slice(&job.payload).unwrap();
    assert_eq!(payload, serde_json::json!({ "app_id": app.to_string() }));
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();

    let digest = "ab".repeat(32);
    assert_eq!(
        stores
            .channels
            .app_secrets(org, ws, app)
            .await
            .unwrap()
            .unwrap()
            .manifest_digest,
        None
    );
    assert_eq!(
        stores.channels.record_app_synced(org, t.sibling, app, &digest).await,
        Ok(false),
        "only in its own workspace"
    );
    assert_eq!(stores.channels.record_app_synced(org, ws, app, &digest).await, Ok(true));
    assert_eq!(
        stores
            .channels
            .app_secrets(org, ws, app)
            .await
            .unwrap()
            .unwrap()
            .manifest_digest,
        Some(digest)
    );
}

async fn rotates_a_teams_tokens_once_per_version(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let channels = stores.channels;
    let kind = "conformance.rotate";
    let rotate = |manager: ChannelAppManagerId, version: u32| QueuedJob {
        kind: Name::new(kind).unwrap(),
        payload: serde_json::json!({ "manager_id": manager.to_string(), "version": version })
            .to_string()
            .into_bytes(),
        options: JobOptions::default(),
    };
    let hour = 60 * 60 * 1000;
    let soon = Timestamp::from_millis(now().millis() + hour);
    let manager = link(stores, &t, ws).await;
    // Its expiry isn't known: rotated at once, by one job, however many sweeps run.
    let sweeps =
        futures_join_all((0..4).map(|_| channels.queue_due_rotations_cross_tenant(&rotate, soon, 500, now()))).await;
    let queued: u32 = sweeps.into_iter().map(|sweep| sweep.unwrap().queued).sum();
    assert!(queued >= 1);
    let job = claim_in(stores, kind, Scope::Workspace(org, ws))
        .await
        .expect("one rotation");
    let payload: serde_json::Value = serde_json::from_slice(&job.payload).unwrap();
    assert_eq!(
        payload,
        serde_json::json!({ "manager_id": manager.to_string(), "version": 1 })
    );
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    channels
        .queue_due_rotations_cross_tenant(&rotate, soon, 500, now())
        .await
        .unwrap();
    assert!(
        claim_in(stores, kind, Scope::Workspace(org, ws)).await.is_none(),
        "once per version"
    );

    // Rotated: the next version, due an hour before it expires.
    let rotated = |version: u32, expires_at: Timestamp| TokenReplacement {
        expected_version: version,
        credential: sealed(3),
        token_last4: "0003".to_owned(),
        token_expires_at: Some(expires_at),
    };
    let later = Timestamp::from_millis(now().millis() + 12 * hour);
    channels
        .replace_manager_tokens(org, ws, manager, None, &rotated(1, later), now(), &|_| by_suite_in(ws))
        .await
        .unwrap();
    channels
        .queue_due_rotations_cross_tenant(&rotate, soon, 500, now())
        .await
        .unwrap();
    assert!(
        claim_in(stores, kind, Scope::Workspace(org, ws)).await.is_none(),
        "not due yet"
    );
    // Due once its expiry is within the sweep's horizon.
    channels
        .queue_due_rotations_cross_tenant(&rotate, Timestamp::from_millis(later.millis() + 1), 500, now())
        .await
        .unwrap();
    let job = claim_in(stores, kind, Scope::Workspace(org, ws))
        .await
        .expect("due within the hour");
    let payload: serde_json::Value = serde_json::from_slice(&job.payload).unwrap();
    assert_eq!(payload["version"], 2);
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();

    // Not rotated in time: expired, announced once.
    let past_expiry = Timestamp::from_millis(later.millis() + 1);
    let sweep = channels
        .queue_due_rotations_cross_tenant(
            &rotate,
            Timestamp::from_millis(past_expiry.millis() + hour),
            500,
            past_expiry,
        )
        .await
        .unwrap();
    assert!(sweep.expired >= 1, "{sweep:?}");
    let found = channels.find_manager(org, ws, manager).await.unwrap().unwrap();
    assert_eq!(found.status, zupflow_core::channels::ManagerStatus::TokenExpired);
    assert!(
        claim_in(stores, kind, Scope::Workspace(org, ws)).await.is_none(),
        "an expired team isn't rotated"
    );
    assert_eq!(
        channels
            .queue_due_rotations_cross_tenant(&rotate, soon, 500, now())
            .await
            .map(|sweep| sweep.expired >= 1),
        Ok(false),
        "expired once"
    );

    // Expiring by hand: only the version read, only in its own workspace.
    let other = link(stores, &t, ws).await;
    for (organization_id, workspace_id, version) in
        [(org, t.sibling, 1), (OrganizationId::generate(), ws, 1), (org, ws, 2)]
    {
        assert_eq!(
            channels
                .expire_manager_tokens(organization_id, workspace_id, other, version, now())
                .await,
            Ok(false)
        );
    }
    assert_eq!(channels.expire_manager_tokens(org, ws, other, 1, now()).await, Ok(true));
    assert_eq!(
        channels.expire_manager_tokens(org, ws, other, 1, now()).await,
        Ok(false)
    );

    // Pasted tokens whose rotation job is buried still expire, within their
    // lifetime of the rotation being queued, rather than staying active forever.
    let pasted = link(stores, &t, ws).await;
    channels
        .queue_due_rotations_cross_tenant(&rotate, soon, 500, now())
        .await
        .unwrap();
    let job = claim_in(stores, kind, Scope::Workspace(org, ws))
        .await
        .expect("its rotation");
    stores
        .jobs
        .bury(&job.scope, job.id, &job.lease, &Name::new("conformance").unwrap())
        .await
        .unwrap();
    let found = channels.find_manager(org, ws, pasted).await.unwrap().unwrap();
    let bound = found.token_expires_at.expect("an expiry once its rotation is queued");
    assert!(bound.millis() <= now().millis() + 12 * hour, "{bound:?}");
    let lapsed = Timestamp::from_millis(bound.millis() + 1);
    channels
        .queue_due_rotations_cross_tenant(&rotate, Timestamp::from_millis(lapsed.millis() + hour), 500, lapsed)
        .await
        .unwrap();
    let found = channels.find_manager(org, ws, pasted).await.unwrap().unwrap();
    assert_eq!(found.status, zupflow_core::channels::ManagerStatus::TokenExpired);
}

/// The people a connection reads from its provider are its own: a name is
/// kept once known (so what a run was sent never changes under it), only a
/// name that fits is kept, someone the provider doesn't know isn't asked
/// about again for a day, an email is what the provider said last and is
/// read again after a day, and another workspace or organization reaches
/// none of them. A run reads a mention by the name read.
async fn reads_people_once_and_replies_only_to_answers(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, connection) = installed(stores, &t, ws, t.agent).await;
    recorded(stores, &t, connection, "2.0", "hello").await;
    let ids = |list: &[&str]| -> Vec<String> { list.iter().map(|id| (*id).to_owned()).collect() };
    let asked = ids(&["U0DANA", "U0NEW", "U0NEW", "U0LONG", "U0LATER", "U0GONE"]);
    let start = now();
    let read = |names: Vec<String>, emails: Vec<String>, at: Timestamp| async move {
        stores
            .channels
            .people_to_read(org, ws, connection, &names, &emails, at)
            .await
    };
    assert_eq!(
        read(asked.clone(), Vec::new(), start).await,
        Ok(ids(&["U0NEW", "U0LONG", "U0LATER", "U0GONE"])),
        "named people aren't read for their names; each other one once, in order"
    );
    assert_eq!(
        read(Vec::new(), ids(&["U0DANA"]), start).await,
        Ok(ids(&["U0DANA"])),
        "an email never read from the provider is read"
    );
    let found = |id: &str, name: String, email: &str| PersonLookup {
        external_id: id.to_owned(),
        found: Some(ProviderPerson {
            handle: Some(id.to_lowercase()),
            display_name: Some(name),
            email: Some(email.to_owned()),
            is_automated: false,
        }),
    };
    let gone = |id: &str| PersonLookup {
        external_id: id.to_owned(),
        found: None,
    };
    stores
        .channels
        .record_people(
            org,
            ws,
            connection,
            &[
                found("U0NEW", "Nadia".to_owned(), "nadia@northwind.example"),
                found("U0DANA", "Renamed".to_owned(), "dana@moved.example"),
                found("U0LONG", "x".repeat(301), "long@northwind.example"),
                gone("U0GONE"),
            ],
            start,
        )
        .await
        .expect("saved");
    assert_eq!(
        read(asked.clone(), ids(&["U0DANA"]), start).await,
        Ok(ids(&["U0LATER"])),
        "someone unknown, or whose name is too long to keep, isn't asked about again yet"
    );
    let dana = recorded(stores, &t, connection, "2.1", "again").await;
    assert_eq!(
        dana.author.and_then(|author| author.display_name).as_deref(),
        Some("Dana"),
        "a known name is kept"
    );
    let email = |message: ChannelMessageId| async move {
        stores
            .channels
            .inbound_context(org, ws, message)
            .await
            .unwrap()
            .expect("the message")
            .author_email
    };
    assert_eq!(
        email(dana.id).await.as_deref(),
        Some("dana@moved.example"),
        "the email is what the provider said last"
    );
    // A day on, someone unknown is asked about again, and so is an email a
    // sender list checks; a known name never is.
    let later = Timestamp::from_millis(start.millis() + 24 * 60 * 60 * 1000);
    assert_eq!(
        read(
            ids(&["U0DANA", "U0NEW", "U0LONG", "U0GONE"]),
            ids(&["U0DANA", "U0NEW"]),
            later
        )
        .await,
        Ok(ids(&["U0LONG", "U0GONE", "U0DANA", "U0NEW"]))
    );
    // Someone the provider no longer shows keeps their name, but no email
    // a sender list could still match.
    stores
        .channels
        .record_people(org, ws, connection, &[gone("U0DANA")], later)
        .await
        .expect("saved");
    assert_eq!(email(dana.id).await, None);
    let dana = recorded(stores, &t, connection, "2.3", "still here").await;
    assert_eq!(
        dana.author.and_then(|author| author.display_name).as_deref(),
        Some("Dana")
    );
    // Another workspace or organization reaches none of them.
    for (other_org, other_ws) in [(org, t.sibling), (OrganizationId::generate(), ws)] {
        assert_eq!(
            stores
                .channels
                .people_to_read(other_org, other_ws, connection, &asked, &[], start)
                .await,
            Ok(ids(&["U0DANA", "U0NEW", "U0LONG", "U0LATER", "U0GONE"]))
        );
        assert_eq!(
            stores
                .channels
                .record_people(
                    other_org,
                    other_ws,
                    connection,
                    &[found("U0LATER", "Lee".to_owned(), "lee@northwind.example")],
                    start
                )
                .await,
            Err(ChannelError::ConnectionNotFound)
        );
    }
    assert_eq!(read(asked.clone(), Vec::new(), start).await, Ok(ids(&["U0LATER"])));

    // A mention of someone read is rendered by their name.
    let channel = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap()[0].clone();
    let mut asking = message("2.2", "C0SUPPORT", "<@U0CONFORM> ask <@U0NEW>", 1_000);
    asking.mentions = vec!["U0CONFORM".to_owned(), "U0NEW".to_owned()];
    let asking = stores
        .channels
        .record_message(org, ws, connection, &asking, now())
        .await
        .unwrap()
        .message;
    let outcome = stores
        .channels
        .dispatch(
            org,
            ws,
            &dispatch_of(asking.id, channel.id, "2.2"),
            Some(&template()),
            now(),
        )
        .await
        .unwrap();
    let DispatchOutcome::Started { run_id } = outcome else {
        panic!("a run starts: {outcome:?}");
    };
    let run = stores.runs.find_run(org, ws, run_id).await.unwrap().unwrap();
    assert!(run.input.contains("ask @Nadia"), "{}", run.input);
    // Running, not answered: nothing to post. A run that isn't a channel
    // run, or another scope's, has none either.
    assert_eq!(stores.channels.reply_of(org, ws, run_id).await, Ok(None));
    assert_eq!(stores.channels.reply_of(org, ws, t.runs[0]).await, Ok(None));
    assert_eq!(stores.channels.reply_of(org, t.sibling, run_id).await, Ok(None));
    assert_eq!(
        stores.channels.reply_of(OrganizationId::generate(), ws, run_id).await,
        Ok(None)
    );
}

/// Drives a run to its final answer, as a worker does: claimed, its model
/// asked, and `text` the answer.
async fn answer_run(stores: &Stores<'_>, org: OrganizationId, ws: WorkspaceId, run: RunId, text: &str) {
    let mut facts = Facts {
        claimant: Some(ClaimantId::from_uuid(uuid::Uuid::now_v7())),
        offer: Some(Offer::default()),
        model_route: Some(RouteState::Usable { key_version: 1 }),
        invocation_id: Some(InvocationId::from_uuid(uuid::Uuid::now_v7())),
        ..Facts::at(now())
    };
    for _ in 0..8 {
        let snapshot = stores.runs.find_snapshot(org, ws, run).await.unwrap().expect("the run");
        facts.model = match snapshot.control.continuation {
            Continuation::Done => return,
            Continuation::AwaitModel { turn, attempt } => Some(ModelAnswer {
                turn,
                attempt,
                result: ModelResult::Answered(Completion {
                    reply: Reply::Text(text.to_owned()),
                    usage: TokenUsage::default(),
                }),
            }),
            _ => None,
        };
        let plan = advance(&snapshot, &facts).expect("a plan");
        let request = plan.request().expect("a transition").clone();
        if let Operation::Claim(claim) = request.operation() {
            facts.claim = Some(claim.epoch);
        }
        let applied = stores.runs.apply(org, ws, &request).await;
        assert!(matches!(applied, Ok(Applied::Committed(_))), "{applied:?}");
    }
    panic!("the run didn't end");
}

/// A channel run that ended with an answer queues its reply in the
/// transaction that ends it; the reply reads where it goes, with one more
/// hop than the most that led to what it took in, until the connection
/// records posting it. Another scope reads none of it.
async fn replies_to_an_answer_until_it_is_sent(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, connection) = installed(stores, &t, ws, t.agent).await;
    let channel = stores.channels.list_agent_channels(org, ws, t.agent).await.unwrap()[0].clone();
    let asked = recorded(stores, &t, connection, "3.0", "<@U0CONFORM> what's due?").await;
    let mut dispatch = dispatch_of(asked.id, channel.id, "3.0");
    dispatch.agent_hop = 2;
    let outcome = stores
        .channels
        .dispatch(org, ws, &dispatch, Some(&template()), now())
        .await
        .unwrap();
    let DispatchOutcome::Started { run_id } = outcome else {
        panic!("a run starts: {outcome:?}");
    };
    answer_run(stores, org, ws, run_id, "Two invoices.").await;
    let run = stores.runs.find_run(org, ws, run_id).await.unwrap().unwrap();
    assert_eq!(run.output.as_deref(), Some("Two invoices."));
    let job = claim_in(stores, REPLY_JOB, Scope::Workspace(org, ws))
        .await
        .expect("the reply is queued");
    assert_eq!(zupflow_core::channels::reply::run_of_job(&job.payload), Some(run_id));
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();

    let reply = stores
        .channels
        .reply_of(org, ws, run_id)
        .await
        .unwrap()
        .expect("a reply");
    assert_eq!(
        reply,
        ChannelReply {
            provider: ChannelProviderKind::Slack,
            connection_id: connection,
            connection_status: ConnectionStatus::Connected,
            conversation_external_id: "C0SUPPORT".to_owned(),
            conversation_kind: ConversationKind::Channel,
            conversation_is_external: false,
            thread_key: "1700000000.000100".to_owned(),
            text: "Two invoices.".to_owned(),
            agent_hop: 3,
            finished_at: run.finished_at.expect("ended"),
            sent: None,
        }
    );
    assert_eq!(stores.channels.reply_of(org, t.sibling, run_id).await, Ok(None));
    assert_eq!(
        stores.channels.reply_of(OrganizationId::generate(), ws, run_id).await,
        Ok(None)
    );
    // Once the connection records posting it, it's sent.
    let mut posted = message("3.1", "C0SUPPORT", "Two invoices.", 2_000);
    posted.direction = Direction::Outbound;
    posted.author = None;
    posted.mentions = Vec::new();
    posted.run_id = Some(run_id);
    posted.agent_hop = reply.agent_hop;
    let posted = stores
        .channels
        .record_message(org, ws, connection, &posted, now())
        .await
        .unwrap()
        .message;
    let reply = stores
        .channels
        .reply_of(org, ws, run_id)
        .await
        .unwrap()
        .expect("a reply");
    assert_eq!(reply.sent, Some(posted.id));
}

/// What a run's channel tools reach is the run's own connection's alone:
/// another connection's message or person, even in the same workspace, is
/// as one that doesn't exist. Edits are kept beside what a message first
/// said, the latest one winning whatever order they arrive in, and the
/// retention sweep deletes only its own workspace's old messages.
async fn acts_only_within_its_connection_and_sweeps_past_retention(stores: &Stores<'_>, prepare: &Prepare) {
    const DAY: i64 = 24 * 60 * 60 * 1000;
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, app, own) = installed(stores, &t, ws, t.agent).await;
    let (_, _, other) = installed(stores, &t, ws, t.other_agent).await;
    let (_, _, sibling) = installed(stores, &t, t.sibling, t.sibling_agent).await;
    let at = now().millis();
    let record = |workspace_id: WorkspaceId, connection_id: ChannelConnectionId, message: NewMessage| async move {
        stores
            .channels
            .record_message(org, workspace_id, connection_id, &message, now())
            .await
            .expect("recorded")
            .message
    };
    let old = record(ws, own, message("1.1", "C0SUPPORT", "Lunch at noon", at - 40 * DAY)).await;
    let recent = record(ws, own, message("1.2", "C0SUPPORT", "lunch moved", at - DAY)).await;
    let others = record(ws, other, message("2.1", "C0SUPPORT", "lunch for two", at - DAY)).await;
    let siblings = record(
        t.sibling,
        sibling,
        message("3.1", "C0SUPPORT", "lunch nearby", at - 40 * DAY),
    )
    .await;

    // Found within the connection only.
    let find = |workspace_id: WorkspaceId, connection_id: ChannelConnectionId, message_id: ChannelMessageId| async move {
        stores
            .channels
            .find_message(org, workspace_id, connection_id, message_id)
            .await
            .unwrap()
            .map(|message| message.id)
    };
    assert_eq!(find(ws, own, recent.id).await, Some(recent.id));
    assert_eq!(
        find(ws, own, others.id).await,
        None,
        "another connection's, same workspace"
    );
    assert_eq!(find(ws, own, siblings.id).await, None, "another workspace's");
    assert_eq!(
        find(t.sibling, own, siblings.id).await,
        None,
        "named with another workspace"
    );
    assert_eq!(
        stores
            .channels
            .find_message(OrganizationId::generate(), ws, own, recent.id)
            .await
            .unwrap(),
        None,
        "named with another organization"
    );
    let found: Vec<ChannelMessageId> = stores
        .channels
        .search_messages(org, ws, own, None, ChannelConversationId::generate(), "LUNCH", 10)
        .await
        .unwrap()
        .iter()
        .map(|message| message.id)
        .collect();
    assert_eq!(found, [recent.id, old.id], "its own, newest first");
    let people = stores.channels.find_people(org, ws, own, "dan", 10).await.unwrap();
    assert_eq!(people.len(), 1);
    assert_eq!(
        people[0].team_id.as_deref(),
        Some("T0NORTH"),
        "the team its message named"
    );
    let others_person = others.author.as_ref().expect("an author").id;
    assert_ne!(people[0].id, others_person, "each connection knows its own people");
    assert!(
        stores
            .channels
            .find_person(org, ws, own, others_person)
            .await
            .unwrap()
            .is_none()
    );

    // Only its own messages are its to edit, and its posts are counted.
    assert!(matches!(
        stores
            .channels
            .record_own_edit(org, ws, own, recent.id, "no", now())
            .await,
        Err(ChannelError::MessageNotFound)
    ));
    let posted = record(
        ws,
        own,
        NewMessage {
            direction: Direction::Outbound,
            author: None,
            mentions: Vec::new(),
            ..message("1.3", "C0SUPPORT", "On it.", at)
        },
    )
    .await;
    stores
        .channels
        .record_own_edit(org, ws, own, posted.id, "On it, now.", now())
        .await
        .unwrap();
    assert!(matches!(
        stores
            .channels
            .record_own_edit(org, ws, other, posted.id, "theirs", now())
            .await,
        Err(ChannelError::MessageNotFound)
    ));
    let edited = stores
        .channels
        .find_message(org, ws, own, posted.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (edited.text.as_str(), edited.edited_text.as_deref()),
        ("On it.", Some("On it, now."))
    );

    // A person's edits: the latest wins, whatever order they arrive in.
    let edit = |id: &str, external_id: &str, text: &str, edited_at: i64| NewDelivery {
        provider_event_id: id.to_owned(),
        kind: "message".to_owned(),
        team_id: Some("T0NORTH".to_owned()),
        event: InboundEvent::Edited(Box::new(EditedMessage {
            conversation_external_id: "C0SUPPORT".to_owned(),
            external_id: external_id.to_owned(),
            text: text.to_owned(),
            edited_at: Timestamp::from_millis(edited_at),
        })),
    };
    for (id, text, edited_at) in [
        ("EvE2", "lunch at one", at - 10),
        ("EvE1", "lunch at half past", at - 20),
    ] {
        let outcome = stores
            .channels
            .record_delivery(org, ws, app, &edit(id, "1.2", text, edited_at), now())
            .await;
        assert_eq!(
            outcome,
            Ok(DeliveryOutcome::Changed {
                connection_id: own,
                message_id: recent.id
            })
        );
    }
    let kept = stores
        .channels
        .find_message(org, ws, own, recent.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (kept.text.as_str(), kept.edited_text.as_deref()),
        ("lunch moved", Some("lunch at one"))
    );
    // Another connection's message, by its provider ID: not this one's.
    assert_eq!(
        stores
            .channels
            .record_delivery(org, ws, app, &edit("EvE3", "2.1", "mine", at), now())
            .await,
        Ok(DeliveryOutcome::Ignored)
    );
    let deleted = NewDelivery {
        provider_event_id: "EvD1".to_owned(),
        kind: "message".to_owned(),
        team_id: Some("T0NORTH".to_owned()),
        event: InboundEvent::Deleted(Box::new(DeletedMessage {
            conversation_external_id: "C0SUPPORT".to_owned(),
            external_id: "1.2".to_owned(),
            deleted_at: Timestamp::from_millis(at),
        })),
    };
    stores
        .channels
        .record_delivery(org, ws, app, &deleted, now())
        .await
        .unwrap();
    let kept = stores
        .channels
        .find_message(org, ws, own, recent.id)
        .await
        .unwrap()
        .unwrap();
    assert!(kept.deleted_at.is_some());
    assert!(
        stores
            .channels
            .search_messages(
                org,
                ws,
                own,
                None,
                ChannelConversationId::generate(),
                "lunch at one",
                10
            )
            .await
            .unwrap()
            .is_empty(),
        "a deleted message isn't found"
    );

    // Past the organization's 30 days: only this workspace's old message.
    let due = stores.channels.retention_due_cross_tenant(now(), 1_000).await.unwrap();
    assert!(due.contains(&(org, ws)) && due.contains(&(org, t.sibling)));
    let swept = stores.channels.sweep_retention(org, ws, now()).await.unwrap();
    assert_eq!(swept.messages, 1);
    assert_eq!(find(ws, own, old.id).await, None);
    assert_eq!(find(ws, own, recent.id).await, Some(recent.id));
    assert_eq!(
        find(t.sibling, sibling, siblings.id).await,
        Some(siblings.id),
        "another workspace's waits"
    );
    let due = stores.channels.retention_due_cross_tenant(now(), 1_000).await.unwrap();
    assert!(!due.contains(&(org, ws)) && due.contains(&(org, t.sibling)));
}

/// A connection's changes at its provider take send slots a second apart,
/// however many runs take them at once, and at most 200 an hour; another
/// connection's slots are its own.
async fn reserves_send_slots_a_second_apart_per_connection(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, own) = installed(stores, &t, ws, t.agent).await;
    let (_, _, other) = installed(stores, &t, ws, t.other_agent).await;
    let at = now().millis();
    let take = |connection_id: ChannelConnectionId, millis: i64| async move {
        stores
            .channels
            .take_send_slot(
                org,
                ws,
                connection_id,
                RunId::generate(),
                ToolCallId::generate(),
                Timestamp::from_millis(millis),
            )
            .await
            .expect("a slot")
    };
    // Taken at once, all together: a second apart, never two in a second,
    // and refused once the next free one is further off than a send waits.
    let mut taken = futures_join_all((0..9).map(|_| take(own, at))).await;
    taken.sort_by_key(|slot| match slot {
        SendSlot::Taken { wait_millis } => *wait_millis,
        _ => u64::MAX,
    });
    assert_eq!(
        taken,
        [
            SendSlot::Taken { wait_millis: 0 },
            SendSlot::Taken { wait_millis: 1000 },
            SendSlot::Taken { wait_millis: 2000 },
            SendSlot::Taken { wait_millis: 3000 },
            SendSlot::Taken { wait_millis: 4000 },
            SendSlot::Taken { wait_millis: 5000 },
            SendSlot::Busy,
            SendSlot::Busy,
            SendSlot::Busy,
        ]
    );
    assert_eq!(
        take(other, at).await,
        SendSlot::Taken { wait_millis: 0 },
        "another connection's"
    );
    assert_eq!(
        stores
            .channels
            .send_counts(org, ws, own, Timestamp::from_millis(at))
            .await
            .unwrap()
            .hour,
        6
    );
    // Later, the next slot is free at once.
    assert_eq!(take(own, at + 10_000).await, SendSlot::Taken { wait_millis: 0 });

    // Two hours on, the old ones are gone: 200 in an hour, then no more.
    let later = at + 2 * 60 * 60 * 1000;
    for second in 0..200 {
        assert_eq!(
            take(own, later + second * 1000).await,
            SendSlot::Taken { wait_millis: 0 }
        );
    }
    assert_eq!(
        stores
            .channels
            .send_counts(org, ws, own, Timestamp::from_millis(later + 200_000))
            .await
            .unwrap()
            .hour,
        200
    );
    assert_eq!(take(own, later + 200_000).await, SendSlot::HourFull);
    assert_eq!(take(other, later + 200_000).await, SendSlot::Taken { wait_millis: 0 });
    // An hour after the first of them, room again.
    assert_eq!(
        take(own, later + 60 * 60 * 1000 + 500).await,
        SendSlot::Taken { wait_millis: 0 }
    );
}

/// A search reads a direct message only in the conversation it names as
/// readable; every other conversation's messages as before.
async fn searches_direct_messages_only_where_readable(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, ws) = (t.organization_id, t.sandbox);
    let (_, _, own) = installed(stores, &t, ws, t.agent).await;
    let at = now().millis();
    let record = |message: NewMessage| async move {
        stores
            .channels
            .record_message(org, ws, own, &message, now())
            .await
            .expect("recorded")
            .message
    };
    let direct = |external_id: &str, conversation: &str| NewMessage {
        conversation: NewConversation {
            external_id: conversation.to_owned(),
            kind: ConversationKind::Dm,
            name: None,
            is_external: false,
            identity_is_member: true,
        },
        ..message(external_id, conversation, "the launch date", at)
    };
    let mine = record(direct("8.1", "D0MINE")).await;
    let theirs = record(direct("8.2", "D0THEIRS")).await;
    let open = record(message("8.3", "C0SUPPORT", "the launch date &amp; time", at)).await;
    let search = |readable: ChannelConversationId| async move {
        let mut found: Vec<ChannelMessageId> = stores
            .channels
            .search_messages(org, ws, own, None, readable, "launch", 10)
            .await
            .unwrap()
            .iter()
            .map(|message| message.id)
            .collect();
        found.sort();
        found
    };
    let mut expected = vec![mine.id, open.id];
    expected.sort();
    assert_eq!(search(mine.conversation_id).await, expected);
    assert_eq!(search(ChannelConversationId::generate()).await, [open.id]);
    assert!(!search(mine.conversation_id).await.contains(&theirs.id));
    // An escape a rendering undoes is a candidate for the caller to match.
    let found = stores
        .channels
        .search_messages(org, ws, own, None, ChannelConversationId::generate(), "date & time", 10)
        .await
        .unwrap();
    assert_eq!(found.iter().map(|message| message.id).collect::<Vec<_>>(), [open.id]);
}
