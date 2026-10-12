//! What operators see of runs (the operations standard): a structured log
//! line for every transition and every send, carrying the run's ID with its
//! organization, workspace and environment and never what a run carries,
//! and the metrics for runs and their tool calls.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::call;
use faults::{Point, Worker, Write};
use run_setup::{Setup, setup};
use serde_json::{Value, json};
use tracing_subscriber::fmt::MakeWriter;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{OrganizationId, RunId, WorkspaceId};
use zupflow_core::models::{Completion, ModelProvider, ProviderError, Turn};
use zupflow_core::runs::CallState;
use zupflow_server::run_engine;

/// What a run carries and no log may show: in its input, a call's
/// arguments, the tool's answer (which echoes them) and the model's text.
const PAYLOAD: &str = "plum-kumquat-7731";

thread_local! {
    /// Where this thread's log lines go while a test captures them.
    static CAPTURING: std::cell::RefCell<Option<Arc<Mutex<Vec<u8>>>>> = const { std::cell::RefCell::new(None) };
}

/// Writes a line to the capture of the thread that logged it, if any.
struct ToThisThread;

impl io::Write for ToThisThread {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        CAPTURING.with(|capturing| {
            if let Some(buffer) = capturing.borrow().as_ref() {
                buffer.lock().unwrap().extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct ThreadWriters;

impl<'a> MakeWriter<'a> for ThreadWriters {
    type Writer = ToThisThread;

    fn make_writer(&'a self) -> ToThisThread {
        ToThisThread
    }
}

/// One subscriber for the whole test binary, set before any test logs:
/// a subscriber set per thread can miss lines while another thread caches
/// what a call site wants.
fn subscribe_once() {
    static SUBSCRIBED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    SUBSCRIBED.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(ThreadWriters)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
    });
}

/// Every line this thread logs while it's held, as JSON.
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Drop for Captured {
    fn drop(&mut self) {
        CAPTURING.with(|capturing| capturing.borrow_mut().take());
    }
}

impl Captured {
    /// Captures what this thread logs, down to `debug`, until it drops.
    fn start() -> Self {
        subscribe_once();
        let buffer = Arc::new(Mutex::new(Vec::new()));
        CAPTURING.with(|capturing| *capturing.borrow_mut() = Some(buffer.clone()));
        Self(buffer)
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    /// Each line's fields, with its level.
    fn lines(&self) -> Vec<Value> {
        self.text()
            .lines()
            .map(|line| {
                let line: Value = serde_json::from_str(line).unwrap();
                let mut fields = line["fields"].clone();
                fields["level"] = line["level"].clone();
                fields
            })
            .collect()
    }

    /// The lines of one event.
    fn of(&self, event: &str) -> Vec<Value> {
        self.lines().into_iter().filter(|line| line["event"] == event).collect()
    }
}

#[tokio::test]
async fn logs_each_transition_and_send_with_the_run_and_never_its_payload() {
    subscribe_once();
    let setup = setup(&[("Main", "tk_main")]).await;
    let captured = Captured::start();
    let run = setup
        .start(&setup.calling("lookup", &json!({ "note": PAYLOAD }).to_string()).await)
        .await;
    setup.kit.drain(Worker::A).await;
    let done = setup.get(run).await;
    assert_eq!(done["status"], "completed", "{done}");
    assert!(
        done["output"].as_str().unwrap().contains(PAYLOAD),
        "the payload went through the run: {done}"
    );
    let text = captured.text();
    assert!(!text.contains(PAYLOAD), "a log line carries the run's payload: {text}");
    assert!(!text.contains("tk_main"), "a log line carries the credential");

    let transitions = captured.of("runs.transition");
    let operations: Vec<&str> = transitions
        .iter()
        .map(|line| line["operation"].as_str().unwrap())
        .collect();
    assert_eq!(
        operations,
        [
            "admit",
            "claim",
            "prepare_model",
            "commit_turn",
            "authorize_call",
            "record_outcome",
            "prepare_model",
            "commit_turn"
        ]
    );
    let (org, ws) = (setup.org.as_str(), setup.ws.as_str());
    for line in transitions.iter().chain(&captured.of("runs.call_answered")) {
        assert_eq!(line["run_id"], run.to_string(), "{line}");
        assert_eq!(line["organization_id"], org, "{line}");
        assert_eq!(line["workspace_id"], ws, "{line}");
        assert_eq!(line["environment"], "sandbox", "{line}");
    }
    let call_id = setup.calls(run).await[0].id.to_string();
    let authorized = &transitions[4];
    assert_eq!(
        (&authorized["level"], &authorized["call_id"], &authorized["detail"]),
        (&json!("INFO"), &json!(call_id), &json!("send"))
    );
    let recorded = &transitions[5];
    assert_eq!(
        (&recorded["call_id"], &recorded["detail"]),
        (&json!(call_id), &json!("completed"))
    );
    let ended = transitions.last().unwrap();
    assert_eq!(
        (&ended["level"], &ended["status"]),
        (&json!("INFO"), &json!("completed"))
    );
    assert_eq!(transitions[1]["level"], "DEBUG", "a claim is routine");
    let answered = captured.of("runs.call_answered");
    assert_eq!(answered.len(), 1);
    assert_eq!(answered[0]["call_id"], call_id);
    assert_eq!(answered[0]["outcome"], "completed");
    assert!(answered[0]["latency_ms"].is_u64(), "{}", answered[0]);
}

/// A model that answers its first turn as scripted and then fails.
struct FailsAfterFirstTurn(Arc<dyn ModelProvider>);

#[async_trait::async_trait]
impl ModelProvider for FailsAfterFirstTurn {
    fn name(&self) -> &'static str {
        "fails-after-first-turn"
    }

    async fn complete(&self, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        if turn.messages.len() > 1 {
            return Err(ProviderError::Failed("scripted failure"));
        }
        self.0.complete(turn).await
    }
}

/// How many times `name` (with `label`) was measured in the setup's workspace.
fn measured(setup: &Setup, name: &str, label: Option<&str>) -> u64 {
    let (org, ws) = setup.scope();
    setup.kit.metrics.count(org, ws, name, label)
}

#[tokio::test]
async fn measures_runs_and_their_tool_calls() {
    subscribe_once();
    let mut setup = setup(&[("Main", "tk_main")]).await;
    let key = setup.api_key().await;

    // Completed, with a call done.
    let first = setup.start(&setup.calling("lookup", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(first).await["status"], "completed");
    assert_eq!(measured(&setup, "runs_started", None), 1);
    assert_eq!(measured(&setup, "runs_completed", None), 1);
    assert_eq!(measured(&setup, "tool_calls", Some("completed")), 1);
    let (org, ws) = setup.scope();
    let latency = setup
        .kit
        .metrics
        .figures(org, ws, Environment::Sandbox, "tool_calls", Some("completed"))
        .unwrap();
    assert_eq!(latency.latency_buckets.iter().sum::<u64>(), 1, "timed");

    // Waiting for a decision, then denied.
    let waiting = setup.start(&setup.calling("send", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(measured(&setup, "runs_waiting", None), 1);
    let call_id = setup.calls(waiting).await[0].id.to_string();
    assert_eq!(setup.decide(&key, waiting, &call_id, "denied").await.0, StatusCode::OK);
    setup.kit.drain(Worker::A).await;
    assert_eq!(measured(&setup, "runs_completed", None), 2);

    // Cancelled while it waits.
    let cancelled = setup.start(&setup.calling("send", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    setup.cancel(cancelled).await;
    assert_eq!(measured(&setup, "runs_cancelled", None), 1);

    // A worker dies with the answer, after the send: settled unknown once
    // its send lease lapses. And one dies before recording its model
    // answer: that attempt is abandoned.
    let unknown = setup.start(&setup.calling("lookup", "{}").await).await;
    let gate = setup.kit.faults.arm(Worker::A, Point::AfterSend);
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    a.crash().await;
    setup.kit.drain(Worker::B).await;
    assert_eq!(setup.calls(unknown).await[0].state, CallState::Unknown);
    assert_eq!(measured(&setup, "tool_calls_settled_unknown", None), 1);
    let abandoned = setup.start("hello").await;
    let gate = setup.kit.faults.arm(Worker::A, Point::Before(Write::CommitTurn));
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    a.crash().await;
    setup.kit.drain(Worker::B).await;
    assert_eq!(setup.get(abandoned).await["status"], "completed");
    assert_eq!(measured(&setup, "model_attempts_abandoned", None), 1);

    // Failed: the model fails after the first turn.
    let inner = setup.server.state.provider.clone();
    setup.kit.set_model(Arc::new(FailsAfterFirstTurn(inner)));
    let failed = setup.start(&setup.calling("lookup", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    let done = setup.get(failed).await;
    assert_eq!(done["status"], "failed", "{done}");
    assert_eq!(measured(&setup, "runs_failed", Some("model_failed")), 1);
    let scripted = setup.server.state.provider.clone();
    setup.kit.set_model(scripted);

    // Refused at send: its installation went after the turn named it.
    let refused = setup.start(&setup.calling("lookup", "{}").await).await;
    let gate = setup.kit.faults.arm(Worker::A, Point::After(Write::CommitTurn));
    let a = setup.kit.spawn(Worker::A);
    gate.reached().await;
    let installation = setup.installations[0].1.clone();
    let (status, _) = setup
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
    assert_eq!(status, StatusCode::OK);
    gate.release();
    assert!(a.finish().await);
    setup.kit.drain(Worker::B).await;
    assert_eq!(setup.calls(refused).await[0].state, CallState::Refused);
    assert_eq!(
        measured(&setup, "tool_calls_refused", Some("installation_disconnected")),
        1
    );

    // Seven runs started here; nothing measured in another workspace.
    assert_eq!(measured(&setup, "runs_started", None), 7);
    assert_eq!(
        setup
            .kit
            .metrics
            .count(org, WorkspaceId::generate(), "runs_started", None),
        0
    );
    assert_eq!(setup.kit.metrics.dropped(), 0);
}

/// The job refusing the calls waiting on an installation that went can't
/// read the runs waiting on it, and dies after its attempts: that is logged,
/// and the call it should have refused is still refused when it would be
/// sent, after a decision, without reaching the app's server.
#[tokio::test]
async fn logs_a_refusal_job_that_died_and_still_refuses_the_call_at_send() {
    subscribe_once();
    let setup = setup(&[("Main", "tk_main")]).await;
    let key = setup.api_key().await;
    let run = setup.start(&setup.calling("send", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
    let captured = Captured::start();
    setup
        .kit
        .faults
        .runs_waiting_on_unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let installation = setup.installations[0].1.clone();
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
    let state = setup.kit.state(Worker::B);
    let mut claims = 0;
    for _ in 0..20 {
        while run_engine::work_once(&state).await {
            claims += 1;
        }
        setup.kit.clock.advance(Duration::from_secs(60));
    }
    assert_eq!(claims, 5, "tried on each of its attempts");
    let exhausted = captured.of("runs.refusals_job_exhausted");
    assert_eq!(exhausted.len(), 1, "{}", captured.text());
    assert_eq!(exhausted[0]["level"], "WARN");
    assert_eq!(exhausted[0]["installation_id"], installation);
    assert_eq!(exhausted[0]["workspace_id"], setup.ws);
    assert_eq!(exhausted[0]["organization_id"], setup.org);
    assert_eq!(exhausted[0]["environment"], "sandbox");
    assert_eq!(exhausted[0]["why"], "store_unavailable");
    setup
        .kit
        .faults
        .runs_waiting_on_unavailable
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // Still waiting, so the decision is taken; the send refuses the call.
    assert_eq!(setup.get(run).await["status"], "waiting_for_approval");
    let call_id = setup.calls(run).await[0].id.to_string();
    let (status, body) = setup.decide(&key, run, &call_id, "approved").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.kit.drain(Worker::A).await;
    let done = setup.get(run).await;
    assert_eq!(done["status"], "completed", "{done}");
    let calls = setup.calls(run).await;
    assert_eq!(calls[0].state, CallState::Refused);
    assert_eq!(
        calls[0].failure.as_ref().map(|failure| failure.code.as_str()),
        Some("installation_removed")
    );
    assert!(setup.server.providers.received_calls().is_empty(), "nothing sent");
}

/// A tool's own error and a denial's reason reach the run, and neither
/// reaches a log line.
#[tokio::test]
async fn never_logs_a_tool_error_or_a_denials_reason() {
    subscribe_once();
    let setup = setup(&[("Main", "tk_main")]).await;
    let key = setup.api_key().await;
    let captured = Captured::start();
    let failing = setup
        .start(
            &setup
                .calling("lookup", &json!({ "note": PAYLOAD, "fail": true }).to_string())
                .await,
        )
        .await;
    setup.kit.drain(Worker::A).await;
    let calls = setup.calls(failing).await;
    assert_eq!(calls[0].state, CallState::Failed);
    assert!(
        calls[0].outcome.as_ref().unwrap().content.contains(PAYLOAD),
        "the tool's error went back to the model: {:?}",
        calls[0].outcome
    );
    let denied = setup.start(&setup.calling("send", "{}").await).await;
    setup.kit.drain(Worker::A).await;
    let call_id = setup.calls(denied).await[0].id.to_string();
    let (status, body) = setup
        .kit
        .send(
            call(
                &key,
                Method::POST,
                format!("/v1/workspaces/{}/runs/{denied}/approvals", setup.ws),
            )
            .body(json!({ "call_id": call_id, "decision": "denied", "reason": format!("Not now, {PAYLOAD}") })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    setup.kit.drain(Worker::A).await;
    assert_eq!(setup.get(denied).await["status"], "completed");
    let calls = setup.calls(denied).await;
    assert_eq!(calls[0].state, CallState::Denied);
    assert!(
        calls[0].outcome.as_ref().unwrap().content.contains(PAYLOAD),
        "the reason went back to the model: {:?}",
        calls[0].outcome
    );
    let text = captured.text();
    assert!(!text.contains(PAYLOAD), "a log line carries a run's payload: {text}");
    let answered = captured.of("runs.call_answered");
    assert_eq!(answered.len(), 1);
    assert_eq!(
        (&answered[0]["outcome"], &answered[0]["level"]),
        (&json!("tool_error"), &json!("WARN"))
    );
    let decided: Vec<Value> = captured
        .of("runs.transition")
        .into_iter()
        .filter(|line| line["operation"] == "resolve_approval")
        .collect();
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0]["detail"], "denied");
    assert_eq!(decided[0]["call_id"], call_id);
    let (org, ws) = setup.scope();
    assert_eq!(setup.kit.metrics.count(org, ws, "tool_calls", Some("tool_error")), 1);
}

/// Runs in a sibling workspace of the same organization, in another
/// organization and in a Production workspace (started with a live API
/// key): each run's log lines and measurements are under its own
/// organization, workspace and environment, and nothing of one shows under
/// another's.
#[tokio::test]
async fn logs_and_measures_each_run_in_its_own_scope_only() {
    subscribe_once();
    let setup = setup(&[("Main", "tk_main")]).await;
    let sibling = setup.sibling("sibling", "tk_sibling").await;
    let other = setup.other_organization("bob", "larkspur", "tk_other").await;
    let live = setup.production("live", "tk_live").await;
    let captured = Captured::start();
    let main_run = setup.start(&setup.calling("lookup", "{}").await).await;
    // The sibling's run waits for a decision; the other organization's
    // two runs each make a call.
    let sibling_run = setup.start_in(&sibling, "send", &json!({})).await;
    let other_runs = [
        setup.start_in(&other, "lookup", &json!({ "n": 1 })).await,
        setup.start_in(&other, "lookup", &json!({ "n": 2 })).await,
    ];
    let live_run = setup.start_in(&live, "lookup", &json!({})).await;
    setup.kit.drain(Worker::A).await;
    let main = setup.scope();
    let (sibling_scope, other_scope, live_scope) = (sibling.scope(), other.scope(), live.scope());
    let status = |org: OrganizationId, ws: WorkspaceId, run: RunId| {
        let kit = &setup.kit;
        async move { kit.progress(org, ws, run).await.run.status.as_str() }
    };
    assert_eq!(status(main.0, main.1, main_run).await, "completed");
    assert_eq!(
        status(sibling_scope.0, sibling_scope.1, sibling_run).await,
        "waiting_for_approval"
    );
    for run in other_runs {
        assert_eq!(status(other_scope.0, other_scope.1, run).await, "completed");
    }
    assert_eq!(status(live_scope.0, live_scope.1, live_run).await, "completed");

    let metrics = &setup.kit.metrics;
    let figures = |(org, ws): (OrganizationId, WorkspaceId)| {
        ["runs_started", "runs_waiting", "runs_completed", "tool_calls"].map(|name| metrics.count(org, ws, name, None))
    };
    assert_eq!(figures(main), [1, 0, 1, 1], "the setup's workspace");
    assert_eq!(figures(sibling_scope), [1, 1, 0, 0], "the sibling workspace");
    assert_eq!(figures(other_scope), [2, 0, 2, 2], "the other organization");
    assert_eq!(figures(live_scope), [1, 0, 1, 1], "the Production workspace");
    let in_production = |name| metrics.count_in(live_scope.0, live_scope.1, Environment::Production, name, None);
    assert_eq!(
        (in_production("runs_started"), in_production("runs_completed")),
        (1, 1),
        "measured as Production"
    );
    assert!(
        metrics
            .figures(
                live_scope.0,
                live_scope.1,
                Environment::Production,
                "tool_calls",
                Some("completed")
            )
            .is_some()
    );
    assert_eq!(
        metrics.figures(
            live_scope.0,
            live_scope.1,
            Environment::Sandbox,
            "tool_calls",
            Some("completed")
        ),
        None
    );
    // No measurement under one scope's organization and another's workspace.
    assert_eq!(
        figures((main.0, sibling_scope.1)),
        [1, 1, 0, 0],
        "the sibling is the setup organization's"
    );
    for (org, ws) in [
        (main.0, other_scope.1),
        (other_scope.0, main.1),
        (other_scope.0, sibling_scope.1),
        (other_scope.0, live_scope.1),
    ] {
        assert_eq!(figures((org, ws)), [0, 0, 0, 0], "{org} with {ws}");
    }
    let known = [
        (main, Environment::Sandbox),
        (sibling_scope, Environment::Sandbox),
        (other_scope, Environment::Sandbox),
        (live_scope, Environment::Production),
    ];
    for (series, _) in metrics.snapshot() {
        assert!(
            known.contains(&((series.organization_id, series.workspace_id), series.environment)),
            "a series under {}, {} and {}",
            series.organization_id,
            series.workspace_id,
            series.environment
        );
    }

    let mut runs = vec![
        (main_run, main, "sandbox"),
        (sibling_run, sibling_scope, "sandbox"),
        (live_run, live_scope, "production"),
    ];
    runs.extend(other_runs.map(|run| (run, other_scope, "sandbox")));
    let lines: Vec<Value> = captured
        .of("runs.transition")
        .into_iter()
        .chain(captured.of("runs.call_answered"))
        .collect();
    for (run, (org, ws), environment) in &runs {
        let own: Vec<&Value> = lines.iter().filter(|line| line["run_id"] == run.to_string()).collect();
        assert!(!own.is_empty(), "{run} was logged");
        for line in own {
            assert_eq!(line["organization_id"], org.to_string(), "{line}");
            assert_eq!(line["workspace_id"], ws.to_string(), "{line}");
            assert_eq!(line["environment"], *environment, "{line}");
        }
    }
    assert!(
        lines
            .iter()
            .all(|line| runs.iter().any(|(run, _, _)| line["run_id"] == run.to_string())),
        "every line is one of these runs'"
    );
}

/// A run's job that can't read its run on any of its attempts dies: that
/// is logged with its scope and environment, and the sweep fails the run.
#[tokio::test]
async fn logs_a_run_job_that_died_and_the_sweep_fails_its_run() {
    subscribe_once();
    let setup = setup(&[("Main", "tk_main")]).await;
    let run = setup.start("hello").await;
    let captured = Captured::start();
    setup
        .kit
        .faults
        .snapshots_unavailable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let state = setup.kit.state(Worker::A);
    let mut claims = 0;
    for _ in 0..20 {
        while run_engine::work_once(&state).await {
            claims += 1;
        }
        setup.kit.clock.advance(Duration::from_secs(60));
    }
    assert_eq!(claims, 5, "tried on each of its attempts");
    let exhausted = captured.of("runs.job_exhausted");
    assert_eq!(exhausted.len(), 1, "{}", captured.text());
    assert_eq!(exhausted[0]["level"], "WARN");
    assert_eq!(exhausted[0]["run_id"], run.to_string());
    assert_eq!(exhausted[0]["organization_id"], setup.org);
    assert_eq!(exhausted[0]["workspace_id"], setup.ws);
    assert_eq!(exhausted[0]["environment"], "sandbox");
    assert_eq!(exhausted[0]["why"], "store_unavailable");
    setup
        .kit
        .faults
        .snapshots_unavailable
        .store(false, std::sync::atomic::Ordering::SeqCst);
    run_engine::sweep(&state).await;
    let done = setup.get(run).await;
    assert_eq!(
        (&done["status"], &done["error"]["code"]),
        (&json!("failed"), &json!("run_unavailable")),
        "{done}"
    );
    let (org, ws) = setup.scope();
    assert_eq!(
        setup.kit.metrics.count(org, ws, "runs_failed", Some("run_unavailable")),
        1
    );
}
