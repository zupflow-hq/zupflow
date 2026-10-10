//! The `RunStore` suite: runs, their timelines and calls reached only through
//! their own organization and workspace; a run only of an agent of its
//! workspace, queued with its job in one change; lists newest first, paged,
//! filtered, and across an organization's workspaces only its own; each
//! step taken only from the state it expects, so a step repeated (a second
//! worker, a retried job) moves nothing; a call begun at most once; an
//! approval resolved only while the run waits on exactly that call, queueing
//! the run again; events numbered from 1 without gaps; and a run that
//! outlives its agent.
//!
//! `prepare` makes a [`Tenant`] exist: its organization and two workspaces,
//! the first in Sandbox, the second in Production. The suite makes agents
//! through `agents` and claims queued jobs through `jobs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use zupflow_core::agents::{AgentStore, NewAgent};
use zupflow_core::environment::Environment;
use zupflow_core::ids::{AgentId, ApiKeyId, InstallationId, OrganizationId, RunId, SessionId, ToolCallId, WorkspaceId};
use zupflow_core::jobs::{JobOptions, JobQueue};
use zupflow_core::models::TokenUsage;
use zupflow_core::runs::{
    ApprovalDecision, CallOutcome, CallState, CallTarget, Ending, Failure, NewRun, NewRunEvent, NewToolCall, NewTurn,
    OrganizationRunFilter, QueuedJob, RecordedDecision, RunError, RunEventType, RunFilter, RunStatus, RunStore,
    RunTrigger,
};
use zupflow_core::scope::{Name, Scope};
use zupflow_core::time::Timestamp;

#[derive(Debug, Clone, Copy)]
pub struct Tenant {
    pub organization_id: OrganizationId,
    /// In Sandbox.
    pub workspace: WorkspaceId,
    /// In Production.
    pub sibling: WorkspaceId,
}

pub type Prepare = Arc<dyn Fn(Tenant) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct Stores<'a> {
    pub runs: &'a dyn RunStore,
    pub agents: &'a dyn AgentStore,
    pub jobs: &'a dyn JobQueue,
}

/// The job kind the suite queues: its own, so it claims only its jobs.
const KIND: &str = "conformance_run";

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
                model: "model-a".to_owned(),
                instructions: "Be brief.".to_owned(),
            },
        )
        .await
        .unwrap()
        .id
}

fn job(run: RunId) -> QueuedJob {
    QueuedJob {
        kind: Name::new(KIND).unwrap(),
        payload: run.to_string().into_bytes(),
        options: JobOptions::default(),
    }
}

fn event(kind: RunEventType) -> NewRunEvent {
    NewRunEvent::new(kind, json!({}))
}

fn new_run(agent_id: AgentId, input: &str) -> NewRun {
    NewRun {
        agent_id,
        session_id: SessionId::generate(),
        trigger: RunTrigger::Api,
        input: input.to_owned(),
        model: "model-a".to_owned(),
        instructions: "Be brief.".to_owned(),
    }
}

async fn create(
    stores: &Stores<'_>,
    organization_id: OrganizationId,
    workspace: WorkspaceId,
    agent_id: AgentId,
    input: &str,
    at: Timestamp,
) -> RunId {
    let id = RunId::generate();
    let (run, events) = stores
        .runs
        .create_run(
            organization_id,
            workspace,
            id,
            &new_run(agent_id, input),
            &[event(RunEventType::RunQueued)],
            &job(id),
            at,
        )
        .await
        .unwrap();
    assert_eq!((run.id, run.status), (id, RunStatus::Queued), "created queued");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 1, "events count from 1");
    id
}

/// Claims every queued job of the suite's kind, answering their payloads,
/// each with its scope.
async fn claim_all(stores: &Stores<'_>) -> Vec<(Scope, String)> {
    let kind = Name::new(KIND).unwrap();
    let mut claimed = Vec::new();
    while let Some(job) = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        stores.jobs.complete(&job.scope, job.id, &job.lease).await.unwrap();
        claimed.push((job.scope, String::from_utf8(job.payload).unwrap()));
    }
    claimed
}

fn target(tool: &str) -> CallTarget {
    CallTarget {
        installation_id: InstallationId::generate(),
        installation_label: "Main".to_owned(),
        app_name: "Catalog".to_owned(),
        tool: tool.to_owned(),
    }
}

fn decision(verdict: &str, rule: &str) -> RecordedDecision {
    RecordedDecision {
        verdict: verdict.to_owned(),
        policy: "configured".to_owned(),
        mode: Some("manual".to_owned()),
        rule: rule.to_owned(),
        reason: (verdict != "allow").then(|| format!("because {rule}")),
    }
}

fn call(model_call_id: &str, tool: Option<&str>, state: CallState) -> NewToolCall {
    let mut arguments = serde_json::Map::new();
    arguments.insert("query".to_owned(), json!(model_call_id));
    NewToolCall {
        id: ToolCallId::generate(),
        model_call_id: model_call_id.to_owned(),
        name: tool.unwrap_or("imagined").to_owned(),
        target: tool.map(target),
        arguments,
        state,
        decision: match state {
            CallState::Allowed => Some(decision("allow", "allowed")),
            CallState::AwaitingApproval => Some(decision("require_approval", "grant_requires_approval")),
            _ => tool.map(|_| decision("deny", "organization_denies")),
        },
        outcome: (state == CallState::Refused).then(|| CallOutcome {
            content: "Refused.".to_owned(),
            is_error: true,
        }),
    }
}

pub async fn run(stores: Stores<'_>, prepare: Prepare) {
    let a = tenant(&prepare).await;
    let b = tenant(&prepare).await;
    let org = a.organization_id;
    let agent_a = agent(&stores, org, a.workspace, "Here").await;
    let agent_sibling = agent(&stores, org, a.sibling, "Sibling").await;
    let agent_b = agent(&stores, b.organization_id, b.workspace, "Theirs").await;
    claim_all(&stores).await;

    creates_runs_only_of_the_workspaces_agents(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    lists_newest_first_in_scope(&stores, a, b, agent_a, agent_sibling, agent_b).await;
    takes_each_step_once(&stores, a, b, agent_a).await;
    cancels_and_finishes_once(&stores, a, agent_a).await;
    decides_a_call_again_before_sending(&stores, a, agent_a).await;
    finds_runs_whose_job_died(&stores, a, agent_a).await;
    keeps_runs_of_a_deleted_agent(&stores, a).await;
}

async fn creates_runs_only_of_the_workspaces_agents(
    stores: &Stores<'_>,
    a: Tenant,
    b: Tenant,
    agent_a: AgentId,
    agent_sibling: AgentId,
    agent_b: AgentId,
) {
    let org = a.organization_id;
    let now = Timestamp::now();
    for (organization, workspace, agent) in [
        (org, a.workspace, agent_sibling),
        (org, a.workspace, agent_b),
        (b.organization_id, b.workspace, agent_a),
        (org, a.workspace, AgentId::generate()),
    ] {
        let id = RunId::generate();
        assert_eq!(
            stores
                .runs
                .create_run(
                    organization,
                    workspace,
                    id,
                    &new_run(agent, "hi"),
                    &[event(RunEventType::RunQueued)],
                    &job(id),
                    now
                )
                .await,
            Err(RunError::AgentNotFound),
            "a run only of an agent of its own workspace"
        );
    }
    assert!(claim_all(stores).await.is_empty(), "a refused run queues nothing");

    let id = create(stores, org, a.workspace, agent_a, "Hello", now).await;
    assert_eq!(
        claim_all(stores).await,
        [(Scope::Workspace(org, a.workspace), id.to_string())],
        "the run's job, queued with it, in its workspace"
    );
    let run = stores.runs.find_run(org, a.workspace, id).await.unwrap().unwrap();
    assert_eq!(
        (run.agent_id, run.agent_name.as_deref(), run.input.as_str(), run.trigger),
        (agent_a, Some("Here"), "Hello", RunTrigger::Api)
    );
    assert_eq!(
        (run.output, run.error, run.started_at, run.finished_at),
        (None, None, None, None)
    );
    for (organization, workspace) in [
        (org, a.sibling),
        (b.organization_id, b.workspace),
        (b.organization_id, a.workspace),
    ] {
        assert_eq!(stores.runs.find_run(organization, workspace, id).await.unwrap(), None);
        assert_eq!(
            stores.runs.list_events(organization, workspace, id, 0).await,
            Err(RunError::NotFound)
        );
        assert_eq!(
            stores.runs.find_progress(organization, workspace, id).await.unwrap(),
            None
        );
        assert_eq!(
            stores
                .runs
                .start_run(organization, workspace, id, &[event(RunEventType::RunStarted)], now)
                .await,
            Err(RunError::NotFound),
            "no step from another scope"
        );
        assert_eq!(
            stores
                .runs
                .cancel_run(organization, workspace, id, &[event(RunEventType::RunCancelled)], now)
                .await
                .map(|_| ()),
            Err(RunError::NotFound)
        );
    }
    assert_eq!(
        stores
            .runs
            .find_run(org, a.workspace, id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RunStatus::Queued,
        "untouched"
    );
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
    let base = Timestamp::now().millis() + 1_000_000;
    let mut made = Vec::new();
    for offset in 0..5 {
        made.push(
            create(
                stores,
                org,
                a.workspace,
                agent_a,
                "Page",
                Timestamp::from_millis(base + offset),
            )
            .await,
        );
    }
    // Two at the same instant, ordered by ID.
    made.push(
        create(
            stores,
            org,
            a.workspace,
            agent_a,
            "Same",
            Timestamp::from_millis(base + 10),
        )
        .await,
    );
    made.push(
        create(
            stores,
            org,
            a.workspace,
            agent_a,
            "Same",
            Timestamp::from_millis(base + 10),
        )
        .await,
    );
    let sibling = create(
        stores,
        org,
        a.sibling,
        agent_sibling,
        "Sibling",
        Timestamp::from_millis(base + 20),
    )
    .await;
    let theirs = create(
        stores,
        b.organization_id,
        b.workspace,
        agent_b,
        "Theirs",
        Timestamp::from_millis(base + 30),
    )
    .await;
    claim_all(stores).await;

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
        Err(RunError::InvalidCursor)
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
    let queued = stores
        .runs
        .list_runs(
            org,
            a.workspace,
            &RunFilter {
                agent_id: Some(agent_a),
                status: Some(RunStatus::Completed),
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert!(queued.items.iter().all(|run| run.status == RunStatus::Completed));

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

    let activity = stores
        .runs
        .agent_activity(org, a.workspace, Timestamp::from_millis(base + 5))
        .await
        .unwrap();
    assert_eq!(activity.len(), 1, "the workspace's own agents");
    assert_eq!(activity[0].agent_id, agent_a);
    assert_eq!(activity[0].runs_since, 2, "runs since the instant");
    assert_eq!(activity[0].last_run_at, Some(Timestamp::from_millis(base + 10)));
}

async fn takes_each_step_once(stores: &Stores<'_>, a: Tenant, b: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    let now = Timestamp::now();
    let id = create(stores, org, ws, agent_a, "Steps", now).await;
    claim_all(stores).await;
    let turn_with = |calls: Vec<NewToolCall>| NewTurn {
        text: Some("Working on it.".to_owned()),
        calls,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 3,
        },
        finish: None,
    };
    assert_eq!(
        stores
            .runs
            .record_turn(org, ws, id, 0, &turn_with(Vec::new()), &[], now)
            .await,
        Err(RunError::Moved),
        "no turn before the run starts"
    );
    let started = stores
        .runs
        .start_run(org, ws, id, &[event(RunEventType::RunStarted)], now)
        .await
        .unwrap();
    assert_eq!(started[0].sequence, 2);
    assert_eq!(
        stores
            .runs
            .start_run(org, ws, id, &[event(RunEventType::RunStarted)], now)
            .await,
        Err(RunError::Moved),
        "started once"
    );

    let allowed = call("m-1", Some("search"), CallState::Allowed);
    let awaiting = call("m-2", Some("send"), CallState::AwaitingApproval);
    let refused = call("m-3", None, CallState::Refused);
    let later = call("m-4", Some("send"), CallState::AwaitingApproval);
    let turn = turn_with(vec![allowed.clone(), awaiting.clone(), refused.clone(), later.clone()]);
    stores
        .runs
        .record_turn(org, ws, id, 0, &turn, &[event(RunEventType::MessageAssistant)], now)
        .await
        .unwrap();
    assert_eq!(
        stores
            .runs
            .record_turn(org, ws, id, 0, &turn_with(Vec::new()), &[], now)
            .await,
        Err(RunError::Moved),
        "a turn is recorded once"
    );
    assert_eq!(
        stores
            .runs
            .record_turn(b.organization_id, b.workspace, id, 1, &turn_with(Vec::new()), &[], now)
            .await,
        Err(RunError::NotFound)
    );
    let progress = stores.runs.find_progress(org, ws, id).await.unwrap().unwrap();
    assert_eq!(
        (progress.model.as_str(), progress.instructions.as_str()),
        ("model-a", "Be brief.")
    );
    assert_eq!(progress.turns.len(), 1);
    assert_eq!(progress.turns[0].text.as_deref(), Some("Working on it."));
    assert_eq!(
        progress.turns[0].calls,
        [allowed.id, awaiting.id, refused.id, later.id],
        "the turn's calls, in order"
    );
    assert_eq!(
        progress
            .calls
            .iter()
            .map(|call| (call.id, call.position, call.state))
            .collect::<Vec<_>>(),
        [
            (allowed.id, 0, CallState::Allowed),
            (awaiting.id, 1, CallState::AwaitingApproval),
            (refused.id, 2, CallState::Refused),
            (later.id, 3, CallState::AwaitingApproval),
        ]
    );
    let stored = &progress.calls[0];
    assert_eq!(
        (
            stored.model_call_id.as_str(),
            stored.name.as_str(),
            stored.target.as_ref()
        ),
        ("m-1", "search", allowed.target.as_ref())
    );
    assert_eq!(stored.arguments, allowed.arguments);
    assert_eq!(
        stored.decision, allowed.decision,
        "the decision, with its policy, mode and rule"
    );
    assert_eq!(progress.calls[2].target, None, "a tool it wasn't given has no target");
    assert_eq!(
        progress.calls[2].outcome, refused.outcome,
        "a refusal is settled as recorded"
    );
    assert_eq!(progress.run.usage.input_tokens, 10);
    assert_eq!(progress.run.usage.output_tokens, 3);

    // A call is begun once, only once allowed or approved.
    for not_ready in [awaiting.id, refused.id, ToolCallId::generate()] {
        assert_eq!(
            stores.runs.begin_call(org, ws, id, not_ready, &[], now).await,
            Err(RunError::Moved),
            "only an allowed or approved call is begun"
        );
    }
    stores
        .runs
        .begin_call(org, ws, id, allowed.id, &[event(RunEventType::ToolCallStarted)], now)
        .await
        .unwrap();
    assert_eq!(
        stores.runs.begin_call(org, ws, id, allowed.id, &[], now).await,
        Err(RunError::Moved),
        "never begun twice"
    );
    let done = CallOutcome {
        content: "found".to_owned(),
        is_error: false,
    };
    assert_eq!(
        stores
            .runs
            .settle_call(
                org,
                ws,
                id,
                awaiting.id,
                &[CallState::Started],
                CallState::Completed,
                &done,
                &[],
                now
            )
            .await,
        Err(RunError::Moved),
        "settled only from the state expected"
    );
    stores
        .runs
        .settle_call(
            org,
            ws,
            id,
            allowed.id,
            &[CallState::Started],
            CallState::Completed,
            &done,
            &[event(RunEventType::ToolCallCompleted)],
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        stores
            .runs
            .settle_call(
                org,
                ws,
                id,
                allowed.id,
                &[CallState::Started],
                CallState::Unknown,
                &done,
                &[],
                now
            )
            .await,
        Err(RunError::Moved),
        "settled once"
    );

    // Approval: only for the call the run waits on, while it waits.
    let denial = CallOutcome {
        content: "Denied.".to_owned(),
        is_error: true,
    };
    let key = ApiKeyId::generate();
    let denial = &denial;
    let resolve = |call: ToolCallId, decision: ApprovalDecision| async move {
        stores
            .runs
            .resolve_approval(
                org,
                ws,
                id,
                call,
                decision,
                key,
                denial,
                &[event(RunEventType::ApprovalResolved)],
                &job(id),
                Timestamp::now(),
            )
            .await
            .map(|(run, _)| run.status)
    };
    assert_eq!(
        resolve(awaiting.id, ApprovalDecision::Approved).await,
        Err(RunError::NotWaiting),
        "a running run waits for nothing"
    );
    stores
        .runs
        .request_approval(org, ws, id, awaiting.id, &[event(RunEventType::ApprovalRequested)], now)
        .await
        .unwrap();
    assert_eq!(
        stores.runs.request_approval(org, ws, id, later.id, &[], now).await,
        Err(RunError::Moved),
        "one approval at a time"
    );
    assert_eq!(
        stores.runs.find_run(org, ws, id).await.unwrap().unwrap().status,
        RunStatus::WaitingForApproval
    );
    for other in [allowed.id, later.id, refused.id, ToolCallId::generate()] {
        assert_eq!(
            resolve(other, ApprovalDecision::Approved).await,
            Err(RunError::NotWaiting),
            "only the call it waits on"
        );
    }
    assert_eq!(
        stores
            .runs
            .resolve_approval(
                b.organization_id,
                b.workspace,
                id,
                awaiting.id,
                ApprovalDecision::Approved,
                key,
                denial,
                &[],
                &job(id),
                now,
            )
            .await
            .map(|_| ()),
        Err(RunError::NotFound)
    );
    // No step reaches the run from its sibling workspace (same
    // organization, other environment) or another organization.
    for (organization, workspace) in [(org, a.sibling), (b.organization_id, b.workspace)] {
        assert_eq!(
            stores
                .runs
                .resolve_approval(
                    organization,
                    workspace,
                    id,
                    awaiting.id,
                    ApprovalDecision::Approved,
                    key,
                    denial,
                    &[],
                    &job(id),
                    now,
                )
                .await
                .map(|_| ()),
            Err(RunError::NotFound),
            "a decision only in the run's own workspace"
        );
        assert_eq!(
            stores
                .runs
                .request_approval(organization, workspace, id, later.id, &[], now)
                .await,
            Err(RunError::NotFound)
        );
        assert_eq!(
            stores
                .runs
                .begin_call(organization, workspace, id, awaiting.id, &[], now)
                .await,
            Err(RunError::NotFound)
        );
        assert_eq!(
            stores
                .runs
                .settle_call(
                    organization,
                    workspace,
                    id,
                    later.id,
                    &[CallState::AwaitingApproval],
                    CallState::Refused,
                    denial,
                    &[],
                    now
                )
                .await,
            Err(RunError::NotFound)
        );
        assert_eq!(
            stores
                .runs
                .decide_call(
                    organization,
                    workspace,
                    id,
                    later.id,
                    &[CallState::AwaitingApproval],
                    CallState::Refused,
                    &decision("deny", "organization_denies"),
                    Some(denial),
                    &[],
                    now
                )
                .await,
            Err(RunError::NotFound)
        );
        assert_eq!(
            stores
                .runs
                .finish_run(
                    organization,
                    workspace,
                    id,
                    1,
                    &Ending::Failed(Failure::new("run_unavailable", "Gone.")),
                    &[],
                    now
                )
                .await,
            Err(RunError::NotFound)
        );
    }
    assert_eq!(
        stores.runs.find_run(org, ws, id).await.unwrap().unwrap().status,
        RunStatus::WaitingForApproval,
        "untouched from elsewhere"
    );
    assert!(
        claim_all(stores).await.is_empty(),
        "nothing queued by a refused decision"
    );
    assert_eq!(
        resolve(awaiting.id, ApprovalDecision::Approved).await,
        Ok(RunStatus::Running)
    );
    assert_eq!(
        claim_all(stores).await,
        [(Scope::Workspace(org, ws), id.to_string())],
        "a decision queues the run again"
    );
    assert_eq!(
        resolve(awaiting.id, ApprovalDecision::Denied).await,
        Err(RunError::NotWaiting),
        "decided once"
    );
    let progress = stores.runs.find_progress(org, ws, id).await.unwrap().unwrap();
    let approved = progress.calls.iter().find(|call| call.id == awaiting.id).unwrap();
    assert_eq!(approved.state, CallState::Approved);
    assert_eq!(approved.approval, Some((ApprovalDecision::Approved, key)));
    stores
        .runs
        .begin_call(org, ws, id, awaiting.id, &[], now)
        .await
        .unwrap();
    stores
        .runs
        .settle_call(
            org,
            ws,
            id,
            awaiting.id,
            &[CallState::Started],
            CallState::Unknown,
            &CallOutcome {
                content: "Unknown.".to_owned(),
                is_error: true,
            },
            &[],
            now,
        )
        .await
        .unwrap();

    stores
        .runs
        .request_approval(org, ws, id, later.id, &[event(RunEventType::ApprovalRequested)], now)
        .await
        .unwrap();
    assert_eq!(
        resolve(later.id, ApprovalDecision::Denied).await,
        Ok(RunStatus::Running)
    );
    claim_all(stores).await;
    let progress = stores.runs.find_progress(org, ws, id).await.unwrap().unwrap();
    let denied = progress.calls.iter().find(|call| call.id == later.id).unwrap();
    assert_eq!(denied.state, CallState::Denied);
    assert_eq!(
        denied.outcome.as_ref(),
        Some(denial),
        "a denial is what the model reads back"
    );
    assert_eq!(progress.run.usage.tool_calls, 2, "calls sent are counted");
    assert!(progress.calls.iter().all(|call| call.state.is_settled()));

    stores
        .runs
        .record_turn(
            org,
            ws,
            id,
            1,
            &NewTurn {
                text: Some("Done.".to_owned()),
                calls: Vec::new(),
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 1,
                },
                // A final answer ends the run with its turn, in one change.
                finish: Some(Ending::Completed {
                    output: "Done.".to_owned(),
                }),
            },
            &[event(RunEventType::MessageAssistant), event(RunEventType::RunCompleted)],
            now,
        )
        .await
        .unwrap();
    let run = stores.runs.find_run(org, ws, id).await.unwrap().unwrap();
    assert_eq!(
        (run.status, run.output.as_deref()),
        (RunStatus::Completed, Some("Done."))
    );
    assert_eq!((run.usage.input_tokens, run.usage.output_tokens), (15, 4));
    assert!(run.started_at.is_some() && run.finished_at.is_some());

    let events = stores.runs.list_events(org, ws, id, 0).await.unwrap();
    let sequences: Vec<u64> = events.iter().map(|event| event.sequence).collect();
    assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>(), "no gaps");
    assert!(
        events
            .iter()
            .all(|event| event.run_id == id && event.workspace_id == ws)
    );
    assert_eq!(events.first().map(|event| event.kind), Some(RunEventType::RunQueued));
    assert_eq!(events.last().map(|event| event.kind), Some(RunEventType::RunCompleted));
    let later_events = stores.runs.list_events(org, ws, id, 3).await.unwrap();
    assert_eq!(
        later_events.first().map(|event| event.sequence),
        Some(4),
        "after a sequence"
    );
}

async fn cancels_and_finishes_once(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    let now = Timestamp::now();
    let queued = create(stores, org, ws, agent_a, "Cancel me", now).await;
    let cancelled = || NewRunEvent::new(RunEventType::RunCancelled, json!({ "cancelled_by": "Someone" }));
    let (run, events) = stores
        .runs
        .cancel_run(org, ws, queued, &[cancelled()], now)
        .await
        .unwrap();
    assert_eq!(run.status, RunStatus::Cancelled);
    assert_eq!(run.finished_at, Some(now));
    assert_eq!(
        events[0].data,
        json!({ "cancelled_by": "Someone" }),
        "the event's data, as given"
    );
    assert_eq!(
        stores
            .runs
            .cancel_run(org, ws, queued, &[cancelled()], now)
            .await
            .map(|_| ()),
        Err(RunError::Finished)
    );
    assert_eq!(
        stores.runs.start_run(org, ws, queued, &[], now).await,
        Err(RunError::Moved),
        "a cancelled run never starts"
    );
    let failing = create(stores, org, ws, agent_a, "Fail me", now).await;
    stores.runs.start_run(org, ws, failing, &[], now).await.unwrap();
    let failure = Failure::new("model_unavailable", "The model provider did not respond in time.");
    assert_eq!(
        stores
            .runs
            .finish_run(
                org,
                ws,
                failing,
                1,
                &Ending::Failed(failure.clone()),
                &[event(RunEventType::RunFailed)],
                now,
            )
            .await,
        Err(RunError::Moved),
        "finished only by the step that read its turns"
    );
    stores
        .runs
        .finish_run(
            org,
            ws,
            failing,
            0,
            &Ending::Failed(failure.clone()),
            &[event(RunEventType::RunFailed)],
            now,
        )
        .await
        .unwrap();
    let run = stores.runs.find_run(org, ws, failing).await.unwrap().unwrap();
    assert_eq!((run.status, run.error), (RunStatus::Failed, Some(failure)));
    assert_eq!(
        stores
            .runs
            .finish_run(
                org,
                ws,
                failing,
                0,
                &Ending::Completed {
                    output: "late".to_owned()
                },
                &[],
                now
            )
            .await,
        Err(RunError::Finished)
    );
    assert_eq!(
        stores.runs.cancel_run(org, ws, failing, &[], now).await.map(|_| ()),
        Err(RunError::Finished)
    );
    claim_all(stores).await;
}

async fn keeps_runs_of_a_deleted_agent(stores: &Stores<'_>, a: Tenant) {
    let org = a.organization_id;
    let agent_id = agent(stores, org, a.workspace, "Short-lived").await;
    let id = create(stores, org, a.workspace, agent_id, "Before", Timestamp::now()).await;
    stores.agents.delete_agent(org, a.workspace, agent_id).await.unwrap();
    let run = stores.runs.find_run(org, a.workspace, id).await.unwrap().unwrap();
    assert_eq!(
        (run.agent_id, run.agent_name),
        (agent_id, None),
        "the run stays, its agent named by ID"
    );
    assert_eq!(stores.runs.list_events(org, a.workspace, id, 0).await.unwrap().len(), 1);
    claim_all(stores).await;
}

async fn decides_a_call_again_before_sending(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    let now = Timestamp::now();
    let id = create(stores, org, ws, agent_a, "Decide again", now).await;
    claim_all(stores).await;
    stores.runs.start_run(org, ws, id, &[], now).await.unwrap();
    let first = call("m-1", Some("send"), CallState::Allowed);
    let second = call("m-2", Some("drop"), CallState::Allowed);
    let turn = NewTurn {
        text: None,
        calls: vec![first.clone(), second.clone()],
        usage: TokenUsage::default(),
        finish: None,
    };
    stores.runs.record_turn(org, ws, id, 0, &turn, &[], now).await.unwrap();
    let wait = decision("require_approval", "organization_requires_approval");
    let from = [CallState::Allowed, CallState::Approved];
    assert_eq!(
        stores
            .runs
            .decide_call(
                org,
                ws,
                id,
                first.id,
                &from,
                CallState::Completed,
                &wait,
                None,
                &[],
                now
            )
            .await,
        Err(RunError::Moved),
        "only to awaiting approval or refused"
    );
    stores
        .runs
        .decide_call(
            org,
            ws,
            id,
            first.id,
            &from,
            CallState::AwaitingApproval,
            &wait,
            None,
            &[event(RunEventType::MessageAssistant)],
            now,
        )
        .await
        .unwrap();
    assert_eq!(
        stores
            .runs
            .decide_call(
                org,
                ws,
                id,
                first.id,
                &from,
                CallState::AwaitingApproval,
                &wait,
                None,
                &[],
                now
            )
            .await,
        Err(RunError::Moved),
        "decided again once"
    );
    let deny = decision("deny", "organization_denies");
    let refused = CallOutcome {
        content: "Blocked.".to_owned(),
        is_error: true,
    };
    stores
        .runs
        .decide_call(
            org,
            ws,
            id,
            second.id,
            &from,
            CallState::Refused,
            &deny,
            Some(&refused),
            &[],
            now,
        )
        .await
        .unwrap();
    let progress = stores.runs.find_progress(org, ws, id).await.unwrap().unwrap();
    assert_eq!(progress.calls[0].state, CallState::AwaitingApproval);
    assert_eq!(
        progress.calls[0].decision.as_ref(),
        Some(&wait),
        "the new decision replaces the old"
    );
    assert_eq!(progress.calls[1].state, CallState::Refused);
    assert_eq!(progress.calls[1].decision.as_ref(), Some(&deny));
    assert_eq!(progress.calls[1].outcome.as_ref(), Some(&refused));
    assert_eq!(
        stores.runs.begin_call(org, ws, id, second.id, &[], now).await,
        Err(RunError::Moved),
        "a refused call is never begun"
    );
    stores.runs.cancel_run(org, ws, id, &[], now).await.unwrap();
    assert_eq!(
        stores
            .runs
            .decide_call(
                org,
                ws,
                id,
                first.id,
                &[CallState::AwaitingApproval],
                CallState::Refused,
                &deny,
                Some(&refused),
                &[],
                now
            )
            .await,
        Err(RunError::Moved),
        "only while the run is running"
    );
}

async fn finds_runs_whose_job_died(stores: &Stores<'_>, a: Tenant, agent_a: AgentId) {
    let org = a.organization_id;
    let ws = a.workspace;
    claim_all(stores).await;
    let id = create(stores, org, ws, agent_a, "Abandoned", Timestamp::now()).await;
    let abandoned = || async { stores.runs.abandoned_runs(100).await.unwrap() };
    assert!(
        !abandoned().await.contains(&(org, ws, id)),
        "a run whose job is queued isn't abandoned"
    );
    let kind = Name::new(KIND).unwrap();
    let job = stores
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert!(!abandoned().await.contains(&(org, ws, id)), "nor one whose job is held");
    stores
        .jobs
        .bury(&job.scope, job.id, &job.lease, &Name::new("lease_expired").unwrap())
        .await
        .unwrap();
    assert!(
        abandoned().await.contains(&(org, ws, id)),
        "a run whose job died is, with its scope"
    );
    stores
        .runs
        .finish_run(
            org,
            ws,
            id,
            0,
            &Ending::Failed(Failure::new("run_unavailable", "Gone.")),
            &[],
            Timestamp::now(),
        )
        .await
        .unwrap();
    assert!(!abandoned().await.contains(&(org, ws, id)), "a finished run isn't");
}
