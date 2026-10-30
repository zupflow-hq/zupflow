//! Digests over canonical JSON (decision 0046).
//!
//! A run's engine binds things to exactly what they were: an approval to the
//! call that was reviewed, a transition to the request that was sent, a
//! model attempt to the request it made. Each is a SHA-256 over **canonical
//! JSON**: object keys sorted by their UTF-8 bytes, no whitespace, strings
//! escaped and numbers written as `serde_json` writes them. The same value
//! always gives the same bytes, whatever order its keys were built in.
//!
//! Every digest starts with a domain tag and a zero byte, so a call's digest
//! can never be taken for a transition's, even over the same JSON.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::ids::{ChannelConnectionId, ChannelConversationId, InstallationId, MemoryTypeId};

/// The domain of a call's digest.
pub const CALL_DOMAIN: &str = "zupflow.run.call.v1";

/// The domain of a channel tool call's digest.
pub const CHANNEL_CALL_DOMAIN: &str = "zupflow.run.channel_call.v1";

/// The domain of a memory tool call's digest.
pub const MEMORY_CALL_DOMAIN: &str = "zupflow.run.memory_call.v1";

/// A SHA-256 digest, written `sha256:` and 64 lowercase hex characters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha256Digest([u8; 32]);

impl Sha256Digest {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The SHA-256 of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// The 64 lowercase hex characters, without the `sha256:` prefix.
    pub fn to_hex(&self) -> String {
        hex(&self.0)
    }

    /// Parses the written form strictly: `sha256:` and 64 lowercase hex
    /// characters, nothing else.
    pub fn parse(value: &str) -> Option<Self> {
        let body = value.strip_prefix("sha256:")?;
        if body.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        for (slot, pair) in bytes.iter_mut().zip(body.as_bytes().chunks_exact(2)) {
            *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(bytes))
    }
}

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.to_hex())
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = std::borrow::Cow::<str>::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| de::Error::custom("not a sha256 digest"))
    }
}

/// `value` as canonical JSON: keys sorted by their UTF-8 bytes, no
/// whitespace.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(object) => {
            let mut keys: Vec<&String> = object.keys().collect();
            keys.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                if let Some(item) = object.get(key) {
                    write_canonical(item, out);
                }
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    // A string always serializes; the fallback is never taken.
    match serde_json::to_string(text) {
        Ok(escaped) => out.push_str(&escaped),
        Err(_) => out.push_str("\"\""),
    }
}

/// The SHA-256 of `domain`, a zero byte and `value` as canonical JSON.
pub fn digest_json(domain: &str, value: &Value) -> Sha256Digest {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical_json(value).as_bytes());
    Sha256Digest(hasher.finalize().into())
}

/// A call's digest: what an approval authorizes and a send checks again
/// (decision 0046). It covers the installation the call goes to, the app's
/// key, the tool's original MCP name, the tool's pinned definition (by the
/// SHA-256 of its canonical JSON, which the tool registry keeps) and the
/// arguments. Any of them changing gives another digest.
pub fn call_digest(
    installation_id: InstallationId,
    app_key: &str,
    tool: &str,
    definition_sha256: &Sha256Digest,
    arguments: &Map<String, Value>,
) -> Sha256Digest {
    let mut value = Map::new();
    value.insert("installation_id".to_owned(), Value::String(installation_id.to_string()));
    value.insert("app_key".to_owned(), Value::String(app_key.to_owned()));
    value.insert("tool".to_owned(), Value::String(tool.to_owned()));
    value.insert(
        "definition_sha256".to_owned(),
        Value::String(definition_sha256.to_hex()),
    );
    value.insert("arguments".to_owned(), Value::Object(arguments.clone()));
    digest_json(CALL_DOMAIN, &Value::Object(value))
}

/// A channel tool call's digest: its connection, the provider and the
/// tool, the definition it was decided against, its arguments (the exact
/// text, and any conversation, thread, person or message it names), and
/// the conversation and thread the run was started in, where a `reply`
/// goes. An approval is bound to it, and a send checks it again, so what's
/// posted is exactly what was reviewed, and where.
pub fn channel_call_digest(
    connection_id: ChannelConnectionId,
    provider: &str,
    tool: &str,
    definition_sha256: &Sha256Digest,
    arguments: &Map<String, Value>,
    origin: (ChannelConversationId, &str),
) -> Sha256Digest {
    let mut value = Map::new();
    value.insert(
        "channel_connection_id".to_owned(),
        Value::String(connection_id.to_string()),
    );
    value.insert("provider".to_owned(), Value::String(provider.to_owned()));
    value.insert("tool".to_owned(), Value::String(tool.to_owned()));
    value.insert(
        "definition_sha256".to_owned(),
        Value::String(definition_sha256.to_hex()),
    );
    value.insert("arguments".to_owned(), Value::Object(arguments.clone()));
    value.insert("origin_conversation_id".to_owned(), Value::String(origin.0.to_string()));
    value.insert("origin_thread_key".to_owned(), Value::String(origin.1.to_owned()));
    digest_json(CHANNEL_CALL_DOMAIN, &Value::Object(value))
}

/// A memory tool call's digest: the memory type the run started with, its
/// kind and the tool, the definition it was decided against and its
/// arguments. An approval is bound to it, and a send checks it again, so
/// what's saved is exactly what was reviewed.
pub fn memory_call_digest(
    memory_type_id: MemoryTypeId,
    kind: &str,
    tool: &str,
    definition_sha256: &Sha256Digest,
    arguments: &Map<String, Value>,
) -> Sha256Digest {
    let mut value = Map::new();
    value.insert("memory_type_id".to_owned(), Value::String(memory_type_id.to_string()));
    value.insert("kind".to_owned(), Value::String(kind.to_owned()));
    value.insert("tool".to_owned(), Value::String(tool.to_owned()));
    value.insert(
        "definition_sha256".to_owned(),
        Value::String(definition_sha256.to_hex()),
    );
    value.insert("arguments".to_owned(), Value::Object(arguments.clone()));
    digest_json(MEMORY_CALL_DOMAIN, &Value::Object(value))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn object(value: Value) -> Map<String, Value> {
        match value {
            Value::Object(object) => object,
            _ => Map::new(),
        }
    }

    #[test]
    fn canonical_json_sorts_keys_at_every_depth() {
        let a = json!({ "b": 1, "a": { "z": [3, { "y": true, "x": null }], "m": "é\n\"" } });
        assert_eq!(
            canonical_json(&a),
            r#"{"a":{"m":"é\n\"","z":[3,{"x":null,"y":true}]},"b":1}"#
        );
        // Built in another order, the same bytes.
        let mut inner = Map::new();
        inner.insert("z".to_owned(), json!([3, { "x": null, "y": true }]));
        inner.insert("m".to_owned(), json!("é\n\""));
        let mut outer = Map::new();
        outer.insert("b".to_owned(), json!(1));
        outer.insert("a".to_owned(), Value::Object(inner));
        assert_eq!(canonical_json(&Value::Object(outer)), canonical_json(&a));
    }

    #[test]
    fn keys_sort_by_bytes_not_by_characters() {
        // "Z" (0x5A) sorts before "a" (0x61), and "é" (0xC3 0xA9) after both.
        assert_eq!(
            canonical_json(&json!({ "é": 1, "a": 2, "Z": 3 })),
            r#"{"Z":3,"a":2,"é":1}"#
        );
    }

    #[test]
    fn digests_round_trip_their_written_form() {
        let digest = Sha256Digest::of(b"zupflow");
        let written = digest.to_string();
        assert!(written.starts_with("sha256:"));
        assert_eq!(written.len(), 7 + 64);
        assert_eq!(Sha256Digest::parse(&written), Some(digest));
        assert_eq!(
            serde_json::from_value::<Sha256Digest>(serde_json::to_value(digest).expect("serializes"))
                .expect("deserializes"),
            digest
        );
        for bad in [
            "",
            "sha256:",
            &written.to_uppercase(),
            &written[7..],
            &format!("{written}0"),
            &written.replace("sha256:", "sha1:"),
        ] {
            assert_eq!(Sha256Digest::parse(bad), None, "{bad}");
        }
    }

    /// Floats with 17 significant digits, as a model may write them: parsed,
    /// stored as text and parsed again, they must give the same digest.
    const LONG_FLOATS: [&str; 4] = [
        "99982.865181082772",
        "0.30000000000000004",
        "1e-7",
        "-2.2250738585072014e-308",
    ];

    #[test]
    fn digests_survive_a_round_trip_through_storage() {
        let definition = Sha256Digest::of(b"definition");
        let installation = InstallationId::from_uuid(Uuid::from_u128(1));
        for float in LONG_FLOATS {
            let text = format!(r#"{{"amount":{float},"nested":{{"rate":[{float},1]}}}}"#);
            let first: Value = serde_json::from_str(&text).expect("parses");
            let stored = first.to_string();
            let second: Value = serde_json::from_str(&stored).expect("parses again");
            assert_eq!(canonical_json(&first), canonical_json(&second), "{float}");
            assert_eq!(
                call_digest(
                    installation,
                    "taskboard",
                    "create_task",
                    &definition,
                    &object(first.clone())
                ),
                call_digest(installation, "taskboard", "create_task", &definition, &object(second)),
                "{float}"
            );
            // Stored twice over, still the same.
            let third: Value = serde_json::from_str(&canonical_json(&first)).expect("parses");
            assert_eq!(canonical_json(&third), canonical_json(&first), "{float}");
        }
    }

    #[test]
    fn domains_separate_digests_of_the_same_value() {
        let value = json!({ "a": 1 });
        assert_ne!(digest_json("one", &value), digest_json("two", &value));
        assert_eq!(digest_json("one", &value), digest_json("one", &json!({ "a": 1 })));
    }

    #[test]
    fn a_call_digest_covers_every_part() {
        let installation = InstallationId::from_uuid(Uuid::from_u128(1));
        let other = InstallationId::from_uuid(Uuid::from_u128(2));
        let definition = Sha256Digest::of(b"definition");
        let arguments = object(json!({ "title": "Q3 notes", "tags": ["a", "b"] }));
        let base = call_digest(installation, "taskboard", "create_task", &definition, &arguments);
        // Deterministic, whatever order the arguments were built in.
        let reordered = object(json!({ "tags": ["a", "b"], "title": "Q3 notes" }));
        assert_eq!(
            call_digest(installation, "taskboard", "create_task", &definition, &reordered),
            base
        );
        let changed = [
            call_digest(other, "taskboard", "create_task", &definition, &arguments),
            call_digest(installation, "taskboard_two", "create_task", &definition, &arguments),
            call_digest(installation, "taskboard", "delete_task", &definition, &arguments),
            call_digest(
                installation,
                "taskboard",
                "create_task",
                &Sha256Digest::of(b"changed"),
                &arguments,
            ),
            call_digest(
                installation,
                "taskboard",
                "create_task",
                &definition,
                &object(json!({ "title": "Q4 notes", "tags": ["a", "b"] })),
            ),
            call_digest(installation, "taskboard", "create_task", &definition, &Map::new()),
        ];
        for digest in changed {
            assert_ne!(digest, base);
        }
    }

    #[test]
    fn a_memory_call_digest_covers_its_type_tool_definition_and_arguments() {
        let memory_type = MemoryTypeId::from_uuid(Uuid::from_u128(1));
        let definition = Sha256Digest::of(b"save_memory");
        let arguments = object(json!({ "text": "Prefers mornings." }));
        let base = memory_call_digest(memory_type, "notes", "save_memory", &definition, &arguments);
        assert_eq!(
            base,
            memory_call_digest(memory_type, "notes", "save_memory", &definition, &arguments),
            "the same call, the same digest"
        );
        let others = [
            memory_call_digest(
                MemoryTypeId::from_uuid(Uuid::from_u128(2)),
                "notes",
                "save_memory",
                &definition,
                &arguments,
            ),
            memory_call_digest(memory_type, "other", "save_memory", &definition, &arguments),
            memory_call_digest(memory_type, "notes", "update_memory", &definition, &arguments),
            memory_call_digest(
                memory_type,
                "notes",
                "save_memory",
                &Sha256Digest::of(b"other"),
                &arguments,
            ),
            memory_call_digest(
                memory_type,
                "notes",
                "save_memory",
                &definition,
                &object(json!({ "text": "Prefers evenings." })),
            ),
        ];
        for digest in others {
            assert_ne!(digest, base);
        }
    }

    #[test]
    fn a_channel_call_digest_covers_its_target_and_where_the_run_began() {
        let connection = ChannelConnectionId::from_uuid(Uuid::from_u128(1));
        let conversation = ChannelConversationId::from_uuid(Uuid::from_u128(2));
        let definition = Sha256Digest::of(b"reply");
        let arguments = object(json!({ "text": "On it." }));
        let base = channel_call_digest(
            connection,
            "slack",
            "reply",
            &definition,
            &arguments,
            (conversation, "1.0"),
        );
        let others = [
            channel_call_digest(
                ChannelConnectionId::from_uuid(Uuid::from_u128(9)),
                "slack",
                "reply",
                &definition,
                &arguments,
                (conversation, "1.0"),
            ),
            channel_call_digest(
                connection,
                "slack",
                "send_message",
                &definition,
                &arguments,
                (conversation, "1.0"),
            ),
            channel_call_digest(
                connection,
                "slack",
                "reply",
                &Sha256Digest::of(b"other"),
                &arguments,
                (conversation, "1.0"),
            ),
            channel_call_digest(
                connection,
                "slack",
                "reply",
                &definition,
                &object(json!({ "text": "On it!" })),
                (conversation, "1.0"),
            ),
            channel_call_digest(
                connection,
                "slack",
                "reply",
                &definition,
                &arguments,
                (ChannelConversationId::from_uuid(Uuid::from_u128(3)), "1.0"),
            ),
            channel_call_digest(
                connection,
                "slack",
                "reply",
                &definition,
                &arguments,
                (conversation, "2.0"),
            ),
        ];
        for other in others {
            assert_ne!(other, base);
        }
        // Never an installation call's digest over the same parts.
        let installation = InstallationId::from_uuid(Uuid::from_u128(1));
        assert_ne!(
            call_digest(installation, "slack", "reply", &definition, &arguments),
            base
        );
    }
}
