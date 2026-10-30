//! The `NoteStore` suite: the `notes` kind's notes, reached only through
//! their full scope (organization, workspace, agent and type), each of its
//! four keys changed alone; saved only in a scope that's there, up to its
//! limit, with a short ID held once in it, saves made at once included;
//! found there by ID, as they are now, whatever their audience;
//! changed and deleted only where their reach goes (a run in a direct
//! conversation changes that conversation's notes only, a run elsewhere the
//! workspace's, and the people who manage them every one), an update keeping
//! who wrote the text, and the text a note holds already changing nothing;
//! recalled by last change, and listed by when they were saved, with counts
//! of one moment; searched by every word, in any case, in an order no other
//! scope's notes move; and every note of an agent, a workspace, a type or an
//! organization deleted, and nothing else.
//!
//! Which note ranks first is the store's own: a suite can't hold another
//! database to SQLite's BM25, so its order is tested where it's kept.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, a workspace with
//! two agents, another workspace with one, and two memory types of the
//! `notes` kind.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::ids::{
    AgentId, ApiKeyId, ChannelConversationId, MemoryId, MemoryTypeId, OrganizationId, RunId, ToolCallId, UserId,
    WorkspaceId,
};
use zupflow_core::installations::ShortId;
use zupflow_core::memory::notes::{NewNote, NoteRef, NoteStore, Reach};
use zupflow_core::memory::{Audience, Forget, Memory, MemoryError, MemoryScope, Origin, Rewritten};
use zupflow_core::time::Timestamp;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    /// Where [`Tenant::agent`] and [`Tenant::other_agent`] work.
    pub workspace: WorkspaceId,
    pub agent: AgentId,
    pub other_agent: AgentId,
    /// Another workspace, with an agent of its own.
    pub sibling: WorkspaceId,
    pub sibling_agent: AgentId,
    /// Two of the organization's memory types, of the `notes` kind.
    pub memory_type: MemoryTypeId,
    pub other_type: MemoryTypeId,
}

impl Tenant {
    /// The first agent's notes, in the first type.
    pub fn scope(&self) -> MemoryScope {
        MemoryScope {
            organization_id: self.organization_id,
            workspace_id: self.workspace,
            agent_id: self.agent,
            memory_type_id: self.memory_type,
        }
    }

    /// [`Tenant::scope`] with one key changed, each of the four alone:
    /// `other`'s organization, the other workspace (where the agent isn't),
    /// the other agent and the other type.
    pub fn changed_alone(&self, other: &Tenant) -> [(&'static str, MemoryScope); 4] {
        let scope = self.scope();
        [
            (
                "organization",
                MemoryScope {
                    organization_id: other.organization_id,
                    ..scope
                },
            ),
            (
                "workspace",
                MemoryScope {
                    workspace_id: self.sibling,
                    ..scope
                },
            ),
            (
                "agent",
                MemoryScope {
                    agent_id: self.other_agent,
                    ..scope
                },
            ),
            (
                "type",
                MemoryScope {
                    memory_type_id: self.other_type,
                    ..scope
                },
            ),
        ]
    }

    /// Scopes beside [`Tenant::scope`] that hold notes of their own: the
    /// other type's, the other agent's and the other workspace's.
    pub fn neighbors(&self) -> [MemoryScope; 3] {
        let scope = self.scope();
        [
            MemoryScope {
                memory_type_id: self.other_type,
                ..scope
            },
            MemoryScope {
                agent_id: self.other_agent,
                ..scope
            },
            MemoryScope {
                workspace_id: self.sibling,
                agent_id: self.sibling_agent,
                ..scope
            },
        ]
    }
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub async fn run(store: &dyn NoteStore, prepare: Prepare) {
    saves_only_in_a_scope_that_is_there(store, &prepare).await;
    finds_a_note_by_its_id_in_its_scope_only(store, &prepare).await;
    gives_each_note_a_short_id_once_in_its_scope(store, &prepare).await;
    keeps_a_scope_to_its_limit(store, &prepare).await;
    keeps_the_limit_and_short_ids_when_saved_at_once(store, &prepare).await;
    changes_only_what_its_reach_goes_to(store, &prepare).await;
    keeps_who_wrote_the_text(store, &prepare).await;
    changes_nothing_with_the_text_a_note_holds(store, &prepare).await;
    recalls_the_notes_last_changed(store, &prepare).await;
    searches_by_every_word(store, &prepare).await;
    ranks_by_the_scopes_own_notes_only(store, &prepare).await;
    lists_every_note_newest_first(store, &prepare).await;
    deletes_all_of_exactly_what_it_names(store, &prepare).await;
}

pub(crate) async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        workspace: WorkspaceId::generate(),
        agent: AgentId::generate(),
        other_agent: AgentId::generate(),
        sibling: WorkspaceId::generate(),
        sibling_agent: AgentId::generate(),
        memory_type: MemoryTypeId::generate(),
        other_type: MemoryTypeId::generate(),
    };
    prepare(tenant).await;
    tenant
}

fn origin() -> Origin {
    run_origin(false)
}

/// A run's tool call; `after_app_content` once it read an app's content.
fn run_origin(after_app_content: bool) -> Origin {
    Origin::Run {
        run_id: RunId::generate(),
        tool_call_id: ToolCallId::generate(),
        after_app_content,
    }
}

fn member() -> Origin {
    Origin::Member {
        user_id: UserId::generate(),
    }
}

/// A short ID of its own for each `n`.
fn short(n: u32) -> ShortId {
    ShortId::from_bits(n)
}

fn at(millis: i64) -> Timestamp {
    Timestamp::from_millis(1_800_000_000_000 + millis)
}

/// Saves `text` in `scope` with `short(n)`, at `millis`.
async fn save(store: &dyn NoteStore, scope: &MemoryScope, n: u32, text: &str, millis: i64) -> Memory {
    save_in(store, scope, None, n, text, millis).await
}

/// Saves `text` in `scope`, in a direct conversation when one is given.
async fn save_in(
    store: &dyn NoteStore,
    scope: &MemoryScope,
    conversation: Option<ChannelConversationId>,
    n: u32,
    text: &str,
    millis: i64,
) -> Memory {
    let origin = origin();
    store
        .insert(
            scope,
            &NewNote {
                id: MemoryId::generate(),
                text,
                channel_conversation_id: conversation,
                origin: &origin,
                short_ids: &[short(n)],
            },
            200,
            at(millis),
        )
        .await
        .unwrap_or_else(|error| panic!("saving {text:?}: {error}"))
}

/// The texts of the scope's notes, newest first, whatever their audience.
async fn texts(store: &dyn NoteStore, scope: &MemoryScope) -> Vec<String> {
    store
        .list(scope, None, None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|note| note.text)
        .collect()
}

fn words(words: &[&str]) -> Vec<String> {
    words.iter().map(|word| (*word).to_owned()).collect()
}

async fn found(store: &dyn NoteStore, scope: &MemoryScope, audience: &Audience, query: &[&str]) -> Vec<String> {
    store
        .search(scope, audience, &words(query), 20)
        .await
        .unwrap()
        .into_iter()
        .map(|note| note.text)
        .collect()
}

async fn finds_a_note_by_its_id_in_its_scope_only(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let note = save(store, &scope, 1, "Prefers short answers.", 1).await;
    let direct = save_in(
        store,
        &scope,
        Some(ChannelConversationId::generate()),
        2,
        "Said in a direct conversation.",
        2,
    )
    .await;
    for kept in [&note, &direct] {
        assert_eq!(
            store.get(&scope, kept.id).await.as_ref(),
            Ok(kept),
            "whatever its audience"
        );
        for (key, elsewhere) in t.changed_alone(&other) {
            assert_eq!(
                store.get(&elsewhere, kept.id).await,
                Err(MemoryError::NotFound),
                "{key}"
            );
        }
    }
    // A neighbor's note is found in its own scope, never through this one.
    for (neighbor, n) in t.neighbors().iter().zip(3..) {
        let theirs = save(store, neighbor, n, "A neighbor's note.", 3).await;
        assert_eq!(store.get(neighbor, theirs.id).await.as_ref(), Ok(&theirs));
        assert_eq!(
            store.get(&scope, theirs.id).await,
            Err(MemoryError::NotFound),
            "{neighbor:?}"
        );
    }
    assert_eq!(
        store.get(&scope, MemoryId::generate()).await,
        Err(MemoryError::NotFound),
        "an ID no note holds"
    );

    // As it is now: changed, then gone.
    let changed = store
        .update(
            &scope,
            NoteRef::Id(note.id),
            Reach::Everything,
            &member(),
            "Prefers shorter answers.",
            at(4),
        )
        .await
        .unwrap();
    assert_eq!(store.get(&scope, note.id).await, Ok(changed.memory));
    store
        .delete(&scope, NoteRef::Id(note.id), Reach::Everything)
        .await
        .unwrap();
    assert_eq!(
        store.get(&scope, note.id).await,
        Err(MemoryError::NotFound),
        "once deleted"
    );
    store
        .delete_all(&Forget::Agent {
            organization_id: t.organization_id,
            workspace_id: t.workspace,
            agent_id: t.agent,
        })
        .await
        .unwrap();
    assert_eq!(
        store.get(&scope, direct.id).await,
        Err(MemoryError::NotFound),
        "nor once its agent's notes are"
    );
}

async fn saves_only_in_a_scope_that_is_there(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let origin = Origin::ApiKey {
        api_key_id: ApiKeyId::generate(),
    };
    let id = MemoryId::generate();
    let saved = store
        .insert(
            &scope,
            &NewNote {
                id,
                text: "Prefers answers in Portuguese.",
                channel_conversation_id: None,
                origin: &origin,
                short_ids: &[short(1), short(2)],
            },
            200,
            at(1),
        )
        .await
        .unwrap();
    assert_eq!(
        saved,
        Memory {
            id,
            organization_id: t.organization_id,
            workspace_id: t.workspace,
            agent_id: t.agent,
            memory_type_id: t.memory_type,
            short_id: short(1),
            text: "Prefers answers in Portuguese.".to_owned(),
            channel_conversation_id: None,
            origin,
            created_at: at(1),
            updated_at: at(1),
        },
        "saved as given, with the first short ID free"
    );
    let conversation = ChannelConversationId::generate();
    let direct = save_in(
        store,
        &scope,
        Some(conversation),
        3,
        "Their project is due in April.",
        2,
    )
    .await;
    assert_eq!(direct.channel_conversation_id, Some(conversation));
    let listed = store.list(&scope, None, None, 100).await.unwrap();
    assert_eq!(listed.items, [direct, saved], "read back as saved");

    // A scope whose agent isn't there, in that workspace or organization, or
    // whose type isn't the organization's, keeps nothing.
    let nowhere = [
        (
            "another organization",
            MemoryScope {
                organization_id: other.organization_id,
                ..scope
            },
        ),
        (
            "another workspace",
            MemoryScope {
                workspace_id: t.sibling,
                ..scope
            },
        ),
        (
            "another organization's agent",
            MemoryScope {
                agent_id: other.agent,
                ..scope
            },
        ),
        (
            "another organization's type",
            MemoryScope {
                memory_type_id: other.memory_type,
                ..scope
            },
        ),
        (
            "an agent that isn't there",
            MemoryScope {
                agent_id: AgentId::generate(),
                ..scope
            },
        ),
        (
            "a type that isn't there",
            MemoryScope {
                memory_type_id: MemoryTypeId::generate(),
                ..scope
            },
        ),
    ];
    for (place, nowhere) in nowhere {
        let refused = store
            .insert(
                &nowhere,
                &NewNote {
                    id: MemoryId::generate(),
                    text: "Saved nowhere.",
                    channel_conversation_id: None,
                    origin: &origin,
                    short_ids: &[short(9)],
                },
                200,
                at(3),
            )
            .await;
        assert_eq!(refused, Err(MemoryError::NotFound), "{place}");
    }
    assert_eq!(texts(store, &scope).await.len(), 2);
    assert_eq!(texts(store, &other.scope()).await, Vec::<String>::new());
}

async fn gives_each_note_a_short_id_once_in_its_scope(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let scope = t.scope();
    let origin = origin();
    let insert = |scope: MemoryScope, short_ids: Vec<ShortId>| async move {
        store
            .insert(
                &scope,
                &NewNote {
                    id: MemoryId::generate(),
                    text: "A note.",
                    channel_conversation_id: None,
                    origin: &origin,
                    short_ids: &short_ids,
                },
                200,
                at(1),
            )
            .await
            .map(|note| note.short_id)
    };
    let (a, b, c) = (short(1), short(2), short(3));
    assert_eq!(insert(scope, vec![a.clone(), b.clone()]).await, Ok(a.clone()));
    assert_eq!(
        insert(scope, vec![a.clone(), b.clone()]).await,
        Ok(b.clone()),
        "the first one free"
    );
    assert_eq!(
        insert(scope, vec![a.clone(), b.clone()]).await,
        Err(MemoryError::Unavailable),
        "none free"
    );
    assert_eq!(insert(scope, vec![c.clone()]).await, Ok(c.clone()));
    for neighbor in t.neighbors() {
        assert_eq!(
            insert(neighbor, vec![a.clone()]).await,
            Ok(a.clone()),
            "once in its own scope only"
        );
    }
    assert_eq!(texts(store, &scope).await.len(), 3);
}

async fn keeps_a_scope_to_its_limit(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let scope = t.scope();
    let origin = origin();
    let insert = |scope: MemoryScope, n: u32, max_notes: u32| async move {
        store
            .insert(
                &scope,
                &NewNote {
                    id: MemoryId::generate(),
                    text: "A note.",
                    channel_conversation_id: None,
                    origin: &origin,
                    short_ids: &[short(n)],
                },
                max_notes,
                at(i64::from(n)),
            )
            .await
            .map(|_| ())
    };
    insert(scope, 1, 3).await.unwrap();
    insert(scope, 2, 3).await.unwrap();
    // A direct conversation's notes count too.
    save_in(store, &scope, Some(ChannelConversationId::generate()), 3, "Direct.", 3).await;
    assert_eq!(insert(scope, 4, 3).await, Err(MemoryError::Full));
    assert_eq!(
        insert(scope, 4, 1).await,
        Err(MemoryError::Full),
        "a limit lowered since"
    );
    insert(scope, 4, 4).await.unwrap();
    for neighbor in t.neighbors() {
        insert(neighbor, 5, 1).await.unwrap();
        assert_eq!(
            insert(neighbor, 6, 1).await,
            Err(MemoryError::Full),
            "each scope its own"
        );
    }
    assert_eq!(texts(store, &scope).await.len(), 4);
}

/// Saves made at once hold what one at a time would: never more notes than
/// the limit, and a short ID once in its scope.
async fn keeps_the_limit_and_short_ids_when_saved_at_once(store: &dyn NoteStore, prepare: &Prepare) {
    const MAX_NOTES: u32 = 4;
    let t = tenant(prepare).await;
    let scope = t.scope();
    let origin = origin();
    let insert = |scope: MemoryScope, n: u32, short_ids: Vec<ShortId>| {
        let origin = &origin;
        async move {
            store
                .insert(
                    &scope,
                    &NewNote {
                        id: MemoryId::generate(),
                        text: "Saved at once.",
                        channel_conversation_id: None,
                        origin,
                        short_ids: &short_ids,
                    },
                    MAX_NOTES,
                    at(i64::from(n)),
                )
                .await
                .map(|note| note.short_id)
        }
    };
    for n in 1..MAX_NOTES {
        save(store, &scope, n, "Saved before.", i64::from(n)).await;
    }
    let (a, b, c, d, e) = tokio::join!(
        insert(scope, 10, vec![short(10)]),
        insert(scope, 11, vec![short(11)]),
        insert(scope, 12, vec![short(12)]),
        insert(scope, 13, vec![short(13)]),
        insert(scope, 14, vec![short(14)]),
    );
    let outcomes = [a, b, c, d, e];
    let saved = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    let full = outcomes
        .iter()
        .filter(|outcome| **outcome == Err(MemoryError::Full))
        .count();
    assert_eq!((saved, full), (1, 4), "one more, then full: {outcomes:?}");
    assert_eq!(
        texts(store, &scope).await.len(),
        MAX_NOTES as usize,
        "exactly the limit"
    );

    // Two saves at once offered the same short ID, and only it: one gets it.
    let [other_type, ..] = t.neighbors();
    let (one, two) = tokio::join!(
        insert(other_type, 20, vec![short(20)]),
        insert(other_type, 21, vec![short(20)]),
    );
    let mut outcomes = [one, two];
    outcomes.sort_by_key(Result::is_err);
    assert_eq!(outcomes, [Ok(short(20)), Err(MemoryError::Unavailable)]);
    assert_eq!(texts(store, &other_type).await.len(), 1, "saved once");
}

async fn changes_only_what_its_reach_goes_to(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let conversation = ChannelConversationId::generate();
    let here = Audience::Direct(conversation);
    let elsewhere = Audience::Direct(ChannelConversationId::generate());
    let writer = origin();
    let note = save(store, &scope, 1, "Ships on Thursdays.", 1).await;
    let direct = save_in(
        store,
        &scope,
        Some(conversation),
        2,
        "Their project is due in April.",
        2,
    )
    .await;

    // A run in the workspace, or in another conversation, doesn't reach a
    // direct conversation's note, by its short ID or by its ID.
    for audience in [Audience::Workspace, elsewhere] {
        for reference in [NoteRef::ShortId(&direct.short_id), NoteRef::Id(direct.id)] {
            assert_eq!(
                store
                    .update(&scope, reference, Reach::Run(&audience), &writer, "Changed.", at(3))
                    .await,
                Err(MemoryError::NotFound),
                "{audience:?}"
            );
            assert_eq!(
                store.delete(&scope, reference, Reach::Run(&audience)).await,
                Err(MemoryError::NotFound),
                "{audience:?}"
            );
        }
    }
    // A run in a direct conversation is shown the workspace's notes, and
    // changes none of them.
    for audience in [here, elsewhere] {
        for reference in [NoteRef::ShortId(&note.short_id), NoteRef::Id(note.id)] {
            assert_eq!(
                store
                    .update(&scope, reference, Reach::Run(&audience), &writer, "Changed.", at(3))
                    .await,
                Err(MemoryError::NotFound),
                "{audience:?}"
            );
            assert_eq!(
                store.delete(&scope, reference, Reach::Run(&audience)).await,
                Err(MemoryError::NotFound),
                "{audience:?}"
            );
        }
        let shown = store.recent(&scope, &audience, 10).await.unwrap();
        assert!(shown.notes.contains(&note), "{audience:?}: shown it");
        assert_eq!(
            found(store, &scope, &audience, &["thursdays"]).await,
            ["Ships on Thursdays."],
            "{audience:?}: and finds it"
        );
    }
    assert_eq!(
        store.list(&scope, None, None, 100).await.unwrap().items,
        [direct.clone(), note.clone()],
        "a refused change changes nothing"
    );

    let changed = store
        .update(
            &scope,
            NoteRef::ShortId(&direct.short_id),
            Reach::Run(&here),
            &writer,
            "Their project is due in May, not April.",
            at(4),
        )
        .await
        .unwrap();
    assert_eq!(
        changed,
        Rewritten {
            memory: Memory {
                text: "Their project is due in May, not April.".to_owned(),
                origin: writer,
                updated_at: at(4),
                ..direct.clone()
            },
            changed: true,
        },
        "its text, who wrote it and when, and nothing else"
    );
    let in_workspace = store
        .update(
            &scope,
            NoteRef::ShortId(&note.short_id),
            Reach::Run(&Audience::Workspace),
            &writer,
            "Ships on Thursdays and Fridays.",
            at(5),
        )
        .await
        .unwrap();
    assert_eq!(
        (in_workspace.memory.text.as_str(), in_workspace.changed),
        ("Ships on Thursdays and Fridays.", true),
        "a run in the workspace changes the workspace's notes"
    );
    // The people who manage notes reach every one.
    let person = member();
    let rewritten = store
        .update(
            &scope,
            NoteRef::Id(direct.id),
            Reach::Everything,
            &person,
            "Their project is due in June.",
            at(6),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            rewritten.memory.text.as_str(),
            rewritten.memory.origin,
            rewritten.memory.updated_at,
            rewritten.changed
        ),
        ("Their project is due in June.", person, at(6), true)
    );

    // No other scope reaches either, with each key changed alone.
    for (key, elsewhere) in t.changed_alone(&other) {
        for reference in [NoteRef::Id(note.id), NoteRef::ShortId(&note.short_id)] {
            assert_eq!(
                store
                    .update(&elsewhere, reference, Reach::Everything, &person, "Changed.", at(7))
                    .await,
                Err(MemoryError::NotFound),
                "{key}"
            );
            assert_eq!(
                store.delete(&elsewhere, reference, Reach::Everything).await,
                Err(MemoryError::NotFound),
                "{key}"
            );
        }
    }
    assert_eq!(
        texts(store, &scope).await,
        ["Their project is due in June.", "Ships on Thursdays and Fridays."],
        "both as they were"
    );

    for audience in [here, elsewhere] {
        assert_eq!(
            store
                .delete(&scope, NoteRef::ShortId(&note.short_id), Reach::Run(&audience))
                .await,
            Err(MemoryError::NotFound),
            "{audience:?}: never forgets the workspace's"
        );
    }
    let deleted = store
        .delete(
            &scope,
            NoteRef::ShortId(&note.short_id),
            Reach::Run(&Audience::Workspace),
        )
        .await
        .unwrap();
    assert_eq!(
        (deleted.id, deleted.text.as_str(), deleted.updated_at),
        (note.id, "Ships on Thursdays and Fridays.", at(5)),
        "the note as it was"
    );
    assert_eq!(
        store.delete(&scope, NoteRef::Id(note.id), Reach::Everything).await,
        Err(MemoryError::NotFound),
        "once"
    );
    assert_eq!(
        store
            .update(&scope, NoteRef::Id(note.id), Reach::Everything, &person, "Back.", at(8))
            .await,
        Err(MemoryError::NotFound)
    );
    assert!(
        found(store, &scope, &here, &["thursdays"]).await.is_empty(),
        "nor found"
    );
    store
        .delete(&scope, NoteRef::ShortId(&direct.short_id), Reach::Run(&here))
        .await
        .unwrap();
    assert_eq!(texts(store, &scope).await, Vec::<String>::new());
}

/// A note's origin is who wrote its current text: a run's rewrite after it
/// read an app's content marks the note, and a person's later rewrite
/// clears the mark.
async fn keeps_who_wrote_the_text(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let scope = t.scope();
    let note = save(store, &scope, 1, "Ships on Thursdays.", 1).await;
    let after_app = run_origin(true);
    let marked = store
        .update(
            &scope,
            NoteRef::ShortId(&note.short_id),
            Reach::Run(&Audience::Workspace),
            &after_app,
            "Ships on Fridays.",
            at(2),
        )
        .await
        .unwrap();
    assert_eq!(marked.memory.origin, after_app, "marked");
    assert_eq!(
        store.list(&scope, None, None, 10).await.unwrap().items,
        std::slice::from_ref(&marked.memory),
        "and kept so"
    );
    let person = member();
    let cleared = store
        .update(
            &scope,
            NoteRef::Id(note.id),
            Reach::Everything,
            &person,
            "Ships on Fridays, from May.",
            at(3),
        )
        .await
        .unwrap();
    assert_eq!(cleared.memory.origin, person, "cleared");
    let recent = store.recent(&scope, &Audience::Workspace, 10).await.unwrap();
    assert_eq!(recent.notes, std::slice::from_ref(&cleared.memory), "and kept so");
    let api_key = Origin::ApiKey {
        api_key_id: ApiKeyId::generate(),
    };
    let by_key = store
        .update(
            &scope,
            NoteRef::Id(note.id),
            Reach::Everything,
            &api_key,
            "Ships on Fridays, from June.",
            at(4),
        )
        .await
        .unwrap();
    assert_eq!(
        (by_key.memory.origin, by_key.memory.created_at),
        (api_key, at(1)),
        "whoever wrote it last, whenever it was saved"
    );
}

/// The text a note holds already changes nothing: not when it last changed,
/// nor who wrote it, so its recall stays the same bytes. Where the update
/// doesn't reach, it's refused all the same.
async fn changes_nothing_with_the_text_a_note_holds(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let scope = t.scope();
    let conversation = ChannelConversationId::generate();
    let here = Audience::Direct(conversation);
    let first = save(store, &scope, 1, "Ships on Thursdays.", 1).await;
    let second = save(store, &scope, 2, "Prefers short answers.", 2).await;
    let direct = save_in(store, &scope, Some(conversation), 3, "Due in April.", 3).await;
    let recent = || async move {
        let recent = store.recent(&scope, &here, 10).await.unwrap();
        recent.notes.into_iter().map(|note| note.id).collect::<Vec<_>>()
    };
    let before = recent().await;
    assert_eq!(before, [direct.id, second.id, first.id]);
    for (reference, reach) in [
        (NoteRef::Id(first.id), Reach::Everything),
        (NoteRef::ShortId(&first.short_id), Reach::Run(&Audience::Workspace)),
    ] {
        let unchanged = store
            .update(&scope, reference, reach, &member(), "Ships on Thursdays.", at(5))
            .await
            .unwrap();
        assert_eq!(
            unchanged,
            Rewritten {
                memory: first.clone(),
                changed: false,
            },
            "the note as it is, changed in nothing"
        );
    }
    let unchanged = store
        .update(
            &scope,
            NoteRef::ShortId(&direct.short_id),
            Reach::Run(&here),
            &run_origin(true),
            "Due in April.",
            at(6),
        )
        .await
        .unwrap();
    assert_eq!(
        unchanged,
        Rewritten {
            memory: direct.clone(),
            changed: false,
        }
    );
    assert_eq!(recent().await, before, "no note moved");
    assert_eq!(
        store.list(&scope, None, None, 10).await.unwrap().items,
        [direct.clone(), second, first.clone()],
        "each as it was"
    );
    // Its own text, where the update doesn't reach: refused, as any text is.
    let elsewhere = Audience::Direct(ChannelConversationId::generate());
    for (reference, audience, text) in [
        (NoteRef::Id(direct.id), Audience::Workspace, "Due in April."),
        (NoteRef::ShortId(&direct.short_id), elsewhere, "Due in April."),
        (NoteRef::ShortId(&first.short_id), here, "Ships on Thursdays."),
    ] {
        assert_eq!(
            store
                .update(&scope, reference, Reach::Run(&audience), &origin(), text, at(7))
                .await,
            Err(MemoryError::NotFound),
            "{audience:?}"
        );
    }
}

async fn recalls_the_notes_last_changed(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let conversation = ChannelConversationId::generate();
    let here = Audience::Direct(conversation);
    let first = save(store, &scope, 1, "First.", 1).await;
    let second = save(store, &scope, 2, "Second.", 2).await;
    let third = save(store, &scope, 3, "Third.", 3).await;
    save_in(store, &scope, Some(conversation), 4, "Direct.", 4).await;
    for neighbor in t.neighbors() {
        save(store, &neighbor, 5, "Elsewhere.", 5).await;
    }
    let recent = |audience: Audience, limit: u32| async move {
        let recent = store.recent(&scope, &audience, limit).await.unwrap();
        let texts: Vec<String> = recent.notes.into_iter().map(|note| note.text).collect();
        (texts, recent.total)
    };
    assert_eq!(
        recent(Audience::Workspace, 10).await,
        (vec!["Third.".to_owned(), "Second.".to_owned(), "First.".to_owned()], 3),
        "newest first, the workspace's only"
    );
    assert_eq!(
        recent(here, 10).await,
        (
            vec![
                "Direct.".to_owned(),
                "Third.".to_owned(),
                "Second.".to_owned(),
                "First.".to_owned()
            ],
            4
        ),
        "and the conversation's"
    );
    assert_eq!(
        recent(Audience::Direct(ChannelConversationId::generate()), 10).await.1,
        3,
        "never another conversation's"
    );
    assert_eq!(
        recent(here, 2).await,
        (vec!["Direct.".to_owned(), "Third.".to_owned()], 4),
        "a limit, and how many in all"
    );
    store
        .update(
            &scope,
            NoteRef::Id(first.id),
            Reach::Everything,
            &origin(),
            "First, changed.",
            at(6),
        )
        .await
        .unwrap();
    assert_eq!(
        recent(Audience::Workspace, 1).await,
        (vec!["First, changed.".to_owned()], 3),
        "by last change"
    );
    // Changed at the same moment: by ID, the newest first.
    for note in [&second, &third] {
        let text = note.text.replace('.', ", changed.");
        store
            .update(&scope, NoteRef::Id(note.id), Reach::Everything, &origin(), &text, at(7))
            .await
            .unwrap();
    }
    let ids: Vec<MemoryId> = store
        .recent(&scope, &Audience::Workspace, 2)
        .await
        .unwrap()
        .notes
        .into_iter()
        .map(|note| note.id)
        .collect();
    let mut expected = [second.id, third.id];
    expected.sort();
    expected.reverse();
    assert_eq!(ids, expected);

    for (key, elsewhere) in t.changed_alone(&other) {
        let recent = store.recent(&elsewhere, &here, 10).await.unwrap();
        assert!(
            recent.notes.iter().all(|note| note.scope() == elsewhere),
            "{key}: only its own notes"
        );
        assert!(recent.notes.iter().all(|note| note.text == "Elsewhere."), "{key}");
    }
}

async fn searches_by_every_word(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let conversation = ChannelConversationId::generate();
    let here = Audience::Direct(conversation);
    let workspace = Audience::Workspace;
    save(store, &scope, 1, "Prefers answers in Portuguese.", 1).await;
    save(store, &scope, 2, "Invoices go to the finance team.", 2).await;
    save(store, &scope, 3, "The finance team meets on Mondays.", 3).await;
    save(store, &scope, 4, "Café com leite, não chá.", 4).await;
    save(store, &scope, 5, "Call or email, never text.", 5).await;
    save_in(store, &scope, Some(conversation), 6, "Finance prefers email.", 6).await;

    assert_eq!(
        found(store, &scope, &workspace, &["FINANCE", "mondays"]).await,
        ["The finance team meets on Mondays."],
        "every word, in any case"
    );
    assert_eq!(
        found(store, &scope, &workspace, &["finance"]).await.len(),
        2,
        "never a direct conversation's"
    );
    assert_eq!(found(store, &scope, &here, &["finance"]).await.len(), 3, "but there");
    assert!(
        found(store, &scope, &workspace, &["finance", "portuguese"])
            .await
            .is_empty(),
        "every word, not any"
    );
    assert_eq!(
        found(store, &scope, &workspace, &["cafe", "NAO"]).await,
        ["Café com leite, não chá."],
        "and any accent"
    );
    assert_eq!(
        found(store, &scope, &workspace, &["OR"]).await,
        ["Call or email, never text."],
        "a word, never an operator"
    );
    assert!(found(store, &scope, &workspace, &["NOT", "finance"]).await.is_empty());
    assert_eq!(
        found(store, &scope, &workspace, &["\"finance\"", "mondays*"]).await,
        ["The finance team meets on Mondays."],
        "quotes and stars are no syntax"
    );
    assert!(
        found(store, &scope, &workspace, &["fin"]).await.is_empty(),
        "whole words"
    );
    assert_eq!(
        store
            .search(&scope, &here, &words(&["finance"]), 1)
            .await
            .unwrap()
            .len(),
        1,
        "up to the limit"
    );
    assert!(
        store.search(&scope, &here, &[], 20).await.unwrap().is_empty(),
        "no words, no notes"
    );

    for neighbor in t.neighbors() {
        save(store, &neighbor, 7, "Finance moved to Fridays.", 7).await;
    }
    save(store, &other.scope(), 7, "Finance moved to Fridays.", 7).await;
    assert!(
        found(store, &scope, &here, &["fridays"]).await.is_empty(),
        "nor another scope's"
    );
    for (key, elsewhere) in t.changed_alone(&other) {
        let found = found(store, &elsewhere, &here, &["finance"]).await;
        assert!(
            found.iter().all(|text| text == "Finance moved to Fridays."),
            "{key}: {found:?}"
        );
    }
}

/// A search finds the notes holding every word, in an order its scope's own
/// notes alone decide: a word every other scope holds many times over moves
/// nothing here. A store ranking by an index every scope shares would rank
/// these two the other way once the others have their notes.
async fn ranks_by_the_scopes_own_notes_only(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let workspace = Audience::Workspace;
    save(store, &scope, 1, "alpha alpha beta", 1).await;
    save(store, &scope, 2, "alpha beta beta", 2).await;
    for (n, filler) in ["gamma one", "gamma two", "gamma three", "gamma four"]
        .iter()
        .enumerate()
    {
        save(store, &scope, 10 + n as u32, filler, 3).await;
    }
    let ranked = found(store, &scope, &workspace, &["alpha", "beta"]).await;
    let mut matched = ranked.clone();
    matched.sort();
    assert_eq!(
        matched,
        ["alpha alpha beta", "alpha beta beta"],
        "the notes holding both"
    );
    assert_eq!(
        found(store, &scope, &workspace, &["alpha", "beta"]).await,
        ranked,
        "in one order"
    );
    let elsewhere: Vec<MemoryScope> = t.neighbors().into_iter().chain([other.scope()]).collect();
    for (index, neighbor) in elsewhere.iter().enumerate() {
        for n in 0..8 {
            save(store, neighbor, 100 + n, "beta", 4 + index as i64).await;
        }
    }
    assert_eq!(
        found(store, &scope, &workspace, &["alpha", "beta"]).await,
        ranked,
        "whatever other scopes hold"
    );
}

async fn lists_every_note_newest_first(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let scope = t.scope();
    let first = save(store, &scope, 1, "The finance team meets on Mondays.", 1).await;
    save(store, &scope, 2, "Prefers answers in Portuguese.", 2).await;
    save_in(
        store,
        &scope,
        Some(ChannelConversationId::generate()),
        3,
        "Finance prefers email.",
        3,
    )
    .await;
    save(store, &scope, 4, "Ships on Thursdays.", 4).await;
    save(store, &scope, 5, "Invoices go to finance.", 5).await;
    for neighbor in t.neighbors() {
        save(store, &neighbor, 6, "Finance, elsewhere.", 6).await;
    }
    // A change doesn't move a note in the list.
    store
        .update(
            &scope,
            NoteRef::Id(first.id),
            Reach::Everything,
            &origin(),
            "The finance team meets on Mondays at ten.",
            at(9),
        )
        .await
        .unwrap();

    let page = store.list(&scope, None, None, 2).await.unwrap();
    let names = |page: &zupflow_core::workspaces::Page<Memory>| -> Vec<String> {
        page.items.iter().map(|note| note.text.clone()).collect()
    };
    assert_eq!(
        (names(&page), page.total),
        (
            vec!["Invoices go to finance.".to_owned(), "Ships on Thursdays.".to_owned()],
            5
        ),
        "newest first, every audience's"
    );
    let cursor = page.next_cursor.clone().unwrap();
    let page = store.list(&scope, None, Some(&cursor), 2).await.unwrap();
    assert_eq!(
        names(&page),
        ["Finance prefers email.", "Prefers answers in Portuguese."]
    );
    let cursor = page.next_cursor.clone().unwrap();
    let page = store.list(&scope, None, Some(&cursor), 2).await.unwrap();
    assert_eq!(
        (names(&page), page.next_cursor),
        (vec!["The finance team meets on Mondays at ten.".to_owned()], None)
    );

    let finance = words(&["FINANCE"]);
    let page = store.list(&scope, Some(&finance), None, 2).await.unwrap();
    assert_eq!(
        (names(&page), page.total),
        (
            vec![
                "Invoices go to finance.".to_owned(),
                "Finance prefers email.".to_owned()
            ],
            3
        ),
        "with words, the notes holding them"
    );
    let page = store
        .list(&scope, Some(&finance), page.next_cursor.as_deref(), 2)
        .await
        .unwrap();
    assert_eq!(
        (names(&page), page.next_cursor),
        (vec!["The finance team meets on Mondays at ten.".to_owned()], None)
    );
    let page = store.list(&scope, Some(&[]), None, 25).await.unwrap();
    assert_eq!((page.items.len(), page.total), (0, 0), "no words, no notes");
    assert_eq!(
        store.list(&scope, None, Some("not-a-cursor"), 25).await,
        Err(MemoryError::InvalidCursor)
    );

    for (key, elsewhere) in t.changed_alone(&other) {
        let page = store.list(&elsewhere, None, None, 100).await.unwrap();
        assert!(
            page.items.iter().all(|note| note.text == "Finance, elsewhere."),
            "{key}"
        );
        let page = store.list(&elsewhere, Some(&finance), None, 100).await.unwrap();
        assert!(
            page.items.iter().all(|note| note.text == "Finance, elsewhere."),
            "{key}"
        );
        assert_eq!(page.total, page.items.len() as u64, "{key}");
    }
}

async fn deletes_all_of_exactly_what_it_names(store: &dyn NoteStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let org = t.organization_id;
    let scope = t.scope();
    let [other_type, other_agent, sibling] = t.neighbors();
    save(store, &scope, 1, "Base.", 1).await;
    save_in(
        store,
        &scope,
        Some(ChannelConversationId::generate()),
        2,
        "Base, direct.",
        2,
    )
    .await;
    save(store, &other_type, 3, "Other type.", 3).await;
    save(store, &other_agent, 4, "Other agent.", 4).await;
    save(store, &sibling, 5, "Sibling.", 5).await;
    save(store, &other.scope(), 6, "Other organization.", 6).await;
    let everywhere = [scope, other_type, other_agent, sibling, other.scope()];
    let counts = || async move {
        let mut counts = Vec::new();
        for scope in everywhere {
            counts.push(texts(store, &scope).await.len());
        }
        counts
    };

    // Another organization's IDs reach nothing of this one.
    for what in [
        Forget::Agent {
            organization_id: other.organization_id,
            workspace_id: t.workspace,
            agent_id: t.agent,
        },
        Forget::Workspace {
            organization_id: other.organization_id,
            workspace_id: t.workspace,
        },
        Forget::Type {
            organization_id: other.organization_id,
            memory_type_id: t.memory_type,
        },
        Forget::Agent {
            organization_id: org,
            workspace_id: t.sibling,
            agent_id: t.agent,
        },
    ] {
        assert_eq!(store.delete_all(&what).await, Ok(0), "{what:?}");
    }
    assert_eq!(counts().await, [2, 1, 1, 1, 1]);

    let agent = Forget::Agent {
        organization_id: org,
        workspace_id: t.workspace,
        agent_id: t.agent,
    };
    assert_eq!(store.delete_all(&agent).await, Ok(3), "the agent's, in every type");
    assert_eq!(counts().await, [0, 0, 1, 1, 1]);

    save(store, &scope, 1, "Base.", 7).await;
    save(store, &other_type, 3, "Other type.", 8).await;
    let memory_type = Forget::Type {
        organization_id: org,
        memory_type_id: t.other_type,
    };
    assert_eq!(store.delete_all(&memory_type).await, Ok(1));
    assert_eq!(counts().await, [1, 0, 1, 1, 1]);

    let workspace = Forget::Workspace {
        organization_id: org,
        workspace_id: t.sibling,
    };
    assert_eq!(store.delete_all(&workspace).await, Ok(1));
    assert_eq!(counts().await, [1, 0, 1, 0, 1]);

    let organization = Forget::Organization { organization_id: org };
    assert_eq!(store.delete_all(&organization).await, Ok(2));
    assert_eq!(counts().await, [0, 0, 0, 0, 1], "never another organization's");
    assert_eq!(store.delete_all(&organization).await, Ok(0));
    for scope in [scope, sibling] {
        assert!(
            store
                .search(&scope, &Audience::Workspace, &words(&["base"]), 20)
                .await
                .unwrap()
                .is_empty()
        );
    }
}
