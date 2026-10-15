//! Channel providers ([`zupflow_core::channels::provider::ChannelProvider`]).
//!
//! [`fake`] is the only one built so far: it reads deliveries in Slack's
//! wire format, signed as Slack signs them, and its [`fake::FakeChat`]
//! keeps a team's chat in memory, for development and tests. It calls no
//! one and holds no credential.

pub mod fake;
