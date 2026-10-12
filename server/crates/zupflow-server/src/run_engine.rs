//! The run engine's driver (decision 0046): a worker claims a run's job,
//! reads the run as committed, gathers the facts the reducer needs, calls
//! [`advance`], applies the one transition it plans through the
//! [`RunStore`], and only then does the one piece of I/O the plan names:
//! asks the model, or sends one call through the [`ToolPort`]. What comes of
//! the I/O is the next step's fact. Nothing here decides how a run moves:
//! the reducer does, and the store holds every write to its guards.
//!
//! The facts are read fresh for every step, in the run's own organization
//! and workspace: the store's clock time, a claimant of this worker's own,
//! IDs for the calls and the send claim, the tools offered to the model
//! (each granted tool of each installation, named
//! `<app_key>_<short_id>_<tool_name>` and frozen with the model attempt,
//! decision 0047), the floor's and the [`ApprovalPolicy`]'s decision on
//! each call, and, before a call is sent, its installation, grant and
//! pinned definition read again.
//!
//! The job queue's lease and the run's claim go together: the job's lease
//! is extended before every step that keeps the claim, and once a step
//! releases the claim (it parks, yields or finishes) the store has queued,
//! postponed or released the run's job in the same transaction, so the
//! worker leaves it alone.
//!
//! Every event a transition appended is published on the [`EVENTS_TOPIC`]
//! of the workspace and of its environment, as a notification: listeners
//! read the timeline from the store.
//!
//! [`ApprovalPolicy`]: zupflow_core::policy::ApprovalPolicy

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use uuid::Uuid;
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AgentId, InstallationId, OrganizationId, RunId, ToolCallId, WorkspaceId};
use zupflow_core::installations::{Installation, InstallationStatus, InstalledApp};
use zupflow_core::jobs::{Job, JobId, LeaseToken};
use zupflow_core::models::{ProviderError, validate_completion};
use zupflow_core::policy::{CallApp, Decision, GrantedAs, OrganizationLevel, ToolCall, Verdict, floor};
use zupflow_core::runs::digest::{Sha256Digest, call_digest};
use zupflow_core::runs::names::{Candidate, Offer, OfferedTool, build_offer};
use zupflow_core::runs::reducer::{
    Facts, Idle, InstallationCheck, Io, Limits, ModelAnswer, ModelRequest, ModelResult, Plan, SendCheck, SendRequest,
    SendResult, advance, calls_to_decide, fail_abandoned,
};
use zupflow_core::runs::state::{CallDecision, CallRecord, CallVerdict, ClaimantId, InvocationId, RequestId, Snapshot};
use zupflow_core::runs::store::{Applied, RUN_JOB, RunStore, StoreError, run_of_job};
use zupflow_core::runs::transition::{Conflict, Operation, SendOutcome};
use zupflow_core::runs::{CallState, Failure, RunEvent};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::tools::{RegisteredTool, ToolError};
use zupflow_mcp::CallError;

use crate::auth::AppState;
use crate::mcp_access::{self, Unreachable};

/// The job kind: [`RUN_JOB`].
pub const RUN_AGENT: &str = RUN_JOB;

/// The event bus topic every appended run event is announced on.
pub const EVENTS_TOPIC: &str = "run_events";

/// How long a claim holds a run, and the job's lease with it; renewed by
/// every step, and longer than any one step (a model turn, a tool call).
pub const LEASE: Duration = Duration::from_secs(5 * 60);

const IDLE: Duration = Duration::from_secs(1);

/// How often the worker looks for runs no job takes forward.
const SWEEP: Duration = Duration::from_secs(60);

/// The limits a worker runs under: the reducer's, with the lease above.
fn limits() -> Limits {
    Limits {
        lease_ms: u64::try_from(LEASE.as_millis()).unwrap_or(u64::MAX),
        ..Limits::default()
    }
}

/// Announces appended events on the workspace's and its environment's
/// topic. Best effort: the events are in the store either way.
pub async fn publish(
    state: &AppState,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    events: &[RunEvent],
) {
    let Ok(topic) = Name::new(EVENTS_TOPIC) else {
        return;
    };
    for event in events {
        let payload = json!({
            "workspace_id": workspace_id,
            "run_id": event.run_id,
            "sequence": event.sequence,
            "type": event.kind.as_str(),
        })
        .to_string()
        .into_bytes();
        for scope in [
            Scope::Workspace(organization_id, workspace_id),
            Scope::Environment(organization_id, environment),
        ] {
            if let Err(error) = state.events.publish(&scope, &topic, payload.clone()).await {
                tracing::debug!(event = "runs.publish_failed", %organization_id, %workspace_id, %error);
            }
        }
    }
}

/// How a run reaches an app's server: the one way a call is sent. Every
/// method takes the run's organization and workspace and reaches only the
/// installation it names, in them.
#[async_trait]
pub trait ToolPort: Send + Sync {
    /// The installation's server and credential, opened for the call about
    /// to be made.
    type Reach: Send;

    /// Reads the installation's server and credential again in its own
    /// scope, refreshing an OAuth token near expiry. Never another
    /// installation's.
    async fn reach(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation: &Installation,
    ) -> Result<Self::Reach, Unreachable>;

    /// Sends one `tools/call` under its original MCP name, with the
    /// installation's credential added last. A `401` allows one refresh and
    /// one replay (decision 0042); nothing else is replayed.
    async fn send(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        reach: Self::Reach,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> SendOutcome;
}

/// The MCP path: [`mcp_access`] over the server's `OutboundHttp`.
pub struct McpTools<'a> {
    state: &'a AppState,
}

impl<'a> McpTools<'a> {
    pub fn new(state: &'a AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ToolPort for McpTools<'_> {
    type Reach = mcp_access::Reach;

    async fn reach(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation: &Installation,
    ) -> Result<Self::Reach, Unreachable> {
        mcp_access::reach(self.state, organization_id, workspace_id, installation).await
    }

    async fn send(
        &self,
        organization_id: OrganizationId,
        workspace_id: WorkspaceId,
        installation_id: InstallationId,
        reach: Self::Reach,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> SendOutcome {
        let answer = mcp_access::call_tool(
            self.state,
            organization_id,
            workspace_id,
            installation_id,
            reach,
            tool,
            arguments,
        )
        .await;
        match answer {
            Ok(answer) if !answer.is_error => SendOutcome::Completed { result: answer.result },
            Ok(answer) => SendOutcome::ToolError { result: answer.result },
            Err(CallError::NotSent(_)) => SendOutcome::NotSent,
            Err(CallError::Rejected(_)) => SendOutcome::Rejected,
            Err(CallError::Withheld) => SendOutcome::Withheld,
            Err(CallError::Unknown(_)) => SendOutcome::Unknown,
            Err(CallError::Unauthorized) => SendOutcome::Unauthorized,
        }
    }
}

/// What becomes of the job once the worker stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Nothing more for it: the run finished, waits for a person, or is gone.
    Complete,
    /// Another worker's claim, or a call's send claim, holds the run until
    /// then: claim again then, without using an attempt.
    Postpone(zupflow_core::time::Timestamp),
    /// Worth another attempt later, with why.
    Retry(&'static str),
    /// The store already queued, postponed or released the run's job with
    /// the step that released the claim, or the job's lease is lost.
    Leave,
}

/// The organization's levels for a catalog app's tools, as read.
enum Levels {
    Read(HashMap<String, OrganizationLevel>),
    Unreadable,
}

/// One claim of one run, in its organization and workspace.
struct Driver<'a, P: ToolPort> {
    state: &'a AppState,
    tools: &'a P,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    environment: Environment,
    run_id: RunId,
    agent_id: AgentId,
    /// The job this worker holds: its scope, ID and lease.
    job: (&'a Scope, JobId, &'a LeaseToken),
    /// This worker's own identity for its claim: fresh per claim.
    claimant: ClaimantId,
    /// The owner epoch this worker's claim holds.
    claim: Option<u64>,
    /// Steps committed under this claim.
    steps: u32,
    last_attempt: bool,
    limits: Limits,
}

fn fresh_claimant() -> ClaimantId {
    ClaimantId::from_uuid(Uuid::now_v7())
}

impl<P: ToolPort> Driver<'_, P> {
    fn store(&self) -> &dyn RunStore {
        self.state.runs.as_ref()
    }

    /// Keeps the job's lease as long as the claim's; `false` once it's lost:
    /// another worker has the job.
    async fn extend_job(&self) -> bool {
        let (scope, id, lease) = self.job;
        self.state.jobs.extend(scope, id, lease, LEASE).await.is_ok()
    }

    /// Works the run until it parks, yields, finishes, or another worker
    /// holds it.
    async fn work(&mut self) -> Outcome {
        let mut model: Option<ModelAnswer> = None;
        let mut sent: Option<SendResult> = None;
        let mut reach: Option<P::Reach> = None;
        // Every step commits or reloads; a run that keeps conflicting is
        // given back rather than worked forever.
        let budget = self.limits.max_steps_per_claim.saturating_mul(4).saturating_add(16);
        for _ in 0..budget {
            let snapshot = match self
                .store()
                .find_snapshot(self.organization_id, self.workspace_id, self.run_id)
                .await
            {
                Ok(Some(snapshot)) => snapshot,
                // Gone with its workspace, or not one this engine advances.
                Ok(None) | Err(StoreError::Unsupported) => return Outcome::Complete,
                Err(_) => return Outcome::Retry("store_unavailable"),
            };
            // An answer to an attempt the run no longer waits on is nobody's.
            if model.as_ref().is_some_and(|answer| {
                snapshot.control.continuation
                    != (zupflow_core::runs::state::Continuation::AwaitModel {
                        turn: answer.turn,
                        attempt: answer.attempt,
                    })
            }) {
                model = None;
            }
            let facts = match self.facts(&snapshot, model.as_ref(), sent.as_ref(), &mut reach).await {
                Ok(facts) => facts,
                Err(why) => return Outcome::Retry(why),
            };
            let plan = match advance(&snapshot, &facts) {
                Ok(plan) => plan,
                Err(error) => {
                    tracing::error!(event = "runs.advance_failed", organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, %error);
                    return Outcome::Retry("run_inconsistent");
                }
            };
            let (request, then) = match plan {
                Plan::Apply { request, then } => (request, then),
                Plan::Idle(Idle::Recorded) => {
                    sent = None;
                    continue;
                }
                Plan::Idle(Idle::Finished | Idle::AwaitingApproval) => return Outcome::Complete,
                Plan::Idle(Idle::Busy { until }) => return Outcome::Postpone(until),
                // This worker's claim ran out or another worker took the
                // run: claim it again, as a new claimant, if it's free.
                Plan::Idle(Idle::Lost) => {
                    self.claim = None;
                    self.claimant = fresh_claimant();
                    self.steps = 0;
                    continue;
                }
            };
            let operation = request.operation();
            let worker_step = request.guards().owner.is_some() || matches!(operation, Operation::Claim(_));
            if worker_step && !self.extend_job().await {
                return Outcome::Leave;
            }
            let applied = match self
                .store()
                .apply(self.organization_id, self.workspace_id, &request)
                .await
            {
                Ok(applied) => applied,
                Err(StoreError::NotFound | StoreError::Unsupported) => return Outcome::Complete,
                Err(StoreError::Unavailable) => return Outcome::Retry("store_unavailable"),
                Err(error) => {
                    tracing::error!(event = "runs.transition_refused", organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, operation = operation.name(), %error);
                    return Outcome::Retry("transition_refused");
                }
            };
            let receipt = match applied {
                Applied::Committed(receipt) => receipt,
                // Nothing changed, and no I/O the request allowed happens.
                Applied::Conflict(conflict) => {
                    match conflict {
                        // Read the run again and plan again.
                        Conflict::Revision | Conflict::Cancelled | Conflict::Claimed => {}
                        Conflict::Owner => {
                            self.claim = None;
                            self.claimant = fresh_claimant();
                            self.steps = 0;
                        }
                        // The send claim isn't this outcome's to record.
                        Conflict::SendClaim => sent = None,
                    }
                    continue;
                }
            };
            publish(
                self.state,
                self.organization_id,
                self.workspace_id,
                self.environment,
                &receipt.events,
            )
            .await;
            self.steps = self.steps.saturating_add(1);
            match operation {
                Operation::Claim(claim) => self.claim = Some(claim.epoch),
                Operation::RecordOutcome(_) => sent = None,
                Operation::CommitTurn(_) | Operation::Yield(_) | Operation::Finish(_) => model = None,
                _ => {}
            }
            // A step that released the claim dealt with the job too.
            let released = request
                .guards()
                .owner
                .is_some_and(|owner| owner.renew_lease_ms.is_none());
            match then {
                Io::CallModel(asked) => model = Some(self.ask(&asked).await),
                Io::SendCall(send) => {
                    let outcome = match reach.take() {
                        Some(opened) => {
                            // The run's own scope, never one read off the plan.
                            self.tools
                                .send(
                                    self.organization_id,
                                    self.workspace_id,
                                    send.installation_id,
                                    opened,
                                    &send.tool,
                                    &send.arguments,
                                )
                                .await
                        }
                        // Authorized without its server opened: it never left.
                        None => SendOutcome::NotSent,
                    };
                    sent = Some(sent_result(&send, outcome));
                }
                Io::Nothing if released => return Outcome::Leave,
                Io::Nothing => {}
            }
        }
        Outcome::Retry("too_many_steps")
    }

    /// The facts the next step needs, read now.
    async fn facts(
        &self,
        snapshot: &Snapshot,
        model: Option<&ModelAnswer>,
        sent: Option<&SendResult>,
        reach: &mut Option<P::Reach>,
    ) -> Result<Facts, &'static str> {
        use zupflow_core::runs::state::Continuation;

        let mut facts = Facts::at(self.state.clock.now());
        facts.claimant = Some(self.claimant);
        facts.claim = self.claim;
        facts.steps = self.steps;
        facts.limits = self.limits;
        facts.last_attempt = self.last_attempt;
        facts.model = model.cloned();
        facts.sent = sent.cloned();
        *reach = None;
        let control = &snapshot.control;
        let holds = self.claim == Some(control.owner_epoch) && control.owner_claimant == Some(self.claimant);
        // An outcome to record, a claim to make, or a claim lost: nothing
        // else is read.
        if facts.sent.is_some() || !holds {
            return Ok(facts);
        }
        match control.continuation {
            Continuation::NeedModel { .. } => facts.offer = Some(self.offer().await?),
            Continuation::AwaitModel { .. } => match model.map(|answer| &answer.result) {
                None => facts.offer = Some(self.offer().await?),
                Some(ModelResult::Answered(completion)) => {
                    let wanted = calls_to_decide(snapshot, completion, &self.limits).map_err(|_| "run_inconsistent")?;
                    for (position, tool) in wanted {
                        facts.decisions.push((position, self.decide_offered(tool).await?));
                    }
                    let asked = match &completion.reply {
                        zupflow_core::models::Reply::ToolCalls { calls, .. } => calls.len(),
                        zupflow_core::models::Reply::Text(_) => 0,
                    };
                    facts.call_ids = (0..asked).map(|_| ToolCallId::generate()).collect();
                }
                Some(_) => {}
            },
            Continuation::NeedCall { turn, position } => {
                if let Some(call) = snapshot
                    .call_at(turn, position)
                    .filter(|call| matches!(call.state, CallState::Allowed | CallState::Approved))
                {
                    let (check, opened) = self.send_check(call).await?;
                    facts.send_check = Some(check);
                    facts.invocation_id = Some(InvocationId::from_uuid(Uuid::now_v7()));
                    *reach = opened;
                }
            }
            Continuation::AwaitApproval { .. } | Continuation::AwaitCall { .. } | Continuation::Done => {}
        }
        Ok(facts)
    }

    /// Asks the model; what came of it, for the step that commits it.
    async fn ask(&self, asked: &ModelRequest) -> ModelAnswer {
        let turn = asked.as_turn();
        let result = match self.state.provider.complete(&turn).await {
            Ok(completion) => {
                if let Err(why) = validate_completion(&turn, &completion) {
                    tracing::error!(event = "runs.model_answer_invalid", provider = self.state.provider.name(), run_id = %self.run_id, why);
                }
                ModelResult::Answered(completion)
            }
            Err(ProviderError::Unavailable) => ModelResult::Unavailable,
            Err(ProviderError::NotConfigured) => ModelResult::Failed(Failure::new(
                "model_unavailable",
                "No model provider is configured for this deployment, so the run can't be answered.",
            )),
            Err(ProviderError::UnknownModel) => ModelResult::Failed(Failure::new(
                "model_unavailable",
                format!(
                    "The model {} isn't available. Choose another model for the agent and start a new run.",
                    asked.model
                ),
            )),
            Err(error) => {
                tracing::error!(event = "runs.model_failed", provider = self.state.provider.name(), organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, %error);
                ModelResult::Failed(Failure::new(
                    "model_failed",
                    "The model provider couldn't answer this run. Start it again.",
                ))
            }
        };
        ModelAnswer {
            turn: asked.turn,
            attempt: asked.attempt,
            result,
        }
    }

    /// The tools to offer the next model attempt: each granted tool of each
    /// installation that neither the floor nor the policy refuses, named
    /// after its app's key, its installation's short ID and itself, and
    /// described with its pinned definition. A deleted or disconnected
    /// installation offers nothing (decision 0047).
    async fn offer(&self) -> Result<Offer, &'static str> {
        let grants = match self
            .state
            .agents
            .list_grants(self.organization_id, self.workspace_id, self.agent_id)
            .await
        {
            Ok(grants) => grants,
            Err(zupflow_core::agents::AgentError::Unavailable) => return Err("store_unavailable"),
            // The agent is gone: its run goes on without tools.
            Err(_) => Vec::new(),
        };
        let mut candidates = Vec::new();
        for grant in grants {
            let Some(installation) = self.installation(grant.installation_id).await? else {
                continue;
            };
            if installation.status == InstallationStatus::Disconnected {
                continue;
            }
            let Some((app_key, app_name)) = self.app(&installation).await? else {
                continue;
            };
            let registered = self.registry(&installation).await?;
            let levels = self.levels(&installation, &grant.tools).await;
            for tool in &grant.tools {
                let call = self.policy_call(
                    &installation,
                    tool,
                    Some(grant.approval_required.contains(tool)),
                    registered.get(tool),
                    &levels,
                );
                let policy = self.state.approvals.name();
                if floor(&call, policy).is_some_and(|decision| matches!(decision.verdict, Verdict::Deny { .. })) {
                    continue;
                }
                if matches!(self.state.approvals.decide(&call).await.verdict, Verdict::Deny { .. }) {
                    continue;
                }
                let Some(registered) = registered.get(tool) else {
                    continue;
                };
                // The model is given the pinned definition, never a changed one.
                let definition: Map<String, Value> =
                    serde_json::from_str(&registered.pinned.definition).unwrap_or_default();
                candidates.push(Candidate {
                    installation_id: installation.id,
                    installation_short_id: installation.short_id.as_str().to_owned(),
                    installation_label: installation.label.clone(),
                    app_key: app_key.clone(),
                    app_name: app_name.clone(),
                    tool: tool.clone(),
                    description: definition
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: definition
                        .get("inputSchema")
                        .filter(|schema| schema.is_object())
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object" })),
                    definition_sha256: Sha256Digest::from_bytes(registered.pinned.hash),
                });
            }
        }
        let built = build_offer(candidates);
        for clash in &built.clashes {
            let installations: Vec<String> = clash.entries.iter().map(|(id, _)| id.to_string()).collect();
            tracing::warn!(event = "runs.tool_names_clash", organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, name = %clash.name, installations = ?installations);
        }
        for excluded in &built.excluded {
            tracing::warn!(event = "runs.tool_not_named", organization_id = %self.organization_id, workspace_id = %self.workspace_id, run_id = %self.run_id, installation_id = %excluded.installation_id, why = ?excluded.why);
        }
        Ok(built.offer)
    }

    /// The floor's and then the policy's decision on a call to an offered
    /// tool, read now.
    async fn decide_offered(&self, tool: &OfferedTool) -> Result<CallDecision, &'static str> {
        let Some(installation) = self.installation(tool.installation_id).await? else {
            // Deleted since it was offered, and its grants with it.
            return Ok(not_granted(&tool.tool, self.state.approvals.name()));
        };
        let call = self.tool_call(&installation, &tool.tool).await?;
        Ok(CallDecision::from(&self.decide(&call).await))
    }

    /// A call checked again just before it's sent: its installation, the
    /// floor and the policy, and the tool's pinned definition. When it would
    /// be sent, its server is reached now, so a credential that needs
    /// reconnecting refuses it before it's recorded as sent.
    async fn send_check(&self, call: &CallRecord) -> Result<(SendCheck, Option<P::Reach>), &'static str> {
        let Some(target) = &call.target else {
            // An open call always has a target; the reducer says so.
            return Err("run_inconsistent");
        };
        let Some(installation) = self.installation(target.installation_id).await? else {
            let decision = call.decision.clone().ok_or("run_inconsistent")?;
            return Ok((
                SendCheck {
                    call_id: call.id,
                    installation: InstallationCheck::Removed,
                    definition_sha256: None,
                    decision,
                },
                None,
            ));
        };
        let registered = self.registry(&installation).await?;
        let definition = registered
            .get(&target.tool)
            .map(|tool| Sha256Digest::from_bytes(tool.pinned.hash));
        let tool_call = self.tool_call_with(&installation, &target.tool, &registered).await?;
        let decision = CallDecision::from(&self.decide(&tool_call).await);
        let mut opened = None;
        let check = match installation.status {
            InstallationStatus::Disconnected => InstallationCheck::Disconnected,
            InstallationStatus::ReconnectRequired => InstallationCheck::ReconnectRequired,
            InstallationStatus::Connected => {
                let sends = match &decision.verdict {
                    CallVerdict::Allow => true,
                    CallVerdict::RequireApproval { .. } => call.state == CallState::Approved,
                    CallVerdict::Deny { .. } => false,
                } && definition.is_some_and(|definition| {
                    Some(call_digest(
                        target.installation_id,
                        &target.app_key,
                        &target.tool,
                        &definition,
                        &call.arguments,
                    )) == call.call_digest
                });
                if sends {
                    match self
                        .tools
                        .reach(self.organization_id, self.workspace_id, &installation)
                        .await
                    {
                        Ok(reach) => {
                            opened = Some(reach);
                            InstallationCheck::Usable
                        }
                        Err(Unreachable::Retry(why)) => return Err(why),
                        // Removed or disconnected since it was read: say so,
                        // and the run goes on.
                        Err(unreachable) if unreachable.needs_reconnect() => {
                            match self.installation(installation.id).await? {
                                None => InstallationCheck::Removed,
                                Some(now) if now.status == InstallationStatus::Disconnected => {
                                    InstallationCheck::Disconnected
                                }
                                Some(_) => InstallationCheck::ReconnectRequired,
                            }
                        }
                        Err(unreachable) => {
                            tracing::warn!(event = "runs.installation_unreachable", organization_id = %self.organization_id, workspace_id = %self.workspace_id, installation_id = %installation.id, why = unreachable.why());
                            InstallationCheck::Unusable
                        }
                    }
                } else {
                    InstallationCheck::Usable
                }
            }
        };
        Ok((
            SendCheck {
                call_id: call.id,
                installation: check,
                definition_sha256: definition,
                decision,
            },
            opened,
        ))
    }

    async fn installation(&self, installation_id: InstallationId) -> Result<Option<Installation>, &'static str> {
        self.state
            .installations
            .find_installation(self.organization_id, self.workspace_id, installation_id)
            .await
            .map_err(|_| "store_unavailable")
    }

    /// The installation's app's key and name, as the model and the timeline
    /// name it; `None` once the app is gone.
    async fn app(&self, installation: &Installation) -> Result<Option<(String, String)>, &'static str> {
        let found = match installation.app {
            InstalledApp::Catalog(app_id) => {
                self.state
                    .apps
                    .find_organization_app(self.organization_id, app_id)
                    .await
            }
            InstalledApp::Workspace(app_id) => {
                self.state
                    .apps
                    .find_workspace_app(self.organization_id, self.workspace_id, app_id)
                    .await
            }
        };
        found
            .map(|app| app.map(|app| (app.key, app.name)))
            .map_err(|_| "store_unavailable")
    }

    /// The policy's view of one call to `tool` through `installation`, read
    /// now: the grant, the registry and the organization's level.
    async fn tool_call(&self, installation: &Installation, tool: &str) -> Result<ToolCall, &'static str> {
        let registered = self.registry(installation).await?;
        self.tool_call_with(installation, tool, &registered).await
    }

    async fn tool_call_with(
        &self,
        installation: &Installation,
        tool: &str,
        registered: &BTreeMap<String, RegisteredTool>,
    ) -> Result<ToolCall, &'static str> {
        let grants = match self
            .state
            .agents
            .list_grants(self.organization_id, self.workspace_id, self.agent_id)
            .await
        {
            Ok(grants) => grants,
            Err(zupflow_core::agents::AgentError::Unavailable) => return Err("store_unavailable"),
            Err(_) => Vec::new(),
        };
        let grant = grants
            .iter()
            .find(|grant| grant.installation_id == installation.id && grant.tools.iter().any(|name| name == tool));
        let levels = self.levels(installation, &[tool.to_owned()]).await;
        Ok(self.policy_call(
            installation,
            tool,
            grant.map(|grant| grant.approval_required.iter().any(|name| name == tool)),
            registered.get(tool),
            &levels,
        ))
    }

    fn policy_call(
        &self,
        installation: &Installation,
        tool: &str,
        approval_required: Option<bool>,
        registered: Option<&RegisteredTool>,
        levels: &Levels,
    ) -> ToolCall {
        let (app, level) = match installation.app {
            InstalledApp::Workspace(app_id) => (CallApp::Workspace(app_id), OrganizationLevel::NotApplicable),
            InstalledApp::Catalog(app_id) => (
                CallApp::Catalog(app_id),
                match levels {
                    Levels::Unreadable => OrganizationLevel::Unreadable,
                    Levels::Read(levels) => levels.get(tool).copied().unwrap_or(OrganizationLevel::Unset),
                },
            ),
        };
        ToolCall {
            organization_id: self.organization_id,
            workspace_id: self.workspace_id,
            agent_id: self.agent_id,
            installation_id: installation.id,
            app,
            tool: tool.to_owned(),
            status: registered.map(RegisteredTool::status),
            grant: match approval_required {
                None => GrantedAs::NotGranted,
                Some(approval_required) => GrantedAs::Granted { approval_required },
            },
            level,
        }
    }

    /// Every tool in the installation's registry, by name.
    async fn registry(&self, installation: &Installation) -> Result<BTreeMap<String, RegisteredTool>, &'static str> {
        let mut tools = BTreeMap::new();
        let mut after: Option<String> = None;
        loop {
            let page = match self
                .state
                .tools
                .list_tools(
                    self.organization_id,
                    self.workspace_id,
                    installation.id,
                    after.as_deref(),
                    zupflow_core::tools::MAX_PAGE,
                )
                .await
            {
                Ok(page) => page,
                Err(ToolError::NotFound) => return Ok(tools),
                Err(_) => return Err("store_unavailable"),
            };
            for tool in page.items {
                tools.insert(tool.name.clone(), tool);
            }
            match page.next_after {
                Some(next) => after = Some(next),
                None => return Ok(tools),
            }
        }
    }

    /// The organization's levels for a catalog app's tools.
    async fn levels(&self, installation: &Installation, tools: &[String]) -> Levels {
        let InstalledApp::Catalog(app_id) = installation.app else {
            return Levels::Read(HashMap::new());
        };
        match self
            .state
            .policies
            .find_levels(self.organization_id, app_id, tools)
            .await
        {
            Ok(levels) => Levels::Read(
                levels
                    .into_iter()
                    .map(|level| {
                        (
                            level.tool,
                            OrganizationLevel::Set {
                                level: level.level,
                                confirmed: level.confirmed,
                            },
                        )
                    })
                    .collect(),
            ),
            // Fails closed: the floor requires approval.
            Err(_) => Levels::Unreadable,
        }
    }

    /// Floor, then policy.
    async fn decide(&self, call: &ToolCall) -> Decision {
        match floor(call, self.state.approvals.name()) {
            Some(decision) => decision,
            None => self.state.approvals.decide(call).await,
        }
    }
}

/// The floor's answer for a tool no grant names any more.
fn not_granted(tool: &str, policy: &str) -> CallDecision {
    CallDecision {
        verdict: CallVerdict::Deny {
            reason: format!("{tool} isn't granted to this agent."),
        },
        policy: policy.to_owned(),
        mode: None,
        rule: zupflow_core::policy::Rule::NotGranted.as_str().to_owned(),
    }
}

fn sent_result(send: &SendRequest, outcome: SendOutcome) -> SendResult {
    SendResult {
        call_id: send.call_id,
        attempt: send.attempt,
        invocation_id: send.invocation_id,
        outcome,
    }
}

/// Fails the runs no job takes forward any more (their last job died), so
/// none stays queued or running forever: `run_unavailable`, unless a
/// worker's lease or a call's send claim on it is still live. How many it
/// found.
pub async fn sweep(state: &AppState) -> usize {
    // Cross-tenant by design, as the claim: each run comes with its scope.
    let abandoned = match state.runs.abandoned_runs(100).await {
        Ok(abandoned) => abandoned,
        Err(error) => {
            tracing::warn!(event = "runs.sweep_failed", %error);
            return 0;
        }
    };
    for (organization_id, workspace_id, run_id) in &abandoned {
        let (organization_id, workspace_id, run_id) = (*organization_id, *workspace_id, *run_id);
        let environment = match state.workspaces.find_workspace(organization_id, workspace_id).await {
            Ok(Some(workspace)) => workspace.environment,
            _ => continue,
        };
        tracing::warn!(event = "runs.abandoned", %organization_id, %workspace_id, %run_id);
        // A revision conflict means the run moved: read it again.
        for _ in 0..3 {
            let Ok(Some(snapshot)) = state.runs.find_snapshot(organization_id, workspace_id, run_id).await else {
                break;
            };
            let Ok(request) = fail_abandoned(
                organization_id,
                workspace_id,
                &snapshot,
                RequestId::from_uuid(Uuid::now_v7()),
                state.clock.now(),
            ) else {
                break;
            };
            match state.runs.apply(organization_id, workspace_id, &request).await {
                Ok(Applied::Committed(receipt)) => {
                    publish(state, organization_id, workspace_id, environment, &receipt.events).await;
                    break;
                }
                Ok(Applied::Conflict(conflict)) if conflict.reload() => {}
                _ => break,
            }
        }
    }
    abandoned.len()
}

/// Claims and works one run's job, if one is due. Whether one was.
pub async fn work_once(state: &AppState) -> bool {
    let Ok(kind) = Name::new(RUN_AGENT) else {
        return false;
    };
    // Cross-tenant by design: a worker takes the next job of any scope.
    let job = match state.jobs.claim_next(std::slice::from_ref(&kind), LEASE).await {
        Ok(Some(job)) => job,
        Ok(None) => return false,
        Err(error) => {
            tracing::warn!(event = "runs.claim_failed", %error);
            return false;
        }
    };
    handle(state, &McpTools::new(state), job).await;
    true
}

async fn handle<P: ToolPort>(state: &AppState, tools: &P, job: Job) {
    let Job {
        id,
        scope,
        payload,
        attempt,
        lease,
        ..
    } = job;
    let (Scope::Workspace(organization_id, workspace_id), Some(run_id)) = (scope, run_of_job(&payload)) else {
        tracing::error!(event = "runs.malformed_job", kind = RUN_AGENT);
        if let Ok(reason) = Name::new("malformed") {
            let _ = state.jobs.bury(&scope, id, &lease, &reason).await;
        }
        return;
    };
    let retry = |why: &'static str| {
        let (scope, lease) = (&scope, &lease);
        async move {
            let Ok(reason) = Name::new(why) else {
                return state.jobs.complete(scope, id, lease).await;
            };
            let after = Duration::from_secs(5 * u64::from(attempt.max(1)));
            state.jobs.retry(scope, id, lease, after, &reason).await.map(|_| ())
        }
    };
    let environment = match state.workspaces.find_workspace(organization_id, workspace_id).await {
        Ok(Some(workspace)) => workspace.environment,
        // Gone, with its runs.
        Ok(None) => {
            let _ = state.jobs.complete(&scope, id, &lease).await;
            return;
        }
        Err(_) => {
            let _ = retry("store_unavailable").await;
            return;
        }
    };
    let agent_id = match state.runs.find_run(organization_id, workspace_id, run_id).await {
        Ok(Some(run)) => run.agent_id,
        Ok(None) => {
            let _ = state.jobs.complete(&scope, id, &lease).await;
            return;
        }
        Err(_) => {
            let _ = retry("store_unavailable").await;
            return;
        }
    };
    let mut driver = Driver {
        state,
        tools,
        organization_id,
        workspace_id,
        environment,
        run_id,
        agent_id,
        job: (&scope, id, &lease),
        claimant: fresh_claimant(),
        claim: None,
        steps: 0,
        last_attempt: attempt >= zupflow_core::runs::store::RUN_JOB_ATTEMPTS,
        limits: limits(),
    };
    let finished = match driver.work().await {
        Outcome::Complete => state.jobs.complete(&scope, id, &lease).await,
        Outcome::Leave => Ok(()),
        Outcome::Postpone(until) => {
            let after = until.millis().saturating_sub(state.clock.now().millis());
            let after = Duration::from_millis(u64::try_from(after).unwrap_or(0));
            state.jobs.postpone(&scope, id, &lease, after).await
        }
        Outcome::Retry(why) => retry(why).await,
    };
    match finished {
        // The store dealt with the run's job in the step that released it.
        Ok(()) | Err(zupflow_core::jobs::JobError::LeaseLost) => {}
        Err(error) => {
            tracing::warn!(event = "runs.finish_failed", %organization_id, %workspace_id, %run_id, %error);
        }
    }
}

/// Works runs until the process stops.
pub async fn run_worker(state: AppState) {
    let mut swept = tokio::time::Instant::now();
    loop {
        if swept.elapsed() >= SWEEP {
            sweep(&state).await;
            swept = tokio::time::Instant::now();
        }
        if !work_once(&state).await {
            tokio::time::sleep(IDLE).await;
        }
    }
}
