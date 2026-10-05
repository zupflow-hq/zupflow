//! The in-process event bus passes the `EventBus` suite, tells a slow
//! subscriber what it missed, and keeps only channels someone listens to.

use zupflow_basic::in_process_events::InProcessEventBus;
use zupflow_core::events::{EventBus, EventError, MIN_EVENT_BYTES};
use zupflow_core::ids::OrganizationId;
use zupflow_core::scope::{Name, Scope};

fn topic(value: &str) -> Name {
    Name::new(value).expect("a valid name")
}

#[tokio::test]
async fn conforms() {
    zupflow_conformance::event_bus::run(&InProcessEventBus::default()).await;
}

#[tokio::test]
async fn tells_a_slow_subscriber_what_it_missed() {
    let bus = InProcessEventBus::new(4, 1024);
    let scope = Scope::Organization(OrganizationId::generate());
    let mut subscription = bus.subscribe(&scope, &topic("runs")).await.expect("subscribes");
    for n in 0..10u8 {
        bus.publish(&scope, &topic("runs"), vec![n]).await.expect("publishes");
    }
    assert_eq!(
        subscription.next().await,
        Err(EventError::Lagged(Some(6))),
        "six were dropped for it"
    );
    let next = subscription.next().await.expect("goes on").expect("an event");
    assert_eq!(next.as_ref(), &[6], "and it goes on with what's left");
}

#[tokio::test]
async fn refuses_an_event_too_large() {
    let bus = InProcessEventBus::new(4, 8);
    let scope = Scope::Organization(OrganizationId::generate());
    // A limit below the trait's minimum is raised to it.
    bus.publish(&scope, &topic("runs"), vec![0; MIN_EVENT_BYTES])
        .await
        .expect("the minimum fits");
    assert_eq!(
        bus.publish(&scope, &topic("runs"), vec![0; MIN_EVENT_BYTES + 1]).await,
        Err(EventError::TooLarge)
    );
}

#[tokio::test]
async fn drops_a_channel_with_its_last_subscriber() {
    let bus = InProcessEventBus::default();
    let scope = Scope::Organization(OrganizationId::generate());
    for n in 0..100 {
        drop(
            bus.subscribe(&scope, &topic(&format!("run-{n}")))
                .await
                .expect("subscribes"),
        );
    }
    assert_eq!(
        bus.open_channels(),
        0,
        "subscribing and leaving, with nothing published, leaves nothing behind"
    );
    let first = bus.subscribe(&scope, &topic("runs")).await.expect("subscribes");
    let second = bus.subscribe(&scope, &topic("runs")).await.expect("subscribes");
    drop(first);
    assert_eq!(bus.open_channels(), 1, "one subscriber still listens");
    drop(second);
    assert_eq!(bus.open_channels(), 0);
}
