//! A deterministic fault kit for the run engine: a clock the test moves,
//! named points around every run-store write, the model's answer and the
//! outbound MCP `tools/call` where a worker can be stopped or crashed, and
//! the race where a second worker takes a run over from a first whose lease
//! ran out.
//!
//! Each worker is the real engine (`run_engine::work_once`) over its own
//! copy of the state, whose run store, model provider and outbound path are
//! wrapped to stop at the points a test arms. A worker stopped at a point
//! holds no transaction; crashing it drops it there, as a process dying
//! would. What happened is read from records the runtime can't rewrite: the
//! fake MCP server's ledger (`Providers::ledger`), the answers the model
//! actually produced (`Faults::answers`), and every write each worker tried
//! with whether it committed (`Faults::writes`).
//!
//! Include it beside `common` with `#[path = "common/faults.rs"] mod faults;`.

#![allow(dead_code, clippy::unwrap_used, clippy::too_many_arguments)]

use std::future::Future;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zupflow_core::audit::Audit;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{ApiKeyId, OrganizationId, RunId, ToolCallId, WorkspaceId};
use zupflow_core::models::{Completion, Message, ModelProvider, ProviderError, TokenUsage, ToolSpec, Turn};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse};
use zupflow_core::runs::{
    AgentActivity, ApprovalDecision, CallOutcome, CallState, Ending, NewRun, NewRunEvent, NewTurn, OrganizationRun,
    OrganizationRunFilter, Page, QueuedJob, RecordedDecision, Run, RunError, RunEvent, RunFilter, RunProgress,
    RunStore, RunToolCall, UsageDay,
};
use zupflow_core::time::{Clock, Timestamp};
use zupflow_server::{AppState, RequestGuard, run_engine};
use zupflow_sqlite::SqliteJobQueue;

use crate::common::{Call, Server, send_to};

/// How long a test waits for a worker to reach a point or finish its claim.
const PATIENCE: Duration = Duration::from_secs(20);

/// A clock that stands still until the test moves it. It starts an hour
/// ahead of the wall clock: jobs the stores queue with their changes take
/// the wall clock's time, so they are already due by this one.
pub struct TestClock(AtomicI64);

impl TestClock {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(Timestamp::now().millis() + 3_600_000)))
    }

    pub fn advance(&self, by: Duration) {
        self.0
            .fetch_add(i64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0.load(Ordering::SeqCst))
    }
}

/// Who acts: the API (its routes' run-store writes), or a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Worker {
    Api,
    A,
    B,
}

/// Each write of the run store, by the method that makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    CreateRun,
    StartRun,
    RecordTurn,
    RequestApproval,
    ResolveApproval,
    BeginCall,
    SettleCall,
    DecideCall,
    FinishRun,
    CancelRun,
}

/// A named boundary a worker can be stopped at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// The write is about to be made: nothing of it is committed.
    Before(Write),
    /// The write returned, committed or refused, and the worker has not
    /// acted on its answer yet.
    After(Write),
    /// The model is about to be asked.
    BeforeModel,
    /// The model answered, and the worker has not seen the answer yet.
    AfterModel,
    /// A `tools/call` is about to leave: the app's server hasn't received it.
    BeforeSend,
    /// The app's server received the `tools/call` and answered; the answer
    /// hasn't reached the worker. Crashing here loses the answer.
    AfterSend,
}

/// Where one armed point stands: reached by its worker, and let go by the test.
pub struct Gate {
    worker: Worker,
    point: Point,
    reached: watch::Sender<bool>,
    released: watch::Sender<bool>,
}

impl Gate {
    /// Waits until the worker stops here.
    pub async fn reached(&self) {
        let mut reached = self.reached.subscribe();
        let outcome =
            tokio::time::timeout(PATIENCE, async { reached.wait_for(|reached| *reached).await.is_ok() }).await;
        assert!(
            matches!(outcome, Ok(true)),
            "{:?} never reached {:?}",
            self.worker,
            self.point
        );
    }

    /// Lets the worker go on.
    pub fn release(&self) {
        self.released.send_replace(true);
    }

    async fn hold(&self) {
        self.reached.send_replace(true);
        let mut released = self.released.subscribe();
        let _ = released.wait_for(|released| *released).await;
    }
}

/// One write a worker tried: whether it committed, or the store's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempted {
    pub worker: Worker,
    pub write: Write,
    pub result: Result<(), RunError>,
}

/// One answer the model produced, whatever became of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub worker: Worker,
    /// The turn it answered: how many assistant messages it was shown.
    pub turn: usize,
    /// The tools it was offered.
    pub tools: Vec<ToolSpec>,
    pub usage: TokenUsage,
}

/// The armed points, and what the workers did.
#[derive(Default)]
pub struct Faults {
    armed: Mutex<Vec<Arc<Gate>>>,
    writes: Mutex<Vec<Attempted>>,
    answers: Mutex<Vec<Answer>>,
}

impl Faults {
    /// Stops `worker` the next time it reaches `point`, once.
    pub fn arm(&self, worker: Worker, point: Point) -> Arc<Gate> {
        let gate = Arc::new(Gate {
            worker,
            point,
            reached: watch::Sender::new(false),
            released: watch::Sender::new(false),
        });
        self.armed.lock().unwrap().push(gate.clone());
        gate
    }

    async fn pass(&self, worker: Worker, point: Point) {
        let gate = {
            let mut armed = self.armed.lock().unwrap();
            armed
                .iter()
                .position(|gate| gate.worker == worker && gate.point == point)
                .map(|at| armed.remove(at))
        };
        if let Some(gate) = gate {
            gate.hold().await;
        }
    }

    /// Every write tried so far, in order.
    pub fn writes(&self) -> Vec<Attempted> {
        self.writes.lock().unwrap().clone()
    }

    /// How many writes were tried so far: a mark for [`Faults::committed_since`].
    pub fn mark(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    /// The writes `worker` committed after `mark`.
    pub fn committed_since(&self, mark: usize, worker: Worker) -> Vec<Attempted> {
        self.writes.lock().unwrap()[mark..]
            .iter()
            .filter(|attempted| attempted.worker == worker && attempted.result.is_ok())
            .cloned()
            .collect()
    }

    /// Every answer the model produced, in order.
    pub fn answers(&self) -> Vec<Answer> {
        self.answers.lock().unwrap().clone()
    }
}

/// The run store, stopping its worker around each write and noting the write.
struct FaultyRuns {
    inner: Arc<dyn RunStore>,
    worker: Worker,
    faults: Arc<Faults>,
}

impl FaultyRuns {
    async fn write<T: Send>(
        &self,
        write: Write,
        call: impl Future<Output = Result<T, RunError>> + Send,
    ) -> Result<T, RunError> {
        self.faults.pass(self.worker, Point::Before(write)).await;
        let result = call.await;
        self.faults.writes.lock().unwrap().push(Attempted {
            worker: self.worker,
            write,
            result: result.as_ref().map(|_| ()).map_err(|error| *error),
        });
        self.faults.pass(self.worker, Point::After(write)).await;
        result
    }
}

#[async_trait]
impl RunStore for FaultyRuns {
    async fn create_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        run: &NewRun,
        events: &[NewRunEvent],
        job: &QueuedJob,
        now: Timestamp,
    ) -> Result<(Run, Vec<RunEvent>), RunError> {
        self.write(
            Write::CreateRun,
            self.inner
                .create_run(organization_id, workspace_id, run_id, run, events, job, now),
        )
        .await
    }

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Run>, RunError> {
        self.inner.find_run(organization_id, workspace_id, run_id).await
    }

    async fn list_runs(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &RunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Run>, RunError> {
        self.inner
            .list_runs(organization_id, workspace_id, filter, cursor, limit)
            .await
    }

    async fn list_organization_runs(
        &self,
        organization_id: OrganizationId,
        filter: &OrganizationRunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<OrganizationRun>, RunError> {
        self.inner
            .list_organization_runs(organization_id, filter, cursor, limit)
            .await
    }

    async fn list_events(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        after: u64,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.inner
            .list_events(organization_id, workspace_id, run_id, after)
            .await
    }

    async fn agent_activity(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        since: Timestamp,
    ) -> Result<Vec<AgentActivity>, RunError> {
        self.inner.agent_activity(organization_id, workspace_id, since).await
    }

    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        since: Timestamp,
    ) -> Result<Vec<UsageDay>, RunError> {
        self.inner.usage(organization_id, environment, since).await
    }

    async fn find_progress(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<RunProgress>, RunError> {
        self.inner.find_progress(organization_id, workspace_id, run_id).await
    }

    async fn start_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::StartRun,
            self.inner.start_run(organization_id, workspace_id, run_id, events, now),
        )
        .await
    }

    async fn record_turn(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        index: u32,
        turn: &NewTurn,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::RecordTurn,
            self.inner
                .record_turn(organization_id, workspace_id, run_id, index, turn, events, now),
        )
        .await
    }

    async fn request_approval(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::RequestApproval,
            self.inner
                .request_approval(organization_id, workspace_id, run_id, call_id, events, now),
        )
        .await
    }

    async fn resolve_approval(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        decision: ApprovalDecision,
        api_key_id: ApiKeyId,
        denial: &CallOutcome,
        events: &[NewRunEvent],
        job: &QueuedJob,
        now: Timestamp,
        audit: &Audit<'_, Run>,
    ) -> Result<(Run, Vec<RunEvent>), RunError> {
        self.write(
            Write::ResolveApproval,
            self.inner.resolve_approval(
                organization_id,
                workspace_id,
                run_id,
                call_id,
                decision,
                api_key_id,
                denial,
                events,
                job,
                now,
                audit,
            ),
        )
        .await
    }

    async fn begin_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::BeginCall,
            self.inner
                .begin_call(organization_id, workspace_id, run_id, call_id, events, now),
        )
        .await
    }

    async fn settle_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        from: &[CallState],
        to: CallState,
        outcome: &CallOutcome,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::SettleCall,
            self.inner.settle_call(
                organization_id,
                workspace_id,
                run_id,
                call_id,
                from,
                to,
                outcome,
                events,
                now,
            ),
        )
        .await
    }

    async fn decide_call(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        call_id: ToolCallId,
        from: &[CallState],
        to: CallState,
        decision: &RecordedDecision,
        outcome: Option<&CallOutcome>,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::DecideCall,
            self.inner.decide_call(
                organization_id,
                workspace_id,
                run_id,
                call_id,
                from,
                to,
                decision,
                outcome,
                events,
                now,
            ),
        )
        .await
    }

    async fn finish_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        turns: u32,
        ending: &Ending,
        events: &[NewRunEvent],
        now: Timestamp,
    ) -> Result<Vec<RunEvent>, RunError> {
        self.write(
            Write::FinishRun,
            self.inner
                .finish_run(organization_id, workspace_id, run_id, turns, ending, events, now),
        )
        .await
    }

    async fn abandoned_runs(&self, limit: u32) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, RunError> {
        self.inner.abandoned_runs(limit).await
    }

    async fn cancel_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
        events: &[NewRunEvent],
        now: Timestamp,
        audit: &Audit<'_, Run>,
    ) -> Result<(Run, Vec<RunEvent>), RunError> {
        self.write(
            Write::CancelRun,
            self.inner
                .cancel_run(organization_id, workspace_id, run_id, events, now, audit),
        )
        .await
    }
}

/// The model, stopping its worker around each answer and keeping every
/// answer it produced.
struct FaultyModel {
    inner: Arc<dyn ModelProvider>,
    worker: Worker,
    faults: Arc<Faults>,
}

#[async_trait]
impl ModelProvider for FaultyModel {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    async fn complete(&self, turn: &Turn<'_>) -> Result<Completion, ProviderError> {
        self.faults.pass(self.worker, Point::BeforeModel).await;
        let completion = self.inner.complete(turn).await;
        if let Ok(completion) = &completion {
            self.faults.answers.lock().unwrap().push(Answer {
                worker: self.worker,
                turn: turn
                    .messages
                    .iter()
                    .filter(|message| matches!(message, Message::Assistant { .. }))
                    .count(),
                tools: turn.tools.to_vec(),
                usage: completion.usage,
            });
        }
        self.faults.pass(self.worker, Point::AfterModel).await;
        completion
    }
}

/// The outbound path, stopping its worker around each `tools/call`.
struct FaultyOutbound {
    inner: Arc<dyn OutboundHttp>,
    worker: Worker,
    faults: Arc<Faults>,
}

#[async_trait]
impl OutboundHttp for FaultyOutbound {
    async fn send(&self, request: OutboundRequest, limits: Limits) -> Result<OutboundResponse, OutboundError> {
        let tool_call = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_slice::<Value>(body).ok())
            .is_some_and(|message| message["method"] == "tools/call");
        if !tool_call {
            return self.inner.send(request, limits).await;
        }
        self.faults.pass(self.worker, Point::BeforeSend).await;
        let response = self.inner.send(request, limits).await;
        self.faults.pass(self.worker, Point::AfterSend).await;
        response
    }
}

fn worker_state(base: &AppState, faults: &Arc<Faults>, worker: Worker) -> AppState {
    let mut state = base.clone();
    state.runs = Arc::new(FaultyRuns {
        inner: base.runs.clone(),
        worker,
        faults: faults.clone(),
    });
    state.provider = Arc::new(FaultyModel {
        inner: base.provider.clone(),
        worker,
        faults: faults.clone(),
    });
    state.outbound = Arc::new(FaultyOutbound {
        inner: base.outbound.clone(),
        worker,
        faults: faults.clone(),
    });
    state
}

/// A worker taking one claim, in the background.
pub struct Running {
    worker: Worker,
    handle: JoinHandle<bool>,
}

impl Running {
    /// Waits for the worker to finish its claim: whether it had one.
    pub async fn finish(self) -> bool {
        let worker = self.worker;
        tokio::time::timeout(PATIENCE, self.handle)
            .await
            .unwrap_or_else(|_| panic!("{worker:?} never finished its claim"))
            .unwrap()
    }

    /// Stops the worker where it is, as if its process died there.
    pub async fn crash(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

/// Worker A stopped mid-claim, its lease run out, and worker B holding the
/// run's job. The test lets A go on (`a_gate.release()`) or crashes it.
pub struct Takeover {
    pub a: Running,
    pub a_gate: Arc<Gate>,
    /// B, stopped at the point it was given; `None` once it finished.
    pub b: Option<Running>,
    pub b_gate: Option<Arc<Gate>>,
    /// The writes tried before A's lease ran out (see [`Faults::committed_since`]).
    pub lease_ran_out: usize,
}

/// The kit over one test server.
pub struct Kit {
    pub clock: Arc<TestClock>,
    pub faults: Arc<Faults>,
    /// The server's state on the kit's clock, with a job queue on it too.
    base: AppState,
    /// The API over the run store as [`Worker::Api`] writes to it.
    api: axum::Router,
}

impl Kit {
    pub fn new(server: &Server) -> Self {
        let clock = TestClock::new();
        let faults = Arc::new(Faults::default());
        let mut base = server.state.clone();
        base.clock = clock.clone();
        base.jobs = Arc::new(SqliteJobQueue::with_clock(server.database.clone(), clock.clone()));
        let api = zupflow_server::app(worker_state(&base, &faults, Worker::Api), RequestGuard::default());
        Self {
            clock,
            faults,
            base,
            api,
        }
    }

    /// `worker`'s own state: the real engine's, stopping where `faults` says.
    pub fn state(&self, worker: Worker) -> AppState {
        worker_state(&self.base, &self.faults, worker)
    }

    /// A request to the API.
    pub async fn send(&self, call: Call<'_>) -> (StatusCode, Value) {
        send_to(&self.api, call).await
    }

    /// `worker` claims the next run job due, if any, and works it.
    pub fn spawn(&self, worker: Worker) -> Running {
        let state = self.state(worker);
        Running {
            worker,
            handle: tokio::spawn(async move { run_engine::work_once(&state).await }),
        }
    }

    /// Moves the clock past a run's lease: a claim that wasn't renewed since
    /// has run out.
    pub fn expire_lease(&self) {
        self.clock.advance(run_engine::LEASE + Duration::from_secs(1));
    }

    /// `worker` works every run job it can, then moves the clock ten
    /// minutes on, one minute at a time, working again after each, so
    /// retries come due and a lease a crashed worker left runs out. A worker
    /// still stopped at a point loses its lease too.
    pub async fn drain(&self, worker: Worker) {
        let state = self.state(worker);
        while run_engine::work_once(&state).await {}
        for _ in 0..10 {
            self.clock.advance(Duration::from_secs(60));
            while run_engine::work_once(&state).await {}
        }
    }

    /// The two-worker race: worker A claims the run's job and works until
    /// `a_at`, where it stops; its lease runs out; worker B claims the job
    /// and works it, until `b_at` if given, or else to the end of its claim.
    pub async fn take_over(&self, a_at: Point, b_at: Option<Point>) -> Takeover {
        let a_gate = self.faults.arm(Worker::A, a_at);
        let a = self.spawn(Worker::A);
        a_gate.reached().await;
        assert!(
            !run_engine::work_once(&self.state(Worker::B)).await,
            "while A's lease holds, nobody else claims its run"
        );
        self.expire_lease();
        let lease_ran_out = self.faults.mark();
        let b_gate = b_at.map(|at| self.faults.arm(Worker::B, at));
        let b = self.spawn(Worker::B);
        let b = match &b_gate {
            Some(gate) => {
                gate.reached().await;
                Some(b)
            }
            None => {
                assert!(b.finish().await, "B found the run's job to take over");
                None
            }
        };
        Takeover {
            a,
            a_gate,
            b,
            b_gate,
            lease_ran_out,
        }
    }

    /// The run as the store holds it, with its turns and calls.
    pub async fn progress(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> RunProgress {
        self.base
            .runs
            .find_progress(organization_id, workspace_id, run_id)
            .await
            .unwrap()
            .unwrap()
    }
}

/// What the runtime kept of the answer a sent call got back. Today that is
/// the call's outcome once it settled `completed` or `failed`; an `unknown`
/// call keeps only the placeholder the model reads. Once sends are fenced
/// by a send claim, this adds the evidence an attempt keeps when its answer
/// arrives after the call was settled unknown.
pub fn kept_answers(call: &RunToolCall) -> Vec<String> {
    match (call.state, &call.outcome) {
        (CallState::Completed | CallState::Failed, Some(outcome)) => vec![outcome.content.clone()],
        _ => Vec::new(),
    }
}

/// The model's tokens the runtime accounts for in a run. Today that is the
/// run's usage, counted from recorded turns only. Once model attempts are
/// recorded, this adds the attempts a run abandoned.
pub fn accounted_tokens(progress: &RunProgress) -> u64 {
    progress.run.usage.input_tokens + progress.run.usage.output_tokens
}

/// The tokens of `answers`, as the model counted them.
pub fn produced_tokens(answers: &[Answer]) -> u64 {
    answers
        .iter()
        .map(|answer| answer.usage.input_tokens + answer.usage.output_tokens)
        .sum()
}
