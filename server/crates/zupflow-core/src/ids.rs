//! Typed, prefixed identifiers.
//!
//! Every identifier Zupflow shows looks like `{prefix}_{body}`, for example
//! `run_031DCp7XMZ9AHIB2uF6QgS`: the prefix names the object, the body is its
//! UUIDv7 in base62 (`0-9A-Za-z`, in ASCII order), left-padded to 22
//! characters. Storage keeps the UUID; the prefix is never stored.
//!
//! [`Id<K>`] carries its kind in the type, so an agent's ID can't be passed
//! where a workspace's is expected, and organization and workspace arguments
//! can't be swapped. Formatting, parsing and serialization all go through it;
//! no code builds or splits an ID by hand.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

/// The body's alphabet, in ASCII order, so IDs sort like their UUIDs.
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Every body is exactly this long: the width that keeps string order equal to UUID order.
pub const BODY_LENGTH: usize = 22;

mod sealed {
    /// Only the kinds declared below implement it, so no crate can add a prefix the registry doesn't hold.
    pub trait Sealed {}
}

/// An object type that has an ID: its name and its prefix. Sealed: every kind
/// is declared in this module's registry, which a test holds equal to the
/// contract's.
pub trait Kind: sealed::Sealed + 'static {
    /// The prefix, without the underscore. Never changed, reused or shared.
    const PREFIX: &'static str;
    /// The name the contract uses for it, e.g. `connect_session`.
    const NAME: &'static str;
}

macro_rules! kinds {
    ($($(#[$doc:meta])* $kind:ident, $alias:ident => $name:literal, $prefix:literal;)*) => {
        /// The object types that have IDs.
        pub mod kind {
            use super::{Kind, sealed::Sealed};
            $(
                $(#[$doc])*
                #[derive(Debug)]
                pub enum $kind {}
                impl Sealed for $kind {}
                impl Kind for $kind {
                    const PREFIX: &'static str = $prefix;
                    const NAME: &'static str = $name;
                }
            )*
        }
        $(pub type $alias = Id<kind::$kind>;)*

        /// Every kind's name and prefix, in one place. The contract holds the same registry.
        pub const REGISTRY: &[(&str, &str)] = &[$(($name, $prefix)),*];
    };
}

kinds! {
    /// One of Zupflow's customers.
    Organization, OrganizationId => "organization", "org";
    /// One of an organization's own customers, in one environment.
    Workspace, WorkspaceId => "workspace", "ws";
    User, UserId => "user", "usr";
    /// A user's membership in an organization; an invitation is one with status `invited`.
    Member, MemberId => "member", "mem";
    App, AppId => "app", "app";
    Installation, InstallationId => "installation", "inst";
    ConnectSession, ConnectSessionId => "connect_session", "cs";
    Agent, AgentId => "agent", "agt";
    Grant, GrantId => "grant", "grt";
    Run, RunId => "run", "run";
    RunEvent, RunEventId => "run_event", "rev";
    ToolCall, ToolCallId => "tool_call", "call";
    Session, SessionId => "session", "ses";
    ApiKey, ApiKeyId => "api_key", "key";
    WebhookEndpoint, WebhookEndpointId => "webhook_endpoint", "we";
    WebhookEvent, WebhookEventId => "webhook_event", "evt";
    WebhookDelivery, WebhookDeliveryId => "webhook_delivery", "whd";
    AuditEvent, AuditEventId => "audit_event", "aud";
    /// One organization's account with a model provider, in one environment.
    ModelConnection, ModelConnectionId => "model_connection", "mcn";
    /// A model an organization added from one of its model connections.
    Model, ModelId => "model", "mdl";
}

/// An object's identifier, typed by its kind.
pub struct Id<K: Kind> {
    uuid: Uuid,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind> Id<K> {
    /// A new ID for an object created now: UUIDv7, so IDs sort by creation time.
    pub fn generate() -> Self {
        Self::from_uuid(Uuid::now_v7())
    }

    /// The ID of a stored object, from its UUID column. For row mapping only:
    /// the caller vouches that the UUID belongs to an object of this kind, so
    /// never build one from another kind's [`Id::uuid`].
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self {
            uuid,
            kind: PhantomData,
        }
    }

    /// The UUID, for binding to a storage column. Never shown outside the server.
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// Parses the prefixed form strictly: the exact prefix, one underscore and a
    /// 22-character body that decodes to 128 bits. No trimming, no case folding,
    /// and no bare UUIDs. Anything else is an ID that can't exist.
    pub fn parse(value: &str) -> Result<Self, ParseIdError> {
        let invalid = || ParseIdError { kind: K::NAME };
        let body = value
            .strip_prefix(K::PREFIX)
            .and_then(|rest| rest.strip_prefix('_'))
            .ok_or_else(invalid)?;
        decode(body)
            .map(|n| Self::from_uuid(Uuid::from_u128(n)))
            .ok_or_else(invalid)
    }
}

/// A string that isn't a valid ID of the expected kind. The rejected input is
/// deliberately not kept: callers answer `404` and reveal nothing about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a valid {kind} ID")]
pub struct ParseIdError {
    /// The kind that was expected.
    pub kind: &'static str,
}

fn encode(mut n: u128) -> [u8; BODY_LENGTH] {
    let mut out = [b'0'; BODY_LENGTH];
    for slot in out.iter_mut().rev() {
        // `n % 62` is below 62, so the index is always in range.
        *slot = ALPHABET[(n % 62) as usize];
        n /= 62;
    }
    out
}

fn decode(body: &str) -> Option<u128> {
    if body.len() != BODY_LENGTH {
        return None;
    }
    body.bytes().try_fold(0u128, |n, byte| {
        let digit = match byte {
            b'0'..=b'9' => byte - b'0',
            b'A'..=b'Z' => byte - b'A' + 10,
            b'a'..=b'z' => byte - b'a' + 36,
            _ => return None,
        };
        // Twenty-two base62 digits can exceed 128 bits; those bodies aren't IDs.
        n.checked_mul(62)?.checked_add(u128::from(digit))
    })
}

impl<K: Kind> fmt::Display for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let body = encode(self.uuid.as_u128());
        // The alphabet is ASCII, so the body is always valid UTF-8.
        let body = std::str::from_utf8(&body).map_err(|_| fmt::Error)?;
        write!(f, "{}_{}", K::PREFIX, body)
    }
}

impl<K: Kind> fmt::Debug for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<K: Kind> FromStr for Id<K> {
    type Err = ParseIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl<K: Kind> Clone for Id<K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<K: Kind> Copy for Id<K> {}

impl<K: Kind> PartialEq for Id<K> {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
    }
}

impl<K: Kind> Eq for Id<K> {}

impl<K: Kind> Hash for Id<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.uuid.hash(state);
    }
}

impl<K: Kind> PartialOrd for Id<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// UUIDv7 order: creation time first, the same order as the strings.
impl<K: Kind> Ord for Id<K> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.uuid.cmp(&other.uuid)
    }
}

impl<K: Kind> Serialize for Id<K> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de, K: Kind> Deserialize<'de> for Id<K> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Cow::<str>::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn encodes_the_documented_example() {
        let uuid = Uuid::parse_str("0199a7c4-5e2b-7c3d-9f1a-2b3c4d5e6f70").expect("a valid UUID literal");
        let id = RunId::from_uuid(uuid);
        assert_eq!(id.to_string(), "run_031DCp7XMZ9AHIB2uF6QgS");
        assert_eq!(RunId::parse("run_031DCp7XMZ9AHIB2uF6QgS"), Ok(id));
    }

    #[test]
    fn round_trips_the_extremes() {
        for n in [0, 1, 61, 62, u128::MAX] {
            let id = WorkspaceId::from_uuid(Uuid::from_u128(n));
            assert_eq!(WorkspaceId::parse(&id.to_string()), Ok(id));
        }
        assert_eq!(
            WorkspaceId::from_uuid(Uuid::from_u128(u128::MAX)).to_string(),
            "ws_7n42DGM5Tflk9n8mt7Fhc7"
        );
    }

    #[test]
    fn parses_strictly() {
        let valid = "ws_031DCp7XMZ9AHIB2uF6QgS";
        assert!(WorkspaceId::parse(valid).is_ok());
        for bad in [
            "",
            "ws_",
            "ws031DCp7XMZ9AHIB2uF6QgS",
            "WS_031DCp7XMZ9AHIB2uF6QgS",
            "run_031DCp7XMZ9AHIB2uF6QgS",
            "ws_031DCp7XMZ9AHIB2uF6Qg",
            "ws_031DCp7XMZ9AHIB2uF6QgSx",
            "ws_031DCp7XMZ9AHIB2uF6Qg-",
            " ws_031DCp7XMZ9AHIB2uF6QgS",
            "ws_031DCp7XMZ9AHIB2uF6QgS ",
            "ws__031DCp7XMZ9AHIB2uF6QgS",
            // One past the largest 128-bit body.
            "ws_7n42DGM5Tflk9n8mt7Fhc8",
            "ws_zzzzzzzzzzzzzzzzzzzzzz",
            "0199a7c4-5e2b-7c3d-9f1a-2b3c4d5e6f70",
        ] {
            assert_eq!(
                WorkspaceId::parse(bad),
                Err(ParseIdError { kind: "workspace" }),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn sorts_by_creation_as_ids_and_as_strings() {
        let ids: Vec<RunId> = (0..64).map(|_| RunId::generate()).collect();
        let mut by_id = ids.clone();
        by_id.sort();
        let mut by_string: Vec<String> = ids.iter().map(ToString::to_string).collect();
        by_string.sort();
        assert_eq!(by_id.iter().map(ToString::to_string).collect::<Vec<_>>(), by_string);
        assert_eq!(by_id, ids, "generated in creation order");
    }

    #[test]
    fn serializes_as_the_prefixed_string() {
        let id = AgentId::generate();
        let json = serde_json::to_string(&id).expect("an ID serializes");
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<AgentId>(&json).expect("and parses back"), id);
        assert!(
            serde_json::from_str::<WorkspaceId>(&json).is_err(),
            "never as another kind"
        );
        assert_eq!(
            format!("{id:?}"),
            id.to_string(),
            "debug output never shows a bare UUID"
        );
    }

    #[test]
    fn registers_each_prefix_once() {
        let prefixes: HashSet<_> = REGISTRY.iter().map(|(_, prefix)| *prefix).collect();
        let names: HashSet<_> = REGISTRY.iter().map(|(name, _)| *name).collect();
        assert_eq!(prefixes.len(), REGISTRY.len());
        assert_eq!(names.len(), REGISTRY.len());
        for (_, prefix) in REGISTRY {
            assert!(prefix.bytes().all(|b| b.is_ascii_lowercase()), "{prefix}");
        }
    }
}
