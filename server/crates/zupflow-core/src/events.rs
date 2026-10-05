//! The event bus: in-process notifications, such as "this run changed", for
//! whoever in the server is listening (a live view, a worker).
//!
//! **Exact scope.** A subscriber hears only its own scope and topic. A
//! workspace's subscriber never hears its organization's events, and an
//! organization's or environment's subscriber never hears its workspaces'.
//! A consumer that follows a whole environment (an organization's live list of
//! runs, say) is served by publishers that also publish to
//! `Scope::Environment(org, env)`, naming the workspace in the payload. Never
//! put environment-bound notifications on the bare organization scope, or
//! Sandbox and Production would share a channel.
//!
//! **Channels.** An adapter with a flat channel namespace names each channel
//! `Scope::storage_prefix()` + `/` + topic, so no topic spells another scope's
//! channel; the conformance suite tries topics that would.
//!
//! **Delivery.** At most once and never replayed: once `subscribe` returns the
//! subscription is active, and it receives everything published after that,
//! in each publisher's order. Events are notifications, so a reader fetches
//! current state after one. Anything that must not be lost (a webhook to a
//! customer) goes through the outbox and the job queue, never only the bus.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;

use crate::scope::{Name, Scope};

/// Every adapter accepts events of at least this many bytes.
pub const MIN_EVENT_BYTES: usize = 16 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    /// The bus can't be reached. From `recv`, the subscription is over:
    /// subscribe again, then refetch state.
    #[error("the event bus is unavailable")]
    Unavailable,
    #[error("the event is larger than this bus accepts")]
    TooLarge,
    /// The subscriber missed events: how many when the bus knows, `None` when
    /// it can't (after a reconnect, say). The subscription goes on; the reader
    /// refetches state.
    #[error("missed events")]
    Lagged(Option<u64>),
}

/// The receiving end an implementation hands out.
#[async_trait]
pub trait EventReceiver: Send {
    /// The next event, or `None` once the bus is gone.
    async fn recv(&mut self) -> Result<Option<Arc<[u8]>>, EventError>;
}

/// A subscription to one topic in one scope.
pub struct Subscription(Box<dyn EventReceiver>);

impl Subscription {
    pub fn new(receiver: Box<dyn EventReceiver>) -> Self {
        Self(receiver)
    }

    pub async fn next(&mut self) -> Result<Option<Arc<[u8]>>, EventError> {
        self.0.recv().await
    }
}

impl fmt::Debug for Subscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Subscription")
    }
}

/// Publishes and subscribes to events, by scope and topic. Implementations
/// pass the suite in `zupflow-conformance`.
#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, scope: &Scope, topic: &Name, payload: Vec<u8>) -> Result<(), EventError>;
    /// Returns once the subscription is active.
    async fn subscribe(&self, scope: &Scope, topic: &Name) -> Result<Subscription, EventError>;
}
