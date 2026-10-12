//! The run engine under faults, with the kit in `common/faults.rs`: workers
//! stopped or crashed at named points, leases running out, two workers on
//! one run, and what the app's server and the model actually saw.
//!
//! Every test here holds. A hole found in the engine is written here as a
//! test of the behaviour the engine owes, marked
//! `#[ignore = "fixed when …: …"]` and failing on the assertion that names
//! the hole (`cargo test -p zupflow-server --test run_faults -- --ignored`);
//! the change that closes it removes the `ignore`. None is open today.
//!
//! Whatever happens, the app's server receives at most one request per call:
//! its own ledger says so, not the runtime's records.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use faults::{Kit, Point, RecordedCall, Worker, Write, accounted_tokens, kept_answers, produced_tokens};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use zupflow_core::ids::{OrganizationId, RunId, WorkspaceId};
use zupflow_core::models::{Completion, Message, ModelProvider, ModelToolCall, ProviderError, Reply, TokenUsage, Turn};
use zupflow_core::runs::CallState;
use zupflow_core::runs::state::ModelAttemptState;
use zupflow_server::run_engine;

const MCP: &str = "https://tools.example/mcp";

/// What `lookup` is described as.
const LOOKUP: &str = "Look something up";

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

/// An organization's Sandbox workspace with installations of one catalog
/// app, each with its own label and token, and an agent granted `lookup`
/// (allowed) and `send` (needs approval) on each, in the order given.
struct Setup {
    server: Server,
    kit: Kit,
    org: String,
    ws: String,
    agent: String,
    /// By label.
    installations: Vec<(String, String)>,
}

async fn setup(installations: &[(&str, &str)]) -> Setup {
    let server = Server::new().await;
    server.providers.serve_mcp(
        MCP,
        json!([
            { "name": "lookup", "description": LOOKUP, "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } },
            { "name": "send", "description": "Send a message", "inputSchema": { "type": "object" } },
        ]),
    );
    let org = server.organization("alice", "northwind").await;
    let ws = id(&server.workspace("alice", &org, "sandbox", "sandbox").await).to_owned();
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/apps")).body(json!({
                "kind": "mcp", "name": "Tools", "key": "tools", "description": "", "auth_type": "token",
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let app = id(&created["data"]).to_owned();
    let (status, body) = server
        .send(
            call("alice", Method::PATCH, format!("/v1/organizations/{org}/apps/{app}")).body(json!({ "auth": {
                "type": "token",
                "inputs": [{ "key": "api_key", "label": "API key", "secret": true, "help": null, "placeholder": null, "pattern": null }],
                "placement": { "location": "header", "name": "X-Api-Key", "prefix": "" },
                "verify_url": null, "instructions": "", "docs_url": null,
            }})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{org}/apps/{app}/environments/sandbox/configuration"),
            )
            .body(json!({ "mcp_server_url": MCP })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let mut connected = Vec::new();
    for (label, token) in installations {
        connected.push((
            (*label).to_owned(),
            connect(&server, &org, &ws, &app, label, token).await,
        ));
    }
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/apps/{app}/tool-levels"),
            )
            .body(json!({
                "levels": [{ "tool": "lookup", "level": "allow" }, { "tool": "send", "level": "approve" }],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, agent) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(&org)
                .body(json!({ "name": "Helper", "description": "", "model": "model-a", "instructions": "Be brief." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    let agent = id(&agent["data"]).to_owned();
    let kit = Kit::new(&server);
    let setup = Setup {
        server,
        kit,
        org,
        ws,
        agent,
        installations: connected,
    };
    for (_, installation) in &setup.installations {
        setup.grant(installation).await;
    }
    setup
}

async fn connect(server: &Server, org: &str, ws: &str, app: &str, label: &str, token: &str) -> String {
    let (status, session) = server
        .send(
            call("alice", Method::POST, format!("/v1/workspaces/{ws}/connect-sessions"))
                .naming(org)
                .body(json!({ "app_id": app, "intent": "create", "label": label, "installation_id": null, "return_url": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let reference = session["data"]["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let (status, done) = server
        .send(
            call(
                "nobody",
                Method::POST,
                format!("/v1/connect/sessions/{reference}/credentials"),
            )
            .body(json!({ "values": { "api_key": token } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    while zupflow_server::discovery::work_once(&server.state).await {}
    id(&done["data"]["installation"]).to_owned()
}

impl Setup {
    /// `call <tool> <arguments>`, naming the first installation's `tool` as
    /// the model is offered it: `tools_<short_id>_<tool>` (decision 0047).
    async fn calling(&self, tool: &str, arguments: &str) -> String {
        let installation = &self.installations[0].1;
        let (status, read) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::GET,
                    format!("/v1/workspaces/{}/installations/{installation}", self.ws),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{read}");
        format!(
            "call tools_{}_{tool} {arguments}",
            read["data"]["short_id"].as_str().unwrap()
        )
    }

    fn scope(&self) -> (OrganizationId, WorkspaceId) {
        (
            OrganizationId::parse(&self.org).unwrap(),
            WorkspaceId::parse(&self.ws).unwrap(),
        )
    }

    async fn grant(&self, installation: &str) {
        let (status, grant) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/grants", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "installation_id": installation, "tools": ["lookup", "send"], "approval_required": ["send"] })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{grant}");
    }

    /// Removes the agent's grant on `installation` and grants it again, so
    /// it comes last among the agent's grants.
    async fn grant_again(&self, installation: &str) {
        let grants = format!("/v1/workspaces/{}/agents/{}/grants", self.ws, self.agent);
        let (status, listed) = self
            .kit
            .send(call("alice", Method::GET, grants.clone()).naming(&self.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{listed}");
        let grant = listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|grant| grant["installation_id"] == installation)
            .unwrap();
        let (status, _) = self
            .kit
            .send(call("alice", Method::DELETE, format!("{grants}/{}", id(grant))).naming(&self.org))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        self.grant(installation).await;
    }

    /// Starts a run as alice; its ID. Nothing works it yet.
    async fn start(&self, input: &str) -> RunId {
        let (status, run) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/agents/{}/runs", self.ws, self.agent),
                )
                .naming(&self.org)
                .body(json!({ "input": input })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{run}");
        RunId::parse(id(&run["data"])).unwrap()
    }

    async fn get(&self, run: RunId) -> Value {
        let (status, body) = self
            .kit
            .send(call("alice", Method::GET, format!("/v1/workspaces/{}/runs/{run}", self.ws)).naming(&self.org))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["data"].clone()
    }

    async fn cancel(&self, run: RunId) {
        let (status, body) = self
            .kit
            .send(
                call(
                    "alice",
                    Method::POST,
                    format!("/v1/workspaces/{}/runs/{run}/cancel", self.ws),
                )
                .naming(&self.org),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["status"], "cancelled");
    }

    async fn calls(&self, run: RunId) -> Vec<RecordedCall> {
        let (org, ws) = self.scope();
        self.kit.progress(org, ws, run).await.calls
    }

    /// The credential each `tools/call` the app's server received carried.
    fn credentials_received(&self) -> Vec<Option<(String, String)>> {
        self.server
            .providers
            .received_calls()
            .into_iter()
            .map(|received| received.credential)
            .collect()
    }
}

fn main_credential() -> Option<(String, String)> {
    Some(("X-Api-Key".to_owned(), "tk_main".to_owned()))
}

#[tokio::test]
async fn never_sends_a_call_whose_worker_died_before_it_left() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start(&setup.calling("lookup", "{}").await).await;
    let gate = setup.kit.faults.arm(Worker::A, Point::BeforeSend);
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    a.crash().await;
    setup.kit.drain(Worker::B).await;
    assert!(
        setup.server.providers.received_calls().is_empty(),
        "the app's server never received it"
    );
    let calls = setup.calls(run).await;
    assert_eq!(calls[0].state, CallState::Unknown, "recorded as sent, so never sent");
    let done = setup.get(run).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert!(done["output"].as_str().unwrap().contains("_lookup failed: "), "{done}");
}

#[tokio::test]
async fn never_sends_again_a_call_whose_answer_was_lost() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start(&setup.calling("lookup", "{}").await).await;
    let gate = setup.kit.faults.arm(Worker::A, Point::AfterSend);
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    a.crash().await;
    assert_eq!(setup.credentials_received(), [main_credential()]);
    setup.kit.drain(Worker::B).await;
    assert_eq!(
        setup.credentials_received(),
        [main_credential()],
        "received once, never again"
    );
    assert_eq!(setup.calls(run).await[0].state, CallState::Unknown);
    assert_eq!(setup.get(run).await["status"], "completed");
}

#[tokio::test]
async fn takes_a_run_over_from_its_last_recorded_turn() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start(&setup.calling("lookup", "{}").await).await;
    // A recorded the model's turn, then stopped; B takes the run over.
    let takeover = setup.kit.take_over(Point::After(Write::CommitTurn), None).await;
    takeover.a.crash().await;
    let answers = setup.kit.faults.answers();
    let asked: Vec<(Worker, usize)> = answers.iter().map(|answer| (answer.worker, answer.turn)).collect();
    assert_eq!(
        asked,
        [(Worker::A, 0), (Worker::B, 1)],
        "the recorded turn isn't asked for again"
    );
    assert_eq!(setup.credentials_received(), [main_credential()]);
    let calls = setup.calls(run).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].state, CallState::Completed);
    assert_eq!(setup.get(run).await["status"], "completed");
}

#[tokio::test]
async fn keeps_the_answer_a_slow_worker_gets_after_a_takeover() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start(&setup.calling("lookup", "{}").await).await;
    // A sent the call and the server acted on it; before A hears back, its
    // lease runs out and B takes the run over and finishes it.
    let takeover = setup.kit.take_over(Point::AfterSend, None).await;
    takeover.a_gate.release();
    assert!(takeover.a.finish().await);
    setup.kit.drain(Worker::B).await;
    assert_eq!(
        setup.credentials_received(),
        [main_credential()],
        "the server received the call once"
    );
    assert_eq!(setup.get(run).await["status"], "completed");
    let calls = setup.calls(run).await;
    assert_eq!(calls.len(), 1);
    assert!(
        kept_answers(&calls[0])
            .iter()
            .any(|answer| answer.contains("lookup done")),
        "the answer the app's server gave was lost: the call is {:?}, keeping {:?}",
        calls[0].state,
        calls[0].outcome
    );
}

#[tokio::test]
async fn closes_the_calls_a_cancel_left_unstarted() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup
        .start(&format!(
            "{}\n{}",
            setup.calling("lookup", "{}").await,
            setup.calling("lookup", "{\"page\":2}").await
        ))
        .await;
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::AuthorizeCall));
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    // The cancel commits first.
    setup.cancel(run).await;
    gate.release();
    assert!(a.finish().await);
    setup.kit.drain(Worker::B).await;
    assert_eq!(setup.get(run).await["status"], "cancelled");
    assert!(
        setup.server.providers.received_calls().is_empty(),
        "nothing is sent after a cancel"
    );
    let calls = setup.calls(run).await;
    assert_eq!(calls.len(), 2);
    let open: Vec<CallState> = calls
        .iter()
        .map(|call| call.state)
        .filter(|state| !state.is_settled())
        .collect();
    assert!(open.is_empty(), "calls left open after the run was cancelled: {open:?}");
}

#[tokio::test]
async fn closes_the_call_a_cancelled_run_waited_on() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let (status, key) = setup
        .server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{}/api-keys", setup.org),
            )
            .body(json!({ "name": "Backend", "environment": "sandbox" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let key = key["data"]["secret"].as_str().unwrap().to_owned();
    let run = setup.start(&setup.calling("send", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
    setup.cancel(run).await;
    let call_id = setup.calls(run).await[0].id;
    let (status, body) = setup
        .kit
        .send(
            call(
                &key,
                Method::POST,
                format!("/v1/workspaces/{}/runs/{run}/approvals", setup.ws),
            )
            .body(json!({ "call_id": call_id, "decision": "approved" })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "not_waiting"));
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(run).await["status"], "cancelled");
    assert!(setup.server.providers.received_calls().is_empty());
    let calls = setup.calls(run).await;
    assert!(
        calls[0].state.is_settled(),
        "the call is left {:?} after the run was cancelled",
        calls[0].state
    );
}

#[tokio::test]
async fn records_a_model_answer_lost_before_its_turn_was_committed() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("hello").await;
    // The model answered; A dies before committing the turn.
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::CommitTurn));
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    a.crash().await;
    setup.kit.drain(Worker::B).await;
    let (org, ws) = setup.scope();
    let progress = setup.kit.progress(org, ws, run).await;
    assert_eq!(setup.get(run).await["status"], "completed");
    assert_eq!(progress.turns.len(), 1, "the turn is committed once");
    let answers = setup.kit.faults.answers();
    let asked: Vec<(Worker, usize)> = answers.iter().map(|answer| (answer.worker, answer.turn)).collect();
    assert_eq!(
        asked,
        [(Worker::A, 0), (Worker::B, 0)],
        "the lost answer is asked for again, under a new attempt"
    );
    let attempts: Vec<(u32, u32, ModelAttemptState)> = progress
        .model_attempts
        .iter()
        .map(|attempt| (attempt.turn, attempt.attempt, attempt.state))
        .collect();
    assert_eq!(
        attempts,
        [
            (0, 1, ModelAttemptState::Abandoned),
            (0, 2, ModelAttemptState::Answered),
        ],
        "the abandoned attempt is recorded, for operators: its tokens may have been spent"
    );
    // Usage counts the committed answer: a lost one's tokens never reached
    // the store (decision 0046 keeps them internal).
    assert_eq!(accounted_tokens(&progress), produced_tokens(&answers[1..]));
}

#[tokio::test]
async fn refuses_the_writes_of_a_worker_whose_lease_ran_out() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("hello").await;
    // A has the model's answer and is about to record it when its lease
    // runs out; B claims the run and is waiting on the model.
    let takeover = setup
        .kit
        .take_over(Point::Before(Write::CommitTurn), Some(Point::BeforeModel))
        .await;
    takeover.a_gate.release();
    assert!(takeover.a.finish().await);
    takeover.b_gate.unwrap().release();
    assert!(takeover.b.unwrap().finish().await);
    assert_eq!(setup.get(run).await["status"], "completed");
    let stale = setup.kit.faults.committed_since(takeover.lease_ran_out, Worker::A);
    assert!(
        stale.is_empty(),
        "A committed after its lease ran out and B claimed the run: {stale:?}"
    );
}

/// The `lookup` tools offered to the model in its first answer.
fn offered_lookups(setup: &Setup) -> Vec<zupflow_core::models::ToolSpec> {
    setup.kit.faults.answers()[0]
        .tools
        .iter()
        .filter(|tool| tool.description.contains(LOOKUP))
        .cloned()
        .collect()
}

#[tokio::test]
async fn tells_two_installations_of_one_app_apart() {
    let setup = setup(&[("Work", "tk_work"), ("Personal", "tk_personal")]).await;
    setup.start("hello").await;
    setup.kit.drain(Worker::A).await;
    let lookups = offered_lookups(&setup);
    assert_eq!(lookups.len(), 2, "{lookups:?}");
    assert_ne!(lookups[0].name, lookups[1].name);
    for (label, _) in &setup.installations {
        let naming: Vec<&str> = lookups
            .iter()
            .filter(|tool| tool.description.contains(label.as_str()))
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(
            naming.len(),
            1,
            "the model can't tell which tool reaches {label}: offered {lookups:?}"
        );
    }
}

#[tokio::test]
async fn names_a_tool_after_its_installation_whatever_the_grant_order() {
    let setup = setup(&[("Work", "tk_work"), ("Personal", "tk_personal")]).await;
    setup.start("hello").await;
    setup.kit.drain(Worker::A).await;
    let name = offered_lookups(&setup)[0].name.clone();
    setup.start(&format!("call {name} {{}}")).await;
    setup.kit.drain(Worker::A).await;
    // Work's grant now comes after Personal's.
    let work = setup.installations[0].1.clone();
    setup.grant_again(&work).await;
    setup.start(&format!("call {name} {{}}")).await;
    setup.kit.drain(Worker::A).await;
    let received = setup.credentials_received();
    assert_eq!(received.len(), 2, "both runs called {name}: {received:?}");
    assert_eq!(
        received[0], received[1],
        "{name} reached another installation once the grants were in another order"
    );
}

/// The `tools/call` requests the app's server received with `arguments`.
fn received_with(setup: &Setup, arguments: &Value) -> usize {
    setup
        .server
        .providers
        .received_calls()
        .iter()
        .filter(|received| &received.arguments == arguments)
        .count()
}

/// Every point a worker can stop at while it takes a run with two calls
/// forward: each write it makes, the model's answer and the send.
const STOPS: [Point; 12] = [
    Point::After(Write::Claim),
    Point::Before(Write::PrepareModel),
    Point::After(Write::PrepareModel),
    Point::BeforeModel,
    Point::AfterModel,
    Point::Before(Write::CommitTurn),
    Point::After(Write::CommitTurn),
    Point::Before(Write::AuthorizeCall),
    Point::After(Write::AuthorizeCall),
    Point::BeforeSend,
    Point::AfterSend,
    Point::Before(Write::RecordOutcome),
];

/// Worker A stops at each point, its lease runs out and B takes the run over
/// and finishes it; then A is crashed there, or let go on. The app's server
/// receives each call at most once, a call it never received is never
/// reported as done, and the run completes.
async fn sends_each_call_at_most_once_after_a_takeover(a_goes_on: bool) {
    for stop in STOPS {
        let setup = setup(&[("Main", "tk_main")]).await;
        let first = json!({ "n": 1 });
        let second = json!({ "n": 2 });
        let run = setup
            .start(&format!(
                "{}\n{}",
                setup.calling("lookup", &first.to_string()).await,
                setup.calling("lookup", &second.to_string()).await
            ))
            .await;
        let takeover = setup.kit.take_over(stop, None).await;
        if a_goes_on {
            takeover.a_gate.release();
            assert!(takeover.a.finish().await, "{stop:?}");
        } else {
            takeover.a.crash().await;
        }
        setup.kit.drain(Worker::B).await;
        let done = setup.get(run).await;
        assert_eq!(done["status"], "completed", "{stop:?}: {done}");
        let calls = setup.calls(run).await;
        assert_eq!(calls.len(), 2, "{stop:?}");
        for (call, arguments) in calls.iter().zip([&first, &second]) {
            let received = received_with(&setup, arguments);
            assert!(
                received <= 1,
                "{stop:?}: the app's server received {arguments} {received} times"
            );
            assert!(call.state.is_settled(), "{stop:?}: {:?}", call.state);
            if call.state == CallState::Completed {
                assert_eq!(received, 1, "{stop:?}: done, so received once");
            }
            if received == 0 {
                assert_ne!(call.state, CallState::Completed, "{stop:?}: never received");
            }
        }
        assert!(
            setup
                .credentials_received()
                .iter()
                .all(|credential| credential == &main_credential()),
            "{stop:?}: only its own credential"
        );
    }
}

#[tokio::test]
async fn sends_each_call_at_most_once_when_the_first_worker_dies() {
    sends_each_call_at_most_once_after_a_takeover(false).await;
}

#[tokio::test]
async fn sends_each_call_at_most_once_when_the_first_worker_comes_back() {
    sends_each_call_at_most_once_after_a_takeover(true).await;
}

#[tokio::test]
async fn records_the_send_a_cancel_came_after() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let first = json!({ "n": 1 });
    let second = json!({ "n": 2 });
    let run = setup
        .start(&format!(
            "{}\n{}",
            setup.calling("lookup", &first.to_string()).await,
            setup.calling("lookup", &second.to_string()).await
        ))
        .await;
    // The first call is authorized to send; the cancel commits before it
    // leaves.
    let gate = setup.kit.faults.arm(Worker::A, Point::BeforeSend);
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    setup.cancel(run).await;
    gate.release();
    assert!(a.finish().await);
    setup.kit.drain(Worker::B).await;
    assert_eq!(setup.get(run).await["status"], "cancelled");
    assert_eq!(received_with(&setup, &first), 1, "the send already authorized went out");
    assert_eq!(received_with(&setup, &second), 0, "nothing after it");
    let calls = setup.calls(run).await;
    assert_eq!(calls[0].state, CallState::Completed, "its outcome recorded");
    assert_eq!(calls[1].state, CallState::Refused);
    assert_eq!(
        calls[1].failure.as_ref().map(|failure| failure.code.as_str()),
        Some("run_cancelled")
    );
}

#[tokio::test]
async fn records_a_call_in_flight_when_its_credential_is_deleted() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let first = json!({ "n": 1 });
    let second = json!({ "n": 2 });
    let run = setup
        .start(&format!(
            "{}\n{}",
            setup.calling("lookup", &first.to_string()).await,
            setup.calling("lookup", &second.to_string()).await
        ))
        .await;
    let gate = setup.kit.faults.arm(Worker::A, Point::BeforeSend);
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    // The credential goes while the first call is on its way.
    let installation = &setup.installations[0].1;
    let (status, body) = setup
        .kit
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/workspaces/{}/installations/{installation}/disconnect", setup.ws),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    gate.release();
    assert!(a.finish().await);
    setup.kit.drain(Worker::B).await;
    assert_eq!(setup.get(run).await["status"], "completed");
    assert_eq!(
        setup.credentials_received(),
        [main_credential()],
        "that request went out"
    );
    assert_eq!(received_with(&setup, &second), 0);
    let calls = setup.calls(run).await;
    assert_eq!(calls[0].state, CallState::Completed, "and its outcome is recorded");
    assert_eq!(calls[1].state, CallState::Refused, "the next call to it is refused");
    assert_eq!(
        calls[1].failure.as_ref().map(|failure| failure.code.as_str()),
        Some("installation_disconnected")
    );
}

#[tokio::test]
async fn refuses_at_send_a_call_whose_installation_went_after_its_turn() {
    for (disconnect, code) in [(false, "installation_removed"), (true, "installation_disconnected")] {
        let setup = setup(&[("Main", "tk_main")]).await;
        let run = setup.start(&setup.calling("lookup", "{}").await).await;
        let gate = setup.kit.faults.arm(Worker::A, Point::After(Write::CommitTurn));
        let a = setup.kit.spawn(Worker::A);
        gate.reached().await;
        let installation = format!("/v1/workspaces/{}/installations/{}", setup.ws, setup.installations[0].1);
        let (status, body) = if disconnect {
            setup
                .kit
                .send(call("alice", Method::POST, format!("{installation}/disconnect")).naming(&setup.org))
                .await
        } else {
            setup
                .kit
                .send(call("alice", Method::DELETE, installation).naming(&setup.org))
                .await
        };
        assert!(status.is_success(), "{body}");
        gate.release();
        assert!(a.finish().await);
        setup.kit.drain(Worker::B).await;
        let done = setup.get(run).await;
        assert_eq!(done["status"], "completed", "the model read why and went on: {done}");
        assert!(setup.server.providers.received_calls().is_empty(), "nothing sent");
        let calls = setup.calls(run).await;
        assert_eq!(calls[0].state, CallState::Refused);
        assert_eq!(
            calls[0].failure.as_ref().map(|failure| failure.code.as_str()),
            Some(code)
        );
    }
}

/// The installation is deleted after worker A read it there, on its way to
/// parking the run on a call to it: the job its deletion queued finds the
/// run not parked yet, and waits for the park instead of missing it. The
/// call is refused once parked, nobody is asked, and nothing is sent.
#[tokio::test]
async fn refuses_a_call_parked_on_after_its_installation_went() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let (status, key) = setup
        .server
        .send(
            call(
                "alice",
                Method::POST,
                format!("/v1/organizations/{}/api-keys", setup.org),
            )
            .body(json!({ "name": "Backend", "environment": "sandbox" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{key}");
    let key = key["data"]["secret"].as_str().unwrap().to_owned();
    let run = setup.start(&setup.calling("send", "{}").await).await;
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::Park));
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    let installation = &setup.installations[0].1;
    let (status, _) = setup
        .kit
        .send(
            call(
                "alice",
                Method::DELETE,
                format!("/v1/workspaces/{}/installations/{installation}", setup.ws),
            )
            .naming(&setup.org),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // B takes the job the deletion queued: the run isn't parked yet.
    assert!(run_engine::work_once(&setup.kit.state(Worker::B)).await);
    assert_eq!(setup.get(run).await["status"], "running", "not parked yet");
    assert_eq!(setup.calls(run).await[0].state, CallState::AwaitingApproval);
    gate.release();
    assert!(a.finish().await);
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval", "A parked it");
    setup.kit.drain(Worker::B).await;
    let done = setup.get(run).await;
    assert_eq!(done["status"], "completed", "{done}");
    let calls = setup.calls(run).await;
    assert_eq!(calls[0].state, CallState::Refused);
    assert_eq!(
        calls[0].failure.as_ref().map(|failure| failure.code.as_str()),
        Some("installation_removed")
    );
    let refused = setup
        .kit
        .faults
        .writes()
        .into_iter()
        .filter(|attempted| attempted.write == Write::RefuseWaiting && attempted.result.is_ok())
        .map(|attempted| attempted.worker)
        .collect::<Vec<_>>();
    assert_eq!(refused, [Worker::B], "the job refused it once it was parked");
    let (status, body) = setup
        .kit
        .send(
            call(
                &key,
                Method::POST,
                format!("/v1/workspaces/{}/runs/{run}/approvals", setup.ws),
            )
            .body(json!({ "call_id": calls[0].id, "decision": "approved" })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "not_waiting"));
    assert!(setup.server.providers.received_calls().is_empty(), "nothing sent");
}

/// A model that asks for `per_turn` lookups a turn, `turns` times, then
/// answers.
struct Busy {
    turns: usize,
    per_turn: usize,
}

#[async_trait::async_trait]
impl ModelProvider for Busy {
    fn name(&self) -> &'static str {
        "busy"
    }

    async fn complete(&self, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        let asked = turn
            .messages
            .iter()
            .filter(|message| matches!(message, Message::Assistant { .. }))
            .count();
        let usage = TokenUsage {
            input_tokens: 1,
            output_tokens: 1,
        };
        if asked >= self.turns {
            return Ok(Completion {
                reply: Reply::Text("Done.".to_owned()),
                usage,
            });
        }
        let lookup = turn
            .tools
            .iter()
            .find(|tool| tool.description.contains(LOOKUP))
            .expect("lookup is offered");
        Ok(Completion {
            reply: Reply::ToolCalls {
                text: None,
                calls: (0..self.per_turn)
                    .map(|n| {
                        let mut arguments = Map::new();
                        arguments.insert("n".to_owned(), json!(n));
                        ModelToolCall {
                            id: format!("c{asked}_{n}"),
                            name: lookup.name.clone(),
                            arguments,
                        }
                    })
                    .collect(),
            },
            usage,
        })
    }
}

/// A run of more steps than one claim takes completes on one job attempt:
/// each claim hands the run back with a yield, which gives the attempt
/// back, rather than failing and using one.
#[tokio::test]
async fn completes_a_three_hundred_step_run_on_one_job_attempt() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("Look everything up.").await;
    let mut state = setup.kit.state(Worker::A);
    // Nine turns of twenty calls, and an answer: 381 steps.
    state.provider = Arc::new(Busy { turns: 9, per_turn: 20 });
    let (org, ws) = setup.scope();
    let mut claims = 0;
    while run_engine::work_once(&state).await {
        claims += 1;
        assert!(claims < 10, "the run keeps being given back");
        if let Some((attempts, job_state)) = setup.kit.run_job(org, ws, run).await {
            assert_eq!(
                (attempts, job_state.as_str()),
                (0, "queued"),
                "a yield gives the job's attempt back"
            );
        }
    }
    let done = setup.get(run).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["usage"]["tool_calls"], 180);
    assert_eq!(setup.server.providers.received_calls().len(), 180, "each call once");
    let committed: Vec<Write> = setup
        .kit
        .faults
        .writes()
        .into_iter()
        .filter(|attempted| attempted.worker == Worker::A && attempted.result.is_ok())
        .map(|attempted| attempted.write)
        .collect();
    assert!(committed.len() >= 300, "{} steps", committed.len());
    let yields = committed.iter().filter(|write| **write == Write::Yield).count();
    assert!(yields >= 1, "it yielded");
    assert_eq!(
        committed.iter().filter(|write| **write == Write::Claim).count(),
        yields + 1,
        "claimed once more per yield, and only then"
    );
    assert_eq!(claims, yields + 1, "each claim its job's first attempt again");
    assert_eq!(setup.kit.run_job(org, ws, run).await, None, "its job is done");
}
