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
#[path = "common/run_setup.rs"]
mod run_setup;

use axum::http::{Method, StatusCode};
use common::{call, code};
use faults::{Point, Worker, Write, accounted_tokens, kept_answers, produced_tokens};
use run_setup::{LOOKUP, Setup, main_credential, setup};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use zupflow_core::ids::RunId;
use zupflow_core::models::{
    Completion, Message, ModelProvider, ModelToolCall, ProviderAccess, ProviderError, Reply, TokenUsage, Turn,
};
use zupflow_core::runs::CallState;
use zupflow_core::runs::state::ModelAttemptState;
use zupflow_server::run_engine;

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

/// One named boundary on a worker's way: `point`, the `nth` time the worker
/// reaches it.
#[derive(Debug, Clone, Copy)]
struct Stop {
    point: Point,
    nth: usize,
}

const fn at(point: Point, nth: usize) -> Stop {
    Stop { point, nth }
}

/// Every boundary a worker crosses on a run with two calls and a closing
/// turn, before and after each: its claim; the first turn's model attempt,
/// the model's answer and the turn; each call's authorization, send and
/// outcome; the closing turn's model attempt, answer and turn, which ends
/// the run.
const TWO_CALLS: [Stop; 26] = [
    at(Point::Before(Write::Claim), 1),
    at(Point::After(Write::Claim), 1),
    at(Point::Before(Write::PrepareModel), 1),
    at(Point::After(Write::PrepareModel), 1),
    at(Point::BeforeModel, 1),
    at(Point::AfterModel, 1),
    at(Point::Before(Write::CommitTurn), 1),
    at(Point::After(Write::CommitTurn), 1),
    at(Point::Before(Write::AuthorizeCall), 1),
    at(Point::After(Write::AuthorizeCall), 1),
    at(Point::BeforeSend, 1),
    at(Point::AfterSend, 1),
    at(Point::Before(Write::RecordOutcome), 1),
    at(Point::After(Write::RecordOutcome), 1),
    at(Point::Before(Write::AuthorizeCall), 2),
    at(Point::After(Write::AuthorizeCall), 2),
    at(Point::BeforeSend, 2),
    at(Point::AfterSend, 2),
    at(Point::Before(Write::RecordOutcome), 2),
    at(Point::After(Write::RecordOutcome), 2),
    at(Point::Before(Write::PrepareModel), 2),
    at(Point::After(Write::PrepareModel), 2),
    at(Point::BeforeModel, 2),
    at(Point::AfterModel, 2),
    at(Point::Before(Write::CommitTurn), 2),
    at(Point::After(Write::CommitTurn), 2),
];

/// What becomes of whoever was stopped at a boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// Crashed there, as a process dying would.
    Dies,
    /// Let go on, late.
    ComesBack,
}

/// Whether every answer an app's server gave reaches the store: not when
/// the worker holding one dies with it.
fn answers_kept(stop: Stop, then: Then) -> bool {
    !(then == Then::Dies && matches!(stop.point, Point::AfterSend | Point::Before(Write::RecordOutcome)))
}

/// The run's ledger, state and timeline owe nothing, and it ended `status`.
async fn holds(setup: &Setup, run: RunId, asked: &[Value], stop: Stop, then: Then, status: &str) {
    let broken = setup.violations(run, asked, answers_kept(stop, then)).await;
    assert!(broken.is_empty(), "{stop:?}, {then:?}: {broken:#?}");
    let done = setup.get(run).await;
    assert_eq!(done["status"], status, "{stop:?}, {then:?}: {done}");
}

/// Worker A stops at `stop`; its lease runs out and B takes the run over,
/// where there is a run to take; A dies there or comes back; then B works
/// whatever is left.
async fn stop_and_take_over(setup: &Setup, stop: Stop, then: Then) {
    let takeover = setup.kit.take_over_at(stop.point, stop.nth).await;
    match then {
        Then::Dies => takeover.a.crash().await,
        Then::ComesBack => {
            takeover.a_gate.release();
            assert!(takeover.a.finish().await, "{stop:?}");
        }
    }
    setup.kit.drain(Worker::B).await;
}

fn two_lookups() -> Vec<Value> {
    vec![json!({ "n": 1 }), json!({ "n": 2 })]
}

async fn two_call_matrix(stops: &[Stop], then: Then) {
    for stop in stops {
        let setup = setup(&[("Main", "tk_main")]).await;
        let run = setup.run_calling(&["lookup", "lookup"], &two_lookups()).await;
        stop_and_take_over(&setup, *stop, then).await;
        holds(&setup, run, &two_lookups(), *stop, then, "completed").await;
    }
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_claim_and_first_turn_when_the_worker_dies() {
    two_call_matrix(&TWO_CALLS[..8], Then::Dies).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_claim_and_first_turn_when_the_worker_comes_back() {
    two_call_matrix(&TWO_CALLS[..8], Then::ComesBack).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_first_call_when_the_worker_dies() {
    two_call_matrix(&TWO_CALLS[8..14], Then::Dies).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_first_call_when_the_worker_comes_back() {
    two_call_matrix(&TWO_CALLS[8..14], Then::ComesBack).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_second_call_when_the_worker_dies() {
    two_call_matrix(&TWO_CALLS[14..20], Then::Dies).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_second_call_when_the_worker_comes_back() {
    two_call_matrix(&TWO_CALLS[14..20], Then::ComesBack).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_closing_turn_when_the_worker_dies() {
    two_call_matrix(&TWO_CALLS[20..], Then::Dies).await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_the_closing_turn_when_the_worker_comes_back() {
    two_call_matrix(&TWO_CALLS[20..], Then::ComesBack).await;
}

/// How the model answers the closing turn, after the first turn's calls.
enum Closing {
    /// It fails: the run ends failed, through `Finish`.
    Fails,
    /// No answer the first time it's asked: the worker yields.
    SilentOnce(AtomicBool),
}

/// The scripted model for the first turn, then `closing`.
struct ClosingTurn {
    inner: Arc<dyn ModelProvider>,
    closing: Closing,
}

#[async_trait::async_trait]
impl ModelProvider for ClosingTurn {
    fn name(&self) -> &'static str {
        "closing-turn"
    }

    async fn complete(&self, access: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        let closing = turn
            .messages
            .iter()
            .any(|message| matches!(message, Message::Assistant { .. }));
        match &self.closing {
            Closing::Fails if closing => Err(ProviderError::Failed("the closing turn fails")),
            Closing::SilentOnce(silent) if closing && !silent.swap(true, Ordering::SeqCst) => {
                Err(ProviderError::Unavailable)
            }
            _ => self.inner.complete(access, turn).await,
        }
    }
}

/// `Finish` (the closing turn's model fails) and `Yield` (it doesn't
/// answer the first time), before and after each.
async fn closing_matrix(closing: fn() -> Closing, write: Write, status: &str) {
    for then in [Then::Dies, Then::ComesBack] {
        for stop in [at(Point::Before(write), 1), at(Point::After(write), 1)] {
            let mut setup = setup(&[("Main", "tk_main")]).await;
            let inner = setup.server.state.provider.clone();
            setup.kit.set_model(Arc::new(ClosingTurn {
                inner,
                closing: closing(),
            }));
            let run = setup.run_calling(&["lookup", "lookup"], &two_lookups()).await;
            stop_and_take_over(&setup, stop, then).await;
            holds(&setup, run, &two_lookups(), stop, then, status).await;
            let calls = setup.calls(run).await;
            assert!(
                calls.iter().all(|call| call.state == CallState::Completed),
                "{stop:?}, {then:?}: both calls were done before the closing turn"
            );
        }
    }
}

#[tokio::test]
async fn holds_from_each_boundary_of_a_finish() {
    closing_matrix(|| Closing::Fails, Write::Finish, "failed").await;
}

#[tokio::test]
async fn holds_from_each_boundary_of_a_yield() {
    closing_matrix(
        || Closing::SilentOnce(AtomicBool::new(false)),
        Write::Yield,
        "completed",
    )
    .await;
}

/// A run with a call that needs approval, then a lookup: worker A stopped
/// before or after parking it (and taken over), or the API's decision
/// stopped before or after it commits; the call approved once, sent once.
#[tokio::test]
async fn holds_from_each_boundary_of_an_approval() {
    let asked = [json!({ "n": 1 }), json!({ "n": 2 })];
    let stops = [
        (Worker::A, Point::Before(Write::Park)),
        (Worker::A, Point::After(Write::Park)),
        (Worker::Api, Point::Before(Write::ResolveApproval)),
        (Worker::Api, Point::After(Write::ResolveApproval)),
    ];
    for then in [Then::Dies, Then::ComesBack] {
        for (worker, point) in stops {
            let setup = setup(&[("Main", "tk_main")]).await;
            let key = setup.api_key().await;
            let run = setup.run_calling(&["send", "lookup"], &asked).await;
            if worker == Worker::A {
                stop_and_take_over(&setup, at(point, 1), then).await;
            } else {
                setup.kit.drain(Worker::A).await;
            }
            assert_eq!(
                setup.get(run).await["status"],
                "waiting_for_approval",
                "{point:?}, {then:?}"
            );
            let call_id = setup.calls(run).await[0].id.to_string();
            let uri = format!("/v1/workspaces/{}/runs/{run}/approvals", setup.ws);
            let body = json!({ "call_id": call_id, "decision": "approved" });
            let (status, answer) = if worker == Worker::Api {
                setup
                    .api_stopped(point, then == Then::Dies, &key, uri, Some(body))
                    .await
            } else {
                setup.decide(&key, run, &call_id, "approved").await
            };
            let committed_first = worker == Worker::Api && then == Then::Dies && matches!(point, Point::After(_));
            if committed_first {
                assert_eq!(
                    (status, code(&answer)),
                    (StatusCode::CONFLICT, "not_waiting"),
                    "{point:?}"
                );
            } else {
                assert_eq!(status, StatusCode::OK, "{point:?}, {then:?}: {answer}");
            }
            setup.kit.drain(Worker::B).await;
            holds(&setup, run, &asked, at(point, 1), then, "completed").await;
            let resolved = setup
                .events(run)
                .await
                .into_iter()
                .filter(|event| event["type"] == "approval.resolved")
                .count();
            assert_eq!(resolved, 1, "{point:?}, {then:?}: decided once");
            assert_eq!(received_with(&setup, &asked[0]), 1, "{point:?}, {then:?}: sent once");
        }
    }
}

/// A cancel stopped before or after it commits, let go on or crashed and
/// sent again: while worker A holds the run between its two calls, and
/// while the run waits for an approval. Nothing is sent after it.
#[tokio::test]
async fn holds_from_each_boundary_of_a_cancel() {
    for parked in [false, true] {
        for then in [Then::Dies, Then::ComesBack] {
            for point in [Point::Before(Write::Cancel), Point::After(Write::Cancel)] {
                let setup = setup(&[("Main", "tk_main")]).await;
                let tools = if parked {
                    ["send", "lookup"]
                } else {
                    ["lookup", "lookup"]
                };
                let asked = two_lookups();
                let run = setup.run_calling(&tools, &asked).await;
                let a = if parked {
                    setup.kit.drain(Worker::A).await;
                    None
                } else {
                    let gate = setup.kit.faults.arm(Worker::A, Point::After(Write::RecordOutcome));
                    let a = setup.kit.spawn(Worker::A);
                    gate.reached().await;
                    Some((a, gate))
                };
                let uri = format!("/v1/workspaces/{}/runs/{run}/cancel", setup.ws);
                let (status, answer) = setup.api_stopped(point, then == Then::Dies, "alice", uri, None).await;
                if then == Then::Dies && matches!(point, Point::After(_)) {
                    assert_eq!(
                        (status, code(&answer)),
                        (StatusCode::CONFLICT, "run_finished"),
                        "{point:?}"
                    );
                } else {
                    assert_eq!(status, StatusCode::OK, "{point:?}, {then:?}: {answer}");
                }
                if let Some((a, gate)) = a {
                    gate.release();
                    assert!(a.finish().await);
                }
                setup.kit.drain(Worker::B).await;
                holds(&setup, run, &asked, at(point, 1), then, "cancelled").await;
                let calls = setup.calls(run).await;
                let sent: Vec<usize> = asked.iter().map(|arguments| received_with(&setup, arguments)).collect();
                let expected = if parked { [0, 0] } else { [1, 0] };
                assert_eq!(
                    sent, expected,
                    "parked {parked}, {point:?}, {then:?}: nothing after the cancel"
                );
                for (call, sent) in calls.iter().zip(sent) {
                    if sent == 0 {
                        assert_eq!(
                            (call.state, call.failure.as_ref().map(|failure| failure.code.as_str())),
                            (CallState::Refused, Some("run_cancelled")),
                            "parked {parked}, {point:?}, {then:?}: closed by the cancel"
                        );
                    }
                }
            }
        }
    }
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

    async fn complete(&self, _: &ProviderAccess<'_>, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        let asked = turn
            .messages
            .iter()
            .filter(|message| matches!(message, Message::Assistant { .. }))
            .count();
        let usage = TokenUsage::new(1, 1, 0, 0);
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
