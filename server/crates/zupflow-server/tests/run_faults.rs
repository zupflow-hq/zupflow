//! The run engine under faults, with the kit in `common/faults.rs`: workers
//! stopped or crashed at named points, leases running out, two workers on
//! one run, and what the app's server and the model actually saw.
//!
//! The tests without `ignore` hold today. Each test marked
//! `#[ignore = "fixed when …: …"]` reproduces a known hole in today's
//! engine: it asserts the behaviour the engine owes and fails on the
//! assertion that names the hole. Run them with
//! `cargo test -p zupflow-server --test run_faults -- --ignored`. The change
//! that closes a hole removes its `ignore`.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use faults::{Kit, Point, Worker, Write, accounted_tokens, kept_answers, produced_tokens};
use serde_json::{Value, json};
use zupflow_core::ids::{OrganizationId, RunId, WorkspaceId};
use zupflow_core::runs::{CallState, RunToolCall};

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

    async fn calls(&self, run: RunId) -> Vec<RunToolCall> {
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
    let run = setup.start("call lookup {}").await;
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
    assert!(
        done["output"].as_str().unwrap().starts_with("lookup failed: "),
        "{done}"
    );
}

#[tokio::test]
async fn never_sends_again_a_call_whose_answer_was_lost() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("call lookup {}").await;
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
    let run = setup.start("call lookup {}").await;
    // A recorded the model's turn, then stopped; B takes the run over.
    let takeover = setup.kit.take_over(Point::After(Write::RecordTurn), None).await;
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
#[ignore = "fixed when sends are fenced by a send claim: after a takeover, the answer the app's server gave the slow worker is lost"]
async fn keeps_the_answer_a_slow_worker_gets_after_a_takeover() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("call lookup {}").await;
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
#[ignore = "fixed when cancel closes open calls: cancelling a run leaves its unstarted calls open"]
async fn closes_the_calls_a_cancel_left_unstarted() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("call lookup {}\ncall lookup {\"page\":2}").await;
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::BeginCall));
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
#[ignore = "fixed when cancel closes open calls: cancelling a run leaves its call waiting for approval open"]
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
    let run = setup.start("call send {}").await;
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
#[ignore = "fixed when model attempts are recorded: a model answer lost before its turn was recorded is asked for again and never counted"]
async fn accounts_for_a_model_answer_lost_before_its_turn_was_recorded() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("hello").await;
    // The model answered; A dies before recording the turn.
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::RecordTurn));
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    a.crash().await;
    setup.kit.drain(Worker::B).await;
    let (org, ws) = setup.scope();
    let progress = setup.kit.progress(org, ws, run).await;
    assert_eq!(setup.get(run).await["status"], "completed");
    assert_eq!(progress.turns.len(), 1, "one turn recorded");
    let answers = setup.kit.faults.answers();
    assert_eq!(answers.len(), 2, "the model answered turn 0 twice: {answers:?}");
    assert_eq!(
        accounted_tokens(&progress),
        produced_tokens(&answers),
        "tokens the model spent on the lost answer aren't accounted for"
    );
}

#[tokio::test]
#[ignore = "fixed when writes check the owner's lease: a worker whose lease ran out still commits its writes"]
async fn refuses_the_writes_of_a_worker_whose_lease_ran_out() {
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("hello").await;
    // A has the model's answer and is about to record it when its lease
    // runs out; B claims the run and is waiting on the model.
    let takeover = setup
        .kit
        .take_over(Point::Before(Write::RecordTurn), Some(Point::BeforeModel))
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
#[ignore = "fixed when tools are named per installation: two installations of one app are offered under the same description, as `x` and `x_2`"]
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
#[ignore = "fixed when tools are named per installation: which installation a tool name reaches depends on the order of the agent's grants"]
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
