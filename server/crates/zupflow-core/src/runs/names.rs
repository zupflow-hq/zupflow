//! The names tools reach a model by (decision 0047).
//!
//! Each granted tool of each installation is offered as
//! `<app_key>_<short_id>_<tool_part>`: the app's key (1 to 24 characters of
//! snake case), the installation's six-character short ID, and the tool's
//! pinned MCP name with every character outside `[A-Za-z0-9_-]` replaced by
//! `_`. The budget is fixed, so a name never passes 64 characters: the tool
//! part keeps up to 32, and a longer one keeps its first 27, then `_` and
//! the first 4 hex characters of the SHA-256 of the full tool name. The same
//! installation and tool always give the same name.
//!
//! A name is only ever looked up in the [`Offer`] it was built in, never
//! split on `_`, so an underscore in a key or a tool can't confuse anything.
//! [`build_offer`] checks the whole offer: when two entries would still
//! share a name, neither is offered and the clash is reported, so a lookup
//! never has to choose.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::apps::{APP_KEY_MAX, is_app_key};
use crate::ids::InstallationId;
use crate::installations::ShortId;
use crate::models::{MAX_MODEL_TOOL_NAME, ToolSpec};
use crate::runs::digest::Sha256Digest;

/// An installation's short ID is exactly this long ([`ShortId`]).
pub const SHORT_ID_LENGTH: usize = 6;

/// The longest tool part: what is left of 64 after the longest key, the
/// short ID and two separators.
pub const MAX_TOOL_PART: usize = MAX_MODEL_TOOL_NAME - APP_KEY_MAX - SHORT_ID_LENGTH - 2;

/// How much of a longer tool part is kept before its hash.
pub const KEPT_WHEN_CUT: usize = 27;

/// How many hex characters of the full tool name's SHA-256 end a cut part.
pub const HASH_CHARACTERS: usize = 4;

/// The tool part of a name: the tool's name with every character outside
/// `[A-Za-z0-9_-]` replaced by `_`, cut to [`MAX_TOOL_PART`] as described
/// above. Always ASCII.
pub fn tool_part(tool: &str) -> String {
    let sanitized: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() <= MAX_TOOL_PART {
        return sanitized;
    }
    // Sanitized, so ASCII: a byte is a character.
    let kept = &sanitized[..KEPT_WHEN_CUT];
    let hash = Sha256Digest::of(tool.as_bytes()).to_hex();
    format!("{kept}_{}", &hash[..HASH_CHARACTERS])
}

/// Why an entry can't be named, so isn't offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unnamed {
    /// The app's key isn't snake case of 1 to 24 characters.
    InvalidAppKey,
    /// The installation's short ID isn't six base32 characters.
    InvalidShortId,
    /// The tool has no name.
    EmptyTool,
    /// The tool's pinned input schema isn't a JSON object.
    SchemaNotObject,
}

/// The name a tool of an installation is offered by.
pub fn tool_name(app_key: &str, short_id: &str, tool: &str) -> Result<String, Unnamed> {
    if !is_app_key(app_key) {
        return Err(Unnamed::InvalidAppKey);
    }
    if ShortId::parse(short_id).is_none() {
        return Err(Unnamed::InvalidShortId);
    }
    if tool.is_empty() {
        return Err(Unnamed::EmptyTool);
    }
    Ok(format!("{app_key}_{short_id}_{}", tool_part(tool)))
}

/// What the model reads about a tool: the app's name, the installation's
/// label and short ID, then the pinned description.
pub fn describe(app_name: &str, label: &str, short_id: &str, description: &str) -> String {
    let lead = format!("{app_name}, installation \"{label}\" ({short_id}).");
    let description = description.trim();
    if description.is_empty() {
        lead
    } else {
        format!("{lead} {description}")
    }
}

/// One granted tool of one installation, as the driver read it, before it
/// is named.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub installation_id: InstallationId,
    pub installation_short_id: String,
    pub installation_label: String,
    pub app_key: String,
    pub app_name: String,
    /// The tool's original MCP name.
    pub tool: String,
    /// The pinned definition's description.
    pub description: String,
    /// The pinned definition's input schema: a JSON object.
    pub input_schema: Value,
    /// The SHA-256 of the pinned definition's canonical JSON, as the tool
    /// registry keeps it.
    pub definition_sha256: Sha256Digest,
}

/// A tool as one turn offers it: the name the model calls it by, what the
/// model reads about it, and where a call to it goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfferedTool {
    pub name: String,
    /// The full description the model reads, label and short ID first.
    pub description: String,
    pub input_schema: Value,
    pub installation_id: InstallationId,
    pub installation_short_id: String,
    pub installation_label: String,
    pub app_key: String,
    pub app_name: String,
    /// The original MCP tool name a call is sent under.
    pub tool: String,
    pub definition_sha256: Sha256Digest,
}

impl OfferedTool {
    /// The tool as the model is given it.
    pub fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }
}

/// Why a list of tools isn't an offer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OfferError {
    #[error("a tool name a model can't call")]
    InvalidName,
    #[error("a tool name offered twice")]
    DuplicateName,
}

/// The tools one turn offers, by name, frozen with the model attempt that
/// offered them (`PrepareModel`). A name the model uses is looked up here
/// and nowhere else.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<OfferedTool>", into = "Vec<OfferedTool>")]
pub struct Offer {
    /// Sorted by name; names unique.
    tools: Vec<OfferedTool>,
}

impl Offer {
    /// An offer of `tools`, as frozen: every name one a model can call, none
    /// twice. Sorted by name.
    pub fn new(mut tools: Vec<OfferedTool>) -> Result<Self, OfferError> {
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        for tool in &tools {
            if !valid_model_name(&tool.name) {
                return Err(OfferError::InvalidName);
            }
        }
        if tools.windows(2).any(|pair| pair[0].name == pair[1].name) {
            return Err(OfferError::DuplicateName);
        }
        Ok(Self { tools })
    }

    pub fn tools(&self) -> &[OfferedTool] {
        &self.tools
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// The tool offered by exactly `name`. Never parses the name.
    pub fn find(&self, name: &str) -> Option<&OfferedTool> {
        self.tools
            .binary_search_by(|tool| tool.name.as_str().cmp(name))
            .ok()
            .and_then(|index| self.tools.get(index))
    }

    /// The tools as the model is given them, in the offer's order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(OfferedTool::spec).collect()
    }
}

impl TryFrom<Vec<OfferedTool>> for Offer {
    type Error = OfferError;

    fn try_from(tools: Vec<OfferedTool>) -> Result<Self, Self::Error> {
        Self::new(tools)
    }
}

impl From<Offer> for Vec<OfferedTool> {
    fn from(offer: Offer) -> Self {
        offer.tools
    }
}

fn valid_model_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_MODEL_TOOL_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Two or more entries that would share a name: none of them is offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clash {
    pub name: String,
    /// Each entry's installation and original tool name.
    pub entries: Vec<(InstallationId, String)>,
}

/// An entry left out because it can't be named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Excluded {
    pub installation_id: InstallationId,
    pub tool: String,
    pub why: Unnamed,
}

/// An offer, with what was left out of it and why: for the driver to log.
#[derive(Debug, Clone, PartialEq)]
pub struct BuiltOffer {
    pub offer: Offer,
    pub clashes: Vec<Clash>,
    pub excluded: Vec<Excluded>,
}

/// Names every candidate and builds the turn's offer. Deterministic: the
/// same candidates, in any order, give the same offer and the same report.
/// A candidate repeated exactly is offered once; two different ones that
/// would share a name are both left out, as a clash.
pub fn build_offer(candidates: impl IntoIterator<Item = Candidate>) -> BuiltOffer {
    let mut by_name: BTreeMap<String, Vec<OfferedTool>> = BTreeMap::new();
    let mut excluded = Vec::new();
    for candidate in candidates {
        let named = tool_name(&candidate.app_key, &candidate.installation_short_id, &candidate.tool).and_then(|name| {
            if candidate.input_schema.is_object() {
                Ok(name)
            } else {
                Err(Unnamed::SchemaNotObject)
            }
        });
        let name = match named {
            Ok(name) => name,
            Err(why) => {
                excluded.push(Excluded {
                    installation_id: candidate.installation_id,
                    tool: candidate.tool,
                    why,
                });
                continue;
            }
        };
        let tool = OfferedTool {
            description: describe(
                &candidate.app_name,
                &candidate.installation_label,
                &candidate.installation_short_id,
                &candidate.description,
            ),
            name: name.clone(),
            input_schema: candidate.input_schema,
            installation_id: candidate.installation_id,
            installation_short_id: candidate.installation_short_id,
            installation_label: candidate.installation_label,
            app_key: candidate.app_key,
            app_name: candidate.app_name,
            tool: candidate.tool,
            definition_sha256: candidate.definition_sha256,
        };
        let entries = by_name.entry(name).or_default();
        if !entries.contains(&tool) {
            entries.push(tool);
        }
    }
    let mut tools = Vec::new();
    let mut clashes = Vec::new();
    for (name, mut entries) in by_name {
        if entries.len() == 1 {
            tools.append(&mut entries);
            continue;
        }
        let mut entries: Vec<(InstallationId, String)> = entries
            .into_iter()
            .map(|tool| (tool.installation_id, tool.tool))
            .collect();
        entries.sort();
        clashes.push(Clash { name, entries });
    }
    excluded.sort_by(|a, b| (a.installation_id, &a.tool, a.why).cmp(&(b.installation_id, &b.tool, b.why)));
    // Every name was built by `tool_name` and is unique here, so the offer
    // is always valid; an empty one is the safe answer if it weren't.
    let offer = Offer::new(tools).unwrap_or_default();
    BuiltOffer {
        offer,
        clashes,
        excluded,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::json;
    use uuid::Uuid;

    use super::*;

    fn installation(n: u128) -> InstallationId {
        InstallationId::from_uuid(Uuid::from_u128(n))
    }

    fn candidate(n: u128, key: &str, short_id: &str, label: &str, tool: &str) -> Candidate {
        Candidate {
            installation_id: installation(n),
            installation_short_id: short_id.to_owned(),
            installation_label: label.to_owned(),
            app_key: key.to_owned(),
            app_name: "Taskboard".to_owned(),
            tool: tool.to_owned(),
            description: "Create a task.".to_owned(),
            input_schema: json!({ "type": "object" }),
            definition_sha256: Sha256Digest::of(tool.as_bytes()),
        }
    }

    #[test]
    fn the_budget_matches_keys_and_short_ids() {
        assert_eq!(ShortId::from_bits(0).as_str().len(), SHORT_ID_LENGTH);
        assert_eq!(ShortId::from_bits(u32::MAX).as_str().len(), SHORT_ID_LENGTH);
        assert_eq!(APP_KEY_MAX + SHORT_ID_LENGTH + MAX_TOOL_PART + 2, MAX_MODEL_TOOL_NAME);
        // Any short ID the server issues names a tool.
        for bits in [0, 1, 0x3fff_ffff, 0x1234_5678] {
            let short_id = ShortId::from_bits(bits);
            assert!(tool_name("taskboard", short_id.as_str(), "create_task").is_ok());
        }
    }

    #[test]
    fn names_the_decisions_example() {
        assert_eq!(
            tool_name("taskboard", "k7m2qd", "create_task").as_deref(),
            Ok("taskboard_k7m2qd_create_task")
        );
        assert_eq!(
            tool_name("taskboard", "p3x7ta", "create_task").as_deref(),
            Ok("taskboard_p3x7ta_create_task")
        );
        assert_eq!(
            describe("Taskboard", "Work", "k7m2qd", "Create a task."),
            "Taskboard, installation \"Work\" (k7m2qd). Create a task."
        );
        assert_eq!(
            describe("Taskboard", "Work", "k7m2qd", "  "),
            "Taskboard, installation \"Work\" (k7m2qd)."
        );
    }

    #[test]
    fn sanitizes_the_tool_part() {
        let cases = [
            ("create_task", "create_task"),
            ("tasks.create", "tasks_create"),
            ("a__b", "a__b"),
            ("__leading", "__leading"),
            ("with-dash", "with-dash"),
            ("with space/and:colon", "with_space_and_colon"),
            ("créer_tâche", "cr_er_t_che"),
            ("タスク作成", "_____"),
            ("🚀launch", "_launch"),
            ("tab\tnew\nline", "tab_new_line"),
        ];
        for (tool, part) in cases {
            assert_eq!(tool_part(tool), part, "{tool}");
        }
    }

    #[test]
    fn cuts_a_long_tool_part_with_its_hash() {
        let tool = "list_all_calendar_events_with_attendees_and_rooms";
        let part = tool_part(tool);
        assert_eq!(part.len(), KEPT_WHEN_CUT + 1 + HASH_CHARACTERS);
        assert!(part.starts_with("list_all_calendar_events_wi_"), "{part}");
        assert_eq!(
            &part[KEPT_WHEN_CUT + 1..],
            &Sha256Digest::of(tool.as_bytes()).to_hex()[..4]
        );
        // Exactly the budget is kept whole; one more is cut.
        let exact = "a".repeat(MAX_TOOL_PART);
        assert_eq!(tool_part(&exact), exact);
        let over = "a".repeat(MAX_TOOL_PART + 1);
        assert_eq!(tool_part(&over).len(), MAX_TOOL_PART);
        assert_ne!(tool_part(&over), tool_part(&"a".repeat(MAX_TOOL_PART + 2)));
        // The hash is of the full, original name: two names that sanitize
        // alike but differ are still told apart when cut.
        let dotted = format!("{}.x", "b".repeat(40));
        let underscored = format!("{}_x", "b".repeat(40));
        assert_ne!(tool_part(&dotted), tool_part(&underscored));
    }

    #[test]
    fn every_name_fits_the_budget() {
        let key = "a".repeat(APP_KEY_MAX);
        let tools = [
            "x".to_owned(),
            "y".repeat(31),
            "y".repeat(32),
            "y".repeat(33),
            "z".repeat(500),
            "é".repeat(200),
            "🚀".repeat(100),
            "a.b".repeat(70),
        ];
        for tool in &tools {
            let name = tool_name(&key, "k7m2qd", tool).expect("named");
            assert!(name.len() <= MAX_MODEL_TOOL_NAME, "{name} is {} long", name.len());
            assert!(valid_model_name(&name), "{name}");
            // Deterministic.
            assert_eq!(tool_name(&key, "k7m2qd", tool), Ok(name));
        }
        assert_eq!(
            tool_name(&key, "k7m2qd", &"y".repeat(33)).map(|name| name.len()),
            Ok(MAX_MODEL_TOOL_NAME)
        );
    }

    #[test]
    fn refuses_what_it_cannot_name() {
        for key in [
            "",
            "Taskboard",
            "task-board",
            "_task",
            "task_",
            "task__board",
            "1task",
            &"a".repeat(25),
        ] {
            assert_eq!(tool_name(key, "k7m2qd", "t"), Err(Unnamed::InvalidAppKey), "{key:?}");
        }
        for key in ["a", "taskboard", "task_board_2", "a1_b2", &"a".repeat(24)] {
            assert!(is_app_key(key), "{key:?}");
        }
        for short_id in ["", "k7m2q", "k7m2qdx", "K7M2QD", "k1m2qd", "k8m2qd", "k7m2q_"] {
            assert_eq!(
                tool_name("taskboard", short_id, "t"),
                Err(Unnamed::InvalidShortId),
                "{short_id:?}"
            );
        }
        assert_eq!(tool_name("taskboard", "k7m2qd", ""), Err(Unnamed::EmptyTool));
    }

    #[test]
    fn two_installations_of_one_app_get_their_own_names() {
        let built = build_offer([
            candidate(1, "taskboard", "k7m2qd", "Work", "create_task"),
            candidate(2, "taskboard", "p3x7ta", "Personal", "create_task"),
        ]);
        assert!(built.clashes.is_empty());
        assert!(built.excluded.is_empty());
        let work = built.offer.find("taskboard_k7m2qd_create_task").expect("offered");
        assert_eq!(work.installation_id, installation(1));
        assert_eq!(work.tool, "create_task");
        assert_eq!(
            work.description,
            "Taskboard, installation \"Work\" (k7m2qd). Create a task."
        );
        let personal = built.offer.find("taskboard_p3x7ta_create_task").expect("offered");
        assert_eq!(personal.installation_id, installation(2));
        assert!(personal.description.contains("\"Personal\" (p3x7ta)"));
        assert_eq!(built.offer.find("create_task"), None);
        assert_eq!(built.offer.find("taskboard_k7m2qd_create_tas"), None);
    }

    #[test]
    fn the_same_candidates_in_any_order_give_the_same_offer() {
        let candidates = vec![
            candidate(1, "taskboard", "k7m2qd", "Work", "create_task"),
            candidate(1, "taskboard", "k7m2qd", "Work", "list_tasks"),
            candidate(2, "mail", "p3x7ta", "Personal", "send.email"),
            candidate(2, "mail", "p3x7ta", "Personal", "send_email"),
            candidate(3, "Bad-Key", "t4n6vb", "Team", "x"),
        ];
        let forward = build_offer(candidates.clone());
        let backward = build_offer(candidates.into_iter().rev());
        assert_eq!(forward, backward);
        let names: Vec<&str> = forward.offer.tools().iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(names, ["taskboard_k7m2qd_create_task", "taskboard_k7m2qd_list_tasks"]);
    }

    #[test]
    fn a_clash_offers_neither_and_reports_it() {
        let built = build_offer([
            candidate(2, "mail", "p3x7ta", "Personal", "send.email"),
            candidate(2, "mail", "p3x7ta", "Personal", "send_email"),
            candidate(2, "mail", "p3x7ta", "Personal", "archive"),
        ]);
        assert_eq!(built.offer.tools().len(), 1);
        assert_eq!(built.offer.find("mail_p3x7ta_send_email"), None);
        assert_eq!(
            built.clashes,
            vec![Clash {
                name: "mail_p3x7ta_send_email".to_owned(),
                entries: vec![
                    (installation(2), "send.email".to_owned()),
                    (installation(2), "send_email".to_owned()),
                ],
            }]
        );
    }

    #[test]
    fn a_clash_of_cut_names_offers_neither() {
        // Long names that share their first 27 characters and the first 4
        // hex characters of their hash: found by search, so real.
        let prefix = "x".repeat(KEPT_WHEN_CUT);
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let (first, second) = (0..1_000_000)
            .find_map(|n| {
                let tool = format!("{prefix}{n:08}");
                let part = tool_part(&tool);
                seen.insert(part, tool.clone()).map(|earlier| (earlier, tool))
            })
            .expect("a 16-bit prefix collides well within the range");
        assert_ne!(first, second);
        let built = build_offer([
            candidate(1, "taskboard", "k7m2qd", "Work", &first),
            candidate(1, "taskboard", "k7m2qd", "Work", &second),
        ]);
        assert!(built.offer.is_empty());
        assert_eq!(built.clashes.len(), 1);
        assert_eq!(built.clashes[0].entries.len(), 2);
    }

    #[test]
    fn an_exact_repeat_is_offered_once() {
        let built = build_offer([
            candidate(1, "taskboard", "k7m2qd", "Work", "create_task"),
            candidate(1, "taskboard", "k7m2qd", "Work", "create_task"),
        ]);
        assert_eq!(built.offer.tools().len(), 1);
        assert!(built.clashes.is_empty());
    }

    #[test]
    fn underscores_never_confuse_a_lookup() {
        // `a_b` + `c` and `a` + `b_c` can't meet: the short ID sits between.
        // Within one installation, names with `__` stay distinct and are
        // found only exactly.
        let built = build_offer([
            candidate(1, "a_b", "k7m2qd", "One", "c"),
            candidate(2, "a", "p3x7ta", "Two", "b_c"),
            candidate(1, "a_b", "k7m2qd", "One", "__c"),
            candidate(1, "a_b", "k7m2qd", "One", "c__"),
        ]);
        assert!(built.clashes.is_empty());
        let names: HashSet<&str> = built.offer.tools().iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            HashSet::from(["a_b_k7m2qd_c", "a_p3x7ta_b_c", "a_b_k7m2qd___c", "a_b_k7m2qd_c__"])
        );
        assert_eq!(
            built.offer.find("a_b_k7m2qd___c").map(|tool| tool.tool.as_str()),
            Some("__c")
        );
        assert_eq!(
            built.offer.find("a_b_k7m2qd_c__").map(|tool| tool.tool.as_str()),
            Some("c__")
        );
        assert_eq!(
            built.offer.find("a_p3x7ta_b_c").map(|tool| tool.installation_id),
            Some(installation(2))
        );
    }

    #[test]
    fn leaves_out_what_it_cannot_name() {
        let mut no_schema = candidate(1, "taskboard", "k7m2qd", "Work", "list_tasks");
        no_schema.input_schema = json!("not an object");
        let built = build_offer([
            candidate(1, "Taskboard", "k7m2qd", "Work", "create_task"),
            candidate(1, "taskboard", "K7M2QD", "Work", "create_task"),
            candidate(1, "taskboard", "k7m2qd", "Work", ""),
            no_schema,
        ]);
        assert!(built.offer.is_empty());
        let mut why: Vec<Unnamed> = built.excluded.iter().map(|excluded| excluded.why).collect();
        why.sort_by_key(|why| *why as u8);
        assert_eq!(
            why,
            [
                Unnamed::InvalidAppKey,
                Unnamed::InvalidShortId,
                Unnamed::EmptyTool,
                Unnamed::SchemaNotObject
            ]
        );
    }

    #[test]
    fn what_is_left_out_is_reported_in_one_order() {
        // One installation and tool left out twice, for two reasons: the
        // report is the same whatever order the candidates came in.
        let candidates = vec![
            candidate(1, "Bad", "k7m2qd", "Work", "create_task"),
            candidate(1, "taskboard", "BAD", "Work", "create_task"),
            candidate(1, "taskboard", "k7m2qd", "Work", ""),
        ];
        let forward = build_offer(candidates.clone()).excluded;
        let backward = build_offer(candidates.into_iter().rev()).excluded;
        assert_eq!(forward, backward);
        let why: Vec<Unnamed> = forward.iter().map(|excluded| excluded.why).collect();
        assert_eq!(
            why,
            [Unnamed::EmptyTool, Unnamed::InvalidAppKey, Unnamed::InvalidShortId]
        );
    }

    #[test]
    fn a_frozen_offer_round_trips_and_is_checked() {
        let built = build_offer([
            candidate(1, "taskboard", "k7m2qd", "Work", "create_task"),
            candidate(2, "taskboard", "p3x7ta", "Personal", "create_task"),
        ]);
        let json = serde_json::to_value(&built.offer).expect("serializes");
        let back: Offer = serde_json::from_value(json).expect("deserializes");
        assert_eq!(back, built.offer);
        let mut twice = built.offer.tools().to_vec();
        twice.push(twice[0].clone());
        assert_eq!(Offer::new(twice), Err(OfferError::DuplicateName));
        let mut bad = built.offer.tools().to_vec();
        bad[0].name = "has space".to_owned();
        assert_eq!(Offer::new(bad), Err(OfferError::InvalidName));
        let mut long = built.offer.tools().to_vec();
        long[0].name = "a".repeat(MAX_MODEL_TOOL_NAME + 1);
        assert_eq!(Offer::new(long), Err(OfferError::InvalidName));
    }

    #[test]
    fn specs_are_what_a_model_can_take() {
        let built = build_offer([
            candidate(1, "taskboard", "k7m2qd", "Work", "create_task"),
            candidate(1, "taskboard", "k7m2qd", "Work", &"long_name_".repeat(10)),
        ]);
        let specs = built.offer.specs();
        let messages = [crate::models::Message::User { text: "hi".to_owned() }];
        let turn = crate::models::Turn {
            model: "m",
            instructions: "",
            messages: &messages,
            tools: &specs,
        };
        assert_eq!(crate::models::validate_turn(&turn), Ok(()));
    }
}
