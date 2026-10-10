//! The `ToolPolicyStore` suite: one policy per organization, read as the
//! defaults until saved; levels kept per catalog app and tool name, reached
//! only through their own organization; a suggested level unconfirmed and
//! never replacing one already there; a set level confirmed by its writer;
//! a malformed change written not at all; levels read a page at a time, by
//! name.
//!
//! `prepare` makes a [`Tenant`] exist: its organization, a member user, two
//! catalog apps, and a workspace with an app of its own.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use zupflow_core::apps::ToolEffect;
use zupflow_core::ids::{AppId, OrganizationId, UserId, WorkspaceId};
use zupflow_core::policy::{
    AppToolLevel, ApprovalMode, MAX_LEVELS_PER_CHANGE, MAX_PAGE, NewToolLevel, PolicyError, PolicySettings, ToolLevel,
    ToolPolicyStore,
};
use zupflow_core::time::Timestamp;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    /// A user, member of the organization.
    pub user: UserId,
    pub catalog_app: AppId,
    pub second_catalog_app: AppId,
    pub workspace: WorkspaceId,
    /// The workspace's own app: the organization keeps no levels for it.
    pub workspace_app: AppId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        user: UserId::generate(),
        catalog_app: AppId::generate(),
        second_catalog_app: AppId::generate(),
        workspace: WorkspaceId::generate(),
        workspace_app: AppId::generate(),
    };
    prepare(tenant).await;
    tenant
}

fn set(levels: &[(&str, ToolLevel)]) -> Vec<(String, ToolLevel)> {
    levels
        .iter()
        .map(|(tool, level)| ((*tool).to_owned(), *level))
        .collect()
}

fn listing(tools: &[(&str, ToolEffect)]) -> Vec<(String, ToolEffect)> {
    tools
        .iter()
        .map(|(tool, effect)| ((*tool).to_owned(), *effect))
        .collect()
}

/// Every level of the app, read two at a time, so every case pages.
async fn all(store: &dyn ToolPolicyStore, org: OrganizationId, app: AppId) -> Result<Vec<AppToolLevel>, PolicyError> {
    let mut levels = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store.list_levels(org, app, after.as_deref(), 2).await?;
        assert!(page.items.len() <= 2, "a page holds at most its limit");
        levels.extend(page.items);
        match page.next_after {
            Some(next) => after = Some(next),
            None => {
                assert_eq!(
                    usize::try_from(page.total).ok(),
                    Some(levels.len()),
                    "the total counts every level"
                );
                return Ok(levels);
            }
        }
    }
}

/// Each level as (tool, level, confirmed, destructive).
fn summary(levels: &[AppToolLevel]) -> Vec<(&str, ToolLevel, bool, bool)> {
    levels
        .iter()
        .map(|level| (level.tool.as_str(), level.level, level.confirmed, level.destructive))
        .collect()
}

pub async fn run(store: &dyn ToolPolicyStore, prepare: Prepare) {
    reads_the_defaults_until_saved(store, &prepare).await;
    suggests_levels_without_replacing_any(store, &prepare).await;
    sets_levels_confirmed_by_their_writer(store, &prepare).await;
    writes_a_malformed_change_not_at_all(store, &prepare).await;
    pages_by_name(store, &prepare).await;
    keeps_levels_to_their_organization_and_catalog_app(store, &prepare).await;
}

async fn reads_the_defaults_until_saved(store: &dyn ToolPolicyStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let other = tenant(prepare).await;
    let org = t.organization_id;
    let policy = store.load_policy(org).await.unwrap();
    assert_eq!(
        (
            policy.organization_id,
            policy.settings,
            policy.updated_at,
            policy.updated_by
        ),
        (org, PolicySettings::default(), None, None),
        "a policy never saved reads as the defaults"
    );
    assert_eq!(
        PolicySettings::default(),
        PolicySettings {
            mode: ApprovalMode::Manual,
            new_tool_level: NewToolLevel::Approve
        }
    );

    let deny = PolicySettings {
        mode: ApprovalMode::Manual,
        new_tool_level: NewToolLevel::Deny,
    };
    let now = Timestamp::from_millis(Timestamp::now().millis());
    let saved = store.save_policy(org, deny, Some(t.user), now).await.unwrap();
    assert_eq!(
        (saved.organization_id, saved.settings, saved.updated_at),
        (org, deny, Some(now))
    );
    assert_eq!(saved.updated_by.as_ref().map(|user| user.id), Some(t.user));
    assert_eq!(store.load_policy(org).await.unwrap(), saved, "saved as answered");
    assert_eq!(
        store.load_policy(other.organization_id).await.unwrap().settings,
        PolicySettings::default(),
        "one organization's policy is never another's"
    );

    // Saved again, by an API key: one record, no writer.
    let later = Timestamp::from_millis(now.millis() + 1);
    let again = store
        .save_policy(org, PolicySettings::default(), None, later)
        .await
        .unwrap();
    assert_eq!(
        (again.settings, again.updated_at, again.updated_by.clone()),
        (PolicySettings::default(), Some(later), None)
    );
    assert_eq!(store.load_policy(org).await.unwrap(), again);
}

async fn suggests_levels_without_replacing_any(store: &dyn ToolPolicyStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, app) = (t.organization_id, t.catalog_app);
    let now = Timestamp::now();
    let suggested = store
        .suggest_levels(
            org,
            app,
            &listing(&[
                ("search", ToolEffect::Read),
                ("send", ToolEffect::Write),
                ("drop", ToolEffect::Destructive),
            ]),
            now,
        )
        .await
        .unwrap();
    assert_eq!(suggested, 3);
    let levels = all(store, org, app).await.unwrap();
    assert_eq!(
        summary(&levels),
        [
            ("drop", ToolLevel::Approve, false, true),
            ("search", ToolLevel::Allow, false, false),
            ("send", ToolLevel::Approve, false, false),
        ],
        "suggested from hints, unconfirmed, by name"
    );
    assert!(
        levels
            .iter()
            .all(|level| level.app_id == app && level.updated_by.is_none())
    );

    store
        .set_levels(org, app, &set(&[("send", ToolLevel::Allow)]), Some(t.user), now)
        .await
        .unwrap();
    // Listed again with other hints, and a new tool: only the new one is suggested.
    let suggested = store
        .suggest_levels(
            org,
            app,
            &listing(&[
                ("search", ToolEffect::Destructive),
                ("send", ToolEffect::Destructive),
                ("archive", ToolEffect::Write),
            ]),
            now,
        )
        .await
        .unwrap();
    assert_eq!(suggested, 1);
    assert_eq!(
        summary(&all(store, org, app).await.unwrap()),
        [
            ("archive", ToolLevel::Approve, false, false),
            ("drop", ToolLevel::Approve, false, true),
            ("search", ToolLevel::Allow, false, false),
            ("send", ToolLevel::Allow, true, false),
        ],
        "a suggestion never changes a level already there, suggested or confirmed"
    );
    assert_eq!(store.suggest_levels(org, app, &[], now).await, Ok(0));
}

async fn sets_levels_confirmed_by_their_writer(store: &dyn ToolPolicyStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, app) = (t.organization_id, t.catalog_app);
    let earlier = Timestamp::from_millis(Timestamp::now().millis() - 60_000);
    store
        .suggest_levels(org, app, &listing(&[("drop", ToolEffect::Destructive)]), earlier)
        .await
        .unwrap();
    let now = Timestamp::from_millis(Timestamp::now().millis());
    let written = store
        .set_levels(
            org,
            app,
            &set(&[("drop", ToolLevel::Deny), ("ahead", ToolLevel::Allow)]),
            Some(t.user),
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        summary(&written),
        [
            ("ahead", ToolLevel::Allow, true, false),
            ("drop", ToolLevel::Deny, true, true),
        ],
        "confirmed, the hint kept, by name; a name nothing listed yet can be set ahead of it"
    );
    assert!(written.iter().all(|level| level.app_id == app
        && level.updated_at == now
        && level.updated_by.as_ref().map(|user| user.id) == Some(t.user)));
    assert_eq!(all(store, org, app).await.unwrap(), written, "stored as answered");

    // Set by an API key: confirmed, no writer.
    let changed = store
        .set_levels(org, app, &set(&[("ahead", ToolLevel::Approve)]), None, now)
        .await
        .unwrap();
    assert_eq!(summary(&changed), [("ahead", ToolLevel::Approve, true, false)]);
    assert_eq!(changed[0].updated_by, None);

    let found = store
        .find_levels(org, app, &["drop".to_owned(), "missing".to_owned()])
        .await
        .unwrap();
    assert_eq!(
        summary(&found),
        [("drop", ToolLevel::Deny, true, true)],
        "only those named, those it has"
    );
    assert_eq!(store.find_levels(org, app, &[]).await, Ok(vec![]));
}

async fn writes_a_malformed_change_not_at_all(store: &dyn ToolPolicyStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, app) = (t.organization_id, t.catalog_app);
    let now = Timestamp::now();
    let too_many: Vec<(String, ToolLevel)> = (0..=MAX_LEVELS_PER_CHANGE)
        .map(|i| (format!("t{i:03}"), ToolLevel::Allow))
        .collect();
    for levels in [
        vec![],
        set(&[("a", ToolLevel::Allow), ("a", ToolLevel::Deny)]),
        set(&[("a", ToolLevel::Allow), ("", ToolLevel::Deny)]),
        vec![("a".to_owned(), ToolLevel::Allow), ("x".repeat(129), ToolLevel::Deny)],
        too_many,
    ] {
        assert_eq!(
            store.set_levels(org, app, &levels, Some(t.user), now).await,
            Err(PolicyError::InvalidLevels),
            "{} levels",
            levels.len()
        );
    }
    assert_eq!(all(store, org, app).await.unwrap(), [], "nothing written");
    let longest = vec![("x".repeat(128), ToolLevel::Deny)];
    assert_eq!(store.set_levels(org, app, &longest, None, now).await.unwrap().len(), 1);
}

async fn pages_by_name(store: &dyn ToolPolicyStore, prepare: &Prepare) {
    let t = tenant(prepare).await;
    let (org, app) = (t.organization_id, t.catalog_app);
    let names = ["b", "a", "d", "c", "e"];
    store
        .set_levels(
            org,
            app,
            &names
                .iter()
                .map(|name| ((*name).to_owned(), ToolLevel::Allow))
                .collect::<Vec<_>>(),
            None,
            Timestamp::now(),
        )
        .await
        .unwrap();
    let levels = all(store, org, app).await.unwrap();
    assert_eq!(
        levels.iter().map(|level| level.tool.as_str()).collect::<Vec<_>>(),
        ["a", "b", "c", "d", "e"]
    );
    let page = store.list_levels(org, app, Some("b"), 2).await.unwrap();
    assert_eq!(
        (
            page.items.iter().map(|level| level.tool.as_str()).collect::<Vec<_>>(),
            page.next_after.as_deref(),
            page.total
        ),
        (vec!["c", "d"], Some("d"), 5)
    );
    let last = store.list_levels(org, app, Some("d"), 2).await.unwrap();
    assert_eq!((last.items.len(), last.next_after), (1, None));
    // Limits are clamped.
    assert_eq!(store.list_levels(org, app, None, 0).await.unwrap().items.len(), 1);
    let most = store.list_levels(org, app, None, MAX_PAGE + 1).await.unwrap();
    assert_eq!((most.items.len(), most.next_after), (5, None));
}

async fn keeps_levels_to_their_organization_and_catalog_app(store: &dyn ToolPolicyStore, prepare: &Prepare) {
    let a = tenant(prepare).await;
    let b = tenant(prepare).await;
    let now = Timestamp::now();
    let org = a.organization_id;
    store
        .set_levels(org, a.catalog_app, &set(&[("search", ToolLevel::Deny)]), None, now)
        .await
        .unwrap();
    store
        .suggest_levels(
            org,
            a.second_catalog_app,
            &listing(&[("search", ToolEffect::Read)]),
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        summary(&all(store, org, a.second_catalog_app).await.unwrap()),
        [("search", ToolLevel::Allow, false, false)],
        "one app's levels are never another's"
    );
    assert_eq!(
        summary(&all(store, org, a.catalog_app).await.unwrap()),
        [("search", ToolLevel::Deny, true, false)]
    );

    // A workspace app, and another organization's app, are not found.
    for (org, app) in [
        (org, a.workspace_app),
        (org, b.catalog_app),
        (b.organization_id, a.catalog_app),
        (org, AppId::generate()),
    ] {
        let what = format!("{org} {app}");
        assert_eq!(
            store.list_levels(org, app, None, 10).await,
            Err(PolicyError::AppNotFound),
            "{what}"
        );
        assert_eq!(
            store.find_levels(org, app, &["search".to_owned()]).await,
            Err(PolicyError::AppNotFound),
            "{what}"
        );
        assert_eq!(
            store
                .set_levels(org, app, &set(&[("search", ToolLevel::Allow)]), None, now)
                .await,
            Err(PolicyError::AppNotFound),
            "{what}"
        );
        assert_eq!(
            store
                .suggest_levels(org, app, &listing(&[("search", ToolEffect::Read)]), now)
                .await,
            Err(PolicyError::AppNotFound),
            "{what}"
        );
    }
    assert_eq!(all(store, b.organization_id, b.catalog_app).await.unwrap(), []);
    assert_eq!(
        summary(&all(store, org, a.catalog_app).await.unwrap()),
        [("search", ToolLevel::Deny, true, false)]
    );
}
