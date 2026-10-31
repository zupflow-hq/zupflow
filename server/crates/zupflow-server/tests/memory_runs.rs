//! Memory in API and Console runs: the agent's memory tools, decided when
//! each call is made from its type as it is then, made in the run's own
//! organization, workspace, agent and type, with the run and the call as the
//! origin of what they write; each change committed with its `memory.*`
//! webhook events and never audited; the recall kept on the run while it
//! runs; a call whose worker stopped answered as it was made, whatever
//! became of its memory since, and a stalled worker's late call changing
//! nothing; a call refused with its reason's own code when its type can't
//! be used, waiting for approval or not; and a run started with memory
//! `off` refusing them all.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use std::future::Future;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::call;
use faults::{Halt, Point, Worker, Write};
use run_setup::{Setup, Tenant, calling_memory, id, setup};
use serde_json::{Value, json};
use zupflow_core::ids::{MemoryTypeId, RunId};
use zupflow_core::models::{Completion, ModelProvider, ProviderAccess, ProviderError, Turn};
use zupflow_core::runs::CallState;
use zupflow_core::runs::digest::Sha256Digest;
use zupflow_core::runs::state::{CallRecord, CallVerdict};

fn save(text: &str) -> String {
    calling_memory("save_memory", &json!({ "text": text }))
}

/// The setup, with the agent remembering in a `notes` type of its own that
/// writes at `writes`: the type's ID.
async fn remembering(writes: &str) -> (Setup, String) {
    let setup = setup(&[("Main", "tk_main")]).await;
    let memory_type = setup.with_memory(writes).await;
    (setup, memory_type)
}

/// Starts a run with `input` and works it until it waits or ends: the run
/// as the API shows it.
async fn run(setup: &Setup, input: &str) -> (RunId, Value) {
    let run = setup.start(input).await;
    setup.kit.drain(Worker::A).await;
    (run, setup.get(run).await)
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

async fn database(setup: &Setup) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!(
        "sqlite:{}",
        setup.server.directory.join("zupflow.sqlite").display()
    ))
    .await
    .unwrap()
}

/// Every `memory.*` webhook event recorded, oldest first: its type and its
/// body.
async fn memory_events(setup: &Setup) -> Vec<(String, Value)> {
    let pool = database(setup).await;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT type, body FROM webhook_events WHERE type LIKE 'memory.%' ORDER BY created_at, rowid")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    rows.into_iter()
        .map(|(kind, body)| (kind, serde_json::from_str(&body).unwrap()))
        .collect()
}

/// How many audit events name a memory change.
async fn memory_audits(setup: &Setup) -> i64 {
    let pool = database(setup).await;
    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_events WHERE action LIKE 'memory.%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    count
}

/// The model every worker asks, keeping what each turn it was asked
/// recalled, in order.
struct Recalling {
    inner: Arc<dyn ModelProvider>,
    recalled: Mutex<Vec<Option<String>>>,
}

impl Recalling {
    /// Has every worker of `setup` ask the model through it from now.
    fn install(setup: &mut Setup) -> Arc<Self> {
        let recalling = Arc::new(Self {
            inner: setup.server.state.provider.clone(),
            recalled: Mutex::new(Vec::new()),
        });
        setup.kit.set_model(recalling.clone());
        recalling
    }

    /// What each turn asked since the last take recalled, in order.
    fn take(&self) -> Vec<Option<String>> {
        std::mem::take(&mut *self.recalled.lock().unwrap())
    }
}

#[async_trait]
impl ModelProvider for Recalling {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn complete(&self, access: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        self.recalled.lock().unwrap().push(turn.memory.map(str::to_owned));
        self.inner.complete(access, turn).await
    }
}

/// What `run` keeps of its memory: its type, its recall and the recall's
/// digest.
async fn kept_memory(setup: &Setup, run: RunId) -> (Option<MemoryTypeId>, Option<String>, Option<String>) {
    let pool = database(setup).await;
    let (memory_type_id, recall, digest): (Option<uuid::Uuid>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT memory_type_id, recall, recall_digest FROM runs WHERE id = ?")
            .bind(run.uuid())
            .fetch_one(&pool)
            .await
            .unwrap();
    pool.close().await;
    (memory_type_id.map(MemoryTypeId::from_uuid), recall, digest)
}

fn type_id(memory_type: &str) -> MemoryTypeId {
    MemoryTypeId::parse(memory_type).unwrap()
}

/// What the model reads back from a memory call that answered `answer`.
fn result(answer: &str) -> String {
    json!({ "result": answer }).to_string()
}

fn short_id(note: &Value) -> &str {
    note["short_id"].as_str().unwrap()
}

/// How many `kind` webhook events were recorded naming a run as who wrote
/// their memory's text, and how many naming anyone else: for a save or an
/// update, who made it.
async fn changes(setup: &Setup, kind: &str) -> (usize, usize) {
    let events = memory_events(setup).await;
    let of_kind = || events.iter().filter(|(recorded, _)| recorded == kind);
    let by_runs = of_kind()
        .filter(|(_, body)| body["data"]["origin"]["type"] == "run")
        .count();
    (by_runs, of_kind().count() - by_runs)
}

/// Changes `note`'s text as alice.
async fn edit_note(setup: &Setup, note: &Value, text: &str) {
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
                    id(note)
                ),
            )
            .naming(&setup.org)
            .body(json!({ "text": text })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Forgets `note` as alice.
async fn forget_note(setup: &Setup, note: &Value) {
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
                    id(note)
                ),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
}

/// The text the model ended the run with.
fn output(finished: &Value) -> &str {
    finished["output"].as_str().unwrap_or_default()
}

#[tokio::test]
async fn saves_with_the_run_and_the_call_as_its_origin_and_records_the_change_with_it() {
    let (mut setup, memory_type) = remembering("allow").await;
    let recalling = Recalling::install(&mut setup);
    setup.save_note("Prefers answers in Portuguese.").await;
    let lookup = setup.calling("lookup", &json!({ "n": 1 }).to_string()).await;
    // The first save is made with the lookup, before the model read its
    // result; the second after.
    let input = format!(
        "{lookup}\n{}\nthen\n{}",
        save("Saved beside the lookup."),
        save("Saved after the lookup.")
    );
    let (run, finished) = run(&setup, &input).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    assert_eq!(finished["memory"], "agent");

    let calls = memory_calls(&setup, run).await;
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|call| call.state == CallState::Completed), "{calls:?}");
    let notes = setup.notes().await;
    assert_eq!(notes.len(), 3, "{notes:?}");
    for (text, call, after_app_content) in [
        ("Saved beside the lookup.", &calls[0], false),
        ("Saved after the lookup.", &calls[1], true),
    ] {
        let note = notes.iter().find(|note| note["text"] == text).unwrap();
        assert_eq!(
            note["origin"],
            json!({
                "type": "run",
                "run_id": run.to_string(),
                "tool_call_id": call.id.to_string(),
                "after_app_content": after_app_content,
            }),
            "{text}"
        );
        assert_eq!(note["memory_type_id"], memory_type);
        // The model read the kind's answer.
        let answered = &call.outcome.as_ref().unwrap().content;
        assert_eq!(
            answered,
            &json!({ "result": format!("Saved as {}.", note["short_id"].as_str().unwrap()) }).to_string()
        );
    }
    // The run recalled what was saved before it, and only that, on every
    // turn. Its type and the recall's digest stay on the run; the recall
    // goes once it ended.
    let recalled = recalling.take();
    assert_eq!(recalled.len(), 3);
    assert!(recalled.iter().all(|recall| recall == &recalled[0]));
    let recall = recalled[0].as_deref().unwrap();
    assert!(
        recall.contains("Prefers answers in Portuguese.") && !recall.contains("Saved"),
        "{recall}"
    );
    assert_eq!(
        kept_memory(&setup, run).await,
        (
            Some(type_id(&memory_type)),
            None,
            Some(Sha256Digest::of(recall.as_bytes()).to_string())
        )
    );

    // Each save committed its webhook event, naming the memory and the run,
    // never its text; nothing was audited.
    let events = memory_events(&setup).await;
    let from_run: Vec<&(String, Value)> = events
        .iter()
        .filter(|(_, body)| body["data"]["origin"]["type"] == "run")
        .collect();
    assert_eq!(from_run.len(), 2, "{events:?}");
    for ((kind, body), call) in from_run.iter().zip(&calls) {
        assert_eq!(kind, "memory.saved");
        assert_eq!(body["workspace"]["id"], setup.ws);
        assert_eq!(body["data"]["origin"]["tool_call_id"], call.id.to_string());
        assert_eq!(body["data"]["memory_type_id"], memory_type);
        assert!(!body.to_string().contains("Saved "), "no text: {body}");
    }
    assert_eq!(memory_audits(&setup).await, 1, "only the person's save is audited");

    // The timeline names the memory type a call went to.
    let events = setup.events(run).await;
    let started: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "tool_call.started" && event["data"]["tool"] == "save_memory")
        .collect();
    assert_eq!(started.len(), 2, "{events:?}");
    assert_eq!(started[0]["data"]["memory_type_id"], memory_type);
    assert_eq!(started[0]["data"]["memory_type_name"], "Customer notes");
    assert_eq!(started[0]["data"]["kind"], "notes");
}

#[tokio::test]
async fn decides_each_call_by_the_types_writes_when_its_made() {
    let (setup, memory_type) = remembering("allow").await;
    let send = setup.calling("send", &json!({ "to": "ops" }).to_string()).await;
    let search = calling_memory("search_memory", &json!({ "query": "Fridays" }));
    let input = format!("{send}\nthen\n{}\n{search}", save("Ships on Fridays."));
    let run = setup.start(&input).await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
    // While the run waits, the type turns read-only. Its tools stay offered,
    // and the save is refused when the model makes it; a search reads.
    setup.set_writes(&memory_type, "deny").await;
    let key = setup.api_key().await;
    let waiting = setup.calls(run).await[0].id;
    let (status, body) = setup.decide(&key, run, &waiting.to_string(), "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.kit.drain(Worker::A).await;
    let finished = setup.get(run).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    let calls = memory_calls(&setup, run).await;
    assert_eq!(calls.len(), 2);
    let (saved, searched) = (&calls[0], &calls[1]);
    assert_eq!(saved.state, CallState::Refused);
    assert!(matches!(
        &saved.decision.as_ref().unwrap().verdict,
        CallVerdict::Deny { reason } if reason == "This agent's memory is read-only now, so it can't be changed."
    ));
    assert_eq!(saved.decision.as_ref().unwrap().policy, "memory");
    assert!(saved.outcome.as_ref().unwrap().is_error);
    assert_eq!(searched.state, CallState::Completed, "{searched:?}");
    assert!(setup.notes().await.is_empty());
    assert!(memory_events(&setup).await.is_empty(), "a refused call records nothing");

    // The next run offers only the tool that reads.
    let (_, next) = run_again(&setup, &save("Ships on Fridays.")).await;
    assert_eq!(next["status"], "completed");
    assert!(
        output(&next).contains("I can't use save_memory: it isn't one of my tools."),
        "{next}"
    );
}

async fn run_again(setup: &Setup, input: &str) -> (RunId, Value) {
    run(setup, input).await
}

#[tokio::test]
async fn waits_for_approval_of_a_save_and_makes_it_only_once_approved() {
    let (setup, _) = remembering("approve").await;
    let key = setup.api_key().await;

    // Approved: made once, recorded once.
    let first = setup.start(&save("Ships on Fridays.")).await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(first).await["status"], "waiting_for_approval");
    let waiting = &memory_calls(&setup, first).await[0];
    assert_eq!(waiting.state, CallState::AwaitingApproval);
    assert!(matches!(
        &waiting.decision.as_ref().unwrap().verdict,
        CallVerdict::RequireApproval { reason } if reason == "Your organization requires approval for save_memory."
    ));
    assert!(setup.notes().await.is_empty(), "nothing saved before approval");
    let events = setup.events(first).await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "approval.requested" && event["data"]["memory_type_id"].is_string()),
        "{events:?}"
    );
    let (status, body) = setup.decide(&key, first, &waiting.id.to_string(), "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(first).await["status"], "completed");
    assert_eq!(setup.notes().await.len(), 1);
    assert_eq!(memory_events(&setup).await.len(), 1);

    // Denied: never made, and the model reads why.
    let second = setup.start(&save("Closed on Mondays.")).await;
    setup.kit.drain(Worker::A).await;
    let waiting = &memory_calls(&setup, second).await[0];
    let (status, body) = setup
        .kit
        .send(
            call(
                &key,
                Method::POST,
                format!("/v1/workspaces/{}/runs/{second}/approvals", setup.ws),
            )
            .body(json!({ "call_id": waiting.id, "decision": "denied", "reason": "Not that one." })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.kit.drain(Worker::A).await;
    let finished = setup.get(second).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    assert!(output(&finished).contains("Not that one."), "{finished}");
    assert_eq!(setup.notes().await.len(), 1);
    assert_eq!(memory_events(&setup).await.len(), 1, "a denied call records nothing");
}

/// A run waits for approval of a call in its type; the agent moves to
/// another type and the run's is deleted: the job its deletion queued
/// refuses the call without anyone deciding it, and the run moves on, its
/// later calls refused for the same reason.
#[tokio::test]
async fn refuses_a_call_waiting_in_a_deleted_type_and_moves_the_run_on() {
    let (setup, memory_type) = remembering("approve").await;
    setup.save_note("Prefers answers in Portuguese.").await;
    let search = calling_memory("search_memory", &json!({ "query": "Fridays" }));
    let run = setup
        .start(&format!("{}\nthen\n{search}", save("Ships on Fridays.")))
        .await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
    let (kept_type, recall, _) = kept_memory(&setup, run).await;
    assert_eq!(kept_type, Some(type_id(&memory_type)));
    assert!(recall.is_some(), "kept on the run while it waits");

    let other = setup.notes_type("Other notes", "allow").await;
    setup.remember_in(&other).await;
    let (status, body) = setup
        .kit
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{}/memory-types/{memory_type}", setup.org),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    setup.kit.drain(Worker::A).await;

    let finished = setup.get(run).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    let calls = memory_calls(&setup, run).await;
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.state, CallState::Refused, "{call:?}");
        assert_eq!(call.failure.as_ref().unwrap().code, "memory_type_removed");
        assert_eq!(
            call.outcome.as_ref().unwrap().content,
            "This run's memory was deleted, so this call wasn't made."
        );
    }
    assert_eq!(calls[0].approval, None, "nobody decided it");
    let key = setup.api_key().await;
    let (status, body) = setup.decide(&key, run, &calls[0].id.to_string(), "approved").await;
    assert_eq!(status, StatusCode::CONFLICT, "nothing waits for a decision: {body}");
    assert_eq!(changes(&setup, "memory.saved").await.0, 0, "nothing saved by the run");
    assert_eq!(kept_memory(&setup, run).await.1, None, "an ended run keeps no recall");
}

/// Works a run of a save until the model answered its first turn, changes
/// the run's type with `change` (bound to the type's ID) before the worker
/// decides the answer's call, and works the run to its end: the setup, the
/// run and its memory calls.
async fn changed_before_deciding(change: &'static str) -> (Setup, RunId, Vec<CallRecord>) {
    let (setup, memory_type) = remembering("allow").await;
    let run = setup.start(&save("Ships on Fridays.")).await;
    let gate = setup.kit.faults.arm(Worker::A, Point::AfterModel);
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    let pool = database(&setup).await;
    sqlx::query(change)
        .bind(type_id(&memory_type).uuid())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    gate.release();
    assert!(a.finish().await);
    setup.kit.drain(Worker::A).await;
    let calls = memory_calls(&setup, run).await;
    (setup, run, calls)
}

/// A type its run can't use when a call is decided, for a reason that
/// won't pass by asking again: the call is refused with that reason's own
/// code, and the model isn't asked for the turn again.
#[tokio::test]
async fn refuses_a_call_its_type_cant_serve_with_the_reasons_code_and_asks_no_turn_again() {
    for (change, code, message) in [
        (
            "UPDATE memory_types SET settings = '{\"max_notes\": 0}' WHERE id = ?",
            "invalid_memory_settings",
            "This run's memory has settings this server can't use, so this call wasn't made.",
        ),
        (
            "UPDATE memory_types SET kind = 'unserved_kind' WHERE id = ?",
            "memory_kind_not_served",
            "This run's memory can't be reached on this server, so this call wasn't made.",
        ),
    ] {
        let (setup, run, calls) = changed_before_deciding(change).await;
        let finished = setup.get(run).await;
        assert_eq!(finished["status"], "completed", "{code}: {finished}");
        assert_eq!(calls.len(), 1, "{code}");
        assert_eq!(calls[0].state, CallState::Refused, "{code}: {calls:?}");
        assert_eq!(calls[0].failure.as_ref().unwrap().code, code);
        assert_eq!(calls[0].outcome.as_ref().unwrap().content, message);
        let turns: Vec<usize> = setup.kit.faults.answers().iter().map(|answer| answer.turn).collect();
        assert_eq!(turns, [0, 1], "{code}: the turn with the call is asked once");
        assert_eq!(changes(&setup, "memory.saved").await, (0, 0), "{code}");
    }
}

#[tokio::test]
async fn refuses_every_memory_call_in_a_run_started_with_memory_off() {
    let (mut setup, memory_type) = remembering("allow").await;
    let recalling = Recalling::install(&mut setup);
    setup.save_note("Prefers answers in Portuguese.").await;
    let search = calling_memory("search_memory", &json!({ "query": "Portuguese" }));
    let input = format!("{}\n{search}", save("Ships on Fridays."));
    let run = setup.start_with_memory(&input, "off").await;
    setup.kit.drain(Worker::A).await;
    let finished = setup.get(run).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    assert_eq!(finished["memory"], "off");
    let calls = memory_calls(&setup, run).await;
    assert_eq!(calls.len(), 2, "both offered, as with memory on");
    for call in &calls {
        assert_eq!(call.state, CallState::Refused, "{call:?}");
        assert_eq!(call.failure.as_ref().unwrap().code, "memory_off");
        assert_eq!(call.outcome.as_ref().unwrap().content, "Memory is off for this run.");
    }
    assert_eq!(recalling.take(), [None, None], "nothing recalled");
    assert_eq!(
        kept_memory(&setup, run).await,
        (Some(type_id(&memory_type)), None, None),
        "its tools' type kept, and no recall"
    );
    assert_eq!(setup.notes().await.len(), 1, "nothing saved");
    let from_runs = memory_events(&setup)
        .await
        .into_iter()
        .filter(|(_, body)| body["data"]["origin"]["type"] == "run")
        .count();
    assert_eq!(from_runs, 0);

    // Anything but `agent` or `off` is refused.
    for memory in [json!("none"), json!(null), json!(true)] {
        let (status, body) = setup
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/runs", setup.ws, setup.agent),
                )
                .naming(&setup.org)
                .body(json!({ "input": "hello", "memory": memory })),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"]["code"], "validation_failed");
        assert_eq!(body["error"]["fields"]["memory"], "Choose agent or off.");
    }
}

/// A save whose worker stopped at `point`, its run taken over by the next
/// worker: the run.
async fn stopped_at(setup: &Setup, point: Point) -> RunId {
    let run = setup.start(&save("Ships on Fridays.")).await;
    let takeover = setup.kit.take_over_at(point, 1).await;
    assert!(takeover.b_found, "B took the run over");
    takeover.a.crash().await;
    setup.kit.drain(Worker::B).await;
    run
}

#[tokio::test]
async fn settles_a_save_whose_worker_stopped_without_making_it_twice() {
    // Saved, and the worker stopped before recording it: answered as it was
    // made.
    let (setup, _) = remembering("allow").await;
    let run = stopped_at(&setup, Point::Before(Write::RecordOutcome)).await;
    assert_eq!(setup.get(run).await["status"], "completed");
    let notes = setup.notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    let calls = memory_calls(&setup, run).await;
    assert_eq!(calls[0].state, CallState::Completed, "{calls:?}");
    assert_eq!(
        calls[0].outcome.as_ref().unwrap().content,
        result(&format!("Saved as {}.", short_id(&notes[0])))
    );
    assert_eq!(notes[0]["origin"]["tool_call_id"], calls[0].id.to_string());
    assert_eq!(memory_events(&setup).await.len(), 1, "recorded once");

    // Claimed, and the worker stopped before saving: made by the next.
    let (setup, _) = remembering("allow").await;
    let run = stopped_at(&setup, Point::After(Write::AuthorizeCall)).await;
    assert_eq!(setup.get(run).await["status"], "completed");
    assert_eq!(setup.notes().await.len(), 1);
    assert_eq!(memory_calls(&setup, run).await[0].state, CallState::Completed);
    assert_eq!(memory_events(&setup).await.len(), 1);
}

/// Works the started run with worker A until it made its memory call and
/// stopped before recording what came of it, as if its process died there;
/// then `meanwhile` runs, and worker B takes the run over and works it to
/// its end: what `meanwhile` gave.
async fn stopped_after_making<T>(setup: &Setup, meanwhile: impl Future<Output = T>) -> T {
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::RecordOutcome));
    let Halt::Stopped(a) = setup.kit.spawn(Worker::A).until(&gate).await else {
        panic!("A never made the call");
    };
    a.crash().await;
    let given = meanwhile.await;
    setup.kit.expire_lease();
    setup.kit.drain(Worker::B).await;
    given
}

/// The run's one memory call, completed: what the model read back.
async fn answered(setup: &Setup, run: RunId) -> String {
    assert_eq!(setup.get(run).await["status"], "completed");
    let calls = memory_calls(setup, run).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].state, CallState::Completed, "{calls:?}");
    calls[0].outcome.clone().unwrap().content
}

/// A call made, whose worker stopped before recording what came of it, is
/// answered as it was made, whatever a person did to its memory since: a
/// save forgotten or edited isn't saved again, an update edited since isn't
/// made again over the edit, even one that gave the note the text it held,
/// and a forget answers as it forgot.
#[tokio::test]
async fn answers_a_lapsed_call_as_it_was_made_whatever_became_of_its_memory() {
    // A save, forgotten before the call was settled.
    let (setup, _) = remembering("allow").await;
    let run = setup.start(&save("Ships on Fridays.")).await;
    let note = stopped_after_making(&setup, async {
        let note = setup.notes().await.remove(0);
        forget_note(&setup, &note).await;
        note
    })
    .await;
    assert_eq!(
        answered(&setup, run).await,
        result(&format!("Saved as {}.", short_id(&note)))
    );
    assert!(setup.notes().await.is_empty(), "not saved again");
    assert_eq!(changes(&setup, "memory.saved").await, (1, 0));

    // A save, edited before the call was settled.
    let (setup, _) = remembering("allow").await;
    let run = setup.start(&save("Ships on Fridays.")).await;
    let note = stopped_after_making(&setup, async {
        let note = setup.notes().await.remove(0);
        edit_note(&setup, &note, "Ships on Saturdays.").await;
        note
    })
    .await;
    assert_eq!(
        answered(&setup, run).await,
        result(&format!("Saved as {}.", short_id(&note)))
    );
    let notes = setup.notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0]["text"], "Ships on Saturdays.", "the edit kept");
    assert_eq!(changes(&setup, "memory.saved").await, (1, 0));

    // An update, edited before the call was settled: not made again over
    // the edit.
    let (setup, _) = remembering("allow").await;
    let note = setup.save_note("Ships on Fridays.").await;
    let update = calling_memory(
        "update_memory",
        &json!({ "memory": short_id(&note), "text": "Ships on Thursdays." }),
    );
    let run = setup.start(&update).await;
    stopped_after_making(&setup, edit_note(&setup, &note, "Ships on Wednesdays.")).await;
    assert_eq!(
        answered(&setup, run).await,
        result(&format!("Updated {}.", short_id(&note)))
    );
    let notes = setup.notes().await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["text"], "Ships on Wednesdays.", "the edit kept");
    assert_eq!(changes(&setup, "memory.updated").await, (1, 1));

    // A forget, answered as it forgot.
    let forget = calling_memory("forget_memory", &json!({ "memory": short_id(&note) }));
    let run = setup.start(&forget).await;
    stopped_after_making(&setup, async {}).await;
    assert_eq!(
        answered(&setup, run).await,
        result(&format!("Forgot {}.", short_id(&note)))
    );
    assert!(setup.notes().await.is_empty());
    let forgotten = memory_events(&setup)
        .await
        .into_iter()
        .filter(|(kind, _)| kind == "memory.forgotten")
        .count();
    assert_eq!(forgotten, 1, "forgotten once");

    // An update to the text the note held, which changed nothing, edited
    // before the call was settled: answered as it was made, the edit kept,
    // and nothing sent for the run.
    let (setup, _) = remembering("allow").await;
    let note = setup.save_note("Ships on Fridays.").await;
    let unchanged = calling_memory(
        "update_memory",
        &json!({ "memory": short_id(&note), "text": "Ships on Fridays." }),
    );
    let run = setup.start(&unchanged).await;
    stopped_after_making(&setup, edit_note(&setup, &note, "Ships on Wednesdays.")).await;
    assert_eq!(
        answered(&setup, run).await,
        result(&format!("Updated {}.", short_id(&note)))
    );
    let notes = setup.notes().await;
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["text"], "Ships on Wednesdays.", "the edit kept");
    assert_eq!(changes(&setup, "memory.updated").await, (0, 1), "the edit's only");
}

/// A worker stalls after claiming a save's send, before making it; its
/// lease runs out, and the next worker settles the call by making it and
/// finishes the run. The stalled worker's own call, with the same run and
/// tool call, is refused when it goes on, and changes nothing.
#[tokio::test]
async fn refuses_a_stalled_workers_call_once_another_settled_it() {
    let (setup, _) = remembering("allow").await;
    let run = setup.start(&save("Ships on Fridays.")).await;
    let takeover = setup.kit.take_over(Point::After(Write::AuthorizeCall), None).await;
    assert_eq!(setup.get(run).await["status"], "completed");
    let notes = setup.notes().await;
    assert_eq!(notes.len(), 1, "{notes:?}");

    takeover.a_gate.release();
    assert!(takeover.a.finish().await);
    assert_eq!(setup.notes().await, notes, "nothing saved again");
    assert_eq!(changes(&setup, "memory.saved").await, (1, 0), "recorded once");
    let calls = memory_calls(&setup, run).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].state, CallState::Completed);
    assert_eq!(
        calls[0].outcome.as_ref().unwrap().content,
        result(&format!("Saved as {}.", short_id(&notes[0])))
    );
    assert_eq!(notes[0]["origin"]["tool_call_id"], calls[0].id.to_string());
    let late: Vec<Write> = setup
        .kit
        .faults
        .committed_since(takeover.lease_ran_out, Worker::A)
        .iter()
        .map(|attempted| attempted.write)
        .collect();
    assert_eq!(
        late,
        [],
        "nothing the stalled worker wrote after its lease ran out committed"
    );
}

/// `tenant`'s agent remembering in `memory_type`.
async fn tenant_remembers(setup: &Setup, tenant: &Tenant, memory_type: &str) {
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

async fn tenant_notes(setup: &Setup, tenant: &Tenant, agent: &str) -> Vec<String> {
    let (status, body) = setup
        .kit
        .send(
            call(
                &tenant.user,
                Method::GET,
                format!("/v1/workspaces/{}/agents/{agent}/memories", tenant.ws),
            )
            .naming(&tenant.org),
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

async fn tenant_saves(setup: &Setup, tenant: &Tenant, text: &str) {
    let (status, body) = setup
        .kit
        .send(
            call(
                &tenant.user,
                Method::POST,
                format!("/v1/workspaces/{}/agents/{}/memories", tenant.ws, tenant.agent),
            )
            .naming(&tenant.org)
            .body(json!({ "text": text })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
async fn reads_and_writes_only_its_own_organization_workspace_agent_and_type() {
    let (mut setup, memory_type) = remembering("allow").await;
    let recalling = Recalling::install(&mut setup);
    setup.save_note("The agent's own note.").await;
    // Another agent of the workspace, and an agent of a sibling workspace,
    // in the same type; an agent of another organization in its own.
    let neighbour = setup.sibling("neighbour", "tk_neighbour").await;
    tenant_remembers(&setup, &neighbour, &memory_type).await;
    tenant_saves(&setup, &neighbour, "The sibling workspace's note.").await;
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
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/agents/{other_agent}/memories", setup.ws),
            )
            .naming(&setup.org)
            .body(json!({ "text": "The other agent's note." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
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
    tenant_remembers(&setup, &elsewhere, id(&created["data"])).await;
    tenant_saves(&setup, &elsewhere, "Another organization's note.").await;

    let search = calling_memory("search_memory", &json!({ "query": "note" }));
    let (run, finished) = run(&setup, &format!("{}\n{search}", save("Saved by the run."))).await;
    assert_eq!(finished["status"], "completed", "{finished}");
    // It recalled its own agent's notes, and found only those.
    let recall = recalling.take()[0].clone().unwrap();
    assert!(recall.contains("The agent's own note."), "{recall}");
    let found = memory_calls(&setup, run).await[1].outcome.clone().unwrap().content;
    assert!(found.contains("The agent's own note."), "{found}");
    for theirs in [
        "The sibling workspace's note.",
        "The other agent's note.",
        "Another organization's note.",
    ] {
        assert!(!recall.contains(theirs), "{recall}");
        assert!(!found.contains(theirs), "{found}");
    }
    // It saved for its own agent only.
    let own = setup.notes().await;
    assert!(own.iter().any(|note| note["text"] == "Saved by the run."));
    assert_eq!(
        tenant_notes(&setup, &neighbour, &neighbour.agent).await,
        ["The sibling workspace's note."]
    );
    assert_eq!(
        tenant_notes(&setup, &elsewhere, &elsewhere.agent).await,
        ["Another organization's note."]
    );
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::GET,
                format!("/v1/workspaces/{}/agents/{other_agent}/memories", setup.ws),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"].as_array().unwrap().len(), 1);
    // Its webhook event belongs to its own workspace.
    let from_run: Vec<(String, Value)> = memory_events(&setup)
        .await
        .into_iter()
        .filter(|(_, body)| body["data"]["origin"]["type"] == "run")
        .collect();
    assert_eq!(from_run.len(), 1);
    assert_eq!(from_run[0].1["workspace"]["id"], setup.ws);
    assert_eq!(from_run[0].1["data"]["agent_id"], setup.agent);
}
