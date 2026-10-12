//! The basic implementations of Zupflow's external-resource traits. They're
//! real and supported, not test doubles: a single-node Zupflow runs on them.
//! Each passes its trait's conformance suite.

pub mod configured_models;
pub mod configured_policy;
pub mod in_process_events;
pub mod in_process_metrics;
#[cfg(unix)]
pub mod local_files;
pub mod local_identity;
pub mod local_keys;
pub mod memory_cache;
pub mod no_model;
pub mod scripted_model;
