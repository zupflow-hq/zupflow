//! [`InProcessEventBus`]: the event bus within one server process, on tokio
//! broadcast channels, one per scope and topic. Each subscriber has a bounded
//! backlog (rounded up to a power of two); one that falls behind is told how
//! many events it missed instead of holding everyone else up. A channel is
//! dropped when its last subscriber is, so the map holds only channels someone
//! is listening to.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use tokio::sync::broadcast;
use zupflow_core::events::{EventBus, EventError, EventReceiver, MIN_EVENT_BYTES, Subscription};
use zupflow_core::scope::{Name, Scope};

type Slot = (Scope, Name);
type Channels = HashMap<Slot, broadcast::Sender<Arc<[u8]>>>;

pub struct InProcessEventBus {
    channels: Arc<Mutex<Channels>>,
    backlog: usize,
    max_event_bytes: usize,
}

/// The largest backlog accepted; tokio rounds it up to a power of two.
const MAX_BACKLOG: usize = 1 << 16;

fn lock(channels: &Mutex<Channels>) -> MutexGuard<'_, Channels> {
    // The map holds only senders: after a panic elsewhere it's still consistent.
    channels.lock().unwrap_or_else(|poisoned| {
        channels.clear_poison();
        poisoned.into_inner()
    })
}

impl InProcessEventBus {
    /// `backlog` events per subscriber before it lags (1 to 65,536); events up
    /// to `max_event_bytes`, raised to `MIN_EVENT_BYTES` if below it.
    pub fn new(backlog: usize, max_event_bytes: usize) -> Self {
        Self {
            channels: Arc::new(Mutex::new(HashMap::new())),
            backlog: backlog.clamp(1, MAX_BACKLOG),
            max_event_bytes: max_event_bytes.max(MIN_EVENT_BYTES),
        }
    }

    /// How many scope-and-topic channels are open, for tests.
    #[doc(hidden)]
    pub fn open_channels(&self) -> usize {
        lock(&self.channels).len()
    }
}

impl Default for InProcessEventBus {
    /// 256 events of backlog per subscriber, 64 KiB per event.
    fn default() -> Self {
        Self::new(256, 64 << 10)
    }
}

struct Receiver {
    receiver: Option<broadcast::Receiver<Arc<[u8]>>>,
    channels: Weak<Mutex<Channels>>,
    slot: Slot,
}

#[async_trait]
impl EventReceiver for Receiver {
    async fn recv(&mut self) -> Result<Option<Arc<[u8]>>, EventError> {
        let Some(receiver) = self.receiver.as_mut() else {
            return Ok(None);
        };
        match receiver.recv().await {
            Ok(payload) => Ok(Some(payload)),
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                let (scope, topic) = &self.slot;
                tracing::warn!(
                    event = "events.subscriber_lagged",
                    organization_id = %scope.organization_id(),
                    workspace_id = scope.workspace_id().map(|id| id.to_string()),
                    environment = scope.environment().map(|environment| environment.as_str()),
                    topic = topic.as_str(),
                    missed,
                );
                Err(EventError::Lagged(Some(missed)))
            }
            Err(broadcast::error::RecvError::Closed) => Ok(None),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        // Let go of this subscriber first, then drop the channel if nobody else listens.
        drop(self.receiver.take());
        if let Some(channels) = self.channels.upgrade() {
            let mut channels = lock(&channels);
            if channels
                .get(&self.slot)
                .is_some_and(|sender| sender.receiver_count() == 0)
            {
                channels.remove(&self.slot);
            }
        }
    }
}

#[async_trait]
impl EventBus for InProcessEventBus {
    async fn publish(&self, scope: &Scope, topic: &Name, payload: Vec<u8>) -> Result<(), EventError> {
        if payload.len() > self.max_event_bytes {
            return Err(EventError::TooLarge);
        }
        let channels = lock(&self.channels);
        if let Some(sender) = channels.get(&(*scope, topic.clone())) {
            // Only fails when every receiver dropped in between: nobody to tell.
            let _ = sender.send(Arc::from(payload));
        }
        Ok(())
    }

    async fn subscribe(&self, scope: &Scope, topic: &Name) -> Result<Subscription, EventError> {
        let slot = (*scope, topic.clone());
        let mut channels = lock(&self.channels);
        let sender = channels
            .entry(slot.clone())
            .or_insert_with(|| broadcast::channel(self.backlog).0);
        Ok(Subscription::new(Box::new(Receiver {
            receiver: Some(sender.subscribe()),
            channels: Arc::downgrade(&self.channels),
            slot,
        })))
    }
}
