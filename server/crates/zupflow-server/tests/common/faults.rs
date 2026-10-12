//! A deterministic fault kit for the run engine: a clock the test moves,
//! named points around every transition a worker or the API applies to the
//! run store, the model's answer and the outbound MCP `tools/call` where a
//! worker can be stopped or crashed, and the race where a second worker
//! takes a run over from a first whose lease ran out.
//!
//! Each worker is the real engine (`run_engine::work_once`) over its own
//! copy of the state, whose run store, model provider and outbound path are
//! wrapped to stop at the points a test arms: the first time the worker
//! reaches one, or the nth (`Faults::arm_nth`), such as a run's second send. A worker stopped at a point
//! holds no transaction; crashing it drops it there, as a process dying
//! would. What happened is read from records the runtime can't rewrite: the
//! fake MCP server's ledger (`Providers::ledger`), the answers the model
//! actually produced (`Faults::answers`), and every transition each worker
//! tried with whether it committed (`Faults::writes`). The run store and the
//! job queue run on the kit's clock.
//!
//! Include it beside `common` with `#[path = "common/faults.rs"] mod faults;`.

#![allow(dead_code, clippy::unwrap_used, clippy::too_many_arguments)]

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zupflow_basic::in_process_metrics::InProcessMetrics;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{InstallationId, OrganizationId, RunId, WorkspaceId};
use zupflow_core::models::{Completion, Message, ModelProvider, ProviderError, TokenUsage, ToolSpec, Turn};
use zupflow_core::outbound::{Limits, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse};
use zupflow_core::runs::state::{CallRecord, CommittedTurn, ModelAttempt, Snapshot};
use zupflow_core::runs::store::{Applied, Durability, RunStore, StoreError};
use zupflow_core::runs::transition::{Conflict, Operation, TransitionRequest};
use zupflow_core::runs::{
    AgentActivity, CallState, OrganizationRun, OrganizationRunFilter, Page, Run, RunEvent, RunFilter, UsageDay,
};
use zupflow_core::time::{Clock, Timestamp};
use zupflow_server::{AppState, RequestGuard, run_engine};
use zupflow_sqlite::{SqliteJobQueue, SqliteRunStore};

use crate::common::{Call, Server, send_to};

/// How long a test waits for a worker to reach a point or finish its claim.
const PATIENCE: Duration = Duration::from_secs(20);

/// A clock that stands still until the test moves it. It starts an hour
/// ahead of the wall clock: jobs the other stores queue with their changes
/// (discovery's, say) take the wall clock's time, so they are already due by
/// this one.
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

/// Each write of the run store: a transition, by its operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    Admit,
    Claim,
    PrepareModel,
    CommitTurn,
    Park,
    ResolveApproval,
    RefuseWaiting,
    AuthorizeCall,
    RecordOutcome,
    SettleUnknown,
    Cancel,
    Yield,
    Finish,
    FailAbandoned,
}

impl Write {
    fn of(operation: &Operation) -> Self {
        match operation {
            Operation::Admit(_) => Self::Admit,
            Operation::Claim(_) => Self::Claim,
            Operation::PrepareModel(_) => Self::PrepareModel,
            Operation::CommitTurn(_) => Self::CommitTurn,
            Operation::Park(_) => Self::Park,
            Operation::ResolveApproval(_) => Self::ResolveApproval,
            Operation::RefuseWaiting(_) => Self::RefuseWaiting,
            Operation::AuthorizeCall(_) => Self::AuthorizeCall,
            Operation::RecordOutcome(_) => Self::RecordOutcome,
            Operation::SettleUnknown(_) => Self::SettleUnknown,
            Operation::Cancel(_) => Self::Cancel,
            Operation::Yield(_) => Self::Yield,
            Operation::Finish(_) => Self::Finish,
            Operation::FailAbandoned(_) => Self::FailAbandoned,
        }
    }
}

/// Why a transition didn't commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// A guard, or another request from the same revision.
    Conflict(Conflict),
    Store(StoreError),
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
    /// How many more times the worker passes the point before it stops there.
    skip: AtomicUsize,
    reached: watch::Sender<bool>,
    released: watch::Sender<bool>,
}

impl Gate {
    /// Whether the worker stopped here.
    pub fn is_reached(&self) -> bool {
        *self.reached.borrow()
    }

    pub fn worker(&self) -> Worker {
        self.worker
    }

    pub fn point(&self) -> Point {
        self.point
    }

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

    /// Waits, however long, until the worker stops here.
    pub async fn stopped(&self) {
        let mut reached = self.reached.subscribe();
        let _ = reached.wait_for(|reached| *reached).await;
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

/// One transition a worker tried: whether it committed, or why not.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempted {
    pub worker: Worker,
    pub write: Write,
    /// The owner epoch it claims (a `Claim`) or is guarded by (a worker's
    /// step); `None` for the API's, the sweep's and a sender's.
    pub epoch: Option<u64>,
    pub result: Result<(), Refused>,
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
    /// While set, the run store can't list the runs waiting on an
    /// installation, as if the database were down for that read.
    pub runs_waiting_on_unavailable: std::sync::atomic::AtomicBool,
    /// While set, the run store can't read a run's snapshot.
    pub snapshots_unavailable: std::sync::atomic::AtomicBool,
    writes: Mutex<Vec<Attempted>>,
    answers: Mutex<Vec<Answer>>,
}

impl Faults {
    /// Stops `worker` the next time it reaches `point`, once.
    pub fn arm(&self, worker: Worker, point: Point) -> Arc<Gate> {
        self.arm_nth(worker, point, 1)
    }

    /// Stops `worker` the `nth` time (from 1) it reaches `point` from now,
    /// once: the second call's send, say, or the closing turn's model
    /// attempt.
    pub fn arm_nth(&self, worker: Worker, point: Point, nth: usize) -> Arc<Gate> {
        assert!(nth >= 1, "the first time is 1");
        let gate = Arc::new(Gate {
            worker,
            point,
            skip: AtomicUsize::new(nth - 1),
            reached: watch::Sender::new(false),
            released: watch::Sender::new(false),
        });
        self.armed.lock().unwrap().push(gate.clone());
        gate
    }

    /// Takes back every point armed and not reached yet.
    pub fn disarm_all(&self) {
        self.armed.lock().unwrap().clear();
    }

    async fn pass(&self, worker: Worker, point: Point) {
        let gate = {
            let mut armed = self.armed.lock().unwrap();
            match armed
                .iter()
                .position(|gate| gate.worker == worker && gate.point == point)
            {
                Some(at) if armed[at].skip.load(Ordering::SeqCst) > 0 => {
                    armed[at].skip.fetch_sub(1, Ordering::SeqCst);
                    None
                }
                Some(at) => Some(armed.remove(at)),
                None => None,
            }
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

/// The run store, stopping its worker around each transition and noting it.
struct FaultyRuns {
    inner: Arc<dyn RunStore>,
    worker: Worker,
    faults: Arc<Faults>,
}

#[async_trait]
impl RunStore for FaultyRuns {
    fn durability(&self) -> Durability {
        self.inner.durability()
    }

    async fn apply(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        request: &TransitionRequest,
    ) -> Result<Applied, StoreError> {
        let write = Write::of(request.operation());
        self.faults.pass(self.worker, Point::Before(write)).await;
        let applied = self.inner.apply(organization_id, workspace_id, request).await;
        self.faults.writes.lock().unwrap().push(Attempted {
            worker: self.worker,
            write,
            epoch: match request.operation() {
                Operation::Claim(claim) => Some(claim.epoch),
                _ => request.guards().owner.map(|owner| owner.epoch),
            },
            result: match &applied {
                Ok(Applied::Committed(_)) => Ok(()),
                Ok(Applied::Conflict(conflict)) => Err(Refused::Conflict(*conflict)),
                Err(error) => Err(Refused::Store(*error)),
            },
        });
        self.faults.pass(self.worker, Point::After(write)).await;
        applied
    }

    async fn find_snapshot(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Snapshot>, StoreError> {
        if self.faults.snapshots_unavailable.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        self.inner.find_snapshot(organization_id, workspace_id, run_id).await
    }

    async fn find_run(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Result<Option<Run>, StoreError> {
        self.inner.find_run(organization_id, workspace_id, run_id).await
    }

    async fn list_runs(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        filter: &RunFilter,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page<Run>, StoreError> {
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
    ) -> Result<Page<OrganizationRun>, StoreError> {
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
    ) -> Result<Vec<RunEvent>, StoreError> {
        self.inner
            .list_events(organization_id, workspace_id, run_id, after)
            .await
    }

    async fn agent_activity(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        since: Timestamp,
    ) -> Result<Vec<AgentActivity>, StoreError> {
        self.inner.agent_activity(organization_id, workspace_id, since).await
    }

    async fn usage(
        &self,
        organization_id: OrganizationId,
        environment: Environment,
        since: Timestamp,
    ) -> Result<Vec<UsageDay>, StoreError> {
        self.inner.usage(organization_id, environment, since).await
    }

    async fn runs_waiting_on(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        after: Option<RunId>,
        limit: u32,
    ) -> Result<Vec<RunId>, StoreError> {
        if self.faults.runs_waiting_on_unavailable.load(Ordering::SeqCst) {
            return Err(StoreError::Unavailable);
        }
        self.inner
            .runs_waiting_on(organization_id, workspace_id, installation_id, after, limit)
            .await
    }

    async fn abandoned_runs(&self, limit: u32) -> Result<Vec<(OrganizationId, WorkspaceId, RunId)>, StoreError> {
        self.inner.abandoned_runs(limit).await
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

/// Where a worker is once it stopped at its gate or finished first.
pub enum Halt {
    /// Stopped at the gate; still running.
    Stopped(Running),
    /// Finished its claim without reaching the gate: whether it had one.
    Finished(bool),
}

impl Running {
    /// Waits until the worker stops at `gate` or finishes its claim first.
    pub async fn until(mut self, gate: &Gate) -> Halt {
        let worker = self.worker;
        let mut reached = gate.reached.subscribe();
        let halted = tokio::time::timeout(PATIENCE, async {
            tokio::select! {
                biased;
                _ = reached.wait_for(|reached| *reached) => None,
                joined = &mut self.handle => Some(joined.unwrap()),
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{worker:?} neither stopped nor finished"));
        match halted {
            None => Halt::Stopped(self),
            Some(had_one) => Halt::Finished(had_one),
        }
    }

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
    /// Whether B found the run's job: not when A's step at the point
    /// released the job with the claim (it parked, or the run ended).
    pub b_found: bool,
}

/// The kit over one test server.
pub struct Kit {
    pub clock: Arc<TestClock>,
    pub faults: Arc<Faults>,
    /// What every worker and the API measured, readable.
    pub metrics: Arc<InProcessMetrics>,
    /// The server's state on the kit's clock, with a run store and a job
    /// queue on it too.
    base: AppState,
    /// The server's database file, read directly for what no API shows.
    database: std::path::PathBuf,
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
        base.runs = Arc::new(SqliteRunStore::with_clock(server.database.clone(), clock.clone()));
        let metrics = Arc::new(InProcessMetrics::default());
        base.metrics = metrics.clone();
        let api = zupflow_server::app(worker_state(&base, &faults, Worker::Api), RequestGuard::default());
        Self {
            clock,
            faults,
            metrics,
            base,
            database: server.directory.join("zupflow.sqlite"),
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
        let takeover = self.after_a_stopped(a, a_gate, b_at).await;
        assert!(takeover.b_found, "B found the run's job to take over");
        takeover
    }

    /// The race from any boundary: worker A works until the `nth` time it
    /// reaches `a_at`, where it stops; its lease runs out; worker B claims
    /// the run's job, if there's one to claim (a step that releases the
    /// claim deals with the job too: parked or finished, nobody takes the
    /// run over), and works it to the end of its claim.
    pub async fn take_over_at(&self, a_at: Point, nth: usize) -> Takeover {
        let a_gate = self.faults.arm_nth(Worker::A, a_at, nth);
        let a = match self.spawn(Worker::A).until(&a_gate).await {
            Halt::Stopped(a) => a,
            Halt::Finished(_) => panic!("A never reached {a_at:?} #{nth}"),
        };
        self.after_a_stopped(a, a_gate, None).await
    }

    async fn after_a_stopped(&self, a: Running, a_gate: Arc<Gate>, b_at: Option<Point>) -> Takeover {
        self.expire_lease();
        let lease_ran_out = self.faults.mark();
        let b_gate = b_at.map(|at| self.faults.arm(Worker::B, at));
        let b = self.spawn(Worker::B);
        let (b, b_found) = match &b_gate {
            Some(gate) => {
                gate.reached().await;
                (Some(b), true)
            }
            None => (None, b.finish().await),
        };
        Takeover {
            a,
            a_gate,
            b,
            b_gate,
            lease_ran_out,
            b_found,
        }
    }

    /// Replaces the model every worker asks, from now on.
    pub fn set_model(&mut self, provider: Arc<dyn ModelProvider>) {
        self.base.provider = provider;
    }

    /// A request to the API in the background, so the test can stop it at
    /// a point (as [`Worker::Api`]) and crash it there, as a handler whose
    /// process died.
    pub fn spawn_send(
        &self,
        user: &str,
        method: axum::http::Method,
        uri: String,
        organization: Option<&str>,
        body: Option<Value>,
    ) -> JoinHandle<(StatusCode, Value)> {
        let api = self.api.clone();
        let (user, organization) = (user.to_owned(), organization.map(str::to_owned));
        tokio::spawn(async move {
            let mut call = crate::common::call(&user, method, uri);
            if let Some(organization) = &organization {
                call = call.naming(organization);
            }
            if let Some(body) = body {
                call = call.body(body);
            }
            send_to(&api, call).await
        })
    }

    /// The run as the store holds it, with its turns and calls, and what
    /// each call's attempts kept of the answers their senders got.
    pub async fn progress(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Progress {
        let run = self
            .base
            .runs
            .find_run(organization_id, workspace_id, run_id)
            .await
            .unwrap()
            .unwrap();
        let snapshot = self
            .base
            .runs
            .find_snapshot(organization_id, workspace_id, run_id)
            .await
            .unwrap()
            .unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", self.database.display()))
            .await
            .unwrap();
        let mut calls = Vec::new();
        for record in snapshot.calls {
            // What each attempt's sender reported, kept on the attempt
            // whether it settled the call or came too late to.
            let evidence: Vec<String> = sqlx::query_scalar(
                "SELECT outcome FROM run_call_attempts WHERE organization_id = ? AND workspace_id = ? \
                 AND run_id = ? AND call_id = ? AND outcome IS NOT NULL ORDER BY attempt",
            )
            .bind(organization_id.uuid())
            .bind(workspace_id.uuid())
            .bind(run_id.uuid())
            .bind(record.id.uuid())
            .fetch_all(&pool)
            .await
            .unwrap();
            calls.push(RecordedCall { record, evidence });
        }
        pool.close().await;
        Progress {
            run,
            turns: snapshot.turns,
            model_attempts: snapshot.model_attempts,
            calls,
        }
    }
}

impl Kit {
    /// Whether the run ended: completed, failed or cancelled.
    pub async fn run_finished(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> bool {
        self.base
            .runs
            .find_run(organization_id, workspace_id, run_id)
            .await
            .unwrap()
            .is_some_and(|run| run.status.is_terminal())
    }

    /// The run's job as the queue holds it: how many attempts it has used
    /// and its state; `None` once it's gone.
    pub async fn run_job(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        run_id: RunId,
    ) -> Option<(i64, String)> {
        let pool = sqlx::SqlitePool::connect(&format!("sqlite:{}", self.database.display()))
            .await
            .unwrap();
        let job: Option<(i64, String)> = sqlx::query_as(
            "SELECT j.attempts, j.state FROM jobs j JOIN runs r ON r.organization_id = j.organization_id \
             AND r.workspace_id = j.workspace_id AND r.job_id = j.id \
             WHERE r.organization_id = ? AND r.workspace_id = ? AND r.id = ?",
        )
        .bind(organization_id.uuid())
        .bind(workspace_id.uuid())
        .bind(run_id.uuid())
        .fetch_optional(&pool)
        .await
        .unwrap();
        pool.close().await;
        job
    }
}

/// A run as [`Kit::progress`] reads it.
pub struct Progress {
    pub run: Run,
    pub turns: Vec<CommittedTurn>,
    /// Every request to the model, by turn and attempt.
    pub model_attempts: Vec<ModelAttempt>,
    pub calls: Vec<RecordedCall>,
}

/// A call, with what its attempts kept of their senders' answers.
#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub record: CallRecord,
    /// Each attempt's recorded outcome, as the store keeps it.
    pub evidence: Vec<String>,
}

impl std::ops::Deref for RecordedCall {
    type Target = CallRecord;

    fn deref(&self) -> &CallRecord {
        &self.record
    }
}

/// What the runtime kept of the answer a sent call got back: the call's
/// outcome once it settled `completed` or `failed`, and the outcome each
/// attempt recorded, including one that arrived after the call was settled
/// unknown (late evidence, for operators only).
pub fn kept_answers(call: &RecordedCall) -> Vec<String> {
    let mut kept = match (call.state, &call.outcome) {
        (CallState::Completed | CallState::Failed, Some(outcome)) => vec![outcome.content.clone()],
        _ => Vec::new(),
    };
    kept.extend(call.evidence.iter().cloned());
    kept
}

/// The model's tokens the runtime accounts for in a run: the run's usage,
/// counted from its committed turns. An attempt a stopped worker abandoned
/// keeps no tokens: its answer never reached the store.
pub fn accounted_tokens(progress: &Progress) -> u64 {
    progress.run.usage.input_tokens + progress.run.usage.output_tokens
}

/// The tokens of `answers`, as the model counted them.
pub fn produced_tokens(answers: &[Answer]) -> u64 {
    answers
        .iter()
        .map(|answer| answer.usage.input_tokens + answer.usage.output_tokens)
        .sum()
}
