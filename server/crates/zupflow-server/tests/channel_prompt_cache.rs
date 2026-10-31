//! A channel session's prompt, turn by turn, on the Anthropic and OpenAI
//! paths, over the providers' APIs faked in process: every turn of every
//! run in a thread's session continues the turn before it, byte for byte,
//! through a second run, a steered turn, a collected follow-up, an
//! approval pause, a worker that dies mid-run, a run that calls a channel
//! tool, an edit noted in the next turn, and the agent, its connection and
//! its team renamed mid-session.
//! The one break is the trim, when the history would go over its budget:
//! once, there, and the runs after it continue again.
//!
//! With memory, a session is fixed with its memory as with its tools: every
//! run of it sends the same tools, instructions and recall, the recall once,
//! as the first block of the history, and the next run reads them from the
//! cache; a save in one run doesn't change what a later one recalls, and a
//! trim reads the recall anew from the session's own type, as a new session
//! does. A forget reaches the session at its next run instead, whoever
//! forgets, its history kept, and so does deleting its type (it recalls
//! nothing then, and its calls are refused); a run under way keeps the
//! recall it started with, and ends as it would have. Only the sessions of
//! the forget's agent, workspace and type read their recall anew, and of
//! those, for a direct conversation's note, only that conversation's; none
//! keeps the recall's text once no run of it is under way. A person's
//! change to a note reaches the session as a forget does; a run's own
//! change, at its next trim.
//! A `dm`'s or a `group_dm`'s session recalls, finds and saves its own
//! conversation's notes beside the workspace's, and nothing else sees them;
//! a channel's saves are the workspace's. Once a run of a session read an
//! app's tool result, a later save says it may come from it, a trim
//! included. No turn marks more than four breakpoints, and a session reads
//! and writes only its own organization, workspace, agent and memory type.
//! A recall takes no more than half the history budget, so one that would
//! fill it still leaves the session's earlier runs their room.

#![allow(clippy::unwrap_used)]

#[path = "common/channel_world.rs"]
mod channel_world;
mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use axum::http::{Method, StatusCode};
use channel_world::{ANA, BO, SUPPORT, TEAM, World, world};
use common::{PROVIDER_KEY, Server, call};
use faults::{Point, Worker};
use run_setup::{Setup, Tenant, calling_memory, id, setup_on};
use serde_json::{Value, json};
use zupflow_channels::fake::ChatMessage;
use zupflow_core::ids::{MemoryTypeId, RunId};
use zupflow_core::memory::RECALL_OPEN;
use zupflow_core::model_providers::ProviderKind;
use zupflow_core::runs::CallState;
use zupflow_core::runs::digest::Sha256Digest;
use zupflow_core::runs::state::CallRecord;
use zupflow_providers::fake::Received;
use zupflow_providers::fake::prompt::{MAX_BREAKPOINTS, Prompt, at_breakpoints, check_prefix, markers};

/// The paths, each with a model it serves that calls tools.
const PATHS: [(ProviderKind, &str); 2] = [
    (ProviderKind::Anthropic, "claude-sonnet-4-5"),
    (ProviderKind::OpenAi, "gpt-5-mini"),
];

/// A world whose Sandbox default is `model` through `kind`; its model's ID.
async fn on(kind: ProviderKind, model: &str) -> (World, String) {
    let setup = setup_on(Server::with_model_providers().await, &[("Main", "tk_main")]).await;
    let key = match kind {
        ProviderKind::OpenAi => PROVIDER_KEY.to_owned(),
        _ => format!("sk-{}-0123456789abcdefWXYZ", kind.as_str()),
    };
    let (status, connection) = setup
        .kit
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{}/model-connections", setup.org),
            )
            .body(json!({ "environment": "sandbox", "kind": kind.as_str(), "label": kind.name(), "api_key": key })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{connection}");
    let (status, added) = setup
        .kit
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{}/models", setup.org))
                .body(json!({ "connection_id": id(&connection["data"]), "provider_model_id": model })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{added}");
    let model_id = id(&added["data"]).to_owned();
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/default-models/sandbox", setup.org),
            )
            .body(json!({ "model_id": model_id })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (world(setup).await, model_id)
}

fn turns(world: &World) -> Vec<Received> {
    world.setup.server.models.turns()
}

async fn database(world: &World) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", world.setup.kit.database().display()))
        .await
        .unwrap()
}

/// Gives the model a small context window: a history budget of 3,300
/// tokens, which three [`long`] messages and their answers nearly fill.
async fn narrow(world: &World, model_id: &str) {
    let pool = database(world).await;
    let model = zupflow_core::ids::ModelId::parse(model_id).unwrap();
    let (org, _) = world.setup.scope();
    sqlx::query("UPDATE models SET context_window = 5500 WHERE organization_id = ? AND id = ?")
        .bind(org.uuid())
        .bind(model.uuid())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// A message of about 500 tokens, which the model answers by repeating.
fn long(n: u32) -> String {
    format!("{n} {}", "lorem ipsum ".repeat(166))
}

/// Which consecutive turns don't continue the one before, by index.
fn breaks(kind: ProviderKind, turns: &[Received]) -> Vec<(usize, String)> {
    turns
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            check_prefix(kind, &pair[0].body, &pair[1].body)
                .err()
                .map(|why| (index + 1, why))
        })
        .collect()
}

#[tokio::test]
async fn continues_a_session_through_runs_a_steer_a_collection_and_an_approval() {
    for (kind, model) in PATHS {
        let (world, _) = on(kind, model).await;
        let name = format!("{kind:?} {model}");
        let main = &world.main;
        let lookup = world.setup.calling("lookup", r#"{"n":1}"#).await;
        let send = world.setup.calling("send", r#"{"to":"ops"}"#).await;
        let before = turns(&world).len();

        // A run with a round of calls.
        let asked = world
            .say(ANA, &format!("{} look it up\n{lookup}", main.mention()), None)
            .await;
        world.work().await;
        // The next run in the thread.
        world.say(BO, "thanks", Some(&asked.ts)).await;
        world.work().await;
        // One that waits for approval, and takes in what arrives meanwhile.
        world.say(ANA, &format!("now tell ops\n{send}"), Some(&asked.ts)).await;
        world.work().await;
        let waiting = World::run_id(world.runs(main).await.last().unwrap());
        world.say(BO, "and cc finance", Some(&asked.ts)).await;
        world.handle().await;
        world.approve(waiting).await;
        world.work().await;
        // Collected: what arrives mid-run, answered after it.
        world.configure(main, json!({ "mid_run": "collect" })).await;
        world
            .say(ANA, &format!("tell ops again\n{send}"), Some(&asked.ts))
            .await;
        world.work().await;
        let waiting = World::run_id(world.runs(main).await.last().unwrap());
        world.say(BO, "and legal", Some(&asked.ts)).await;
        world.say(ANA, "and support", Some(&asked.ts)).await;
        world.handle().await;
        world.approve(waiting).await;
        world.work().await;

        let runs = world.runs(main).await;
        let statuses: Vec<&str> = runs.iter().map(|run| run["status"].as_str().unwrap()).collect();
        assert_eq!(statuses, ["completed"; 5], "{name}: {runs:?}");
        assert!(
            runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]),
            "{name}"
        );
        assert_eq!(
            world.taken_in(World::run_id(&runs[2])).await,
            [("started".to_owned(), 1), ("steered".to_owned(), 1)],
            "{name}"
        );
        assert_eq!(
            world.taken_in(World::run_id(&runs[4])).await,
            [("collected".to_owned(), 2)]
        );
        let sent = turns(&world).split_off(before);
        // Two turns for the calls and the answer, one, two around the
        // pause, two around the second pause, and the collected one.
        assert_eq!(sent.len(), 8, "{name}");
        assert_eq!(breaks(kind, &sent), [], "{name}: every turn continues the one before");
        // The steered turn is the one after the call's result, as written.
        let steered = String::from_utf8_lossy(&sent[4].raw).into_owned();
        assert!(
            steered.contains(r"New in the thread while you worked:\n[U0BO] and cc finance"),
            "{name}: {steered}"
        );
    }
}

#[tokio::test]
async fn asks_again_with_the_same_bytes_after_a_worker_restart_mid_session() {
    for (kind, model) in PATHS {
        let (world, _) = on(kind, model).await;
        let name = format!("{kind:?} {model}");
        let main = &world.main;
        let first = world.setup.calling("lookup", r#"{"n":1}"#).await;
        let second = world.setup.calling("lookup", r#"{"n":2}"#).await;
        let asked = world.say(ANA, &format!("{} hello", main.mention()), None).await;
        world.work().await;
        let before = turns(&world).len();
        world
            .say(BO, &format!("look these up\n{first}\nthen\n{second}"), Some(&asked.ts))
            .await;
        world.handle().await;
        // A has the run's second turn answered and dies before recording
        // it; B takes the run over and asks for that turn again.
        let takeover = world.setup.kit.take_over_at(Point::AfterModel, 2).await;
        takeover.a.crash().await;
        world.setup.kit.drain(Worker::B).await;
        let runs = world.runs(main).await;
        assert_eq!(runs[1]["status"], "completed", "{name}: {}", runs[1]);
        let sent = turns(&world).split_off(before - 1);
        assert_eq!(
            sent.len(),
            5,
            "{name}: the first run's turn, then the lost one asked for twice"
        );
        assert_eq!(sent[2].raw, sent[3].raw, "{name}: rebuilt byte for byte");
        assert_eq!(breaks(kind, &sent), [], "{name}");
    }
}

#[tokio::test]
async fn breaks_the_prefix_once_when_the_history_is_trimmed() {
    for (kind, model) in PATHS {
        let (world, model_id) = on(kind, model).await;
        let name = format!("{kind:?} {model}");
        // The fourth long message trims the first run away, and a short one
        // after fits.
        narrow(&world, &model_id).await;
        let main = &world.main;
        let before = turns(&world).len();
        let asked = world.say(ANA, &format!("{} {}", main.mention(), long(1)), None).await;
        world.work().await;
        for n in 2..=4 {
            world.say(ANA, &long(n), Some(&asked.ts)).await;
            world.work().await;
        }
        world.say(ANA, "ok", Some(&asked.ts)).await;
        world.work().await;
        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 5, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]));
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 5, "{name}: one turn a run");
        let broken: Vec<usize> = breaks(kind, &sent).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [3], "{name}: only the fourth run starts a new prefix");
        // From the trim on, the history is the second run onwards.
        let messages = |turn: &Received| {
            turn.body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] != "system")
                .count()
        };
        assert_eq!(messages(&sent[2]), 5, "{name}: two runs, then the third's input");
        assert_eq!(
            messages(&sent[3]),
            5,
            "{name}: the second and third runs, then the fourth's"
        );
        assert_eq!(messages(&sent[4]), 7, "{name}");
    }
}

#[tokio::test]
async fn continues_a_session_through_a_channel_tool_an_edit_and_a_rename() {
    for (kind, model) in PATHS {
        let (world, _) = on(kind, model).await;
        let name = format!("{kind:?} {model}");
        let main = &world.main;
        world.grant(main, &["reply", "react"]).await;
        let reply = world.calling(main, "reply", json!({ "text": "On it." })).await;
        let before = turns(&world).len();

        // A run that answers through its reply tool.
        let asked = world
            .say(ANA, &format!("{} is the build red?\n{reply}", main.mention()), None)
            .await;
        world.work().await;
        // The question edited, and the agent renamed, while the session
        // lasts.
        world
            .deliver(main, &world.chat.edit(&asked, "is the build green?", world.now()))
            .await;
        let (status, renamed) = world
            .setup
            .kit
            .send(
                call(
                    "alice",
                    Method::PATCH,
                    format!("/v1/workspaces/{}/agents/{}", world.setup.ws, main.agent),
                )
                .naming(&world.setup.org)
                .body(json!({ "name": "Release helper" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{renamed}");
        // Its connection relabelled, and the team renamed at the provider:
        // names people see, not the model.
        let (status, relabelled) = world
            .setup
            .kit
            .send(
                call(
                    "alice",
                    Method::PATCH,
                    format!("/v1/workspaces/{}/channel-connections/{}", main.ws, main.connection),
                )
                .naming(&world.setup.org)
                .body(json!({ "label": "Release desk" })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{relabelled}");
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", world.setup.kit.database().display()))
            .await
            .unwrap();
        sqlx::query("UPDATE channel_connections SET team_name = 'North Renamed' WHERE team_id = 'T0NORTH'")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let state = world.setup.kit.state(Worker::A);
        while zupflow_server::channels::work_once(&state).await {}
        world.say(BO, "thanks", Some(&asked.ts)).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 2, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert_eq!(runs[0]["session_id"], runs[1]["session_id"], "{name}");
        assert_eq!(world.chat.posted_by_apps().len(), 1, "{name}");
        let sent = turns(&world).split_off(before);
        // The call and the answer after it, then the follow-up.
        assert_eq!(sent.len(), 3, "{name}");
        assert_eq!(breaks(kind, &sent), [], "{name}: every turn continues the one before");
        // Its tools are the session's, offered unchanged on every turn.
        let tools = |turn: &Received| turn.body["tools"].clone();
        assert_eq!(tools(&sent[0]), tools(&sent[2]), "{name}");
        let offered = tools(&sent[0]).to_string();
        assert!(
            offered.contains("_reply") && offered.contains("stay_silent"),
            "{name}: {offered}"
        );
        // The edit is noted at the start of the next turn, and the turn the
        // model read before is as it was.
        let last = String::from_utf8_lossy(&sent[2].raw).into_owned();
        assert!(
            last.contains("[U0ANA edited an earlier message] is the build green?"),
            "{name}: {last}"
        );
        assert!(last.contains("is the build red?"), "{name}");
        assert!(
            !last.contains("Release helper"),
            "{name}: the session keeps its preface"
        );
        assert!(
            !offered.contains("Release desk") && !offered.contains("North Renamed"),
            "{name}: {offered}"
        );
    }
}

// --- Memory ------------------------------------------------------------------------

fn save(text: &str) -> String {
    calling_memory("save_memory", &json!({ "text": text }))
}

fn search(query: &str) -> String {
    calling_memory("search_memory", &json!({ "query": query }))
}

/// [`on`], with the agent remembering in a `notes` type of its own that
/// writes at `allow`, and one note saved, so every session has something to
/// recall: the world, its model's ID and the type's ID.
async fn remembering(kind: ProviderKind, model: &str) -> (World, String, String) {
    let (world, model_id) = on(kind, model).await;
    let memory_type = world.setup.with_memory("allow").await;
    world.setup.save_note("Prefers answers in Portuguese.").await;
    (world, model_id, memory_type)
}

/// The recall a turn's body sent: the first block of its first message,
/// framed, its breakpoint left out. `None` without one.
fn recall(kind: ProviderKind, body: &Value) -> Option<String> {
    let prompt = Prompt::read(kind, body).unwrap();
    let block = prompt.messages.first()?["content"].as_array()?.first()?.clone();
    block["text"]
        .as_str()
        .filter(|text| text.starts_with(RECALL_OPEN))
        .map(str::to_owned)
}

/// Each turn's recall, in order.
fn recalls(kind: ProviderKind, turns: &[Received]) -> Vec<Option<String>> {
    turns.iter().map(|turn| recall(kind, &turn.body)).collect()
}

/// The breakpoints a turn's body should carry on a path that takes them:
/// after the instructions (a session's are never empty: they open with its
/// preface), on the recall, on the last block of the message before the
/// latest answer, and on the last block of the last message. None on a path
/// that caches on its own.
fn expected_markers(kind: ProviderKind, model: &str, body: &Value) -> Vec<String> {
    if !at_breakpoints(kind, model) {
        return Vec::new();
    }
    let mut messages: Vec<&Value> = body["messages"].as_array().unwrap().iter().collect();
    if kind != ProviderKind::Anthropic {
        messages.retain(|message| message["role"] != "system");
    }
    let last_block = |index: usize| {
        let blocks = messages[index]["content"].as_array().unwrap().len();
        format!("messages[{index}].content[{}]", blocks - 1)
    };
    let mut expected = vec!["system[0]".to_owned()];
    if recall(kind, body).is_some() {
        expected.push("messages[0].content[0]".to_owned());
    }
    if let Some(answer) = messages.iter().rposition(|message| message["role"] == "assistant") {
        expected.push(last_block(answer - 1));
    }
    expected.push(last_block(messages.len() - 1));
    expected
}

/// One answered model attempt's tokens, as stored.
#[derive(Debug, Clone, Copy)]
struct Attempt {
    input: u64,
    read: u64,
}

/// The run's answered model attempts, in order.
async fn answered(world: &World, run: RunId) -> Vec<Attempt> {
    let (org, ws) = world.setup.scope();
    let pool = database(world).await;
    let rows: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT input_tokens, cache_read_input_tokens FROM run_model_attempts \
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
        .map(|(input, read)| Attempt {
            input: u64::try_from(input).unwrap(),
            read: u64::try_from(read).unwrap(),
        })
        .collect()
}

/// What a channel run keeps of its memory, and what its session keeps.
#[derive(Debug, PartialEq, Eq)]
struct Kept {
    memory_type_id: Option<MemoryTypeId>,
    /// The run's own copy of its recall: never one.
    recall: Option<String>,
    digest: Option<String>,
    /// The recall the session keeps for its current history start.
    session_recall: Option<String>,
}

async fn kept(world: &World, run: RunId) -> Kept {
    let (org, ws) = world.setup.scope();
    let pool = database(world).await;
    let (memory_type_id, recall, digest, session_recall): (
        Option<uuid::Uuid>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT r.memory_type_id, r.recall, r.recall_digest, s.recall FROM runs r \
         JOIN channel_sessions s ON s.organization_id = r.organization_id AND s.workspace_id = r.workspace_id \
         AND s.id = r.channel_session_id \
         WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?",
    )
    .bind(org.uuid())
    .bind(ws.uuid())
    .bind(run.uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    Kept {
        memory_type_id: memory_type_id.map(MemoryTypeId::from_uuid),
        recall,
        digest,
        session_recall,
    }
}

/// What a channel run of `memory_type` that sent `recall` keeps, in a
/// session that keeps `session_recall`.
fn keeping(memory_type: &str, recall: Option<&str>, session_recall: Option<&str>) -> Kept {
    Kept {
        memory_type_id: Some(MemoryTypeId::parse(memory_type).unwrap()),
        recall: None,
        digest: recall.map(|recall| Sha256Digest::of(recall.as_bytes()).to_string()),
        session_recall: session_recall.map(str::to_owned),
    }
}

/// The run's calls to its memory tools, in order.
async fn memory_calls(setup: &Setup, run: RunId) -> Vec<CallRecord> {
    setup
        .calls(run)
        .await
        .into_iter()
        .map(|call| call.record)
        .filter(|call| call.target.as_ref().is_some_and(|target| target.memory().is_some()))
        .collect()
}

/// What the model read back from each of the run's memory searches.
async fn found(setup: &Setup, run: RunId) -> Vec<String> {
    memory_calls(setup, run)
        .await
        .into_iter()
        .filter(|call| call.model_tool_name == "search_memory")
        .map(|call| {
            assert_eq!(call.state, CallState::Completed, "{call:?}");
            call.outcome.unwrap().content
        })
        .collect()
}

/// The texts of the agent's memories in `memory_type`, as the API lists
/// them to `tenant`'s user.
async fn texts(setup: &Setup, tenant: (&str, &str, &str, &str), memory_type: Option<&str>) -> Vec<String> {
    let (user, org, ws, agent) = tenant;
    let query = memory_type.map_or_else(String::new, |memory_type| format!("?memory_type={memory_type}"));
    let (status, body) = setup
        .kit
        .send(
            call(
                user,
                Method::GET,
                format!("/v1/workspaces/{ws}/agents/{agent}/memories{query}"),
            )
            .naming(org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| note["text"].as_str().unwrap().to_owned())
        .collect()
}

/// The setup's agent, as [`texts`] takes it.
fn own(setup: &Setup) -> (&str, &str, &str, &str) {
    ("alice", &setup.org, &setup.ws, &setup.agent)
}

/// Saves `text` for `agent` as `tenant`'s user, in `memory_type` or, left
/// out, the agent's own.
async fn save_for(setup: &Setup, tenant: (&str, &str, &str, &str), memory_type: Option<&str>, text: &str) {
    let (user, org, ws, agent) = tenant;
    let mut body = json!({ "text": text });
    if let Some(memory_type) = memory_type {
        body["memory_type_id"] = json!(memory_type);
    }
    let (status, created) = setup
        .kit
        .send(
            call(
                user,
                Method::POST,
                format!("/v1/workspaces/{ws}/agents/{agent}/memories"),
            )
            .naming(org)
            .body(body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
}

/// Has `tenant`'s agent remember in `memory_type`.
async fn remembers(setup: &Setup, tenant: &Tenant, memory_type: &str) {
    let (status, body) = setup
        .kit
        .send(
            call(
                &tenant.user,
                Method::PATCH,
                format!("/v1/workspaces/{}/agents/{}", tenant.ws, tenant.agent),
            )
            .naming(&tenant.org)
            .body(json!({ "memory": "type", "memory_type_id": memory_type })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn tenant_of(tenant: &Tenant) -> (&str, &str, &str, &str) {
    (&tenant.user, &tenant.org, &tenant.ws, &tenant.agent)
}

/// Ana posts `text` in the conversation `channel`, of Slack's
/// `channel_type`, to the setup's agent.
async fn post_in(world: &World, channel: &str, channel_type: &str, text: &str) -> ChatMessage {
    let message = world.chat.post(channel, channel_type, ANA, text, None, world.now());
    world.deliver(&world.main, &world.chat.delivery(&message)).await;
    message
}

/// Forgets the agent's `memory` through the API, as alice.
async fn forget(setup: &Setup, memory: &Value) {
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::DELETE,
                format!(
                    "/v1/workspaces/{}/agents/{}/memories/{}",
                    setup.ws,
                    setup.agent,
                    id(memory)
                ),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// Changes the agent's `memory`'s text through the API, as alice.
async fn change_text(setup: &Setup, memory: &Value, text: &str) {
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::PATCH,
                format!(
                    "/v1/workspaces/{}/agents/{}/memories/{}",
                    setup.ws,
                    setup.agent,
                    id(memory)
                ),
            )
            .naming(&setup.org)
            .body(json!({ "text": text })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Whether the session of the channel run `run`, of the setup's
/// organization, is marked to read its recall anew at its next run.
async fn refreshing(world: &World, run: &Value) -> bool {
    let (org, _) = world.setup.scope();
    let ws = zupflow_core::ids::WorkspaceId::parse(run["workspace_id"].as_str().unwrap()).unwrap();
    let pool = database(world).await;
    let marked: i64 = sqlx::query_scalar(
        "SELECT s.refresh_recall FROM runs r JOIN channel_sessions s ON s.organization_id = r.organization_id \
         AND s.workspace_id = r.workspace_id AND s.id = r.channel_session_id \
         WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?",
    )
    .bind(org.uuid())
    .bind(ws.uuid())
    .bind(World::run_id(run).uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    marked == 1
}

/// Whether the session of the channel run `run`, of the setup's
/// organization, keeps recall text.
async fn holds_recall(world: &World, run: &Value) -> bool {
    let (org, _) = world.setup.scope();
    let ws = zupflow_core::ids::WorkspaceId::parse(run["workspace_id"].as_str().unwrap()).unwrap();
    let pool = database(world).await;
    let held: i64 = sqlx::query_scalar(
        "SELECT s.recall IS NOT NULL FROM runs r JOIN channel_sessions s ON s.organization_id = r.organization_id \
         AND s.workspace_id = r.workspace_id AND s.id = r.channel_session_id \
         WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?",
    )
    .bind(org.uuid())
    .bind(ws.uuid())
    .bind(World::run_id(run).uuid())
    .fetch_one(&pool)
    .await
    .unwrap();
    pool.close().await;
    held == 1
}

/// Each of the organization's channel sessions with memory, by its
/// conversation's external ID, and whether it's marked to read its recall
/// anew.
async fn session_marks(world: &World) -> Vec<(String, bool)> {
    let (org, _) = world.setup.scope();
    let pool = database(world).await;
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT c.external_id, s.refresh_recall FROM channel_sessions s JOIN channel_conversations c \
         ON c.organization_id = s.organization_id AND c.workspace_id = s.workspace_id AND c.id = s.conversation_id \
         WHERE s.organization_id = ? AND s.memory_type_id IS NOT NULL ORDER BY c.external_id, s.thread_key",
    )
    .bind(org.uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    rows.into_iter()
        .map(|(conversation, marked)| (conversation, marked == 1))
        .collect()
}

/// What each of the organization's channel sessions keeps of its recall,
/// oldest first: the text, its weight in tokens, and whether its next run
/// is marked to read it anew.
async fn at_rest(world: &World) -> Vec<(Option<String>, i64, bool)> {
    let (org, _) = world.setup.scope();
    let pool = database(world).await;
    let rows: Vec<(Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT recall, recall_weight, refresh_recall FROM channel_sessions WHERE organization_id = ? \
         ORDER BY created_at, id",
    )
    .bind(org.uuid())
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;
    rows.into_iter()
        .map(|(recall, weight, marked)| (recall, weight, marked == 1))
        .collect()
}

/// The agent's last channel run, which must have completed.
async fn last_run(world: &World, name: &str) -> RunId {
    let runs = world.runs(&world.main).await;
    let run = runs.last().unwrap();
    assert_eq!(run["status"], "completed", "{name}: {run}");
    World::run_id(run)
}

#[tokio::test]
async fn sends_every_run_of_a_session_the_same_tools_instructions_and_recall_from_the_cache() {
    for (kind, model) in PATHS {
        let (world, _, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        let main = &world.main;
        let lookup = world.setup.calling("lookup", r#"{"n":1}"#).await;
        let before = turns(&world).len();
        // A run with a call, and two that follow up in the thread.
        let asked = world
            .say(ANA, &format!("{} look it up\n{lookup}", main.mention()), None)
            .await;
        world.work().await;
        world.say(BO, "thanks", Some(&asked.ts)).await;
        world.work().await;
        world.say(ANA, "and tomorrow?", Some(&asked.ts)).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 3, "{name}");
        for run in &runs {
            assert_eq!(run["status"], "completed", "{name}: {run}");
            assert_eq!(run["memory"], "agent", "{name}");
            assert_eq!(run["session_id"], runs[0]["session_id"], "{name}");
        }
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 4, "{name}: the call and its answer, then a turn a run");
        assert_eq!(breaks(kind, &sent), [], "{name}: every turn continues the one before");
        // Every turn offers the session's tools, its memory's among them,
        // after the same instructions, and opens with the same recall, once.
        let first = Prompt::read(kind, &sent[0].body).unwrap();
        let recalled = recall(kind, &sent[0].body).unwrap_or_else(|| panic!("{name}: no recall"));
        assert!(
            recalled.contains("Prefers answers in Portuguese."),
            "{name}: {recalled}"
        );
        for turn in &sent {
            let prompt = Prompt::read(kind, &turn.body).unwrap();
            assert_eq!(prompt.tools, first.tools, "{name}");
            assert_eq!(prompt.system, first.system, "{name}");
            assert_eq!(recall(kind, &turn.body).as_ref(), Some(&recalled), "{name}");
            let raw = String::from_utf8_lossy(&turn.raw);
            assert_eq!(raw.matches(RECALL_OPEN).count(), 1, "{name}: recalled once: {raw}");
        }
        let offered: String = first.tools.iter().map(Value::to_string).collect();
        for tool in [
            "save_memory",
            "search_memory",
            "update_memory",
            "forget_memory",
            "_lookup",
            "stay_silent",
        ] {
            assert!(offered.contains(tool), "{name}: {tool} in {offered}");
        }
        // Each attempt read all of the one before from the cache, the next
        // run's first too: the tools, the instructions, the recall and the
        // runs before it.
        let mut attempts = Vec::new();
        for run in &runs {
            attempts.extend(answered(&world, World::run_id(run)).await);
        }
        assert_eq!(attempts.len(), 4, "{name}");
        for pair in attempts.windows(2) {
            assert!(pair[1].read >= pair[0].input, "{name}: {attempts:?}");
        }
        // The session keeps the recall, once; each run keeps its type and
        // the recall's digest.
        for run in &runs {
            assert_eq!(
                kept(&world, World::run_id(run)).await,
                keeping(&memory_type, Some(&recalled), Some(&recalled)),
                "{name}"
            );
        }
    }
}

#[tokio::test]
async fn recalls_a_save_only_after_a_trim_or_in_a_new_session() {
    for (kind, model) in PATHS {
        let (world, model_id, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        narrow(&world, &model_id).await;
        let main = &world.main;
        let before = turns(&world).len();
        // The first run saves a note; the fifth long message trims the
        // history to the third run's, and a short one after fits.
        let asked = world
            .say(
                ANA,
                &format!("{} hello\n{}", main.mention(), save("Ships on Fridays.")),
                None,
            )
            .await;
        world.work().await;
        for n in 2..=5 {
            world.say(ANA, &long(n), Some(&asked.ts)).await;
            world.work().await;
        }
        // A person saves a note after the trim.
        world.setup.save_note("Closed on Mondays.").await;
        world.say(ANA, "ok", Some(&asked.ts)).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 6, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]));
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 7, "{name}: the save and its answer, then a turn a run");
        let broken: Vec<usize> = breaks(kind, &sent).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [5], "{name}: only the fifth run starts a new prefix");
        let recalled: Vec<String> = recalls(kind, &sent)
            .into_iter()
            .map(|recall| recall.unwrap_or_else(|| panic!("{name}: a turn without a recall")))
            .collect();
        // Until the trim, every turn recalls what the session began with,
        // not what its first run saved.
        assert!(recalled[..5].iter().all(|recall| recall == &recalled[0]), "{name}");
        assert!(
            recalled[0].contains("Prefers answers in Portuguese.") && !recalled[0].contains("Ships on Fridays."),
            "{name}: {}",
            recalled[0]
        );
        // The trim reads it again from the session's type, the save in it,
        // ahead of the third run's input; the run after it sends the same.
        assert!(
            recalled[5].contains("Prefers answers in Portuguese.") && recalled[5].contains("Ships on Fridays."),
            "{name}: {}",
            recalled[5]
        );
        assert_eq!(recalled[6], recalled[5], "{name}");
        let opening = Prompt::read(kind, &sent[5].body).unwrap().messages[0].to_string();
        assert!(opening.contains("3 lorem ipsum"), "{name}: {opening}");
        // The session keeps the trim's recall; each run, its own's digest.
        for (index, run) in runs.iter().enumerate() {
            let sent = if index < 4 { &recalled[0] } else { &recalled[5] };
            assert_eq!(
                kept(&world, World::run_id(run)).await,
                keeping(&memory_type, Some(sent), Some(&recalled[5])),
                "{name}: run {index}"
            );
        }

        // A new session reads it as it is now.
        let before = turns(&world).len();
        world.say(BO, &format!("{} hello", main.mention()), None).await;
        world.work().await;
        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 7, "{name}");
        assert_ne!(runs[6]["session_id"], runs[0]["session_id"], "{name}");
        let fresh = recall(kind, &turns(&world)[before].body).unwrap();
        assert!(
            fresh.contains("Ships on Fridays.") && fresh.contains("Closed on Mondays."),
            "{name}: {fresh}"
        );
    }
}

#[tokio::test]
async fn keeps_a_direct_conversations_notes_to_it_and_a_channels_to_the_workspace() {
    const ANAS: &str = "Ana's review is on Friday.";
    const TRIOS: &str = "The trio meets on Tuesdays.";
    const WORKSPACES: [&str; 3] = [
        "Prefers answers in Portuguese.",
        "Support closes at six.",
        "Payroll runs on the 25th.",
    ];
    for (kind, model) in PATHS {
        let (world, _, _) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        let main = &world.main;
        // Each place saves a note: Ana's dm, a group dm, a channel and a
        // private one.
        let dm = world
            .post(&[main], "D0ANA", ANA, &format!("remember\n{}", save(ANAS)), None)
            .await;
        world.work().await;
        let to_agent = |text: &str| format!("{} remember\n{}", main.mention(), save(text));
        post_in(&world, "G0TRIO", "mpim", &to_agent(TRIOS)).await;
        world.work().await;
        world.say(ANA, &to_agent(WORKSPACES[1]), None).await;
        world.work().await;
        post_in(&world, "G0PRIVATE", "group", &to_agent(WORKSPACES[2])).await;
        world.work().await;
        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 4, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        // A direct conversation's note is stamped with it; the others are
        // the workspace's.
        let (status, body) = world
            .setup
            .kit
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!(
                        "/v1/workspaces/{}/agents/{}/memories",
                        world.setup.ws, world.setup.agent
                    ),
                )
                .naming(&world.setup.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let stamped = |text: &str| {
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|note| note["text"] == text)
                .unwrap_or_else(|| panic!("{name}: {text} not saved: {body}"))["channel_conversation_id"]
                .clone()
        };
        assert_eq!(
            stamped(ANAS),
            json!(world.conversation_id(main, "D0ANA").await),
            "{name}"
        );
        assert_eq!(
            stamped(TRIOS),
            json!(world.conversation_id(main, "G0TRIO").await),
            "{name}"
        );
        for text in WORKSPACES {
            assert_eq!(stamped(text), Value::Null, "{name}: {text}");
        }

        // Then each place recalls and searches, in a session of its own (a
        // dm's top-level messages are one session, so Ana's in a thread of
        // it): Ana's dm and the group dm their own notes, nothing else the
        // others.
        let looking = format!("{}\n{}", search("review"), search("trio"));
        let asking = format!("{} look\n{looking}", main.mention());
        let mut seen = Vec::new();
        for (place, channel, channel_type, user, thread) in [
            ("Ana's dm", "D0ANA", "im", ANA, Some(dm.ts.as_str())),
            ("Bo's dm", "D0BO", "im", BO, None),
            ("the group dm", "G0TRIO", "mpim", ANA, None),
            ("a channel", "C0SUPPORT", "channel", ANA, None),
            ("a private channel", "G0PRIVATE", "group", ANA, None),
        ] {
            let before = turns(&world).len();
            let text = if channel_type == "im" {
                format!("look\n{looking}")
            } else {
                asking.clone()
            };
            let message = world.chat.post(channel, channel_type, user, &text, thread, world.now());
            world.deliver(main, &world.chat.delivery(&message)).await;
            world.work().await;
            let run = last_run(&world, &name).await;
            let recalled = recall(kind, &turns(&world)[before].body).unwrap();
            seen.push((place, recalled, found(&world.setup, run).await));
        }
        // An API run is the workspace's.
        let before = turns(&world).len();
        let run = world.setup.start(&looking).await;
        world.setup.kit.drain(Worker::A).await;
        assert_eq!(world.setup.get(run).await["status"], "completed", "{name}");
        let recalled = recall(kind, &turns(&world)[before].body).unwrap();
        seen.push(("an API run", recalled, found(&world.setup, run).await));

        for (place, recalled, found) in &seen {
            for text in WORKSPACES {
                assert!(recalled.contains(text), "{name}: {place} recalls {text}: {recalled}");
            }
            let mine = |text: &str| match text {
                ANAS => *place == "Ana's dm",
                _ => *place == "the group dm",
            };
            for (text, which) in [(ANAS, 0), (TRIOS, 1)] {
                assert_eq!(recalled.contains(text), mine(text), "{name}: {place}: {recalled}");
                assert_eq!(found[which].contains(text), mine(text), "{name}: {place}: {found:?}");
            }
        }

        // Forgetting Ana's dm's note marks only the sessions in Ana's dm,
        // the only ones that recall it; forgetting a workspace note marks
        // every session.
        let note = |text: &str| {
            body["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|note| note["text"] == text)
                .unwrap()
                .clone()
        };
        forget(&world.setup, &note(ANAS)).await;
        // A session for each save and each look: two in Ana's dm.
        let marks = session_marks(&world).await;
        assert_eq!(marks.len(), 9, "{name}: {marks:?}");
        assert!(
            marks
                .iter()
                .all(|(conversation, marked)| *marked == (conversation == "D0ANA")),
            "{name}: {marks:?}"
        );
        forget(&world.setup, &note(WORKSPACES[1])).await;
        let marks = session_marks(&world).await;
        assert!(marks.iter().all(|(_, marked)| *marked), "{name}: {marks:?}");
    }
}

#[tokio::test]
async fn marks_every_later_save_of_a_session_once_a_run_read_an_apps_result_a_trim_included() {
    for (kind, model) in PATHS {
        let (world, model_id, _) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        narrow(&world, &model_id).await;
        let main = &world.main;
        let lookup = world.setup.calling("lookup", r#"{"n":1}"#).await;
        let before = turns(&world).len();
        // A save before any app's result was read; a run that reads one; a
        // save in the run after it.
        let asked = world
            .say(
                ANA,
                &format!("{} hello\n{}", main.mention(), save("Saved first.")),
                None,
            )
            .await;
        world.work().await;
        world.say(ANA, &format!("look it up\n{lookup}"), Some(&asked.ts)).await;
        world.work().await;
        world
            .say(
                ANA,
                &format!("note it\n{}", save("Saved after the lookup.")),
                Some(&asked.ts),
            )
            .await;
        world.work().await;
        // Long runs, until the history is trimmed past the run that read
        // the result: a save then says so still.
        for n in 4..=6 {
            world.say(ANA, &long(n), Some(&asked.ts)).await;
            world.work().await;
        }
        world
            .say(
                ANA,
                &format!("{}\n{}", long(7), save("Saved after the trim.")),
                Some(&asked.ts),
            )
            .await;
        world.work().await;
        // Another thread's session read no app's result.
        world
            .say(
                BO,
                &format!("{} hello\n{}", main.mention(), save("Saved elsewhere.")),
                None,
            )
            .await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 8, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        let sent = turns(&world).split_off(before);
        // Two turns for each of the three runs with a call, one for each
        // long one, two for the trimmed one and two in the other thread.
        assert_eq!(sent.len(), 13, "{name}");
        let broken: Vec<usize> = breaks(kind, &sent[..11]).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [9], "{name}: the seventh run is trimmed");
        let opening = Prompt::read(kind, &sent[9].body).unwrap().messages[0].to_string();
        assert!(opening.contains("5 lorem ipsum"), "{name}: past the lookup: {opening}");
        let notes = world.setup.notes().await;
        for (text, after_app_content) in [
            ("Saved first.", false),
            ("Saved after the lookup.", true),
            ("Saved after the trim.", true),
            ("Saved elsewhere.", false),
        ] {
            let note = notes
                .iter()
                .find(|note| note["text"] == text)
                .unwrap_or_else(|| panic!("{name}: {text} not saved: {notes:?}"));
            assert_eq!(note["origin"]["type"], "run", "{name}: {note}");
            assert_eq!(note["origin"]["after_app_content"], after_app_content, "{name}: {text}");
        }
    }
}

#[tokio::test]
async fn marks_at_most_four_breakpoints_on_every_turn_of_every_run_of_a_session() {
    let paths = [
        PATHS[0],
        PATHS[1],
        (ProviderKind::OpenRouter, "anthropic/claude-sonnet-4.5"),
    ];
    for (kind, model) in paths {
        let (world, model_id, _) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        narrow(&world, &model_id).await;
        let main = &world.main;
        let mut lookups = Vec::new();
        for n in 1..=3 {
            lookups.push(world.setup.calling("lookup", &json!({ "n": n }).to_string()).await);
        }
        let before = turns(&world).len();
        // Rounds of calls, a save, follow-ups and a trim.
        let asked = world
            .say(
                ANA,
                &format!(
                    "{} look these up\n{}\n{}\nthen\n{}",
                    main.mention(),
                    lookups[0],
                    lookups[1],
                    lookups[2]
                ),
                None,
            )
            .await;
        world.work().await;
        world
            .say(
                BO,
                &format!("note it\n{}\nthen\n{}", save("Ships on Fridays."), lookups[0]),
                Some(&asked.ts),
            )
            .await;
        world.work().await;
        for n in 1..=4 {
            world.say(ANA, &long(n), Some(&asked.ts)).await;
            world.work().await;
        }
        world.say(ANA, &format!("again\n{}", lookups[1]), Some(&asked.ts)).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 7, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]));
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 12, "{name}");
        assert_eq!(breaks(kind, &sent).len(), 1, "{name}: the trim");
        for (index, turn) in sent.iter().enumerate() {
            let marked = markers(kind, &turn.body).unwrap();
            assert!(marked.len() <= MAX_BREAKPOINTS, "{name}: turn {index}: {marked:?}");
            assert_eq!(
                marked,
                expected_markers(kind, model, &turn.body),
                "{name}: turn {index}"
            );
            // Once the model answered, a path that takes them uses all four.
            if at_breakpoints(kind, model) && index > 0 {
                assert_eq!(marked.len(), MAX_BREAKPOINTS, "{name}: turn {index}");
            }
            assert!(recall(kind, &turn.body).is_some(), "{name}: turn {index}");
        }
    }
}

#[tokio::test]
async fn reads_and_writes_only_its_own_organization_workspace_agent_and_type() {
    const THEIRS: [&str; 4] = [
        "The sibling workspace's note.",
        "The other agent's note.",
        "Another organization's note.",
        "The agent's note in another type.",
    ];
    for (kind, model) in PATHS {
        let (world, _, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        let setup = &world.setup;
        let main = &world.main;
        save_for(setup, own(setup), None, "The agent's own note.").await;
        // The same type's notes of an agent of a sibling workspace and of
        // another agent of the workspace; another organization's; and the
        // agent's own in another type.
        let neighbour = setup.sibling("neighbour", "tk_neighbour").await;
        remembers(setup, &neighbour, &memory_type).await;
        save_for(setup, tenant_of(&neighbour), None, THEIRS[0]).await;
        let (status, other_agent) = setup
            .kit
            .send(
                call("alice", Method::POST, format!("/v1/workspaces/{}/agents", setup.ws))
                    .naming(&setup.org)
                    .body(json!({
                        "name": "Other", "description": "", "instructions": "",
                        "memory": "type", "memory_type_id": memory_type,
                    })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{other_agent}");
        let other_agent = id(&other_agent["data"]).to_owned();
        let other = ("alice", setup.org.as_str(), setup.ws.as_str(), other_agent.as_str());
        save_for(setup, other, None, THEIRS[1]).await;
        let elsewhere = setup.other_organization("bob", "larkspur", "tk_larkspur").await;
        let (status, created) = setup
            .kit
            .send(
                call(
                    "bob",
                    Method::POST,
                    format!("/v1/organizations/{}/memory-types", elsewhere.org),
                )
                .body(json!({ "name": "Customer notes", "kind": "notes", "settings": {} })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{created}");
        remembers(setup, &elsewhere, id(&created["data"])).await;
        save_for(setup, tenant_of(&elsewhere), None, THEIRS[2]).await;
        let other_type = setup.notes_type("Order notes", "allow").await;
        save_for(setup, own(setup), Some(&other_type), THEIRS[3]).await;

        // A session begins in the agent's type. While its run waits for
        // approval, the agent moves to the other type, and what arrives in
        // the thread meanwhile is collected into the session's next run.
        world.configure(main, json!({ "mid_run": "collect" })).await;
        let send = setup.calling("send", r#"{"to":"ops"}"#).await;
        let before = turns(&world).len();
        let asked = world
            .say(
                ANA,
                &format!(
                    "{} tell ops\n{send}\nthen\n{}\n{}",
                    main.mention(),
                    save("Saved in the session."),
                    search("note")
                ),
                None,
            )
            .await;
        world.work().await;
        let waiting = World::run_id(world.runs(main).await.last().unwrap());
        setup.remember_in(&other_type).await;
        world
            .say(
                BO,
                &format!("and this\n{}\n{}", save("Saved later in the session."), search("note")),
                Some(&asked.ts),
            )
            .await;
        world.handle().await;
        world.approve(waiting).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 2, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert_eq!(runs[0]["session_id"], runs[1]["session_id"], "{name}");
        let sent = turns(&world).split_off(before);
        let recalled = recalls(kind, &sent);
        assert!(recalled.iter().all(|recall| recall == &recalled[0]), "{name}");
        let recalled = recalled[0].clone().unwrap();
        assert!(
            recalled.contains("Prefers answers in Portuguese.") && recalled.contains("The agent's own note."),
            "{name}: {recalled}"
        );
        // Both runs called the session's type, found only its own agent's
        // notes there, and recalled only those.
        let session_type = MemoryTypeId::parse(&memory_type).unwrap();
        for run in &runs {
            let run = World::run_id(run);
            let calls = memory_calls(setup, run).await;
            assert_eq!(calls.len(), 2, "{name}: {calls:?}");
            for call in &calls {
                assert_eq!(call.state, CallState::Completed, "{name}: {call:?}");
                assert_eq!(call.target.as_ref().unwrap().memory(), Some(session_type), "{name}");
            }
            let found = found(setup, run).await;
            assert!(found[0].contains("The agent's own note."), "{name}: {found:?}");
            for theirs in THEIRS {
                assert!(!recalled.contains(theirs), "{name}: {recalled}");
                assert!(!found[0].contains(theirs), "{name}: {found:?}");
            }
        }
        // It saved for its own agent, in its own type, only.
        let kept = texts(setup, own(setup), Some(&memory_type)).await;
        for text in ["Saved in the session.", "Saved later in the session."] {
            assert!(kept.iter().any(|kept| kept == text), "{name}: {kept:?}");
        }
        assert_eq!(texts(setup, own(setup), Some(&other_type)).await, [THEIRS[3]], "{name}");
        assert_eq!(texts(setup, tenant_of(&neighbour), None).await, [THEIRS[0]], "{name}");
        assert_eq!(texts(setup, other, None).await, [THEIRS[1]], "{name}");
        assert_eq!(texts(setup, tenant_of(&elsewhere), None).await, [THEIRS[2]], "{name}");

        // A new session takes the agent's memory as it is now.
        let before = turns(&world).len();
        world.say(ANA, &format!("{} hello", main.mention()), None).await;
        world.work().await;
        let fresh = recall(kind, &turns(&world)[before].body).unwrap();
        assert!(
            fresh.contains(THEIRS[3]) && !fresh.contains("The agent's own note."),
            "{name}: {fresh}"
        );
    }
}

#[tokio::test]
async fn forgets_a_note_at_a_sessions_next_run_whoever_forgets_it_its_history_kept() {
    const NOTES: [&str; 3] = ["Ships on Fridays.", "Closed on Mondays.", "Opens at nine."];
    for (kind, model) in PATHS {
        let (world, _, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        let setup = &world.setup;
        let main = &world.main;
        let mut saved = Vec::new();
        for text in NOTES {
            saved.push(setup.save_note(text).await);
        }
        let send = setup.calling("send", r#"{"to":"ops"}"#).await;
        let forgetting = calling_memory("forget_memory", &json!({ "memory": saved[0]["short_id"] }));
        let before = turns(&world).len();
        // The first run forgets a note itself, then answers.
        let asked = world
            .say(ANA, &format!("{} hello\n{forgetting}", main.mention()), None)
            .await;
        world.work().await;
        // The second waits for approval while a person forgets another.
        world.say(ANA, &format!("tell ops\n{send}"), Some(&asked.ts)).await;
        world.work().await;
        let waiting = World::run_id(world.runs(main).await.last().unwrap());
        forget(setup, &saved[1]).await;
        let rest = at_rest(&world).await;
        assert_eq!(rest.len(), 1, "{name}: {rest:?}");
        assert!(
            rest[0].0.as_ref().is_some_and(|recall| recall.contains(NOTES[1])) && rest[0].2,
            "{name}: the run under way keeps its recall: {rest:?}"
        );
        world.approve(waiting).await;
        world.work().await;
        // Its session keeps no recall text once it ends, only its weight.
        let rest = at_rest(&world).await;
        assert!(rest[0].0.is_none() && rest[0].1 > 0 && rest[0].2, "{name}: {rest:?}");
        // The third follows; then, with the session idle, a person forgets
        // the last, and two more runs follow.
        world.say(BO, "thanks", Some(&asked.ts)).await;
        world.work().await;
        let rest = at_rest(&world).await;
        assert!(
            rest[0].0.as_ref().is_some_and(|recall| recall.contains(NOTES[2])) && !rest[0].2,
            "{name}: {rest:?}"
        );
        forget(setup, &saved[2]).await;
        // The idle session keeps no recall text from the forget on.
        let rest = at_rest(&world).await;
        assert!(rest[0].0.is_none() && rest[0].1 > 0 && rest[0].2, "{name}: {rest:?}");
        for text in ["and tomorrow?", "ok"] {
            world.say(ANA, text, Some(&asked.ts)).await;
            world.work().await;
        }

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 5, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(
            runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]),
            "{name}"
        );
        let forgot = memory_calls(setup, World::run_id(&runs[0])).await;
        assert_eq!(forgot.len(), 1, "{name}: {forgot:?}");
        assert_eq!(forgot[0].state, CallState::Completed, "{name}: {forgot:?}");
        let left: Vec<Value> = setup.notes().await;
        assert_eq!(left.len(), 1, "{name}: {left:?}");
        let sent = turns(&world).split_off(before);
        assert_eq!(
            sent.len(),
            7,
            "{name}: two turns for each run with a call, then a turn a run"
        );
        // The prefix breaks once for each run that reads the recall anew:
        // the run after each forget, a run under way keeping its own. Every
        // other turn continues the one before, byte for byte.
        let broken: Vec<usize> = breaks(kind, &sent).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [2, 4, 5], "{name}");
        let recalled: Vec<String> = recalls(kind, &sent)
            .into_iter()
            .map(|recall| recall.unwrap_or_else(|| panic!("{name}: a turn without a recall")))
            .collect();
        let held = [
            [true, true, true],
            // The first run's own forget reaches the run after it.
            [true, true, true],
            [false, true, true],
            // The person's forget, while the second waited, too.
            [false, true, true],
            [false, false, true],
            [false, false, false],
            [false, false, false],
        ];
        for (index, (recall, held)) in recalled.iter().zip(held).enumerate() {
            assert!(
                recall.contains("Prefers answers in Portuguese."),
                "{name}: turn {index}: {recall}"
            );
            for (note, held) in NOTES.iter().zip(held) {
                assert_eq!(recall.contains(note), held, "{name}: turn {index}: {recall}");
            }
        }
        // Nothing was trimmed: every turn's history opens with the first
        // run's input, after its recall.
        for (index, turn) in sent.iter().enumerate() {
            let opening = Prompt::read(kind, &turn.body).unwrap().messages[0].to_string();
            assert!(opening.contains("hello"), "{name}: turn {index}: {opening}");
        }
        // The session keeps the last recall it read; each run, the digest of
        // the one it sent.
        for (run, first) in runs.iter().zip([0, 2, 4, 5, 6]) {
            assert_eq!(
                kept(&world, World::run_id(run)).await,
                keeping(&memory_type, Some(&recalled[first]), Some(&recalled[6])),
                "{name}"
            );
            assert!(!refreshing(&world, run).await, "{name}: each mark taken");
        }
    }
}

#[tokio::test]
async fn recalls_a_persons_change_at_a_sessions_next_run_and_a_runs_at_its_next_trim() {
    for (kind, model) in PATHS {
        let (world, _, _) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        let setup = &world.setup;
        let main = &world.main;
        let card = setup.save_note("Card on file ends 4242.").await;
        let before = turns(&world).len();
        let asked = world.say(ANA, &format!("{} hello", main.mention()), None).await;
        world.work().await;
        // A person redacts the note while the session is idle: the session
        // keeps no recall text from then on, and its next run recalls the
        // redacted note, its history kept.
        change_text(setup, &card, "Card on file.").await;
        let rest = at_rest(&world).await;
        assert!(rest[0].0.is_none() && rest[0].2, "{name}: {rest:?}");
        world.say(ANA, "and now?", Some(&asked.ts)).await;
        world.work().await;
        // A run changes the note itself: that marks nothing, and the run
        // after still sends the recall its session keeps.
        let updating = calling_memory(
            "update_memory",
            &json!({ "memory": card["short_id"], "text": "Card on file, expired." }),
        );
        world.say(ANA, &format!("update it\n{updating}"), Some(&asked.ts)).await;
        world.work().await;
        let rest = at_rest(&world).await;
        assert!(rest[0].0.is_some() && !rest[0].2, "{name}: {rest:?}");
        world.say(ANA, "ok", Some(&asked.ts)).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 4, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(
            runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]),
            "{name}"
        );
        let changed = memory_calls(setup, World::run_id(&runs[2])).await;
        assert_eq!(changed.len(), 1, "{name}: {changed:?}");
        assert_eq!(changed[0].state, CallState::Completed, "{name}: {changed:?}");
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 5, "{name}: a turn a run, two for the one with a call");
        let broken: Vec<usize> = breaks(kind, &sent).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [1], "{name}: only the run after the person's change reads anew");
        let recalled: Vec<String> = recalls(kind, &sent)
            .into_iter()
            .map(|recall| recall.unwrap_or_else(|| panic!("{name}: a turn without a recall")))
            .collect();
        assert!(
            recalled[0].contains("Card on file ends 4242."),
            "{name}: {}",
            recalled[0]
        );
        for (index, recall) in recalled.iter().enumerate().skip(1) {
            assert!(
                recall.contains("Card on file.") && !recall.contains("ends 4242") && !recall.contains("expired"),
                "{name}: turn {index}: {recall}"
            );
        }
        for (index, turn) in sent.iter().enumerate() {
            let opening = Prompt::read(kind, &turn.body).unwrap().messages[0].to_string();
            assert!(opening.contains("hello"), "{name}: turn {index}: {opening}");
        }
    }
}

#[tokio::test]
async fn marks_only_the_sessions_of_the_forgets_agent_workspace_and_type() {
    let (kind, model) = PATHS[0];
    let (world, _, memory_type) = remembering(kind, model).await;
    let setup = &world.setup;
    let main = &world.main;
    // The agent's session kept on another type: begun there, before the
    // agent moved back to its own.
    let other_type = setup.notes_type("Order notes", "allow").await;
    setup.remember_in(&other_type).await;
    let typed = world.say(ANA, &format!("{} hello", main.mention()), None).await;
    world.work().await;
    setup.remember_in(&memory_type).await;
    // The agent's session in its type; another agent's of the workspace in
    // the same type; and an agent's of a sibling workspace in it.
    let asked = world.say(ANA, &format!("{} hi", main.mention()), None).await;
    world.work().await;
    let (status, created) = setup
        .kit
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{}/agents", setup.ws))
                .naming(&setup.org)
                .body(json!({
                    "name": "Other", "description": "", "instructions": "",
                    "memory": "type", "memory_type_id": memory_type,
                })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let colleagues = ("alice", setup.org.as_str(), setup.ws.as_str(), id(&created["data"]));
    save_for(setup, colleagues, Some(&memory_type), "Prefers short answers.").await;
    let colleague = world
        .install(
            "alice",
            &setup.org,
            &setup.ws,
            id(&created["data"]),
            TEAM,
            "U0OTHER",
            "other",
        )
        .await;
    world
        .post(
            &[&colleague],
            SUPPORT,
            ANA,
            &format!("{} hello", colleague.mention()),
            None,
        )
        .await;
    world.work().await;
    let neighbour = setup.sibling("neighbour", "tk_neighbour").await;
    remembers(setup, &neighbour, &memory_type).await;
    save_for(
        setup,
        tenant_of(&neighbour),
        Some(&memory_type),
        "Prefers short answers.",
    )
    .await;
    let sibling = world
        .install(
            "alice",
            &setup.org,
            &neighbour.ws,
            &neighbour.agent,
            TEAM,
            "U0NEIGHBOUR",
            "neighbour",
        )
        .await;
    world
        .post(&[&sibling], SUPPORT, ANA, &format!("{} hello", sibling.mention()), None)
        .await;
    world.work().await;
    let session_runs = [
        world.runs(main).await[0].clone(),
        world.runs(main).await[1].clone(),
        world.runs(&colleague).await[0].clone(),
        world.runs(&sibling).await[0].clone(),
    ];
    assert!(
        session_runs.iter().all(|run| run["status"] == "completed"),
        "{session_runs:?}"
    );
    let marks = || async {
        let mut marks = Vec::new();
        for run in &session_runs {
            marks.push(refreshing(&world, run).await);
        }
        marks
    };
    assert_eq!(marks().await, [false; 4], "nothing forgotten yet");
    // Whether each session keeps recall text: all but the first, whose type
    // holds nothing to recall.
    let held = || async {
        let mut held = Vec::new();
        for run in &session_runs {
            held.push(holds_recall(&world, run).await);
        }
        held
    };
    assert_eq!(held().await, [false, true, true, true]);
    let follow_up = || async {
        world.say(BO, "and now?", Some(&asked.ts)).await;
        world.work().await;
    };

    // A person forgets the agent's note: only its session in the type,
    // which keeps no recall text from then on.
    let noted = setup.save_note("Ships on Fridays.").await;
    forget(setup, &noted).await;
    assert_eq!(marks().await, [false, true, false, false]);
    assert_eq!(held().await, [false, false, true, true]);
    follow_up().await;
    assert_eq!(marks().await, [false; 4], "the next run takes the mark");
    assert_eq!(held().await, [false, true, true, true], "and reads the recall anew");

    // An erasure of everything the agent keeps: the same.
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/agents/{}/memories", setup.ws, setup.agent),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(marks().await, [false, true, false, false]);
    assert_eq!(held().await, [false, false, true, true]);
    follow_up().await;
    assert_eq!(marks().await, [false; 4]);
    assert_eq!(held().await, [false, false, true, true], "nothing left to recall");

    // Deleting the other type: only the session kept on it.
    let (status, body) = setup
        .kit
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{}/memory-types/{other_type}", setup.org),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(marks().await, [true, false, false, false]);
    // Its next run takes the mark, in a new run session on the agent's own
    // type now.
    world.say(BO, "still there?", Some(&typed.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    let last = runs.last().unwrap();
    assert_eq!(last["status"], "completed", "{last}");
    assert_ne!(last["session_id"], session_runs[0]["session_id"], "{last}");
    assert_eq!(marks().await, [false; 4]);
}

#[tokio::test]
async fn recalls_nothing_once_its_type_is_deleted_and_refuses_its_calls() {
    for (kind, model) in PATHS {
        let (world, model_id, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        narrow(&world, &model_id).await;
        let setup = &world.setup;
        let main = &world.main;
        world.configure(main, json!({ "mid_run": "collect" })).await;
        let send = setup.calling("send", r#"{"to":"ops"}"#).await;
        let before = turns(&world).len();
        let asked = world.say(ANA, &format!("{} {}", main.mention(), long(1)), None).await;
        world.work().await;
        for n in 2..=3 {
            world.say(ANA, &long(n), Some(&asked.ts)).await;
            world.work().await;
        }
        // The fourth run waits for approval. Meanwhile the agent moves to
        // another type, the session's is deleted, and a message with a save
        // arrives, for the session's next run, which the history leaves room
        // for.
        world.say(ANA, &format!("tell ops\n{send}"), Some(&asked.ts)).await;
        world.work().await;
        let waiting = World::run_id(world.runs(main).await.last().unwrap());
        let other_type = setup.notes_type("Other notes", "allow").await;
        setup.remember_in(&other_type).await;
        let (status, body) = setup
            .kit
            .send(call(
                "alice",
                Method::DELETE,
                format!("/v1/organizations/{}/memory-types/{memory_type}", setup.org),
            ))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        // The run under way keeps the recall it started with.
        let rest = at_rest(&world).await;
        assert!(rest[0].0.is_some() && rest[0].2, "{name}: {rest:?}");
        world
            .say(BO, &format!("and this\n{}", save("Ships on Fridays.")), Some(&asked.ts))
            .await;
        world.handle().await;
        world.approve(waiting).await;
        world.work().await;

        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 5, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]));
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 7, "{name}: a turn a run, two for each with a call");
        let broken: Vec<usize> = breaks(kind, &sent).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [5], "{name}: the fifth run reads the recall anew");
        let opening = Prompt::read(kind, &sent[5].body).unwrap().messages[0].to_string();
        assert!(opening.contains("1 lorem ipsum"), "{name}: untrimmed: {opening}");
        // Every turn of the fourth run sends the recall the session began
        // with, the type's deletion under way included; from the next run,
        // the session's type is gone, and nothing is recalled. Its tools
        // stay.
        let recalled = recalls(kind, &sent);
        assert!(recalled[0].is_some(), "{name}");
        assert!(recalled[..5].iter().all(|recall| recall == &recalled[0]), "{name}");
        assert_eq!(recalled[5..], [None, None], "{name}");
        let tools = Prompt::read(kind, &sent[0].body).unwrap().tools;
        for turn in &sent {
            assert_eq!(Prompt::read(kind, &turn.body).unwrap().tools, tools, "{name}");
        }
        let calls = memory_calls(setup, World::run_id(&runs[4])).await;
        assert_eq!(calls.len(), 1, "{name}: {calls:?}");
        assert_eq!(calls[0].state, CallState::Refused, "{name}: {calls:?}");
        assert_eq!(calls[0].failure.as_ref().unwrap().code, "memory_type_removed");
        assert!(texts(setup, own(setup), None).await.is_empty(), "{name}: nothing saved");
        assert_eq!(
            kept(&world, World::run_id(&runs[4])).await,
            keeping(&memory_type, None, None),
            "{name}"
        );
    }
}

/// What a turn's messages say, its recall among them, in tokens as the
/// history budget counts them: a token for four bytes of their text, which
/// is all a turn of runs that call no tools sends.
fn text_tokens(kind: ProviderKind, body: &Value) -> u64 {
    let prompt = Prompt::read(kind, body).unwrap();
    let bytes: usize = prompt
        .messages
        .iter()
        .map(|message| match &message["content"] {
            Value::String(text) => text.len(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .map(str::len)
                .sum(),
            _ => 0,
        })
        .sum();
    u64::try_from(bytes).unwrap().div_ceil(4)
}

/// Saves `count` notes of 489 characters through the API, numbered from
/// `from`.
async fn long_notes(setup: &Setup, from: u32, count: u32) {
    for n in from..from + count {
        setup
            .save_note(&format!("Note {n:02}: {}", "dolor sit amet ".repeat(32)))
            .await;
    }
}

/// Has `memory_type` recall up to `characters` of notes.
async fn recall_characters(setup: &Setup, memory_type: &str, characters: u32) {
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{}/memory-types/{memory_type}", setup.org),
            )
            .body(json!({ "settings": { "recall_characters": characters } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// The most bytes a recall read anew takes in a session of [`narrow`]'s
/// budget: half of its 3,300 tokens, four bytes to a token.
const HALF_THE_BUDGET: usize = 3_300 / 2 * 4;

#[tokio::test]
async fn keeps_the_recall_within_the_history_budget_beside_the_history_and_the_input() {
    for (kind, model) in PATHS {
        let (world, model_id, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        narrow(&world, &model_id).await;
        let setup = &world.setup;
        let main = &world.main;
        // A recall of about 900 tokens of the session's 3,300. Two long runs
        // and a third long input take about 2,500: they'd fit without it,
        // not with it, so the third run trims the first.
        long_notes(setup, 1, 7).await;
        let before = turns(&world).len();
        let asked = world.say(ANA, &format!("{} {}", main.mention(), long(1)), None).await;
        world.work().await;
        for n in 2..=4 {
            world.say(ANA, &long(n), Some(&asked.ts)).await;
            world.work().await;
        }
        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 4, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]));
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 4, "{name}: one turn a run");
        let recalled = recalls(kind, &sent);
        let held = recalled[0].clone().unwrap();
        assert!(held.len() / 4 > 850, "{name}: {}", held.len());
        assert!(recalled.iter().all(|sent| sent.as_ref() == Some(&held)), "{name}");
        for (index, turn) in sent.iter().enumerate() {
            let tokens = text_tokens(kind, &turn.body);
            assert!(tokens <= 3_300, "{name}: turn {index} takes {tokens}");
        }
        let broken: Vec<usize> = breaks(kind, &sent).into_iter().map(|(index, _)| index).collect();
        assert_eq!(broken, [2, 3], "{name}: the third and fourth runs trim");
        let opening = Prompt::read(kind, &sent[2].body).unwrap().messages[0].to_string();
        assert!(opening.contains("2 lorem ipsum"), "{name}: {opening}");

        // A session's first run whose recall would take more than half the
        // budget recalls the newest notes that fit in that half, and counts
        // the rest.
        recall_characters(setup, &memory_type, 32_000).await;
        long_notes(setup, 8, 22).await;
        let before = turns(&world).len();
        world.say(BO, &format!("{} hello", main.mention()), None).await;
        world.work().await;
        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 5, "{name}");
        assert_eq!(runs[4]["status"], "completed", "{name}: {}", runs[4]);
        assert_ne!(runs[4]["session_id"], runs[0]["session_id"], "{name}");
        let first = &turns(&world)[before];
        let tokens = text_tokens(kind, &first.body);
        assert!((1_500..=1_700).contains(&tokens), "{name}: {tokens}");
        let fitted = recall(kind, &first.body).unwrap();
        // Within half the budget, short of it by less than a note's line.
        assert!(
            (HALF_THE_BUDGET - 500..=HALF_THE_BUDGET).contains(&fitted.len()),
            "{name}: {}",
            fitted.len()
        );
        assert!(
            fitted.contains("Note 29: ") && !fitted.contains("Prefers answers in Portuguese."),
            "{name}: {fitted}"
        );
        assert!(
            fitted.ends_with(" older notes aren't shown. search_memory finds them.\n</memory>"),
            "{name}: {fitted}"
        );
        assert_eq!(
            kept(&world, World::run_id(&runs[4])).await,
            keeping(&memory_type, Some(&fitted), Some(&fitted)),
            "{name}: the session keeps what was sent"
        );
    }
}

#[tokio::test]
async fn keeps_every_earlier_run_of_a_session_beside_a_recall_that_would_fill_the_budget() {
    for (kind, model) in PATHS {
        let (world, model_id, memory_type) = remembering(kind, model).await;
        let name = format!("{kind:?} {model}");
        narrow(&world, &model_id).await;
        let setup = &world.setup;
        let main = &world.main;
        // Notes of more than the whole budget, which the type would recall
        // in full.
        recall_characters(setup, &memory_type, 32_000).await;
        long_notes(setup, 1, 29).await;
        let before = turns(&world).len();
        let asked = world
            .say(
                BO,
                &format!("{} hello, my name is Bo and my order is 4417", main.mention()),
                None,
            )
            .await;
        world.work().await;
        for text in [
            "what's my order number?",
            "and what's my name?",
            "thanks, one more question",
            "bye",
        ] {
            world.say(BO, text, Some(&asked.ts)).await;
            world.work().await;
        }
        let runs = world.runs(main).await;
        assert_eq!(runs.len(), 5, "{name}");
        assert!(runs.iter().all(|run| run["status"] == "completed"), "{name}: {runs:?}");
        assert!(runs.iter().all(|run| run["session_id"] == runs[0]["session_id"]));
        let sent = turns(&world).split_off(before);
        assert_eq!(sent.len(), 5, "{name}: one turn a run");
        let broken = breaks(kind, &sent);
        assert!(broken.is_empty(), "{name}: no run trims: {broken:?}");
        // The recall takes half the budget, and every run sends it again.
        let held = recall(kind, &sent[0].body).unwrap();
        assert!(held.len() <= HALF_THE_BUDGET, "{name}: {}", held.len());
        assert!(
            held.ends_with(" older notes aren't shown. search_memory finds them.\n</memory>"),
            "{name}: {held}"
        );
        assert!(
            recalls(kind, &sent).iter().all(|sent| sent.as_ref() == Some(&held)),
            "{name}"
        );
        // Every run carries the runs before it, the first one's order number
        // included.
        for (index, turn) in sent.iter().enumerate() {
            let tokens = text_tokens(kind, &turn.body);
            assert!(tokens <= 3_300, "{name}: turn {index} takes {tokens}");
            let messages = Prompt::read(kind, &turn.body).unwrap().messages;
            assert_eq!(messages.len(), 2 * index + 1, "{name}: turn {index}");
            assert!(
                messages[0].to_string().contains("my order is 4417"),
                "{name}: turn {index}: {}",
                messages[0]
            );
        }
    }
}
