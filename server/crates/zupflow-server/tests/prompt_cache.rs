//! Prompt caching on every provider path: Anthropic, OpenAI, Claude through
//! OpenRouter and OpenRouter's other models, over the providers' APIs faked
//! in process (`zupflow_providers::fake`), whose caches answer what each
//! turn read from them.
//!
//! Within a run, every turn's request continues the one before: the same
//! tools and system, then every earlier message as it was sent
//! ([`check_prefix`]), with the breakpoints where the provider looks for
//! them ([`markers`]); each answered attempt read at least all of the one
//! before from the cache; and the run and the organization's usage count
//! those tokens. That holds through several rounds of tool calls, an
//! approval pause in which the installation is renamed, its grant narrowed
//! and another installation granted (the tool list is the one the run
//! started with; the call no longer granted is refused when the model makes
//! it; the next run sees the changes), a worker restart, a retry after a
//! provider error, and a key rotation.
//!
//! With memory, the recall is the first block of the conversation, with a
//! breakpoint of its own where the provider takes them (never more than
//! four in all), and the same bytes on every turn of a run: through a save
//! the run makes itself, a retry, a worker restart and an approval pause
//! with the memory changed meanwhile. Two runs with nothing changed share
//! the tools, the instructions and the recall in the cache; a run after a
//! change to the memory, or one started with memory `off`, still shares the
//! tools and the instructions (`off` sends the same tools and prompt cache
//! key, and no recall).

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::{PROVIDER_KEY, Server, call};
use faults::{Point, Worker};
use run_setup::{Setup, calling_memory, connect, id, setup_on};
use serde_json::{Value, json};
use zupflow_core::ids::RunId;
use zupflow_core::memory::RECALL_OPEN;
use zupflow_core::model_providers::ProviderKind;
use zupflow_core::runs::CallState;
use zupflow_providers::fake::prompt::{Caches, MAX_BREAKPOINTS, Prompt, at_breakpoints, check_prefix, markers};
use zupflow_providers::fake::{Fault, Received};

/// Every provider path, with a model it serves that calls tools.
const PATHS: [(ProviderKind, &str); 4] = [
    (ProviderKind::Anthropic, "claude-sonnet-4-5"),
    (ProviderKind::OpenAi, "gpt-5-mini"),
    (ProviderKind::OpenRouter, "anthropic/claude-sonnet-4.5"),
    (ProviderKind::OpenRouter, "openai/gpt-5-mini"),
];

/// A run's world on one path: [`Setup`] with `model` through `kind` as
/// Sandbox's default.
struct OnPath {
    setup: Setup,
    kind: ProviderKind,
    model: &'static str,
    connection: String,
}

fn key_of(kind: ProviderKind) -> String {
    match kind {
        ProviderKind::OpenAi => PROVIDER_KEY.to_owned(),
        _ => format!("sk-{}-0123456789abcdefWXYZ", kind.as_str()),
    }
}

async fn on(kind: ProviderKind, model: &'static str) -> OnPath {
    let setup = setup_on(Server::with_model_providers().await, &[("Main", "tk_main")]).await;
    let connection = if kind == ProviderKind::OpenAi && model == common::DEFAULT_MODEL {
        default_connection(&setup).await
    } else {
        route(&setup, kind, &key_of(kind), model).await
    };
    OnPath {
        setup,
        kind,
        model,
        connection,
    }
}

/// Connects `kind` in Sandbox with `key`, adds `model` from it and makes it
/// the default; the connection's ID.
async fn route(setup: &Setup, kind: ProviderKind, key: &str, model: &str) -> String {
    let (status, connection) = setup
        .kit
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{}/model-connections", setup.org),
            )
            .body(json!({
                "environment": "sandbox", "kind": kind.as_str(), "label": kind.name(), "api_key": key,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{connection}");
    let connection = id(&connection["data"]).to_owned();
    let (status, added) = setup
        .kit
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/models", setup.org))
                .body(json!({ "connection_id": connection, "provider_model_id": model })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/default-models/sandbox", setup.org),
            )
            .body(json!({ "model_id": id(&added["data"]) })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    connection
}

async fn default_connection(setup: &Setup) -> String {
    let (status, body) = setup
        .kit
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{}/default-models/sandbox", setup.org),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]["model"]["route"]["connection_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// One answered model attempt's tokens, as stored.
#[derive(Debug, Clone, Copy)]
struct Attempt {
    input: u64,
    read: u64,
    written: u64,
}

impl OnPath {
    fn name(&self) -> String {
        format!("{:?} {}", self.kind, self.model)
    }

    /// The turns sent so far.
    fn turns(&self) -> Vec<Received> {
        self.setup.server.models.turns()
    }

    async fn work(&self) {
        self.setup.kit.drain(Worker::A).await;
    }

    /// `call` lines for the first installation's tools, rounds split by `then`.
    async fn asking(&self, rounds: &[&[(&str, Value)]]) -> String {
        let mut lines = Vec::new();
        for (index, round) in rounds.iter().enumerate() {
            if index > 0 {
                lines.push("then".to_owned());
            }
            for (tool, arguments) in *round {
                lines.push(self.setup.calling(tool, &arguments.to_string()).await);
            }
        }
        lines.join("\n")
    }

    /// The run's answered model attempts, in order, as the store holds them.
    async fn answered(&self, run: RunId) -> Vec<Attempt> {
        let (org, ws) = self.setup.scope();
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite:{}",
            self.setup.server.directory.join("zupflow.sqlite").display()
        ))
        .await
        .unwrap();
        let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
            "SELECT input_tokens, cache_read_input_tokens, cache_write_input_tokens FROM run_model_attempts \
             WHERE organization_id = ? AND workspace_id = ? AND run_id = ? AND state = 'answered' \
             ORDER BY turn, attempt",
        )
        .bind(org.uuid())
        .bind(ws.uuid())
        .bind(run.uuid())
        .fetch_all(&pool)
        .await
        .unwrap();
        pool.close().await;
        rows.into_iter()
            .map(|(input, read, written)| Attempt {
                input: u64::try_from(input).unwrap(),
                read: u64::try_from(read).unwrap(),
                written: u64::try_from(written).unwrap(),
            })
            .collect()
    }

    /// The breakpoints a turn's body should carry on this path: after the
    /// tools and instructions (on the last tool, without instructions, where
    /// tools take one), on the recall, on the last block of the message
    /// before the latest answer, and on the last block of the last message.
    fn expected_markers(&self, body: &Value) -> Vec<String> {
        if !at_breakpoints(self.kind, self.model) {
            return Vec::new();
        }
        let mut messages: Vec<&Value> = body["messages"].as_array().unwrap().iter().collect();
        // Chat's system message is the prompt's system, apart from its messages.
        let instructed = match self.kind {
            ProviderKind::Anthropic => body.get("system").is_some(),
            _ => messages.first().is_some_and(|first| first["role"] == "system"),
        };
        if self.kind != ProviderKind::Anthropic {
            messages.retain(|message| message["role"] != "system");
        }
        let last_block = |index: usize| {
            let blocks = messages[index]["content"].as_array().unwrap().len();
            format!("messages[{index}].content[{}]", blocks - 1)
        };
        let mut expected = Vec::new();
        if instructed {
            expected.push("system[0]".to_owned());
        } else if self.kind == ProviderKind::Anthropic
            && let Some(tools) = body["tools"].as_array()
        {
            expected.push(format!("tools[{}]", tools.len() - 1));
        }
        if self.recall(body).is_some() {
            expected.push("messages[0].content[0]".to_owned());
        }
        if let Some(answer) = messages.iter().rposition(|message| message["role"] == "assistant") {
            expected.push(last_block(answer - 1));
        }
        expected.push(last_block(messages.len() - 1));
        expected
    }

    /// The recall a turn's body sent, breakpoints left out: the first block
    /// of its first message, framed. `None` without one.
    fn recall(&self, body: &Value) -> Option<Value> {
        let prompt = Prompt::read(self.kind, body).unwrap();
        let block = prompt.messages.first()?["content"].as_array()?.first()?.clone();
        block["text"]
            .as_str()
            .is_some_and(|text| text.starts_with(RECALL_OPEN))
            .then_some(block)
    }

    /// The tokens of what a turn's prompt opens with, as the provider counts
    /// them: its tools and system, then the first `blocks` blocks of its
    /// first message (`0`: none of it).
    fn opening(&self, body: &Value, blocks: usize) -> u64 {
        let mut opening = body.clone();
        let messages = opening["messages"].as_array_mut().unwrap();
        let system = match self.kind {
            ProviderKind::Anthropic => 0,
            _ => messages
                .iter()
                .take_while(|message| message["role"] == "system")
                .count(),
        };
        messages.truncate(system + usize::from(blocks > 0));
        if blocks > 0 {
            messages[system]["content"].as_array_mut().unwrap().truncate(blocks);
        }
        let prompt = Prompt::read(self.kind, &opening).unwrap();
        Caches::default().answer(self.kind, &opening, &prompt).input
    }

    /// The prompt as the provider caches it: tools, system and messages,
    /// breakpoints left out.
    fn prompt(&self, turn: &Received) -> Prompt {
        Prompt::read(self.kind, &turn.body).unwrap()
    }

    /// Every turn in `turns` continues the one before and marks its
    /// breakpoints where expected, and each answered attempt of `run` after
    /// its first read all of the one before from the cache. The tokens the
    /// run read from the cache.
    async fn assert_cached(&self, run: RunId, turns: &[Received]) -> u64 {
        let name = self.name();
        assert!(turns.len() >= 2, "{name}: {} turns", turns.len());
        for (index, pair) in turns.windows(2).enumerate() {
            check_prefix(self.kind, &pair[0].body, &pair[1].body)
                .unwrap_or_else(|why| panic!("{name}: turn {} breaks the prefix: {why}", index + 1));
        }
        for turn in turns {
            assert_eq!(
                markers(self.kind, &turn.body).unwrap(),
                self.expected_markers(&turn.body),
                "{name}: {}",
                turn.body
            );
            assert!(!turn.key_elsewhere, "{name}: the key is the credential only");
            match self.kind {
                ProviderKind::Anthropic => assert_eq!(turn.body.get("prompt_cache_key"), None),
                _ => {
                    let key = turn.body["prompt_cache_key"].as_str().unwrap();
                    assert!(key.starts_with("zf_") && key.len() == 35, "{name}: {key}");
                }
            }
        }
        let answered = self.answered(run).await;
        assert!(answered.len() >= 2, "{name}: {answered:?}");
        for pair in answered.windows(2) {
            assert!(
                pair[1].read >= pair[0].input,
                "{name}: an attempt read the turn before from the cache: {answered:?}"
            );
        }
        for attempt in &answered {
            assert!(attempt.read + attempt.written <= attempt.input, "{name}: {answered:?}");
            if !at_breakpoints(self.kind, self.model) {
                assert_eq!(attempt.written, 0, "{name}: caches on its own, writes nothing");
            }
        }
        if at_breakpoints(self.kind, self.model) {
            assert!(
                answered
                    .iter()
                    .all(|attempt| attempt.written > 0 || attempt.read == attempt.input),
                "{name}: each turn wrote what it added, unless it was all cached: {answered:?}"
            );
        }
        // The run counts what its turns did.
        let usage = self.setup.get(run).await["usage"].clone();
        let read: u64 = answered.iter().map(|attempt| attempt.read).sum();
        let written: u64 = answered.iter().map(|attempt| attempt.written).sum();
        let input: u64 = answered.iter().map(|attempt| attempt.input).sum();
        assert_eq!(
            (
                usage["input_tokens"].as_u64().unwrap(),
                usage["cache_read_input_tokens"].as_u64().unwrap(),
                usage["cache_write_input_tokens"].as_u64().unwrap(),
            ),
            (input, read, written),
            "{name}: {usage}"
        );
        read
    }

    /// The Sandbox usage totals of the last two days.
    async fn usage_totals(&self) -> Value {
        let (status, body) = self
            .setup
            .kit
            .send(call(
                "alice",
                Method::GET,
                format!("/v1/organizations/{}/usage?environment=sandbox&days=2", self.setup.org),
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"]["totals"].clone()
    }

    /// Approves the call `run` waits on, as the organization's backend would.
    async fn approve(&self, run: RunId) {
        let key = self.setup.api_key().await;
        let waiting = self
            .setup
            .calls(run)
            .await
            .into_iter()
            .find(|call| call.state == CallState::AwaitingApproval)
            .unwrap();
        let (status, body) = self
            .setup
            .kit
            .send(
                call(
                    &key,
                    Method::POST,
                    format!("/v1/workspaces/{}/runs/{run}/approvals", self.setup.ws),
                )
                .body(json!({ "call_id": waiting.id, "decision": "approved" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    /// The names of the tools a turn offered, and their descriptions.
    fn tools(&self, turn: &Received) -> Vec<(String, String)> {
        turn.body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                let tool = if self.kind == ProviderKind::Anthropic {
                    tool
                } else {
                    &tool["function"]
                };
                (
                    tool["name"].as_str().unwrap().to_owned(),
                    tool["description"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

#[tokio::test]
async fn continues_each_turn_through_rounds_of_tool_calls() {
    for (kind, model) in PATHS {
        let path = on(kind, model).await;
        let input = path
            .asking(&[
                &[("lookup", json!({ "n": 1 })), ("lookup", json!({ "n": 2 }))],
                &[("lookup", json!({ "n": 3 }))],
            ])
            .await;
        let before = path.turns().len();
        let run = path.setup.start(&input).await;
        path.work().await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        assert_eq!(finished["usage"]["tool_calls"], 3);
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 3, "{}: two rounds of calls, then the answer", path.name());
        let read = path.assert_cached(run, &turns).await;
        assert!(read > 0);
        // The organization's usage counts the same tokens.
        let totals = path.usage_totals().await;
        for (field, run_field) in [
            ("input_tokens", "input_tokens"),
            ("output_tokens", "output_tokens"),
            ("cache_read_input_tokens", "cache_read_input_tokens"),
            ("cache_write_input_tokens", "cache_write_input_tokens"),
        ] {
            assert_eq!(totals[field], finished["usage"][run_field], "{}: {field}", path.name());
        }
    }
}

/// The organization's usage counts a run on the day the server's clock
/// says it ran: its report's days are read on that clock too, never the
/// wall clock, so a run near midnight is never left out.
#[tokio::test]
async fn counts_usage_on_the_servers_own_days() {
    let (kind, model) = PATHS[0];
    let path = on(kind, model).await;
    // Two days past the wall clock: outside its two-day window entirely.
    path.setup.kit.clock.advance(Duration::from_secs(2 * 24 * 60 * 60));
    let input = path.asking(&[&[("lookup", json!({ "n": 1 }))]]).await;
    let run = path.setup.start(&input).await;
    path.work().await;
    let finished = path.setup.get(run).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    assert!(finished["usage"]["input_tokens"].as_u64().unwrap() > 0);
    let totals = path.usage_totals().await;
    assert_eq!(totals["input_tokens"], finished["usage"]["input_tokens"]);
    assert_eq!(totals["runs"], 1, "{totals}");
}

#[tokio::test]
async fn keeps_the_tool_list_through_an_approval_pause_a_rename_and_a_grant_edit() {
    for (kind, model) in PATHS {
        let path = on(kind, model).await;
        let setup = &path.setup;
        let main = setup.installations[0].1.clone();
        let input = path
            .asking(&[&[("send", json!({ "to": "ops" }))], &[("lookup", json!({ "n": 1 }))]])
            .await;
        let before = path.turns().len();
        let run = setup.start(&input).await;
        path.work().await;
        assert_eq!(setup.get(run).await["status"], "waiting_for_approval");

        // While it waits: the installation renamed, its grant narrowed to
        // `send`, and a second installation connected and granted.
        let (status, renamed) = setup
            .kit
            .send(
                call(
                    "alice",
                    Method::PATCH,
                    format!("/v1/workspaces/{}/installations/{main}", setup.ws),
                )
                .naming(&setup.org)
                .body(json!({ "label": "Renamed" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{renamed}");
        let grants = format!("/v1/workspaces/{}/agents/{}/grants", setup.ws, setup.agent);
        let (status, listed) = setup
            .kit
            .send(call("alice", Method::GET, grants.clone()).naming(&setup.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let grant = id(&listed["data"][0]).to_owned();
        let (status, narrowed) = setup
            .kit
            .send(
                call("alice", Method::PATCH, format!("{grants}/{grant}"))
                    .naming(&setup.org)
                    .body(json!({ "tools": ["send"], "approval_required": ["send"] })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{narrowed}");
        let second = connect(&setup.server, &setup.org, &setup.ws, &setup.app, "Second", "tk_second").await;
        setup.grant(&second).await;

        path.approve(run).await;
        path.work().await;
        let finished = setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 3, "{}", path.name());
        path.assert_cached(run, &turns).await;

        // Every turn offered the tools the run started with, described as
        // they were: the old label, no `lookup` taken away, nothing added.
        let started = path.tools(&turns[0]);
        assert_eq!(started.len(), 2, "{started:?}");
        assert!(started.iter().all(|(_, description)| description.contains("\"Main\"")));
        for turn in &turns {
            assert_eq!(path.tools(turn), started, "{}", path.name());
        }
        // The call to the tool no longer granted was refused when made.
        let calls = setup.calls(run).await;
        let states: Vec<CallState> = calls.iter().map(|call| call.state).collect();
        assert_eq!(states, [CallState::Completed, CallState::Refused], "{}", path.name());
        assert!(
            turns[2]
                .body
                .to_string()
                .contains("lookup isn't granted to this agent."),
            "{}: the model was told why: {}",
            path.name(),
            turns[2].body
        );

        // The next run sees the changes.
        let before = path.turns().len();
        let next = setup.start("hello").await;
        path.work().await;
        assert_eq!(setup.get(next).await["status"], "completed");
        let offered = path.tools(&path.turns()[before]);
        assert_eq!(offered.len(), 3, "{}: {offered:?}", path.name());
        let (main_lookup, _) = started.iter().find(|(name, _)| name.ends_with("_lookup")).unwrap();
        assert!(
            !offered.iter().any(|(name, _)| name == main_lookup),
            "no longer granted"
        );
        assert!(
            offered
                .iter()
                .any(|(_, description)| description.contains("\"Renamed\""))
        );
        assert!(
            offered
                .iter()
                .any(|(_, description)| description.contains("\"Second\""))
        );
    }
}

#[tokio::test]
async fn asks_again_with_the_same_bytes_after_a_worker_restart() {
    for (kind, model) in PATHS {
        let path = on(kind, model).await;
        let input = path
            .asking(&[&[("lookup", json!({ "n": 1 }))], &[("lookup", json!({ "n": 2 }))]])
            .await;
        let before = path.turns().len();
        let run = path.setup.start(&input).await;
        // A has the second turn's answer and dies before recording it; B
        // takes the run over and asks for that turn again.
        let takeover = path.setup.kit.take_over_at(Point::AfterModel, 2).await;
        takeover.a.crash().await;
        path.setup.kit.drain(Worker::B).await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 4, "{}: the lost turn asked for twice", path.name());
        assert_eq!(turns[1].raw, turns[2].raw, "{}: rebuilt byte for byte", path.name());
        path.assert_cached(run, &turns).await;
    }
}

#[tokio::test]
async fn tries_a_failed_turn_again_with_the_same_bytes() {
    for (kind, model) in PATHS {
        let path = on(kind, model).await;
        let input = path
            .asking(&[&[("lookup", json!({ "n": 1 }))], &[("lookup", json!({ "n": 2 }))]])
            .await;
        // The second turn fails once upstream.
        let models = path.setup.server.models.clone();
        let inner = models.clone();
        models.before_next_turn(async move {
            let failing = inner.clone();
            inner.before_next_turn(async move { failing.fail_next(&[Fault::Status(503)]) });
        });
        let before = path.turns().len();
        let run = path.setup.start(&input).await;
        path.work().await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        let statuses: Vec<Option<u16>> = turns.iter().map(|turn| turn.status).collect();
        assert_eq!(
            statuses,
            [Some(200), Some(503), Some(200), Some(200)],
            "{}",
            path.name()
        );
        assert_eq!(turns[1].raw, turns[2].raw, "{}: tried again byte for byte", path.name());
        path.assert_cached(run, &turns).await;
    }
}

#[tokio::test]
async fn keeps_the_request_through_a_key_rotation() {
    for (kind, model) in PATHS {
        let path = on(kind, model).await;
        let setup = &path.setup;
        let input = path
            .asking(&[&[("send", json!({ "to": "ops" }))], &[("lookup", json!({ "n": 1 }))]])
            .await;
        let before = path.turns().len();
        let run = setup.start(&input).await;
        path.work().await;
        assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
        // While it waits, the key is replaced and the old one revoked.
        let fresh = format!("sk-{}-rotated-0123456789abcd", kind.as_str());
        let (status, replaced) = setup
            .kit
            .send(
                call(
                    "alice",
                    Method::PUT,
                    format!(
                        "/v1/organizations/{}/model-connections/{}/api-key",
                        setup.org, path.connection
                    ),
                )
                .body(json!({ "api_key": fresh })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{replaced}");
        setup.server.models.revoke(&key_of(kind));
        path.approve(run).await;
        path.work().await;
        let finished = setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 3, "{}", path.name());
        let credential = |turn: &Received| turn.credential.as_ref().unwrap().1.clone();
        assert!(credential(&turns[0]).contains(&key_of(kind)));
        assert!(credential(&turns[2]).contains(&fresh), "{}: the new key", path.name());
        for turn in &turns {
            let raw = String::from_utf8_lossy(&turn.raw);
            assert!(
                !raw.contains(&key_of(kind)) && !raw.contains(&fresh),
                "no key in the prompt"
            );
        }
        path.assert_cached(run, &turns).await;
    }
}

// --- Memory ----------------------------------------------------------------------

/// [`on`], with the agent remembering in a `notes` type of its own whose
/// agents write at `writes`, and one note saved already: every run starts
/// with a recall. The type's ID.
async fn remembering(kind: ProviderKind, model: &'static str, writes: &str) -> (OnPath, String) {
    let path = on(kind, model).await;
    let memory_type = path.setup.with_memory(writes).await;
    path.setup.save_note("Prefers answers in Portuguese.").await;
    (path, memory_type)
}

impl OnPath {
    /// The recall every turn of `turns` sent, the same bytes on each.
    fn same_recall(&self, turns: &[Received]) -> String {
        let name = self.name();
        let recall = self
            .recall(&turns[0].body)
            .unwrap_or_else(|| panic!("{name}: no recall in {}", turns[0].body));
        for turn in turns {
            assert_eq!(self.recall(&turn.body).as_ref(), Some(&recall), "{name}");
        }
        recall["text"].as_str().unwrap().to_owned()
    }

    /// Starts and works a run for each input, with its memory (`agent` or
    /// `off`), one after another: each run, with the first model turn it
    /// sent.
    async fn first_turns(&self, inputs: &[(&str, &str)]) -> Vec<(RunId, Received)> {
        let mut first = Vec::new();
        for (input, memory) in inputs {
            let before = self.turns().len();
            let run = self.setup.start_with_memory(input, memory).await;
            self.work().await;
            let finished = self.setup.get(run).await;
            assert_eq!(finished["status"], "completed", "{}: {finished}", self.name());
            first.push((run, self.turns()[before].clone()));
        }
        first
    }
}

#[tokio::test]
async fn keeps_every_turn_a_continuation_through_a_save_the_run_makes() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "allow").await;
        let lookup = path.setup.calling("lookup", &json!({ "n": 1 }).to_string()).await;
        let save = calling_memory("save_memory", &json!({ "text": "Ships on Fridays." }));
        let before = path.turns().len();
        let run = path.setup.start(&format!("{save}\nthen\n{lookup}")).await;
        path.work().await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 3, "{}: the save, the lookup, then the answer", path.name());
        path.assert_cached(run, &turns).await;
        // Every turn recalls what the run started with, not what it saved.
        let recall = path.same_recall(&turns);
        assert!(
            recall.contains("Prefers answers in Portuguese.") && !recall.contains("Ships on Fridays."),
            "{recall}"
        );
        assert_eq!(path.setup.notes().await.len(), 2, "{}: saved", path.name());
        // The next run reads the change.
        let before = path.turns().len();
        let next = path.setup.start("hello").await;
        path.work().await;
        assert_eq!(path.setup.get(next).await["status"], "completed");
        let recalled = path.recall(&path.turns()[before].body).unwrap();
        assert!(recalled["text"].as_str().unwrap().contains("Ships on Fridays."));
    }
}

#[tokio::test]
async fn reads_the_tools_instructions_and_recall_a_run_before_sent_from_the_cache() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "allow").await;
        let one = path.asking(&[&[("lookup", json!({ "n": 1 }))]]).await;
        let two = path.asking(&[&[("lookup", json!({ "n": 2 }))]]).await;
        let first = path.first_turns(&[(&one, "agent"), (&two, "agent")]).await;
        let ((_, before), (second, next)) = (&first[0], &first[1]);
        let name = path.name();
        let (a, b) = (path.prompt(before), path.prompt(next));
        assert_eq!(a.tools, b.tools, "{name}");
        assert_eq!(a.system, b.system, "{name}");
        assert!(path.recall(&next.body).is_some(), "{name}");
        assert_eq!(path.recall(&before.body), path.recall(&next.body), "{name}");
        assert_eq!(before.body.get("prompt_cache_key"), next.body.get("prompt_cache_key"));
        // The second run's first turn read the three from the cache, and
        // nothing of its own input.
        let read = path.answered(*second).await[0];
        let shared = path.opening(&next.body, 1);
        assert!(read.read >= shared, "{name}: read {read:?}, shared {shared}");
        assert!(read.read < read.input, "{name}: {read:?}");
    }
}

#[tokio::test]
async fn reads_the_tools_and_instructions_from_the_cache_after_the_memory_changed() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "allow").await;
        let input = path.asking(&[&[("lookup", json!({ "n": 1 }))]]).await;
        let mut first = path.first_turns(&[(&input, "agent")]).await;
        path.setup.save_note("Ships on Fridays.").await;
        first.extend(path.first_turns(&[(&input, "agent")]).await);
        let ((_, before), (second, next)) = (&first[0], &first[1]);
        let name = path.name();
        let (a, b) = (path.prompt(before), path.prompt(next));
        assert_eq!(a.tools, b.tools, "{name}");
        assert_eq!(a.system, b.system, "{name}");
        assert_eq!(before.body.get("prompt_cache_key"), next.body.get("prompt_cache_key"));
        let (was, now) = (path.recall(&before.body).unwrap(), path.recall(&next.body).unwrap());
        assert_ne!(was, now, "{name}: the next run reads the change");
        let read = path.answered(*second).await[0];
        let head = path.opening(&next.body, 0);
        assert!(
            read.read >= head,
            "{name}: read {read:?}, tools and instructions {head}"
        );
        assert!(
            read.read < path.opening(&next.body, 1),
            "{name}: the changed recall can't be: {read:?}"
        );
    }
}

#[tokio::test]
async fn offers_the_same_tools_and_key_with_memory_off_and_reads_them_from_the_cache() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "allow").await;
        let input = path.asking(&[&[("lookup", json!({ "n": 1 }))]]).await;
        let first = path.first_turns(&[(&input, "agent"), (&input, "off")]).await;
        let ((_, with), (off, without)) = (&first[0], &first[1]);
        let name = path.name();
        let (a, b) = (path.prompt(with), path.prompt(without));
        assert_eq!(a.tools, b.tools, "{name}: the same tools, memory's among them");
        assert!(
            path.tools(without).iter().any(|(tool, _)| tool == "save_memory"),
            "{name}"
        );
        assert_eq!(a.system, b.system, "{name}");
        assert_eq!(with.body.get("prompt_cache_key"), without.body.get("prompt_cache_key"));
        assert!(path.recall(&with.body).is_some(), "{name}");
        assert_eq!(path.recall(&without.body), None, "{name}: nothing recalled");
        let read = path.answered(*off).await[0];
        let head = path.opening(&without.body, 0);
        assert!(
            read.read >= head,
            "{name}: read {read:?}, tools and instructions {head}"
        );
    }
}

#[tokio::test]
async fn sends_the_same_recall_when_a_failed_turn_is_tried_again() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "allow").await;
        let input = path
            .asking(&[&[("lookup", json!({ "n": 1 }))], &[("lookup", json!({ "n": 2 }))]])
            .await;
        // The second turn fails once upstream.
        let models = path.setup.server.models.clone();
        let inner = models.clone();
        models.before_next_turn(async move {
            let failing = inner.clone();
            inner.before_next_turn(async move { failing.fail_next(&[Fault::Status(503)]) });
        });
        let before = path.turns().len();
        let run = path.setup.start(&input).await;
        path.work().await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 4, "{}", path.name());
        assert_eq!(turns[1].raw, turns[2].raw, "{}: tried again byte for byte", path.name());
        path.same_recall(&turns);
        path.assert_cached(run, &turns).await;
    }
}

#[tokio::test]
async fn sends_the_same_recall_after_a_worker_restart() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "allow").await;
        let input = path
            .asking(&[&[("lookup", json!({ "n": 1 }))], &[("lookup", json!({ "n": 2 }))]])
            .await;
        let before = path.turns().len();
        let run = path.setup.start(&input).await;
        // A has the second turn's answer and dies before recording it, and
        // the memory changes before B takes the run over.
        let takeover = path.setup.kit.take_over_at(Point::AfterModel, 2).await;
        takeover.a.crash().await;
        path.setup.save_note("Ships on Fridays.").await;
        path.setup.kit.drain(Worker::B).await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 4, "{}: the lost turn asked for twice", path.name());
        assert_eq!(turns[1].raw, turns[2].raw, "{}: rebuilt byte for byte", path.name());
        let recall = path.same_recall(&turns);
        assert!(!recall.contains("Ships on Fridays."), "{recall}");
        path.assert_cached(run, &turns).await;
    }
}

#[tokio::test]
async fn sends_the_same_recall_through_an_approval_pause_with_the_memory_changed() {
    for (kind, model) in PATHS {
        let (path, _) = remembering(kind, model, "approve").await;
        let save = calling_memory("save_memory", &json!({ "text": "Ships on Fridays." }));
        let lookup = path.setup.calling("lookup", &json!({ "n": 1 }).to_string()).await;
        let before = path.turns().len();
        let run = path.setup.start(&format!("{save}\nthen\n{lookup}")).await;
        path.work().await;
        assert_eq!(path.setup.get(run).await["status"], "waiting_for_approval");
        // While it waits, a person saves a note too.
        path.setup.save_note("Closed on Mondays.").await;
        path.approve(run).await;
        path.work().await;
        let finished = path.setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
        let turns = path.turns().split_off(before);
        assert_eq!(turns.len(), 3, "{}", path.name());
        let recall = path.same_recall(&turns);
        assert!(
            !recall.contains("Closed on Mondays.") && !recall.contains("Ships on Fridays."),
            "{recall}"
        );
        path.assert_cached(run, &turns).await;
        assert_eq!(
            path.setup.notes().await.len(),
            3,
            "{}: saved once approved",
            path.name()
        );
    }
}

#[tokio::test]
async fn marks_at_most_four_breakpoints_on_every_turn() {
    for (kind, model) in PATHS {
        for instructions in ["Be brief.", ""] {
            let (path, _) = remembering(kind, model, "allow").await;
            let setup = &path.setup;
            let (status, body) = setup
                .kit
                .send(
                    call(
                        "alice",
                        Method::PATCH,
                        format!("/v1/workspaces/{}/agents/{}", setup.ws, setup.agent),
                    )
                    .naming(&setup.org)
                    .body(json!({ "instructions": instructions })),
                )
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let input = path
                .asking(&[
                    &[("lookup", json!({ "n": 1 })), ("lookup", json!({ "n": 2 }))],
                    &[("lookup", json!({ "n": 3 }))],
                    &[("lookup", json!({ "n": 4 }))],
                ])
                .await;
            let before = path.turns().len();
            let run = setup.start(&input).await;
            path.work().await;
            let finished = setup.get(run).await;
            assert_eq!(finished["status"], "completed", "{}: {finished}", path.name());
            let turns = path.turns().split_off(before);
            assert_eq!(turns.len(), 4, "{}", path.name());
            for turn in &turns {
                let marked = markers(kind, &turn.body).unwrap();
                assert!(marked.len() <= MAX_BREAKPOINTS, "{}: {marked:?}", path.name());
                assert_eq!(marked, path.expected_markers(&turn.body), "{}", path.name());
            }
            // Once the model answered, a path that marks the tools or the
            // instructions uses all four.
            if at_breakpoints(kind, model) && (kind == ProviderKind::Anthropic || !instructions.is_empty()) {
                for turn in &turns[1..] {
                    assert_eq!(
                        markers(kind, &turn.body).unwrap().len(),
                        MAX_BREAKPOINTS,
                        "{}",
                        path.name()
                    );
                }
            }
            path.assert_cached(run, &turns).await;
        }
    }
}
