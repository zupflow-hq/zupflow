//! Conformance suites for the external-resource traits.
//!
//! Every implementation, the basic ones in this repository and private ones
//! elsewhere, runs the suite for its trait from its own tests. An adapter that
//! doesn't pass doesn't ship. Each suite checks behavior, failure modes and the
//! isolation rules; it panics with a message naming the rule that broke.

// A suite reports a broken rule by panicking, like any assertion.
#![allow(clippy::unwrap_used, clippy::panic)]

pub mod cache;
pub mod event_bus;
mod imitations;
pub mod job_queue;
pub mod key_service;
pub mod object_store;
