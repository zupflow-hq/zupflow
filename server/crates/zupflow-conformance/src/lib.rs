//! Conformance suites for the external-resource traits.
//!
//! Every implementation, the basic ones in this repository and private ones
//! elsewhere, runs the suite for its trait from its own tests. An adapter that
//! doesn't pass doesn't ship. Each suite checks behavior, failure modes and the
//! isolation rules; it panics with a message naming the rule that broke.

// A suite reports a broken rule by panicking, like any assertion.
#![allow(clippy::unwrap_used, clippy::panic)]

pub mod agent_store;
pub mod api_key_store;
pub mod app_store;
pub mod approval_policy;
pub mod audit_store;
pub mod cache;
pub mod connect_store;
pub mod event_bus;
pub mod idempotency_store;
pub mod identity;
mod imitations;
pub mod installation_store;
pub mod job_queue;
pub mod key_service;
pub mod metrics;
pub mod model_catalog;
pub mod model_provider;
pub mod object_store;
pub mod organization_store;
pub mod outbound_http;
pub mod run_store;
pub mod tool_policy_store;
pub mod tool_registry;
pub mod webhook_store;
pub mod workspace_store;
