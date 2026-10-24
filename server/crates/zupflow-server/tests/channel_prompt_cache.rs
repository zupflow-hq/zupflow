//! A channel session's prompt, turn by turn, on the Anthropic and OpenAI
//! paths, over the providers' APIs faked in process: every turn of every
//! run in a thread's session continues the turn before it, byte for byte,
//! through a second run, a steered turn, a collected follow-up, an
//! approval pause, a worker that dies mid-run, a run that calls a channel
//! tool, an edit noted in the next turn, and the agent, its connection and
//! its team renamed mid-session.
//! The one break is the trim, when the history would go over its budget:
//! once, there, and the runs after it continue again.

#![allow(clippy::unwrap_used)]

#[path = "common/channel_world.rs"]
mod channel_world;
mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use axum::http::{Method, StatusCode};
use channel_world::{ANA, BO, World, world};
use common::{PROVIDER_KEY, Server, call};
use faults::{Point, Worker};
use run_setup::{id, setup_on};
use serde_json::json;
use zupflow_core::model_providers::ProviderKind;
use zupflow_providers::fake::Received;
use zupflow_providers::fake::prompt::check_prefix;

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
        // A small window: a budget of 3,300 tokens, so the fourth long
        // message trims the first run away, and a short one after fits.
        let pool = sqlx::SqlitePool::connect(&format!(
            "sqlite:{}",
            world.setup.server.directory.join("zupflow.sqlite").display()
        ))
        .await
        .unwrap();
        let model_uuid = zupflow_core::ids::ModelId::parse(&model_id).unwrap().uuid();
        let (org, _) = world.setup.scope();
        sqlx::query("UPDATE models SET context_window = 5500 WHERE organization_id = ? AND id = ?")
            .bind(org.uuid())
            .bind(model_uuid)
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;

        let main = &world.main;
        let long = |n: u32| format!("{n} {}", "lorem ipsum ".repeat(166));
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
