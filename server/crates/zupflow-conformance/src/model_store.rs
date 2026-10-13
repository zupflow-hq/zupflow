//! The `ModelStore` suite: connections, their sealed keys, models and
//! defaults reached only through their own organization and environment;
//! a key replaced only from the version read, the earlier ones forgotten; a
//! model added once per connection and provider model, from a connection of
//! its environment; a default only of a model of its environment; an agent's
//! model only of its workspace's environment (through the `AgentStore`); a
//! model, or a connection's models, kept while the default or an agent holds
//! them, and a connection's models and keys gone with it; every change made
//! only by a manager who still may, with one audit event, and none for a
//! refused change; and a run's model resolved from the agent's override or
//! the environment's default, never the other environment's.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, two Sandbox
//! workspaces and a Production one (each named), an active admin and an
//! active viewer with their users, and an API key per environment.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::agents::{AgentChanges, AgentError, AgentStore, NewAgent};
use zupflow_core::audit::{
    AuditAction, AuditActor, AuditEntry, AuditFilter, AuditObject, AuditPlace, AuditStore, AuditTarget,
};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AgentId, ApiKeyId, MemberId, ModelConnectionId, ModelId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::keys::{Sealed, WrappedKey};
use zupflow_core::model_providers::ProviderKind;
use zupflow_core::model_store::{
    ConnectionStatus, KeyReplacement, Model, ModelConnection, ModelFilter, ModelManager, ModelStore, ModelStoreError,
    NewConnection, NewModel, RouteState, SealedKey,
};
use zupflow_core::time::Timestamp;

use crate::audit_store::by_suite_in;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    /// Another Sandbox workspace.
    pub sibling: WorkspaceId,
    pub production: WorkspaceId,
    /// An active admin, and the user they are.
    pub admin: MemberId,
    pub admin_user: UserId,
    /// An active viewer, and the user they are.
    pub viewer: MemberId,
    pub viewer_user: UserId,
    pub sandbox_key: ApiKeyId,
    pub production_key: ApiKeyId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The stores the suite drives: the one under test, agents that hold its
/// models, and the log its changes write.
pub struct Stores<'a> {
    pub models: &'a dyn ModelStore,
    pub agents: &'a dyn AgentStore,
    pub audit: &'a dyn AuditStore,
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    connects_and_replaces_keys(&stores, &prepare).await;
    adds_lists_and_renames_models(&stores, &prepare).await;
    keeps_defaults_and_agents_to_their_environment(&stores, &prepare).await;
    keeps_what_is_in_use(&stores, &prepare).await;
    resolves_a_runs_model(&stores, &prepare).await;
    lets_only_managers_change(&stores, &prepare).await;
    keeps_organizations_apart(&stores, &prepare).await;
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        sandbox: WorkspaceId::generate(),
        sibling: WorkspaceId::generate(),
        production: WorkspaceId::generate(),
        admin: MemberId::generate(),
        admin_user: UserId::generate(),
        viewer: MemberId::generate(),
        viewer_user: UserId::generate(),
        sandbox_key: ApiKeyId::generate(),
        production_key: ApiKeyId::generate(),
    };
    prepare(tenant).await;
    tenant
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

fn key(marker: u8, last4: &str) -> SealedKey {
    SealedKey {
        sealed: sealed(marker),
        last4: last4.to_owned(),
    }
}

fn place(environment: Environment) -> AuditPlace {
    AuditPlace::Environment(environment)
}

fn actor() -> AuditActor {
    AuditActor::System {
        name: "Conformance".to_owned(),
    }
}

fn connection_entry(action: AuditAction) -> impl Fn(&ModelConnection) -> AuditEntry + Send + Sync {
    move |connection| {
        AuditEntry::new(
            actor(),
            action,
            AuditTarget::new(AuditObject::ModelConnection(connection.id), &connection.label),
            place(connection.environment),
        )
    }
}

fn model_entry(action: AuditAction) -> impl Fn(&Model) -> AuditEntry + Send + Sync {
    move |model| {
        AuditEntry::new(
            actor(),
            action,
            AuditTarget::new(AuditObject::Model(model.id), &model.name),
            place(model.environment),
        )
    }
}

fn admin(t: &Tenant) -> ModelManager {
    ModelManager::Member(t.admin)
}

async fn connect(
    stores: &Stores<'_>,
    t: &Tenant,
    environment: Environment,
    kind: ProviderKind,
    label: &str,
    marker: u8,
) -> ModelConnection {
    stores
        .models
        .create_connection(
            t.organization_id,
            ModelConnectionId::generate(),
            admin(t),
            &NewConnection {
                environment,
                kind,
                label: label.to_owned(),
                key: key(marker, "ab12"),
            },
            Timestamp::now(),
            &connection_entry(AuditAction::ModelConnectionCreated),
        )
        .await
        .unwrap()
}

fn new_model(connection: &ModelConnection, provider_model_id: &str, name: &str) -> NewModel {
    NewModel {
        connection_id: connection.id,
        provider_model_id: provider_model_id.to_owned(),
        name: name.to_owned(),
        author: "openai".to_owned(),
        context_window: Some(400_000),
        supports_tools: true,
    }
}

async fn add(
    stores: &Stores<'_>,
    t: &Tenant,
    connection: &ModelConnection,
    provider_model_id: &str,
    name: &str,
) -> Model {
    stores
        .models
        .create_model(
            t.organization_id,
            connection.environment,
            ModelId::generate(),
            admin(t),
            &new_model(connection, provider_model_id, name),
            Timestamp::now(),
            &model_entry(AuditAction::ModelCreated),
        )
        .await
        .unwrap()
}

async fn set_default(
    stores: &Stores<'_>,
    t: &Tenant,
    environment: Environment,
    model_id: Option<ModelId>,
) -> Result<zupflow_core::model_store::DefaultModel, ModelStoreError> {
    stores
        .models
        .set_default_model(
            t.organization_id,
            environment,
            admin(t),
            model_id,
            Some(t.admin_user),
            Timestamp::now(),
            &|default| {
                AuditEntry::new(
                    actor(),
                    AuditAction::DefaultModelUpdated,
                    AuditTarget::new(
                        AuditObject::DefaultModel(default.model.as_ref().map(|model| model.id)),
                        default.model.as_ref().map_or("None", |model| model.name.as_str()),
                    ),
                    place(default.environment),
                )
            },
        )
        .await
}

async fn agent(
    stores: &Stores<'_>,
    t: &Tenant,
    workspace: WorkspaceId,
    name: &str,
    model_id: Option<ModelId>,
) -> Result<zupflow_core::agents::Agent, AgentError> {
    stores
        .agents
        .create_agent(
            t.organization_id,
            workspace,
            AgentId::generate(),
            &NewAgent {
                name: name.to_owned(),
                description: String::new(),
                model_id,
                instructions: String::new(),
            },
            &|_| by_suite_in(workspace),
        )
        .await
}

/// The organization's events with these actions, newest first.
async fn events(stores: &Stores<'_>, t: &Tenant, actions: &[AuditAction]) -> Vec<(AuditAction, Option<Environment>)> {
    stores
        .audit
        .list_audit_events(t.organization_id, &AuditFilter::default(), None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| actions.contains(&event.action))
        .map(|event| (event.action, event.environment))
        .collect()
}

async fn connects_and_replaces_keys(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let models = stores.models;
    let first = connect(stores, &t, Environment::Sandbox, ProviderKind::OpenRouter, "Router", 1).await;
    assert_eq!(
        (
            first.organization_id,
            first.environment,
            first.kind,
            first.label.as_str(),
            first.key_version,
            first.status,
            first.model_count
        ),
        (
            org,
            Environment::Sandbox,
            ProviderKind::OpenRouter,
            "Router",
            1,
            ConnectionStatus::Active,
            0
        )
    );
    assert!(first.api_key.set);
    assert_eq!(first.api_key.last4.as_deref(), Some("ab12"));
    let second = connect(
        stores,
        &t,
        Environment::Production,
        ProviderKind::Anthropic,
        "Claude",
        2,
    )
    .await;
    let third = connect(stores, &t, Environment::Sandbox, ProviderKind::Anthropic, "Claude", 3).await;
    let ids = |list: Vec<ModelConnection>| list.into_iter().map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(
        ids(models.list_connections(org, None).await.unwrap()),
        [first.id, second.id, third.id],
        "oldest first"
    );
    assert_eq!(
        ids(models.list_connections(org, Some(Environment::Sandbox)).await.unwrap()),
        [first.id, third.id]
    );
    assert_eq!(
        models.find_connection(org, None, first.id).await.unwrap(),
        Some(first.clone())
    );

    // The key comes back sealed exactly, only in its own environment.
    let stored = models
        .connection_key(org, Environment::Sandbox, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.version, stored.sealed), (1, sealed(1)));
    assert_eq!(
        models
            .connection_key(org, Environment::Production, first.id)
            .await
            .unwrap(),
        None,
        "another environment's connection"
    );

    let renamed = models
        .rename_connection(
            org,
            Environment::Sandbox,
            first.id,
            admin(&t),
            "Router 2",
            Timestamp::now(),
            &connection_entry(AuditAction::ModelConnectionUpdated),
        )
        .await
        .unwrap();
    assert_eq!((renamed.label.as_str(), renamed.key_version), ("Router 2", 1));
    assert_eq!(
        models
            .rename_connection(
                org,
                Environment::Production,
                first.id,
                admin(&t),
                "x",
                Timestamp::now(),
                &connection_entry(AuditAction::ModelConnectionUpdated),
            )
            .await,
        Err(ModelStoreError::ConnectionNotFound),
        "renamed only in its own environment"
    );

    // A refused key marks the connection, unless the key changed since.
    models
        .mark_key_refused(org, Environment::Sandbox, first.id, 1)
        .await
        .unwrap();
    let refused = models.find_connection(org, None, first.id).await.unwrap().unwrap();
    assert_eq!(refused.status, ConnectionStatus::InvalidKey);

    let replace = |expected_version: u32, marker: u8| KeyReplacement {
        expected_version,
        key: key(marker, "K2pa"),
    };
    let replaced = models
        .replace_key(
            org,
            Environment::Sandbox,
            first.id,
            admin(&t),
            &replace(1, 9),
            Timestamp::now(),
            &connection_entry(AuditAction::ModelConnectionKeyReplaced),
        )
        .await
        .unwrap();
    assert_eq!(
        (replaced.key_version, replaced.api_key.last4.as_deref(), replaced.status),
        (2, Some("K2pa"), ConnectionStatus::Active),
        "a new key makes the connection active again"
    );
    let stored = models
        .connection_key(org, Environment::Sandbox, first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((stored.version, stored.sealed), (2, sealed(9)), "only the current key");
    assert_eq!(
        models
            .replace_key(
                org,
                Environment::Sandbox,
                first.id,
                admin(&t),
                &replace(1, 10),
                Timestamp::now(),
                &connection_entry(AuditAction::ModelConnectionKeyReplaced),
            )
            .await,
        Err(ModelStoreError::KeyChanged),
        "replaced from a version that's gone"
    );
    assert_eq!(
        models.find_connection(org, None, first.id).await.unwrap(),
        Some(replaced.clone())
    );
    models
        .mark_key_refused(org, Environment::Sandbox, first.id, 1)
        .await
        .unwrap();
    assert_eq!(
        models
            .find_connection(org, None, first.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        ConnectionStatus::Active,
        "a refusal of a replaced key changes nothing"
    );

    assert_eq!(
        events(
            stores,
            &t,
            &[
                AuditAction::ModelConnectionCreated,
                AuditAction::ModelConnectionUpdated,
                AuditAction::ModelConnectionKeyReplaced
            ]
        )
        .await,
        [
            (AuditAction::ModelConnectionKeyReplaced, Some(Environment::Sandbox)),
            (AuditAction::ModelConnectionUpdated, Some(Environment::Sandbox)),
            (AuditAction::ModelConnectionCreated, Some(Environment::Sandbox)),
            (AuditAction::ModelConnectionCreated, Some(Environment::Production)),
            (AuditAction::ModelConnectionCreated, Some(Environment::Sandbox)),
        ],
        "one event per change, none for a refused one"
    );
}

async fn adds_lists_and_renames_models(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let models = stores.models;
    let router = connect(stores, &t, Environment::Sandbox, ProviderKind::OpenRouter, "Router", 1).await;
    let other = connect(
        stores,
        &t,
        Environment::Sandbox,
        ProviderKind::OpenRouter,
        "Router 2",
        2,
    )
    .await;
    let production = connect(stores, &t, Environment::Production, ProviderKind::OpenAi, "OpenAI", 3).await;

    let gpt = add(stores, &t, &router, "openai/gpt-5", "GPT-5").await;
    assert_eq!(
        (
            gpt.environment,
            gpt.route.connection_id,
            gpt.route.kind,
            gpt.route.label.as_str()
        ),
        (Environment::Sandbox, router.id, ProviderKind::OpenRouter, "Router")
    );
    assert_eq!(
        (gpt.provider_model_id.as_str(), gpt.context_window, gpt.supports_tools),
        ("openai/gpt-5", Some(400_000), true)
    );
    assert_eq!((gpt.is_default, gpt.agent_count), (false, 0));
    assert_eq!(
        models
            .find_connection(org, None, router.id)
            .await
            .unwrap()
            .unwrap()
            .model_count,
        1
    );
    assert_eq!(
        models
            .create_model(
                org,
                Environment::Sandbox,
                ModelId::generate(),
                admin(&t),
                &new_model(&router, "openai/gpt-5", "Again"),
                Timestamp::now(),
                &model_entry(AuditAction::ModelCreated),
            )
            .await,
        Err(ModelStoreError::ModelExists),
        "once per connection"
    );
    assert_eq!(
        models
            .create_model(
                org,
                Environment::Sandbox,
                ModelId::generate(),
                admin(&t),
                &new_model(&production, "gpt-5", "GPT-5"),
                Timestamp::now(),
                &model_entry(AuditAction::ModelCreated),
            )
            .await,
        Err(ModelStoreError::ConnectionNotFound),
        "from a connection of its own environment"
    );
    // The route is the model's identity: another connection, another model.
    let twin = add(stores, &t, &other, "openai/gpt-5", "GPT-5").await;
    assert_ne!(twin.id, gpt.id);
    let alpha = add(stores, &t, &router, "x/alpha", "alpha").await;
    let beta = add(stores, &t, &router, "x/beta", "Beta").await;
    let in_production = add(stores, &t, &production, "gpt-5", "GPT-5").await;

    assert_eq!(
        models
            .added_models(org, Environment::Sandbox, router.id)
            .await
            .unwrap()
            .len(),
        3
    );
    assert!(
        models
            .added_models(org, Environment::Production, router.id)
            .await
            .unwrap()
            .is_empty()
    );

    let list = |filter: ModelFilter, cursor: Option<String>, limit: u32| async move {
        models
            .list_models(org, &filter, cursor.as_deref(), limit)
            .await
            .unwrap()
    };
    let sandbox = ModelFilter {
        environment: Some(Environment::Sandbox),
        ..ModelFilter::default()
    };
    let all = list(sandbox.clone(), None, 100).await;
    let names: Vec<&str> = all.items.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, ["alpha", "Beta", "GPT-5", "GPT-5"], "by name, any case");
    assert_eq!(all.total, 4);
    assert!(all.items.iter().all(|m| m.environment == Environment::Sandbox));
    let mut paged = Vec::new();
    let mut cursor = None;
    loop {
        let page = list(sandbox.clone(), cursor, 1).await;
        assert_eq!(page.total, 4);
        paged.extend(page.items.into_iter().map(|m| m.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        paged,
        all.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        "pages follow the list"
    );
    let mine = list(
        ModelFilter {
            connection_id: Some(router.id),
            ..ModelFilter::default()
        },
        None,
        100,
    )
    .await;
    assert_eq!(
        mine.items.iter().map(|m| m.id).collect::<Vec<_>>(),
        [alpha.id, beta.id, gpt.id]
    );
    let searched = list(
        ModelFilter {
            q: Some(" X/BE ".to_owned()),
            ..ModelFilter::default()
        },
        None,
        100,
    )
    .await;
    assert_eq!(searched.items.iter().map(|m| m.id).collect::<Vec<_>>(), [beta.id]);
    let nothing = list(
        ModelFilter {
            matches_nothing: true,
            ..ModelFilter::default()
        },
        None,
        100,
    )
    .await;
    assert_eq!((nothing.items.len(), nothing.total), (0, 0));
    assert_eq!(
        list(ModelFilter::default(), None, 100).await.total,
        5,
        "both environments without a filter"
    );
    assert!(matches!(
        models
            .list_models(org, &ModelFilter::default(), Some("not a cursor"), 10)
            .await,
        Err(ModelStoreError::InvalidCursor)
    ));

    let renamed = models
        .rename_model(
            org,
            Environment::Sandbox,
            beta.id,
            admin(&t),
            "Suite model",
            Timestamp::now(),
            &model_entry(AuditAction::ModelUpdated),
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Suite model");
    assert_eq!(models.find_model(org, None, beta.id).await.unwrap(), Some(renamed));
    assert_eq!(
        models
            .rename_model(
                org,
                Environment::Sandbox,
                in_production.id,
                admin(&t),
                "x",
                Timestamp::now(),
                &model_entry(AuditAction::ModelUpdated),
            )
            .await,
        Err(ModelStoreError::ModelNotFound),
        "renamed only in its own environment"
    );
}

async fn keeps_defaults_and_agents_to_their_environment(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let org = t.organization_id;
    let models = stores.models;
    let sandbox = connect(stores, &t, Environment::Sandbox, ProviderKind::Anthropic, "Claude", 1).await;
    let production = connect(
        stores,
        &t,
        Environment::Production,
        ProviderKind::Anthropic,
        "Claude",
        2,
    )
    .await;
    let theirs = connect(
        stores,
        &other,
        Environment::Sandbox,
        ProviderKind::Anthropic,
        "Claude",
        3,
    )
    .await;
    let haiku = add(stores, &t, &sandbox, "claude-haiku-4-5", "Claude Haiku 4.5").await;
    let sonnet = add(stores, &t, &production, "claude-sonnet-4-5", "Claude Sonnet 4.5").await;
    let foreign = add(stores, &other, &theirs, "claude-haiku-4-5", "Claude Haiku 4.5").await;

    // Found within the environment asked for, and only there.
    for (environment, connection, model) in [
        (Environment::Sandbox, &sandbox, &haiku),
        (Environment::Production, &production, &sonnet),
    ] {
        let elsewhere = match environment {
            Environment::Sandbox => Environment::Production,
            Environment::Production => Environment::Sandbox,
        };
        assert_eq!(
            models
                .find_connection(org, Some(environment), connection.id)
                .await
                .unwrap()
                .map(|found| found.id),
            Some(connection.id)
        );
        assert_eq!(
            models
                .find_connection(org, Some(elsewhere), connection.id)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            models
                .find_model(org, Some(environment), model.id)
                .await
                .unwrap()
                .map(|found| found.id),
            Some(model.id)
        );
        assert_eq!(models.find_model(org, Some(elsewhere), model.id).await.unwrap(), None);
    }

    let none = models.default_model(org, Environment::Sandbox).await.unwrap();
    assert_eq!(
        (none.model, none.updated_at, none.updated_by),
        (None, None, None),
        "none until one is chosen"
    );
    for refused in [sonnet.id, foreign.id, ModelId::generate()] {
        assert_eq!(
            set_default(stores, &t, Environment::Sandbox, Some(refused)).await,
            Err(ModelStoreError::UnknownModel)
        );
    }
    let chosen = set_default(stores, &t, Environment::Sandbox, Some(haiku.id))
        .await
        .unwrap();
    assert_eq!(chosen.environment, Environment::Sandbox);
    assert_eq!(
        chosen.model.as_ref().map(|m| (m.id, m.is_default)),
        Some((haiku.id, true))
    );
    assert!(chosen.updated_at.is_some());
    assert_eq!(chosen.updated_by.as_ref().map(|user| user.id), Some(t.admin_user));
    assert_eq!(models.default_model(org, Environment::Sandbox).await.unwrap(), chosen);
    assert_eq!(
        models.default_model(org, Environment::Production).await.unwrap().model,
        None,
        "one environment's default never stands in for the other's"
    );
    assert!(
        models
            .find_model(org, None, haiku.id)
            .await
            .unwrap()
            .unwrap()
            .is_default
    );
    let cleared = set_default(stores, &t, Environment::Sandbox, None).await.unwrap();
    assert_eq!(cleared.model, None);
    assert!(cleared.updated_at.is_some());
    assert_eq!(
        events(stores, &t, &[AuditAction::DefaultModelUpdated]).await,
        [
            (AuditAction::DefaultModelUpdated, Some(Environment::Sandbox)),
            (AuditAction::DefaultModelUpdated, Some(Environment::Sandbox)),
        ]
    );

    // An agent's model is one of its workspace's environment, of its organization.
    let modelled = agent(stores, &t, t.sandbox, "Modelled", Some(haiku.id)).await.unwrap();
    assert_eq!(modelled.model_id, Some(haiku.id));
    for refused in [sonnet.id, foreign.id, ModelId::generate()] {
        assert_eq!(
            agent(stores, &t, t.sandbox, "Refused", Some(refused)).await,
            Err(AgentError::UnknownModel)
        );
    }
    let unmodelled = agent(stores, &t, t.production, "Defaulted", None).await.unwrap();
    assert_eq!(unmodelled.model_id, None);
    let change = |model_id: Option<ModelId>| AgentChanges {
        model_id: Some(model_id),
        ..AgentChanges::default()
    };
    assert_eq!(
        stores
            .agents
            .update_agent(org, t.production, unmodelled.id, &change(Some(haiku.id)), &|_| {
                by_suite_in(t.production)
            })
            .await,
        Err(AgentError::UnknownModel),
        "a Sandbox model in a Production workspace"
    );
    let chosen = stores
        .agents
        .update_agent(org, t.production, unmodelled.id, &change(Some(sonnet.id)), &|_| {
            by_suite_in(t.production)
        })
        .await
        .unwrap();
    assert_eq!(chosen.model_id, Some(sonnet.id));
    let renamed = stores
        .agents
        .update_agent(
            org,
            t.production,
            unmodelled.id,
            &AgentChanges {
                name: Some("Renamed".to_owned()),
                ..AgentChanges::default()
            },
            &|_| by_suite_in(t.production),
        )
        .await
        .unwrap();
    assert_eq!(renamed.model_id, Some(sonnet.id), "kept when not named");
    assert_eq!(
        models
            .find_model(org, None, sonnet.id)
            .await
            .unwrap()
            .unwrap()
            .agent_count,
        1
    );
    let back = stores
        .agents
        .update_agent(org, t.production, unmodelled.id, &change(None), &|_| {
            by_suite_in(t.production)
        })
        .await
        .unwrap();
    assert_eq!(back.model_id, None, "back to the default");
    assert_eq!(
        models
            .find_model(org, None, sonnet.id)
            .await
            .unwrap()
            .unwrap()
            .agent_count,
        0
    );
    assert_eq!(
        models
            .find_model(org, None, haiku.id)
            .await
            .unwrap()
            .unwrap()
            .agent_count,
        1
    );
}

async fn keeps_what_is_in_use(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let models = stores.models;
    let router = connect(stores, &t, Environment::Sandbox, ProviderKind::OpenRouter, "Router", 1).await;
    let gpt = add(stores, &t, &router, "openai/gpt-5", "GPT-5").await;
    let gemini = add(stores, &t, &router, "google/gemini-2.5-flash", "Gemini 2.5 Flash").await;
    let delete_model = |model_id: ModelId| async move {
        models
            .delete_model(
                org,
                Environment::Sandbox,
                model_id,
                admin(&t),
                &model_entry(AuditAction::ModelDeleted),
            )
            .await
    };
    let delete_connection = || async move {
        models
            .delete_connection(
                org,
                Environment::Sandbox,
                router.id,
                admin(&t),
                &connection_entry(AuditAction::ModelConnectionDeleted),
            )
            .await
    };

    let mut holders = Vec::new();
    for index in 0..7 {
        let workspace = if index % 2 == 0 { t.sandbox } else { t.sibling };
        holders.push(
            agent(stores, &t, workspace, &format!("Holder {index}"), Some(gpt.id))
                .await
                .unwrap(),
        );
    }
    let Err(ModelStoreError::InUse(held)) = delete_model(gpt.id).await else {
        panic!("a model agents hold is kept");
    };
    assert_eq!(held.default_model, None);
    assert_eq!(held.agent_total, 7);
    assert_eq!(held.agents.len(), 5, "names the first five");
    assert_eq!(held.agents[0].agent, "Holder 0");
    assert!(!held.agents[0].workspace.is_empty());
    let Err(ModelStoreError::InUse(held)) = delete_connection().await else {
        panic!("a connection whose models agents hold is kept");
    };
    assert_eq!(held.agent_total, 7);
    assert!(models.find_model(org, None, gpt.id).await.unwrap().is_some());

    for holder in &holders {
        stores
            .agents
            .delete_agent(org, holder.workspace_id, holder.id, &|_| {
                by_suite_in(holder.workspace_id)
            })
            .await
            .unwrap();
    }
    set_default(stores, &t, Environment::Sandbox, Some(gemini.id))
        .await
        .unwrap();
    let Err(ModelStoreError::InUse(held)) = delete_model(gemini.id).await else {
        panic!("the default is kept");
    };
    assert_eq!(
        (held.default_model.as_deref(), held.agent_total),
        (Some("Gemini 2.5 Flash"), 0)
    );
    assert!(matches!(delete_connection().await, Err(ModelStoreError::InUse(_))));
    // Another model is free.
    delete_model(gpt.id).await.unwrap();
    assert_eq!(models.find_model(org, None, gpt.id).await.unwrap(), None);
    assert_eq!(delete_model(gpt.id).await, Err(ModelStoreError::ModelNotFound));

    set_default(stores, &t, Environment::Sandbox, None).await.unwrap();
    delete_connection().await.unwrap();
    assert_eq!(models.find_connection(org, None, router.id).await.unwrap(), None);
    assert_eq!(
        models.find_model(org, None, gemini.id).await.unwrap(),
        None,
        "its models go with it"
    );
    assert_eq!(
        models
            .connection_key(org, Environment::Sandbox, router.id)
            .await
            .unwrap(),
        None,
        "and its key"
    );
    assert_eq!(delete_connection().await, Err(ModelStoreError::ConnectionNotFound));
    assert_eq!(
        events(
            stores,
            &t,
            &[AuditAction::ModelDeleted, AuditAction::ModelConnectionDeleted]
        )
        .await,
        [
            (AuditAction::ModelConnectionDeleted, Some(Environment::Sandbox)),
            (AuditAction::ModelDeleted, Some(Environment::Sandbox)),
        ],
        "a refused removal records nothing"
    );
}

async fn resolves_a_runs_model(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let models = stores.models;
    let sandbox = connect(stores, &t, Environment::Sandbox, ProviderKind::Anthropic, "Claude", 1).await;
    let production = connect(stores, &t, Environment::Production, ProviderKind::OpenAi, "OpenAI", 2).await;
    let haiku = add(stores, &t, &sandbox, "claude-haiku-4-5", "Claude Haiku 4.5").await;
    let sonnet = add(stores, &t, &sandbox, "claude-sonnet-4-5", "Claude Sonnet 4.5").await;
    let gpt = add(stores, &t, &production, "gpt-5", "GPT-5").await;

    assert_eq!(models.run_model(org, t.sandbox, None).await.unwrap(), None, "neither");
    let overridden = models.run_model(org, t.sandbox, Some(haiku.id)).await.unwrap().unwrap();
    assert_eq!(
        (
            overridden.model_id,
            overridden.connection_id,
            overridden.provider_model_id.as_str()
        ),
        (haiku.id, sandbox.id, "claude-haiku-4-5")
    );
    assert_eq!(
        models.run_model(org, t.sandbox, Some(gpt.id)).await.unwrap(),
        None,
        "never another environment's model"
    );
    set_default(stores, &t, Environment::Sandbox, Some(sonnet.id))
        .await
        .unwrap();
    assert_eq!(
        models
            .run_model(org, t.sibling, None)
            .await
            .unwrap()
            .map(|m| m.model_id),
        Some(sonnet.id)
    );
    assert_eq!(
        models.run_model(org, t.production, None).await.unwrap(),
        None,
        "Production has no default"
    );
    assert_eq!(
        models
            .run_model(org, t.sibling, Some(haiku.id))
            .await
            .unwrap()
            .map(|m| m.model_id),
        Some(haiku.id),
        "the override first"
    );

    let route = |model_id: ModelId, connection_id: ModelConnectionId| async move {
        models
            .route_state(org, t.sandbox, model_id, connection_id)
            .await
            .unwrap()
    };
    assert_eq!(route(haiku.id, sandbox.id).await, RouteState::Usable { key_version: 1 });
    assert_eq!(
        route(haiku.id, production.id).await,
        RouteState::Removed,
        "through another connection"
    );
    assert_eq!(
        route(gpt.id, production.id).await,
        RouteState::Removed,
        "another environment's"
    );
    models
        .mark_key_refused(org, Environment::Sandbox, sandbox.id, 1)
        .await
        .unwrap();
    assert_eq!(
        route(haiku.id, sandbox.id).await,
        RouteState::KeyInvalid {
            connection_label: "Claude".to_owned()
        }
    );
    models
        .delete_model(
            org,
            Environment::Sandbox,
            haiku.id,
            admin(&t),
            &model_entry(AuditAction::ModelDeleted),
        )
        .await
        .unwrap();
    assert_eq!(route(haiku.id, sandbox.id).await, RouteState::Removed);
}

async fn lets_only_managers_change(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let models = stores.models;
    let sandbox = connect(stores, &t, Environment::Sandbox, ProviderKind::Anthropic, "Claude", 1).await;
    let haiku = add(stores, &t, &sandbox, "claude-haiku-4-5", "Claude Haiku 4.5").await;
    let before = events(stores, &t, AuditAction::ALL).await.len();

    for manager in [ModelManager::Member(t.viewer), ModelManager::ApiKey(t.production_key)] {
        let refused = Err::<(), _>(ModelStoreError::NotAllowed);
        assert_eq!(
            models
                .create_connection(
                    org,
                    ModelConnectionId::generate(),
                    manager,
                    &NewConnection {
                        environment: Environment::Sandbox,
                        kind: ProviderKind::OpenAi,
                        label: "x".to_owned(),
                        key: key(5, "abcd"),
                    },
                    Timestamp::now(),
                    &connection_entry(AuditAction::ModelConnectionCreated),
                )
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            models
                .rename_connection(
                    org,
                    Environment::Sandbox,
                    sandbox.id,
                    manager,
                    "x",
                    Timestamp::now(),
                    &connection_entry(AuditAction::ModelConnectionUpdated),
                )
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            models
                .replace_key(
                    org,
                    Environment::Sandbox,
                    sandbox.id,
                    manager,
                    &KeyReplacement {
                        expected_version: 1,
                        key: key(6, "abcd"),
                    },
                    Timestamp::now(),
                    &connection_entry(AuditAction::ModelConnectionKeyReplaced),
                )
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            models
                .delete_connection(
                    org,
                    Environment::Sandbox,
                    sandbox.id,
                    manager,
                    &connection_entry(AuditAction::ModelConnectionDeleted),
                )
                .await,
            refused
        );
        assert_eq!(
            models
                .create_model(
                    org,
                    Environment::Sandbox,
                    ModelId::generate(),
                    manager,
                    &new_model(&sandbox, "claude-opus-4-1", "Opus"),
                    Timestamp::now(),
                    &model_entry(AuditAction::ModelCreated),
                )
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            models
                .rename_model(
                    org,
                    Environment::Sandbox,
                    haiku.id,
                    manager,
                    "x",
                    Timestamp::now(),
                    &model_entry(AuditAction::ModelUpdated),
                )
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            models
                .delete_model(
                    org,
                    Environment::Sandbox,
                    haiku.id,
                    manager,
                    &model_entry(AuditAction::ModelDeleted),
                )
                .await,
            refused
        );
        assert_eq!(
            models
                .set_default_model(
                    org,
                    Environment::Sandbox,
                    manager,
                    Some(haiku.id),
                    None,
                    Timestamp::now(),
                    &|_| by_suite_in(t.sandbox),
                )
                .await
                .map(|_| ()),
            refused
        );
    }
    assert_eq!(
        events(stores, &t, AuditAction::ALL).await.len(),
        before,
        "nothing refused is recorded"
    );
    assert_eq!(
        models.find_model(org, None, haiku.id).await.unwrap(),
        Some(haiku.clone())
    );

    // A key of the environment may, and is recorded as no person.
    let by_key = models
        .set_default_model(
            org,
            Environment::Sandbox,
            ModelManager::ApiKey(t.sandbox_key),
            Some(haiku.id),
            None,
            Timestamp::now(),
            &|_| by_suite_in(t.sandbox),
        )
        .await
        .unwrap();
    assert_eq!((by_key.model.map(|m| m.id), by_key.updated_by), (Some(haiku.id), None));
}

async fn keeps_organizations_apart(stores: &Stores<'_>, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let models = stores.models;
    let connection = connect(stores, &a, Environment::Sandbox, ProviderKind::OpenAi, "OpenAI", 1).await;
    let model = add(stores, &a, &connection, "gpt-5", "GPT-5").await;
    set_default(stores, &a, Environment::Sandbox, Some(model.id))
        .await
        .unwrap();
    let other = b.organization_id;

    assert!(models.list_connections(other, None).await.unwrap().is_empty());
    assert_eq!(models.find_connection(other, None, connection.id).await.unwrap(), None);
    assert_eq!(
        models
            .connection_key(other, Environment::Sandbox, connection.id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(models.find_model(other, None, model.id).await.unwrap(), None);
    assert_eq!(
        models
            .list_models(other, &ModelFilter::default(), None, 100)
            .await
            .unwrap()
            .total,
        0
    );
    assert!(
        models
            .added_models(other, Environment::Sandbox, connection.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        models.default_model(other, Environment::Sandbox).await.unwrap().model,
        None
    );
    assert_eq!(models.run_model(other, b.sandbox, Some(model.id)).await.unwrap(), None);
    assert_eq!(
        models.run_model(other, a.sandbox, None).await.unwrap(),
        None,
        "another organization's workspace"
    );
    assert_eq!(
        models
            .route_state(other, b.sandbox, model.id, connection.id)
            .await
            .unwrap(),
        RouteState::Removed
    );

    // Changes from the other organization, by its own admin, reach nothing.
    let manager = admin(&b);
    assert_eq!(
        models
            .rename_connection(
                other,
                Environment::Sandbox,
                connection.id,
                manager,
                "Owned",
                Timestamp::now(),
                &connection_entry(AuditAction::ModelConnectionUpdated),
            )
            .await,
        Err(ModelStoreError::ConnectionNotFound)
    );
    assert_eq!(
        models
            .replace_key(
                other,
                Environment::Sandbox,
                connection.id,
                manager,
                &KeyReplacement {
                    expected_version: 1,
                    key: key(7, "abcd"),
                },
                Timestamp::now(),
                &connection_entry(AuditAction::ModelConnectionKeyReplaced),
            )
            .await,
        Err(ModelStoreError::ConnectionNotFound)
    );
    models
        .mark_key_refused(other, Environment::Sandbox, connection.id, 1)
        .await
        .unwrap();
    assert_eq!(
        models
            .delete_connection(
                other,
                Environment::Sandbox,
                connection.id,
                manager,
                &connection_entry(AuditAction::ModelConnectionDeleted),
            )
            .await,
        Err(ModelStoreError::ConnectionNotFound)
    );
    assert_eq!(
        models
            .create_model(
                other,
                Environment::Sandbox,
                ModelId::generate(),
                manager,
                &new_model(&connection, "gpt-5-mini", "GPT-5 mini"),
                Timestamp::now(),
                &model_entry(AuditAction::ModelCreated),
            )
            .await,
        Err(ModelStoreError::ConnectionNotFound),
        "a model from another organization's connection"
    );
    assert_eq!(
        models
            .rename_model(
                other,
                Environment::Sandbox,
                model.id,
                manager,
                "Owned",
                Timestamp::now(),
                &model_entry(AuditAction::ModelUpdated),
            )
            .await,
        Err(ModelStoreError::ModelNotFound)
    );
    assert_eq!(
        models
            .delete_model(
                other,
                Environment::Sandbox,
                model.id,
                manager,
                &model_entry(AuditAction::ModelDeleted),
            )
            .await,
        Err(ModelStoreError::ModelNotFound)
    );
    assert_eq!(
        set_default(stores, &b, Environment::Sandbox, Some(model.id)).await,
        Err(ModelStoreError::UnknownModel)
    );
    assert_eq!(
        agent(stores, &b, b.sandbox, "Borrower", Some(model.id)).await,
        Err(AgentError::UnknownModel)
    );
    // An admin of one organization manages nothing in another.
    assert_eq!(
        models
            .rename_connection(
                a.organization_id,
                Environment::Sandbox,
                connection.id,
                manager,
                "Owned",
                Timestamp::now(),
                &connection_entry(AuditAction::ModelConnectionUpdated),
            )
            .await,
        Err(ModelStoreError::NotAllowed)
    );

    // Untouched.
    let kept = models
        .find_connection(a.organization_id, None, connection.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (kept.label.as_str(), kept.key_version, kept.status),
        ("OpenAI", 1, ConnectionStatus::Active)
    );
    let kept = models
        .find_model(a.organization_id, None, model.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((kept.name.as_str(), kept.is_default), ("GPT-5", true));
}
