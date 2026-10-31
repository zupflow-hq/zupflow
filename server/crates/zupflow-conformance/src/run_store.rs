//! The `RunStore` suite (decision 0046): every change to a run is one
//! `apply`, and every adapter holds it to the same rules.
//!
//! - **Scope.** A run, its timeline, calls and attempts are reached only
//!   through their own organization and workspace. A request whose scope or
//!   ID isn't `apply`'s is refused as not found, changing nothing, and so is
//!   one for a run of another workspace or organization.
//! - **Identity.** The same request again answers the same receipt and
//!   changes nothing; another request from the same revision conflicts.
//! - **Guards**, on the store's own clock: the revision, the owner's epoch,
//!   claimant and live lease, the send claim and its lease, the cancel flag,
//!   no live claim and no live send. A step that leaves out the guards its
//!   operation needs is refused.
//! - **Everything an operation implies, or nothing**: the run, its turns,
//!   calls and attempts, events numbered from 1 without gaps, the webhook
//!   announcements those events name, the audit rows of control requests and
//!   the run's job: queued with the run and with a decision, released when no
//!   worker needs it, postponed by a yield without using an attempt.
//! - **Runs as the API knows them**: a run only of an agent of
//!   its workspace; lists newest first, paged and filtered, and across an
//!   organization's workspaces only its own; a step taken once; a decision
//!   only while the run waits on exactly that call; a call sent once;
//!   abandoned runs found and failed; a run that outlives its agent; usage
//!   per UTC day in one environment.
//! - **Sends and attempts**: cancel beats an authorization not yet
//!   committed, and a send already authorized is recorded after it; a send
//!   claim is held once; an outcome that arrives after the call was settled
//!   unknown is kept as evidence only; a model attempt left by a stopped
//!   worker is marked abandoned.
//! - **Installations that go away** (decision 0047): a call waiting for
//!   approval on one is refused, parked on or not yet, only with the reason
//!   it went, and its run moves on; the runs such calls wait in are found
//!   only in their own workspace.
//! - **Memory**: a run keeps the memory it was started with (`agent` or
//!   `off`), and the memory type and recall its first model attempt froze
//!   with its offer; every later attempt is prepared with the same, a run
//!   started with memory off recalls nothing, and a recall is never empty. A
//!   memory tool's call keeps its type, with no short ID, and one waiting
//!   for approval is refused, parked on or not yet, only because the type
//!   was deleted; the runs such calls wait in are found across the
//!   organization's workspaces, and only there.
//!
//! The suite drives runs with the reducer (`advance`) where a worker would,
//! and builds the requests a buggy or stale caller would by hand. Its
//! [`SuiteClock`] is the store's and the queue's clock, so leases run out
//! exactly when the suite says. `prepare` makes a [`Tenant`] exist: its
//! organization and two workspaces, the first in Sandbox, the second in
//! Production.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::{Map, json};
use uuid::Uuid;
use zupflow_core::agents::{AgentStore, NewAgent, SentMemory};
use zupflow_core::audit::{AuditAction, AuditActor, AuditFilter, AuditObject, AuditStore};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{
    AgentId, ApiKeyId, InstallationId, MemoryTypeId, ModelConnectionId, ModelId, OrganizationId, RunId, SessionId,
    ToolCallId, WorkspaceId,
};
use zupflow_core::jobs::{Job, JobError, JobQueue};
use zupflow_core::memory::{MemoryWrites, notes};
use zupflow_core::model_store::{RouteState, RunModel};
use zupflow_core::models::{Completion, ModelToolCall, Reply, TokenUsage};
use zupflow_core::runs::digest::{Sha256Digest, call_digest, memory_call_digest};
use zupflow_core::runs::names::{Candidate, Offer, ToolHost, build_offer, memory_tool};
use zupflow_core::runs::reducer::{
    Approval, Facts, Gone, INSTALLATION_DISCONNECTED, INSTALLATION_REMOVED, InstallationCheck, Io, MEMORY_TYPE_REMOVED,
    MEMORY_TYPE_REMOVED_MESSAGE, ModelAnswer, ModelResult, Plan, SendCheck, SendRequest, SendResult, Unanswered,
    UnansweredWhy, admit, advance, calls_to_decide, cancel, fail_abandoned, refuse_waiting, resolve_approval,
};
use zupflow_core::runs::state::{
    CallDecision, CallVerdict, ClaimantId, Continuation, EXECUTION_VERSION, FrozenMemory, InvocationId,
    ModelAttemptState, RequestId, Snapshot,
};
use zupflow_core::runs::store::{
    Applied, Durability, RUN_JOB, RUN_JOB_ATTEMPTS, Receipt, RunStore, StoreError, run_of_job,
};
use zupflow_core::runs::transition::{
    Authorization, AuthorizeCall, Claim, Closing, CommitTurn, Conflict, Finish, Guards, NewCall, Operation, OwnerGuard,
    PrepareModel, RecordOutcome, RefuseWaiting, RunEnding, SendGuard, SendLease, SendOutcome, SettleUnknown,
    Settlement, TransitionRequest, WentAway, Yield, YieldReason,
};
use zupflow_core::runs::{
    ApprovalDecision, CallState, Failure, MemoryMode, NewRun, OrganizationRunFilter, RunEventType, RunFilter,
    RunStatus, RunTrigger, UsageDay,
};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::{Clock, Timestamp};
use zupflow_core::webhooks::{ANNOUNCE, Announcement, Subject, WebhookEventType};

use crate::audit_store::by_suite_in;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    /// In Sandbox.
    pub workspace: WorkspaceId,
    /// In Production.
    pub sibling: WorkspaceId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The clock the store and the queue read: it stands still until the suite
/// moves it.
pub struct SuiteClock(AtomicI64);

impl SuiteClock {
    /// A clock at the system's time now.
    pub fn new() -> Self {
        Self(AtomicI64::new(Timestamp::now().millis()))
    }

    pub fn set(&self, at: Timestamp) {
        self.0.store(at.millis(), Ordering::SeqCst);
    }

    pub fn advance(&self, by: Duration) {
        self.0
            .fetch_add(i64::try_from(by.as_millis()).unwrap(), Ordering::SeqCst);
    }
}

impl Default for SuiteClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SuiteClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_millis(self.0.load(Ordering::SeqCst))
    }
}

pub struct Stores<'a> {
    pub runs: &'a dyn RunStore,
    pub agents: &'a dyn AgentStore,
    /// The queue in the store's database, on `clock`.
    pub jobs: &'a dyn JobQueue,
    pub audit: &'a dyn AuditStore,
    /// The store's clock.
    pub clock: &'a SuiteClock,
}

/// A run, where it lives.
#[derive(Debug, Clone, Copy)]
struct At {
    org: OrganizationId,
    ws: WorkspaceId,
    run: RunId,
}

const LEASE: Duration = Duration::from_secs(5 * 60);
const SEND_LEASE: Duration = Duration::from_secs(60);

fn claimant() -> ClaimantId {
    ClaimantId::from_uuid(Uuid::now_v7())
}

fn caller() -> RequestId {
    RequestId::from_uuid(Uuid::now_v7())
}

fn invocation() -> InvocationId {
    InvocationId::from_uuid(Uuid::now_v7())
}

fn api_key() -> (ApiKeyId, &'static str) {
    (ApiKeyId::generate(), "Backend")
}

fn person() -> AuditActor {
    AuditActor::ApiKey {
        id: ApiKeyId::generate(),
        name: "Support desk".to_owned(),
    }
}

async fn tenant(prepare: &Prepare) -> Tenant {
    let tenant = Tenant {
        organization_id: OrganizationId::generate(),
        workspace: WorkspaceId::generate(),
        sibling: WorkspaceId::generate(),
    };
    prepare(tenant).await;
    tenant
}

async fn agent(stores: &Stores<'_>, organization_id: OrganizationId, workspace: WorkspaceId, name: &str) -> AgentId {
    stores
        .agents
        .create_agent(
            organization_id,
            workspace,
            AgentId::generate(),
            &NewAgent {
                name: name.to_owned(),
                description: String::new(),
                model_id: None,
                memory: SentMemory::default(),
                instructions: "Be brief.".to_owned(),
            },
            &|_| by_suite_in(workspace),
        )
        .await
        .unwrap()
        .id
}

/// The organization's model every suite run is admitted with. The run store
/// keeps it as the run's own record; it references no model.
fn run_model() -> RunModel {
    RunModel {
        model_id: ModelId::from_uuid(Uuid::from_u128(0x5eed_0001)),
        connection_id: ModelConnectionId::from_uuid(Uuid::from_u128(0x5eed_0002)),
        provider_model_id: "model-a".to_owned(),
    }
}

fn new_run(agent_id: AgentId, input: &str) -> NewRun {
    NewRun {
        agent_id,
        session_id: SessionId::generate(),
        trigger: RunTrigger::Api,
        input: input.to_owned(),
        memory: MemoryMode::Agent,
        model: run_model(),
        instructions: "Be brief.".to_owned(),
        channel: None,
    }
}

fn committed(applied: Result<Applied, StoreError>) -> Receipt {
    match applied {
        Ok(Applied::Committed(receipt)) => receipt,
        other => panic!("expected a commit, got {other:?}"),
    }
}

async fn apply(stores: &Stores<'_>, at: At, request: &TransitionRequest) -> Result<Applied, StoreError> {
    stores.runs.apply(at.org, at.ws, request).await
}

async fn snapshot(stores: &Stores<'_>, at: At) -> Snapshot {
    stores
        .runs
        .find_snapshot(at.org, at.ws, at.run)
        .await
        .unwrap()
        .expect("the run's snapshot")
}

/// Everything about a run a refused request must leave as it was: its
/// snapshot, its timeline and the run as the API shows it.
async fn state(stores: &Stores<'_>, at: At) -> (Snapshot, usize, zupflow_core::runs::Run) {
    (
        snapshot(stores, at).await,
        stores.runs.list_events(at.org, at.ws, at.run, 0).await.unwrap().len(),
        stores.runs.find_run(at.org, at.ws, at.run).await.unwrap().unwrap(),
    )
}

async fn admitted(stores: &Stores<'_>, org: OrganizationId, ws: WorkspaceId, agent_id: AgentId, input: &str) -> At {
    let run = RunId::generate();
    let receipt = committed(
        stores
            .runs
            .apply(org, ws, &admit(org, ws, run, &new_run(agent_id, input)))
            .await,
    );
    assert_eq!((receipt.from_revision, receipt.to_revision), (0, 1));
    assert_eq!(
        Some(receipt.run.as_ref()),
        stores.runs.find_run(org, ws, run).await.unwrap().as_ref(),
        "a receipt carries the run as it committed"
    );
    At { org, ws, run }
}

/// Claims every due job of `kind`, keeping the ones for `run` held and
/// finishing the rest.
async fn jobs_of(stores: &Stores<'_>, kind: &str, run: Option<RunId>) -> Vec<Job> {
    let kind = Name::new(kind).unwrap();
    let mut held = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        if run.is_some() && run_of_job(&job.payload) == run {
            held.push(job);
        } else {
            stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
        }
    }
    held
}

/// The run's due job, held, if it has one.
async fn run_job(stores: &Stores<'_>, at: At) -> Option<Job> {
    let mut held = jobs_of(stores, RUN_JOB, Some(at.run)).await;
    assert!(held.len() <= 1, "one job takes a run forward");
    held.pop()
}

/// Finishes every queued announcement.
async fn drain_announcements(stores: &Stores<'_>) {
    jobs_of(stores, ANNOUNCE, None).await;
}

/// Claims every queued announcement of `run`, answering each with its
/// job's scope; finishes the others.
async fn announced(stores: &Stores<'_>, run: RunId) -> Vec<(Scope, Announcement)> {
    let kind = Name::new(ANNOUNCE).unwrap();
    let mut claimed = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
        let announcement = Announcement::parse(&job.payload).expect("an announcement reads back");
        if announcement.subject == Subject::Run(run) {
            claimed.push((job.scope, announcement));
        }
    }
    claimed
}

fn work() -> InstallationId {
    InstallationId::from_uuid(Uuid::from_u128(0x0199_0000_0000_7000_8000_0000_0000_0010))
}

fn personal() -> InstallationId {
    InstallationId::from_uuid(Uuid::from_u128(0x0199_0000_0000_7000_8000_0000_0000_0011))
}

const LIST: &str = "taskboard_k7m2qd_list_tasks";
const CREATE: &str = "taskboard_p3x7ta_create_task";

fn candidate(installation_id: InstallationId, short_id: &str, label: &str, tool: &str) -> Candidate {
    Candidate {
        installation_id,
        installation_short_id: short_id.to_owned(),
        installation_label: label.to_owned(),
        app_key: "taskboard".to_owned(),
        app_name: "Taskboard".to_owned(),
        tool: tool.to_owned(),
        description: format!("Does {tool}."),
        input_schema: json!({ "type": "object" }),
        definition_sha256: Sha256Digest::of(tool.as_bytes()),
    }
}

/// Two Taskboard installations: reading on Work, writing on Personal.
fn offer() -> Offer {
    build_offer([
        candidate(work(), "k7m2qd", "Work", "list_tasks"),
        candidate(personal(), "p3x7ta", "Personal", "create_task"),
    ])
    .offer
}

fn decision(verdict: CallVerdict, rule: &str) -> CallDecision {
    CallDecision {
        verdict,
        policy: "configured".to_owned(),
        mode: Some("manual".to_owned()),
        rule: rule.to_owned(),
    }
}

fn allow() -> CallDecision {
    decision(CallVerdict::Allow, "allowed")
}

fn needs_approval() -> CallDecision {
    decision(
        CallVerdict::RequireApproval {
            reason: "Needs a person.".to_owned(),
        },
        "grant_requires_approval",
    )
}

fn deny() -> CallDecision {
    decision(
        CallVerdict::Deny {
            reason: "Not for this agent.".to_owned(),
        },
        "organization_denies",
    )
}

/// The facts a worker brings: its claimant and, once it claimed, its epoch.
fn facts(stores: &Stores<'_>, who: ClaimantId, epoch: Option<u64>) -> Facts {
    Facts {
        claimant: Some(who),
        claim: epoch,
        offer: Some(offer()),
        invocation_id: Some(invocation()),
        ..Facts::at(stores.clock.now())
    }
}

/// The plan `advance` makes for the run as committed. At a call waiting for
/// approval, the worker found its installation still there, unless `facts`
/// say otherwise.
async fn plan(stores: &Stores<'_>, at: At, facts: &Facts) -> Plan {
    let snapshot = snapshot(stores, at).await;
    let mut facts = facts.clone();
    if facts.model_route.is_none() {
        facts.model_route = Some(RouteState::Usable { key_version: 1 });
    }
    if facts.send_check.is_none() {
        facts.send_check = parking_check(&snapshot, InstallationCheck::Usable);
    }
    advance(&snapshot, &facts).expect("a plan")
}

/// The check of the call waiting for approval the run is at, before it parks
/// on it: its installation as `installation` says.
fn parking_check(snapshot: &Snapshot, installation: InstallationCheck) -> Option<SendCheck> {
    let Continuation::NeedCall { turn, position } = snapshot.control.continuation else {
        return None;
    };
    let call = snapshot
        .call_at(turn, position)
        .filter(|call| call.state == CallState::AwaitingApproval)?;
    Some(SendCheck {
        call_id: call.id,
        installation,
        definition_sha256: call.target.as_ref().map(|target| target.definition_sha256),
        decision: call.decision.clone()?,
    })
}

fn request(plan: &Plan) -> TransitionRequest {
    plan.request().expect("a transition").clone()
}

/// Plans one step and commits it.
async fn step(stores: &Stores<'_>, at: At, facts: &Facts) -> (Plan, Receipt) {
    let plan = plan(stores, at, facts).await;
    let receipt = committed(apply(stores, at, &request(&plan)).await);
    (plan, receipt)
}

/// Claims the run as `who`: its new epoch.
async fn claim(stores: &Stores<'_>, at: At, who: ClaimantId) -> u64 {
    let (plan, receipt) = step(stores, at, &facts(stores, who, None)).await;
    let Operation::Claim(claim) = request(&plan).operation().clone() else {
        panic!("expected a claim");
    };
    assert_eq!(
        receipt.lease_until,
        Some(Timestamp::from_millis(
            receipt.committed_at.millis() + i64::try_from(LEASE.as_millis()).unwrap()
        )),
        "a claim's lease runs from the store's clock"
    );
    claim.epoch
}

/// Prepares the next model attempt as the run's owner.
async fn prepare(stores: &Stores<'_>, at: At, who: ClaimantId, epoch: u64) -> Receipt {
    let (plan, receipt) = step(stores, at, &facts(stores, who, Some(epoch))).await;
    assert!(
        matches!(plan.io(), Io::CallModel(_)),
        "the model is asked after it commits"
    );
    receipt
}

fn calls(asked: &[(&str, &str)]) -> Reply {
    Reply::ToolCalls {
        text: Some("Working on it.".to_owned()),
        calls: asked
            .iter()
            .map(|(id, name)| {
                let mut arguments = Map::new();
                arguments.insert("title".to_owned(), json!(format!("Task {id}")));
                ModelToolCall {
                    id: (*id).to_owned(),
                    name: (*name).to_owned(),
                    arguments,
                }
            })
            .collect(),
    }
}

/// The model's answer to the attempt the run waits on, committed with a
/// decision per call.
async fn answer(
    stores: &Stores<'_>,
    at: At,
    who: ClaimantId,
    epoch: u64,
    reply: Reply,
    decide: impl Fn(&str) -> CallDecision,
) -> Receipt {
    let commit = answer_request(stores, at, who, epoch, reply, decide).await;
    committed(apply(stores, at, &commit).await)
}

async fn answer_request(
    stores: &Stores<'_>,
    at: At,
    who: ClaimantId,
    epoch: u64,
    reply: Reply,
    decide: impl Fn(&str) -> CallDecision,
) -> TransitionRequest {
    let snapshot = snapshot(stores, at).await;
    let Continuation::AwaitModel { turn, attempt } = snapshot.control.continuation else {
        panic!("the run waits on no model attempt");
    };
    let completion = Completion {
        reply,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 3,
            cache_read_input_tokens: 6,
            cache_write_input_tokens: 2,
        },
    };
    let mut facts = facts(stores, who, Some(epoch));
    facts.decisions = calls_to_decide(&snapshot, &completion, &facts.limits)
        .unwrap()
        .into_iter()
        .map(|(position, tool)| (position, decide(&tool.name)))
        .collect();
    facts.call_ids = (0..20).map(|_| ToolCallId::generate()).collect();
    facts.model = Some(ModelAnswer {
        turn,
        attempt,
        result: ModelResult::Answered(completion),
    });
    request(&advance(&snapshot, &facts).expect("a plan"))
}

/// The run's next call checked again at send, with `decision`: the plan.
async fn at_send(stores: &Stores<'_>, at: At, who: ClaimantId, epoch: u64, decision: CallDecision) -> Plan {
    let snapshot = snapshot(stores, at).await;
    let Continuation::NeedCall { turn, position } = snapshot.control.continuation else {
        panic!("the run isn't at a call");
    };
    let call = snapshot.call_at(turn, position).unwrap();
    let mut facts = facts(stores, who, Some(epoch));
    facts.send_check = Some(SendCheck {
        call_id: call.id,
        installation: InstallationCheck::Usable,
        definition_sha256: call.target.as_ref().map(|target| target.definition_sha256),
        decision,
    });
    advance(&snapshot, &facts).expect("a plan")
}

/// Authorizes the run's next call to send: the send it allows.
async fn authorize(stores: &Stores<'_>, at: At, who: ClaimantId, epoch: u64, decision: CallDecision) -> SendRequest {
    let plan = at_send(stores, at, who, epoch, decision).await;
    let Io::SendCall(send) = plan.io().clone() else {
        panic!("expected a send, got {plan:?}");
    };
    let receipt = committed(apply(stores, at, &request(&plan)).await);
    assert_eq!(
        receipt.continuation,
        Continuation::AwaitCall {
            call_id: send.call_id,
            attempt: send.attempt
        }
    );
    send
}

fn completed(title: &str) -> SendOutcome {
    let mut result = Map::new();
    result.insert("title".to_owned(), json!(title));
    SendOutcome::Completed { result }
}

/// The sender's outcome, as `advance` plans it: the request.
async fn outcome_request(stores: &Stores<'_>, at: At, send: &SendRequest, outcome: SendOutcome) -> TransitionRequest {
    let mut facts = Facts::at(stores.clock.now());
    facts.sent = Some(SendResult {
        call_id: send.call_id,
        attempt: send.attempt,
        invocation_id: send.invocation_id,
        outcome,
    });
    request(&plan(stores, at, &facts).await)
}

async fn report(stores: &Stores<'_>, at: At, send: &SendRequest, outcome: SendOutcome) -> Receipt {
    let record = outcome_request(stores, at, send, outcome).await;
    committed(apply(stores, at, &record).await)
}

fn kinds(receipt: &Receipt) -> Vec<RunEventType> {
    receipt.events.iter().map(|event| event.kind).collect()
}

/// A hand-built request from the run's current revision.
async fn by_hand(stores: &Stores<'_>, at: At, guards: Guards, operation: Operation) -> TransitionRequest {
    let revision = snapshot(stores, at).await.control.revision;
    TransitionRequest::new(at.org, at.ws, at.run, revision, guards, operation)
}

fn owner(epoch: u64, who: ClaimantId, keeps: bool) -> Guards {
    Guards {
        owner: Some(OwnerGuard {
            epoch,
            claimant: who,
            renew_lease_ms: keeps.then(|| u64::try_from(LEASE.as_millis()).unwrap()),
        }),
        not_cancelled: true,
        ..Guards::default()
    }
}

/// Answers `refused` for `request` and leaves the run exactly as it was.
async fn refuses_without_change(
    stores: &Stores<'_>,
    at: At,
    request: &TransitionRequest,
    expected: Result<Applied, StoreError>,
    why: &str,
) {
    let before = state(stores, at).await;
    assert_eq!(apply(stores, at, request).await, expected, "{why}");
    assert_eq!(state(stores, at).await, before, "{why}: nothing changed");
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    let a = tenant(&prepare).await;
    let b = tenant(&prepare).await;
    let org = a.organization_id;
    let agent_a = agent(&stores, org, a.workspace, "Here").await;
    let agent_sibling = agent(&stores, org, a.sibling, "Sibling").await;
    let agent_b = agent(&stores, b.organization_id, b.workspace, "Theirs").await;

    assert_eq!(
        stores.runs.durability(),
        Durability::Synchronous,
        "a run store commits to stable storage before it answers"
    );
    admits_runs_only_of_the_workspaces_agents(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    keeps_runs_and_requests_inside_their_scope(&stores, a, b, agent_a).await;
    lists_newest_first_in_scope(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    walks_a_run_through_every_step(&stores, a, agent_a).await;
    replays_a_request_and_conflicts_on_another(&stores, a, agent_a).await;
    refuses_a_stale_or_expired_owner(&stores, a, agent_a).await;
    refuses_steps_without_their_guards(&stores, a, agent_a).await;
    decides_only_the_call_the_run_waits_on(&stores, a, agent_a).await;
    refuses_steps_that_dont_fit_the_call(&stores, a, agent_a).await;
    cancel_beats_an_authorization_not_yet_committed(&stores, a, agent_a).await;
    records_a_send_authorized_before_a_cancel(&stores, a, agent_a).await;
    holds_a_send_claim_once(&stores, a, agent_a).await;
    refuses_a_waiting_call_whose_installation_went(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    keeps_a_runs_memory_with_its_attempts(&stores, a, agent_a).await;
    refuses_a_waiting_memory_call_whose_type_went(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    keeps_a_late_outcome_as_evidence(&stores, a, agent_a).await;
    abandons_a_model_attempt_a_stopped_worker_left(&stores, a, agent_a).await;
    yields_without_using_an_attempt(&stores, a, agent_a).await;
    fails_a_run_whose_model_stays_silent(&stores, a, agent_a).await;
    waits_as_the_provider_says_and_stops_at_a_second_timeout(&stores, a, agent_a).await;
    refuses_another_runs_calls(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    writes_all_or_nothing(&stores, a, agent_a).await;
    finds_and_fails_abandoned_runs(&stores, a, agent_a).await;
    keeps_runs_of_a_deleted_agent(&stores, a).await;
    announces_what_webhooks_name(&stores, a, agent_a).await;
    counts_usage_per_day(&stores, a, b, agent_a, agent_sibling, agent_b).await;
}

async fn admits_runs_only_of_the_workspaces_agents(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    jobs_of(stores, RUN_JOB, None).await;
    for (organization, workspace, agent) in [
        (org, a.workspace, agent_sibling),
        (org, a.workspace, agent_b),
        (b.organization_id, b.workspace, agent_a),
        (org, a.workspace, AgentId::generate()),
        // The agent's own workspace, named under another organization.
        (b.organization_id, a.workspace, agent_a),
    ] {
        let run = RunId::generate();
        assert_eq!(
            stores
                .runs
                .apply(
                    organization,
                    workspace,
                    &admit(organization, workspace, run, &new_run(agent, "hi"))
                )
                .await,
            Err(StoreError::AgentNotFound),
            "a run only of an agent of its own workspace"
        );
        assert_eq!(stores.runs.find_run(organization, workspace, run).await.unwrap(), None);
    }
    assert!(
        jobs_of(stores, RUN_JOB, None).await.is_empty(),
        "a refused run queues nothing"
    );

    let now = stores.clock.now();
    let run = RunId::generate();
    let receipt = committed(
        stores
            .runs
            .apply(
                org,
                a.workspace,
                &admit(org, a.workspace, run, &new_run(agent_a, "Hello")),
            )
            .await,
    );
    let at = At {
        org,
        ws: a.workspace,
        run,
    };
    assert_eq!(
        (
            receipt.status,
            receipt.continuation,
            receipt.lease_until,
            receipt.committed_at
        ),
        (RunStatus::Queued, Continuation::NeedModel { turn: 0 }, None, now)
    );
    assert_eq!(kinds(&receipt), [RunEventType::RunQueued], "queued");
    assert_eq!(receipt.events[0].sequence, 1, "events count from 1");
    assert_eq!(receipt.events[0].created_at, now, "on the store's clock");
    let job = run_job(stores, at).await.expect("the run's job, queued with it");
    assert_eq!(job.scope, Scope::Workspace(org, a.workspace), "in its workspace");
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();

    let found = stores.runs.find_run(org, a.workspace, run).await.unwrap().unwrap();
    assert_eq!(
        (
            found.agent_id,
            found.agent_name.as_deref(),
            found.input.as_str(),
            found.trigger,
            found.status
        ),
        (agent_a, Some("Here"), "Hello", RunTrigger::Api, RunStatus::Queued)
    );
    assert_eq!(found.created_at, now);
    assert_eq!(
        (found.output, found.error, found.started_at, found.finished_at),
        (None, None, None, None)
    );
    let snapshot = snapshot(stores, at).await;
    assert_eq!(
        (
            snapshot.control.revision,
            snapshot.control.owner_epoch,
            snapshot.control.execution_version,
            snapshot.model.as_str(),
            snapshot.instructions.as_str(),
            snapshot.input.as_str(),
        ),
        (1, 0, EXECUTION_VERSION, "model-a", "Be brief.", "Hello")
    );
    // The same run ID admitted again is the same transition: only the same
    // request replays.
    assert_eq!(
        stores
            .runs
            .apply(
                org,
                a.workspace,
                &admit(org, a.workspace, run, &new_run(agent_a, "Hello again"))
            )
            .await,
        Ok(Applied::Conflict(Conflict::Revision)),
        "a run is admitted once"
    );
}

/// No request or read reaches a run from its sibling workspace (same
/// organization, other environment) or another organization, and a request
/// whose own scope or ID isn't `apply`'s is refused as not found.
async fn keeps_runs_and_requests_inside_their_scope(stores: &Stores<'_>, a: Tenant, b: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Mine").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(stores, at, who, epoch, calls(&[("m1", CREATE)]), |_| needs_approval()).await;
    // Parked, so a decision has something to resolve.
    step(stores, at, &facts(stores, who, Some(epoch))).await;
    let parked = snapshot(stores, at).await;
    assert_eq!(parked.control.status, RunStatus::WaitingForApproval);
    let call_id = parked.calls[0].id;
    let before = state(stores, at).await;

    for (organization, workspace) in [
        (org, a.sibling),
        (b.organization_id, b.workspace),
        (b.organization_id, a.workspace),
        (org, b.workspace),
    ] {
        let there = At {
            org: organization,
            ws: workspace,
            run: at.run,
        };
        assert_eq!(
            stores.runs.find_run(organization, workspace, at.run).await.unwrap(),
            None
        );
        assert_eq!(
            stores.runs.find_snapshot(organization, workspace, at.run).await,
            Ok(None)
        );
        assert_eq!(
            stores.runs.list_events(organization, workspace, at.run, 0).await,
            Err(StoreError::NotFound)
        );
        // Requests built for the other scope, with the run's real IDs.
        let mut elsewhere = parked.clone();
        elsewhere.organization_id = organization;
        elsewhere.workspace_id = workspace;
        let (key, name) = api_key();
        let decide = resolve_approval(
            organization,
            workspace,
            &elsewhere,
            caller(),
            Approval {
                call_id,
                decision: ApprovalDecision::Approved,
                api_key_id: key,
                api_key_name: name,
                reason: None,
            },
        )
        .unwrap();
        let stop = cancel(organization, workspace, &elsewhere, caller(), RunTrigger::Api, person()).unwrap();
        for request in [&decide, &stop] {
            assert_eq!(
                apply(stores, there, request).await,
                Err(StoreError::NotFound),
                "no request from another scope"
            );
            // The same request applied in the run's own scope: its scope
            // isn't `apply`'s.
            assert_eq!(
                apply(stores, at, request).await,
                Err(StoreError::NotFound),
                "a request is applied only in its own scope"
            );
        }
        // The run's own requests, applied from the other scope.
        let mine = cancel(org, a.workspace, &parked, caller(), RunTrigger::Api, person()).unwrap();
        assert_eq!(
            stores.runs.apply(organization, workspace, &mine).await,
            Err(StoreError::NotFound),
            "a request names its own scope"
        );
    }
    // A request whose ID isn't derived from its run and revision.
    let mut other_run = parked.clone();
    other_run.run_id = RunId::generate();
    let wrong = cancel(org, a.workspace, &other_run, caller(), RunTrigger::Api, person()).unwrap();
    assert_eq!(
        apply(stores, at, &wrong).await,
        Err(StoreError::NotFound),
        "a request for another run finds no run"
    );
    assert_eq!(state(stores, at).await, before, "untouched from elsewhere");
    assert!(
        stores
            .runs
            .list_runs(b.organization_id, b.workspace, &RunFilter::default(), None, 100)
            .await
            .unwrap()
            .items
            .iter()
            .all(|run| run.id != at.run)
    );
    let stop = cancel(org, a.workspace, &parked, caller(), RunTrigger::Api, person()).unwrap();
    committed(apply(stores, at, &stop).await);
}

async fn lists_newest_first_in_scope(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    let start = stores.clock.now();
    let base = start.millis() + 1_000_000;
    let at = |offset: i64| Timestamp::from_millis(base + offset);
    let mut made = Vec::new();
    for offset in 0..5 {
        stores.clock.set(at(offset));
        made.push(admitted(stores, org, a.workspace, agent_a, "Page").await.run);
    }
    // Two at the same instant, ordered by ID.
    stores.clock.set(at(10));
    made.push(admitted(stores, org, a.workspace, agent_a, "Same").await.run);
    made.push(admitted(stores, org, a.workspace, agent_a, "Same").await.run);
    stores.clock.set(at(20));
    let sibling = admitted(stores, org, a.sibling, agent_sibling, "Sibling").await.run;
    stores.clock.set(at(30));
    let theirs = admitted(stores, b.organization_id, b.workspace, agent_b, "Theirs")
        .await
        .run;

    let mut seen = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = stores
            .runs
            .list_runs(org, a.workspace, &RunFilter::default(), cursor.as_deref(), 2)
            .await
            .unwrap();
        assert!(page.items.len() <= 2);
        assert!(page.total >= 7);
        seen.extend(page.items.iter().map(|run| run.id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let mut expected = made.clone();
    expected.reverse();
    assert_eq!(&seen[..7], expected.as_slice(), "newest first, then by ID");
    assert!(
        !seen.contains(&sibling) && !seen.contains(&theirs),
        "only the workspace's own"
    );
    assert_eq!(
        stores
            .runs
            .list_runs(org, a.workspace, &RunFilter::default(), Some("not-a-cursor"), 10)
            .await,
        Err(StoreError::InvalidCursor)
    );
    let by_agent = stores
        .runs
        .list_runs(
            org,
            a.workspace,
            &RunFilter {
                agent_id: Some(agent_sibling),
                status: None,
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(by_agent.total, 0, "another workspace's agent matches nothing here");
    let cancelled = At {
        org,
        ws: a.workspace,
        run: made[0],
    };
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, cancelled).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, cancelled, &stop).await);
    let by_status = stores
        .runs
        .list_runs(
            org,
            a.workspace,
            &RunFilter {
                agent_id: Some(agent_a),
                status: Some(RunStatus::Cancelled),
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert!(by_status.items.iter().any(|run| run.id == made[0]));
    assert!(by_status.items.iter().all(|run| run.status == RunStatus::Cancelled));

    let organization = |filter: OrganizationRunFilter| async move {
        stores
            .runs
            .list_organization_runs(org, &filter, None, 100)
            .await
            .unwrap()
    };
    let all = organization(OrganizationRunFilter::default()).await;
    let ids: Vec<RunId> = all.items.iter().map(|item| item.run.id).collect();
    assert!(ids.contains(&sibling) && ids.contains(&made[0]), "both workspaces");
    assert!(!ids.contains(&theirs), "never another organization's");
    let production = organization(OrganizationRunFilter {
        environment: Some(Environment::Production),
        ..OrganizationRunFilter::default()
    })
    .await;
    assert_eq!(
        production.items.iter().map(|item| item.run.id).collect::<Vec<_>>(),
        [sibling],
        "one environment"
    );
    assert_eq!(production.items[0].environment, Environment::Production);
    assert_eq!(production.items[0].run.workspace_id, a.sibling);
    let foreign = organization(OrganizationRunFilter {
        workspace_id: Some(b.workspace),
        ..OrganizationRunFilter::default()
    })
    .await;
    assert_eq!(foreign.total, 0, "another organization's workspace matches nothing");

    let activity = stores.runs.agent_activity(org, a.workspace, at(5)).await.unwrap();
    assert_eq!(activity.len(), 1, "the workspace's own agents");
    assert_eq!(activity[0].agent_id, agent_a);
    assert_eq!(activity[0].runs_since, 2, "runs since the instant");
    assert_eq!(activity[0].last_run_at, Some(at(10)));
    stores.clock.set(at(1_000));
}

/// A whole run, as a worker drives it: each step commits once, with what it
/// implies, and the store's records are what the reducer reads back.
async fn walks_a_run_through_every_step(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Steps").await;
    let job = run_job(stores, at).await.expect("its job");

    // Claimed and started, once.
    let who = claimant();
    let (plan, started) = step(stores, at, &facts(stores, who, None)).await;
    assert_eq!(kinds(&started), [RunEventType::RunStarted]);
    assert_eq!(
        started.events[0].data,
        json!({
            "model": "model-a",
            "model_id": run_model().model_id,
            "connection_id": run_model().connection_id,
        }),
        "the model the run was admitted with"
    );
    assert_eq!(started.events[0].sequence, 2);
    assert_eq!(started.status, RunStatus::Running);
    assert_eq!(
        apply(stores, at, &request(&plan)).await,
        Ok(Applied::Committed(started.clone())),
        "the same claim again: the same receipt"
    );
    let run = stores.runs.find_run(org, a.workspace, at.run).await.unwrap().unwrap();
    assert_eq!(run.started_at, Some(started.committed_at));
    let epoch = 1;
    assert_eq!(snapshot(stores, at).await.control.owner_claimant, Some(who));

    // The model attempt, before the request goes out.
    let prepared = prepare(stores, at, who, epoch).await;
    assert!(prepared.events.is_empty(), "nothing on the timeline");
    let recorded = snapshot(stores, at).await;
    assert_eq!(recorded.model_attempts.len(), 1);
    assert_eq!(
        (
            recorded.model_attempts[0].turn,
            recorded.model_attempts[0].attempt,
            recorded.model_attempts[0].state
        ),
        (0, 1, ModelAttemptState::Prepared)
    );
    assert_eq!(recorded.model_attempts[0].offer, offer(), "the offer, frozen");

    // The whole answer: a read, a write that needs a person, a tool it
    // wasn't given, and a read the organization denies by the time it's sent.
    let turn = answer(
        stores,
        at,
        who,
        epoch,
        calls(&[("m1", LIST), ("m2", CREATE), ("m3", "imagined"), ("m4", LIST)]),
        |name| match name {
            LIST => allow(),
            _ => needs_approval(),
        },
    )
    .await;
    assert_eq!(
        kinds(&turn),
        [RunEventType::MessageAssistant],
        "a refusal without a target leaves no event"
    );
    let recorded = snapshot(stores, at).await;
    assert_eq!(recorded.turns.len(), 1);
    assert_eq!(recorded.turns[0].text.as_deref(), Some("Working on it."));
    assert_eq!(recorded.model_attempts[0].state, ModelAttemptState::Answered);
    let states: Vec<CallState> = recorded.calls.iter().map(|call| call.state).collect();
    assert_eq!(
        states,
        [
            CallState::Allowed,
            CallState::AwaitingApproval,
            CallState::Refused,
            CallState::Allowed
        ],
        "the turn's calls, in order"
    );
    let read = &recorded.calls[0];
    let target = read.target.as_ref().expect("a target");
    assert_eq!(
        (
            target.installation_id.expect("an app tool"),
            target.installation_short_id.as_str(),
            target.app_key.as_str(),
            target.tool.as_str()
        ),
        (work(), "k7m2qd", "taskboard", "list_tasks"),
        "the installation behind the name"
    );
    assert_eq!(
        read.decision,
        Some(allow()),
        "the decision, with its policy, mode and rule"
    );
    assert!(read.call_digest.is_some(), "the call's digest");
    assert_eq!(recorded.calls[2].target, None, "a tool it wasn't given has no target");
    assert_eq!(recorded.calls[2].call_digest, None);
    assert!(recorded.calls[2].outcome.is_some(), "a refusal is settled as recorded");
    assert_eq!(
        recorded.control.continuation,
        Continuation::NeedCall { turn: 0, position: 0 }
    );
    let run = stores.runs.find_run(org, a.workspace, at.run).await.unwrap().unwrap();
    assert_eq!((run.usage.input_tokens, run.usage.output_tokens), (10, 3));
    assert_eq!(
        (run.usage.cache_read_input_tokens, run.usage.cache_write_input_tokens),
        (6, 2),
        "the parts of the input read from and written to the provider's cache"
    );

    // The read: authorized, sent, recorded.
    let send = authorize(stores, at, who, epoch, allow()).await;
    assert_eq!(
        (send.installation_id, send.tool.as_str()),
        (work(), "list_tasks"),
        "sent only to its own installation, under the MCP name"
    );
    let sending = snapshot(stores, at).await;
    let attempt = sending.calls[0].attempt(1).expect("the send claim");
    assert_eq!(
        (attempt.dispatch_epoch, attempt.invocation_id, attempt.outcome_recorded),
        (epoch, send.invocation_id, false)
    );
    assert_eq!(sending.calls[0].state, CallState::Started);
    stores.clock.advance(Duration::from_millis(250));
    let reported = report(stores, at, &send, completed("Q3 notes")).await;
    assert_eq!(kinds(&reported), [RunEventType::ToolCallCompleted]);
    let data = &reported.events[0].data;
    assert_eq!(data["call_id"], json!(send.call_id));
    assert_eq!(data["installation_id"], json!(work()));
    assert_eq!(data["installation_label"], json!("Work"));
    assert_eq!(data["app_name"], json!("Taskboard"));
    assert_eq!(data["tool"], json!("list_tasks"));
    assert_eq!(data["result"], json!({ "title": "Q3 notes" }));
    assert_eq!(data["duration_ms"], json!(250), "timed on the store's clock");
    assert_eq!(reported.continuation, Continuation::NeedCall { turn: 0, position: 1 });

    // The write waits for a person: parked, its claim and job let go.
    let (_, parked) = step(stores, at, &facts(stores, who, Some(epoch))).await;
    assert_eq!(kinds(&parked), [RunEventType::ApprovalRequested]);
    assert_eq!(parked.events[0].data["arguments"], json!({ "title": "Task m2" }));
    assert_eq!(
        (parked.status, parked.lease_until),
        (RunStatus::WaitingForApproval, None)
    );
    assert_eq!(
        stores.jobs.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "a waiting run's job is released with the change"
    );
    assert!(
        run_job(stores, at).await.is_none(),
        "nothing takes a waiting run forward"
    );

    // Approved: the run runs again, with a new job.
    let waiting = snapshot(stores, at).await;
    let write = waiting.calls[1].id;
    let (key, key_name) = api_key();
    let approve = resolve_approval(
        org,
        a.workspace,
        &waiting,
        caller(),
        Approval {
            call_id: write,
            decision: ApprovalDecision::Approved,
            api_key_id: key,
            api_key_name: key_name,
            reason: None,
        },
    )
    .unwrap();
    let resolved = committed(apply(stores, at, &approve).await);
    assert_eq!(kinds(&resolved), [RunEventType::ApprovalResolved]);
    assert_eq!(
        resolved.events[0].data,
        json!({ "call_id": write, "decision": "approved", "api_key_id": key })
    );
    assert_eq!(resolved.status, RunStatus::Running);
    let approved = snapshot(stores, at).await;
    assert_eq!(approved.calls[1].state, CallState::Approved);
    assert_eq!(approved.calls[1].approval, Some((ApprovalDecision::Approved, key)));
    let audit = stores
        .audit
        .list_audit_events(
            org,
            &AuditFilter {
                workspace_id: Some(a.workspace),
            },
            None,
            10,
        )
        .await
        .unwrap();
    let recorded = audit
        .items
        .iter()
        .find(|event| event.target.object == AuditObject::Run(at.run))
        .expect("the decision's audit row");
    assert_eq!(recorded.action, AuditAction::ApprovalResolved);
    assert_eq!(
        recorded.actor,
        AuditActor::ApiKey {
            id: key,
            name: "Backend".to_owned()
        }
    );
    assert_eq!(recorded.target.name, "Here", "named by its agent");
    assert_eq!(recorded.created_at, resolved.committed_at);
    let job = run_job(stores, at).await.expect("a decision queues the run again");

    // A new worker claims it, sends the write, then the denied one is refused.
    let next = claimant();
    let epoch = claim(stores, at, next).await;
    assert_eq!(epoch, 2, "each claim raises the epoch");
    let send = authorize(stores, at, next, epoch, needs_approval()).await;
    assert_eq!((send.installation_id, send.tool.as_str()), (personal(), "create_task"));
    report(stores, at, &send, completed("Hello")).await;
    // The last call: the organization denies it now.
    let plan = at_send(stores, at, next, epoch, deny()).await;
    assert_eq!(plan.io(), &Io::Nothing, "a refused call is never sent");
    let refused = committed(apply(stores, at, &request(&plan)).await);
    assert_eq!(kinds(&refused), [RunEventType::ToolCallFailed]);
    assert_eq!(refused.events[0].data["error"]["code"], json!("tool_refused"));
    assert_eq!(refused.continuation, Continuation::NeedModel { turn: 1 });

    // The final answer ends the run with its turn.
    prepare(stores, at, next, epoch).await;
    let done = answer(stores, at, next, epoch, Reply::Text("Done.".to_owned()), |_| allow()).await;
    assert_eq!(
        kinds(&done),
        [RunEventType::MessageAssistant, RunEventType::RunCompleted]
    );
    assert_eq!(
        done.events[1].data,
        json!({ "usage": {
            "input_tokens": 20,
            "output_tokens": 6,
            "cache_read_input_tokens": 12,
            "cache_write_input_tokens": 4,
            "tool_calls": 2,
        } }),
        "the run's totals"
    );
    assert_eq!(
        (done.status, done.continuation, done.lease_until),
        (RunStatus::Completed, Continuation::Done, None)
    );
    let run = stores.runs.find_run(org, a.workspace, at.run).await.unwrap().unwrap();
    assert_eq!(
        (run.status, run.output.as_deref()),
        (RunStatus::Completed, Some("Done."))
    );
    assert_eq!(run.usage.tool_calls, 2, "calls sent are counted");
    assert_eq!(run.finished_at, Some(done.committed_at));
    assert_eq!(
        stores.jobs.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "a finished run's job is released"
    );
    assert!(
        plan_idle(stores, at, &facts(stores, next, Some(epoch))).await,
        "nothing more to do"
    );

    let late_cancel = by_hand(
        stores,
        at,
        Guards::default(),
        Operation::Cancel(zupflow_core::runs::transition::Cancel {
            request_id: caller(),
            source: RunTrigger::Api,
            actor: person(),
            closing: Closing {
                calls: Vec::new(),
                outcome: zupflow_core::runs::CallOutcome {
                    content: "Cancelled.".to_owned(),
                    is_error: true,
                },
                failure: Failure::new("run_cancelled", "Cancelled."),
            },
            abandons: None,
        }),
    )
    .await;
    refuses_with_nothing_written(stores, at, &late_cancel, "a cancel of a finished run").await;
    let events = stores.runs.list_events(org, a.workspace, at.run, 0).await.unwrap();
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>(), "no gaps");
    assert_eq!(events.first().map(|event| event.kind), Some(RunEventType::RunQueued));
    assert_eq!(events.last().map(|event| event.kind), Some(RunEventType::RunCompleted));
    assert_eq!(
        stores
            .runs
            .list_events(org, a.workspace, at.run, 3)
            .await
            .unwrap()
            .first()
            .map(|event| event.sequence),
        Some(4),
        "after a sequence"
    );
    let announced: Vec<WebhookEventType> = announced(stores, at.run)
        .await
        .into_iter()
        .map(|(_, announcement)| announcement.kind)
        .collect();
    assert_eq!(
        announced,
        [WebhookEventType::RunWaitingForApproval, WebhookEventType::RunCompleted]
    );
}

async fn plan_idle(stores: &Stores<'_>, at: At, facts: &Facts) -> bool {
    matches!(plan(stores, at, facts).await, Plan::Idle(_))
}

/// The same request again answers the first receipt and changes nothing;
/// another request from the same revision conflicts, and changes nothing.
async fn replays_a_request_and_conflicts_on_another(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Twice").await;
    let first = claimant();
    let second = claimant();
    let mine = request(&plan(stores, at, &facts(stores, first, None)).await);
    let theirs = request(&plan(stores, at, &facts(stores, second, None)).await);
    assert_eq!(mine.id(), theirs.id(), "two claims from one revision share an ID");
    let receipt = committed(apply(stores, at, &mine).await);
    let after = state(stores, at).await;
    stores.clock.advance(Duration::from_secs(1));
    assert_eq!(
        apply(stores, at, &mine).await,
        Ok(Applied::Committed(receipt.clone())),
        "a retry after an unclear commit: the same receipt, its time and events included"
    );
    assert_eq!(state(stores, at).await, after, "and nothing more");
    refuses_without_change(
        stores,
        at,
        &theirs,
        Ok(Applied::Conflict(Conflict::Revision)),
        "the same ID with another digest",
    )
    .await;
    // Even once the run moved on, the first request still finds its receipt,
    // and the other still conflicts.
    prepare(stores, at, first, 1).await;
    let again = committed(apply(stores, at, &mine).await);
    let now = stores.runs.find_run(at.org, at.ws, at.run).await.unwrap().unwrap();
    assert_eq!(*again.run, now, "with the run as it is now");
    assert_eq!(
        again,
        Receipt {
            run: Box::new(now),
            ..receipt
        }
    );
    assert_eq!(
        apply(stores, at, &theirs).await,
        Ok(Applied::Conflict(Conflict::Revision))
    );
    // A request from a revision the run hasn't reached conflicts too.
    let current = snapshot(stores, at).await.control.revision;
    let ahead = TransitionRequest::new(
        at.org,
        at.ws,
        at.run,
        current + 1,
        owner(1, first, false),
        Operation::Yield(Yield {
            why: YieldReason::StepBudget,
        }),
    );
    refuses_without_change(
        stores,
        at,
        &ahead,
        Ok(Applied::Conflict(Conflict::Revision)),
        "a request from a revision ahead of the run",
    )
    .await;
    // A step planned from an older revision conflicts: reload, recompute.
    let stale = by_hand(
        stores,
        at,
        owner(1, first, false),
        Operation::Yield(Yield {
            why: YieldReason::StepBudget,
        }),
    )
    .await;
    prepare_conflict(stores, at, stale).await;
}

/// `request`, once the run moved past its revision: a revision conflict.
async fn prepare_conflict(stores: &Stores<'_>, at: At, request: TransitionRequest) {
    let who = snapshot(stores, at).await.control.owner_claimant.unwrap();
    let epoch = snapshot(stores, at).await.control.owner_epoch;
    // Move the run on with another step of its owner: a model attempt
    // abandoned and prepared again.
    step(stores, at, &facts(stores, who, Some(epoch))).await;
    refuses_without_change(
        stores,
        at,
        &request,
        Ok(Applied::Conflict(Conflict::Revision)),
        "a request from an older revision",
    )
    .await;
}

/// A worker's step needs its own claim, under a live lease: another epoch,
/// another claimant of the same epoch, or a lease run out on the store's
/// clock is refused. Once the lease runs out another worker takes over.
async fn refuses_a_stale_or_expired_owner(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Owners").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    let prepare_by = |epoch: u64, claimant: ClaimantId| {
        (
            owner(epoch, claimant, true),
            Operation::PrepareModel(PrepareModel {
                turn: 0,
                attempt: 1,
                abandons: None,
                request_digest: Sha256Digest::of(b"request"),
                offer: offer(),
                memory: None,
                key_version: Some(1),
                steer: None,
            }),
        )
    };
    for ((guards, operation), why) in [
        (prepare_by(epoch + 1, who), "another epoch"),
        (prepare_by(epoch - 1, who), "a stale epoch"),
        (prepare_by(epoch, claimant()), "another claimant of the same epoch"),
    ] {
        let request = by_hand(stores, at, guards, operation).await;
        refuses_without_change(stores, at, &request, Ok(Applied::Conflict(Conflict::Owner)), why).await;
    }
    // Another worker can't claim while the lease holds.
    let rival = claimant();
    let early = by_hand(
        stores,
        at,
        Guards {
            not_cancelled: true,
            no_live_claim: true,
            ..Guards::default()
        },
        Operation::Claim(Claim {
            epoch: epoch + 1,
            claimant: rival,
            lease_ms: u64::try_from(LEASE.as_millis()).unwrap(),
            starts: false,
        }),
    )
    .await;
    refuses_without_change(
        stores,
        at,
        &early,
        Ok(Applied::Conflict(Conflict::Claimed)),
        "a claim while another's lease holds",
    )
    .await;
    // The owner's lease runs out on the store's clock: its own step is
    // refused, even planned while it held.
    let planned = request(&plan(stores, at, &facts(stores, who, Some(epoch))).await);
    stores.clock.advance(LEASE);
    refuses_without_change(
        stores,
        at,
        &planned,
        Ok(Applied::Conflict(Conflict::Owner)),
        "a step once the lease ran out",
    )
    .await;
    assert!(!Conflict::Owner.reload(), "an owner conflict means stop");
    // Another worker takes over; the first one's request still loses.
    let epoch = claim(stores, at, rival).await;
    assert_eq!(epoch, 2);
    refuses_without_change(
        stores,
        at,
        &planned,
        Ok(Applied::Conflict(Conflict::Revision)),
        "a stale worker after a takeover",
    )
    .await;
    assert!(
        plan_idle(stores, at, &facts(stores, who, Some(1))).await,
        "replanned, it knows it lost"
    );
    prepare(stores, at, rival, epoch).await;
}

/// A step without the guards its operation needs is refused, whatever its
/// content: the store never applies a worker's step unfenced.
async fn refuses_steps_without_their_guards(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Unguarded").await;
    let who = claimant();
    let claim_op = Operation::Claim(Claim {
        epoch: 1,
        claimant: who,
        lease_ms: u64::try_from(LEASE.as_millis()).unwrap(),
        starts: true,
    });
    let unguarded_claim = by_hand(stores, at, Guards::default(), claim_op.clone()).await;
    refuses_with_nothing_written(stores, at, &unguarded_claim, "a claim that ignores a live claim").await;
    for (epoch, starts, why) in [
        (2, true, "a claim that skips an epoch"),
        (1, false, "a claim that doesn't start a queued run"),
    ] {
        let misfit = by_hand(
            stores,
            at,
            Guards {
                not_cancelled: true,
                no_live_claim: true,
                ..Guards::default()
            },
            Operation::Claim(Claim {
                epoch,
                claimant: who,
                lease_ms: u64::try_from(LEASE.as_millis()).unwrap(),
                starts,
            }),
        )
        .await;
        refuses_with_nothing_written(stores, at, &misfit, why).await;
    }
    let epoch = claim(stores, at, who).await;
    let prepare_op = Operation::PrepareModel(PrepareModel {
        turn: 0,
        attempt: 1,
        abandons: None,
        request_digest: Sha256Digest::of(b"request"),
        offer: offer(),
        memory: None,
        key_version: Some(1),
        steer: None,
    });
    for (guards, why) in [
        (Guards::default(), "a worker's step without its owner"),
        (
            Guards {
                not_cancelled: false,
                ..owner(epoch, who, true)
            },
            "a worker's step that doesn't stop at a cancel",
        ),
        (
            owner(epoch, who, false),
            "a step that keeps working but lets its claim go",
        ),
    ] {
        let request = by_hand(stores, at, guards, prepare_op.clone()).await;
        refuses_with_nothing_written(stores, at, &request, why).await;
    }
    // A step that doesn't fit the run as committed.
    let wrong_turn = by_hand(
        stores,
        at,
        owner(epoch, who, true),
        Operation::PrepareModel(PrepareModel {
            turn: 3,
            attempt: 1,
            abandons: None,
            request_digest: Sha256Digest::of(b"request"),
            offer: offer(),
            memory: None,
            key_version: Some(1),
            steer: None,
        }),
    )
    .await;
    refuses_with_nothing_written(stores, at, &wrong_turn, "a model attempt for a turn the run isn't at").await;
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
}

/// A decision resolves only the call the run waits on, while it waits, bound
/// to that call's digest; two callers deciding at once: one wins. A denial
/// is what the model reads back.
async fn decides_only_the_call_the_run_waits_on(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Decide").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(stores, at, who, epoch, calls(&[("m1", CREATE), ("m2", CREATE)]), |_| {
        needs_approval()
    })
    .await;
    let running = snapshot(stores, at).await;
    let (key, name) = api_key();
    let decide =
        |snapshot: &Snapshot, call_id: ToolCallId, decision: ApprovalDecision, reason: Option<&'static str>| {
            resolve_approval(
                org,
                a.workspace,
                snapshot,
                caller(),
                Approval {
                    call_id,
                    decision,
                    api_key_id: key,
                    api_key_name: name,
                    reason,
                },
            )
        };
    assert!(
        decide(&running, running.calls[0].id, ApprovalDecision::Approved, None).is_err(),
        "a running run waits for nothing"
    );
    // A decision built by hand while the run runs: refused, nothing changed.
    let mut pretend = running.clone();
    pretend.control.status = RunStatus::WaitingForApproval;
    pretend.control.continuation = Continuation::AwaitApproval {
        call_id: running.calls[0].id,
    };
    let early = decide(&pretend, running.calls[0].id, ApprovalDecision::Approved, None).unwrap();
    let before = state(stores, at).await;
    assert!(matches!(apply(stores, at, &early).await, Err(StoreError::Refused(_))));
    assert_eq!(state(stores, at).await, before);

    step(stores, at, &facts(stores, who, Some(epoch))).await;
    let waiting = snapshot(stores, at).await;
    let (first, second) = (waiting.calls[0].id, waiting.calls[1].id);
    assert!(
        decide(&waiting, second, ApprovalDecision::Approved, None).is_err(),
        "only the call it waits on"
    );
    let mut other = waiting.clone();
    other.control.continuation = Continuation::AwaitApproval { call_id: second };
    let wrong_call = decide(&other, second, ApprovalDecision::Approved, None).unwrap();
    let before = state(stores, at).await;
    assert!(matches!(
        apply(stores, at, &wrong_call).await,
        Err(StoreError::Refused(_))
    ));
    assert_eq!(state(stores, at).await, before, "not the call it waits on");
    // Bound to the digest the call holds.
    let mut tampered = waiting.clone();
    tampered.calls[0].call_digest = Some(Sha256Digest::of(b"another call"));
    let unbound = decide(&tampered, first, ApprovalDecision::Approved, None).unwrap();
    let before = state(stores, at).await;
    assert!(matches!(apply(stores, at, &unbound).await, Err(StoreError::Refused(_))));
    assert_eq!(state(stores, at).await, before, "a decision bound to another digest");

    // Two callers at once: one wins, the other conflicts.
    let deny_it = decide(&waiting, first, ApprovalDecision::Denied, Some("Not now.")).unwrap();
    let approve_it = decide(&waiting, first, ApprovalDecision::Approved, None).unwrap();
    let denied = committed(apply(stores, at, &deny_it).await);
    assert_eq!(
        apply(stores, at, &approve_it).await,
        Ok(Applied::Conflict(Conflict::Revision)),
        "one winner"
    );
    assert_eq!(kinds(&denied), [RunEventType::ApprovalResolved], "one event");
    let decided = snapshot(stores, at).await;
    assert_eq!(decided.calls[0].state, CallState::Denied);
    assert_eq!(
        decided.calls[0]
            .outcome
            .as_ref()
            .map(|outcome| outcome.content.as_str()),
        Some("A person denied this call, so it wasn't made. Their reason: Not now."),
        "a denial is what the model reads back"
    );
    assert_eq!(
        decided.control.continuation,
        Continuation::NeedCall { turn: 0, position: 1 }
    );
    let audit = stores
        .audit
        .list_audit_events(
            org,
            &AuditFilter {
                workspace_id: Some(a.workspace),
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(
        audit
            .items
            .iter()
            .filter(|event| event.target.object == AuditObject::Run(at.run))
            .count(),
        1,
        "one audit row"
    );
    let stop = cancel(org, a.workspace, &decided, caller(), RunTrigger::Api, person()).unwrap();
    committed(apply(stores, at, &stop).await);
}

/// A call waiting for approval on an installation that was deleted or
/// disconnected (decision 0047): refused before the run parks on it, or,
/// once parked, by the job the installation's change queued, only with the
/// reason it went; the run moves on and its job is queued. The runs such
/// calls wait in are found only in their own workspace and organization.
async fn refuses_a_waiting_call_whose_installation_went(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    // The same installation ID in a run of each other scope, parked on it:
    // none is found, or refused, from here, nor this one from there.
    let mut elsewhere = Vec::new();
    for (organization, workspace, agent) in [
        (org, a.sibling, agent_sibling),
        (b.organization_id, b.workspace, agent_b),
    ] {
        let there = admitted(stores, organization, workspace, agent, "Elsewhere").await;
        let who = claimant();
        let epoch = claim(stores, there, who).await;
        prepare(stores, there, who, epoch).await;
        answer(stores, there, who, epoch, calls(&[("m1", CREATE)]), |_| {
            needs_approval()
        })
        .await;
        step(stores, there, &facts(stores, who, Some(epoch))).await;
        assert_eq!(
            snapshot(stores, there).await.control.status,
            RunStatus::WaitingForApproval
        );
        elsewhere.push(there);
    }

    // Parked on a call to Personal, a second one to Personal after it.
    let at = admitted(stores, org, a.workspace, agent_a, "Waiting").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(
        stores,
        at,
        who,
        epoch,
        calls(&[("m1", CREATE), ("m2", CREATE), ("m3", LIST)]),
        |name| if name == LIST { allow() } else { needs_approval() },
    )
    .await;
    // Not parked yet: found all the same, for the job to wait on.
    assert_eq!(
        stores
            .runs
            .runs_waiting_on(org, a.workspace, personal(), None, 10)
            .await
            .unwrap(),
        [at.run]
    );
    step(stores, at, &facts(stores, who, Some(epoch))).await;
    let parked = snapshot(stores, at).await;
    assert_eq!(parked.control.status, RunStatus::WaitingForApproval);
    let (first, second) = (parked.calls[0].id, parked.calls[1].id);
    assert_eq!(
        parked.control.continuation,
        Continuation::AwaitApproval { call_id: first }
    );

    for there in &elsewhere {
        assert_eq!(
            stores
                .runs
                .runs_waiting_on(there.org, there.ws, personal(), None, 10)
                .await
                .unwrap(),
            [there.run],
            "each scope finds its own"
        );
    }
    for (organization, workspace) in [
        (org, a.sibling),
        (b.organization_id, b.workspace),
        (b.organization_id, a.workspace),
        (org, b.workspace),
    ] {
        let found = stores
            .runs
            .runs_waiting_on(organization, workspace, personal(), None, 10)
            .await
            .unwrap();
        assert!(!found.contains(&at.run), "another scope never finds it");
    }
    assert_eq!(
        stores
            .runs
            .runs_waiting_on(org, a.workspace, personal(), None, 10)
            .await
            .unwrap(),
        [at.run]
    );
    assert!(
        stores
            .runs
            .runs_waiting_on(org, a.workspace, work(), None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing waits on another installation"
    );
    assert!(
        stores
            .runs
            .runs_waiting_on(org, a.workspace, personal(), Some(at.run), 10)
            .await
            .unwrap()
            .is_empty(),
        "paged after a run"
    );

    // The job's request, applied in another scope: not found, nothing changed.
    let request = refuse_waiting(
        org,
        a.workspace,
        &parked,
        caller(),
        WentAway::Installation(personal()),
        Gone::Removed,
    )
    .unwrap();
    let before = state(stores, at).await;
    for (organization, workspace) in [
        (org, a.sibling),
        (b.organization_id, b.workspace),
        (b.organization_id, a.workspace),
    ] {
        assert_eq!(
            stores.runs.apply(organization, workspace, &request).await,
            Err(StoreError::NotFound)
        );
    }
    assert_eq!(state(stores, at).await, before, "nothing changed");
    // Nor can the job reach another scope's run with this run's request.
    for there in &elsewhere {
        let theirs = state(stores, *there).await;
        let pretend = TransitionRequest::new(
            there.org,
            there.ws,
            there.run,
            theirs.0.control.revision,
            Guards::default(),
            request.operation().clone(),
        );
        assert!(matches!(
            apply(stores, *there, &pretend).await,
            Err(StoreError::Refused(_))
        ));
        assert_eq!(state(stores, *there).await, theirs, "another run, untouched");
    }

    // By hand: only the call the run waits on, only for its installation,
    // only with the reason it went, moving on as it leads, unclaimed.
    let Operation::RefuseWaiting(refuse) = request.operation().clone() else {
        panic!("a refusal");
    };
    let owned = Guards {
        owner: Some(OwnerGuard {
            epoch,
            claimant: who,
            renew_lease_ms: None,
        }),
        ..Guards::default()
    };
    for (guards, operation, why) in [
        (
            Guards::default(),
            RefuseWaiting {
                call_id: second,
                ..refuse.clone()
            },
            "a call the run doesn't wait on",
        ),
        (
            Guards::default(),
            RefuseWaiting {
                went: WentAway::Installation(work()),
                ..refuse.clone()
            },
            "an installation the call isn't bound to",
        ),
        (
            Guards::default(),
            RefuseWaiting {
                failure: Failure::new("tool_refused", "No."),
                ..refuse.clone()
            },
            "another reason",
        ),
        (
            Guards::default(),
            RefuseWaiting {
                next: Continuation::NeedModel { turn: 1 },
                ..refuse.clone()
            },
            "a next step it doesn't lead to",
        ),
    ] {
        let by_hand = by_hand(stores, at, guards, Operation::RefuseWaiting(operation)).await;
        refuses_with_nothing_written(stores, at, &by_hand, why).await;
    }
    // A parked run has no owner whose claim could fence it.
    let claimed = by_hand(stores, at, owned, Operation::RefuseWaiting(refuse.clone())).await;
    refuses_without_change(
        stores,
        at,
        &claimed,
        Ok(Applied::Conflict(Conflict::Owner)),
        "under a claim",
    )
    .await;

    // The job's request commits: the call refused, never sent, the run
    // running again at the next call, its job queued.
    jobs_of(stores, RUN_JOB, Some(at.run)).await;
    let receipt = committed(apply(stores, at, &request).await);
    assert_eq!(
        (receipt.status, receipt.continuation, receipt.lease_until),
        (
            RunStatus::Running,
            Continuation::NeedCall { turn: 0, position: 1 },
            None
        )
    );
    assert_eq!(kinds(&receipt), [RunEventType::ToolCallFailed], "the timeline says why");
    assert_eq!(receipt.events[0].data["call_id"], json!(first));
    assert_eq!(receipt.events[0].data["error"]["code"], INSTALLATION_REMOVED);
    assert_eq!(
        receipt.events[0].data["error"]["message"],
        "Personal was removed, so this call wasn't made."
    );
    let job = run_job(stores, at).await.expect("the run's job, queued with it");
    assert_eq!(job.scope, Scope::Workspace(org, a.workspace));
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    let refused = snapshot(stores, at).await;
    let call = refused.call(first).unwrap();
    assert_eq!(call.state, CallState::Refused);
    assert_eq!(
        call.outcome
            .as_ref()
            .map(|outcome| (outcome.content.as_str(), outcome.is_error)),
        Some(("Personal was removed, so this call wasn't made.", true)),
        "what the model reads back"
    );
    assert_eq!(call.approval, None, "nobody decided it");
    assert!(call.attempts.is_empty(), "never authorized to send");
    // The same request again: the same receipt, nothing more.
    let again = committed(apply(stores, at, &request).await);
    assert_eq!(
        (again.to_revision, again.events.clone()),
        (receipt.to_revision, receipt.events.clone())
    );
    assert!(run_job(stores, at).await.is_none(), "no second job");
    // Another job's, or a decision, from the same revision: one winner.
    let late = refuse_waiting(
        org,
        a.workspace,
        &parked,
        caller(),
        WentAway::Installation(personal()),
        Gone::Removed,
    )
    .unwrap();
    assert_eq!(
        apply(stores, at, &late).await,
        Ok(Applied::Conflict(Conflict::Revision))
    );
    let (key, name) = api_key();
    let decision = resolve_approval(
        org,
        a.workspace,
        &parked,
        caller(),
        Approval {
            call_id: first,
            decision: ApprovalDecision::Approved,
            api_key_id: key,
            api_key_name: name,
            reason: None,
        },
    )
    .unwrap();
    assert_eq!(
        apply(stores, at, &decision).await,
        Ok(Applied::Conflict(Conflict::Revision))
    );
    assert!(
        resolve_approval(
            org,
            a.workspace,
            &refused,
            caller(),
            Approval {
                call_id: first,
                decision: ApprovalDecision::Approved,
                api_key_id: key,
                api_key_name: name,
                reason: None,
            },
        )
        .is_err(),
        "a decision rebuilt from the run as it is: not waiting"
    );

    // The next call waits for approval on the same installation, now
    // disconnected: refused before the run parks on it, only with that reason.
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    let at_call = snapshot(stores, at).await;
    let mut wrong = facts(stores, who, Some(epoch));
    wrong.send_check = parking_check(&at_call, InstallationCheck::Disconnected);
    let Operation::AuthorizeCall(mut authorize) = request_of(&advance(&at_call, &wrong).unwrap()) else {
        panic!("a refusal at the call");
    };
    let Authorization::Refuse { outcome, failure } = authorize.authorization.clone() else {
        panic!("refused");
    };
    assert_eq!(failure.code, INSTALLATION_DISCONNECTED);
    authorize.authorization = Authorization::Refuse {
        outcome: outcome.clone(),
        failure: Failure::new("tool_refused", "No."),
    };
    let other_reason = by_hand(stores, at, owner(epoch, who, true), Operation::AuthorizeCall(authorize)).await;
    refuses_with_nothing_written(
        stores,
        at,
        &other_reason,
        "a waiting call refused for another reason than its installation",
    )
    .await;
    let (_, refused_here) = step(stores, at, &wrong).await;
    assert_eq!(kinds(&refused_here), [RunEventType::ToolCallFailed]);
    assert_eq!(refused_here.events[0].data["error"]["code"], INSTALLATION_DISCONNECTED);
    assert_eq!(refused_here.status, RunStatus::Running, "never parked");
    assert_eq!(
        refused_here.continuation,
        Continuation::NeedCall { turn: 0, position: 2 },
        "on to the next call"
    );
    assert_eq!(
        snapshot(stores, at).await.call(second).unwrap().state,
        CallState::Refused
    );
    assert!(
        stores
            .runs
            .runs_waiting_on(org, a.workspace, personal(), None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing waits on it any more"
    );
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
    for there in elsewhere {
        let theirs = snapshot(stores, there).await;
        let stop = cancel(there.org, there.ws, &theirs, caller(), RunTrigger::Api, person()).unwrap();
        committed(apply(stores, there, &stop).await);
    }
}

/// The memory type the suite's runs remember in.
fn customer_notes() -> MemoryTypeId {
    MemoryTypeId::from_uuid(Uuid::from_u128(0x0199_0000_0000_7000_8000_0000_0000_0020))
}

const RECALL: &str = "<memory>\n- Prefers replies in Portuguese.\n</memory>";

/// The suite's offer with the notes kind's tools of `memory_type_id` joined
/// to it, as a run that remembers offers them.
fn remembering(memory_type_id: MemoryTypeId) -> Offer {
    let tools = notes::tools(MemoryWrites::Approve)
        .iter()
        .map(|tool| memory_tool(memory_type_id, "Customer notes", notes::KIND, notes::NAME, tool))
        .collect();
    let (offer, left_out) = offer().with(tools);
    assert!(left_out.is_empty(), "a memory tool's name meets no app's");
    offer
}

fn frozen(memory_type_id: MemoryTypeId, recall: Option<&str>) -> FrozenMemory {
    FrozenMemory {
        memory_type_id,
        recall: recall.map(str::to_owned),
    }
}

fn memory_decision(verdict: CallVerdict, rule: &str) -> CallDecision {
    CallDecision {
        verdict,
        policy: "memory".to_owned(),
        mode: None,
        rule: rule.to_owned(),
    }
}

/// The model calls one memory tool, with one argument.
fn remember(id: &str, tool: &str, argument: (&str, &str)) -> Reply {
    let mut arguments = Map::new();
    arguments.insert(argument.0.to_owned(), json!(argument.1));
    Reply::ToolCalls {
        text: None,
        calls: vec![ModelToolCall {
            id: id.to_owned(),
            name: tool.to_owned(),
            arguments,
        }],
    }
}

/// A run keeps the memory it was started with, and the memory type and
/// recall its first model attempt froze with its offer: every later attempt
/// is prepared with the same, whatever a worker resolved since; a run
/// started with memory off recalls nothing, and a recall is never empty. A
/// memory tool's call keeps its type, with no short ID, through its decision
/// and send, and one waiting for approval is refused before the run parks on
/// it only because the type was deleted.
async fn keeps_a_runs_memory_with_its_attempts(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    let notes_type = customer_notes();
    let prepare_with = |memory: Option<FrozenMemory>, turn: u32, attempt: u32, abandons: Option<u32>| {
        Operation::PrepareModel(PrepareModel {
            turn,
            attempt,
            abandons,
            request_digest: Sha256Digest::of(b"request"),
            offer: remembering(notes_type),
            memory,
            key_version: Some(1),
            steer: None,
        })
    };

    // Started with memory off: the run and its snapshot say so, and no
    // attempt recalls anything. Its memory tools are frozen all the same.
    let run = RunId::generate();
    let quiet = At { org, ws, run };
    let off = NewRun {
        memory: MemoryMode::Off,
        ..new_run(agent_a, "Without memory")
    };
    committed(apply(stores, quiet, &admit(org, ws, run, &off)).await);
    assert_eq!(
        stores.runs.find_run(org, ws, run).await.unwrap().unwrap().memory,
        MemoryMode::Off
    );
    assert_eq!(snapshot(stores, quiet).await.memory, MemoryMode::Off);
    let who = claimant();
    let epoch = claim(stores, quiet, who).await;
    let recalling = by_hand(
        stores,
        quiet,
        owner(epoch, who, true),
        prepare_with(Some(frozen(notes_type, Some(RECALL))), 0, 1, None),
    )
    .await;
    refuses_with_nothing_written(stores, quiet, &recalling, "a recall for a run started with memory off").await;
    let mut resolved = facts(stores, who, Some(epoch));
    resolved.offer = Some(remembering(notes_type));
    resolved.memory = Some(frozen(notes_type, None));
    let (planned, _) = step(stores, quiet, &resolved).await;
    let Io::CallModel(asked) = planned.io() else {
        panic!("the model is asked");
    };
    assert_eq!(asked.memory, None, "nothing recalled");
    let started = snapshot(stores, quiet).await;
    assert_eq!(
        (started.frozen_memory, &started.model_attempts[0].offer),
        (Some(frozen(notes_type, None)), &remembering(notes_type)),
        "the type kept on the run, with the attempt's offer"
    );

    // Started with the agent's memory: the first attempt freezes what it
    // resolved, recall and all.
    let at = admitted(stores, org, ws, agent_a, "Remember me").await;
    assert_eq!(
        stores.runs.find_run(org, ws, at.run).await.unwrap().unwrap().memory,
        MemoryMode::Agent
    );
    assert_eq!(snapshot(stores, at).await.memory, MemoryMode::Agent);
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    let empty = by_hand(
        stores,
        at,
        owner(epoch, who, true),
        prepare_with(Some(frozen(notes_type, Some(""))), 0, 1, None),
    )
    .await;
    refuses_with_nothing_written(stores, at, &empty, "an empty recall").await;
    let mut resolved = facts(stores, who, Some(epoch));
    resolved.offer = Some(remembering(notes_type));
    resolved.memory = Some(frozen(notes_type, Some(RECALL)));
    let (planned, _) = step(stores, at, &resolved).await;
    let Io::CallModel(asked) = planned.io() else {
        panic!("the model is asked");
    };
    assert_eq!(asked.memory.as_deref(), Some(RECALL));
    let first = snapshot(stores, at).await;
    assert_eq!(
        first.frozen_memory,
        Some(frozen(notes_type, Some(RECALL))),
        "kept on the run by its first attempt"
    );
    let save = first.model_attempts[0]
        .offer
        .find("save_memory")
        .expect("offered")
        .clone();
    assert_eq!(save.host(), ToolHost::Memory(notes_type));

    // A later attempt with any other memory is refused; the reducer's
    // prepares the same again, whatever the worker resolved now.
    for (memory, why) in [
        (None, "an attempt without the run's memory"),
        (Some(frozen(notes_type, None)), "an attempt without the run's recall"),
        (
            Some(frozen(notes_type, Some("<memory>\n- Prefers email.\n</memory>"))),
            "an attempt with another recall",
        ),
        (
            Some(frozen(MemoryTypeId::generate(), Some(RECALL))),
            "an attempt of another memory type",
        ),
    ] {
        let later = by_hand(stores, at, owner(epoch, who, true), prepare_with(memory, 0, 2, Some(1))).await;
        refuses_with_nothing_written(stores, at, &later, why).await;
    }
    let mut changed = facts(stores, who, Some(epoch));
    changed.memory = Some(frozen(notes_type, Some("<memory>\n- Prefers email.\n</memory>")));
    let (planned, _) = step(stores, at, &changed).await;
    let Io::CallModel(asked) = planned.io() else {
        panic!("the model is asked again");
    };
    assert_eq!((asked.attempt, asked.memory.as_deref()), (2, Some(RECALL)));
    let again = snapshot(stores, at).await;
    assert_eq!(
        (again.model_attempts.len(), again.frozen_memory),
        (2, Some(frozen(notes_type, Some(RECALL)))),
        "the same memory, kept once"
    );

    // A memory tool's call keeps its type, with no short ID, through its
    // send; one bound to another type than its name means is refused, even
    // digested to match.
    let commit = answer_request(
        stores,
        at,
        who,
        epoch,
        remember("m1", "save_memory", ("text", "Prefers replies in Portuguese.")),
        |_| memory_decision(CallVerdict::Allow, "allowed"),
    )
    .await;
    let Operation::CommitTurn(mut elsewhere) = commit.operation().clone() else {
        panic!("a turn");
    };
    let other_type = MemoryTypeId::generate();
    let forged = &mut elsewhere.calls[0];
    let target = forged.target.as_mut().expect("bound to its type");
    target.memory_type_id = Some(other_type);
    forged.call_digest = Some(memory_call_digest(
        other_type,
        &target.app_key,
        &target.tool,
        &target.definition_sha256,
        &forged.arguments,
    ));
    let elsewhere = by_hand(stores, at, *commit.guards(), Operation::CommitTurn(elsewhere)).await;
    refuses_with_nothing_written(stores, at, &elsewhere, "a memory call bound to another type").await;
    committed(apply(stores, at, &commit).await);
    let call = snapshot(stores, at).await.calls[0].clone();
    let target = call.target.clone().expect("bound to its type");
    assert_eq!(
        (
            target.installation_id,
            target.channel_connection_id,
            target.memory_type_id
        ),
        (None, None, Some(notes_type))
    );
    assert_eq!(
        (
            target.installation_short_id.as_str(),
            target.installation_label.as_str(),
            target.app_key.as_str(),
            target.app_name.as_str(),
            target.tool.as_str(),
        ),
        ("", "Customer notes", notes::KIND, notes::NAME, "save_memory")
    );
    assert_eq!(target.definition_sha256, save.definition_sha256);
    assert!(call.call_digest.is_some(), "decided on its digest");
    let planned = at_send(stores, at, who, epoch, memory_decision(CallVerdict::Allow, "allowed")).await;
    let Io::SendMemory(send) = planned.io().clone() else {
        panic!("expected a memory call, got {planned:?}");
    };
    assert_eq!(
        (
            send.memory_type_id,
            send.kind.as_str(),
            send.tool.as_str(),
            send.after_app_content
        ),
        (notes_type, notes::KIND, "save_memory", false)
    );
    assert_eq!(Some(send.call_digest), call.call_digest);
    committed(apply(stores, at, &request(&planned)).await);
    let mut sent = Facts::at(stores.clock.now());
    let mut result = Map::new();
    result.insert("result".to_owned(), json!("Saved."));
    sent.sent = Some(SendResult {
        call_id: send.call_id,
        attempt: send.attempt,
        invocation_id: send.invocation_id,
        outcome: SendOutcome::Completed { result },
    });
    let recorded = committed(apply(stores, at, &request(&plan(stores, at, &sent).await)).await);
    assert_eq!(kinds(&recorded), [RunEventType::ToolCallCompleted]);
    assert_eq!(recorded.events[0].data["memory_type_id"], json!(notes_type));
    assert_eq!(recorded.events[0].data["kind"], notes::KIND);
    let saved = snapshot(stores, at).await;
    assert_eq!(saved.calls[0].state, CallState::Completed);
    assert_eq!(saved.calls[0].target, Some(target), "read back as it was bound");

    // The next turn's attempt: the same memory again.
    let (planned, _) = step(stores, at, &facts(stores, who, Some(epoch))).await;
    let Io::CallModel(asked) = planned.io() else {
        panic!("the model is asked for the next turn");
    };
    assert_eq!((asked.turn, asked.memory.as_deref()), (1, Some(RECALL)));

    // A call waiting for approval whose type was deleted: refused before
    // the run parks on it, only with that reason.
    answer(
        stores,
        at,
        who,
        epoch,
        remember("m2", "forget_memory", ("memory", "k7m2qd")),
        |_| {
            memory_decision(
                CallVerdict::RequireApproval {
                    reason: "Your organization requires approval for forget_memory.".to_owned(),
                },
                "organization_requires_approval",
            )
        },
    )
    .await;
    let at_call = snapshot(stores, at).await;
    let mut gone = facts(stores, who, Some(epoch));
    gone.send_check = parking_check(&at_call, InstallationCheck::Removed);
    let Operation::AuthorizeCall(mut authorize) = request_of(&advance(&at_call, &gone).unwrap()) else {
        panic!("a refusal at the call");
    };
    let Authorization::Refuse { outcome, failure } = authorize.authorization.clone() else {
        panic!("refused");
    };
    assert_eq!(
        (failure.code.as_str(), failure.message.as_str()),
        (MEMORY_TYPE_REMOVED, MEMORY_TYPE_REMOVED_MESSAGE)
    );
    authorize.authorization = Authorization::Refuse {
        outcome,
        failure: Failure::new(INSTALLATION_REMOVED, "Personal was removed, so this call wasn't made."),
    };
    let other_reason = by_hand(stores, at, owner(epoch, who, true), Operation::AuthorizeCall(authorize)).await;
    refuses_with_nothing_written(
        stores,
        at,
        &other_reason,
        "a waiting memory call refused for an installation's reason",
    )
    .await;
    let (_, refused_here) = step(stores, at, &gone).await;
    assert_eq!(kinds(&refused_here), [RunEventType::ToolCallFailed]);
    assert_eq!(refused_here.events[0].data["error"]["code"], MEMORY_TYPE_REMOVED);
    assert_eq!(refused_here.events[0].data["memory_type_name"], "Customer notes");
    assert_eq!(refused_here.status, RunStatus::Running, "never parked");
    let forgotten = snapshot(stores, at).await;
    let call = forgotten.calls.iter().find(|call| call.model_call_id == "m2").unwrap();
    assert_eq!(call.state, CallState::Refused);
    assert_eq!(
        call.outcome
            .as_ref()
            .map(|outcome| (outcome.content.as_str(), outcome.is_error)),
        Some((MEMORY_TYPE_REMOVED_MESSAGE, true)),
        "what the model reads back"
    );

    // Once a run is finished, its recall isn't kept; its type is.
    for at in [quiet, at] {
        let stop = cancel(
            org,
            ws,
            &snapshot(stores, at).await,
            caller(),
            RunTrigger::Api,
            person(),
        )
        .unwrap();
        committed(apply(stores, at, &stop).await);
        assert_eq!(
            snapshot(stores, at).await.frozen_memory,
            Some(frozen(notes_type, None)),
            "a finished run keeps no recall"
        );
    }
}

/// A run of `agent` remembering in `memory_type_id`, its one call, to
/// forget a note, waiting for approval: the run is at the call, not parked
/// on it yet. Its claimant and epoch with it.
async fn waiting_in_type(
    stores: &Stores<'_>,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    agent: AgentId,
    memory_type_id: MemoryTypeId,
) -> (At, ClaimantId, u64) {
    let at = admitted(stores, organization_id, workspace_id, agent, "Forget that").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    let mut resolved = facts(stores, who, Some(epoch));
    resolved.offer = Some(remembering(memory_type_id));
    resolved.memory = Some(frozen(memory_type_id, Some(RECALL)));
    step(stores, at, &resolved).await;
    answer(
        stores,
        at,
        who,
        epoch,
        remember("m1", "forget_memory", ("memory", "k7m2qd")),
        |_| {
            memory_decision(
                CallVerdict::RequireApproval {
                    reason: "Your organization requires approval for forget_memory.".to_owned(),
                },
                "organization_requires_approval",
            )
        },
    )
    .await;
    (at, who, epoch)
}

/// A memory call waiting for approval in a type that was deleted: once the
/// run parked on it, refused by the job the deletion queued, only for that
/// type and only with `memory_type_removed`; the run moves on and its job
/// is queued. The runs such calls wait in are found across the
/// organization's workspaces, parked or about to park, and never from
/// another organization, even with the same type ID.
async fn refuses_a_waiting_memory_call_whose_type_went(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    let notes_type = customer_notes();
    let (here, who, epoch) = waiting_in_type(stores, org, a.workspace, agent_a, notes_type).await;
    let (sibling, sibling_who, sibling_epoch) =
        waiting_in_type(stores, org, a.sibling, agent_sibling, notes_type).await;
    let (theirs, their_who, their_epoch) =
        waiting_in_type(stores, b.organization_id, b.workspace, agent_b, notes_type).await;

    // Not parked yet: found all the same, for the job to wait on.
    let mut ours = vec![(a.workspace, here.run), (a.sibling, sibling.run)];
    ours.sort_by_key(|(_, run)| *run);
    assert_eq!(
        stores
            .runs
            .runs_waiting_in_type(org, notes_type, None, 10)
            .await
            .unwrap(),
        ours,
        "in every workspace of the organization"
    );
    for (at, who, epoch) in [
        (here, who, epoch),
        (sibling, sibling_who, sibling_epoch),
        (theirs, their_who, their_epoch),
    ] {
        step(stores, at, &facts(stores, who, Some(epoch))).await;
        assert_eq!(snapshot(stores, at).await.control.status, RunStatus::WaitingForApproval);
    }
    assert_eq!(
        stores
            .runs
            .runs_waiting_in_type(org, notes_type, None, 10)
            .await
            .unwrap(),
        ours
    );
    assert_eq!(
        stores
            .runs
            .runs_waiting_in_type(org, notes_type, None, 1)
            .await
            .unwrap(),
        ours[..1],
        "a page at a time"
    );
    assert_eq!(
        stores
            .runs
            .runs_waiting_in_type(org, notes_type, Some(ours[0].1), 10)
            .await
            .unwrap(),
        ours[1..],
        "paged after a run"
    );
    assert_eq!(
        stores
            .runs
            .runs_waiting_in_type(b.organization_id, notes_type, None, 10)
            .await
            .unwrap(),
        [(b.workspace, theirs.run)],
        "another organization finds only its own"
    );
    assert!(
        stores
            .runs
            .runs_waiting_in_type(org, MemoryTypeId::generate(), None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing waits in another type"
    );
    assert!(
        stores
            .runs
            .runs_waiting_on(org, a.workspace, personal(), None, 10)
            .await
            .unwrap()
            .is_empty(),
        "nor on an installation"
    );

    // The run waits on a memory call: never refused as an installation's.
    let parked = snapshot(stores, here).await;
    let first = parked.calls[0].id;
    assert_eq!(
        parked.control.continuation,
        Continuation::AwaitApproval { call_id: first }
    );
    assert!(
        refuse_waiting(
            org,
            a.workspace,
            &parked,
            caller(),
            WentAway::Installation(personal()),
            Gone::Removed
        )
        .is_err()
    );
    assert!(
        refuse_waiting(
            org,
            a.workspace,
            &parked,
            caller(),
            WentAway::MemoryType(MemoryTypeId::generate()),
            Gone::Removed
        )
        .is_err()
    );
    let request = refuse_waiting(
        org,
        a.workspace,
        &parked,
        caller(),
        WentAway::MemoryType(notes_type),
        Gone::Removed,
    )
    .unwrap();

    // Applied in another scope: not found, nothing changed.
    let before = state(stores, here).await;
    for (organization, workspace) in [
        (org, a.sibling),
        (b.organization_id, b.workspace),
        (b.organization_id, a.workspace),
    ] {
        assert_eq!(
            stores.runs.apply(organization, workspace, &request).await,
            Err(StoreError::NotFound)
        );
    }
    assert_eq!(state(stores, here).await, before, "nothing changed");

    // By hand: only for the type the call is in, only with the reason it
    // went.
    let Operation::RefuseWaiting(refuse) = request.operation().clone() else {
        panic!("a refusal");
    };
    for (operation, why) in [
        (
            RefuseWaiting {
                went: WentAway::MemoryType(MemoryTypeId::generate()),
                ..refuse.clone()
            },
            "a type the call isn't in",
        ),
        (
            RefuseWaiting {
                went: WentAway::Installation(personal()),
                ..refuse.clone()
            },
            "an installation the call isn't bound to",
        ),
        (
            RefuseWaiting {
                failure: Failure::new(INSTALLATION_REMOVED, "Personal was removed, so this call wasn't made."),
                ..refuse.clone()
            },
            "an installation's reason",
        ),
        (
            RefuseWaiting {
                failure: Failure::new("tool_refused", "No."),
                ..refuse.clone()
            },
            "another reason",
        ),
    ] {
        let by_hand = by_hand(stores, here, Guards::default(), Operation::RefuseWaiting(operation)).await;
        refuses_with_nothing_written(stores, here, &by_hand, why).await;
    }

    // The job's request commits: the call refused, never sent, the run
    // running again for its next turn, its job queued.
    jobs_of(stores, RUN_JOB, Some(here.run)).await;
    let receipt = committed(apply(stores, here, &request).await);
    assert_eq!(
        (receipt.status, receipt.continuation, receipt.lease_until),
        (RunStatus::Running, Continuation::NeedModel { turn: 1 }, None)
    );
    assert_eq!(kinds(&receipt), [RunEventType::ToolCallFailed], "the timeline says why");
    assert_eq!(receipt.events[0].data["call_id"], json!(first));
    assert_eq!(receipt.events[0].data["error"]["code"], MEMORY_TYPE_REMOVED);
    assert_eq!(receipt.events[0].data["error"]["message"], MEMORY_TYPE_REMOVED_MESSAGE);
    assert_eq!(receipt.events[0].data["memory_type_id"], json!(notes_type));
    assert_eq!(receipt.events[0].data["memory_type_name"], "Customer notes");
    let job = run_job(stores, here).await.expect("the run's job, queued with it");
    assert_eq!(job.scope, Scope::Workspace(org, a.workspace));
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    let refused = snapshot(stores, here).await;
    let call = refused.call(first).unwrap();
    assert_eq!(call.state, CallState::Refused);
    assert_eq!(
        call.outcome
            .as_ref()
            .map(|outcome| (outcome.content.as_str(), outcome.is_error)),
        Some((MEMORY_TYPE_REMOVED_MESSAGE, true)),
        "what the model reads back"
    );
    assert_eq!(call.approval, None, "nobody decided it");
    assert!(call.attempts.is_empty(), "never authorized to send");
    let late = refuse_waiting(
        org,
        a.workspace,
        &parked,
        caller(),
        WentAway::MemoryType(notes_type),
        Gone::Removed,
    )
    .unwrap();
    assert_eq!(
        apply(stores, here, &late).await,
        Ok(Applied::Conflict(Conflict::Revision)),
        "another job's, from the same revision: one winner"
    );
    assert_eq!(
        stores
            .runs
            .runs_waiting_in_type(org, notes_type, None, 10)
            .await
            .unwrap(),
        [(a.sibling, sibling.run)],
        "nothing waits in it here any more"
    );

    for at in [here, sibling, theirs] {
        let stop = cancel(
            at.org,
            at.ws,
            &snapshot(stores, at).await,
            caller(),
            RunTrigger::Api,
            person(),
        )
        .unwrap();
        committed(apply(stores, at, &stop).await);
    }
    assert!(
        stores
            .runs
            .runs_waiting_in_type(org, notes_type, None, 10)
            .await
            .unwrap()
            .is_empty(),
        "a finished run waits on nothing"
    );
}

fn request_of(plan: &Plan) -> Operation {
    request(plan).operation().clone()
}

/// A model that gives no answer uses one of the job's attempts each time;
/// on the last one the run fails at once with `model_unavailable`, and its
/// job goes.
async fn fails_a_run_whose_model_stays_silent(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let at = admitted(stores, a.organization_id, a.workspace, agent_a, "Silent").await;
    for expected in 1..=RUN_JOB_ATTEMPTS {
        if expected > 1 {
            assert!(run_job(stores, at).await.is_none(), "not at once");
            stores.clock.advance(Duration::from_secs(60));
        }
        let job = run_job(stores, at).await.expect("tried again later");
        assert_eq!(job.attempt, expected, "each silence uses an attempt");
        let who = claimant();
        let epoch = claim(stores, at, who).await;
        prepare(stores, at, who, epoch).await;
        let snapshot = snapshot(stores, at).await;
        let Continuation::AwaitModel { turn, attempt } = snapshot.control.continuation else {
            panic!("the run waits on its model attempt");
        };
        let mut silent = facts(stores, who, Some(epoch));
        silent.last_attempt = job.attempt == RUN_JOB_ATTEMPTS;
        silent.model = Some(ModelAnswer {
            turn,
            attempt,
            result: ModelResult::Unavailable(Unanswered::because(UnansweredWhy::NoAnswer)),
        });
        let (_, receipt) = step(stores, at, &silent).await;
        if expected < RUN_JOB_ATTEMPTS {
            assert_eq!(receipt.continuation, Continuation::NeedModel { turn: 0 });
            continue;
        }
        assert_eq!(kinds(&receipt), [RunEventType::RunFailed]);
        assert_eq!(receipt.events[0].data["error"]["code"], json!("model_unavailable"));
        assert_eq!(
            stores.jobs.complete(&job.scope, job.id, &job.lease).await,
            Err(JobError::LeaseLost),
            "its job goes with it"
        );
    }
    let run = stores.runs.find_run(at.org, at.ws, at.run).await.unwrap().unwrap();
    assert_eq!(
        (run.status, run.error.map(|error| error.code)),
        (RunStatus::Failed, Some("model_unavailable".to_owned()))
    );
    assert!(
        !stores
            .runs
            .abandoned_runs(100)
            .await
            .unwrap()
            .contains(&(at.org, at.ws, at.run)),
        "nothing left for the sweep"
    );
}

/// A provider's `Retry-After` sets when the job is tried again, in place
/// of the usual step; an attempt that timed out is recorded as one, and the
/// turn's next timing out too fails the run `model_failed`.
async fn waits_as_the_provider_says_and_stops_at_a_second_timeout(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let at = admitted(stores, a.organization_id, a.workspace, agent_a, "Slow").await;
    let unanswered = |turn, attempt, why, retry_after| ModelAnswer {
        turn,
        attempt,
        result: ModelResult::Unavailable(Unanswered { why, retry_after }),
    };
    let mut answers = [
        unanswered(0, 1, UnansweredWhy::TimedOut, Some(Duration::from_secs(40))),
        unanswered(0, 2, UnansweredWhy::TimedOut, None),
    ]
    .into_iter();
    for expected in 1..=2 {
        if expected > 1 {
            stores.clock.advance(Duration::from_secs(39));
            assert!(run_job(stores, at).await.is_none(), "not before the provider's wait");
            stores.clock.advance(Duration::from_secs(1));
        }
        let job = run_job(stores, at).await.expect("tried again");
        let who = claimant();
        let epoch = claim(stores, at, who).await;
        prepare(stores, at, who, epoch).await;
        let mut slow = facts(stores, who, Some(epoch));
        slow.model = answers.next();
        let (_, receipt) = step(stores, at, &slow).await;
        if expected == 1 {
            assert_eq!(receipt.continuation, Continuation::NeedModel { turn: 0 });
            let attempts = snapshot(stores, at).await.model_attempts;
            assert_eq!(
                attempts
                    .iter()
                    .map(|attempt| (attempt.state, attempt.timed_out))
                    .collect::<Vec<_>>(),
                [(ModelAttemptState::Unanswered, true)]
            );
            continue;
        }
        assert_eq!(kinds(&receipt), [RunEventType::RunFailed]);
        assert_eq!(receipt.events[0].data["error"]["code"], json!("model_failed"));
        assert_eq!(
            stores.jobs.complete(&job.scope, job.id, &job.lease).await,
            Err(JobError::LeaseLost),
            "its job goes with it"
        );
    }
}

/// A run with a call sent, wherever it lives: `(at, send)`.
async fn sending(
    stores: &Stores<'_>,
    org: OrganizationId,
    ws: WorkspaceId,
    agent_id: AgentId,
    input: &str,
) -> (At, SendRequest) {
    let at = admitted(stores, org, ws, agent_id, input).await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(stores, at, who, epoch, calls(&[("m1", LIST)]), |_| allow()).await;
    let send = authorize(stores, at, who, epoch, allow()).await;
    (at, send)
}

/// A request on a run of the caller's own scope that names a call, or a
/// send claim, of a run in the sibling workspace or another organization
/// reaches nothing: neither run changes.
async fn refuses_another_runs_calls(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    let sibling = sending(stores, org, a.sibling, agent_sibling, "Sibling's").await;
    let theirs = sending(stores, b.organization_id, b.workspace, agent_b, "Theirs").await;

    // Our runs: one waiting for a decision, one at a call, one sending.
    let waiting = admitted(stores, org, a.workspace, agent_a, "Waiting").await;
    let who = claimant();
    let epoch = claim(stores, waiting, who).await;
    prepare(stores, waiting, who, epoch).await;
    answer(stores, waiting, who, epoch, calls(&[("m1", CREATE)]), |_| {
        needs_approval()
    })
    .await;
    step(stores, waiting, &facts(stores, who, Some(epoch))).await;
    let (at_call, call_who, call_epoch) = at_a_call(stores, a, agent_a, "At a call", &[("m1", LIST)]).await;
    let (ours, our_send) = sending(stores, org, a.workspace, agent_a, "Ours").await;

    for (foreign, foreign_send, whose) in [
        (sibling.0, &sibling.1, "the sibling workspace's"),
        (theirs.0, &theirs.1, "another organization's"),
    ] {
        let foreign_call = snapshot(stores, foreign).await.calls[0].clone();
        let foreign_before = state(stores, foreign).await;

        // A decision on their call, on our waiting run.
        let parked = snapshot(stores, waiting).await;
        let (key, name) = api_key();
        let decide = resolve_approval(
            org,
            a.workspace,
            &parked,
            caller(),
            Approval {
                call_id: parked.calls[0].id,
                decision: ApprovalDecision::Approved,
                api_key_id: key,
                api_key_name: name,
                reason: None,
            },
        )
        .unwrap();
        let Operation::ResolveApproval(mut decision) = decide.operation().clone() else {
            panic!("a decision")
        };
        decision.call_id = foreign_call.id;
        decision.call_digest = foreign_call.call_digest;
        decision.next = Continuation::NeedCall {
            turn: foreign_call.turn,
            position: foreign_call.position,
        };
        let decide = by_hand(stores, waiting, Guards::default(), Operation::ResolveApproval(decision)).await;
        refuses_with_nothing_written(stores, waiting, &decide, &format!("a decision on {whose} call")).await;

        // An authorization of their call, on our run at a call.
        let plan = request(&at_send(stores, at_call, call_who, call_epoch, allow()).await);
        let Operation::AuthorizeCall(mut authorize) = plan.operation().clone() else {
            panic!("an authorization")
        };
        authorize.call_id = foreign_call.id;
        if let Authorization::Send { attempt, .. } = authorize.authorization {
            authorize.next = Continuation::AwaitCall {
                call_id: foreign_call.id,
                attempt,
            };
        }
        let authorize = by_hand(stores, at_call, *plan.guards(), Operation::AuthorizeCall(authorize)).await;
        refuses_with_nothing_written(stores, at_call, &authorize, &format!("a send of {whose} call")).await;

        // Their call's outcome, under their send claim, on our sending run.
        let theirs_claim = foreign_call.attempt(1).unwrap().clone();
        let mut record = outcome_request(stores, ours, &our_send, completed("ours")).await;
        let Operation::RecordOutcome(ours_op) = record.operation().clone() else {
            panic!("an outcome")
        };
        record = by_hand(
            stores,
            ours,
            Guards {
                send: Some(SendGuard {
                    call_id: foreign_call.id,
                    attempt: 1,
                    dispatch_epoch: theirs_claim.dispatch_epoch,
                    invocation_id: theirs_claim.invocation_id,
                    lease: SendLease::Any,
                }),
                ..Guards::default()
            },
            Operation::RecordOutcome(RecordOutcome {
                call_id: foreign_call.id,
                invocation_id: theirs_claim.invocation_id,
                ..ours_op.clone()
            }),
        )
        .await;
        refuses_without_change(
            stores,
            ours,
            &record,
            Ok(Applied::Conflict(Conflict::SendClaim)),
            &format!("an outcome for {whose} call"),
        )
        .await;
        // Our call's outcome, under their invocation.
        let borrowed = TransitionRequest::new(
            ours.org,
            ours.ws,
            ours.run,
            snapshot(stores, ours).await.control.revision,
            Guards {
                send: Some(SendGuard {
                    call_id: our_send.call_id,
                    attempt: our_send.attempt,
                    dispatch_epoch: snapshot(stores, ours).await.calls[0].attempts[0].dispatch_epoch,
                    invocation_id: foreign_send.invocation_id,
                    lease: SendLease::Any,
                }),
                ..Guards::default()
            },
            Operation::RecordOutcome(RecordOutcome {
                invocation_id: foreign_send.invocation_id,
                ..ours_op
            }),
        );
        refuses_without_change(
            stores,
            ours,
            &borrowed,
            Ok(Applied::Conflict(Conflict::SendClaim)),
            &format!("our call's outcome under {whose} send claim"),
        )
        .await;
        assert_eq!(state(stores, foreign).await, foreign_before, "{whose} run is untouched");
    }
    // Our own requests still commit.
    report(stores, ours, &our_send, completed("ours")).await;
    for at in [waiting, at_call, ours, sibling.0, theirs.0] {
        let stop = cancel(
            at.org,
            at.ws,
            &snapshot(stores, at).await,
            caller(),
            RunTrigger::Api,
            person(),
        )
        .unwrap();
        committed(apply(stores, at, &stop).await);
    }
}

/// A run at a call to send, its worker holding it: `(at, who, epoch)`.
async fn at_a_call(
    stores: &Stores<'_>,
    a: Tenant,
    agent_a: AgentId,
    input: &str,
    asked: &[(&str, &str)],
) -> (At, ClaimantId, u64) {
    let at = admitted(stores, a.organization_id, a.workspace, agent_a, input).await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(stores, at, who, epoch, calls(asked), |_| allow()).await;
    (at, who, epoch)
}

/// A step for a call that isn't where it says is refused: no park of a call
/// that doesn't wait for a person, no send of one that does.
async fn refuses_steps_that_dont_fit_the_call(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Allowed", &[("m1", LIST)]).await;
    let call = snapshot(stores, at).await.calls[0].id;
    let park = by_hand(
        stores,
        at,
        owner(epoch, who, false),
        Operation::Park(zupflow_core::runs::transition::Park {
            call_id: call,
            decision: None,
        }),
    )
    .await;
    refuses_with_nothing_written(stores, at, &park, "a park of a call allowed to go").await;
    let stop = cancel(
        at.org,
        at.ws,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);

    let at = admitted(stores, a.organization_id, a.workspace, agent_a, "Awaiting").await;
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(stores, at, who, epoch, calls(&[("m1", CREATE)]), |_| needs_approval()).await;
    let call = snapshot(stores, at).await.calls[0].id;
    let send = by_hand(
        stores,
        at,
        owner(epoch, who, true),
        Operation::AuthorizeCall(AuthorizeCall {
            call_id: call,
            decision: allow(),
            authorization: Authorization::Send {
                attempt: 1,
                dispatch_epoch: epoch,
                invocation_id: invocation(),
                send_lease_ms: 1000,
            },
            next: Continuation::AwaitCall {
                call_id: call,
                attempt: 1,
            },
        }),
    )
    .await;
    refuses_with_nothing_written(stores, at, &send, "a send of a call waiting for a person").await;
    let stop = cancel(
        at.org,
        at.ws,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
}

/// Cancel commits first: the authorization planned before it can't commit,
/// no call is sent, the calls not yet sent are closed, and nothing claims
/// the run again.
async fn cancel_beats_an_authorization_not_yet_committed(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Cancel first", &[("m1", LIST), ("m2", CREATE)]).await;
    let job = run_job(stores, at).await.expect("its job");
    let authorize = request(&at_send(stores, at, who, epoch, allow()).await);
    let running = snapshot(stores, at).await;
    let actor = person();
    let stop = cancel(org, a.workspace, &running, caller(), RunTrigger::Console, actor.clone()).unwrap();
    let cancelled = committed(apply(stores, at, &stop).await);
    assert_eq!(
        kinds(&cancelled),
        [RunEventType::RunCancelled],
        "no event for the closed calls"
    );
    assert_eq!(cancelled.events[0].data, json!({ "cancelled_by": "Support desk" }));
    assert_eq!((cancelled.status, cancelled.lease_until), (RunStatus::Cancelled, None));
    refuses_without_change(
        stores,
        at,
        &authorize,
        Ok(Applied::Conflict(Conflict::Revision)),
        "an authorization after the cancel",
    )
    .await;
    let closed = snapshot(stores, at).await;
    assert!(closed.control.cancel_requested_at.is_some());
    for call in &closed.calls {
        assert_eq!(call.state, CallState::Refused, "closed, never sent");
        assert!(call.attempts.is_empty());
        assert_eq!(
            call.failure.as_ref().map(|failure| failure.code.as_str()),
            Some("run_cancelled")
        );
    }
    assert!(
        plan_idle(stores, at, &facts(stores, who, Some(epoch))).await,
        "replanned: nothing to send"
    );
    // A claim at the cancelled revision is refused by the cancel flag.
    let reclaim = by_hand(
        stores,
        at,
        Guards {
            not_cancelled: true,
            no_live_claim: true,
            ..Guards::default()
        },
        Operation::Claim(Claim {
            epoch: epoch + 1,
            claimant: claimant(),
            lease_ms: 1000,
            starts: false,
        }),
    )
    .await;
    refuses_without_change(
        stores,
        at,
        &reclaim,
        Ok(Applied::Conflict(Conflict::Cancelled)),
        "no claim after a cancel",
    )
    .await;
    assert_eq!(
        stores.jobs.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "the cancelled run's job is released"
    );
    let run = stores.runs.find_run(org, a.workspace, at.run).await.unwrap().unwrap();
    assert_eq!(run.finished_at, Some(cancelled.committed_at));
    assert_eq!(run.usage.tool_calls, 0, "nothing sent");
    let audit = stores
        .audit
        .list_audit_events(
            org,
            &AuditFilter {
                workspace_id: Some(a.workspace),
            },
            None,
            100,
        )
        .await
        .unwrap();
    let row = audit
        .items
        .iter()
        .find(|event| event.target.object == AuditObject::Run(at.run))
        .expect("the cancel's audit row");
    assert_eq!((row.action, &row.actor), (AuditAction::RunCancelled, &actor));
    // Cancelling again: the run is finished.
    assert!(cancel(org, a.workspace, &closed, caller(), RunTrigger::Api, person()).is_err());
}

/// Authorization commits first: the cancel still commits, the call stays
/// sent, its outcome is recorded when it returns, and nothing after it runs.
/// A sender that never answers: once its send lease lapses, the call is
/// settled unknown, and the job that waited for it goes.
async fn records_a_send_authorized_before_a_cancel(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Authorized first", &[("m1", LIST), ("m2", CREATE)]).await;
    let send = authorize(stores, at, who, epoch, allow()).await;
    let job = run_job(stores, at).await;
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
    let cancelled = snapshot(stores, at).await;
    assert_eq!(cancelled.calls[0].state, CallState::Started, "a sent call stays sent");
    assert_eq!(cancelled.calls[1].state, CallState::Refused, "the next one is closed");
    let recorded = report(stores, at, &send, completed("Made it")).await;
    assert_eq!(kinds(&recorded), [RunEventType::ToolCallCompleted]);
    assert_eq!(
        (recorded.status, recorded.continuation),
        (RunStatus::Cancelled, Continuation::Done),
        "the run stays cancelled"
    );
    assert_eq!(snapshot(stores, at).await.calls[0].state, CallState::Completed);
    drop(job);

    // A sender that never answers.
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Never answers", &[("m1", LIST)]).await;
    let send = authorize(stores, at, who, epoch, allow()).await;
    if let Some(job) = run_job(stores, at).await {
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    }
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
    assert!(
        run_job(stores, at).await.is_none(),
        "the job waits until the send lease lapses"
    );
    let early = settle_unknown_by_hand(stores, at, &send).await;
    refuses_without_change(
        stores,
        at,
        &early,
        Ok(Applied::Conflict(Conflict::SendClaim)),
        "never unknown while its sender may still answer",
    )
    .await;
    stores.clock.advance(SEND_LEASE);
    let job = run_job(stores, at).await.expect("due once the send lease lapsed");
    let (_, settled) = step(stores, at, &Facts::at(stores.clock.now())).await;
    assert_eq!(kinds(&settled), [RunEventType::ToolCallFailed]);
    assert_eq!(settled.events[0].data["error"]["code"], json!("outcome_unknown"));
    assert_eq!(settled.status, RunStatus::Cancelled);
    assert_eq!(snapshot(stores, at).await.calls[0].state, CallState::Unknown);
    assert_eq!(
        stores.jobs.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "and then it goes"
    );
}

async fn settle_unknown_by_hand(stores: &Stores<'_>, at: At, send: &SendRequest) -> TransitionRequest {
    let call = snapshot(stores, at).await;
    let attempt = call.call(send.call_id).unwrap().attempt(send.attempt).unwrap().clone();
    by_hand(
        stores,
        at,
        Guards {
            send: Some(SendGuard {
                call_id: send.call_id,
                attempt: send.attempt,
                dispatch_epoch: attempt.dispatch_epoch,
                invocation_id: attempt.invocation_id,
                lease: SendLease::Lapsed,
            }),
            ..Guards::default()
        },
        Operation::SettleUnknown(SettleUnknown {
            call_id: send.call_id,
            attempt: send.attempt,
            outcome: zupflow_core::runs::CallOutcome {
                content: "Unknown.".to_owned(),
                is_error: true,
            },
            failure: Failure::new("outcome_unknown", "Unknown."),
            next: None,
        }),
    )
    .await
}

/// A call is authorized once, and its sender's outcome recorded once, under
/// its own send claim.
async fn holds_a_send_claim_once(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Once", &[("m1", LIST), ("m2", LIST)]).await;
    let plan_one = request(&at_send(stores, at, who, epoch, allow()).await);
    let plan_two = request(&at_send(stores, at, who, epoch, allow()).await);
    assert_ne!(plan_one.digest(), plan_two.digest(), "two invocations of one send");
    committed(apply(stores, at, &plan_one).await);
    refuses_without_change(
        stores,
        at,
        &plan_two,
        Ok(Applied::Conflict(Conflict::Revision)),
        "a second authorization from the same revision",
    )
    .await;
    let sending = snapshot(stores, at).await;
    let call = &sending.calls[0];
    // Authorizing it again, by hand, at the current revision.
    let again = by_hand(
        stores,
        at,
        owner(epoch, who, true),
        Operation::AuthorizeCall(AuthorizeCall {
            call_id: call.id,
            decision: allow(),
            authorization: Authorization::Send {
                attempt: 2,
                dispatch_epoch: epoch,
                invocation_id: invocation(),
                send_lease_ms: 1000,
            },
            next: Continuation::AwaitCall {
                call_id: call.id,
                attempt: 2,
            },
        }),
    )
    .await;
    let before = state(stores, at).await;
    assert!(
        matches!(apply(stores, at, &again).await, Err(StoreError::Refused(_))),
        "a call being sent isn't authorized again"
    );
    assert_eq!(state(stores, at).await, before);
    let claim = call.attempt(1).unwrap().clone();
    let send = SendRequest {
        organization_id: org,
        workspace_id: a.workspace,
        call_id: call.id,
        attempt: 1,
        invocation_id: claim.invocation_id,
        installation_id: work(),
        tool: "list_tasks".to_owned(),
        arguments: Map::new(),
    };
    // Evidence for a call still being sent, or an unknown outcome that
    // leaves the run where it is: neither fits.
    let fenced = |lease| Guards {
        send: Some(SendGuard {
            call_id: call.id,
            attempt: 1,
            dispatch_epoch: epoch,
            invocation_id: claim.invocation_id,
            lease,
        }),
        ..Guards::default()
    };
    let evidence = by_hand(
        stores,
        at,
        fenced(SendLease::Any),
        Operation::RecordOutcome(RecordOutcome {
            call_id: call.id,
            attempt: 1,
            invocation_id: claim.invocation_id,
            outcome: completed("early"),
            settle: None,
            next: None,
        }),
    )
    .await;
    refuses_with_nothing_written(stores, at, &evidence, "late evidence for a call still being sent").await;
    let wedged = by_hand(
        stores,
        at,
        fenced(SendLease::Any),
        Operation::RecordOutcome(RecordOutcome {
            call_id: call.id,
            attempt: 1,
            invocation_id: claim.invocation_id,
            outcome: completed("done"),
            settle: Some(Settlement {
                state: CallState::Completed,
                outcome: zupflow_core::runs::CallOutcome {
                    content: "{}".to_owned(),
                    is_error: false,
                },
                failure: None,
            }),
            next: None,
        }),
    )
    .await;
    refuses_with_nothing_written(
        stores,
        at,
        &wedged,
        "an outcome that settles the call its run waits on but leaves the run waiting",
    )
    .await;
    stores.clock.advance(SEND_LEASE);
    let stuck = by_hand(
        stores,
        at,
        Guards {
            owner: owner(epoch, who, true).owner,
            not_cancelled: true,
            ..fenced(SendLease::Lapsed)
        },
        Operation::SettleUnknown(SettleUnknown {
            call_id: call.id,
            attempt: 1,
            outcome: zupflow_core::runs::CallOutcome {
                content: "Unknown.".to_owned(),
                is_error: true,
            },
            failure: Failure::new("outcome_unknown", "Unknown."),
            next: None,
        }),
    )
    .await;
    refuses_with_nothing_written(
        stores,
        at,
        &stuck,
        "an unknown outcome that leaves a running run waiting",
    )
    .await;
    // Another invocation's outcome reaches nothing.
    let mut impostor = outcome_request(stores, at, &send, completed("x")).await;
    let Operation::RecordOutcome(record) = impostor.operation().clone() else {
        panic!("an outcome")
    };
    let forged = invocation();
    impostor = by_hand(
        stores,
        at,
        Guards {
            send: Some(SendGuard {
                call_id: call.id,
                attempt: 1,
                dispatch_epoch: epoch,
                invocation_id: forged,
                lease: SendLease::Any,
            }),
            ..Guards::default()
        },
        Operation::RecordOutcome(RecordOutcome {
            invocation_id: forged,
            ..record
        }),
    )
    .await;
    refuses_without_change(
        stores,
        at,
        &impostor,
        Ok(Applied::Conflict(Conflict::SendClaim)),
        "an outcome under another send claim",
    )
    .await;
    let first = outcome_request(stores, at, &send, completed("first")).await;
    let second = outcome_request(stores, at, &send, completed("second")).await;
    committed(apply(stores, at, &first).await);
    refuses_without_change(
        stores,
        at,
        &second,
        Ok(Applied::Conflict(Conflict::Revision)),
        "a second outcome planned from the same revision",
    )
    .await;
    // Recorded once: an outcome built at the new revision finds the claim spent.
    let Operation::RecordOutcome(record) = second.operation().clone() else {
        panic!("an outcome")
    };
    let spent = by_hand(stores, at, *second.guards(), Operation::RecordOutcome(record)).await;
    refuses_without_change(
        stores,
        at,
        &spent,
        Ok(Applied::Conflict(Conflict::SendClaim)),
        "an outcome recorded once",
    )
    .await;
    let recorded = snapshot(stores, at).await;
    assert_eq!(recorded.calls[0].state, CallState::Completed);
    assert!(recorded.calls[0].attempt(1).unwrap().outcome_recorded);
    let stop = cancel(org, a.workspace, &recorded, caller(), RunTrigger::Api, person()).unwrap();
    committed(apply(stores, at, &stop).await);
}

/// A worker stops after authorizing a send. Once the send lease lapses on
/// the store's clock, a new worker settles the call unknown and the run goes
/// on; the slow sender's outcome is then kept as evidence only, and the call
/// stays unknown.
async fn keeps_a_late_outcome_as_evidence(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Late", &[("m1", LIST)]).await;
    let send = authorize(stores, at, who, epoch, allow()).await;
    // The worker stops. Its lease and the send lease run out.
    let rival = claimant();
    stores.clock.advance(SEND_LEASE);
    match plan(stores, at, &facts(stores, rival, None)).await {
        Plan::Idle(_) => {}
        other => panic!("a live claim waits, got {other:?}"),
    }
    stores.clock.advance(LEASE);
    let epoch = claim(stores, at, rival).await;
    let (_, settled) = step(stores, at, &facts(stores, rival, Some(epoch))).await;
    assert_eq!(kinds(&settled), [RunEventType::ToolCallFailed]);
    assert_eq!(settled.events[0].data["error"]["code"], json!("outcome_unknown"));
    assert_eq!(
        settled.continuation,
        Continuation::NeedModel { turn: 1 },
        "the run goes on"
    );
    let events_before = stores
        .runs
        .list_events(org, a.workspace, at.run, 0)
        .await
        .unwrap()
        .len();
    // The slow sender's answer arrives.
    let late = report(stores, at, &send, completed("Arrived late")).await;
    assert!(late.events.is_empty(), "nothing on the timeline");
    let kept = snapshot(stores, at).await;
    assert_eq!(
        kept.calls[0].state,
        CallState::Unknown,
        "unknown to the model and the API"
    );
    assert!(
        kept.calls[0].attempt(1).unwrap().outcome_recorded,
        "kept on the attempt"
    );
    assert_eq!(kept.control.continuation, Continuation::NeedModel { turn: 1 });
    assert_eq!(
        stores
            .runs
            .list_events(org, a.workspace, at.run, 0)
            .await
            .unwrap()
            .len(),
        events_before
    );
    let stop = cancel(org, a.workspace, &kept, caller(), RunTrigger::Api, person()).unwrap();
    committed(apply(stores, at, &stop).await);
}

/// A worker stops between preparing a model attempt and committing its
/// answer: the next one marks it abandoned and asks again under a new
/// attempt; the first one's answer can no longer commit.
async fn abandons_a_model_attempt_a_stopped_worker_left(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Abandoned answer").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    let late = answer_request(stores, at, who, epoch, Reply::Text("Too late.".to_owned()), |_| allow()).await;
    stores.clock.advance(LEASE);
    let rival = claimant();
    let epoch = claim(stores, at, rival).await;
    let (plan, again) = step(stores, at, &facts(stores, rival, Some(epoch))).await;
    assert!(matches!(
        request(&plan).operation(),
        Operation::PrepareModel(PrepareModel {
            attempt: 2,
            abandons: Some(1),
            ..
        })
    ));
    assert_eq!(again.continuation, Continuation::AwaitModel { turn: 0, attempt: 2 });
    let attempts: Vec<(u32, ModelAttemptState)> = snapshot(stores, at)
        .await
        .model_attempts
        .iter()
        .map(|attempt| (attempt.attempt, attempt.state))
        .collect();
    assert_eq!(
        attempts,
        [(1, ModelAttemptState::Abandoned), (2, ModelAttemptState::Prepared)],
        "the lost answer stays visible"
    );
    refuses_without_change(
        stores,
        at,
        &late,
        Ok(Applied::Conflict(Conflict::Revision)),
        "the first worker's answer",
    )
    .await;
    // A cancel that leaves the prepared attempt as if it could still answer.
    let stop = cancel(
        at.org,
        at.ws,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    let Operation::Cancel(mut forgetful) = stop.operation().clone() else {
        panic!("a cancel")
    };
    assert!(forgetful.abandons.is_some(), "the reducer abandons it");
    forgetful.abandons = None;
    let forgetful = by_hand(stores, at, Guards::default(), Operation::Cancel(forgetful)).await;
    refuses_with_nothing_written(
        stores,
        at,
        &forgetful,
        "a cancel that doesn't abandon the prepared attempt",
    )
    .await;
    answer(stores, at, rival, epoch, Reply::Text("Done.".to_owned()), |_| allow()).await;
    assert_eq!(snapshot(stores, at).await.turns.len(), 1, "one turn recorded");
}

/// A yield lets the claim go at a safe boundary and postpones the run's job
/// without using one of its attempts; a model that gives no answer uses one.
async fn yields_without_using_an_attempt(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Yield").await;
    let job = run_job(stores, at).await.expect("its job");
    assert_eq!(job.attempt, 1);
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    let mut tired = facts(stores, who, Some(epoch));
    tired.steps = tired.limits.max_steps_per_claim;
    let (plan, yielded) = step(stores, at, &tired).await;
    assert!(matches!(
        request(&plan).operation(),
        Operation::Yield(Yield {
            why: YieldReason::StepBudget
        })
    ));
    assert_eq!(yielded.lease_until, None, "the claim is let go");
    assert_eq!(
        stores.jobs.complete(&job.scope, job.id, &job.lease).await,
        Err(JobError::LeaseLost),
        "the store postponed the job"
    );
    let job = run_job(stores, at).await.expect("due again at once");
    assert_eq!(job.attempt, 1, "without using an attempt");

    // A send in flight: resume when its send lease lapses.
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    answer(stores, at, who, epoch, calls(&[("m1", LIST)]), |_| allow()).await;
    let send = authorize(stores, at, who, epoch, allow()).await;
    let (plan, _) = step(stores, at, &facts(stores, who, Some(epoch))).await;
    assert!(matches!(
        request(&plan).operation(),
        Operation::Yield(Yield {
            why: YieldReason::SendInFlight { .. }
        })
    ));
    assert!(run_job(stores, at).await.is_none(), "not before the send lease lapses");
    report(stores, at, &send, completed("Done")).await;
    let job = run_job(stores, at).await.expect("due at once, its outcome recorded");
    assert_eq!(job.attempt, 1, "still without using an attempt");

    // The model gives no answer: back to its turn, the job tried again later.
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    let mut silent = facts(stores, who, Some(epoch));
    silent.model = Some(ModelAnswer {
        turn: 1,
        attempt: 1,
        result: ModelResult::Unavailable(Unanswered::because(UnansweredWhy::NoAnswer)),
    });
    let (_, retried) = step(stores, at, &silent).await;
    assert_eq!(retried.continuation, Continuation::NeedModel { turn: 1 });
    assert_eq!(
        snapshot(stores, at)
            .await
            .model_attempt(1, 1)
            .map(|attempt| attempt.state),
        Some(ModelAttemptState::Unanswered)
    );
    assert!(run_job(stores, at).await.is_none(), "not at once");
    stores.clock.advance(Duration::from_secs(60));
    let job = run_job(stores, at).await.expect("tried again later");
    assert_eq!(job.attempt, 2, "using an attempt");
    stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
}

/// A request that can't be applied as a whole changes nothing: not the run,
/// its turns, calls or attempts, its timeline, nor its job.
async fn writes_all_or_nothing(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let at = admitted(stores, org, a.workspace, agent_a, "Whole").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    prepare(stores, at, who, epoch).await;
    // Two calls answering one model call ID: the second can't be stored.
    let commit = answer_request(stores, at, who, epoch, calls(&[("m1", LIST), ("m2", CREATE)]), |_| {
        allow()
    })
    .await;
    let Operation::CommitTurn(mut turn) = commit.operation().clone() else {
        panic!("a turn")
    };
    turn.calls[1].model_call_id = turn.calls[0].model_call_id.clone();
    let broken = by_hand(stores, at, *commit.guards(), Operation::CommitTurn(turn)).await;
    let job_before = jobs_due(stores).await;
    refuses_with_nothing_written(stores, at, &broken, "a turn whose calls can't all be stored").await;
    assert_eq!(jobs_due(stores).await, job_before, "no job either");
    // An answer that moves the run past its open calls.
    let Operation::CommitTurn(mut skipping) = commit.operation().clone() else {
        panic!("a turn")
    };
    skipping.next = Continuation::NeedModel { turn: 1 };
    let skipping = by_hand(stores, at, *commit.guards(), Operation::CommitTurn(skipping)).await;
    refuses_with_nothing_written(stores, at, &skipping, "a turn that skips its calls").await;
    // Calls that aren't what the attempt's frozen offer makes of them.
    let Operation::CommitTurn(answered) = commit.operation().clone() else {
        panic!("a turn")
    };
    let forge = |change: &dyn Fn(&mut NewCall)| {
        let mut forged = answered.clone();
        change(&mut forged.calls[0]);
        Operation::CommitTurn(forged)
    };
    let redigest = |call: &mut NewCall| {
        let target = call.target.clone().unwrap();
        call.call_digest = Some(call_digest(
            target.installation_id.expect("an app tool"),
            &target.app_key,
            &target.tool,
            &target.definition_sha256,
            &call.arguments,
        ));
    };
    for (operation, why) in [
        (
            forge(&|call: &mut NewCall| call.target.as_mut().unwrap().installation_id = Some(personal())),
            "a call sent to another installation than its name means",
        ),
        (
            forge(&|call: &mut NewCall| {
                call.target.as_mut().unwrap().installation_id = Some(personal());
                redigest(call);
            }),
            "a call sent elsewhere, digested to match",
        ),
        (
            forge(&|call: &mut NewCall| {
                call.target.as_mut().unwrap().definition_sha256 = Sha256Digest::of(b"another definition");
                redigest(call);
            }),
            "a call decided against another definition",
        ),
        (
            forge(&|call: &mut NewCall| call.call_digest = Some(Sha256Digest::of(b"anything"))),
            "a call with a digest of nothing it records",
        ),
        (
            forge(&|call: &mut NewCall| {
                call.target = None;
                call.call_digest = None;
            }),
            "an offered tool recorded without its target",
        ),
    ] {
        let forged = by_hand(stores, at, *commit.guards(), operation).await;
        refuses_with_nothing_written(stores, at, &forged, why).await;
    }
    // The same answer, whole, commits.
    committed(apply(stores, at, &commit).await);
    // A send claimed under another epoch than the owner's.
    let send = request(&at_send(stores, at, who, epoch, allow()).await);
    let Operation::AuthorizeCall(mut foreign) = send.operation().clone() else {
        panic!("an authorization")
    };
    if let Authorization::Send { dispatch_epoch, .. } = &mut foreign.authorization {
        *dispatch_epoch += 1;
    }
    let foreign = by_hand(stores, at, *send.guards(), Operation::AuthorizeCall(foreign)).await;
    refuses_with_nothing_written(stores, at, &foreign, "a send under another epoch").await;
    let recorded = snapshot(stores, at).await;
    assert_eq!(recorded.calls.len(), 2);
    // A finish that leaves a call open, or closes one that isn't: refused.
    let finish = |closing: Option<Closing>| {
        Operation::Finish(Finish {
            ending: RunEnding::Failed {
                error: Failure::new("model_failed", "No."),
            },
            closing,
            abandons: None,
        })
    };
    let partial = by_hand(stores, at, owner(epoch, who, false), finish(None)).await;
    refuses_with_nothing_written(stores, at, &partial, "a finish that leaves calls open").await;
    // An answer for a model attempt the run doesn't wait on.
    let wrong = by_hand(
        stores,
        at,
        owner(epoch, who, true),
        Operation::CommitTurn(CommitTurn {
            turn: 0,
            attempt: 1,
            text: None,
            usage: Default::default(),
            calls: Vec::<NewCall>::new(),
            next: Continuation::NeedModel { turn: 1 },
            ending: None,
            silent: None,
        }),
    )
    .await;
    refuses_with_nothing_written(stores, at, &wrong, "a turn committed twice").await;
    let stop = cancel(org, a.workspace, &recorded, caller(), RunTrigger::Api, person()).unwrap();
    committed(apply(stores, at, &stop).await);
}

async fn refuses_with_nothing_written(stores: &Stores<'_>, at: At, request: &TransitionRequest, why: &str) {
    let before = state(stores, at).await;
    let answered = apply(stores, at, request).await;
    assert!(matches!(answered, Err(StoreError::Refused(_))), "{why}: {answered:?}");
    assert_eq!(state(stores, at).await, before, "{why}: nothing changed");
}

/// Due run jobs, by run and attempt, left as they were.
async fn jobs_due(stores: &Stores<'_>) -> Vec<(Option<RunId>, u32)> {
    let kind = Name::new(RUN_JOB).unwrap();
    let mut seen = Vec::new();
    let mut held = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        seen.push((run_of_job(&job.payload), job.attempt));
        held.push(job);
    }
    for job in held {
        stores
            .jobs
            .postpone(&job.scope, job.id, &job.lease, Duration::ZERO)
            .await
            .unwrap();
    }
    seen.sort_by_key(|(run, attempt)| (run.map(|run| run.uuid()), *attempt));
    seen
}

/// The sweep finds a run whose job died, and fails it, but never while a
/// worker's lease or a call's send lease on it holds.
async fn finds_and_fails_abandoned_runs(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    let at = admitted(stores, org, ws, agent_a, "Abandoned").await;
    let abandoned = || async { stores.runs.abandoned_runs(100).await.unwrap() };
    assert!(
        !abandoned().await.contains(&(org, ws, at.run)),
        "a run whose job is queued isn't abandoned"
    );
    let job = run_job(stores, at).await.expect("its job");
    assert!(
        !abandoned().await.contains(&(org, ws, at.run)),
        "nor one whose job is held"
    );
    // Its worker holds it, then its job dies.
    claim(stores, at, claimant()).await;
    stores
        .jobs
        .bury(&job.scope, job.id, &job.lease, &Name::new("lease_expired").unwrap())
        .await
        .unwrap();
    assert!(
        abandoned().await.contains(&(org, ws, at.run)),
        "a run whose job died is, with its scope"
    );
    let sweep = |snapshot: &Snapshot| fail_abandoned(org, ws, snapshot, caller(), stores.clock.now());
    // Built as if the worker's lease were gone: the store checks it itself.
    let mut lapsed = snapshot(stores, at).await;
    lapsed.control.lease_until = None;
    let early = sweep(&lapsed).unwrap();
    refuses_without_change(
        stores,
        at,
        &early,
        Ok(Applied::Conflict(Conflict::Claimed)),
        "a run whose worker's lease holds",
    )
    .await;
    stores.clock.advance(LEASE);
    let failed = committed(apply(stores, at, &sweep(&snapshot(stores, at).await).unwrap()).await);
    assert_eq!(kinds(&failed), [RunEventType::RunFailed]);
    assert_eq!(failed.events[0].data["error"]["code"], json!("run_unavailable"));
    let run = stores.runs.find_run(org, ws, at.run).await.unwrap().unwrap();
    assert_eq!(
        (run.status, run.error.map(|error| error.code)),
        (RunStatus::Failed, Some("run_unavailable".to_owned()))
    );
    assert!(!abandoned().await.contains(&(org, ws, at.run)), "a finished run isn't");

    // A call being sent: its worker let the run go until the send lease
    // lapses, and the sweep waits for it too.
    let (at, who, epoch) = at_a_call(stores, a, agent_a, "Sending", &[("m1", LIST)]).await;
    authorize(stores, at, who, epoch, allow()).await;
    let (plan, _) = step(stores, at, &facts(stores, who, Some(epoch))).await;
    assert!(matches!(
        request(&plan).operation(),
        Operation::Yield(Yield {
            why: YieldReason::SendInFlight { .. }
        })
    ));
    let mut lapsed = snapshot(stores, at).await;
    for attempt in &mut lapsed.calls[0].attempts {
        attempt.send_lease_until = stores.clock.now();
    }
    let early = sweep(&lapsed).unwrap();
    refuses_without_change(
        stores,
        at,
        &early,
        Ok(Applied::Conflict(Conflict::Claimed)),
        "a run whose call is being sent",
    )
    .await;
    let stop = cancel(
        org,
        ws,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
}

async fn keeps_runs_of_a_deleted_agent(stores: &Stores<'_>, a: Tenant) {
    let org = a.organization_id;
    let agent_id = agent(stores, org, a.workspace, "Short-lived").await;
    let at = admitted(stores, org, a.workspace, agent_id, "Before").await;
    stores
        .agents
        .delete_agent(org, a.workspace, agent_id, &|_| by_suite_in(a.workspace))
        .await
        .unwrap();
    let run = stores.runs.find_run(org, a.workspace, at.run).await.unwrap().unwrap();
    assert_eq!(
        (run.agent_id, run.agent_name),
        (agent_id, None),
        "named \"Deleted agent\" by the API"
    );
    assert_eq!(
        stores
            .runs
            .list_events(org, a.workspace, at.run, 0)
            .await
            .unwrap()
            .len(),
        1
    );
    let stop = cancel(
        org,
        a.workspace,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
    let audit = stores
        .audit
        .list_audit_events(
            org,
            &AuditFilter {
                workspace_id: Some(a.workspace),
            },
            None,
            100,
        )
        .await
        .unwrap();
    let row = audit
        .items
        .iter()
        .find(|event| event.target.object == AuditObject::Run(at.run))
        .expect("the cancel's audit row");
    assert_eq!(row.target.name, "Agent", "its agent gone");
}

/// The outbox: a transition that appends an event a webhook names queues
/// its announcement in the same change, in the run's workspace, on the
/// store's clock; no other event does.
async fn announces_what_webhooks_name(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    drain_announcements(stores).await;
    let at = admitted(stores, org, ws, agent_a, "Announce me").await;
    assert!(
        announced(stores, at.run).await.is_empty(),
        "a queued run announces nothing"
    );
    let now = stores.clock.now();
    let stop = cancel(
        org,
        ws,
        &snapshot(stores, at).await,
        caller(),
        RunTrigger::Api,
        person(),
    )
    .unwrap();
    committed(apply(stores, at, &stop).await);
    let cancelled = announced(stores, at.run).await;
    assert_eq!(cancelled.len(), 1, "cancelling announces once");
    let (scope, announcement) = cancelled[0];
    assert_eq!(scope, Scope::Workspace(org, ws), "in the run's workspace");
    assert_eq!(
        (announcement.kind, announcement.subject, announcement.created_at),
        (WebhookEventType::RunCancelled, Subject::Run(at.run), now)
    );
    // Replayed, the cancel announces nothing more.
    committed(apply(stores, at, &stop).await);
    assert!(announced(stores, at.run).await.is_empty(), "a replay writes nothing");

    let at = admitted(stores, org, ws, agent_a, "Fail me").await;
    let who = claimant();
    let epoch = claim(stores, at, who).await;
    assert!(announced(stores, at.run).await.is_empty(), "starting announces nothing");
    prepare(stores, at, who, epoch).await;
    let mut failing = facts(stores, who, Some(epoch));
    failing.model = Some(ModelAnswer {
        turn: 0,
        attempt: 1,
        result: ModelResult::Failed(Failure::new("model_unavailable", "No model.")),
    });
    let (_, failed) = step(stores, at, &failing).await;
    assert_eq!(kinds(&failed), [RunEventType::RunFailed]);
    assert_eq!(
        snapshot(stores, at).await.model_attempts[0].state,
        ModelAttemptState::Abandoned,
        "an attempt whose answer never commits"
    );
    let kinds: Vec<_> = announced(stores, at.run)
        .await
        .into_iter()
        .map(|(_, announcement)| announcement.kind)
        .collect();
    assert_eq!(kinds, [WebhookEventType::RunFailed], "failing announces the run's end");
}

/// Usage counts an organization's runs in one environment's workspaces, per
/// UTC day, and nothing of another organization or environment.
async fn counts_usage_per_day(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    let resume = stores.clock.now();
    // Days long past, so no other run of the suite lands on them.
    let day = 1_000_i64;
    let at = |day: i64, hour: i64| Timestamp::from_millis(day * 86_400_000 + hour * 3_600_000);
    for hour in [0, 23] {
        stores.clock.set(at(day, hour));
        admitted(stores, org, a.workspace, agent_a, "Sandbox").await;
    }
    stores.clock.set(at(day + 2, 5));
    let failed = admitted(stores, org, a.workspace, agent_a, "Failed").await;
    stores.clock.set(at(day + 2, 6));
    let sweep = fail_abandoned(
        org,
        a.workspace,
        &snapshot(stores, failed).await,
        caller(),
        stores.clock.now(),
    )
    .unwrap();
    committed(apply(stores, failed, &sweep).await);
    stores.clock.set(at(day, 1));
    admitted(stores, org, a.sibling, agent_sibling, "Production").await;
    admitted(stores, b.organization_id, b.workspace, agent_b, "Theirs").await;
    stores.clock.set(resume);

    let usage = stores.runs.usage(org, Environment::Sandbox, at(day, 0)).await.unwrap();
    let early: Vec<_> = usage.iter().filter(|counted| counted.day <= day + 2).copied().collect();
    assert_eq!(
        early,
        vec![
            UsageDay {
                day,
                runs: 2,
                ..UsageDay::default()
            },
            UsageDay {
                day: day + 2,
                runs: 1,
                failed_runs: 1,
                ..UsageDay::default()
            },
        ],
        "runs per UTC day, in order, of the environment's workspaces only; a day without runs left out"
    );
    let later = stores
        .runs
        .usage(org, Environment::Sandbox, at(day + 1, 0))
        .await
        .unwrap();
    assert!(later.iter().all(|counted| counted.day > day), "nothing before `since`");
    let production = stores
        .runs
        .usage(org, Environment::Production, at(day, 0))
        .await
        .unwrap();
    assert_eq!(
        production.first().map(|counted| (counted.day, counted.runs)),
        Some((day, 1))
    );
    let theirs = stores
        .runs
        .usage(b.organization_id, Environment::Sandbox, at(day, 0))
        .await
        .unwrap();
    assert_eq!(
        theirs.first().map(|counted| (counted.day, counted.runs)),
        Some((day, 1))
    );
    jobs_of(stores, RUN_JOB, None).await;
    drain_announcements(stores).await;
}
