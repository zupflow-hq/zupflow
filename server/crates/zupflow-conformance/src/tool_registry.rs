//! The `ToolRegistry` suite: tools kept per installation, reached only
//! through their own organization and workspace; the first definition
//! pinned, a changed one held beside it unreviewed, a missing one gone; a
//! listing that breaks the limits recorded not at all.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, two workspaces, and
//! an installation of one app in each.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{
    ListedTool, ListingChanges, MAX_TOOLS, RegisteredTool, ToolError, ToolRegistry, ToolRevision, ToolStatus,
};

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    pub workspace: WorkspaceId,
    pub installation: InstallationId,
    /// Another workspace of the organization, with its own installation of the same app.
    pub sibling: WorkspaceId,
    pub sibling_installation: InstallationId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        workspace: WorkspaceId::generate(),
        installation: InstallationId::generate(),
        sibling: WorkspaceId::generate(),
        sibling_installation: InstallationId::generate(),
    };
    prepare(tenant).await;
    tenant
}

/// An installation's full scope.
#[derive(Debug, Clone, Copy)]
struct At(OrganizationId, WorkspaceId, InstallationId);

impl Tenant {
    fn at(&self) -> At {
        At(self.organization_id, self.workspace, self.installation)
    }
    fn sibling_at(&self) -> At {
        At(self.organization_id, self.sibling, self.sibling_installation)
    }
}

async fn record(
    registry: &dyn ToolRegistry,
    at: At,
    listed: &[ListedTool],
    now: Timestamp,
) -> Result<ListingChanges, ToolError> {
    registry.record_listing(at.0, at.1, at.2, listed, now).await
}

async fn list(registry: &dyn ToolRegistry, at: At) -> Result<Vec<RegisteredTool>, ToolError> {
    registry.list_tools(at.0, at.1, at.2).await
}

/// A definition the registry treats as opaque: kept and returned exactly.
fn revision(name: &str, version: u8) -> ToolRevision {
    ToolRevision {
        definition: format!(r#"{{"description":"v{version}","name":"{name}"}}"#),
        hash: [version; 32],
    }
}

fn listed(name: &str, version: u8, effect: ToolEffect) -> ListedTool {
    ListedTool {
        name: name.to_owned(),
        revision: revision(name, version),
        effect,
    }
}

fn names(tools: &[RegisteredTool]) -> Vec<(&str, ToolStatus)> {
    tools.iter().map(|tool| (tool.name.as_str(), tool.status())).collect()
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

pub async fn run(registry: &dyn ToolRegistry, prepare: Prepare) {
    keeps_tools_per_installation(registry, &prepare).await;
    pins_the_first_definition_and_holds_changes(registry, &prepare).await;
    marks_missing_tools_gone_until_listed_again(registry, &prepare).await;
    records_an_invalid_listing_not_at_all(registry, &prepare).await;
}

async fn keeps_tools_per_installation(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let changes = record(
        registry,
        a.at(),
        &[
            listed("search", 1, ToolEffect::Read),
            listed("create_issue", 1, ToolEffect::Write),
        ],
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        changes,
        ListingChanges {
            added: strings(&["create_issue", "search"]),
            ..ListingChanges::default()
        }
    );
    let tools = list(registry, a.at()).await.unwrap();
    assert_eq!(
        names(&tools),
        [("create_issue", ToolStatus::Current), ("search", ToolStatus::Current)],
        "by name"
    );
    let search = &tools[1];
    assert_eq!(
        (
            search.effect,
            &search.pinned,
            &search.latest,
            search.first_seen_at,
            search.seen_at
        ),
        (ToolEffect::Read, &revision("search", 1), &None, now, now),
        "kept exactly"
    );

    // The sibling workspace's installation of the same app has its own.
    assert_eq!(list(registry, a.sibling_at()).await.unwrap(), []);
    record(registry, a.sibling_at(), &[listed("search", 7, ToolEffect::Read)], now)
        .await
        .unwrap();
    assert_eq!(
        list(registry, a.at()).await.unwrap()[1].pinned,
        revision("search", 1),
        "one installation's listing is never another's"
    );
    assert_eq!(
        list(registry, a.sibling_at()).await.unwrap()[0].pinned,
        revision("search", 7)
    );

    // An installation out of scope is not found, for reading and for recording.
    for at in [
        At(b.organization_id, a.workspace, a.installation),
        At(b.organization_id, b.workspace, a.installation),
        At(a.organization_id, a.sibling, a.installation),
        At(a.organization_id, a.workspace, a.sibling_installation),
        At(a.organization_id, a.workspace, InstallationId::generate()),
    ] {
        assert_eq!(list(registry, at).await, Err(ToolError::NotFound), "{at:?}");
        assert_eq!(
            record(registry, at, &[listed("search", 9, ToolEffect::Read)], now).await,
            Err(ToolError::NotFound),
            "{at:?}"
        );
    }
    assert_eq!(
        list(registry, a.at()).await.unwrap()[1].pinned,
        revision("search", 1),
        "nothing recorded from another scope"
    );
    assert_eq!(list(registry, a.sibling_at()).await.unwrap().len(), 1);
}

async fn pins_the_first_definition_and_holds_changes(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let owner = a.at();
    let first = Timestamp::from_millis(1_000_000);
    let later = Timestamp::from_millis(2_000_000);
    let latest = Timestamp::from_millis(3_000_000);
    record(registry, owner, &[listed("send", 1, ToolEffect::Write)], first)
        .await
        .unwrap();

    // A changed definition is held beside the pinned one, unreviewed.
    let changes = record(registry, owner, &[listed("send", 2, ToolEffect::Destructive)], later)
        .await
        .unwrap();
    assert_eq!(
        changes,
        ListingChanges {
            changed: strings(&["send"]),
            ..ListingChanges::default()
        }
    );
    let send = list(registry, owner).await.unwrap().remove(0);
    assert_eq!(send.status(), ToolStatus::Changed);
    assert_eq!(
        (&send.pinned, &send.latest, send.changed_at, send.seen_at),
        (&revision("send", 1), &Some(revision("send", 2)), Some(later), later),
        "the pinned definition stays in force"
    );
    assert_eq!(
        send.effect,
        ToolEffect::Write,
        "a listing never changes a known tool's effect"
    );

    // Listed the same again: nothing new. Changed again: the latest moves on.
    assert!(
        record(registry, owner, &[listed("send", 2, ToolEffect::Write)], latest)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        list(registry, owner).await.unwrap()[0].changed_at,
        Some(later),
        "first listed then"
    );
    let changes = record(registry, owner, &[listed("send", 3, ToolEffect::Write)], latest)
        .await
        .unwrap();
    assert_eq!(changes.changed, ["send"]);
    let send = list(registry, owner).await.unwrap().remove(0);
    assert_eq!(
        (&send.pinned, &send.latest, send.changed_at),
        (&revision("send", 1), &Some(revision("send", 3)), Some(latest))
    );

    // Listed as pinned again: current.
    record(registry, owner, &[listed("send", 1, ToolEffect::Write)], latest)
        .await
        .unwrap();
    let send = list(registry, owner).await.unwrap().remove(0);
    assert_eq!(
        (send.status(), &send.latest, send.changed_at),
        (ToolStatus::Current, &None, None)
    );
    assert_eq!(send.first_seen_at, first);
}

async fn marks_missing_tools_gone_until_listed_again(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let owner = a.at();
    let first = Timestamp::from_millis(1_000_000);
    let later = Timestamp::from_millis(2_000_000);
    let latest = Timestamp::from_millis(3_000_000);
    record(
        registry,
        owner,
        &[listed("a", 1, ToolEffect::Read), listed("b", 1, ToolEffect::Read)],
        first,
    )
    .await
    .unwrap();
    let changes = record(registry, owner, &[listed("a", 1, ToolEffect::Read)], later)
        .await
        .unwrap();
    assert_eq!(
        changes,
        ListingChanges {
            gone: strings(&["b"]),
            ..ListingChanges::default()
        }
    );
    let tools = list(registry, owner).await.unwrap();
    assert_eq!(names(&tools), [("a", ToolStatus::Current), ("b", ToolStatus::Gone)]);
    assert_eq!((tools[1].gone_at, tools[1].seen_at), (Some(later), first));

    // Still missing: nothing new. An empty listing marks everything gone.
    assert!(
        record(registry, owner, &[listed("a", 1, ToolEffect::Read)], latest)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        list(registry, owner).await.unwrap()[1].gone_at,
        Some(later),
        "gone since then"
    );

    // Back, with another definition: changed, against the pinned one.
    let changes = record(
        registry,
        owner,
        &[listed("a", 1, ToolEffect::Read), listed("b", 2, ToolEffect::Read)],
        latest,
    )
    .await
    .unwrap();
    assert_eq!(
        changes,
        ListingChanges {
            changed: strings(&["b"]),
            returned: strings(&["b"]),
            ..ListingChanges::default()
        }
    );
    let tools = list(registry, owner).await.unwrap();
    assert_eq!(names(&tools), [("a", ToolStatus::Current), ("b", ToolStatus::Changed)]);
    assert_eq!(tools[1].pinned, revision("b", 1));

    let changes = record(registry, owner, &[], latest).await.unwrap();
    assert_eq!(changes.gone, ["a", "b"]);
}

async fn records_an_invalid_listing_not_at_all(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let owner = a.at();
    let now = Timestamp::now();
    record(registry, owner, &[listed("kept", 1, ToolEffect::Read)], now)
        .await
        .unwrap();
    let too_many: Vec<ListedTool> = (0..=MAX_TOOLS)
        .map(|index| listed(&format!("tool_{index}"), 1, ToolEffect::Read))
        .collect();
    let oversized = ListedTool {
        name: "big".to_owned(),
        revision: ToolRevision {
            definition: format!(r#"{{"description":"{}"}}"#, "x".repeat(64 << 10)),
            hash: [1; 32],
        },
        effect: ToolEffect::Read,
    };
    for invalid in [
        too_many,
        vec![
            listed("twice", 1, ToolEffect::Read),
            listed("twice", 2, ToolEffect::Read),
        ],
        vec![listed("", 1, ToolEffect::Read)],
        vec![listed(&"n".repeat(129), 1, ToolEffect::Read)],
        vec![oversized],
    ] {
        assert_eq!(
            record(registry, owner, &invalid, now).await,
            Err(ToolError::InvalidListing)
        );
    }
    assert_eq!(
        names(&list(registry, owner).await.unwrap()),
        [("kept", ToolStatus::Current)],
        "an invalid listing marks nothing gone"
    );
}
