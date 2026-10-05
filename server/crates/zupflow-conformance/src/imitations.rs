//! Names spelled like another scope's key or channel in the common flat
//! layouts, shared by the suites so their lists can't drift apart.

use zupflow_core::ids::{OrganizationId, WorkspaceId};

/// From an organization's own scope or one of its environments, names that a
/// flat-keyed adapter could confuse with the workspace `ws`'s `name` or the
/// Production environment's `name`.
pub fn imitations(org: OrganizationId, ws: WorkspaceId, name: &str) -> Vec<String> {
    let (org_hex, ws_hex) = (org.uuid().simple().to_string(), ws.uuid().simple().to_string());
    vec![
        format!("{ws_hex}:{name}"),
        format!("{ws_hex}.{name}"),
        format!("w:{ws_hex}:{name}"),
        format!("w.{ws_hex}.{name}"),
        format!("{ws}:{name}"),
        format!("{org_hex}:{ws_hex}:{name}"),
        format!("{org_hex}.{ws_hex}.{name}"),
        format!("production:{name}"),
        format!("e.production.{name}"),
        format!("e:production:{name}"),
        format!("o:{name}"),
    ]
}
