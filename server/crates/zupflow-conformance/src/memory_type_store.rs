//! The `MemoryTypeStore` suite: an organization's memory types and its
//! default, reached only through their own organization; a name the
//! organization's once, two made at once included; a type kept while the
//! default or an agent names it, the first agents named, and its notes gone
//! with it; a default only of one of the organization's types, set and
//! cleared by whoever chose, and never one deleted at once; an agent's
//! memory only a type of its own organization (through the `AgentStore`),
//! and never one deleted at once;
//! every change made only by an owner or admin who still may, with one
//! audit event, and none for a refused change; a run's memory resolved
//! from the agent's own type, else the default, else none; and deleting an
//! agent, a workspace or a type queuing, with it, the job that has each
//! memory kind forget what went, and a refused deletion queuing none.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, a Sandbox and a
//! Production workspace (each named), an active owner, admin and developer,
//! and an admin still invited, with their users.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Map, json};
use zupflow_core::agents::{Agent, AgentChanges, AgentError, AgentStore, NewAgent, SentMemory};
use zupflow_core::audit::{
    AuditAction, AuditActor, AuditEntry, AuditFilter, AuditObject, AuditPlace, AuditStore, AuditTarget,
};
use zupflow_core::ids::{AgentId, MemberId, MemoryId, MemoryTypeId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::installations::ShortId;
use zupflow_core::jobs::JobQueue;
use zupflow_core::memory::notes::{self, NewNote, NoteStore};
use zupflow_core::memory::type_store::{
    DefaultMemoryType, MemoryType, MemoryTypeChanges, MemoryTypeError, MemoryTypeStore, NewMemoryType, RunMemory,
};
use zupflow_core::memory::{
    AgentMemory, FORGET_JOB, Forget, MemoryKind, MemoryScope, MemoryWrites, Origin, Settings, record_nothing,
};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::{WorkspaceError, WorkspaceStore};

use crate::audit_store::by_suite_in;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub sandbox: WorkspaceId,
    pub production: WorkspaceId,
    /// Active members, and the users they are.
    pub owner: MemberId,
    pub owner_user: UserId,
    pub admin: MemberId,
    pub admin_user: UserId,
    pub developer: MemberId,
    pub developer_user: UserId,
    /// Invited as an admin, not yet joined.
    pub invited_admin: MemberId,
    pub invited_admin_user: UserId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The stores the suite drives: the one under test, agents that name its
/// types, the notes kept in them, the log its changes write, the workspaces
/// agents work in, and the queue deletions queue their forget jobs on, in
/// the same store.
pub struct Stores<'a> {
    pub types: &'a dyn MemoryTypeStore,
    pub agents: &'a dyn AgentStore,
    pub notes: &'a dyn NoteStore,
    pub audit: &'a dyn AuditStore,
    pub workspaces: &'a dyn WorkspaceStore,
    pub jobs: &'a dyn JobQueue,
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    creates_lists_and_changes_types(&stores, &prepare).await;
    keeps_a_name_once_when_two_are_made_at_once(&stores, &prepare).await;
    sets_and_clears_the_default(&stores, &prepare).await;
    keeps_what_is_in_use(&stores, &prepare).await;
    never_deletes_a_type_an_agent_names_at_once(&stores, &prepare).await;
    never_deletes_a_type_chosen_as_the_default_at_once(&stores, &prepare).await;
    resolves_a_runs_memory(&stores, &prepare).await;
    lets_only_owners_and_admins_change(&stores, &prepare).await;
    keeps_organizations_apart(&stores, &prepare).await;
    queues_forgetting_with_each_deletion(&stores, &prepare).await;
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        sandbox: WorkspaceId::generate(),
        production: WorkspaceId::generate(),
        owner: MemberId::generate(),
        owner_user: UserId::generate(),
        admin: MemberId::generate(),
        admin_user: UserId::generate(),
        developer: MemberId::generate(),
        developer_user: UserId::generate(),
        invited_admin: MemberId::generate(),
        invited_admin_user: UserId::generate(),
    };
    prepare(tenant).await;
    tenant
}

fn at(millis: i64) -> Timestamp {
    Timestamp::from_millis(1_800_000_000_000 + millis)
}

fn notes_kind() -> MemoryKind {
    MemoryKind::parse(notes::KIND).unwrap()
}

/// `notes` settings, checked: the defaults, with `values` over them.
fn settings(values: serde_json::Value) -> Settings {
    let serde_json::Value::Object(values) = values else {
        panic!("settings are an object");
    };
    notes::check_settings(&values).unwrap()
}

fn new_type(name: &str, writes: MemoryWrites) -> NewMemoryType {
    NewMemoryType {
        name: name.to_owned(),
        kind: notes_kind(),
        settings: settings(json!({})),
        writes,
    }
}

fn actor() -> AuditActor {
    AuditActor::System {
        name: "Conformance".to_owned(),
    }
}

fn type_entry(action: AuditAction) -> impl Fn(&MemoryType) -> AuditEntry + Send + Sync {
    move |memory_type| {
        AuditEntry::new(
            actor(),
            action,
            AuditTarget::new(AuditObject::MemoryType(memory_type.id), &memory_type.name),
            AuditPlace::Organization,
        )
    }
}

fn default_entry(default: &DefaultMemoryType) -> AuditEntry {
    let memory_type = default.memory_type.as_ref();
    AuditEntry::new(
        actor(),
        AuditAction::DefaultMemoryTypeUpdated,
        AuditTarget::new(
            AuditObject::DefaultMemoryType(memory_type.map(|memory_type| memory_type.id)),
            memory_type.map_or("None", |memory_type| memory_type.name.as_str()),
        ),
        AuditPlace::Organization,
    )
}

async fn create(stores: &Stores<'_>, t: &Tenant, name: &str, writes: MemoryWrites) -> MemoryType {
    create_by(stores, t, t.admin, &new_type(name, writes), at(1))
        .await
        .unwrap()
}

async fn create_by(
    stores: &Stores<'_>,
    t: &Tenant,
    manager: MemberId,
    memory_type: &NewMemoryType,
    now: Timestamp,
) -> Result<MemoryType, MemoryTypeError> {
    stores
        .types
        .create_type(
            t.organization_id,
            MemoryTypeId::generate(),
            manager,
            memory_type,
            now,
            &type_entry(AuditAction::MemoryTypeCreated),
        )
        .await
}

async fn update(
    stores: &Stores<'_>,
    t: &Tenant,
    memory_type_id: MemoryTypeId,
    changes: MemoryTypeChanges,
    now: Timestamp,
) -> Result<MemoryType, MemoryTypeError> {
    stores
        .types
        .update_type(
            t.organization_id,
            memory_type_id,
            t.admin,
            &changes,
            now,
            &type_entry(AuditAction::MemoryTypeUpdated),
        )
        .await
}

async fn delete(stores: &Stores<'_>, t: &Tenant, memory_type_id: MemoryTypeId) -> Result<(), MemoryTypeError> {
    stores
        .types
        .delete_type(
            t.organization_id,
            memory_type_id,
            t.admin,
            &type_entry(AuditAction::MemoryTypeDeleted),
        )
        .await
}

async fn set_default(
    stores: &Stores<'_>,
    t: &Tenant,
    manager: MemberId,
    memory_type_id: Option<MemoryTypeId>,
    now: Timestamp,
) -> Result<DefaultMemoryType, MemoryTypeError> {
    stores
        .types
        .set_default_type(t.organization_id, manager, memory_type_id, now, &default_entry)
        .await
}

async fn agent(
    stores: &Stores<'_>,
    t: &Tenant,
    workspace: WorkspaceId,
    name: &str,
    memory: AgentMemory,
) -> Result<Agent, AgentError> {
    stores
        .agents
        .create_agent(
            t.organization_id,
            workspace,
            AgentId::generate(),
            &NewAgent {
                name: name.to_owned(),
                description: String::new(),
                model_id: None,
                memory: SentMemory::of(memory),
                instructions: String::new(),
            },
            &|_| by_suite_in(workspace),
        )
        .await
}

async fn change_memory(
    stores: &Stores<'_>,
    t: &Tenant,
    agent: &Agent,
    memory: AgentMemory,
) -> Result<Agent, AgentError> {
    stores
        .agents
        .update_agent(
            t.organization_id,
            agent.workspace_id,
            agent.id,
            &AgentChanges {
                memory: SentMemory::of(memory),
                ..AgentChanges::default()
            },
            &|_| by_suite_in(agent.workspace_id),
        )
        .await
}

/// The organization's events about memory types, newest first.
async fn events(stores: &Stores<'_>, t: &Tenant) -> Vec<(AuditAction, String)> {
    let actions = [
        AuditAction::MemoryTypeCreated,
        AuditAction::MemoryTypeUpdated,
        AuditAction::MemoryTypeDeleted,
        AuditAction::DefaultMemoryTypeUpdated,
    ];
    stores
        .audit
        .list_audit_events(t.organization_id, &AuditFilter::default(), None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| actions.contains(&event.action))
        .map(|event| (event.action, event.target.name))
        .collect()
}

fn names(types: &[MemoryType]) -> Vec<&str> {
    types.iter().map(|memory_type| memory_type.name.as_str()).collect()
}

async fn creates_lists_and_changes_types(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let types = stores.types;
    let support = create(stores, &t, "Support", MemoryWrites::Allow).await;
    assert_eq!(
        (
            support.organization_id,
            support.name.as_str(),
            &support.kind,
            support.writes,
            support.agent_count,
            support.is_default
        ),
        (org, "Support", &notes_kind(), MemoryWrites::Allow, 0, false)
    );
    assert_eq!(
        support.settings,
        Map::from_iter([
            ("max_notes".to_owned(), json!(200)),
            ("recall_characters".to_owned(), json!(8000)),
        ]),
        "as the kind checked them"
    );
    assert_eq!((support.created_at, support.updated_at), (at(1), at(1)));
    let beta = create(stores, &t, "beta", MemoryWrites::Approve).await;
    let alpha = create(stores, &t, "Alpha", MemoryWrites::Deny).await;
    assert_eq!(types.find_type(org, beta.id).await.unwrap(), Some(beta.clone()));
    assert_eq!(types.find_type(org, MemoryTypeId::generate()).await.unwrap(), None);

    let page = types.list_types(org, None, 2).await.unwrap();
    assert_eq!(
        (names(&page.items), page.total),
        (vec!["Alpha", "beta"], 3),
        "by name, in any case"
    );
    let rest = types.list_types(org, page.next_cursor.as_deref(), 2).await.unwrap();
    assert_eq!((names(&rest.items), rest.next_cursor), (vec!["Support"], None));
    assert_eq!(rest.items[0], support);
    assert_eq!(
        types.list_types(org, Some("not-a-cursor"), 2).await,
        Err(MemoryTypeError::InvalidCursor)
    );

    assert_eq!(
        create_by(stores, &t, t.admin, &new_type("Support", MemoryWrites::Deny), at(2)).await,
        Err(MemoryTypeError::NameTaken),
        "a name once in the organization"
    );
    let rename = |name: &str| MemoryTypeChanges {
        name: Some(name.to_owned()),
        ..MemoryTypeChanges::default()
    };
    assert_eq!(
        update(stores, &t, alpha.id, rename("Support"), at(2)).await,
        Err(MemoryTypeError::NameTaken)
    );
    let renamed = update(stores, &t, beta.id, rename("Gamma"), at(3)).await.unwrap();
    assert_eq!(
        renamed,
        MemoryType {
            name: "Gamma".to_owned(),
            updated_at: at(3),
            ..beta.clone()
        },
        "an update changes only what it names"
    );
    let changed = update(
        stores,
        &t,
        beta.id,
        MemoryTypeChanges {
            name: None,
            settings: Some(settings(json!({ "recall_characters": 100 }))),
            writes: Some(MemoryWrites::Deny),
        },
        at(4),
    )
    .await
    .unwrap();
    assert_eq!(
        (changed.name.as_str(), changed.writes, changed.updated_at),
        ("Gamma", MemoryWrites::Deny, at(4))
    );
    assert_eq!(
        changed.settings,
        Map::from_iter([
            ("max_notes".to_owned(), json!(200)),
            ("recall_characters".to_owned(), json!(100)),
        ])
    );
    assert_eq!(changed.kind, notes_kind(), "its kind never changes");
    update(stores, &t, beta.id, rename("Gamma"), at(5)).await.unwrap();
    assert_eq!(
        update(stores, &t, MemoryTypeId::generate(), rename("Delta"), at(5)).await,
        Err(MemoryTypeError::NotFound)
    );
    assert_eq!(
        types
            .find_type(org, beta.id)
            .await
            .unwrap()
            .map(|found| found.updated_at),
        Some(at(5))
    );
    assert_eq!(
        events(stores, &t).await,
        [
            (AuditAction::MemoryTypeUpdated, "Gamma".to_owned()),
            (AuditAction::MemoryTypeUpdated, "Gamma".to_owned()),
            (AuditAction::MemoryTypeUpdated, "Gamma".to_owned()),
            (AuditAction::MemoryTypeCreated, "Alpha".to_owned()),
            (AuditAction::MemoryTypeCreated, "beta".to_owned()),
            (AuditAction::MemoryTypeCreated, "Support".to_owned()),
        ],
        "one event a change, none for a refused one"
    );
}

async fn keeps_a_name_once_when_two_are_made_at_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let memory_type = new_type("Shared", MemoryWrites::Allow);
    let (first, second) = tokio::join!(
        create_by(stores, &t, t.admin, &memory_type, at(1)),
        create_by(stores, &t, t.owner, &memory_type, at(1)),
    );
    let mut outcomes = [first.map(|_| ()), second.map(|_| ())];
    outcomes.sort_by_key(Result::is_err);
    assert_eq!(outcomes, [Ok(()), Err(MemoryTypeError::NameTaken)]);
    assert_eq!(
        stores
            .types
            .list_types(t.organization_id, None, 25)
            .await
            .unwrap()
            .total,
        1
    );
}

async fn sets_and_clears_the_default(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let org = t.organization_id;
    let types = stores.types;
    let none = DefaultMemoryType {
        memory_type: None,
        updated_at: None,
        updated_by: None,
    };
    assert_eq!(types.default_type(org).await.unwrap(), none, "none until one is chosen");
    let support = create(stores, &t, "Support", MemoryWrites::Allow).await;
    let private = create(stores, &t, "Private", MemoryWrites::Approve).await;

    let set = set_default(stores, &t, t.admin, Some(support.id), at(2)).await.unwrap();
    assert_eq!(
        set.memory_type,
        Some(MemoryType {
            is_default: true,
            ..support.clone()
        })
    );
    assert_eq!(set.updated_at, Some(at(2)));
    assert_eq!(
        set.updated_by.as_ref().map(|user| user.id),
        Some(t.admin_user),
        "who chose"
    );
    assert_eq!(types.default_type(org).await.unwrap(), set);
    assert!(types.find_type(org, support.id).await.unwrap().unwrap().is_default);
    assert!(!types.find_type(org, private.id).await.unwrap().unwrap().is_default);

    let theirs = create(stores, &other, "Theirs", MemoryWrites::Allow).await;
    for unknown in [theirs.id, MemoryTypeId::generate()] {
        assert_eq!(
            set_default(stores, &t, t.admin, Some(unknown), at(3)).await,
            Err(MemoryTypeError::UnknownType),
            "only one of its own types"
        );
    }
    assert_eq!(types.default_type(org).await.unwrap(), set, "as it was");
    assert_eq!(
        types.default_type(other.organization_id).await.unwrap(),
        none,
        "each organization its own"
    );

    let by_owner = set_default(stores, &t, t.owner, Some(private.id), at(4)).await.unwrap();
    assert_eq!(
        (
            by_owner.memory_type.map(|memory_type| memory_type.id),
            by_owner.updated_by.map(|user| user.id)
        ),
        (Some(private.id), Some(t.owner_user))
    );
    assert!(!types.find_type(org, support.id).await.unwrap().unwrap().is_default);
    let cleared = set_default(stores, &t, t.admin, None, at(5)).await.unwrap();
    assert_eq!(
        (
            cleared.memory_type,
            cleared.updated_at,
            cleared.updated_by.map(|user| user.id)
        ),
        (None, Some(at(5)), Some(t.admin_user)),
        "cleared, and by whom"
    );
    assert_eq!(types.default_type(org).await.unwrap().memory_type, None);
    let listed = types.list_types(org, None, 25).await.unwrap();
    assert!(listed.items.iter().all(|memory_type| !memory_type.is_default));
    assert_eq!(
        events(stores, &t)
            .await
            .into_iter()
            .filter(|(action, _)| *action == AuditAction::DefaultMemoryTypeUpdated)
            .map(|(_, name)| name)
            .collect::<Vec<_>>(),
        ["None", "Private", "Support"],
        "one event a change, none for a refused one"
    );
}

async fn keeps_what_is_in_use(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    let types = stores.types;
    let support = create(stores, &t, "Support", MemoryWrites::Allow).await;
    let other_type = create(stores, &t, "Other", MemoryWrites::Allow).await;
    let mut holders = Vec::new();
    for index in 0..7 {
        let workspace = if index % 2 == 0 { t.sandbox } else { t.production };
        holders.push(
            agent(
                stores,
                &t,
                workspace,
                &format!("Holder {index}"),
                AgentMemory::Type(support.id),
            )
            .await
            .unwrap(),
        );
    }
    agent(stores, &t, t.sandbox, "On the default", AgentMemory::Default)
        .await
        .unwrap();
    assert_eq!(holders[0].memory, AgentMemory::Type(support.id));
    assert_eq!(
        types.find_type(org, support.id).await.unwrap().unwrap().agent_count,
        7,
        "agents on the default don't count"
    );
    let Err(MemoryTypeError::InUse(held)) = delete(stores, &t, support.id).await else {
        panic!("a type agents name is kept");
    };
    assert!(!held.is_default);
    assert_eq!(held.agent_total, 7);
    assert_eq!(
        held.agents
            .iter()
            .map(|holder| holder.agent.as_str())
            .collect::<Vec<_>>(),
        ["Holder 0", "Holder 1", "Holder 2", "Holder 3", "Holder 4"],
        "names the first five, oldest first"
    );
    assert!(held.agents.iter().all(|holder| !holder.workspace.is_empty()));

    for (index, holder) in holders.iter().enumerate() {
        let memory = if index % 2 == 0 {
            AgentMemory::Off
        } else {
            AgentMemory::Default
        };
        let changed = change_memory(stores, &t, holder, memory).await.unwrap();
        assert_eq!(changed.memory, memory, "and forgets the type it named");
    }
    assert_eq!(types.find_type(org, support.id).await.unwrap().unwrap().agent_count, 0);
    set_default(stores, &t, t.admin, Some(support.id), at(2)).await.unwrap();
    let Err(MemoryTypeError::InUse(held)) = delete(stores, &t, support.id).await else {
        panic!("the default is kept");
    };
    assert_eq!((held.is_default, held.agent_total, held.agents.len()), (true, 0, 0));

    // Its notes go with it; another type's stay.
    let holder = &holders[0];
    let origin = Origin::ApiKey {
        api_key_id: zupflow_core::ids::ApiKeyId::generate(),
    };
    let scope = |memory_type_id: MemoryTypeId| MemoryScope {
        organization_id: org,
        workspace_id: holder.workspace_id,
        agent_id: holder.id,
        memory_type_id,
    };
    for (memory_type_id, text) in [(support.id, "Goes with its type."), (other_type.id, "Stays.")] {
        stores
            .notes
            .insert(
                &scope(memory_type_id),
                &NewNote {
                    id: MemoryId::generate(),
                    text,
                    channel_conversation_id: None,
                    origin: &origin,
                    short_ids: &[ShortId::from_bits(1)],
                },
                200,
                at(3),
                &record_nothing,
            )
            .await
            .unwrap();
    }
    set_default(stores, &t, t.admin, None, at(4)).await.unwrap();
    delete(stores, &t, support.id).await.unwrap();
    assert_eq!(types.find_type(org, support.id).await.unwrap(), None);
    assert_eq!(delete(stores, &t, support.id).await, Err(MemoryTypeError::NotFound));
    let kept = |memory_type_id: MemoryTypeId| async move {
        stores
            .notes
            .list(&scope(memory_type_id), None, None, 25)
            .await
            .unwrap()
            .total
    };
    assert_eq!((kept(support.id).await, kept(other_type.id).await), (0, 1));
    assert_eq!(
        events(stores, &t)
            .await
            .into_iter()
            .filter(|(action, _)| *action == AuditAction::MemoryTypeDeleted)
            .collect::<Vec<_>>(),
        [(AuditAction::MemoryTypeDeleted, "Support".to_owned())],
        "a refused removal records nothing"
    );
}

async fn never_deletes_a_type_an_agent_names_at_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    for round in 0..4 {
        let memory_type = create(stores, &t, &format!("Raced {round}"), MemoryWrites::Allow).await;
        let racer = agent(stores, &t, t.sandbox, &format!("Racer {round}"), AgentMemory::Default)
            .await
            .unwrap();
        let (deleted, named) = tokio::join!(
            delete(stores, &t, memory_type.id),
            change_memory(stores, &t, &racer, AgentMemory::Type(memory_type.id)),
        );
        match (deleted, named) {
            (Ok(()), Err(AgentError::UnknownMemoryType)) => {}
            (Err(MemoryTypeError::InUse(held)), Ok(agent)) => {
                assert_eq!((held.agent_total, agent.memory), (1, AgentMemory::Type(memory_type.id)));
            }
            outcome => panic!("both can't happen: {outcome:?}"),
        }
    }
}

/// Choosing a type as the default while it's deleted: one of the two
/// happens, and the other is refused, never both.
async fn never_deletes_a_type_chosen_as_the_default_at_once(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    for round in 0..4 {
        let before = stores.types.default_type(org).await.unwrap();
        let memory_type = create(stores, &t, &format!("Default {round}"), MemoryWrites::Allow).await;
        let (deleted, chosen) = tokio::join!(
            delete(stores, &t, memory_type.id),
            set_default(stores, &t, t.admin, Some(memory_type.id), at(2 + round)),
        );
        let now = stores.types.default_type(org).await.unwrap();
        match (deleted, chosen) {
            (Ok(()), Err(MemoryTypeError::UnknownType)) => {
                assert_eq!(now, before, "the default as it was");
            }
            (Err(MemoryTypeError::InUse(held)), Ok(chosen)) => {
                assert!(held.is_default, "kept as the default");
                assert_eq!(
                    chosen.memory_type.map(|memory_type| memory_type.id),
                    Some(memory_type.id)
                );
                assert_eq!(now.memory_type.map(|memory_type| memory_type.id), Some(memory_type.id));
            }
            outcome => panic!("both can't happen: {outcome:?}"),
        }
    }
}

async fn resolves_a_runs_memory(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let org = t.organization_id;
    let types = stores.types;
    let support = create(stores, &t, "Support", MemoryWrites::Allow).await;
    let private = create_by(
        stores,
        &t,
        t.admin,
        &NewMemoryType {
            settings: settings(json!({ "max_notes": 5 })),
            ..new_type("Private", MemoryWrites::Approve)
        },
        at(1),
    )
    .await
    .unwrap();
    let on_default = agent(stores, &t, t.sandbox, "On the default", AgentMemory::Default)
        .await
        .unwrap();
    let own = agent(stores, &t, t.sandbox, "Its own", AgentMemory::Type(private.id))
        .await
        .unwrap();
    let off = agent(stores, &t, t.sandbox, "Off", AgentMemory::Off).await.unwrap();
    let run_memory =
        |workspace: WorkspaceId, agent: AgentId| async move { types.run_memory(org, workspace, agent).await.unwrap() };
    let of = |memory_type: &MemoryType| RunMemory {
        memory_type_id: memory_type.id,
        kind: memory_type.kind.clone(),
        settings: memory_type.settings.clone(),
        writes: memory_type.writes,
    };

    assert_eq!(run_memory(t.sandbox, on_default.id).await, None, "no default yet");
    assert_eq!(run_memory(t.sandbox, own.id).await, Some(of(&private)));
    assert_eq!(run_memory(t.sandbox, off.id).await, None);
    set_default(stores, &t, t.admin, Some(support.id), at(2)).await.unwrap();
    assert_eq!(
        run_memory(t.sandbox, on_default.id).await,
        Some(of(&support)),
        "the default"
    );
    assert_eq!(
        run_memory(t.sandbox, own.id).await,
        Some(of(&private)),
        "its own type first"
    );
    assert_eq!(run_memory(t.sandbox, off.id).await, None, "off, whatever the default");
    assert_eq!(
        run_memory(t.production, on_default.id).await,
        None,
        "an agent of another workspace"
    );
    assert_eq!(run_memory(t.sandbox, AgentId::generate()).await, None);
    assert_eq!(
        types
            .run_memory(other.organization_id, t.sandbox, on_default.id)
            .await
            .unwrap(),
        None,
        "nor of another organization"
    );

    let changed = update(
        stores,
        &t,
        support.id,
        MemoryTypeChanges {
            writes: Some(MemoryWrites::Deny),
            ..MemoryTypeChanges::default()
        },
        at(3),
    )
    .await
    .unwrap();
    assert_eq!(
        run_memory(t.sandbox, on_default.id).await,
        Some(of(&changed)),
        "as the type is now"
    );
    let own = change_memory(stores, &t, &own, AgentMemory::Default).await.unwrap();
    assert_eq!(own.memory, AgentMemory::Default);
    assert_eq!(run_memory(t.sandbox, own.id).await, Some(of(&changed)));
    let off = change_memory(stores, &t, &off, AgentMemory::Type(private.id))
        .await
        .unwrap();
    assert_eq!(run_memory(t.sandbox, off.id).await, Some(of(&private)));
    set_default(stores, &t, t.admin, None, at(4)).await.unwrap();
    assert_eq!(run_memory(t.sandbox, on_default.id).await, None, "the default cleared");
}

async fn lets_only_owners_and_admins_change(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let org = t.organization_id;
    let types = stores.types;
    let support = create(stores, &t, "Support", MemoryWrites::Allow).await;
    let before = events(stores, &t).await;
    let refused = Err::<(), _>(MemoryTypeError::NotAllowed);
    for manager in [t.developer, t.invited_admin, other.admin, MemberId::generate()] {
        assert_eq!(
            create_by(stores, &t, manager, &new_type("Mine", MemoryWrites::Allow), at(2))
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            types
                .update_type(
                    org,
                    support.id,
                    manager,
                    &MemoryTypeChanges {
                        name: Some("Mine".to_owned()),
                        ..MemoryTypeChanges::default()
                    },
                    at(2),
                    &type_entry(AuditAction::MemoryTypeUpdated),
                )
                .await
                .map(|_| ()),
            refused
        );
        assert_eq!(
            types
                .delete_type(org, support.id, manager, &type_entry(AuditAction::MemoryTypeDeleted))
                .await,
            refused
        );
        assert_eq!(
            set_default(stores, &t, manager, Some(support.id), at(2))
                .await
                .map(|_| ()),
            refused
        );
    }
    assert_eq!(events(stores, &t).await, before, "nothing recorded");
    assert_eq!(types.find_type(org, support.id).await.unwrap(), Some(support));
    assert_eq!(types.default_type(org).await.unwrap().memory_type, None);
    create_by(
        stores,
        &t,
        t.owner,
        &new_type("By the owner", MemoryWrites::Allow),
        at(3),
    )
    .await
    .unwrap();
}

async fn keeps_organizations_apart(stores: &Stores<'_>, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let types = stores.types;
    let ours = create(stores, &a, "Shared name", MemoryWrites::Allow).await;
    let theirs = create(stores, &b, "Shared name", MemoryWrites::Allow).await;
    let listed = types.list_types(b.organization_id, None, 25).await.unwrap();
    assert_eq!(
        (listed.items, listed.total),
        (vec![theirs.clone()], 1),
        "a name per organization"
    );
    assert_eq!(types.find_type(b.organization_id, ours.id).await.unwrap(), None);
    assert_eq!(
        update(
            stores,
            &b,
            ours.id,
            MemoryTypeChanges {
                name: Some("Taken over".to_owned()),
                ..MemoryTypeChanges::default()
            },
            at(2),
        )
        .await,
        Err(MemoryTypeError::NotFound)
    );
    assert_eq!(delete(stores, &b, ours.id).await, Err(MemoryTypeError::NotFound));
    assert_eq!(
        set_default(stores, &b, b.admin, Some(ours.id), at(2)).await,
        Err(MemoryTypeError::UnknownType)
    );

    // An agent names only a type of its own organization.
    for unknown in [ours.id, MemoryTypeId::generate()] {
        assert_eq!(
            agent(stores, &b, b.sandbox, "Borrower", AgentMemory::Type(unknown)).await,
            Err(AgentError::UnknownMemoryType)
        );
    }
    let borrower = agent(stores, &b, b.sandbox, "Borrower", AgentMemory::Default)
        .await
        .unwrap();
    assert_eq!(
        change_memory(stores, &b, &borrower, AgentMemory::Type(ours.id)).await,
        Err(AgentError::UnknownMemoryType)
    );
    assert_eq!(
        stores
            .agents
            .find_agent(b.organization_id, b.sandbox, borrower.id)
            .await
            .unwrap()
            .map(|agent| agent.memory),
        Some(AgentMemory::Default),
        "as it was"
    );
    let holder = agent(stores, &a, a.sandbox, "Holder", AgentMemory::Type(ours.id))
        .await
        .unwrap();
    assert_eq!(
        (
            types
                .find_type(a.organization_id, ours.id)
                .await
                .unwrap()
                .map(|t| t.agent_count),
            types
                .find_type(b.organization_id, theirs.id)
                .await
                .unwrap()
                .map(|t| t.agent_count)
        ),
        (Some(1), Some(0))
    );
    assert_eq!(
        types.run_memory(b.organization_id, a.sandbox, holder.id).await.unwrap(),
        None
    );
    set_default(stores, &a, a.admin, Some(ours.id), at(3)).await.unwrap();
    assert_eq!(types.default_type(b.organization_id).await.unwrap().memory_type, None);
    assert_eq!(
        types
            .run_memory(b.organization_id, b.sandbox, borrower.id)
            .await
            .unwrap(),
        None,
        "another organization's default is its own"
    );
    delete(stores, &b, theirs.id).await.unwrap();
    assert!(types.find_type(a.organization_id, ours.id).await.unwrap().is_some());
    assert_eq!(events(stores, &b).await.len(), 2, "b's own changes only");
}

/// The forget jobs queued for the organization, each read back as what it
/// forgets and the kind that forgets it, sorted, every forget job claimed
/// finished: the suite's tenants share one queue.
async fn forget_jobs(stores: &Stores<'_>, organization_id: OrganizationId) -> Vec<(String, Forget)> {
    let kinds = [Name::new(FORGET_JOB).unwrap()];
    let mut queued = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(&kinds, std::time::Duration::from_secs(60))
        .await
        .unwrap()
    {
        let (what, kind) = Forget::of_job(&job.payload).expect("a forget job names what it forgets");
        if what.organization_id() == organization_id {
            assert_eq!(
                job.scope,
                Scope::Organization(organization_id),
                "in the organization's scope"
            );
            queued.push((kind.as_str().to_owned(), what));
        }
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    }
    queued.sort_by_key(|(kind, what)| (kind.clone(), format!("{what:?}")));
    queued
}

/// Deleting an agent, a workspace or a type queues with it, in the
/// organization's scope, the job that has each memory kind forget what went
/// ([`FORGET_JOB`]), naming it by its IDs: for an agent or a workspace, one
/// for each kind of the organization's types at that moment; for a type,
/// one for its own kind. A deletion refused queues none.
async fn queues_forgetting_with_each_deletion(stores: &Stores<'_>, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let org = t.organization_id;
    forget_jobs(stores, org).await;
    let delete_agent = |agent: Agent| async move {
        stores
            .agents
            .delete_agent(org, agent.workspace_id, agent.id, &|_| by_suite_in(agent.workspace_id))
            .await
    };
    let forget_agent = |agent: &Agent| Forget::Agent {
        organization_id: org,
        workspace_id: agent.workspace_id,
        agent_id: agent.id,
    };

    let first = agent(stores, &t, t.sandbox, "First", AgentMemory::Off).await.unwrap();
    delete_agent(first.clone()).await.unwrap();
    assert_eq!(
        forget_jobs(stores, org).await,
        [],
        "no kind to forget it while there's no type"
    );

    let notes = create(stores, &t, "Notes", MemoryWrites::Allow).await;
    create(stores, &t, "More notes", MemoryWrites::Allow).await;
    let other_kind = MemoryKind::parse("other_kind").unwrap();
    let elsewhere = create_by(
        stores,
        &t,
        t.admin,
        &NewMemoryType {
            name: "Kept elsewhere".to_owned(),
            kind: other_kind.clone(),
            settings: Settings::default(),
            writes: MemoryWrites::Allow,
        },
        at(2),
    )
    .await
    .unwrap();
    let second = agent(stores, &t, t.sandbox, "Second", AgentMemory::Type(notes.id))
        .await
        .unwrap();
    delete_agent(second.clone()).await.unwrap();
    assert_eq!(
        forget_jobs(stores, org).await,
        [
            ("notes".to_owned(), forget_agent(&second)),
            ("other_kind".to_owned(), forget_agent(&second)),
        ],
        "an agent's: one for each kind of the organization's types"
    );

    // Refused deletions queue nothing.
    assert_eq!(delete_agent(second).await, Err(AgentError::AgentNotFound));
    let holder = agent(stores, &t, t.production, "Holder", AgentMemory::Type(notes.id))
        .await
        .unwrap();
    assert!(matches!(
        delete(stores, &t, notes.id).await,
        Err(MemoryTypeError::InUse(_))
    ));
    assert_eq!(
        delete(stores, &t, MemoryTypeId::generate()).await,
        Err(MemoryTypeError::NotFound)
    );
    let missing = WorkspaceId::generate();
    assert_eq!(
        stores
            .workspaces
            .delete_workspace(org, missing, &|_| by_suite_in(missing))
            .await,
        Err(WorkspaceError::NotFound)
    );
    assert_eq!(forget_jobs(stores, org).await, [], "a refused deletion queues none");

    delete(stores, &t, elsewhere.id).await.unwrap();
    assert_eq!(
        forget_jobs(stores, org).await,
        [(
            "other_kind".to_owned(),
            Forget::Type {
                organization_id: org,
                memory_type_id: elsewhere.id,
            }
        )],
        "a type's: its own kind's"
    );
    stores
        .workspaces
        .delete_workspace(org, holder.workspace_id, &|_| by_suite_in(holder.workspace_id))
        .await
        .unwrap();
    assert_eq!(
        forget_jobs(stores, org).await,
        [(
            "notes".to_owned(),
            Forget::Workspace {
                organization_id: org,
                workspace_id: t.production,
            }
        )],
        "a workspace's: one for each kind left"
    );
}
