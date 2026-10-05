//! The `EventBus` suite: subscribers get what's published after they
//! subscribe, in order, on their exact scope and topic, and nothing else,
//! including through topics spelled like another scope's channel.
//!
//! "Nothing else" is checked by waiting `settle` for a leak. An adapter that
//! batches or polls passes a settle time longer than its delivery delay.

use std::time::Duration;

use zupflow_core::environment::Environment;
use zupflow_core::events::{EventBus, MIN_EVENT_BYTES, Subscription};
use zupflow_core::ids::{OrganizationId, WorkspaceId};
use zupflow_core::scope::{Name, Scope};

fn topic(value: &str) -> Name {
    Name::new(value).unwrap()
}

/// The next event, failing the suite if none arrives in time.
async fn next(subscription: &mut Subscription) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(10), subscription.next())
        .await
        .expect("an event arrives")
        .unwrap()
        .expect("the bus is still there")
        .to_vec()
}

/// Asserts nothing arrives within `settle`.
async fn nothing(subscription: &mut Subscription, settle: Duration, what: &str) {
    let waited = tokio::time::timeout(settle, subscription.next()).await;
    assert!(waited.is_err(), "{what}: expected nothing, got {waited:?}");
}

/// Asserts none of the subscriptions receives anything within `settle`. Each
/// waits the full `settle` on its own, all at once, so an adapter whose `recv`
/// makes a round trip has the whole time to deliver a leak.
async fn nothing_in_any(subscriptions: Vec<(String, Subscription)>, settle: Duration) {
    let mut waits = tokio::task::JoinSet::new();
    for (what, mut subscription) in subscriptions {
        waits.spawn(async move {
            let waited = tokio::time::timeout(settle, subscription.next()).await;
            (what, waited.map(|event| format!("{event:?}")))
        });
    }
    while let Some(joined) = waits.join_next().await {
        let (what, waited) = joined.expect("a wait doesn't panic");
        assert!(waited.is_err(), "{what}: expected nothing, got {waited:?}");
    }
}

/// Runs the suite with a 300 ms settle time, enough for an in-process bus.
pub async fn run(bus: &dyn EventBus) {
    run_with(bus, Duration::from_millis(300)).await;
}

pub async fn run_with(bus: &dyn EventBus, settle: Duration) {
    delivers_in_order(bus).await;
    delivers_to_every_subscriber(bus).await;
    carries_events_of_the_minimum_size(bus).await;
    never_replays(bus).await;
    keeps_topics_apart(bus, settle).await;
    keeps_scopes_apart(bus, settle).await;
    resists_topics_that_spell_another_channel(bus, settle).await;
}

async fn delivers_in_order(bus: &dyn EventBus) {
    let scope = Scope::Workspace(OrganizationId::generate(), WorkspaceId::generate());
    let runs = topic("runs");
    let mut subscription = bus.subscribe(&scope, &runs).await.unwrap();
    for n in 0..10u8 {
        bus.publish(&scope, &runs, vec![n]).await.unwrap();
    }
    for n in 0..10u8 {
        assert_eq!(next(&mut subscription).await, vec![n], "in publication order");
    }
}

async fn delivers_to_every_subscriber(bus: &dyn EventBus) {
    let scope = Scope::Organization(OrganizationId::generate());
    let audit = topic("audit");
    let mut first = bus.subscribe(&scope, &audit).await.unwrap();
    let mut second = bus.subscribe(&scope, &audit).await.unwrap();
    bus.publish(&scope, &audit, b"event".to_vec()).await.unwrap();
    assert_eq!(next(&mut first).await, b"event");
    assert_eq!(next(&mut second).await, b"event");
    // Publishing with nobody listening is fine.
    bus.publish(
        &Scope::Organization(OrganizationId::generate()),
        &audit,
        b"unheard".to_vec(),
    )
    .await
    .unwrap();
}

async fn carries_events_of_the_minimum_size(bus: &dyn EventBus) {
    let scope = Scope::Organization(OrganizationId::generate());
    let mut subscription = bus.subscribe(&scope, &topic("big")).await.unwrap();
    let payload: Vec<u8> = (0..MIN_EVENT_BYTES).map(|i| (i % 251) as u8).collect();
    bus.publish(&scope, &topic("big"), payload.clone()).await.unwrap();
    assert_eq!(next(&mut subscription).await, payload);
}

async fn never_replays(bus: &dyn EventBus) {
    let scope = Scope::Organization(OrganizationId::generate());
    let runs = topic("runs");
    let mut early = bus.subscribe(&scope, &runs).await.unwrap();
    bus.publish(&scope, &runs, b"before".to_vec()).await.unwrap();
    let mut late = bus.subscribe(&scope, &runs).await.unwrap();
    bus.publish(&scope, &runs, b"after".to_vec()).await.unwrap();
    assert_eq!(next(&mut early).await, b"before");
    assert_eq!(next(&mut early).await, b"after");
    assert_eq!(
        next(&mut late).await,
        b"after",
        "a late subscriber sees only what came after"
    );
}

async fn keeps_topics_apart(bus: &dyn EventBus, settle: Duration) {
    let scope = Scope::Workspace(OrganizationId::generate(), WorkspaceId::generate());
    let mut runs = bus.subscribe(&scope, &topic("runs")).await.unwrap();
    bus.publish(&scope, &topic("installations"), b"other topic".to_vec())
        .await
        .unwrap();
    nothing(&mut runs, settle, "another topic").await;
}

/// The same topic on seven scopes: two organizations, one's own scope, both
/// its environments, two of its workspaces, and the same workspace UUID under
/// the other organization. Each subscriber hears its own scope only.
async fn keeps_scopes_apart(bus: &dyn EventBus, settle: Duration) {
    let (a, b) = (OrganizationId::generate(), OrganizationId::generate());
    let (ws1, ws2) = (WorkspaceId::generate(), WorkspaceId::generate());
    let scopes = [
        Scope::Organization(a),
        Scope::Organization(b),
        Scope::Workspace(a, ws1),
        Scope::Workspace(a, ws2),
        Scope::Workspace(b, ws1),
        Scope::Environment(a, Environment::Sandbox),
        Scope::Environment(a, Environment::Production),
    ];
    let shared = topic("shared");
    let mut subscriptions = Vec::new();
    for scope in &scopes {
        subscriptions.push(bus.subscribe(scope, &shared).await.unwrap());
    }
    for (index, scope) in scopes.iter().enumerate() {
        bus.publish(scope, &shared, vec![index as u8]).await.unwrap();
    }
    let mut heard = Vec::new();
    for (index, mut subscription) in subscriptions.into_iter().enumerate() {
        assert_eq!(
            next(&mut subscription).await,
            vec![index as u8],
            "{:?} hears its own event",
            scopes[index]
        );
        heard.push((format!("{:?} hears only its own scope", scopes[index]), subscription));
    }
    nothing_in_any(heard, settle).await;
}

/// From an organization's own scope and from its Sandbox, topics spelled like
/// a workspace's or an environment's channel, in the common flat layouts, hear
/// nothing published there.
async fn resists_topics_that_spell_another_channel(bus: &dyn EventBus, settle: Duration) {
    let org = OrganizationId::generate();
    let ws = WorkspaceId::generate();
    let workspace = Scope::Workspace(org, ws);
    let production = Scope::Environment(org, Environment::Production);
    let imitations = crate::imitations::imitations(org, ws, "shared");
    let mut subscriptions = Vec::new();
    for listener in [Scope::Organization(org), Scope::Environment(org, Environment::Sandbox)] {
        for imitation in &imitations {
            let subscription = bus.subscribe(&listener, &topic(imitation)).await.unwrap();
            subscriptions.push((format!("{listener:?} through {imitation}"), subscription));
        }
    }
    bus.publish(&workspace, &topic("shared"), b"workspace".to_vec())
        .await
        .unwrap();
    bus.publish(&production, &topic("shared"), b"production".to_vec())
        .await
        .unwrap();
    nothing_in_any(subscriptions, settle).await;
}
