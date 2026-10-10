//! The `ToolRegistry` suite: tools kept per installation, reached only
//! through their own organization and workspace; the first definition
//! pinned, a changed one held beside it unreviewed, a missing one gone; a
//! listing that breaks the limits recorded not at all; read a page at a
//! time, by name; gone tools kept up to a bound; a change accepted only as
//! reviewed, and only in its own installation; a listing that changes a
//! tool announcing `app.tools_changed` with it, in the installation's
//! workspace, and no other listing announcing anything.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, two workspaces, and
//! an installation of one app in each.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use std::time::Duration;
use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{InstallationId, OrganizationId, WorkspaceId};

use zupflow_core::jobs::JobQueue;
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;
use zupflow_core::tools::{
    ListedTool, ListingChanges, MAX_GONE, MAX_PAGE, MAX_TOOLS, RegisteredTool, ToolError, ToolRegistry, ToolRevision,
    ToolStatus,
};
use zupflow_core::webhooks::{ANNOUNCE, Announcement, Subject, WebhookEventType};

use crate::audit_store::by_suite_in;

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

/// Every tool of the installation, read two at a time, so every case pages.
async fn list(registry: &dyn ToolRegistry, at: At) -> Result<Vec<RegisteredTool>, ToolError> {
    let mut tools = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = registry.list_tools(at.0, at.1, at.2, after.as_deref(), 2).await?;
        assert!(page.items.len() <= 2, "a page holds at most its limit");
        tools.extend(page.items);
        match page.next_after {
            Some(next) => after = Some(next),
            None => {
                assert_eq!(
                    usize::try_from(page.total).ok(),
                    Some(tools.len()),
                    "the total counts every tool"
                );
                return Ok(tools);
            }
        }
    }
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

/// `jobs` is the queue the registry queues its announcements in.
pub async fn run(registry: &dyn ToolRegistry, jobs: &dyn JobQueue, prepare: Prepare) {
    announces_changed_tools(registry, jobs, &prepare).await;
    keeps_tools_per_installation(registry, &prepare).await;
    pins_the_first_definition_and_holds_changes(registry, &prepare).await;
    marks_missing_tools_gone_until_listed_again(registry, &prepare).await;
    records_an_invalid_listing_not_at_all(registry, &prepare).await;
    pages_by_name(registry, &prepare).await;
    keeps_a_bounded_number_of_gone_tools(registry, &prepare).await;
    accepts_exactly_the_change_reviewed(registry, &prepare).await;
}

async fn pages_by_name(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let at = a.at();
    let now = Timestamp::now();
    // Byte order: upper case before lower case, and "b" before "b_".
    record(
        registry,
        at,
        &["c", "a", "b_", "B", "b"].map(|name| listed(name, 1, ToolEffect::Read)),
        now,
    )
    .await
    .unwrap();
    let page = |after: Option<&'static str>, limit: u32| async move {
        let page = registry.list_tools(at.0, at.1, at.2, after, limit).await.unwrap();
        let names: Vec<String> = page.items.iter().map(|tool| tool.name.clone()).collect();
        (names, page.next_after, page.total)
    };
    assert_eq!(page(None, 2).await, (strings(&["B", "a"]), Some("a".to_owned()), 5));
    assert_eq!(
        page(Some("a"), 2).await,
        (strings(&["b", "b_"]), Some("b_".to_owned()), 5)
    );
    assert_eq!(page(Some("b_"), 2).await, (strings(&["c"]), None, 5));
    // A name that isn't there still marks a place; the last page has no next.
    assert_eq!(page(Some("b0"), 10).await, (strings(&["b_", "c"]), None, 5));
    assert_eq!(page(Some("z"), 10).await, (Vec::new(), None, 5));
    // A limit is clamped: 0 reads one, a huge one at most the largest page.
    assert_eq!(page(None, 0).await.0, strings(&["B"]));
    assert_eq!(page(None, u32::MAX).await.0.len(), 5);
    let many: Vec<ListedTool> = (0..150)
        .map(|index| listed(&format!("t{index:03}"), 1, ToolEffect::Read))
        .collect();
    record(registry, at, &many, now).await.unwrap();
    let full = registry.list_tools(at.0, at.1, at.2, None, u32::MAX).await.unwrap();
    assert_eq!(full.items.len(), MAX_PAGE as usize);
    assert_eq!(full.total, 155, "gone tools count too");
    // Another scope's page is not found.
    assert_eq!(
        registry
            .list_tools(a.organization_id, a.sibling, a.installation, None, 10)
            .await,
        Err(ToolError::NotFound)
    );
}

async fn keeps_a_bounded_number_of_gone_tools(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let at = a.at();
    let batch = |prefix: &str| -> Vec<ListedTool> {
        (0..MAX_TOOLS)
            .map(|index| listed(&format!("{prefix}{index:04}"), 1, ToolEffect::Read))
            .collect()
    };
    let (first, second, third) = (
        Timestamp::from_millis(1_000_000),
        Timestamp::from_millis(2_000_000),
        Timestamp::from_millis(3_000_000),
    );
    record(registry, at, &batch("a"), first).await.unwrap();
    // Every `a` gone at `second`: exactly the most kept.
    record(registry, at, &batch("b"), second).await.unwrap();
    let tools = list(registry, at).await.unwrap();
    assert_eq!(tools.len(), 2 * MAX_TOOLS);
    // Every `b` gone at `third` too: the `a`s, gone longest, are deleted.
    record(registry, at, &[listed("c", 1, ToolEffect::Read)], third)
        .await
        .unwrap();
    let tools = list(registry, at).await.unwrap();
    let gone: Vec<&RegisteredTool> = tools.iter().filter(|tool| tool.gone_at.is_some()).collect();
    assert_eq!(gone.len(), MAX_GONE);
    assert!(
        gone.iter()
            .all(|tool| tool.name.starts_with('b') && tool.gone_at == Some(third)),
        "the most recently gone are kept"
    );
    assert_eq!(tools.len(), MAX_GONE + 1, "the listed tool stays");
    // A tool deleted while gone is new when it's listed again.
    let changes = record(registry, at, &[listed("a0000", 2, ToolEffect::Write)], third)
        .await
        .unwrap();
    assert_eq!(changes.added, ["a0000"]);
    // The sibling's tools are untouched by another installation's bound.
    assert_eq!(list(registry, a.sibling_at()).await.unwrap(), []);
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

async fn accepts_exactly_the_change_reviewed(registry: &dyn ToolRegistry, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let earlier = Timestamp::from_millis(Timestamp::now().millis() - 60_000);
    let first = [
        listed("current", 1, ToolEffect::Read),
        listed("changed", 1, ToolEffect::Read),
        listed("gone", 1, ToolEffect::Read),
    ];
    // The change lists `changed` as destructive: the effect stays as first seen.
    let second = [
        listed("current", 1, ToolEffect::Read),
        listed("changed", 2, ToolEffect::Destructive),
    ];
    for at in [t.at(), t.sibling_at()] {
        record(registry, at, &first, earlier).await.unwrap();
        record(registry, at, &second, Timestamp::now()).await.unwrap();
    }
    // The gone tool had changed before it went: still nothing to accept.
    let gone_changed = [
        listed("current", 1, ToolEffect::Read),
        listed("changed", 2, ToolEffect::Read),
        listed("gone", 3, ToolEffect::Read),
    ];
    let other = tenant(prepare).await;
    record(registry, other.at(), &first, earlier).await.unwrap();
    record(registry, other.at(), &gone_changed, earlier).await.unwrap();
    record(registry, other.at(), &second, Timestamp::now()).await.unwrap();
    assert_eq!(
        registry
            .accept_change(
                other.organization_id,
                other.workspace,
                other.installation,
                "gone",
                Some([3; 32]),
                &|_| by_suite_in(other.workspace)
            )
            .await,
        Err(ToolError::NotChanged),
        "a gone tool has no change to accept"
    );

    let before = list(registry, t.at()).await.unwrap();
    let accept = |at: At, name: &'static str, hash: u8| async move {
        registry
            .accept_change(at.0, at.1, at.2, name, Some([hash; 32]), &|_| by_suite_in(at.1))
            .await
    };
    assert_eq!(
        accept(t.at(), "changed", 1).await,
        Err(ToolError::Stale),
        "the pinned hash isn't the change"
    );
    assert_eq!(accept(t.at(), "changed", 3).await, Err(ToolError::Stale));
    let placed = |_: &RegisteredTool| by_suite_in(t.workspace);
    let unhashed = |name: &'static str| {
        registry.accept_change(t.organization_id, t.workspace, t.installation, name, None, &placed)
    };
    assert_eq!(unhashed("changed").await, Err(ToolError::Stale), "no hash matches none");
    assert_eq!(
        unhashed("current").await,
        Err(ToolError::NotChanged),
        "after the tool's own checks"
    );
    assert_eq!(unhashed("missing").await, Err(ToolError::ToolNotFound));
    assert_eq!(accept(t.at(), "current", 1).await, Err(ToolError::NotChanged));
    assert_eq!(accept(t.at(), "missing", 2).await, Err(ToolError::ToolNotFound));
    // The full scope, or nothing: another workspace or organization never reaches it.
    let stranger = tenant(prepare).await;
    for at in [
        At(t.organization_id, t.sibling, t.installation),
        At(stranger.organization_id, t.workspace, t.installation),
        At(t.organization_id, t.workspace, t.sibling_installation),
    ] {
        assert_eq!(accept(at, "changed", 2).await, Err(ToolError::NotFound), "{at:?}");
    }
    assert_eq!(
        list(registry, t.at()).await.unwrap(),
        before,
        "a refused accept changes nothing"
    );

    let accepted = accept(t.at(), "changed", 2).await.unwrap();
    let was = before.iter().find(|tool| tool.name == "changed").unwrap();
    assert_eq!(
        accepted,
        RegisteredTool {
            pinned: revision("changed", 2),
            latest: None,
            changed_at: None,
            ..was.clone()
        }
    );
    assert_eq!(accepted.effect, ToolEffect::Read);
    assert_eq!(accepted.status(), ToolStatus::Current);
    let after = list(registry, t.at()).await.unwrap();
    assert_eq!(after.iter().find(|tool| tool.name == "changed"), Some(&accepted));
    assert_eq!(
        accept(t.at(), "changed", 2).await,
        Err(ToolError::NotChanged),
        "accepted once"
    );
    // The sibling's own change is still waiting.
    let sibling = list(registry, t.sibling_at()).await.unwrap();
    assert_eq!(
        sibling
            .iter()
            .find(|tool| tool.name == "changed")
            .map(RegisteredTool::status),
        Some(ToolStatus::Changed)
    );
    // Listed as now pinned, it stays current.
    record(registry, t.at(), &second, Timestamp::now()).await.unwrap();
    assert_eq!(
        names(&list(registry, t.at()).await.unwrap()),
        [
            ("changed", ToolStatus::Current),
            ("current", ToolStatus::Current),
            ("gone", ToolStatus::Gone)
        ]
    );
}

/// The outbox: a listing that changes a tool queues its announcement in the
/// same change; adding, keeping, losing or listing a tool again doesn't.
async fn announces_changed_tools(registry: &dyn ToolRegistry, jobs: &dyn JobQueue, prepare: &Prepare) {
    let kind = Name::new(ANNOUNCE).unwrap();
    let announced = || async {
        let mut claimed = Vec::new();
        while let Some(job) = jobs
            .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
            .await
            .unwrap()
        {
            jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
            claimed.push((
                job.scope,
                Announcement::parse(&job.payload).expect("an announcement reads back"),
            ));
        }
        claimed
    };
    announced().await;
    let a = tenant(prepare).await;
    let at = a.at();
    let now = Timestamp::now();
    record(
        registry,
        at,
        &[
            listed("search", 1, ToolEffect::Read),
            listed("send", 1, ToolEffect::Write),
        ],
        now,
    )
    .await
    .unwrap();
    record(registry, at, &[listed("search", 1, ToolEffect::Read)], now)
        .await
        .unwrap();
    record(
        registry,
        at,
        &[
            listed("search", 1, ToolEffect::Read),
            listed("send", 1, ToolEffect::Write),
        ],
        now,
    )
    .await
    .unwrap();
    assert!(
        announced().await.is_empty(),
        "adding, losing and listing a tool again announce nothing"
    );

    record(
        registry,
        at,
        &[
            listed("search", 2, ToolEffect::Read),
            listed("send", 2, ToolEffect::Write),
        ],
        now,
    )
    .await
    .unwrap();
    let claimed = announced().await;
    assert_eq!(
        claimed.len(),
        1,
        "one announcement for the listing, however many tools changed"
    );
    let (scope, announcement) = claimed[0];
    assert_eq!(
        scope,
        Scope::Workspace(a.organization_id, a.workspace),
        "in the installation's workspace"
    );
    assert_eq!(
        (announcement.kind, announcement.subject, announcement.created_at),
        (
            WebhookEventType::AppToolsChanged,
            Subject::Installation(a.installation),
            now
        )
    );
    record(
        registry,
        at,
        &[
            listed("search", 2, ToolEffect::Read),
            listed("send", 2, ToolEffect::Write),
        ],
        now,
    )
    .await
    .unwrap();
    assert!(
        announced().await.is_empty(),
        "the same change listed again isn't announced again"
    );
}
